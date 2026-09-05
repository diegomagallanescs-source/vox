# Vox

Local, private push-to-talk dictation for Windows. Hold a key or mouse button, talk, release —
the text lands in whatever has focus. Runs entirely on your machine (whisper.cpp on CUDA /
Vulkan / CPU). No account, no cloud, no subscription.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

## Layout

```
crates/vox-core            pure logic + tests (no Win32)
crates/vox-engine-whisper  whisper.cpp engine
crates/vox-platform-win    WASAPI, hooks, SendInput, tray
crates/vox-ipc             daemon <-> UI protocol
crates/voxd                background daemon (bin)
crates/vox-ui              settings window (bin)
```

## Build

```bash
scripts\cargo-msvc.cmd build --release
```

The wrapper picks a Visual Studio install that has the C++ x64 tools and points bindgen at
LLVM; `.cargo/config.toml` carries the CMake flags that make whisper.cpp fast on MSVC. Plain
`cargo build` may produce a working but ~20× slower binary — see `docs/benchmarks.md`.

Prerequisites: Rust (stable, MSVC target), MSVC Build Tools (C++ x64), CMake, LLVM (for
`libclang.dll`). CUDA Toolkit 12.x for the `cuda` feature; Vulkan SDK for `vulkan`.

Models go in `models/` (git-ignored), e.g. `ggml-base.en-q5_1.bin` from
`huggingface.co/ggerganov/whisper.cpp`.

## Run the daemon (phase 1)

```bash
target\release\voxd.exe
```

First run writes `%APPDATA%\Vox\config.toml` (default hotkey `F13`, push-to-talk). Edit
`chord` to something you can press — `"Mouse4"`, `"RCtrl"`, `"Ctrl+Shift+Space"` — and
restart. A tray icon appears; hold the key, talk, release. `RUST_LOG=debug` prints per-segment
timings.

```bash
target\release\voxd.exe devices        # capture devices + which are the Windows defaults
target\release\voxd.exe mic-test 4     # record 4 s from the configured mic and transcribe
target\release\voxd.exe transcribe assets/bench/tts-en.wav
```

## Benchmark

```bash
scripts\cargo-msvc.cmd build --release -p vox-bench
target\release\vox-bench.exe --cpu --runs 3
```
