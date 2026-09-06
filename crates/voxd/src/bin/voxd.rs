//! voxd.exe — the Vox app: tray icon, settings window, dictation daemon.
//!
//! Tauri owns the main thread (window/tray event loop). The daemon runs on background
//! threads started in `setup`; closing the settings window destroys the webview and leaves
//! the daemon running. Quit lives in the tray menu.
#![windows_subsystem = "windows"]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use vox_core::{Config, SAMPLE_RATE};
use vox_platform_win::vad_rms;
use vox_platform_win::{
    hooks, list_capture_devices, message_box, open_capture, watch_devices, AudioMsg, CaptureStream,
    DeviceEvent, DeviceInfo, DeviceWatcher, MessageKind, MINIMIZED_FLAG,
};
use voxd::daemon::Daemon;
use voxd::paths;
use voxd::status::{Status, StatusSink, UiRequest};

const TRAY_IDLE: &[u8] = include_bytes!("../../icons/tray.png");
const TRAY_REC: &[u8] = include_bytes!("../../icons/tray-rec.png");
const SETTINGS_WINDOW: &str = "settings";

// ---------------------------------------------------------------------------------------------

struct TauriSink {
    app: AppHandle,
}

impl StatusSink for TauriSink {
    fn on_status(&self, status: &Status) {
        let _ = self.app.emit("status", status);
        if let Some(tray) = self.app.tray_by_id("main") {
            let recording = matches!(status.state.as_str(), "arming" | "recording");
            if let Ok(img) = Image::from_bytes(if recording { TRAY_REC } else { TRAY_IDLE }) {
                let _ = tray.set_icon(Some(img));
            }
            let _ = tray.set_tooltip(Some(format!("Vox — {}", status.headline())));
        }
    }
}

struct Meter {
    _stream: CaptureStream,
    stop: Arc<AtomicBool>,
}

struct AppState {
    daemon: Mutex<Option<Daemon>>,
    status: Arc<Mutex<Status>>,
    meter: Mutex<Option<Meter>>,
    _watcher: Mutex<Option<DeviceWatcher>>,
}

impl AppState {
    fn send(&self, req: UiRequest) -> Result<(), String> {
        let guard = self
            .daemon
            .lock()
            .map_err(|_| "daemon lock poisoned".to_string())?;
        match guard.as_ref() {
            Some(d) => d
                .ui_tx
                .send(req)
                .map_err(|_| "daemon is not running".to_string()),
            None => Err("daemon is not running".into()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Commands

#[derive(Serialize)]
struct ModelEntry {
    name: String,
    path: String,
    size_mb: u64,
}

#[derive(Serialize, Clone)]
struct LevelPayload {
    /// dBFS, roughly −90 (silence) to 0.
    db: f32,
    peak: f32,
}

#[derive(Serialize)]
struct Paths {
    config: String,
    log: String,
    models_dir: String,
    gpu_build: bool,
    version: String,
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> Status {
    state.status.lock().map(|s| s.clone()).unwrap_or_default()
}

#[tauri::command]
fn get_config() -> Result<Config, String> {
    paths::load_or_create_config().map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn set_config(state: State<'_, AppState>, config: Config) -> Result<(), String> {
    state.send(UiRequest::SetConfig(Box::new(config)))
}

#[tauri::command]
fn list_devices() -> Result<Vec<DeviceInfo>, String> {
    list_capture_devices().map_err(|e| e.to_string())
}

#[tauri::command]
fn list_models() -> Vec<ModelEntry> {
    paths::list_models()
        .into_iter()
        .map(|(name, path)| ModelEntry {
            size_mb: std::fs::metadata(&path)
                .map(|m| m.len() / 1_048_576)
                .unwrap_or(0),
            path: path.display().to_string(),
            name,
        })
        .collect()
}

#[tauri::command]
fn get_paths() -> Paths {
    Paths {
        config: paths::config_path().display().to_string(),
        log: paths::log_path().display().to_string(),
        models_dir: paths::local_dir().join("models").display().to_string(),
        gpu_build: vox_engine_whisper::GPU_BUILD,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Waits for the next key/mouse press. `Ok(None)` = cancelled (Escape or timeout).
#[tauri::command]
async fn capture_hotkey() -> Result<Option<String>, String> {
    let (tx, rx) = unbounded();
    hooks::begin_capture(tx);
    let result =
        tauri::async_runtime::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(30)))
            .await
            .map_err(|e| e.to_string())?;
    hooks::cancel_capture();
    Ok(match result {
        Ok(Some(chord)) => Some(chord.to_string()),
        _ => None,
    })
}

#[tauri::command]
fn cancel_hotkey_capture() {
    hooks::cancel_capture();
}

#[tauri::command]
fn start_meter(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let mut slot = state
        .meter
        .lock()
        .map_err(|_| "meter lock poisoned".to_string())?;
    if let Some(m) = slot.as_ref() {
        return Ok(m._stream.device_name.clone());
    }
    let cfg = paths::load_or_create_config().map_err(|e| format!("{e:#}"))?;
    let (tx, rx) = unbounded();
    let stream = open_capture(&cfg.audio.device, tx).map_err(|e| e.to_string())?;
    let name = stream.device_name.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    std::thread::Builder::new()
        .name("vox-meter".into())
        .spawn(move || {
            let window = SAMPLE_RATE as usize / 20; // 50 ms
            let mut buf: Vec<f32> = Vec::with_capacity(window * 2);
            let mut last = Instant::now();
            while !stop_flag.load(Ordering::SeqCst) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(AudioMsg::Frames(f)) => buf.extend_from_slice(&f),
                    Ok(AudioMsg::Error(_))
                    | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    Err(_) => {}
                }
                if buf.len() >= window
                    || (last.elapsed() > Duration::from_millis(100) && !buf.is_empty())
                {
                    let rms = vad_rms(&buf);
                    let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    buf.clear();
                    last = Instant::now();
                    let db = 20.0 * rms.max(1e-5).log10();
                    let _ = app.emit("level", LevelPayload { db, peak });
                }
            }
        })
        .map_err(|e| e.to_string())?;
    *slot = Some(Meter {
        _stream: stream,
        stop,
    });
    Ok(name)
}

fn stop_meter_inner(state: &AppState) {
    if let Ok(mut slot) = state.meter.lock() {
        if let Some(m) = slot.take() {
            m.stop.store(true, Ordering::SeqCst);
            // Dropping `m` closes the stream, which ends the meter thread's channel.
        }
    }
}

#[tauri::command]
fn stop_meter(state: State<'_, AppState>) {
    stop_meter_inner(&state);
}

#[tauri::command]
fn open_path(path: String) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

// ---------------------------------------------------------------------------------------------

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SETTINGS_WINDOW) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let built =
        WebviewWindowBuilder::new(app, SETTINGS_WINDOW, WebviewUrl::App("index.html".into()))
            .title("Vox")
            .inner_size(900.0, 640.0)
            .min_inner_size(720.0, 520.0)
            .center()
            .build();
    if let Err(e) = built {
        tracing::error!("opening settings window: {e}");
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Vox", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Vox", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &PredefinedMenuItem::separator(app)?, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(TRAY_IDLE)?)
        .tooltip("Vox — starting…")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => open_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                open_settings(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    vox_platform_win::attach_parent_console();
    voxd::logging::init();

    let result = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            set_config,
            list_devices,
            list_models,
            get_paths,
            capture_hotkey,
            cancel_hotkey_capture,
            start_meter,
            stop_meter,
            open_path,
            quit_app,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            build_tray(&handle)?;

            let sink: Arc<dyn StatusSink> = Arc::new(TauriSink {
                app: handle.clone(),
            });
            let daemon = match voxd::daemon::start(sink) {
                Ok(Some(d)) => d,
                Ok(None) => {
                    message_box(
                        "Vox is already running",
                        "Click the Vox icon in the notification area (bottom-right, possibly \
                         under the ^ arrow) to open its window.",
                        MessageKind::Info,
                    );
                    handle.exit(0);
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!("{e:#}");
                    message_box(
                        "Vox could not start",
                        &format!("{e:#}\n\nLog: {}", paths::log_path().display()),
                        MessageKind::Error,
                    );
                    handle.exit(1);
                    return Ok(());
                }
            };
            let status = daemon.status.clone();

            // Device hot-plug → tell the window to refresh its list (debounced).
            let (dev_tx, dev_rx) = unbounded::<DeviceEvent>();
            let watcher = match watch_devices(dev_tx) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!("device notifications unavailable: {e}");
                    None
                }
            };
            let app_for_devices = handle.clone();
            std::thread::Builder::new()
                .name("vox-device-events".into())
                .spawn(move || {
                    while dev_rx.recv().is_ok() {
                        while dev_rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
                        let _ = app_for_devices.emit("devices_changed", ());
                    }
                })
                .ok();

            app.manage(AppState {
                daemon: Mutex::new(Some(daemon)),
                status,
                meter: Mutex::new(None),
                _watcher: Mutex::new(watcher),
            });

            // Launched by hand: show the window. Launched at login (autostart passes
            // --minimized): stay in the tray.
            if !std::env::args().any(|a| a == MINIMIZED_FLAG) {
                open_settings(&handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                if window.label() == SETTINGS_WINDOW {
                    stop_meter_inner(&window.state::<AppState>());
                }
            }
        })
        .build(tauri::generate_context!());

    let app = match result {
        Ok(app) => app,
        Err(e) => {
            tracing::error!("tauri: {e}");
            message_box("Vox could not start", &e.to_string(), MessageKind::Error);
            std::process::exit(1);
        }
    };

    app.run(|app, event| match event {
        // The last window closing must not end the daemon.
        tauri::RunEvent::ExitRequested { api, code, .. } => {
            if code.is_none() {
                api.prevent_exit();
            }
        }
        tauri::RunEvent::Exit => {
            if let Some(state) = app.try_state::<AppState>() {
                stop_meter_inner(&state);
                if let Ok(mut d) = state.daemon.lock() {
                    if let Some(mut daemon) = d.take() {
                        daemon.quit();
                    }
                }
            }
            tracing::info!("bye");
        }
        _ => {}
    });
}
