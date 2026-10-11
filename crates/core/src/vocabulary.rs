use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

/// A technical term the user wants recognised correctly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Term {
    /// The exact spelling, e.g. `useState` or `kubectl`.
    pub term: String,
    /// Phrases Whisper tends to produce instead, e.g. `use state`.
    /// Any match in the transcript is rewritten to `term`.
    #[serde(default)]
    pub sounds_like: Vec<String>,
}

/// Whisper only looks at roughly the last 224 prompt tokens.
const MAX_PROMPT_CHARS: usize = 700;

/// Builds the `prompt` sent to the API. It biases the decoder towards the
/// spellings in the glossary. Earlier terms win when the budget runs out.
pub fn build_prompt(terms: &[Term]) -> Option<String> {
    let mut out = String::from("Software development dictation. Glossary: ");
    let base = out.len();
    for t in terms.iter().filter(|t| !t.term.trim().is_empty()) {
        let piece = t.term.trim();
        if out.len() + piece.len() + 2 > MAX_PROMPT_CHARS {
            break;
        }
        if out.len() > base {
            out.push_str(", ");
        }
        out.push_str(piece);
    }
    (out.len() > base).then(|| {
        out.push('.');
        out
    })
}

/// Rewrites known mis-hearings to the canonical spelling.
pub fn apply_replacements(text: &str, terms: &[Term]) -> String {
    let mut text = text.to_owned();
    for t in terms {
        for alias in t.sounds_like.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
            let pattern = format!(r"(?:^|\b){}(?:\b|$)", regex::escape(alias));
            if let Ok(re) = RegexBuilder::new(&pattern).case_insensitive(true).build() {
                text = re
                    .replace_all(&text, regex::NoExpand(t.term.trim()))
                    .into_owned();
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(t: &str, s: &[&str]) -> Term {
        Term { term: t.into(), sounds_like: s.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn prompt_lists_terms() {
        let p = build_prompt(&[term("useState", &[]), term("kubectl", &[])]).unwrap();
        assert!(p.contains("useState, kubectl."));
    }

    #[test]
    fn prompt_none_when_empty() {
        assert!(build_prompt(&[]).is_none());
    }

    #[test]
    fn replaces_case_insensitively() {
        let out = apply_replacements("call Use State here", &[term("useState", &["use state"])]);
        assert_eq!(out, "call useState here");
    }

    #[test]
    fn replacement_keeps_dollar_literal() {
        let out = apply_replacements("jquery", &[term("$", &["jquery"])]);
        assert_eq!(out, "$");
    }
}
