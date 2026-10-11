//! Spoken commands in a transcript: "neue Zeile", "Komma", "alles löschen", ...
//!
//! They are recognized in German and English and work the same for the local
//! model and the cloud API, because they run on the finished text.

use regex::{Captures, Regex, RegexBuilder};

#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub text: String,
    /// The whole utterance was "delete that": remove the text inserted last.
    pub delete_last: bool,
}

const DELETE_PHRASES: &[&str] = &[
    "alles löschen", "das löschen", "lösche das", "lösch das", "lösche alles", "zurück",
    "delete that", "delete all", "scratch that", "clear all", "undo that",
];

/// Words before "Punkt"/"period" that show it is a noun ("der Punkt"), not a
/// spoken full stop.
const NOUN_CONTEXT: &[&str] = &[
    "der", "die", "das", "ein", "eine", "einen", "einem", "einer", "dem", "den", "diesen",
    "dieser", "diesem", "jeden", "jeder", "kein", "keinen", "am", "im", "zum", "zur", "wichtigen",
    "wichtiger", "letzten", "letzter", "ersten", "erster", "springenden", "springender", "kritischen",
    "a", "the", "this", "that", "every", "no", "key", "main", "same", "one",
];

fn re(pattern: &str) -> Regex {
    RegexBuilder::new(pattern).case_insensitive(true).unicode(true).build().unwrap()
}

fn words(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn apply(text: &str) -> Outcome {
    apply_with(text, true)
}

/// `finished` is false for a piece of a dictation that is still going on:
/// whether "Punkt" ends the dictation and whether the whole thing is a delete
/// command can only be decided once it is over.
pub fn apply_with(text: &str, finished: bool) -> Outcome {
    if finished && DELETE_PHRASES.contains(&words(text).as_str()) {
        return Outcome { text: String::new(), delete_last: true };
    }

    let mut out = text.trim().to_owned();
    // Whisper may put its own punctuation around a spoken command ("Komma,").
    let rules: &[(&str, &str)] = &[
        (r"[,.;:!?]?\s*\b(?:neuer absatz|new paragraph)\b[,.;:!?]?\s*", "\n\n"),
        (r"[,.;:!?]?\s*\b(?:neue zeile|new line|newline)\b[,.;:!?]?\s*", "\n"),
        (r"\s*\b(?:komma|comma)\b[,.]?", ","),
        (r"\s*\b(?:fragezeichen|question mark)\b[,.]?", "?"),
        (r"\s*\b(?:ausrufezeichen|exclamation mark|exclamation point)\b[,.]?", "!"),
        (r"\s*\b(?:doppelpunkt|colon)\b[,.]?", ":"),
        (r"\s*\b(?:semikolon|strichpunkt|semicolon)\b[,.]?", ";"),
    ];
    for (pattern, replacement) in rules {
        out = re(pattern).replace_all(&out, *replacement).into_owned();
    }

    // "Punkt" is also an everyday noun, so it only counts at the very end
    // and when the word before it does not make it a noun.
    let end = re(r"(\S+)?\s*\b(?:punkt|period|full stop)\b[.,!?]*$");
    if finished {
        out = end
        .replace(&out, |c: &Captures| match c.get(1) {
            Some(prev) if NOUN_CONTEXT.contains(&prev.as_str().to_lowercase().trim_matches(|ch: char| !ch.is_alphanumeric())) => {
                c[0].to_owned()
            }
            Some(prev) => format!("{}.", prev.as_str().trim_end_matches([',', '.'])),
            None => ".".to_owned(),
        })
        .into_owned();
    }

    // Capitalise after a sentence end or a line break.
    out = re(r"([.!?]\s+|\n)(\p{Ll})")
        .replace_all(&out, |c: &Captures| format!("{}{}", &c[1], c[2].to_uppercase()))
        .into_owned();
    Outcome { text: out.trim().to_owned(), delete_last: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> String {
        apply(s).text
    }

    #[test]
    fn new_line_and_paragraph() {
        assert_eq!(t("Erste Zeile neue Zeile zweite Zeile"), "Erste Zeile\nZweite Zeile");
        assert_eq!(t("Hello. New paragraph. World"), "Hello\n\nWorld");
    }

    #[test]
    fn spoken_punctuation() {
        assert_eq!(t("Hallo Komma, wie geht's Fragezeichen"), "Hallo, wie geht's?");
        assert_eq!(t("Achtung Doppelpunkt vorsicht"), "Achtung: vorsicht");
    }

    #[test]
    fn punkt_only_at_the_end_and_not_as_noun() {
        assert_eq!(t("Das ist gut Punkt"), "Das ist gut.");
        assert_eq!(t("Das ist ein wichtiger Punkt."), "Das ist ein wichtiger Punkt.");
        assert_eq!(t("Der Punkt liegt links."), "Der Punkt liegt links.");
    }

    #[test]
    fn delete_commands() {
        assert!(apply("Alles löschen.").delete_last);
        assert!(apply("Scratch that").delete_last);
        assert!(!apply("Bitte alles löschen und neu anfangen").delete_last);
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(t("Das erste Video. Wir starten jetzt."), "Das erste Video. Wir starten jetzt.");
    }
}
