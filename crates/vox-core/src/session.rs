//! The dictation session state machine.
//!
//! Pure and synchronous: the coordinator feeds [`Event`]s with a monotonic timestamp and
//! executes the returned [`Action`]s (open/close the mic stream, start/stop capture, run
//! transcription, inject text). Keeping it free of I/O makes every awkward interleaving —
//! releasing before the mic opened, pressing again while a transcript is still being
//! produced, the device vanishing mid-sentence — a plain unit test.
//!
//! ```text
//! Idle ─press─► Arming ─stream opened─► Recording ─release─► Finalizing ─text─► Injecting ─done─► Idle
//!                 │ stream failed → Idle + notify       │ held < min_press → Idle (discard)
//!                 │ released before open → Idle          │ device lost → Finalizing + notify
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Hold to record, release to transcribe.
    PushToTalk,
    /// Tap to start, tap again to stop.
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    /// Hotkey pressed; waiting for the capture stream to open.
    Arming,
    Recording,
    /// Capture ended; waiting for the final transcript.
    Finalizing,
    /// Text handed to the sink; waiting for it to finish typing/pasting.
    Injecting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    HotkeyPressed,
    HotkeyReleased,
    /// Capture stream is delivering audio.
    StreamOpened,
    /// Capture stream could not be opened, or died while recording.
    StreamFailed(String),
    /// Complete transcript for the capture that just ended (may be empty).
    Finalized(String),
    /// Text sink finished.
    Injected,
    /// User or system abort (Escape, lock, shutdown).
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    OpenStream,
    /// Start feeding frames to the segmenter/engine.
    BeginCapture,
    /// Stop feeding frames and flush the tail through the engine; produces `Finalized`.
    EndCapture,
    /// Stop feeding frames and drop everything buffered.
    DiscardCapture,
    /// Release the device (the coordinator may honour a keep-warm grace period).
    CloseStream,
    Inject(String),
    Notify(Notice),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    MicUnavailable(String),
    DeviceLost(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Session {
    mode: Mode,
    min_press_ms: u64,
    state: State,
    record_started_ms: u64,
    /// A press arrived while we were still finalizing/injecting; replay it when idle.
    pending_press: bool,
}

impl Session {
    pub fn new(mode: Mode, min_press_ms: u64) -> Self {
        Session {
            mode,
            min_press_ms,
            state: State::Idle,
            record_started_ms: 0,
            pending_press: false,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Takes effect immediately; an in-flight recording keeps whatever rules it started with
    /// only insofar as the next event is interpreted under the new mode.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_min_press_ms(&mut self, ms: u64) {
        self.min_press_ms = ms;
    }

    /// Feed one event. `now_ms` is any monotonic millisecond clock.
    pub fn handle(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.step(event, now_ms, &mut out);
        out
    }

    fn step(&mut self, event: Event, now_ms: u64, out: &mut Vec<Action>) {
        use Event::*;
        use State::*;
        let ptt = self.mode == Mode::PushToTalk;
        let toggle = !ptt;

        match (self.state, event) {
            // ---- Idle -----------------------------------------------------------------
            (Idle, HotkeyPressed) => {
                self.state = Arming;
                out.push(Action::OpenStream);
            }
            (Idle, _) => {}

            // ---- Arming ---------------------------------------------------------------
            (Arming, StreamOpened) => {
                self.state = Recording;
                self.record_started_ms = now_ms;
                out.push(Action::BeginCapture);
            }
            (Arming, StreamFailed(e)) => {
                self.state = Idle;
                out.push(Action::Notify(Notice::MicUnavailable(e)));
            }
            // Let go before the mic even opened: nothing captured, nothing to do.
            (Arming, HotkeyReleased) if ptt => {
                self.state = Idle;
                out.push(Action::CloseStream);
            }
            // Second tap while arming (toggle): treat as cancel.
            (Arming, HotkeyPressed) if toggle => {
                self.state = Idle;
                out.push(Action::CloseStream);
            }
            (Arming, Cancel) => {
                self.state = Idle;
                out.push(Action::CloseStream);
            }
            (Arming, _) => {}

            // ---- Recording ------------------------------------------------------------
            (Recording, HotkeyReleased) if ptt => self.stop_recording(now_ms, out),
            (Recording, HotkeyPressed) if toggle => self.stop_recording(now_ms, out),
            (Recording, StreamFailed(e)) => {
                // Keep what we have; the user was mid-sentence.
                out.push(Action::Notify(Notice::DeviceLost(e)));
                self.state = Finalizing;
                out.push(Action::EndCapture);
                out.push(Action::CloseStream);
            }
            (Recording, Cancel) => {
                self.state = Idle;
                out.push(Action::DiscardCapture);
                out.push(Action::CloseStream);
                out.push(Action::Notify(Notice::Cancelled));
            }
            (Recording, _) => {}

            // ---- Finalizing -----------------------------------------------------------
            (Finalizing, Finalized(text)) => {
                if text.trim().is_empty() {
                    self.state = Idle;
                    self.drain_pending(now_ms, out);
                } else {
                    self.state = Injecting;
                    out.push(Action::Inject(text));
                }
            }
            (Finalizing, HotkeyPressed) => self.pending_press = true,
            (Finalizing, HotkeyReleased) if ptt => self.pending_press = false,
            (Finalizing, Cancel) => {
                // The late `Finalized` will land in Idle and be ignored.
                self.state = Idle;
                self.pending_press = false;
                out.push(Action::DiscardCapture);
                out.push(Action::Notify(Notice::Cancelled));
            }
            (Finalizing, _) => {}

            // ---- Injecting ------------------------------------------------------------
            (Injecting, Injected) => {
                self.state = Idle;
                self.drain_pending(now_ms, out);
            }
            (Injecting, HotkeyPressed) => self.pending_press = true,
            (Injecting, HotkeyReleased) if ptt => self.pending_press = false,
            (Injecting, Cancel) => {
                self.state = Idle;
                self.pending_press = false;
            }
            (Injecting, _) => {}
        }
    }

    fn stop_recording(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        let held = now_ms.saturating_sub(self.record_started_ms);
        let too_short = self.mode == Mode::PushToTalk && held < self.min_press_ms;
        if too_short {
            self.state = State::Idle;
            out.push(Action::DiscardCapture);
        } else {
            self.state = State::Finalizing;
            out.push(Action::EndCapture);
        }
        out.push(Action::CloseStream);
    }

    fn drain_pending(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        if self.pending_press {
            self.pending_press = false;
            self.step(Event::HotkeyPressed, now_ms, out);
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;
    use Event::*;

    const MIN_PRESS: u64 = 250;

    fn ptt() -> Session {
        Session::new(Mode::PushToTalk, MIN_PRESS)
    }
    fn toggle() -> Session {
        Session::new(Mode::Toggle, MIN_PRESS)
    }

    /// Drives a session to `Recording` at t=0.
    fn recording(mut s: Session) -> Session {
        assert_eq!(s.handle(HotkeyPressed, 0), vec![OpenStream]);
        assert_eq!(s.handle(StreamOpened, 0), vec![BeginCapture]);
        assert_eq!(s.state(), State::Recording);
        s
    }

    #[test]
    fn push_to_talk_happy_path() {
        let mut s = recording(ptt());
        assert_eq!(
            s.handle(HotkeyReleased, 2000),
            vec![EndCapture, CloseStream]
        );
        assert_eq!(s.state(), State::Finalizing);
        assert_eq!(
            s.handle(Finalized("hello world".into()), 2300),
            vec![Inject("hello world".into())]
        );
        assert_eq!(s.state(), State::Injecting);
        assert_eq!(s.handle(Injected, 2350), vec![]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn toggle_happy_path() {
        let mut s = recording(toggle());
        // Release of the first tap is ignored in toggle mode.
        assert_eq!(s.handle(HotkeyReleased, 100), vec![]);
        assert_eq!(s.state(), State::Recording);
        // Second tap stops.
        assert_eq!(s.handle(HotkeyPressed, 3000), vec![EndCapture, CloseStream]);
        assert_eq!(s.state(), State::Finalizing);
        assert_eq!(s.handle(HotkeyReleased, 3050), vec![]);
        assert!(!s.pending_press, "toggle release must not cancel anything");
    }

    #[test]
    fn toggle_ignores_min_press() {
        let mut s = recording(toggle());
        assert_eq!(s.handle(HotkeyPressed, 50), vec![EndCapture, CloseStream]);
    }

    #[test]
    fn short_tap_discards() {
        let mut s = recording(ptt());
        assert_eq!(
            s.handle(HotkeyReleased, MIN_PRESS - 1),
            vec![DiscardCapture, CloseStream]
        );
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn exactly_min_press_counts() {
        let mut s = recording(ptt());
        assert_eq!(
            s.handle(HotkeyReleased, MIN_PRESS),
            vec![EndCapture, CloseStream]
        );
    }

    #[test]
    fn release_before_stream_opens_goes_idle() {
        let mut s = ptt();
        s.handle(HotkeyPressed, 0);
        assert_eq!(s.handle(HotkeyReleased, 20), vec![CloseStream]);
        assert_eq!(s.state(), State::Idle);
        // A late StreamOpened in Idle is ignored (coordinator already closed it).
        assert_eq!(s.handle(StreamOpened, 40), vec![]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn stream_failure_while_arming_notifies() {
        let mut s = ptt();
        s.handle(HotkeyPressed, 0);
        assert_eq!(
            s.handle(StreamFailed("no device".into()), 10),
            vec![Notify(Notice::MicUnavailable("no device".into()))]
        );
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn device_lost_mid_recording_finalizes_what_we_have() {
        let mut s = recording(ptt());
        assert_eq!(
            s.handle(StreamFailed("unplugged".into()), 1500),
            vec![
                Notify(Notice::DeviceLost("unplugged".into())),
                EndCapture,
                CloseStream
            ]
        );
        assert_eq!(s.state(), State::Finalizing);
        // The user's eventual release is irrelevant now.
        assert_eq!(s.handle(HotkeyReleased, 1600), vec![]);
        assert_eq!(s.state(), State::Finalizing);
    }

    #[test]
    fn empty_transcript_skips_injection() {
        let mut s = recording(ptt());
        s.handle(HotkeyReleased, 1000);
        assert_eq!(s.handle(Finalized("   ".into()), 1200), vec![]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn press_during_finalizing_is_replayed_after_injection() {
        let mut s = recording(ptt());
        s.handle(HotkeyReleased, 1000);
        assert_eq!(s.handle(HotkeyPressed, 1100), vec![]);
        assert_eq!(s.state(), State::Finalizing);
        assert_eq!(
            s.handle(Finalized("one".into()), 1200),
            vec![Inject("one".into())]
        );
        // Injection completes; the held key starts a fresh recording immediately.
        assert_eq!(s.handle(Injected, 1250), vec![OpenStream]);
        assert_eq!(s.state(), State::Arming);
    }

    #[test]
    fn press_during_finalizing_with_empty_transcript_is_replayed_too() {
        let mut s = recording(ptt());
        s.handle(HotkeyReleased, 1000);
        s.handle(HotkeyPressed, 1100);
        assert_eq!(s.handle(Finalized(String::new()), 1200), vec![OpenStream]);
        assert_eq!(s.state(), State::Arming);
    }

    #[test]
    fn press_and_release_during_finalizing_cancels_replay_in_ptt() {
        let mut s = recording(ptt());
        s.handle(HotkeyReleased, 1000);
        s.handle(HotkeyPressed, 1100);
        s.handle(HotkeyReleased, 1150);
        assert_eq!(
            s.handle(Finalized("x".into()), 1200),
            vec![Inject("x".into())]
        );
        assert_eq!(s.handle(Injected, 1250), vec![]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn press_during_injecting_is_replayed() {
        let mut s = recording(ptt());
        s.handle(HotkeyReleased, 1000);
        s.handle(Finalized("x".into()), 1200);
        assert_eq!(s.state(), State::Injecting);
        assert_eq!(s.handle(HotkeyPressed, 1210), vec![]);
        assert_eq!(s.handle(Injected, 1250), vec![OpenStream]);
    }

    #[test]
    fn cancel_while_recording_discards() {
        let mut s = recording(ptt());
        assert_eq!(
            s.handle(Cancel, 500),
            vec![DiscardCapture, CloseStream, Notify(Notice::Cancelled)]
        );
        assert_eq!(s.state(), State::Idle);
        assert_eq!(
            s.handle(HotkeyReleased, 600),
            vec![],
            "stale release is ignored"
        );
    }

    #[test]
    fn cancel_while_finalizing_ignores_late_transcript() {
        let mut s = recording(ptt());
        s.handle(HotkeyReleased, 1000);
        s.handle(HotkeyPressed, 1050); // would have been replayed
        assert_eq!(
            s.handle(Cancel, 1100),
            vec![DiscardCapture, Notify(Notice::Cancelled)]
        );
        assert_eq!(s.state(), State::Idle);
        assert_eq!(s.handle(Finalized("late".into()), 1300), vec![]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn cancel_while_arming_closes_stream() {
        let mut s = ptt();
        s.handle(HotkeyPressed, 0);
        assert_eq!(s.handle(Cancel, 5), vec![CloseStream]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn toggle_second_tap_while_arming_cancels() {
        let mut s = toggle();
        s.handle(HotkeyPressed, 0);
        assert_eq!(s.handle(HotkeyPressed, 30), vec![CloseStream]);
        assert_eq!(s.state(), State::Idle);
    }

    #[test]
    fn ptt_press_while_recording_is_ignored() {
        let mut s = recording(ptt());
        assert_eq!(s.handle(HotkeyPressed, 100), vec![]);
        assert_eq!(s.state(), State::Recording);
    }

    #[test]
    fn stray_events_in_idle_are_ignored() {
        let mut s = ptt();
        for ev in [
            HotkeyReleased,
            StreamOpened,
            StreamFailed("x".into()),
            Finalized("x".into()),
            Injected,
            Cancel,
        ] {
            assert_eq!(s.handle(ev, 0), vec![]);
            assert_eq!(s.state(), State::Idle);
        }
    }
}
