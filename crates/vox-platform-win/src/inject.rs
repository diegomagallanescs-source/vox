//! Text injection into the focused window.
//!
//! * `Unicode`: `SendInput` with `KEYEVENTF_UNICODE`, one down/up pair per UTF-16 unit.
//!   Works in every app, roughly a millisecond per character, no side effects.
//! * `Clipboard`: set the clipboard, send Ctrl+V, restore the previous text. Near-instant
//!   for long text. Non-text clipboard contents (images, files) are not preserved.
//!
//! Both are ignored by our own hooks because Windows marks the events as injected.

use std::time::Duration;

use vox_core::{InjectMethod, SinkError, TextSink};
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_RETURN, VK_V,
};

/// How long the target app gets to service WM_PASTE before we restore the clipboard.
const PASTE_SETTLE: Duration = Duration::from_millis(150);
/// `SendInput` batches; large arrays are rejected by some environments.
const SEND_CHUNK: usize = 64;

#[derive(Debug, Default)]
pub struct WinTextSink;

impl TextSink for WinTextSink {
    fn inject(&mut self, text: &str, method: InjectMethod) -> Result<(), SinkError> {
        if text.is_empty() {
            return Ok(());
        }
        match method {
            InjectMethod::Unicode => send_unicode(text),
            InjectMethod::Clipboard => paste_via_clipboard(text),
        }
    }
}

fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn vk_tap(vk: VIRTUAL_KEY, inputs: &mut Vec<INPUT>) {
    inputs.push(key(vk.0, 0, KEYBD_EVENT_FLAGS(0)));
    inputs.push(key(vk.0, 0, KEYEVENTF_KEYUP));
}

fn send_all(inputs: &[INPUT]) -> Result<(), SinkError> {
    for chunk in inputs.chunks(SEND_CHUNK) {
        let sent = unsafe { SendInput(chunk, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != chunk.len() {
            return Err(SinkError::Other(format!(
                "SendInput delivered {sent}/{} events (blocked by an elevated window?)",
                chunk.len()
            )));
        }
    }
    Ok(())
}

fn send_unicode(text: &str) -> Result<(), SinkError> {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => vk_tap(VK_RETURN, &mut inputs),
            _ => {
                for &unit in ch.encode_utf16(&mut buf).iter() {
                    inputs.push(key(0, unit, KEYEVENTF_UNICODE));
                    inputs.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
            }
        }
    }
    send_all(&inputs)
}

fn paste_via_clipboard(text: &str) -> Result<(), SinkError> {
    let previous = read_clipboard_text();
    write_clipboard_text(
        &text
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>(),
    )?;

    let mut inputs = Vec::with_capacity(4);
    inputs.push(key(VK_CONTROL.0, 0, KEYBD_EVENT_FLAGS(0)));
    vk_tap(VK_V, &mut inputs);
    inputs.push(key(VK_CONTROL.0, 0, KEYEVENTF_KEYUP));
    let sent = send_all(&inputs);

    std::thread::sleep(PASTE_SETTLE);
    if let Some(prev) = previous {
        // Best effort; the paste itself already happened.
        let _ = write_clipboard_text(&prev);
    }
    sent
}

/// Opens the clipboard, retrying briefly if another process holds it.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<ClipboardGuard, SinkError> {
        let mut last = None;
        for _ in 0..10 {
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(ClipboardGuard),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        Err(SinkError::Clipboard(format!(
            "OpenClipboard: {}",
            last.map(|e| e.to_string()).unwrap_or_default()
        )))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Current CF_UNICODETEXT contents including the terminator, if any.
fn read_clipboard_text() -> Option<Vec<u16>> {
    let _guard = ClipboardGuard::open().ok()?;
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?;
        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = GlobalLock(hglobal) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let max_units = GlobalSize(hglobal) / 2;
        let mut len = 0usize;
        while len < max_units && *ptr.add(len) != 0 {
            len += 1;
        }
        let mut out = std::slice::from_raw_parts(ptr, len).to_vec();
        out.push(0);
        let _ = GlobalUnlock(hglobal);
        Some(out)
    }
}

fn write_clipboard_text(units_with_nul: &[u16]) -> Result<(), SinkError> {
    let _guard = ClipboardGuard::open()?;
    unsafe {
        EmptyClipboard().map_err(|e| SinkError::Clipboard(format!("EmptyClipboard: {e}")))?;
        let bytes = units_with_nul.len() * 2;
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes)
            .map_err(|e| SinkError::Clipboard(format!("GlobalAlloc: {e}")))?;
        let dst = GlobalLock(hglobal) as *mut u16;
        if dst.is_null() {
            return Err(SinkError::Clipboard("GlobalLock returned null".into()));
        }
        std::ptr::copy_nonoverlapping(units_with_nul.as_ptr(), dst, units_with_nul.len());
        let _ = GlobalUnlock(hglobal);
        // On success the system owns the memory.
        SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(hglobal.0)))
            .map_err(|e| SinkError::Clipboard(format!("SetClipboardData: {e}")))?;
    }
    Ok(())
}
