# Vox

Local, private push-to-talk dictation for Windows. Hold a key or mouse button, talk, release —
the text lands in whatever has focus. Runs entirely on your machine (whisper.cpp on CUDA /
Vulkan / CPU). No account, no cloud, no subscription.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

## Layout

```
crates/vox-core            pure logic + tests (no Win32)
crates/vox-engine-whisper  whisper.cpp engine
crates/vox-platform-win    WASAPI, input hooks, SendInput, autostart
crates/vox-bench           latency/accuracy harness
crates/voxd                voxd.exe (Tauri app + daemon threads), vox.exe (CLI)
crates/voxd/ui             the settings frontend — plain HTML/CSS/JS, no build step
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

## Run

```bash
target\release\voxd.exe
```

The settings window opens and a tray icon appears. Set the hotkey (click **Change…** and press
the key or mouse side button you want), pick a microphone, then hold the key, talk, and
release — the text lands in whatever had focus.

Closing the window leaves Vox listening in the tray; click the tray icon to bring it back,
right-click for Quit. Started at login it stays in the tray without opening the window.

Config: `%APPDATA%\Vox\config.toml` · logs: `%LOCALAPPDATA%\Vox\logs\voxd.log` (also printed
when run from a terminal; `RUST_LOG=debug` adds per-segment timings). Start-up failures show
a message box.

Diagnostics live in the console CLI `vox.exe` (a windowed exe can't print to a terminal in
a way shells wait for):

```bash
target\release\vox.exe info            # config / log / model locations, configured hotkey
target\release\vox.exe devices         # capture devices + which are the Windows defaults
target\release\vox.exe mic-test 4      # record 4 s from the configured mic and transcribe
target\release\vox.exe transcribe assets/bench/tts-en.wav
```

## Benchmark

```bash
scripts\cargo-msvc.cmd build --release -p vox-bench
target\release\vox-bench.exe --cpu --runs 3
```
