//! Windows platform layer for Vox.
//!
//! Everything here is thin: it turns Win32 into the channels and traits `vox-core` defines
//! and keeps no decision logic of its own.
//!
//! * [`hooks`] — `WH_KEYBOARD_LL` / `WH_MOUSE_LL` feeding a [`vox_core::ChordMatcher`].
//! * [`message_thread`] — the thread that owns the hooks, a hidden window, and the tray icon.
//! * [`wasapi`] — capture-device enumeration and an event-driven 16 kHz mono f32 capture stream.
//! * [`inject`] — [`vox_core::TextSink`] via Unicode `SendInput` or clipboard + Ctrl+V.
//! * [`misc`] — single-instance mutex, tick sounds, string helpers.

pub mod hooks;
pub mod inject;
pub mod message_thread;
pub mod misc;
pub mod wasapi;

pub use inject::WinTextSink;
pub use message_thread::{MessageThreadHandle, TrayEvent};
pub use misc::{attach_parent_console, message_box, tick, MessageKind, SingleInstance, Tick};
pub use wasapi::{list_capture_devices, open_capture, AudioMsg, CaptureStream, DeviceInfo};

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("{context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("{0}")]
    Other(String),
}

impl PlatformError {
    pub(crate) fn win(context: &'static str) -> impl FnOnce(windows::core::Error) -> PlatformError {
        move |source| PlatformError::Win { context, source }
    }
}
