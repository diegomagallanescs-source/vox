//! voxd — the Vox daemon.
//!
//! ```text
//! voxd                 run the daemon (tray icon, hotkey, dictation)
//! voxd devices         list capture devices and which are the Windows defaults
//! voxd mic-test [SECS] record from the configured mic for SECS (default 4) and transcribe
//! voxd transcribe WAV  run a 16 kHz WAV through the engine (no hooks, no injection)
//! ```
//!
//! Phase 1 keeps the console subsystem so logs are visible; set `RUST_LOG=debug` for
//! per-segment timings.

mod coordinator;
mod paths;

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use crossbeam_channel::{unbounded, Receiver, Sender};
use tracing_subscriber::EnvFilter;
use vox_core::{Config, Engine, TranscribeOptions, SAMPLE_RATE};
use vox_engine_whisper::{WhisperEngine, WhisperEngineConfig};
use vox_platform_win::{
    list_capture_devices, message_thread, open_capture, AudioMsg, SingleInstance,
};

use coordinator::{Channels, Coordinator, Job, JobResult};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => run_daemon(),
        Some("devices") => cmd_devices(),
        Some("mic-test") => cmd_mic_test(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4)),
        Some("transcribe") => cmd_transcribe(
            args.get(1)
                .ok_or_else(|| anyhow!("transcribe needs a WAV path"))?,
        ),
        Some("-h" | "--help") => {
            println!("voxd | voxd devices | voxd mic-test [SECS] | voxd transcribe FILE.wav");
            Ok(())
        }
        Some(other) => Err(anyhow!("unknown command `{other}`")),
    }
}

// ---------------------------------------------------------------------------------------------

fn engine_config(cfg: &Config) -> anyhow::Result<WhisperEngineConfig> {
    let path = paths::resolve_model(&cfg.engine.model)?;
    let mut ec = WhisperEngineConfig::new(path);
    ec.threads = cfg.engine.threads;
    ec.use_gpu = !matches!(cfg.engine.backend, vox_core::config::Backend::Cpu);
    Ok(ec)
}

fn load_engine(cfg: &Config) -> anyhow::Result<WhisperEngine> {
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

fn transcribe_opts(cfg: &Config) -> TranscribeOptions {
    TranscribeOptions {
        language: Some(cfg.engine.language.clone()),
        initial_prompt: None,
        beam_size: cfg.engine.beam_size,
    }
}

/// Inference thread: one engine, jobs in order, results in order.
fn spawn_inference(
    cfg: Config,
    preloaded: Option<WhisperEngine>,
    jobs_rx: Receiver<Job>,
    results_tx: Sender<JobResult>,
) {
    std::thread::Builder::new()
        .name("vox-inference".into())
        .spawn(move || {
            let base_opts = transcribe_opts(&cfg);
            let mut engine = preloaded;
            for job in jobs_rx {
                if engine.is_none() {
                    match load_engine(&cfg) {
                        Ok(e) => engine = Some(e),
                        Err(e) => {
                            tracing::error!("cannot load model: {e:#}");
                            let _ = results_tx.send(JobResult {
                                generation: job.generation,
                                text: String::new(),
                                is_final: job.is_final,
                                audio_ms: 0,
                                elapsed: Duration::ZERO,
                            });
                            continue;
                        }
                    }
                }
                let engine = engine.as_mut().expect("engine loaded above");
                let opts = TranscribeOptions {
                    initial_prompt: job.prompt.clone(),
                    ..base_opts.clone()
                };
                let t0 = Instant::now();
                let text = match engine.transcribe(&job.samples, &opts) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("transcription failed: {e}");
                        String::new()
                    }
                };
                let _ = results_tx.send(JobResult {
                    generation: job.generation,
                    text,
                    is_final: job.is_final,
                    audio_ms: (job.samples.len() * 1000 / SAMPLE_RATE as usize) as u32,
                    elapsed: t0.elapsed(),
                });
            }
        })
        .expect("spawn inference thread");
}

fn run_daemon() -> anyhow::Result<()> {
    let Some(_instance) = SingleInstance::acquire()? else {
        eprintln!("voxd is already running (check the tray).");
        return Ok(());
    };

    let cfg = paths::load_or_create_config()?;
    tracing::info!("config: {}", paths::config_path().display());

    // Fail fast on a missing model, before any tray icon appears.
    let preloaded = if cfg.engine.preload {
        Some(load_engine(&cfg)?)
    } else {
        engine_config(&cfg)?;
        None
    };

    let (hotkey_tx, hotkey_rx) = unbounded();
    let (tray_tx, tray_rx) = unbounded();
    let (audio_tx, audio_rx) = unbounded();
    let (jobs_tx, jobs_rx) = unbounded();
    let (results_tx, results_rx) = unbounded();

    spawn_inference(cfg.clone(), preloaded, jobs_rx, results_tx);

    let msg = message_thread::spawn(message_thread::Options {
        chord: cfg.hotkey.chord,
        hotkey_tx,
        tray_tx,
        tooltip: "Vox — starting…".into(),
    })?;

    let channels = Channels {
        hotkey_rx,
        audio_tx,
        audio_rx,
        jobs_tx,
        results_rx,
        tray_rx,
    };
    Coordinator::new(cfg, channels, msg).run();
    Ok(())
}

// ---------------------------------------------------------------------------------------------

fn cmd_devices() -> anyhow::Result<()> {
    let devices = list_capture_devices()?;
    if devices.is_empty() {
        println!("no active capture devices");
        return Ok(());
    }
    for d in devices {
        let mut tags = Vec::new();
        if d.is_default_communications {
            tags.push("default communications");
        }
        if d.is_default_console {
            tags.push("default");
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        println!("{}{}\n    id: {}", d.name, tags, d.id);
    }
    Ok(())
}

fn cmd_mic_test(secs: u64) -> anyhow::Result<()> {
    let cfg = paths::load_or_create_config()?;
    let mut engine = load_engine(&cfg)?;
    let (tx, rx) = unbounded();

    println!("opening {:?} …", cfg.audio.device);
    let t0 = Instant::now();
    let stream = open_capture(&cfg.audio.device, tx)?;
    println!(
        "recording from `{}` for {secs} s (opened in {:?}) — speak now",
        stream.device_name,
        t0.elapsed()
    );

    let mut audio: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize * secs as usize);
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(AudioMsg::Frames(f)) => audio.extend_from_slice(&f),
            Ok(AudioMsg::Error(e)) => return Err(anyhow!("capture error: {e}")),
            Err(_) => break,
        }
    }
    drop(stream);

    let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let rms = vox_core::vad::EnergyVad::rms(&audio);
    println!(
        "captured {} samples ({:.2} s) · peak {:.3} · rms {:.4} ({:.1} dBFS)",
        audio.len(),
        audio.len() as f32 / SAMPLE_RATE as f32,
        peak,
        rms,
        20.0 * rms.max(1e-9).log10()
    );
    if peak < 0.001 {
        println!("warning: the capture is silent — wrong device, or the mic is muted");
    }

    let t1 = Instant::now();
    let text = engine.transcribe(&audio, &transcribe_opts(&cfg))?;
    println!(
        "transcribed in {:?}: {:?}",
        t1.elapsed(),
        vox_core::text::clean(&text)
    );
    Ok(())
}

fn cmd_transcribe(path: &str) -> anyhow::Result<()> {
    let cfg = paths::load_or_create_config()?;
    let mut engine = load_engine(&cfg)?;
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {path}"))?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE {
        return Err(anyhow!(
            "{path}: {} Hz, need {} Hz",
            spec.sample_rate,
            SAMPLE_RATE
        ));
    }
    let channels = spec.channels as usize;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u32 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 * scale)
                .collect()
        }
    };
    let audio: Vec<f32> = if channels == 1 {
        raw
    } else {
        raw.chunks(channels)
            .map(|c| c.iter().sum::<f32>() / channels as f32)
            .collect()
    };
    let t0 = Instant::now();
    let text = engine.transcribe(&audio, &transcribe_opts(&cfg))?;
    println!("{:?} → {}", t0.elapsed(), vox_core::text::clean(&text));
    Ok(())
}
