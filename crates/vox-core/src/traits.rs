//! Boundaries implemented by the engine and platform crates.

/// All audio inside Vox is mono f32 at this rate. The platform layer asks Windows to convert.
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscribeOptions {
    /// ISO 639-1 code, e.g. `"en"`. `None` lets the model detect (slower).
    pub language: Option<String>,
    /// Text of the preceding segment; steers spelling/punctuation continuity.
    pub initial_prompt: Option<String>,
    /// `1` = greedy.
    pub beam_size: u32,
}

impl Default for TranscribeOptions {
    fn default() -> Self {
        TranscribeOptions {
            language: Some("en".into()),
            initial_prompt: None,
            beam_size: 1,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("model not loaded")]
    NotLoaded,
    #[error("model file not found: {0}")]
    ModelNotFound(String),
    #[error("inference failed: {0}")]
    Inference(String),
    #[error("{0}")]
    Other(String),
}

/// A speech-to-text backend. One instance is owned by the inference thread; calls are
/// strictly sequential.
pub trait Engine: Send {
    /// `audio` is mono f32 at [`SAMPLE_RATE`]. Returns raw text; callers run
    /// [`crate::text::clean`] on it.
    fn transcribe(
        &mut self,
        audio: &[f32],
        opts: &TranscribeOptions,
    ) -> Result<String, EngineError>;

    fn name(&self) -> &str;
}

/// Per-frame voice-activity detection. Frames are typically 30 ms (480 samples).
pub trait Vad: Send {
    fn is_speech(&mut self, frame: &[f32]) -> bool;
    fn reset(&mut self);
}

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("no focused window accepts input")]
    NoTarget,
    #[error("clipboard unavailable: {0}")]
    Clipboard(String),
    #[error("{0}")]
    Other(String),
}

/// Delivers text into the focused application.
pub trait TextSink: Send {
    fn inject(
        &mut self,
        text: &str,
        method: crate::injection::InjectMethod,
    ) -> Result<(), SinkError>;
}
