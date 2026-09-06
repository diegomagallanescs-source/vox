# Vox — Architecture

Local, private, push-to-talk dictation for Windows. Hold a key (or a mouse button),
talk, release, and the text appears in whatever field has focus. Everything runs on
your machine; no account, no subscription, no network.

Target machine (development): Ryzen 9 3900X · 32 GB RAM · GTX 1070 Ti 8 GB (Pascal) · Windows 11.
Must also run acceptably on weaker machines, including CPU-only laptops.

## Priorities (in order)

1. **Always listening while "closed".** The hotkey works with no window open, from login.
2. **Fast.** End-of-speech → text on screen in well under a second on the dev machine,
   otherwise typing wins and the app has no reason to exist.
3. **Mic control.** Pick a device in the settings UI, or follow the system default so
   AirPods / AirPods Max are used automatically when they connect.
4. **Efficient.** Near-zero cost when idle; no background CPU, tiny RAM.
5. **Clean.** Small dependency surface, testable core, obvious module boundaries.

---

## 1. Process model — one process, window on demand

"Listens while the app is closed" on Windows means the *listening* must live in something
that is not the window you close. A Windows Service cannot do it (services run in session 0
and cannot see the desktop's keyboard or type into it).

The original design used two processes (headless daemon + separate settings exe over a named
pipe). **Phase 2 replaced that with a single Tauri process**, because Tauri creates the
webview when the window opens and destroys it when the window closes — the same "UI costs
nothing while closed" property, without a second binary or an IPC protocol to maintain.

```
┌──────────────────────────────────────────────────────────────┐
│ voxd.exe                                                     │
│                                                              │
│  main thread ── Tauri event loop: tray icon, settings window │
│                 (webview exists only while the window is up) │
│  vox-hooks    ── WH_KEYBOARD_LL / WH_MOUSE_LL + message loop  │
│  vox-coordinator ── session state machine, the real work      │
│  vox-inference   ── whisper.cpp, one model, jobs in order     │
│  vox-capture     ── WASAPI, only while recording              │
└──────────────────────────────────────────────────────────────┘
```

* The **daemon threads** are the product; the window is a view onto them. Closing it leaves
  dictation running. Quit lives in the tray menu (and the window footer).
* Idle target: **< 20 MB private RAM, 0 % CPU** with the window closed (plus the model if
  `preload` is on). Purely event-driven — no polling.
* A named mutex enforces a single instance; a second launch shows a message box pointing at
  the tray.
* The **coordinator owns the config**. The UI sends whole `Config` values; the coordinator
  validates, applies, and writes `config.toml`. The UI never writes it directly.
* Launched by hand → the window opens. Launched at login → the autostart entry passes
  `--minimized`, so it stays in the tray.

---

## 2. Language & frameworks

**Rust for everything.**

| Option | Idle RAM | Effort | Verdict |
|---|---|---|---|
| Rust | ~10 MB | medium | **chosen** — no runtime/GC, small binaries, `whisper-rs` + `windows` crate cover every need |
| C# / .NET NativeAOT | 30–60 MB | medium-low | viable; heavier idle, two runtimes to reason about |
| C++ | ~5 MB | high | max control, max footguns; no real win over Rust |
| Python | 150–300 MB | low | fails the efficiency priority outright |

* **Win32 access:** the `windows` crate, used directly (WASAPI, low-level hooks, `SendInput`,
  clipboard, registry, device notifications). One dependency for the whole platform layer,
  full control, no abstraction gaps (e.g. `cpal` cannot deliver device-change events).
* **Settings UI:** **Tauri 2** (WebView2 + HTML/CSS/JS). `egui` was the original choice for
  its small size, but it cannot produce the intended look; WebView2 ships with Windows 11, so
  the runtime is already present and the binary only grows by ~4 MB. The frontend is plain
  DOM — no npm, no bundler, no build step — organised like a React app (a store, component
  functions returning elements, one `render()`); see §16.
* **Speech engine:** `whisper.cpp` via `whisper-rs` (see §3).
* **Threading:** plain `std::thread` + `crossbeam-channel`. No async runtime — there are five
  long-lived threads and the latency-critical path must never sit behind an executor.

External dependencies, deliberately short: `whisper-rs`, `windows`, `serde`/`serde_json`/`toml`,
`crossbeam-channel`, `tracing`, `thiserror`, `eframe` (UI only).

---

## 3. Speech engine

### Why whisper.cpp

It is the one engine that covers the entire hardware range with a single library: **CUDA,
Vulkan, and CPU backends**, plus quantized GGML models. `faster-whisper`/CTranslate2 is
CUDA-only on GPU and Python — out. GGML also supports **runtime backend loading**: the core
exe loads `ggml-cuda.dll` or `ggml-vulkan.dll` if present, so:

* Default distribution: **Vulkan** backend (NVIDIA, AMD, Intel; small download).
* NVIDIA users optionally add the **CUDA pack** (~400 MB of CUDA runtime DLLs). Dev machine uses this.
* No GPU: CPU backend, quantized model.

### Hardware tiers → default model

Chosen by a first-run benchmark on a bundled 5-second clip; always user-overridable.

| Tier | Hardware | Default model | Tail latency, 3 s utterance |
|---|---|---|---|
| A | NVIDIA ≥ 6 GB VRAM (dev box) | `large-v3-turbo` Q5_0, CUDA | *to be measured* (target ≤ 0.5 s) |
| B | Any Vulkan GPU, 2–4 GB | `small.en` Q5_1 | *to be measured* |
| C | CPU, ≥ 8 physical cores | `small.en` Q5_1 | **~0.65 s** (measured, 3900X) |
| D | CPU, anything else | `base.en` Q5_1 | **~0.2 s** (measured, 3900X) |

CPU rows are measured — see [docs/benchmarks.md](docs/benchmarks.md). `large-v3-turbo` on CPU
is 4–6 s per pass and therefore GPU-only. **Pascal caveat:** the 1070 Ti has no tensor cores
and slow FP16; benchmark CUDA *and* Vulkan before trusting Tier A.

### Inference settings (dictation, not transcription)

Language pinned (`en`) to skip detection · greedy (beam = 1) · no timestamps · single segment ·
non-speech tokens suppressed · no temperature fallback · one warm `whisper_state` reused across
calls · previous segment text passed as `initial_prompt` for continuity of names and punctuation.

Two settings are evidence-based rather than whisper.cpp defaults:

* **Encoder audio context sized to the input** — `clamp(⌈secs·50⌉ + 64, 512, 1500)`. Whisper's
  encoder otherwise always processes a padded 30 s window, so a 3 s segment cost exactly as much
  as a 12 s clip and incremental segmentation bought nothing. Auto-sizing cut CPU latency 2–4×
  with no accuracy change on the bench clip; a 384 floor introduced an error, so 512 is the floor.
* **Flash attention only on GPU builds** — measured ~15 % slower on CPU.
* **Threads = physical cores** (capped at 16); SMT siblings don't help.

**VAD:** whisper.cpp's built-in Silero VAD gates every segment. This also eliminates Whisper's
hallucinations on silence ("Thank you for watching."). A trivial energy VAD lives in `vox-core`
as a fallback and for tests.

### Engine trait

```rust
pub trait Engine: Send {
    fn transcribe(&mut self, audio_16k_mono: &[f32], opts: &TranscribeOptions) -> Result<String, EngineError>;
    fn name(&self) -> &str;
}
```

Phase 4 candidate behind the same trait: **NVIDIA Parakeet TDT 0.6B v2** via ONNX Runtime —
English-only, faster and more accurate than `turbo`, but adds a large runtime dependency.

---

## 4. Latency design — transcribe *while* the user talks

Whisper is not a streaming model; naive streaming (re-transcribing a sliding window) wastes
GPU and produces flicker. Instead, **incremental segmentation**:

1. While the hotkey is held, 16 kHz audio accumulates; VAD runs per 30 ms frame (~1 ms).
2. When VAD sees a pause ≥ `pause_ms` (600) and the open segment is ≥ `min_segment_ms` (1500),
   that segment is dispatched to the inference thread **immediately**, while speech continues.
3. Each finished segment's text becomes the next segment's `initial_prompt`.
4. On release only the **tail** (last 1–3 s) remains to transcribe.

Result: end-of-speech → text ≈ **200–450 ms** on Tier A regardless of utterance length.
Short utterances (< 1.5 s) are a single pass.

**Latency budget, Tier A (target ≤ 500 ms):**

| Stage | Cost |
|---|---|
| Release detected in hook | < 1 ms |
| Flush tail + VAD | ~5 ms |
| Tail transcription (≤ 3 s audio, turbo Q5, warm) | ~150–350 ms |
| Injection — clipboard paste / Unicode `SendInput` | ~10–50 ms / ~1 ms per char |
| **Total** | **~200–450 ms** |

**Warm-model policy:** Tier A preloads the model at daemon start (~700 MB VRAM idle is
irrelevant on an 8 GB card). Lower tiers lazy-load on first use and unload after
`idle_unload_min` (default 15). Both are configuration.

---

## 5. Audio capture — WASAPI, open on demand

* **Let Windows resample.** Request 16 kHz mono f32 with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
  SRC_DEFAULT_QUALITY`. The OS mixer converts; no resampler dependency.
* **Do not hold the mic open while idle.** An open capture stream forces AirPods into the
  Bluetooth hands-free (HFP) profile, which degrades *playback* quality for as long as it is
  held. Open the stream on hotkey press, close on release, with a configurable `keep_warm_ms`
  grace for rapid-fire dictation. HFP switching can take 200–500 ms and clip the first
  syllable — a short "ready" tick plays when capture actually starts. "Keep mic warm" is an
  opt-in for wired mics.
* **Capture thread** is event-driven (WASAPI event callback), raised to Pro Audio priority via
  `AvSetMmThreadCharacteristics`, and only copies frames into a channel.

### Device model

```rust
enum DeviceSelection {
    DefaultCommunications,          // follows Windows' "headset" → AirPods just work
    DefaultConsole,
    Specific { id: String, name: String },  // WASAPI endpoint ID, stable across reboots
}
```

`IMMNotificationClient` delivers plug/unplug and default-change events. If a `Specific` device
disappears, fall back to default and notify. Re-enumerate on resume from sleep
(`WM_POWERBROADCAST`) — stale handles after sleep are a classic bug.

---

## 6. Hotkeys — low-level hooks, any key, mouse buttons

`RegisterHotKey` is rejected: no key-up events (kills push-to-talk) and no mouse buttons.

**`WH_KEYBOARD_LL` + `WH_MOUSE_LL`**, installed from the daemon's Win32 message thread.

* Binds anything: modifier chords, F13–F24 (exist in the API with no physical key), media
  keys, Mouse 4/5 (XBUTTON1/2).
* **Razer:** in Synapse set the button to a plain key **remap** (e.g. F13), not a macro —
  macros fire press+release instantly and break hold-to-talk. Alternatively map it to
  Mouse 4/5 and the mouse hook catches it directly.
* Modes: **push-to-talk** (hold) and **toggle** (tap / tap). Default: push-to-talk.
* Bind flow: UI asks daemon to enter capture mode → user presses the chord → daemon reports it.
* Hook callbacks must be trivial: match against the bound chord, post an event to a channel,
  return in microseconds. Windows silently **uninstalls** hooks that stall
  (`LowLevelHooksTimeout`); a watchdog re-installs if the hook goes missing.
* Suppress the key only when it matches the bound chord; pass everything else through.
* Matching is strict on modifiers (bound `F13` does not fire on `Ctrl+F13`).
* `reset()` on focus loss / session lock clears stuck-key state.

**Known limitation:** un-elevated hooks and `SendInput` do not reach admin-elevated windows
(UIPI). Documented; "run elevated" offered as an option.

---

## 7. Text injection

`TextSink` trait, two methods, `Auto` chooses:

* **Unicode `SendInput`** for short text (< ~120 chars): works everywhere, no clipboard side effects.
* **Clipboard + Ctrl+V** for longer text: near-instant; previous clipboard contents are saved
  and restored.

---

## 8. Threads and the session state machine

```
Win32 message thread ──hotkey events──►┐
  hooks · tray · hidden window          │
WASAPI capture thread ──f32 frames────► Coordinator thread ──segment jobs──► Inference thread
  event-driven, Pro Audio priority       state machine · VAD                owns whisper ctx
                                          segmenter · injection              strictly sequential
IPC server thread ◄────────────────────── state / devices / config ────────► vox-ui
```

The **session state machine** is pure (`vox-core::session`) and drives the coordinator via
returned `Action`s:

```
Idle ──press──► Arming ──stream opened──► Recording ──release──► Finalizing ──text──► Injecting ──done──► Idle
                  │ stream failed → Idle + notify        │ held < min_press → Idle (discard)
                  │ release before open → Idle           │ device lost → Finalizing + notify
```

Presses that arrive during Finalizing/Injecting are queued as one pending press (cancelled if
the release also arrives, in push-to-talk mode). `Cancel` (Escape, device loss) discards.

---

## 9. Crate layout

```
crates/
  vox-core/           pure logic, zero Win32 — traits (Engine, Vad, AudioSource, TextSink),
                      chord parse/match, session state machine, segmenter, text joiner,
                      injection strategy, config schema.        ← nearly all unit tests live here
  vox-engine-whisper/ Engine impl over whisper-rs. features: cuda, vulkan
  vox-platform-win/   WASAPI + device notifications, LL hooks (incl. bind mode),
                      SendInput/clipboard, autostart (HKCU\Run), single-instance mutex, sounds
  vox-bench/   (bin)  latency/WER harness
  voxd/               lib + two bins:
                        voxd.exe — Tauri app (tray, settings window) + daemon threads
                        vox.exe  — console CLI: info / devices / mic-test / transcribe
                      ui/ — the frontend (index.html, styles.css, app.js); no build step
```

```
voxd ──► vox-core
  ├────► vox-engine-whisper ──► vox-core, whisper-rs
  ├────► vox-platform-win   ──► vox-core, windows
  └────► tauri
```

If `whisper-rs` lags whisper.cpp on a feature we need (e.g. the VAD API), a thin `bindgen`
FFI crate replaces it behind the same `Engine` trait.

---

## 10. Configuration

`%APPDATA%\Vox\config.toml`, owned by the daemon. Schema lives in `vox-core::config`.

```toml
[hotkey]
chord = "F13"            # or "Ctrl+Shift+Space", "Mouse4", ...
mode = "push_to_talk"    # | "toggle"
min_press_ms = 250

[audio]
keep_warm_ms = 0
[audio.device]
kind = "default_communications"   # | "default_console" | "specific" (+ id, name)
[audio.segmenter]
pause_ms = 600
min_segment_ms = 1500
max_segment_ms = 15000
min_speech_ms = 250
pre_roll_ms = 300

[engine]
model = "base.en-q5_1"   # CPU default; "large-v3-turbo-q5_0" for GPU tiers
backend = "auto"         # | "cuda" | "vulkan" | "cpu"
beam_size = 1
preload = true
idle_unload_min = 0      # 0 = never unload

[injection.strategy]
kind = "auto"
clipboard_threshold = 120

[behavior]
sounds = true
autostart = true
```

## 11. UI ↔ daemon interface

Tauri commands (frontend → Rust, all in `crates/voxd/src/bin/voxd.rs`):

`get_status` · `get_config` · `set_config` · `list_devices` · `list_models` · `get_paths` ·
`capture_hotkey` / `cancel_hotkey_capture` · `start_meter` / `stop_meter` · `open_path` ·
`quit_app`

Events (Rust → frontend): `status` (the whole [`Status`] struct on every state change),
`level` (mic meter, ~10 Hz, only while the meter runs), `devices_changed` (debounced
`IMMNotificationClient` notification).

`Status` is the single payload the UI renders: state, hotkey, mode, model, `engine_loaded`,
device, last transcript, release→text latency, last error, dictation count. The same struct
drives the tray tooltip and icon (blue idle / red dot while recording).

**Hotkey binding** runs through the hook layer's bind mode: `capture_hotkey` puts the hooks
into a state where the next non-modifier press is reported as a `Chord` and swallowed, rather
than matched. Escape cancels; left/right mouse buttons are ignored so the desktop stays usable.

---

## 12. Testing

* **Unit (vox-core — the bulk):** chord parse/display round-trips, matcher (repeat suppression,
  strict modifiers, modifier-as-key, stuck-key reset); state-machine transitions incl. all
  awkward interleavings; segmenter against synthetic VAD streams; text joining/cleaning;
  injection strategy selection; config defaults, partial files, round-trip.
* **Engine golden tests:** short WAVs with expected transcripts, WER threshold. CI runs
  `tiny.en` on CPU; local/nightly runs `turbo` on GPU.
* **Benchmarks (`criterion`):** latency per model × backend on a fixed clip; output populates
  the tier-default table.
* **IPC:** round-trip serialization, version mismatch, client reconnect after daemon restart.
* **Platform:** smoke tests (enumerate devices, install/uninstall hook, mutex) marked
  `#[ignore]`, run manually.
* **End-to-end:** `voxd --transcribe file.wav` runs the full pipeline with `WavAudioSource` and
  `RecordingTextSink`, printing text and per-stage timings. Doubles as benchmark and repro tool.

## 13. Build order

0. ✅ **Spike (de-risk):** `vox-bench` on CPU — see docs/benchmarks.md. GPU backends still to
   measure once CUDA / Vulkan SDKs are installed.
1. ✅ **Daemon core loop** (`voxd`, 2026-09-05): hooks → WASAPI → whisper → `SendInput`, tray
   with Quit, config file, `devices` / `mic-test` / `transcribe` subcommands. Console
   subsystem kept for logs until phase 3. Measured: AirPods Max take ~800 ms to open
   (Bluetooth HFP switch) — the "ready" tick exists for exactly this.
2. ✅ **Settings UI** (2026-09-05): Tauri app with tray icon, device picker + live level meter,
   press-a-key hotkey binder, mode/model/backend pickers, autostart, injection strategy,
   activity panel; live device-change notifications.
3. Incremental segmentation, injection strategies, keep-warm policy, installer
   (core + optional CUDA pack; models downloaded on first run with SHA-256 check).
4. Optional: Parakeet engine, local-LLM cleanup pass, per-app rules, floating recording indicator.

## 14. Decisions log

| Date | Decision | Rationale |
|---|---|---|
| 2026-09-05 | Two-process (daemon + UI) over single tray app | "closed but listening" with zero UI cost; UI can crash without affecting dictation |
| 2026-09-05 | Rust + `windows` crate directly, no `cpal` | need `IMMNotificationClient`, WASAPI auto-convert, hook control |
| 2026-09-05 | whisper.cpp with dynamic GGML backends | single engine spans CUDA / Vulkan / CPU |
| 2026-09-05 | Personal build first (CUDA baked in, unsigned); Vulkan-default installer later | dev speed; SmartScreen/signing is a distribution problem |
| 2026-09-05 | Default mode push-to-talk; toggle available | matches "hold Razer button" usage |
| 2026-09-05 | Recording indicator = tray icon state + tick sound; floating overlay deferred to phase 4 | avoids a second window on the message thread until proven wanted |
| 2026-09-05 | Mic opened on demand, not held | AirPods HFP degrades playback while a capture stream is open |
| 2026-09-05 | Explicit `/O2` + AVX2/FMA/F16C CMake defines in `.cargo/config.toml`; build via `scripts\cargo-msvc.cmd` | `cmake` crate drops optimisation flags and `GGML_NATIVE` is a no-op on MSVC → 20× slowdown; VS 2022 on the dev box lacks x64 libs |
| 2026-09-05 | LLVM (libclang) is a build prerequisite | whisper-rs-sys's bundled bindings are glibc-specific; bindgen must run on Windows |
| 2026-09-05 | Encoder `audio_ctx` auto-sized per call, 512 floor | fixed 30 s encoder cost defeats incremental segmentation; 2–4× faster, no accuracy loss at 512 |
| 2026-09-05 | Flash attention on GPU builds only | ~15 % slower on CPU |
| 2026-09-05 | Threads = physical cores (`num_cpus`), max 16 | 12 > 8 on the 3900X |
| 2026-09-05 | `large-v3-turbo` is GPU-only; `base.en` is the CPU default | 4–6 s/pass on CPU vs ~0.2 s tail for `base.en` |
| 2026-09-05 | CPU baseline is AVX2 | pre-2013/2015 CPUs deferred until ggml runtime dispatch (`GGML_CPU_ALL_VARIANTS`) is wired in |
| 2026-09-05 | **Reversed**: one Tauri process instead of daemon + UI over a named pipe | the webview exists only while the window is open, giving the same idle cost without a second binary or an IPC protocol; also removes `vox-ipc` |
| 2026-09-05 | **Reversed**: Tauri/WebView2 instead of egui | egui cannot produce the intended look; WebView2 ships with Windows 11 so the runtime is already there (+~4 MB binary) |
| 2026-09-05 | Frontend is plain DOM, no npm/bundler | one `app.js` + one `styles.css`, no `node_modules`, no build step in `cargo build`; structured like a React app so it stays readable |
| 2026-09-05 | Autostart entry passes `--minimized` | launching by hand should show the window; launching at login should not |
| 2026-09-05 | Hotkey binding reuses the hooks in a "capture" mode | binds anything the matcher can match, including mouse side buttons, with no second input path |

## 15. Toolchain (dev machine status, 2026-09-05)

| Tool | Needed for | Status |
|---|---|---|
| Rust 1.98 (rustup, stable, `x86_64-pc-windows-msvc`) | everything | present |
| WebView2 runtime | the settings window | present (ships with Windows 11) |
| MSVC Build Tools 2019 (C++ x64) | linking, whisper.cpp | present — VS 2022 Community is also installed but lacks x64 libs; `scripts\cargo-msvc.cmd` selects 2019 |
| CMake 4.4 | whisper.cpp build via `whisper-rs-sys` | present |
| LLVM 22 (libclang) | bindgen in `whisper-rs-sys` | present |
| CUDA Toolkit 12.x | `cuda` feature | missing (driver 560.94 present — supports 12.x runtime) |
| Vulkan SDK (LunarG) | `vulkan` feature | missing |
| git | — | present |

Always build with `scripts\cargo-msvc.cmd <cargo args>`; see the comments in
`.cargo/config.toml` for why.

## 16. The frontend

`crates/voxd/ui/` — three files, no dependencies, no build step. Tauri serves the directory
as the window's content; editing a file and reopening the window is the whole dev loop.

* `index.html` — a root div and two script/style tags.
* `styles.css` — CSS custom properties for the palette (blue 500/600/700 on a cool grey
  ground), then components: cards, buttons, segmented controls, switches, radio rows, the
  level meter, the bind-key overlay, toasts.
* `app.js` — an `h(tag, props, ...children)` helper, a `store`, component functions
  (`HotkeyCard`, `MicCard`, `EngineCard`, `BehaviorCard`, `ActivityCard`), and `render()`
  which rebuilds from state. Config edits are debounced 250 ms before `set_config`, so a
  slider drag is one write. `status` events patch only the topbar and activity panel to avoid
  stealing focus from inputs.

For design work without the daemon, copy the stub bridge into `ui/preview.local.html`
(git-ignored, see `.claude/launch.json`) and serve the folder:
`python -m http.server 5177 --directory crates/voxd/ui`.
