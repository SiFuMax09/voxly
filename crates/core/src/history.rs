//! Local history of dictated texts, newest last.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Oldest entries are dropped beyond this many.
pub const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Unix time in milliseconds; also serves as the entry's id.
    pub id: i64,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub entries: Vec<Entry>,
}

impl History {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).context("history file is corrupt"),
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

    pub fn add(&mut self, id: i64, text: &str) {
        // Keep ids unique even if two dictations land in the same millisecond.
        let id = self.entries.last().map_or(id, |e| id.max(e.id + 1));
        self.entries.push(Entry { id, text: text.to_owned() });
        let excess = self.entries.len().saturating_sub(MAX_ENTRIES);
        self.entries.drain(..excess);
    }

    pub fn remove(&mut self, id: i64) {
        self.entries.retain(|e| e.id != id);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_keeps_newest() {
        let mut h = History::default();
        for i in 0..(MAX_ENTRIES as i64 + 5) {
            h.add(i, &format!("t{i}"));
        }
        assert_eq!(h.entries.len(), MAX_ENTRIES);
        assert_eq!(h.entries.first().unwrap().text, "t5");
        assert_eq!(h.entries.last().unwrap().text, format!("t{}", MAX_ENTRIES + 4));
    }

    #[test]
    fn ids_stay_unique_and_remove_works() {
        let mut h = History::default();
        h.add(100, "a");
        h.add(100, "b");
        assert_ne!(h.entries[0].id, h.entries[1].id);
        let first = h.entries[0].id;
        h.remove(first);
        assert_eq!(h.entries.len(), 1);
        assert_eq!(h.entries[0].text, "b");
    }
}
