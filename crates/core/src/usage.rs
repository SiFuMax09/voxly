//! Local usage statistics: how much was dictated on which day.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DayStats {
    pub words: u64,
    pub recordings: u64,
    pub seconds: u64,
}

/// Per-day totals keyed by local date (`YYYY-MM-DD`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub days: BTreeMap<String, DayStats>,
}

/// Number of words in dictated text.
pub fn count_words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

/// Duration of a 16 kHz mono 16-bit WAV (as produced by the recorder), in seconds.
pub fn wav_seconds(wav: &[u8]) -> u64 {
    (wav.len().saturating_sub(44) as u64) / (2 * 16_000)
}

/// Duration of a 16 kHz mono 16-bit WAV in milliseconds.
pub fn wav_millis(wav: &[u8]) -> u64 {
    (wav.len().saturating_sub(44) as u64) / 32
}

impl Usage {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).context("usage file is corrupt"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string(self)?)?;
        Ok(())
    }

    pub fn record(&mut self, date: &str, words: u64, seconds: u64) {
        let day = self.days.entry(date.to_owned()).or_default();
        day.words += words;
        day.recordings += 1;
        day.seconds += seconds;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_words() {
        assert_eq!(count_words("  hello   brave\nnew world "), 4);
        assert_eq!(count_words(""), 0);
    }

    #[test]
    fn accumulates_per_day() {
        let mut u = Usage::default();
        u.record("2026-10-11", 10, 5);
        u.record("2026-10-11", 4, 2);
        u.record("2026-10-12", 1, 1);
        assert_eq!(u.days["2026-10-11"], DayStats { words: 14, recordings: 2, seconds: 7 });
        assert_eq!(u.days.len(), 2);
    }

    #[test]
    fn wav_duration() {
        assert_eq!(wav_seconds(&vec![0; 44 + 32_000 * 3]), 3);
        assert_eq!(wav_seconds(&[]), 0);
    }
}
