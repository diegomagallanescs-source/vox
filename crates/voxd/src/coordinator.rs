//! The coordinator: owns the session state machine and turns its actions into I/O.
//!
//! Runs on its own thread and multiplexes five channels — hotkey events from the hook
//! thread, audio frames from the capture thread, results from the inference thread, and
//! requests from the UI. All decisions live in `vox_core::Session`; this file only executes
//! them and reports [`Status`] changes to the UI.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender};
use vox_core::traits::Vad;
use vox_core::vad::EnergyVad;
use vox_core::{
    text, Action, Config, Event, HotkeyEvent, Notice, Segmenter, Session, TextSink, SAMPLE_RATE,
};
use vox_platform_win::{
    open_capture, set_autostart, tick, AudioMsg, CaptureStream, MessageThreadHandle, Tick,
    WinTextSink,
};

use crate::engine::{InferenceEvent, InferenceMsg};
use crate::paths;
use crate::status::{Status, StatusSink, UiRequest};

/// Work for the inference thread.
pub struct Job {
    /// Capture generation; results from an older generation are discarded.
    pub generation: u64,
    /// May be empty for a final job with no tail audio — the engine returns `""` fast.
    pub samples: Vec<f32>,
    pub prompt: Option<String>,
    pub is_final: bool,
}

pub struct JobResult {
    pub generation: u64,
    pub text: String,
    pub is_final: bool,
    pub audio_ms: u32,
    pub elapsed: Duration,
}

/// 30 ms VAD frames.
const FRAME: usize = SAMPLE_RATE as usize * 30 / 1000;

pub struct Channels {
    pub hotkey_rx: Receiver<HotkeyEvent>,
    pub audio_tx: Sender<AudioMsg>,
    pub audio_rx: Receiver<AudioMsg>,
    pub inference_tx: Sender<InferenceMsg>,
    pub events_rx: Receiver<InferenceEvent>,
    pub ui_rx: Receiver<UiRequest>,
}

pub struct Coordinator {
    cfg: Config,
    ch: Channels,
    hooks: MessageThreadHandle,
    status: Arc<Mutex<Status>>,
    sink: Arc<dyn StatusSink>,
    session: Session,
    segmenter: Segmenter,
    vad: EnergyVad,
    text_sink: WinTextSink,
    start: Instant,
    capture: Option<CaptureStream>,
    capturing: bool,
    generation: u64,
    framer: Vec<f32>,
    parts: Vec<String>,
    last_text: Option<String>,
    released_at: Option<Instant>,
}

impl Coordinator {
    pub fn new(
        cfg: Config,
        ch: Channels,
        hooks: MessageThreadHandle,
        status: Arc<Mutex<Status>>,
        sink: Arc<dyn StatusSink>,
    ) -> Self {
        let session = Session::new(cfg.hotkey.mode, cfg.hotkey.min_press_ms);
        let segmenter = Segmenter::new(cfg.audio.segmenter.clone(), SAMPLE_RATE);
        Coordinator {
            cfg,
            ch,
            hooks,
            status,
            sink,
            session,
            segmenter,
            vad: EnergyVad::default(),
            text_sink: WinTextSink,
            start: Instant::now(),
            capture: None,
            capturing: false,
            generation: 0,
            framer: Vec::with_capacity(FRAME * 4),
            parts: Vec::new(),
            last_text: None,
            released_at: None,
        }
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Blocks until the UI asks to quit.
    pub fn run(mut self) {
        if let Err(e) = set_autostart(self.cfg.behavior.autostart) {
            tracing::warn!("autostart registration: {e}");
        }
        self.publish(|_| {});
        tracing::info!(
            "ready - hold {} to dictate ({:?} mode)",
            self.cfg.hotkey.chord,
            self.cfg.hotkey.mode
        );
        loop {
            select! {
                recv(self.ch.hotkey_rx) -> ev => match ev {
                    Ok(HotkeyEvent::Pressed) => self.dispatch(Event::HotkeyPressed),
                    Ok(HotkeyEvent::Released) => self.dispatch(Event::HotkeyReleased),
                    Err(_) => { tracing::error!("hotkey channel closed"); break; }
                },
                recv(self.ch.audio_rx) -> msg => match msg {
                    Ok(AudioMsg::Frames(frames)) => self.on_frames(frames),
                    Ok(AudioMsg::Error(e)) => self.dispatch(Event::StreamFailed(e)),
                    Err(_) => {}
                },
                recv(self.ch.events_rx) -> ev => match ev {
                    Ok(InferenceEvent::Result(r)) => self.on_result(r),
                    Ok(InferenceEvent::Loaded(name)) => self.publish(|s| {
                        s.engine_loaded = true;
                        s.model = name;
                        s.last_error = None;
                    }),
                    Ok(InferenceEvent::Failed(e)) => {
                        tracing::error!("engine: {e}");
                        self.publish(|s| { s.engine_loaded = false; s.last_error = Some(e); });
                    }
                    Err(_) => { tracing::error!("inference thread gone"); break; }
                },
                recv(self.ch.ui_rx) -> req => match req {
                    Ok(UiRequest::SetConfig(cfg)) => self.apply_config(*cfg),
                    Ok(UiRequest::Quit) | Err(_) => break,
                },
            }
        }
        tracing::info!("coordinator shutting down");
        self.capture = None;
        self.hooks.join();
    }

    /// Mutates the shared status and forwards it to the sink.
    fn publish(&self, f: impl FnOnce(&mut Status)) {
        let snapshot = {
            let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut s);
            s.set_state(self.session.state());
            s.clone()
        };
        self.sink.on_status(&snapshot);
    }

    fn apply_config(&mut self, cfg: Config) {
        match cfg.to_toml() {
            Ok(text) => {
                let path = paths::config_path();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(&path, text) {
                    tracing::error!("saving config: {e}");
                }
            }
            Err(e) => tracing::error!("serialising config: {e}"),
        }

        if cfg.hotkey.chord != self.cfg.hotkey.chord {
            self.hooks.rebind(cfg.hotkey.chord);
        }
        self.session.set_mode(cfg.hotkey.mode);
        self.session.set_min_press_ms(cfg.hotkey.min_press_ms);
        if !self.capturing {
            self.segmenter = Segmenter::new(cfg.audio.segmenter.clone(), SAMPLE_RATE);
        }
        if cfg.behavior.autostart != self.cfg.behavior.autostart {
            if let Err(e) = set_autostart(cfg.behavior.autostart) {
                tracing::warn!("autostart: {e}");
            }
        }
        let engine_changed = cfg.engine != self.cfg.engine;
        self.cfg = cfg;
        if engine_changed {
            let _ = self
                .ch
                .inference_tx
                .send(InferenceMsg::Reload(self.cfg.clone()));
        }
        let hotkey = self.cfg.hotkey.chord.to_string();
        let mode = match self.cfg.hotkey.mode {
            vox_core::Mode::PushToTalk => "push_to_talk",
            vox_core::Mode::Toggle => "toggle",
        };
        let model = self.cfg.engine.model.clone();
        self.publish(move |s| {
            s.hotkey = hotkey;
            s.mode = mode.into();
            if engine_changed {
                s.model = model;
                s.engine_loaded = false;
            }
        });
        tracing::info!("config applied");
    }

    fn dispatch(&mut self, event: Event) {
        let now = self.now_ms();
        let before = self.session.state();
        let actions = self.session.handle(event, now);
        for action in actions {
            self.apply(action);
        }
        if self.session.state() != before {
            self.publish(|_| {});
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::OpenStream => {
                let t0 = Instant::now();
                match open_capture(&self.cfg.audio.device, self.ch.audio_tx.clone()) {
                    Ok(stream) => {
                        tracing::debug!("mic `{}` open in {:?}", stream.device_name, t0.elapsed());
                        let name = stream.device_name.clone();
                        self.capture = Some(stream);
                        self.publish(move |s| s.device = Some(name));
                        self.dispatch(Event::StreamOpened);
                    }
                    Err(e) => self.dispatch(Event::StreamFailed(e.to_string())),
                }
            }
            Action::BeginCapture => {
                self.generation += 1;
                self.capturing = true;
                self.segmenter.reset();
                self.vad.reset();
                self.framer.clear();
                self.parts.clear();
                self.last_text = None;
                if self.cfg.behavior.sounds {
                    tick(Tick::Start);
                }
            }
            Action::EndCapture => {
                self.capturing = false;
                self.released_at = Some(Instant::now());
                self.flush_framer();
                let tail = self.segmenter.finish();
                let audio_ms = tail
                    .as_ref()
                    .map(|s| s.samples.len() * 1000 / SAMPLE_RATE as usize)
                    .unwrap_or(0);
                tracing::debug!(
                    "release: tail {audio_ms} ms, {} earlier part(s)",
                    self.parts.len()
                );
                self.send_job(tail.map(|s| s.samples).unwrap_or_default(), true);
                if self.cfg.behavior.sounds {
                    tick(Tick::Stop);
                }
            }
            Action::DiscardCapture => {
                self.capturing = false;
                self.generation += 1;
                self.segmenter.reset();
                self.framer.clear();
                self.parts.clear();
                self.last_text = None;
            }
            Action::CloseStream => {
                // keep_warm_ms is honoured in phase 3; for now release immediately so
                // Bluetooth headsets leave hands-free mode as soon as possible.
                self.capture = None;
            }
            Action::Inject(full) => {
                let method = self.cfg.injection.strategy.choose(&full);
                let t0 = Instant::now();
                let latency = self
                    .released_at
                    .take()
                    .map(|r| r.elapsed().as_millis() as u64);
                match self.text_sink.inject(&full, method) {
                    Ok(()) => {
                        tracing::info!(
                            "typed {} chars via {method:?} in {:?} (release→text {:?} ms): {full}",
                            full.chars().count(),
                            t0.elapsed(),
                            latency
                        );
                        let text = full.clone();
                        self.publish(move |s| {
                            s.last_transcript = Some(text);
                            s.last_latency_ms = latency;
                            s.last_error = None;
                            s.dictations += 1;
                        });
                    }
                    Err(e) => {
                        tracing::error!("injection failed: {e}");
                        let msg = e.to_string();
                        self.publish(move |s| s.last_error = Some(msg));
                        if self.cfg.behavior.sounds {
                            tick(Tick::Error);
                        }
                    }
                }
                self.dispatch(Event::Injected);
            }
            Action::Notify(notice) => {
                let msg = match &notice {
                    Notice::MicUnavailable(e) => {
                        tracing::error!("microphone unavailable: {e}");
                        Some(format!("Microphone unavailable: {e}"))
                    }
                    Notice::DeviceLost(e) => {
                        tracing::warn!("microphone lost mid-recording: {e}");
                        Some(format!("Microphone lost: {e}"))
                    }
                    Notice::Cancelled => {
                        tracing::info!("cancelled");
                        None
                    }
                };
                if let Some(m) = msg {
                    self.publish(move |s| s.last_error = Some(m));
                    if self.cfg.behavior.sounds {
                        tick(Tick::Error);
                    }
                }
            }
        }
    }

    fn on_frames(&mut self, frames: Vec<f32>) {
        if !self.capturing {
            return;
        }
        self.framer.extend_from_slice(&frames);
        while self.framer.len() >= FRAME {
            let frame: Vec<f32> = self.framer.drain(..FRAME).collect();
            self.push_frame(&frame);
        }
    }

    fn flush_framer(&mut self) {
        if !self.framer.is_empty() {
            let rest = std::mem::take(&mut self.framer);
            self.push_frame(&rest);
        }
    }

    fn push_frame(&mut self, frame: &[f32]) {
        let speech = self.vad.is_speech(frame);
        if let Some(segment) = self.segmenter.push(frame, speech) {
            tracing::debug!(
                "intermediate segment: {} ms",
                segment.samples.len() * 1000 / SAMPLE_RATE as usize
            );
            self.send_job(segment.samples, false);
        }
    }

    fn send_job(&mut self, samples: Vec<f32>, is_final: bool) {
        let job = Job {
            generation: self.generation,
            samples,
            prompt: self.last_text.clone(),
            is_final,
        };
        if self.ch.inference_tx.send(InferenceMsg::Job(job)).is_err() {
            tracing::error!("inference thread is gone");
        }
    }

    fn on_result(&mut self, r: JobResult) {
        if r.generation != self.generation {
            tracing::debug!("dropping stale result from generation {}", r.generation);
            return;
        }
        let clean = text::clean(&r.text);
        tracing::debug!(
            "{} segment: {} ms audio -> {:?} -> {clean:?}",
            if r.is_final { "final" } else { "intermediate" },
            r.audio_ms,
            r.elapsed
        );
        if !clean.is_empty() {
            self.parts.push(clean.clone());
            self.last_text = Some(clean);
        }
        if r.is_final {
            let full = text::join(&self.parts);
            self.parts.clear();
            self.dispatch(Event::Finalized(full));
        }
    }
}
