//! voxd.exe — the Vox daemon. Windowed: double-clicking shows only a tray icon.
//! Diagnostics live in `vox.exe`.
#![windows_subsystem = "windows"]

use vox_platform_win::{attach_parent_console, message_box, MessageKind};

fn main() {
    // Only matters when started from a terminal; from Explorer there is no parent console.
    attach_parent_console();
    voxd::logging::init();

    if let Err(e) = voxd::daemon::run() {
        tracing::error!("{e:#}");
        // The daemon has no console; this is the user's only feedback.
        message_box(
            "Vox could not start",
            &format!("{e:#}\n\nLog: {}", voxd::paths::log_path().display()),
            MessageKind::Error,
        );
        std::process::exit(1);
    }
}
