//! Small Win32 helpers: single-instance guard, tick sounds, wide strings.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Diagnostics::Debug::Beep;
use windows::Win32::System::Threading::CreateMutexW;

use crate::PlatformError;

/// NUL-terminated UTF-16 for passing to `*W` APIs. Keep the `Vec` alive for the call.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

/// Process-wide named mutex. Dropping it releases the name.
pub struct SingleInstance(HANDLE);

// The handle is only ever closed, from whichever thread drops the guard.
unsafe impl Send for SingleInstance {}

impl SingleInstance {
    /// `Ok(None)` means another instance already holds the mutex.
    pub fn acquire() -> Result<Option<SingleInstance>, PlatformError> {
        let handle = unsafe { CreateMutexW(None, false, w!("Local\\Vox.Daemon")) }
            .map_err(PlatformError::win("CreateMutexW"))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(SingleInstance(handle)))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Capture actually started — safe to talk.
    Start,
    /// Capture ended, transcribing.
    Stop,
    Error,
}

/// Short cue played on a throwaway thread (`Beep` blocks for its duration).
pub fn tick(kind: Tick) {
    let (freq, ms) = match kind {
        Tick::Start => (1000, 45),
        Tick::Stop => (700, 45),
        Tick::Error => (300, 120),
    };
    std::thread::Builder::new()
        .name("vox-tick".into())
        .spawn(move || unsafe {
            let _ = Beep(freq, ms);
        })
        .ok();
}
