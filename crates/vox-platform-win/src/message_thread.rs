//! The daemon's Win32 message thread: a hidden window, the tray icon, and the input hooks.
//!
//! Low-level hooks only work on a thread that pumps messages, and a tray icon needs a
//! window to deliver its callbacks to, so both live here. Other threads talk to it by
//! posting messages through [`MessageThreadHandle`].

use std::cell::RefCell;
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use vox_core::{Chord, HotkeyEvent};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, HICON, IDI_APPLICATION, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_STYLE, WM_APP,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW, WS_EX_TOOLWINDOW,
};

use crate::hooks::{self, Hooks};
use crate::misc::{pcwstr, wide};
use crate::PlatformError;

const WM_TRAY: u32 = WM_APP + 1;
const WM_SET_TOOLTIP: u32 = WM_APP + 2;
const WM_REBIND: u32 = WM_APP + 3;
const WM_QUIT_DAEMON: u32 = WM_APP + 4;

const TRAY_ID: u32 = 1;
const MENU_QUIT: usize = 1;
const MENU_SETTINGS: usize = 2;
const TIMER_REHOOK: usize = 1;
/// Hooks are re-installed this often as insurance against silent removal.
const REHOOK_INTERVAL_MS: u32 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Quit,
    OpenSettings,
}

pub struct Options {
    pub chord: Chord,
    pub hotkey_tx: Sender<HotkeyEvent>,
    pub tray_tx: Sender<TrayEvent>,
    pub tooltip: String,
}

struct WindowState {
    hwnd: HWND,
    tray_tx: Sender<TrayEvent>,
    hooks: Option<Hooks>,
    mouse: bool,
    tooltip: String,
    icon: HICON,
    taskbar_created: u32,
}

thread_local! {
    static STATE: RefCell<Option<WindowState>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
struct SendHwnd(HWND);
// HWNDs are process-global tokens; posting to one from another thread is the normal use.
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

pub struct MessageThreadHandle {
    hwnd: SendHwnd,
    join: Option<JoinHandle<()>>,
}

impl MessageThreadHandle {
    /// Tray tooltip / status line, e.g. "Vox — recording".
    pub fn set_tooltip(&self, text: &str) {
        let boxed = Box::into_raw(Box::new(text.to_string()));
        let posted = unsafe {
            PostMessageW(
                Some(self.hwnd.0),
                WM_SET_TOOLTIP,
                WPARAM(0),
                LPARAM(boxed as isize),
            )
        };
        if posted.is_err() {
            // Reclaim; the window is gone.
            unsafe { drop(Box::from_raw(boxed)) };
        }
    }

    pub fn rebind(&self, chord: Chord) {
        let boxed = Box::into_raw(Box::new(chord));
        let posted = unsafe {
            PostMessageW(
                Some(self.hwnd.0),
                WM_REBIND,
                WPARAM(0),
                LPARAM(boxed as isize),
            )
        };
        if posted.is_err() {
            unsafe { drop(Box::from_raw(boxed)) };
        }
    }

    /// Ask the thread to tear down the tray icon and exit its loop.
    pub fn quit(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd.0), WM_QUIT_DAEMON, WPARAM(0), LPARAM(0));
        }
    }

    pub fn join(mut self) {
        self.quit();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for MessageThreadHandle {
    fn drop(&mut self) {
        self.quit();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Starts the message thread. Returns once the window, tray icon and hooks exist.
pub fn spawn(opts: Options) -> Result<MessageThreadHandle, PlatformError> {
    let Options {
        chord,
        hotkey_tx,
        tray_tx,
        tooltip,
    } = opts;
    hooks::init(chord, hotkey_tx)?;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<SendHwnd, PlatformError>>();

    let join = std::thread::Builder::new()
        .name("vox-message".into())
        .spawn(move || {
            match create_window_and_tray(chord, tray_tx, tooltip) {
                Ok(hwnd) => {
                    let _ = ready_tx.send(Ok(SendHwnd(hwnd)));
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            STATE.with(|s| *s.borrow_mut() = None);
        })
        .map_err(|e| PlatformError::Other(format!("spawning message thread: {e}")))?;

    let hwnd = ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| PlatformError::Timeout("message thread did not start".into()))??;

    Ok(MessageThreadHandle {
        hwnd,
        join: Some(join),
    })
}

fn create_window_and_tray(
    chord: Chord,
    tray_tx: Sender<TrayEvent>,
    tooltip: String,
) -> Result<HWND, PlatformError> {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)
            .map_err(PlatformError::win("GetModuleHandleW"))?
            .into();
        let class_name = w!("VoxMessageWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // Zero means failure — unless the class already exists from an earlier attempt,
        // which is fine for our purposes.
        let _ = RegisterClassW(&class);

        // A real (never shown) top-level window rather than HWND_MESSAGE so the
        // TaskbarCreated broadcast reaches us after an Explorer restart.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("Vox"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )
        .map_err(PlatformError::win("CreateWindowExW"))?;

        let icon = LoadIconW(None, IDI_APPLICATION).map_err(PlatformError::win("LoadIconW"))?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let mouse = hooks::chord_uses_mouse(&chord);
        let hooks = Hooks::install(mouse)?;

        let added = tray_notify(NIM_ADD, hwnd, icon, &tooltip);
        STATE.with(|s| {
            *s.borrow_mut() = Some(WindowState {
                hwnd,
                tray_tx,
                hooks: Some(hooks),
                mouse,
                tooltip,
                icon,
                taskbar_created,
            })
        });

        if !added {
            tracing::warn!("Shell_NotifyIconW(NIM_ADD) failed; continuing without a tray icon");
        }
        SetTimer(Some(hwnd), TIMER_REHOOK, REHOOK_INTERVAL_MS, None);
        Ok(hwnd)
    }
}

/// Add/modify/delete the tray icon. Returns success.
unsafe fn tray_notify(
    action: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE,
    hwnd: HWND,
    icon: HICON,
    tip: &str,
) -> bool {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon;
    for (dst, src) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *dst = src;
    }
    unsafe { Shell_NotifyIconW(action, &data) }.as_bool()
}

unsafe fn show_menu(hwnd: HWND, tooltip: &str, tray_tx: &Sender<TrayEvent>) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let status = wide(tooltip);
        let settings = wide("Open settings (coming soon)");
        let quit = wide("Quit Vox");
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, pcwstr(&status));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED,
            MENU_SETTINGS,
            pcwstr(&settings),
        );
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, pcwstr(&quit));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        // Documented quirk: post a no-op so the menu dismisses correctly.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            MENU_QUIT => {
                let _ = tray_tx.try_send(TrayEvent::Quit);
            }
            MENU_SETTINGS => {
                let _ = tray_tx.try_send(TrayEvent::OpenSettings);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam.0 & 0xFFFF) as u32;
            match event {
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    // Copy what the menu needs out of the RefCell first: TrackPopupMenu runs
                    // a nested message loop that re-enters this procedure.
                    let snapshot = STATE.with(|s| {
                        s.borrow()
                            .as_ref()
                            .map(|st| (st.tooltip.clone(), st.tray_tx.clone()))
                    });
                    if let Some((tooltip, tx)) = snapshot {
                        unsafe { show_menu(hwnd, &tooltip, &tx) };
                    }
                }
                WM_LBUTTONDBLCLK => {
                    STATE.with(|s| {
                        if let Some(st) = s.borrow().as_ref() {
                            let _ = st.tray_tx.try_send(TrayEvent::OpenSettings);
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SET_TOOLTIP => {
            // SAFETY: posted by MessageThreadHandle::set_tooltip with a Box<String>.
            let text = unsafe { *Box::from_raw(lparam.0 as *mut String) };
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.tooltip = text;
                    unsafe {
                        tray_notify(NIM_MODIFY, st.hwnd, st.icon, &st.tooltip);
                    }
                }
            });
            LRESULT(0)
        }
        WM_REBIND => {
            // SAFETY: posted by MessageThreadHandle::rebind with a Box<Chord>.
            let chord = unsafe { *Box::from_raw(lparam.0 as *mut Chord) };
            hooks::rebind(chord);
            let want_mouse = hooks::chord_uses_mouse(&chord);
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    if st.mouse != want_mouse {
                        st.mouse = want_mouse;
                        if let Some(h) = st.hooks.as_mut() {
                            if let Err(e) = h.reinstall(want_mouse) {
                                tracing::error!("re-installing hooks after rebind: {e}");
                            }
                        }
                    }
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_REHOOK => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    if let Some(h) = st.hooks.as_mut() {
                        if let Err(e) = h.reinstall(st.mouse) {
                            tracing::error!("hook watchdog re-install failed: {e}");
                        }
                    }
                }
            });
            LRESULT(0)
        }
        WM_COMMAND => LRESULT(0),
        WM_QUIT_DAEMON => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    unsafe {
                        tray_notify(NIM_DELETE, st.hwnd, st.icon, "");
                    }
                    st.hooks = None;
                }
            });
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // Explorer restarted: the tray is empty, add our icon again.
            let is_taskbar_created = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .is_some_and(|st| st.taskbar_created != 0 && msg == st.taskbar_created)
            });
            if is_taskbar_created {
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        unsafe {
                            tray_notify(NIM_ADD, st.hwnd, st.icon, &st.tooltip);
                        }
                    }
                });
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}
