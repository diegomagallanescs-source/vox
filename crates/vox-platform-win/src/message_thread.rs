//! The thread that hosts the low-level input hooks.
//!
//! Hooks only work on a thread that pumps messages, so this thread owns a hidden window and
//! a message loop. The tray icon and settings window are Tauri's job; this thread's only
//! duties are the hooks, their watchdog timer, and rebinding.
//!
//! Since the mouse hook is installed only when the chord uses a mouse button, rebinding may
//! add or remove it; that has to happen here, on the hook thread.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use vox_core::{Chord, HotkeyEvent};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, TranslateMessage, HWND_MESSAGE, MSG,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DESTROY, WM_TIMER, WNDCLASSW,
};

use crate::hooks::{self, Hooks};
use crate::PlatformError;

const WM_REBIND: u32 = WM_APP + 3;
const WM_QUIT_THREAD: u32 = WM_APP + 4;
const WM_CAPTURE_MODE: u32 = WM_APP + 5;
const TIMER_REHOOK: usize = 1;
/// Hooks are re-installed this often as insurance against silent removal. Re-installing has
/// a cost — a sliver of time with no hook — so this is deliberately infrequent and is skipped
/// while the user is binding a key.
const REHOOK_INTERVAL_MS: u32 = 5 * 60_000;

pub struct Options {
    pub chord: Chord,
    pub hotkey_tx: Sender<HotkeyEvent>,
}

struct ThreadState {
    hooks: Option<Hooks>,
    /// The bound chord uses a mouse button.
    chord_needs_mouse: bool,
    /// Bind mode is open, so any mouse button must be observable.
    capture_needs_mouse: bool,
}

impl ThreadState {
    fn want_mouse(&self) -> bool {
        self.chord_needs_mouse || self.capture_needs_mouse
    }

    /// Install or drop the mouse hook to match what is currently needed.
    fn sync_hooks(&mut self) {
        let want = self.want_mouse();
        if let Some(h) = self.hooks.as_mut() {
            if h.has_mouse() != want {
                if let Err(e) = h.reinstall(want) {
                    tracing::error!("adjusting hooks: {e}");
                }
            }
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

/// Set once the hook thread's window exists, so other threads can post to it.
static HOOK_WINDOW: OnceLock<SendHwnd> = OnceLock::new();

/// Tell the hook thread that bind mode is opening or closing. While it is open the mouse
/// hook is installed even if the current chord is a keyboard key, so mouse side buttons can
/// be bound; the hook watchdog also stands down.
pub fn set_capture_mode(on: bool) {
    if let Some(hwnd) = HOOK_WINDOW.get() {
        unsafe {
            let _ = PostMessageW(
                Some(hwnd.0),
                WM_CAPTURE_MODE,
                WPARAM(usize::from(on)),
                LPARAM(0),
            );
        }
    }
}

#[derive(Clone, Copy)]
struct SendHwnd(HWND);
// HWNDs are process-global tokens; posting to one from another thread is the normal use.
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

pub struct MessageThreadHandle {
    hwnd: SendHwnd,
    join: Option<JoinHandle<()>>,
}

impl MessageThreadHandle {
    pub fn rebind(&self, chord: Chord) {
        let boxed = Box::into_raw(Box::new(chord));
        let posted = unsafe {
            PostMessageW(
                Some(self.hwnd.0),
                WM_REBIND,
                WPARAM(0),
                LPARAM(boxed as isize),
            )
        };
        if posted.is_err() {
            // Reclaim; the window is gone.
            unsafe { drop(Box::from_raw(boxed)) };
        }
    }

    /// Ask the thread to remove the hooks and exit its loop.
    pub fn quit(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd.0), WM_QUIT_THREAD, WPARAM(0), LPARAM(0));
        }
    }

    pub fn join(mut self) {
        self.quit();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for MessageThreadHandle {
    fn drop(&mut self) {
        self.quit();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Starts the hook thread. Returns once the hooks are installed.
pub fn spawn(opts: Options) -> Result<MessageThreadHandle, PlatformError> {
    let Options { chord, hotkey_tx } = opts;
    hooks::init(chord, hotkey_tx)?;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<SendHwnd, PlatformError>>();

    let join = std::thread::Builder::new()
        .name("vox-hooks".into())
        .spawn(move || {
            match create_window_and_hooks(chord) {
                Ok(hwnd) => {
                    let _ = ready_tx.send(Ok(SendHwnd(hwnd)));
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            STATE.with(|s| *s.borrow_mut() = None);
        })
        .map_err(|e| PlatformError::Other(format!("spawning hook thread: {e}")))?;

    let hwnd = ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| PlatformError::Timeout("hook thread did not start".into()))??;

    Ok(MessageThreadHandle {
        hwnd,
        join: Some(join),
    })
}

fn create_window_and_hooks(chord: Chord) -> Result<HWND, PlatformError> {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)
            .map_err(PlatformError::win("GetModuleHandleW"))?
            .into();
        let class_name = w!("VoxHookWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // Zero means failure — or that the class already exists, which is fine.
        let _ = RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("Vox hooks"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
        .map_err(PlatformError::win("CreateWindowExW"))?;

        let mouse = hooks::chord_uses_mouse(&chord);
        let hooks = Hooks::install(mouse)?;
        STATE.with(|s| {
            *s.borrow_mut() = Some(ThreadState {
                hooks: Some(hooks),
                chord_needs_mouse: mouse,
                capture_needs_mouse: false,
            })
        });
        let _ = HOOK_WINDOW.set(SendHwnd(hwnd));
        SetTimer(Some(hwnd), TIMER_REHOOK, REHOOK_INTERVAL_MS, None);
        Ok(hwnd)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_REBIND => {
            // SAFETY: posted by MessageThreadHandle::rebind with a Box<Chord>.
            let chord = unsafe { *Box::from_raw(lparam.0 as *mut Chord) };
            hooks::rebind(chord);
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.chord_needs_mouse = hooks::chord_uses_mouse(&chord);
                    st.sync_hooks();
                }
            });
            LRESULT(0)
        }
        WM_CAPTURE_MODE => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.capture_needs_mouse = wparam.0 != 0;
                    st.sync_hooks();
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_REHOOK => {
            // Re-installing briefly leaves no hook in place, so never do it mid-binding.
            if !hooks::is_capturing() {
                STATE.with(|s| {
                    if let Some(st) = s.borrow_mut().as_mut() {
                        let want = st.want_mouse();
                        if let Some(h) = st.hooks.as_mut() {
                            if let Err(e) = h.reinstall(want) {
                                tracing::error!("hook watchdog re-install failed: {e}");
                            }
                        }
                    }
                });
            }
            LRESULT(0)
        }
        WM_QUIT_THREAD => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.hooks = None;
                }
            });
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
