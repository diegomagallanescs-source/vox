//! Diagnostics exposed by `vox.exe`.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use crossbeam_channel::unbounded;
use vox_core::vad::EnergyVad;
use vox_core::{text, Engine, SAMPLE_RATE};
use vox_platform_win::{list_capture_devices, open_capture, AudioMsg};

use crate::engine::{load_engine, transcribe_opts};
use crate::paths;

/// Print config/log/model locations.
pub fn info() -> anyhow::Result<()> {
    println!("config: {}", paths::config_path().display());
    println!("log:    {}", paths::log_path().display());
    println!("models searched, in order:");
    for d in paths::model_search_dirs() {
        let mark = if d.is_dir() { "*" } else { " " };
        println!("  {mark} {}", d.display());
    }
    let cfg = paths::load_or_create_config()?;
    match paths::resolve_model(&cfg.engine.model) {
        Ok(p) => println!("model `{}` -> {}", cfg.engine.model, p.display()),
        Err(e) => println!("model `{}` -> NOT FOUND ({e})", cfg.engine.model),
    }
    println!("hotkey: {} ({:?})", cfg.hotkey.chord, cfg.hotkey.mode);
    Ok(())
}

pub fn devices() -> anyhow::Result<()> {
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
        if d.is_bluetooth {
            tags.push("bluetooth");
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags.join(", "))
        };
        println!("{}{}\n    id: {}", d.name, tags, d.id);
        if d.is_bluetooth {
            println!("    note: recording from this switches it to call mode, so its playback");
            println!("          sounds muffled while you dictate (a Bluetooth limitation).");
        }
    }
    Ok(())
}

pub fn mic_test(secs: u64) -> anyhow::Result<()> {
    let cfg = paths::load_or_create_config()?;
    let mut engine = load_engine(&cfg)?;
    let (tx, rx) = unbounded();

    println!("opening {:?} ...", cfg.audio.device);
    let t0 = Instant::now();
    let stream = open_capture(&cfg.audio.device, tx)?;
    println!(
        "recording from `{}` for {secs} s (opened in {:?}) - speak now",
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
    let rms = EnergyVad::rms(&audio);
    println!(
        "captured {} samples ({:.2} s) - peak {:.3} - rms {:.4} ({:.1} dBFS)",
        audio.len(),
        audio.len() as f32 / SAMPLE_RATE as f32,
        peak,
        rms,
        20.0 * rms.max(1e-9).log10()
    );
    if peak < 0.001 {
        println!("warning: the capture is silent - wrong device, or the mic is muted");
    }

    let t1 = Instant::now();
    let out = engine.transcribe(&audio, &transcribe_opts(&cfg))?;
    println!("transcribed in {:?}: {:?}", t1.elapsed(), text::clean(&out));
    Ok(())
}

pub fn transcribe(path: &str) -> anyhow::Result<()> {
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
    let out = engine.transcribe(&audio, &transcribe_opts(&cfg))?;
    println!("{:?} -> {}", t0.elapsed(), text::clean(&out));
    Ok(())
}
