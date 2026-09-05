//! Transcript cleanup and joining.

/// Normalises one engine output: strips Whisper's bracketed non-speech tokens
/// (`[BLANK_AUDIO]`, `[Music]`, …), collapses whitespace, trims.
pub fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0usize;
    for ch in raw.chars() {
        match ch {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    collapse_whitespace(&out)
}

/// Joins already-cleaned segment texts into one utterance: single spaces between parts, no
/// space before closing punctuation, first letter capitalised.
pub fn join<I, S>(parts: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for part in parts {
        let part = part.as_ref().trim();
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() && !starts_with_closing_punct(part) {
            out.push(' ');
        }
        out.push_str(part);
    }
    capitalize_first(&collapse_whitespace(&out))
}

fn starts_with_closing_punct(s: &str) -> bool {
    matches!(
        s.chars().next(),
        Some(',' | '.' | '!' | '?' | ';' | ':' | ')' | '\'' | '"')
    )
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) if first.is_lowercase() => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
        _ => s.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_bracket_tokens_and_whitespace() {
        assert_eq!(clean("  [BLANK_AUDIO] "), "");
        assert_eq!(clean("hello [Music] world"), "hello world");
        assert_eq!(clean("[noise]  nested [a [b] c] text\n"), "nested text");
        assert_eq!(
            clean("  multiple   spaces\tand\ttabs "),
            "multiple spaces and tabs"
        );
        assert_eq!(clean("unbalanced ] bracket"), "unbalanced ] bracket");
        assert_eq!(clean("(parentheses stay)"), "(parentheses stay)");
    }

    #[test]
    fn join_spaces_and_capitalises() {
        assert_eq!(join(["hello", "world."]), "Hello world.");
        assert_eq!(join(["", "  ", "already Capital"]), "Already Capital");
        assert_eq!(join::<[&str; 0], &str>([]), "");
    }

    #[test]
    fn join_does_not_space_before_punctuation() {
        assert_eq!(join(["hello", ",", "world", "!"]), "Hello, world!");
        assert_eq!(join(["so", ". Then"]), "So. Then");
    }

    #[test]
    fn join_leaves_non_letters_alone() {
        assert_eq!(join(["42 is the answer"]), "42 is the answer");
        assert_eq!(join(["ünïcode works"]), "Ünïcode works");
    }
}
