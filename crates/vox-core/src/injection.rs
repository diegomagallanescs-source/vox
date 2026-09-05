//! How transcribed text gets into the focused application.

use serde::{Deserialize, Serialize};

/// Concrete mechanism the platform sink uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMethod {
    /// `SendInput` with `KEYEVENTF_UNICODE`: works everywhere, ~1 ms per character, no side effects.
    Unicode,
    /// Set clipboard, send Ctrl+V, restore clipboard: near-instant regardless of length.
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InjectionStrategy {
    Unicode,
    Clipboard,
    /// Unicode for short text, clipboard once it exceeds `clipboard_threshold` characters.
    Auto {
        clipboard_threshold: usize,
    },
}

impl Default for InjectionStrategy {
    fn default() -> Self {
        InjectionStrategy::Auto {
            clipboard_threshold: 120,
        }
    }
}

impl InjectionStrategy {
    pub fn choose(&self, text: &str) -> InjectMethod {
        match *self {
            InjectionStrategy::Unicode => InjectMethod::Unicode,
            InjectionStrategy::Clipboard => InjectMethod::Clipboard,
            InjectionStrategy::Auto {
                clipboard_threshold,
            } => {
                if text.chars().count() > clipboard_threshold {
                    InjectMethod::Clipboard
                } else {
                    InjectMethod::Unicode
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_strategies() {
        assert_eq!(
            InjectionStrategy::Unicode.choose(&"x".repeat(10_000)),
            InjectMethod::Unicode
        );
        assert_eq!(
            InjectionStrategy::Clipboard.choose("a"),
            InjectMethod::Clipboard
        );
    }

    #[test]
    fn auto_switches_at_threshold_by_chars_not_bytes() {
        let s = InjectionStrategy::Auto {
            clipboard_threshold: 5,
        };
        assert_eq!(s.choose("abcde"), InjectMethod::Unicode);
        assert_eq!(s.choose("abcdef"), InjectMethod::Clipboard);
        // 5 multi-byte chars, 10 bytes: still 5 characters.
        assert_eq!(s.choose("äääää"), InjectMethod::Unicode);
    }

    #[test]
    fn toml_shape() {
        #[derive(Serialize, Deserialize)]
        struct W {
            strategy: InjectionStrategy,
        }
        let w: W =
            toml::from_str("[strategy]\nkind = \"auto\"\nclipboard_threshold = 42\n").unwrap();
        assert_eq!(
            w.strategy,
            InjectionStrategy::Auto {
                clipboard_threshold: 42
            }
        );
        let w: W = toml::from_str("[strategy]\nkind = \"clipboard\"\n").unwrap();
        assert_eq!(w.strategy, InjectionStrategy::Clipboard);
        let text = toml::to_string(&W {
            strategy: InjectionStrategy::default(),
        })
        .unwrap();
        assert!(text.contains("kind = \"auto\""));
        assert!(text.contains("clipboard_threshold = 120"));
    }
}
