//! Low-level keyboard and mouse hooks.
//!
//! The hook procedures run on the thread that installed them (the message thread) and are
//! called for **every** key and mouse event in the session, so they do the minimum: reject
//! injected input (our own `SendInput`), feed the [`ChordMatcher`], forward a
//! [`HotkeyEvent`] over a channel, and return. Windows silently uninstalls hooks that stall
//! (`LowLevelHooksTimeout`); the message thread re-installs on a timer as insurance.
//!
//! Only the bound chord's main key is ever swallowed. The mouse hook is installed only when
//! the chord uses a mouse button, since it otherwise costs a call per mouse move.

use std::sync::{Mutex, OnceLock};

use crossbeam_channel::Sender;
use vox_core::{Chord, ChordMatcher, HotkeyEvent, InputEvent, Key, MouseButton};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLMHF_INJECTED, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::PlatformError;

struct Shared {
    matcher: Mutex<ChordMatcher>,
    tx: Sender<HotkeyEvent>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// One-time setup of the process-wide matcher. Must precede [`Hooks::install`].
pub fn init(chord: Chord, tx: Sender<HotkeyEvent>) -> Result<(), PlatformError> {
    SHARED
        .set(Shared {
            matcher: Mutex::new(ChordMatcher::new(chord)),
            tx,
        })
        .map_err(|_| PlatformError::Other("hooks already initialised".into()))
}

pub fn rebind(chord: Chord) {
    if let Some(s) = SHARED.get() {
        if let Ok(mut m) = s.matcher.lock() {
            m.set_chord(chord);
        }
    }
}

/// Forget held keys (focus loss, session lock, hook re-install).
pub fn reset_held() {
    if let Some(s) = SHARED.get() {
        if let Ok(mut m) = s.matcher.lock() {
            m.reset();
        }
    }
}

pub fn chord_uses_mouse(chord: &Chord) -> bool {
    matches!(chord.key, Key::Mouse(_))
}

/// Returns whether the raw event should be swallowed.
fn dispatch(key: Key, pressed: bool) -> bool {
    let Some(s) = SHARED.get() else {
        return false;
    };
    let result = match s.matcher.lock() {
        Ok(mut m) => m.feed(InputEvent { key, pressed }),
        Err(_) => return false,
    };
    if let Some(ev) = result.event {
        let _ = s.tx.try_send(ev);
    }
    result.suppress
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL with code == HC_ACTION, lparam points at a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if !info.flags.contains(LLKHF_INJECTED) {
            let msg = wparam.0 as u32;
            let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let released = matches!(msg, WM_KEYUP | WM_SYSKEYUP);
            if (pressed || released) && dispatch(Key::Keyboard(info.vkCode as u16), pressed) {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        if !matches!(
            msg,
            WM_XBUTTONDOWN
                | WM_XBUTTONUP
                | WM_MBUTTONDOWN
                | WM_MBUTTONUP
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_RBUTTONUP
        ) {
            // WM_MOUSEMOVE and everything else: out as fast as possible.
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        // SAFETY: for WH_MOUSE_LL with code == HC_ACTION, lparam points at a MSLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if info.flags & LLMHF_INJECTED == 0 {
            let button = match msg {
                // X buttons: which one is in the high word of mouseData (XBUTTON1 = 1, XBUTTON2 = 2).
                WM_XBUTTONDOWN | WM_XBUTTONUP => match (info.mouseData >> 16) as u16 {
                    1 => Some(MouseButton::X1),
                    2 => Some(MouseButton::X2),
                    _ => None,
                },
                WM_MBUTTONDOWN | WM_MBUTTONUP => Some(MouseButton::Middle),
                WM_LBUTTONDOWN | WM_LBUTTONUP => Some(MouseButton::Left),
                _ => Some(MouseButton::Right),
            };
            if let Some(b) = button {
                let pressed = matches!(
                    msg,
                    WM_XBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONDOWN | WM_RBUTTONDOWN
                );
                if dispatch(Key::Mouse(b), pressed) {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Installed hooks. Not `Send`: lives and dies on the message thread.
pub struct Hooks {
    keyboard: Option<HHOOK>,
    mouse: Option<HHOOK>,
}

impl Hooks {
    /// Must be called on a thread that pumps messages.
    pub fn install(with_mouse: bool) -> Result<Hooks, PlatformError> {
        let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) }
            .map_err(PlatformError::win("SetWindowsHookExW(WH_KEYBOARD_LL)"))?;
        let mouse = if with_mouse {
            match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) } {
                Ok(h) => Some(h),
                Err(e) => {
                    unsafe {
                        let _ = UnhookWindowsHookEx(keyboard);
                    }
                    return Err(PlatformError::win("SetWindowsHookExW(WH_MOUSE_LL)")(e));
                }
            }
        } else {
            None
        };
        Ok(Hooks {
            keyboard: Some(keyboard),
            mouse,
        })
    }

    pub fn has_mouse(&self) -> bool {
        self.mouse.is_some()
    }

    fn unhook_all(&mut self) {
        for h in [self.keyboard.take(), self.mouse.take()]
            .into_iter()
            .flatten()
        {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }

    /// Tear down and re-install (watchdog, or the chord switched between keyboard and mouse).
    pub fn reinstall(&mut self, with_mouse: bool) -> Result<(), PlatformError> {
        self.unhook_all();
        let fresh = Hooks::install(with_mouse)?;
        self.keyboard = fresh.keyboard;
        self.mouse = fresh.mouse;
        std::mem::forget(fresh);
        Ok(())
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        self.unhook_all();
    }
}
