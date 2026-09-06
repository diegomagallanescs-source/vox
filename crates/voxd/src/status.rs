//! What the UI sees, and what it can ask for.

use serde::Serialize;
use vox_core::{Config, State};

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    /// `idle` · `arming` · `recording` · `finalizing` · `injecting`
    pub state: String,
    pub hotkey: String,
    pub mode: String,
    pub model: String,
    pub engine_loaded: bool,
    /// Name of the device used for the most recent capture.
    pub device: Option<String>,
    pub last_transcript: Option<String>,
    /// Release-to-text latency of the most recent dictation.
    pub last_latency_ms: Option<u64>,
    pub last_error: Option<String>,
    pub dictations: u32,
}

impl Status {
    pub fn set_state(&mut self, s: State) {
        self.state = match s {
            State::Idle => "idle",
            State::Arming => "arming",
            State::Recording => "recording",
            State::Finalizing => "finalizing",
            State::Injecting => "injecting",
        }
        .to_string();
    }

    pub fn from_config(cfg: &Config) -> Status {
        Status {
            state: "idle".into(),
            hotkey: cfg.hotkey.chord.to_string(),
            mode: match cfg.hotkey.mode {
                vox_core::Mode::PushToTalk => "push_to_talk".into(),
                vox_core::Mode::Toggle => "toggle".into(),
            },
            model: cfg.engine.model.clone(),
            ..Default::default()
        }
    }

    /// Short line for the tray tooltip.
    pub fn headline(&self) -> String {
        match self.state.as_str() {
            "idle" => {
                if self.engine_loaded {
                    format!("idle · hold {}", self.hotkey)
                } else {
                    "loading model…".into()
                }
            }
            "arming" => "opening microphone…".into(),
            "recording" => "recording".into(),
            "finalizing" => "transcribing…".into(),
            "injecting" => "typing…".into(),
            other => other.to_string(),
        }
    }
}

/// Receives every status change (the Tauri app forwards it to the window and tray).
pub trait StatusSink: Send + Sync {
    fn on_status(&self, status: &Status);
}

/// Requests from the UI to the coordinator. `Config` is boxed to keep the enum small.
#[derive(Debug)]
pub enum UiRequest {
    /// Persist and apply a new configuration.
    SetConfig(Box<Config>),
    Quit,
}
