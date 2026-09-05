//! The coordinator: owns the session state machine and turns its actions into I/O.
//!
//! Runs on the main thread and multiplexes four channels — hotkey events from the message
//! thread, audio frames from the capture thread, transcripts from the inference thread,
//! and tray commands. All decisions live in `vox_core::Session`; this file only executes
//! them.

use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender};
use vox_core::traits::Vad;
use vox_core::vad::EnergyVad;
use vox_core::{
    text, Action, Config, Event, HotkeyEvent, Notice, Segmenter, Session, State, TextSink,
    SAMPLE_RATE,
};
use vox_platform_win::{
    open_capture, tick, AudioMsg, CaptureStream, MessageThreadHandle, Tick, TrayEvent, WinTextSink,
};

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
    pub jobs_tx: Sender<Job>,
    pub results_rx: Receiver<JobResult>,
    pub tray_rx: Receiver<TrayEvent>,
}

pub struct Coordinator {
    cfg: Config,
    ch: Channels,
    msg: MessageThreadHandle,
    session: Session,
    segmenter: Segmenter,
    vad: EnergyVad,
    sink: WinTextSink,
    start: Instant,
    capture: Option<CaptureStream>,
    capturing: bool,
    generation: u64,
    framer: Vec<f32>,
    parts: Vec<String>,
    last_text: Option<String>,
    shown_state: Option<State>,
}

impl Coordinator {
    pub fn new(cfg: Config, ch: Channels, msg: MessageThreadHandle) -> Self {
        let session = Session::new(cfg.hotkey.mode, cfg.hotkey.min_press_ms);
        let segmenter = Segmenter::new(cfg.audio.segmenter.clone(), SAMPLE_RATE);
        Coordinator {
            cfg,
            ch,
            msg,
            session,
            segmenter,
            vad: EnergyVad::default(),
            sink: WinTextSink,
            start: Instant::now(),
            capture: None,
            capturing: false,
            generation: 0,
            framer: Vec::with_capacity(FRAME * 4),
            parts: Vec::new(),
            last_text: None,
            shown_state: None,
        }
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Blocks until the tray asks to quit.
    pub fn run(mut self) {
        self.refresh_tooltip();
        tracing::info!(
            "ready — hold {} to dictate ({:?} mode)",
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
                recv(self.ch.results_rx) -> res => match res {
                    Ok(r) => self.on_result(r),
                    Err(_) => { tracing::error!("inference thread gone"); break; }
                },
                recv(self.ch.tray_rx) -> ev => match ev {
                    Ok(TrayEvent::Quit) | Err(_) => break,
                    Ok(TrayEvent::OpenSettings) => tracing::info!("settings UI not built yet (phase 2)"),
                },
            }
        }
        tracing::info!("shutting down");
        self.capture = None;
        self.msg.join();
    }

    fn dispatch(&mut self, event: Event) {
        let now = self.now_ms();
        let actions = self.session.handle(event, now);
        for action in actions {
            self.apply(action);
        }
        self.refresh_tooltip();
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::OpenStream => {
                let t0 = Instant::now();
                match open_capture(&self.cfg.audio.device, self.ch.audio_tx.clone()) {
                    Ok(stream) => {
                        tracing::debug!("mic `{}` open in {:?}", stream.device_name, t0.elapsed());
                        self.capture = Some(stream);
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
                match self.sink.inject(&full, method) {
                    Ok(()) => tracing::info!(
                        "typed {} chars via {method:?} in {:?}: {full}",
                        full.chars().count(),
                        t0.elapsed()
                    ),
                    Err(e) => {
                        tracing::error!("injection failed: {e}");
                        if self.cfg.behavior.sounds {
                            tick(Tick::Error);
                        }
                    }
                }
                self.dispatch(Event::Injected);
            }
            Action::Notify(notice) => {
                match &notice {
                    Notice::MicUnavailable(e) => tracing::error!("microphone unavailable: {e}"),
                    Notice::DeviceLost(e) => tracing::warn!("microphone lost mid-recording: {e}"),
                    Notice::Cancelled => tracing::info!("cancelled"),
                }
                if self.cfg.behavior.sounds && !matches!(notice, Notice::Cancelled) {
                    tick(Tick::Error);
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
        if self.ch.jobs_tx.send(job).is_err() {
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
            "{} segment: {} ms audio → {:?} → {clean:?}",
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

    fn refresh_tooltip(&mut self) {
        let state = self.session.state();
        if self.shown_state == Some(state) {
            return;
        }
        self.shown_state = Some(state);
        let tip = match state {
            State::Idle => format!("Vox — idle (hold {})", self.cfg.hotkey.chord),
            State::Arming => "Vox — opening microphone…".to_string(),
            State::Recording => "Vox — recording".to_string(),
            State::Finalizing => "Vox — transcribing…".to_string(),
            State::Injecting => "Vox — typing…".to_string(),
        };
        self.msg.set_tooltip(&tip);
    }
}
