//! Configuration schema. The daemon owns the file (`%APPDATA%\Vox\config.toml`); the UI
//! only ever sends whole [`Config`] values over IPC.
//!
//! Every struct is `#[serde(default)]` so a partial or older file loads with defaults filled
//! in, and unknown keys are ignored.

use crate::chord::{vk, Chord, Key};
use crate::injection::InjectionStrategy;
use crate::segmenter::SegmenterConfig;
use crate::session::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub hotkey: HotkeyConfig,
    pub audio: AudioConfig,
    pub engine: EngineConfig,
    pub injection: InjectionConfig,
    pub behavior: BehaviorConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub chord: Chord,
    pub mode: Mode,
    /// Push-to-talk presses shorter than this are discarded as accidental.
    pub min_press_ms: u64,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        // F13: exists in the API, no physical key, ideal target for a mouse-button remap.
        HotkeyConfig {
            chord: Chord::new(Key::Keyboard(vk::F1 + 12)),
            mode: Mode::PushToTalk,
            min_press_ms: 250,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DeviceSelection {
    /// Windows' default *communications* capture device — what it considers your headset.
    /// AirPods become active the moment they connect.
    #[default]
    DefaultCommunications,
    /// Windows' default capture device.
    DefaultConsole,
    /// A specific WASAPI endpoint. `name` is informational (shown if the device is missing).
    Specific {
        id: String,
        #[serde(default)]
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AudioConfig {
    pub device: DeviceSelection,
    /// Keep the capture stream open this long after release. `0` = close immediately
    /// (recommended for Bluetooth headsets, see ARCHITECTURE §5).
    pub keep_warm_ms: u64,
    pub segmenter: SegmenterConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Probe available GGML backends and pick the fastest by benchmark.
    Auto,
    Cuda,
    Vulkan,
    Cpu,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Model identifier, resolved to a file under the models directory
    /// (e.g. `large-v3-turbo-q5_0` → `ggml-large-v3-turbo-q5_0.bin`).
    pub model: String,
    pub backend: Backend,
    /// CPU threads for the CPU backend / CPU-side work. `None` = physical core count.
    pub threads: Option<usize>,
    /// `1` = greedy. `2` is a good accuracy/latency trade-off on GPU.
    pub beam_size: u32,
    /// Load the model when the daemon starts instead of on first use.
    pub preload: bool,
    /// Unload the model after this many idle minutes. `0` = never.
    pub idle_unload_min: u32,
    pub language: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            // The measured CPU default (docs/benchmarks.md); GPU tiers switch to
            // `large-v3-turbo-q5_0` once a GPU backend is built.
            model: "base.en-q5_1".into(),
            backend: Backend::Auto,
            threads: None,
            beam_size: 1,
            preload: true,
            idle_unload_min: 0,
            language: "en".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InjectionConfig {
    pub strategy: InjectionStrategy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// Short tick when capture actually starts / stops.
    pub sounds: bool,
    /// Register the daemon in `HKCU\...\Run`.
    pub autostart: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        BehaviorConfig {
            sounds: true,
            autostart: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::MouseButton;

    #[test]
    fn defaults_round_trip() {
        let cfg = Config::default();
        let text = cfg.to_toml().unwrap();
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn default_hotkey_is_f13_push_to_talk() {
        let cfg = Config::default();
        assert_eq!(cfg.hotkey.chord.to_string(), "F13");
        assert_eq!(cfg.hotkey.mode, Mode::PushToTalk);
    }

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_fills_defaults_and_ignores_unknown_keys() {
        let text = r#"
            [hotkey]
            chord = "Ctrl+Mouse4"
            mode = "toggle"
            future_key = true

            [audio.device]
            kind = "specific"
            id = "{0.0.1.00000000}.{abc}"
            name = "AirPods Max"

            [engine]
            backend = "cuda"
        "#;
        let cfg = Config::from_toml(text).unwrap();
        assert_eq!(cfg.hotkey.chord.key, Key::Mouse(MouseButton::X1));
        assert!(cfg.hotkey.chord.modifiers.ctrl);
        assert_eq!(cfg.hotkey.mode, Mode::Toggle);
        assert_eq!(
            cfg.hotkey.min_press_ms,
            HotkeyConfig::default().min_press_ms
        );
        assert_eq!(
            cfg.audio.device,
            DeviceSelection::Specific {
                id: "{0.0.1.00000000}.{abc}".into(),
                name: "AirPods Max".into()
            }
        );
        assert_eq!(cfg.audio.segmenter, SegmenterConfig::default());
        assert_eq!(cfg.engine.backend, Backend::Cuda);
        assert_eq!(cfg.engine.model, EngineConfig::default().model);
    }

    #[test]
    fn specific_device_name_is_optional() {
        let cfg = Config::from_toml("[audio.device]\nkind = \"specific\"\nid = \"x\"\n").unwrap();
        assert_eq!(
            cfg.audio.device,
            DeviceSelection::Specific {
                id: "x".into(),
                name: String::new()
            }
        );
    }

    #[test]
    fn bad_chord_is_a_parse_error() {
        let err = Config::from_toml("[hotkey]\nchord = \"Hyper+Q\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
        assert!(err.to_string().contains("invalid chord"));
    }

    #[test]
    fn serialized_shape_matches_docs() {
        let text = Config::default().to_toml().unwrap();
        for needle in [
            "[hotkey]",
            "chord = \"F13\"",
            "mode = \"push_to_talk\"",
            "[audio.device]",
            "kind = \"default_communications\"",
            "[audio.segmenter]",
            "[engine]",
            "model = \"base.en-q5_1\"",
            "backend = \"auto\"",
            "[injection.strategy]",
            "kind = \"auto\"",
            "[behavior]",
        ] {
            assert!(text.contains(needle), "missing {needle} in:\n{text}");
        }
    }
}
