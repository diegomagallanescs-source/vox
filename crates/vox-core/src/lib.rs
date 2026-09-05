//! Platform-independent core for Vox.
//!
//! Nothing in this crate touches Win32, audio hardware, or a speech model. It holds the
//! logic that is worth testing in isolation: hotkey chord parsing and matching, the
//! dictation session state machine, VAD-driven audio segmentation, transcript cleanup,
//! injection strategy selection, and the configuration schema.
//!
//! Platform and engine crates implement the traits in [`traits`] and drive the pure
//! types from here.

pub mod chord;
pub mod config;
pub mod injection;
pub mod segmenter;
pub mod session;
pub mod text;
pub mod traits;
pub mod vad;

pub use chord::{Chord, ChordMatcher, HotkeyEvent, InputEvent, Key, Modifiers, MouseButton};
pub use config::Config;
pub use injection::{InjectMethod, InjectionStrategy};
pub use segmenter::{Segment, Segmenter, SegmenterConfig};
pub use session::{Action, Event, Mode, Notice, Session, State};
pub use traits::{Engine, EngineError, SinkError, TextSink, TranscribeOptions, Vad, SAMPLE_RATE};
