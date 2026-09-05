# Benchmarks

Results from `vox-bench` (see `crates/vox-bench`). Reproduce with the commands shown; numbers
are medians unless stated. Keep this file append-only per date so trends stay visible.

## 2026-09-05 — Phase 0, CPU only

**Machine:** Ryzen 9 3900X (12c/24t), 32 GB, Windows 11. GPU backends not yet built.
**Build:** whisper.cpp 1.8.3 via whisper-rs 0.16, MSVC 2019, `/O2`, AVX2+FMA+F16C
(`.cargo/config.toml`). **Clip:** `assets/bench/tts-en.wav`, 11.97 s Windows TTS, 34 words.
The 2.9 % "WER" floor is `three` → `3`; 5.9 % is one real error (`the` → `a`).

Segments in the "segmented" runs are what the real pipeline produces from this clip:
a 3.27 s and an 8.61 s segment. **Tail latency** — what the user feels on key release — is the
time to transcribe a segment of the length they just spoke, so the 3.3 s column is the
headline number for typical dictation.

### Latency (ms) — whole 12 s clip / 3.3 s segment / 8.6 s segment

| Configuration | base.en q5_1 | small.en q5_1 | large-v3-turbo q5_0 |
|---|---|---|---|
| *unoptimised build (scalar, no `/O2`)* | 9290 / 8864 / 9366 | 34680 / 32998 / 33803 | (aborted) |
| full audio_ctx · 8 thr · flash-attn | 1130 / 1004 / 1182 | 3684 / 3375 / 3490 | 15943 / 15766 / 16996 |
| auto audio_ctx · 8 thr · flash-attn | 524 / 274 / 365 | 1458 / 870 / 1020 | 5881 / 3894 / 4067 |
| auto audio_ctx · 12 thr · flash-attn | 471 / 232 / 339 | 1382 / 746 / 904 | — |
| **auto audio_ctx · 12 thr · no flash-attn** | **410 / 212 / 312** | **1151 / 665 / 846** | — |
| auto (384 floor) · 12 thr · flash-attn | 461 / 190 / 345 | 1291 / 571 / 878 ⚠ WER 5.9 % | — |

Model load: base 90 ms · small 210 ms · turbo 530 ms (from SSD, warm cache).

### What it means

1. **The build flags were the first-order problem.** The `cmake` crate dropped `/O2` and ggml's
   `GGML_NATIVE` is a no-op on MSVC → a scalar, unoptimised whisper.cpp, ~20× slower. Fixed in
   `.cargo/config.toml`; anyone building on Windows must go through `scripts\cargo-msvc.cmd`.
2. **Encoder cost is fixed per call unless `audio_ctx` is sized to the input.** With the full
   context a 3.3 s segment cost the same as the 12 s clip, which defeats incremental
   segmentation. Auto-sizing (`clamp(⌈secs·50⌉+64, 512, 1500)`) cut latency 2–4× with no
   accuracy change. A 384 floor introduced an error; **512 is the floor.**
3. **Threads = physical cores.** 12 beat 8 by ~10 %. SMT siblings do not help.
4. **Flash attention is a GPU feature.** ~15 % slower on CPU. Default off for CPU builds.
5. **`large-v3-turbo` is GPU-only.** Turbo shrinks the decoder, not the 32-layer encoder;
   4–6 s per pass on CPU even with auto context.

### Resulting CPU tiers

| Tier | Model | Tail latency, 3 s utterance | Verdict |
|---|---|---|---|
| CPU, ≥ 8 physical cores | `small.en` q5_1 | ~650 ms | usable; slightly better accuracy |
| CPU, any | `base.en` q5_1 | ~210 ms | **default for CPU** — feels instant |
| CPU | `large-v3-turbo` | ~4 s | not viable |

GPU tiers (CUDA / Vulkan on the GTX 1070 Ti) are the next measurement.

### Reproduce

```bash
scripts\cargo-msvc.cmd build --release -p vox-bench
target\release\vox-bench.exe --cpu --runs 3
target\release\vox-bench.exe --cpu --runs 3 --audio-ctx full --threads 8 --flash-attn   # the old baseline
```
