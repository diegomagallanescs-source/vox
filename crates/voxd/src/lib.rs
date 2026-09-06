//! Shared code for the two Vox binaries:
//!
//! * `voxd.exe` — the Tauri app: tray icon, settings window, and the dictation daemon
//!   running on background threads. Windowed; no console.
//! * `vox.exe`  — a console CLI for diagnostics: list devices, test the mic, transcribe a WAV.

pub mod cli;
pub mod coordinator;
pub mod daemon;
pub mod engine;
pub mod logging;
pub mod paths;
pub mod status;
