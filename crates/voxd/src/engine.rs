//! Engine construction from config, and the inference thread.

use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use vox_core::config::Backend;
use vox_core::{Config, Engine, TranscribeOptions, SAMPLE_RATE};
use vox_engine_whisper::{WhisperEngine, WhisperEngineConfig};

use crate::coordinator::{Job, JobResult};
use crate::paths;

pub fn engine_config(cfg: &Config) -> anyhow::Result<WhisperEngineConfig> {
    let path = paths::resolve_model(&cfg.engine.model)?;
    let mut ec = WhisperEngineConfig::new(path);
    ec.threads = cfg.engine.threads;
    ec.use_gpu = !matches!(cfg.engine.backend, Backend::Cpu);
    Ok(ec)
}

pub fn load_engine(cfg: &Config) -> anyhow::Result<WhisperEngine> {
    let ec = engine_config(cfg)?;
    let t0 = Instant::now();
    let engine =
        WhisperEngine::load(&ec).with_context(|| format!("loading {}", ec.model_path.display()))?;
    tracing::info!(
        "model {} loaded in {:?} ({} threads, whisper.cpp {})",
        engine.name(),
        t0.elapsed(),
        engine.threads(),
        WhisperEngine::whisper_cpp_version()
    );
    Ok(engine)
}

pub fn transcribe_opts(cfg: &Config) -> TranscribeOptions {
    TranscribeOptions {
        language: Some(cfg.engine.language.clone()),
        initial_prompt: None,
        beam_size: cfg.engine.beam_size,
    }
}

/// Messages to the inference thread.
pub enum InferenceMsg {
    Job(Job),
    /// Config changed (model, threads, language…): drop the engine and load again.
    Reload(Config),
}

/// Messages from the inference thread.
pub enum InferenceEvent {
    Result(JobResult),
    /// Engine (re)loaded; payload is the model name.
    Loaded(String),
    /// Engine could not be loaded; dictation will produce nothing until fixed.
    Failed(String),
}

/// Inference thread: one engine, jobs in order, results in order. Loads the model lazily
/// if `preloaded` is `None`.
pub fn spawn_inference(
    mut cfg: Config,
    preloaded: Option<WhisperEngine>,
    rx: Receiver<InferenceMsg>,
    tx: Sender<InferenceEvent>,
) {
    std::thread::Builder::new()
        .name("vox-inference".into())
        .spawn(move || {
            let mut engine = preloaded;
            if let Some(e) = &engine {
                let _ = tx.send(InferenceEvent::Loaded(e.name().to_string()));
            }
            for msg in rx {
                let job = match msg {
                    InferenceMsg::Reload(new_cfg) => {
                        cfg = new_cfg;
                        engine = None;
                        match load_engine(&cfg) {
                            Ok(e) => {
                                let _ = tx.send(InferenceEvent::Loaded(e.name().to_string()));
                                engine = Some(e);
                            }
                            Err(e) => {
                                let _ = tx.send(InferenceEvent::Failed(format!("{e:#}")));
                            }
                        }
                        continue;
                    }
                    InferenceMsg::Job(job) => job,
                };

                if engine.is_none() {
                    match load_engine(&cfg) {
                        Ok(e) => {
                            let _ = tx.send(InferenceEvent::Loaded(e.name().to_string()));
                            engine = Some(e);
                        }
                        Err(e) => {
                            let _ = tx.send(InferenceEvent::Failed(format!("{e:#}")));
                            let _ = tx.send(InferenceEvent::Result(JobResult {
                                generation: job.generation,
                                text: String::new(),
                                is_final: job.is_final,
                                audio_ms: 0,
                                elapsed: Duration::ZERO,
                            }));
                            continue;
                        }
                    }
                }
                let engine = engine.as_mut().expect("engine loaded above");
                let opts = TranscribeOptions {
                    initial_prompt: job.prompt.clone(),
                    ..transcribe_opts(&cfg)
                };
                let t0 = Instant::now();
                let text = match engine.transcribe(&job.samples, &opts) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("transcription failed: {e}");
                        String::new()
                    }
                };
                let _ = tx.send(InferenceEvent::Result(JobResult {
                    generation: job.generation,
                    text,
                    is_final: job.is_final,
                    audio_ms: (job.samples.len() * 1000 / SAMPLE_RATE as usize) as u32,
                    elapsed: t0.elapsed(),
                }));
            }
        })
        .expect("spawn inference thread");
}
