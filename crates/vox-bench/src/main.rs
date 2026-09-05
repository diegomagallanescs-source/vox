//! Phase-0 benchmark: does the latency story hold on this machine?
//!
//! For each model it measures
//!   * load time,
//!   * whole-clip transcription latency (min / median / max over N runs) and WER, and
//!   * a simulated push-to-talk session: the clip is fed in 30 ms frames through the real
//!     `EnergyVad` + `Segmenter`, intermediate segments are transcribed as they close (in a
//!     real session this overlaps with the user still talking), and the **tail latency** —
//!     time from "release" to final text — is what the user actually feels.
//!
//! Usage:
//!   vox-bench [--model PATH]... [--wav PATH] [--expected PATH] [--runs N] [--threads N]
//!             [--beam N] [--audio-ctx N] [--cpu] [--no-flash-attn] [--mode whole|segmented|both]
//!             [--pause-ms N] [--min-segment-ms N]
//! With no `--model`, every `models/*.bin` is used.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vox_core::traits::Vad;
use vox_core::vad::EnergyVad;
use vox_core::{text, Engine, Segmenter, SegmenterConfig, TranscribeOptions, SAMPLE_RATE};
use vox_engine_whisper::{AudioCtx, WhisperEngine, WhisperEngineConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Whole,
    Segmented,
    Both,
}

struct Args {
    models: Vec<PathBuf>,
    wav: PathBuf,
    expected: Option<PathBuf>,
    runs: usize,
    threads: Option<usize>,
    beam: u32,
    audio_ctx: AudioCtx,
    use_gpu: bool,
    /// `None` = engine default (on for GPU builds, off for CPU).
    flash_attn: Option<bool>,
    mode: Mode,
    segmenter: SegmenterConfig,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        models: Vec::new(),
        wav: PathBuf::from("assets/bench/tts-en.wav"),
        expected: None,
        runs: 5,
        threads: None,
        beam: 1,
        audio_ctx: AudioCtx::DEFAULT_AUTO,
        use_gpu: true,
        flash_attn: None,
        mode: Mode::Both,
        segmenter: SegmenterConfig::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => a.models.push(PathBuf::from(value()?)),
            "--wav" => a.wav = PathBuf::from(value()?),
            "--expected" => a.expected = Some(PathBuf::from(value()?)),
            "--runs" => a.runs = value()?.parse().map_err(|e| format!("--runs: {e}"))?,
            "--threads" => {
                a.threads = Some(value()?.parse().map_err(|e| format!("--threads: {e}"))?)
            }
            "--beam" => a.beam = value()?.parse().map_err(|e| format!("--beam: {e}"))?,
            "--audio-ctx" => a.audio_ctx = parse_audio_ctx(&value()?)?,
            "--cpu" => a.use_gpu = false,
            "--flash-attn" => a.flash_attn = Some(true),
            "--no-flash-attn" => a.flash_attn = Some(false),
            "--mode" => {
                a.mode = match value()?.as_str() {
                    "whole" => Mode::Whole,
                    "segmented" => Mode::Segmented,
                    "both" => Mode::Both,
                    other => return Err(format!("--mode: unknown `{other}`")),
                }
            }
            "--pause-ms" => {
                a.segmenter.pause_ms = value()?.parse().map_err(|e| format!("--pause-ms: {e}"))?
            }
            "--min-segment-ms" => {
                a.segmenter.min_segment_ms = value()?
                    .parse()
                    .map_err(|e| format!("--min-segment-ms: {e}"))?
            }
            "-h" | "--help" => {
                eprintln!("{}", USAGE);
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag `{other}`\n{USAGE}")),
        }
    }
    if a.models.is_empty() {
        a.models = default_models()?;
    }
    if a.expected.is_none() {
        let txt = a.wav.with_extension("txt");
        if txt.is_file() {
            a.expected = Some(txt);
        }
    }
    Ok(a)
}

const USAGE: &str =
    "vox-bench [--model PATH]... [--wav PATH] [--expected PATH] [--runs N] [--threads N] \
[--beam N] [--audio-ctx full|auto|auto:MIN|N] [--cpu] [--flash-attn|--no-flash-attn] \
[--mode whole|segmented|both] [--pause-ms N] [--min-segment-ms N]";

/// `full` · `auto` (512 floor, 64 pad) · `auto:MIN` · a fixed frame count.
fn parse_audio_ctx(s: &str) -> Result<AudioCtx, String> {
    match s {
        "full" | "0" => Ok(AudioCtx::Full),
        "auto" => Ok(AudioCtx::DEFAULT_AUTO),
        _ => {
            if let Some(min) = s.strip_prefix("auto:") {
                let min = min.parse().map_err(|e| format!("--audio-ctx {s}: {e}"))?;
                Ok(AudioCtx::Auto { min, pad: 64 })
            } else {
                s.parse()
                    .map(AudioCtx::Fixed)
                    .map_err(|e| format!("--audio-ctx {s}: {e}"))
            }
        }
    }
}

fn describe_audio_ctx(c: AudioCtx) -> String {
    match c {
        AudioCtx::Full => "full".into(),
        AudioCtx::Fixed(n) => n.to_string(),
        AudioCtx::Auto { min, pad } => format!("auto(min {min}, pad {pad})"),
    }
}

fn default_models() -> Result<Vec<PathBuf>, String> {
    let dir = Path::new("models");
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("no --model given and cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    if out.is_empty() {
        return Err("no --model given and models/ has no .bin files".into());
    }
    // Smallest first so failures show up fast.
    out.sort_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX));
    Ok(out)
}

// ---------------------------------------------------------------------------------------------

fn read_wav_16k_mono(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE {
        return Err(format!(
            "{}: sample rate is {} Hz, bench expects {} Hz",
            path.display(),
            spec.sample_rate,
            SAMPLE_RATE
        ));
    }
    let channels = spec.channels as usize;
    let mono = |frames: Vec<f32>| -> Vec<f32> {
        if channels == 1 {
            frames
        } else {
            frames
                .chunks(channels)
                .map(|c| c.iter().sum::<f32>() / channels as f32)
                .collect()
        }
    };
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u32 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 * scale)
                .collect()
        }
    };
    Ok(mono(samples))
}

fn normalize_words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(|c| c.to_lowercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Word error rate = (substitutions + deletions + insertions) / reference words.
fn wer(reference: &str, hypothesis: &str) -> f32 {
    let r = normalize_words(reference);
    let h = normalize_words(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    let mut cur = vec![0usize; h.len() + 1];
    for i in 1..=r.len() {
        cur[0] = i;
        for j in 1..=h.len() {
            let sub = prev[j - 1] + usize::from(r[i - 1] != h[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[h.len()] as f32 / r.len() as f32
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn summarize(times: &[Duration]) -> (f64, f64, f64) {
    let mut v: Vec<f64> = times.iter().map(|d| ms(*d)).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    };
    (v[0], median, v[v.len() - 1])
}

// ---------------------------------------------------------------------------------------------

fn bench_whole(
    engine: &mut WhisperEngine,
    audio: &[f32],
    runs: usize,
    opts: &TranscribeOptions,
) -> (Vec<Duration>, String) {
    // Warm-up: first call pays one-off allocation costs.
    let _ = engine.transcribe(audio, opts);
    let mut times = Vec::with_capacity(runs);
    let mut last = String::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        last = engine
            .transcribe(audio, opts)
            .unwrap_or_else(|e| format!("<error: {e}>"));
        times.push(t0.elapsed());
    }
    (times, text::clean(&last))
}

struct SegmentedReport {
    /// (audio ms, transcribe ms) per intermediate segment.
    intermediate: Vec<(f64, f64)>,
    /// (audio ms, transcribe ms) for the final segment — the latency the user feels.
    tail: Option<(f64, f64)>,
    text: String,
}

fn bench_segmented(
    engine: &mut WhisperEngine,
    audio: &[f32],
    opts: &TranscribeOptions,
    seg_cfg: &SegmenterConfig,
) -> SegmentedReport {
    const FRAME: usize = SAMPLE_RATE as usize * 30 / 1000;
    let mut vad = EnergyVad::default();
    let mut segmenter = Segmenter::new(seg_cfg.clone(), SAMPLE_RATE);
    let mut parts: Vec<String> = Vec::new();
    let mut intermediate = Vec::new();
    let mut prompt: Option<String> = None;

    let mut run = |samples: &[f32], prompt: &Option<String>| -> (f64, f64, String) {
        let o = TranscribeOptions {
            initial_prompt: prompt.clone(),
            ..opts.clone()
        };
        let t0 = Instant::now();
        let out = engine
            .transcribe(samples, &o)
            .unwrap_or_else(|e| format!("<error: {e}>"));
        let dt = ms(t0.elapsed());
        (
            samples.len() as f64 * 1000.0 / SAMPLE_RATE as f64,
            dt,
            text::clean(&out),
        )
    };

    for frame in audio.chunks(FRAME) {
        let speech = vad.is_speech(frame);
        if let Some(seg) = segmenter.push(frame, speech) {
            let (audio_ms, dt, t) = run(&seg.samples, &prompt);
            intermediate.push((audio_ms, dt));
            if !t.is_empty() {
                prompt = Some(t.clone());
                parts.push(t);
            }
        }
    }
    let tail = segmenter.finish().map(|seg| {
        let (audio_ms, dt, t) = run(&seg.samples, &prompt);
        if !t.is_empty() {
            parts.push(t);
        }
        (audio_ms, dt)
    });

    SegmentedReport {
        intermediate,
        tail,
        text: text::join(&parts),
    }
}

// ---------------------------------------------------------------------------------------------

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vox-bench: {e}");
            std::process::exit(2);
        }
    };

    let audio = match read_wav_16k_mono(&args.wav) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vox-bench: {e}");
            std::process::exit(2);
        }
    };
    let audio_secs = audio.len() as f64 / SAMPLE_RATE as f64;
    let expected = args
        .expected
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string());

    let opts = TranscribeOptions {
        language: Some("en".into()),
        initial_prompt: None,
        beam_size: args.beam,
    };

    println!(
        "vox-bench — whisper.cpp {}",
        WhisperEngine::whisper_cpp_version()
    );
    println!(
        "clip: {} ({audio_secs:.2} s) · runs: {} · beam: {} · audio_ctx: {} · gpu: {} · flash_attn: {}",
        args.wav.display(),
        args.runs,
        args.beam,
        describe_audio_ctx(args.audio_ctx),
        args.use_gpu,
        args.flash_attn.map_or("auto".to_string(), |b| b.to_string())
    );
    if let Some(e) = &expected {
        println!("expected: {e}");
    }
    println!();

    for model in &args.models {
        let mut cfg = WhisperEngineConfig::new(model);
        cfg.use_gpu = args.use_gpu;
        if let Some(flash) = args.flash_attn {
            cfg.flash_attn = flash;
        }
        cfg.threads = args.threads;
        cfg.audio_ctx = args.audio_ctx;

        let t0 = Instant::now();
        let mut engine = match WhisperEngine::load(&cfg) {
            Ok(e) => e,
            Err(e) => {
                println!("== {} — FAILED to load: {e}\n", model.display());
                continue;
            }
        };
        let load_ms = ms(t0.elapsed());
        println!(
            "== {}  (load {load_ms:.0} ms, {} threads, flash_attn {})",
            engine.name(),
            engine.threads(),
            engine.flash_attn()
        );

        if matches!(args.mode, Mode::Whole | Mode::Both) {
            let (times, out) = bench_whole(&mut engine, &audio, args.runs, &opts);
            let (min, med, max) = summarize(&times);
            let rtf = med / 1000.0 / audio_secs;
            print!("  whole clip   min {min:>7.0} ms · median {med:>7.0} ms · max {max:>7.0} ms · RTF {rtf:.2}");
            if let Some(e) = &expected {
                print!(" · WER {:.1}%", wer(e, &out) * 100.0);
            }
            println!();
            println!("               → {out}");
        }

        if matches!(args.mode, Mode::Segmented | Mode::Both) {
            let rep = bench_segmented(&mut engine, &audio, &opts, &args.segmenter);
            let busy: f64 = rep.intermediate.iter().map(|(_, dt)| dt).sum();
            let covered: f64 = rep.intermediate.iter().map(|(a, _)| a).sum();
            println!(
                "  segmented    {} intermediate segment(s): {}",
                rep.intermediate.len(),
                rep.intermediate
                    .iter()
                    .map(|(a, dt)| format!("{a:.0}ms→{dt:.0}ms"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if covered > 0.0 {
                println!(
                    "               keep-up ratio {:.2} (transcribe time / speech time while talking; must stay < 1)",
                    busy / covered
                );
            }
            match rep.tail {
                Some((a, dt)) => {
                    print!("               TAIL LATENCY {dt:.0} ms for a {a:.0} ms final segment")
                }
                None => print!("               no tail segment"),
            }
            if let Some(e) = &expected {
                print!(" · WER {:.1}%", wer(e, &rep.text) * 100.0);
            }
            println!();
            println!("               → {}", rep.text);
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_basics() {
        assert_eq!(wer("hello world", "hello world"), 0.0);
        assert_eq!(wer("Hello, World!", "hello world"), 0.0);
        assert!((wer("a b c d", "a b x d") - 0.25).abs() < 1e-6);
        assert!((wer("a b c d", "a b d") - 0.25).abs() < 1e-6);
        assert!((wer("a b c d", "a b c d e") - 0.25).abs() < 1e-6);
        assert_eq!(wer("", ""), 0.0);
        assert_eq!(wer("", "x"), 1.0);
    }

    #[test]
    fn summarize_median() {
        let d = |ms: u64| Duration::from_millis(ms);
        assert_eq!(summarize(&[d(30), d(10), d(20)]), (10.0, 20.0, 30.0));
        assert_eq!(summarize(&[d(10), d(20)]), (10.0, 15.0, 20.0));
    }
}
