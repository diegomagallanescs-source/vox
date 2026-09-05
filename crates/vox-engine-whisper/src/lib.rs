//! whisper.cpp engine for Vox.
//!
//! One [`WhisperEngine`] owns a model context plus a single reusable decoding state; it is
//! meant to live on the inference thread and be called sequentially. Parameters are tuned
//! for dictation of short utterances rather than long-form transcription: language pinned,
//! no timestamps, single segment, non-speech tokens suppressed, and — crucially for latency —
//! an encoder audio context sized to the input (see [`AudioCtx`]).
//!
//! whisper.cpp's built-in Silero VAD is not wrapped yet: its API is whole-buffer oriented,
//! whereas the segmenter wants a per-frame decision. `vox_core::vad::EnergyVad` stands in.

use std::path::{Path, PathBuf};
use std::sync::Once;

use vox_core::{Engine, EngineError, TranscribeOptions, SAMPLE_RATE};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

/// whisper.cpp refuses (silently returns nothing for) inputs shorter than 1 s.
const MIN_AUDIO_SAMPLES: usize = (SAMPLE_RATE as usize * 11) / 10;

/// Encoder audio-context policy.
///
/// Whisper's encoder always processes a 30 s window (1500 frames) no matter how short the
/// input is, so encoder cost is a fixed ~30 s worth of work per call. `audio_ctx` truncates
/// that window; cost falls roughly in proportion. Input beyond the window is *not* seen, so
/// the context must cover the audio: 50 frames per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCtx {
    /// whisper.cpp default: the full 30 s window regardless of input length.
    Full,
    /// Fixed number of frames. Audio longer than `n / 50` seconds is cut off.
    Fixed(i32),
    /// Sized from the input: `clamp(ceil(secs * 50) + pad, min, 1500)`.
    Auto { min: i32, pad: i32 },
}

impl AudioCtx {
    pub const MAX: i32 = 1500;
    pub const FRAMES_PER_SECOND: f64 = 50.0;

    /// whisper.cpp's streaming example uses 768 fixed; for dictation segments of a few
    /// seconds, 512 minimum with ~1.3 s of slack keeps accuracy while cutting encoder cost.
    pub const DEFAULT_AUTO: AudioCtx = AudioCtx::Auto { min: 512, pad: 64 };

    /// Frames to request for `n_samples` of 16 kHz audio; `0` means "leave the default".
    pub fn for_samples(self, n_samples: usize) -> i32 {
        match self {
            AudioCtx::Full => 0,
            AudioCtx::Fixed(n) => n.clamp(1, Self::MAX),
            AudioCtx::Auto { min, pad } => {
                let secs = n_samples as f64 / SAMPLE_RATE as f64;
                let need = (secs * Self::FRAMES_PER_SECOND).ceil() as i32 + pad;
                let ctx = need.clamp(min.max(1), Self::MAX);
                if ctx >= Self::MAX {
                    0
                } else {
                    ctx
                }
            }
        }
    }
}

/// Whether any GPU backend is compiled into this build.
pub const GPU_BUILD: bool = cfg!(any(feature = "cuda", feature = "vulkan"));

#[derive(Debug, Clone)]
pub struct WhisperEngineConfig {
    pub model_path: PathBuf,
    /// Requires a GPU-enabled build (`cuda` / `vulkan` feature); ignored otherwise.
    pub use_gpu: bool,
    pub gpu_device: i32,
    /// Flash attention helps on GPU and measured ~15 % *slower* on CPU (2026-09-05), so the
    /// default is on for GPU builds only.
    pub flash_attn: bool,
    /// CPU threads. `None` = [`default_threads`].
    pub threads: Option<usize>,
    pub audio_ctx: AudioCtx,
    /// Retry decoding at higher temperatures when confidence is low. Improves hard audio,
    /// multiplies latency on the retries.
    pub temperature_fallback: bool,
}

impl WhisperEngineConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        WhisperEngineConfig {
            model_path: model_path.into(),
            use_gpu: true,
            gpu_device: 0,
            flash_attn: GPU_BUILD,
            threads: None,
            audio_ctx: AudioCtx::DEFAULT_AUTO,
            temperature_fallback: false,
        }
    }
}

/// Physical cores, capped at 16. whisper.cpp gains nothing from SMT siblings (12 physical
/// beat 8 on a 12c/24t 3900X; 24 would only add contention).
pub fn default_threads() -> usize {
    num_cpus::get_physical().clamp(1, 16)
}

pub struct WhisperEngine {
    // Field order matters: `state` must drop before `ctx`.
    state: WhisperState,
    _ctx: WhisperContext,
    threads: i32,
    flash_attn: bool,
    audio_ctx: AudioCtx,
    temperature_fallback: bool,
    name: String,
}

static LOGGING_HOOKS: Once = Once::new();

impl WhisperEngine {
    /// Loads the model. This is the slow call (hundreds of ms to seconds); do it once.
    pub fn load(cfg: &WhisperEngineConfig) -> Result<Self, EngineError> {
        // Without the log/tracing features this routes whisper.cpp's chatter to nowhere
        // instead of stderr.
        LOGGING_HOOKS.call_once(whisper_rs::install_logging_hooks);

        if !cfg.model_path.is_file() {
            return Err(EngineError::ModelNotFound(
                cfg.model_path.display().to_string(),
            ));
        }

        let use_gpu = cfg.use_gpu && GPU_BUILD;
        let mut params = WhisperContextParameters::new();
        params
            .use_gpu(use_gpu)
            .flash_attn(cfg.flash_attn)
            .gpu_device(cfg.gpu_device);

        let ctx =
            WhisperContext::new_with_params(cfg.model_path.as_path(), params).map_err(|e| {
                EngineError::Other(format!("loading {}: {e}", cfg.model_path.display()))
            })?;
        let state = ctx
            .create_state()
            .map_err(|e| EngineError::Other(format!("creating whisper state: {e}")))?;

        let name = model_name(&cfg.model_path);
        Ok(WhisperEngine {
            state,
            _ctx: ctx,
            threads: cfg.threads.unwrap_or_else(default_threads) as i32,
            flash_attn: cfg.flash_attn,
            audio_ctx: cfg.audio_ctx,
            temperature_fallback: cfg.temperature_fallback,
            name,
        })
    }

    pub fn threads(&self) -> i32 {
        self.threads
    }

    pub fn audio_ctx(&self) -> AudioCtx {
        self.audio_ctx
    }

    pub fn flash_attn(&self) -> bool {
        self.flash_attn
    }

    /// whisper.cpp version this build links against.
    pub fn whisper_cpp_version() -> &'static str {
        whisper_rs::WHISPER_CPP_VERSION
    }
}

fn model_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_start_matches("ggml-").to_string())
        .unwrap_or_else(|| path.display().to_string())
}

impl Engine for WhisperEngine {
    fn transcribe(
        &mut self,
        audio: &[f32],
        opts: &TranscribeOptions,
    ) -> Result<String, EngineError> {
        if audio.is_empty() {
            return Ok(String::new());
        }

        // Pad very short clips so whisper.cpp doesn't drop them.
        let padded;
        let pcm: &[f32] = if audio.len() < MIN_AUDIO_SAMPLES {
            let mut v = Vec::with_capacity(MIN_AUDIO_SAMPLES);
            v.extend_from_slice(audio);
            v.resize(MIN_AUDIO_SAMPLES, 0.0);
            padded = v;
            &padded
        } else {
            audio
        };

        let strategy = if opts.beam_size <= 1 {
            SamplingStrategy::Greedy { best_of: 1 }
        } else {
            SamplingStrategy::BeamSearch {
                beam_size: opts.beam_size as i32,
                patience: -1.0,
            }
        };
        let mut p = FullParams::new(strategy);
        p.set_n_threads(self.threads);
        p.set_translate(false);
        p.set_language(opts.language.as_deref());
        p.set_detect_language(opts.language.is_none());
        // We manage cross-segment context ourselves via `initial_prompt`.
        p.set_no_context(true);
        p.set_no_timestamps(true);
        p.set_single_segment(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        p.set_suppress_blank(true);
        p.set_suppress_nst(true);
        if !self.temperature_fallback {
            p.set_temperature_inc(0.0);
        }
        let ctx = self.audio_ctx.for_samples(pcm.len());
        if ctx > 0 {
            p.set_audio_ctx(ctx);
        }
        if let Some(prompt) = opts
            .initial_prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            p.set_initial_prompt(prompt);
        }

        self.state
            .full(p, pcm)
            .map_err(|e| EngineError::Inference(e.to_string()))?;

        let mut out = String::new();
        for segment in self.state.as_iter() {
            let text = segment
                .to_str_lossy()
                .map_err(|e| EngineError::Inference(format!("reading segment: {e}")))?;
            out.push_str(&text);
        }
        Ok(out)
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = SAMPLE_RATE as usize;

    #[test]
    fn model_name_strips_prefix_and_extension() {
        assert_eq!(
            model_name(Path::new("models/ggml-small.en-q5_1.bin")),
            "small.en-q5_1"
        );
        assert_eq!(
            model_name(Path::new("C:\\x\\ggml-large-v3-turbo-q5_0.bin")),
            "large-v3-turbo-q5_0"
        );
        assert_eq!(model_name(Path::new("custom.bin")), "custom");
    }

    #[test]
    fn default_threads_is_sane() {
        let t = default_threads();
        assert!((1..=16).contains(&t));
    }

    #[test]
    fn audio_ctx_full_and_fixed() {
        assert_eq!(AudioCtx::Full.for_samples(SR * 5), 0);
        assert_eq!(AudioCtx::Fixed(768).for_samples(SR * 5), 768);
        assert_eq!(AudioCtx::Fixed(9999).for_samples(SR), AudioCtx::MAX);
        assert_eq!(AudioCtx::Fixed(-3).for_samples(SR), 1);
    }

    #[test]
    fn audio_ctx_auto_scales_with_length_and_clamps() {
        let auto = AudioCtx::DEFAULT_AUTO;
        // 3 s → 150 + 64 = 214, below the 512 floor.
        assert_eq!(auto.for_samples(SR * 3), 512);
        // 12 s → 600 + 64.
        assert_eq!(auto.for_samples(SR * 12), 664);
        // 12.3 s → ceil(615) + 64 = 679.
        assert_eq!(auto.for_samples(SR * 123 / 10), 679);
        // ≥ 30 s → full window, expressed as "leave default".
        assert_eq!(auto.for_samples(SR * 29), 0);
        assert_eq!(auto.for_samples(SR * 40), 0);
    }

    #[test]
    fn missing_model_is_a_clean_error() {
        let cfg = WhisperEngineConfig::new("definitely/not/here.bin");
        match WhisperEngine::load(&cfg) {
            Err(EngineError::ModelNotFound(p)) => assert!(p.contains("not")),
            other => panic!("expected ModelNotFound, got {:?}", other.map(|_| ())),
        }
    }
}
