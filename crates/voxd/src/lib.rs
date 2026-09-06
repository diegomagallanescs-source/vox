//! Shared code for the two Vox binaries:
//!
//! * `voxd.exe` — the windowed daemon (tray icon, hotkey, dictation). No console.
//! * `vox.exe`  — a console CLI for diagnostics: list devices, test the mic, transcribe a WAV.
//!
//! Splitting them is the standard Windows arrangement: a GUI-subsystem process cannot print
//! to a terminal that started it in a way shells will wait for or pipe, and a console
//! process cannot hide its window.

pub mod cli;
pub mod coordinator;
pub mod daemon;
pub mod engine;
pub mod logging;
pub mod paths;
