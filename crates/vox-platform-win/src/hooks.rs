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
//!
//! **Bind mode** ([`begin_capture`]) reports the key the user presses as a [`Chord`] instead
//! of matching it. Escape cancels. Used by the settings UI's "press a key" control. It has to
//! cope with several awkward cases:
//!
//! * A **bare modifier** (`RCtrl`, `RAlt`, …) is a legitimate binding, but a modifier press
//!   might also be the start of `Ctrl+Shift+K`. So a lone modifier is remembered on press and
//!   only bound on *release*, and forgotten the moment any other key joins it.
//! * Keys **already held when bind mode starts** — the Space or Enter that activated the
//!   "Change…" button — must not bind when they are released. Their releases are ignored.
//! * A few keys (PrintScreen most notably) deliver only a key-*up* to low-level hooks. Any
//!   non-modifier release with no matching press therefore binds too.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use crossbeam_channel::Sender;
use vox_core::chord::vk;
use vox_core::{Chord, ChordMatcher, HotkeyEvent, InputEvent, Key, MouseButton};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLMHF_INJECTED, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::PlatformError;

/// Result of a bind-mode session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    Bound(Chord),
    Cancelled,
}

struct Capture {
    tx: Sender<CaptureOutcome>,
    /// Keys held when bind mode began; their releases mean nothing.
    ignore_release: HashSet<Key>,
    /// A modifier pressed with nothing else since — binds if released alone.
    lone_modifier: Option<Key>,
}

struct Shared {
    matcher: Mutex<ChordMatcher>,
    tx: Sender<HotkeyEvent>,
    capture: Mutex<Option<Capture>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// One-time setup of the process-wide matcher. Must precede [`Hooks::install`].
pub fn init(chord: Chord, tx: Sender<HotkeyEvent>) -> Result<(), PlatformError> {
    SHARED
        .set(Shared {
            matcher: Mutex::new(ChordMatcher::new(chord)),
            tx,
            capture: Mutex::new(None),
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

/// Enter bind mode. The captured chord is sent on `tx` and the key is swallowed. Left and
/// right mouse buttons are never bound — the desktop would become unusable.
///
/// Callers should also ask the hook thread for [`crate::message_thread::set_capture_mode`],
/// otherwise mouse buttons are only bindable when the current chord already uses one.
pub fn begin_capture(tx: Sender<CaptureOutcome>) {
    if let Some(s) = SHARED.get() {
        let held: HashSet<Key> = s
            .matcher
            .lock()
            .map(|m| m.held_keys().collect())
            .unwrap_or_default();
        if let Ok(mut c) = s.capture.lock() {
            *c = Some(Capture {
                tx,
                ignore_release: held,
                lone_modifier: None,
            });
        }
    }
}

pub fn cancel_capture() {
    if let Some(s) = SHARED.get() {
        if let Ok(mut c) = s.capture.lock() {
            *c = None;
        }
    }
}

/// Whether bind mode is active (the hook watchdog leaves the hooks alone while it is).
pub fn is_capturing() -> bool {
    SHARED
        .get()
        .and_then(|s| s.capture.lock().ok().map(|c| c.is_some()))
        .unwrap_or(false)
}

pub fn chord_uses_mouse(chord: &Chord) -> bool {
    matches!(chord.key, Key::Mouse(_))
}

/// Returns whether the raw event should be swallowed.
fn dispatch(key: Key, pressed: bool) -> bool {
    let Some(s) = SHARED.get() else {
        return false;
    };
    let Ok(mut matcher) = s.matcher.lock() else {
        return false;
    };

    if let Ok(mut capture) = s.capture.lock() {
        if capture.is_some() {
            // Keep the held-key bookkeeping current, but never emit hotkey events while binding.
            let _ = matcher.feed(InputEvent { key, pressed });
            let (outcome, suppress) =
                capture_step(capture.as_mut().unwrap(), &matcher, key, pressed);
            if let Some(outcome) = outcome {
                let _ = capture.as_ref().unwrap().tx.try_send(outcome);
                *capture = None;
            }
            return suppress;
        }
    }

    let result = matcher.feed(InputEvent { key, pressed });
    if let Some(ev) = result.event {
        let _ = s.tx.try_send(ev);
    }
    result.suppress
}

/// One input event in bind mode. Returns the outcome (if the session is finished) and
/// whether to swallow the event. Pure, so the awkward cases are unit-testable.
fn capture_step(
    cap: &mut Capture,
    matcher: &ChordMatcher,
    key: Key,
    pressed: bool,
) -> (Option<CaptureOutcome>, bool) {
    let unbindable = matches!(key, Key::Mouse(MouseButton::Left | MouseButton::Right));

    if pressed {
        if key == Key::Keyboard(vk::ESCAPE) {
            return (Some(CaptureOutcome::Cancelled), true);
        }
        if key.as_modifier().is_some() {
            // First thing held? Remember it; it binds on release if nothing else joins.
            cap.lone_modifier = if cap.lone_modifier.is_none() && cap.ignore_release.is_empty() {
                Some(key)
            } else {
                None
            };
            return (None, false);
        }
        cap.lone_modifier = None;
        if unbindable {
            return (None, false);
        }
        let chord = Chord::with_modifiers(key, matcher.held_modifiers(key));
        return (Some(CaptureOutcome::Bound(chord)), true);
    }

    // Release.
    if cap.ignore_release.remove(&key) {
        return (None, false);
    }
    if cap.lone_modifier == Some(key) {
        return (Some(CaptureOutcome::Bound(Chord::new(key))), true);
    }
    if key.as_modifier().is_none() && !unbindable {
        // A press we never saw (PrintScreen and friends).
        return (Some(CaptureOutcome::Bound(Chord::new(key))), true);
    }
    (None, false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::chord::vk;

    const F13: Key = Key::Keyboard(0x70 + 12);
    const K: Key = Key::Keyboard(b'K' as u16);
    const MOUSE4: Key = Key::Mouse(MouseButton::X1);

    /// A capture session plus the matcher whose held-key state it reads.
    struct Fix {
        cap: Capture,
        matcher: ChordMatcher,
        _rx: crossbeam_channel::Receiver<CaptureOutcome>,
    }

    fn fixture(held: &[Key]) -> Fix {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut matcher = ChordMatcher::new(Chord::new(F13));
        for k in held {
            matcher.feed(InputEvent::press(*k));
        }
        Fix {
            cap: Capture {
                tx,
                ignore_release: held.iter().copied().collect(),
                lone_modifier: None,
            },
            matcher,
            _rx,
        }
    }

    /// Feed one event to both the matcher and the capture step.
    fn feed(f: &mut Fix, key: Key, pressed: bool) -> (Option<CaptureOutcome>, bool) {
        f.matcher.feed(InputEvent { key, pressed });
        capture_step(&mut f.cap, &f.matcher, key, pressed)
    }

    #[test]
    fn plain_key_binds_on_press_and_is_swallowed() {
        let mut f = fixture(&[]);
        assert_eq!(
            feed(&mut f, F13, true),
            (Some(CaptureOutcome::Bound(Chord::new(F13))), true)
        );
    }

    #[test]
    fn escape_cancels() {
        let mut f = fixture(&[]);
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::ESCAPE), true),
            (Some(CaptureOutcome::Cancelled), true)
        );
    }

    #[test]
    fn modifier_combo_binds_with_its_modifiers() {
        let mut f = fixture(&[]);
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::LCONTROL), true),
            (None, false)
        );
        assert_eq!(feed(&mut f, Key::Keyboard(vk::LSHIFT), true), (None, false));
        let (outcome, suppress) = feed(&mut f, K, true);
        assert!(suppress);
        let chord = match outcome {
            Some(CaptureOutcome::Bound(c)) => c,
            other => panic!("expected a binding, got {other:?}"),
        };
        assert_eq!(chord.to_string(), "Ctrl+Shift+K");
    }

    #[test]
    fn lone_modifier_binds_on_release() {
        let mut f = fixture(&[]);
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::RCONTROL), true),
            (None, false)
        );
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::RCONTROL), false),
            (
                Some(CaptureOutcome::Bound(Chord::new(Key::Keyboard(
                    vk::RCONTROL
                )))),
                true
            )
        );
    }

    #[test]
    fn two_modifiers_alone_bind_nothing() {
        let mut f = fixture(&[]);
        feed(&mut f, Key::Keyboard(vk::LCONTROL), true);
        feed(&mut f, Key::Keyboard(vk::LSHIFT), true);
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::LSHIFT), false),
            (None, false)
        );
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::LCONTROL), false),
            (None, false)
        );
    }

    #[test]
    fn modifier_used_in_a_combo_does_not_bind_on_its_own_release() {
        let mut f = fixture(&[]);
        feed(&mut f, Key::Keyboard(vk::LCONTROL), true);
        feed(&mut f, K, true); // binds; in the real dispatcher the session ends here
        assert_eq!(
            feed(&mut f, Key::Keyboard(vk::LCONTROL), false),
            (None, false)
        );
    }

    #[test]
    fn release_of_a_key_held_before_capture_is_ignored() {
        // Enter activated the "Change…" button, so it was already down.
        let enter = Key::Keyboard(vk::RETURN);
        let mut f = fixture(&[enter]);
        assert_eq!(feed(&mut f, enter, false), (None, false));
        // ...and the session is still live for the real choice.
        assert_eq!(
            feed(&mut f, F13, true),
            (Some(CaptureOutcome::Bound(Chord::new(F13))), true)
        );
    }

    #[test]
    fn keyup_only_key_binds_on_release() {
        // PrintScreen delivers no key-down to low-level hooks.
        let snapshot = Key::Keyboard(0x2C);
        let mut f = fixture(&[]);
        assert_eq!(
            feed(&mut f, snapshot, false),
            (Some(CaptureOutcome::Bound(Chord::new(snapshot))), true)
        );
    }

    #[test]
    fn mouse_side_button_binds_but_left_and_right_do_not() {
        let mut f = fixture(&[]);
        assert_eq!(
            feed(&mut f, Key::Mouse(MouseButton::Left), true),
            (None, false)
        );
        assert_eq!(
            feed(&mut f, Key::Mouse(MouseButton::Right), true),
            (None, false)
        );
        assert_eq!(
            feed(&mut f, Key::Mouse(MouseButton::Left), false),
            (None, false)
        );
        assert_eq!(
            feed(&mut f, MOUSE4, true),
            (Some(CaptureOutcome::Bound(Chord::new(MOUSE4))), true)
        );
    }

    #[test]
    fn modifier_held_from_before_capture_is_not_a_lone_candidate() {
        // Ctrl was already down when bind mode opened; releasing it must not bind Ctrl.
        let ctrl = Key::Keyboard(vk::LCONTROL);
        let mut f = fixture(&[ctrl]);
        assert_eq!(feed(&mut f, ctrl, false), (None, false));
    }
}
