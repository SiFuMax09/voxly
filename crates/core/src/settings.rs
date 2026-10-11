use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::vocabulary::Term;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Record while the keys are held down (push-to-talk).
    pub hold: String,
    /// Press once to start, press again to stop and transcribe.
    pub toggle: String,
    /// Paste the most recent transcription again.
    pub paste_last: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            hold: "CommandOrControl+Alt+Space".into(),
            toggle: "CommandOrControl+Shift+Space".into(),
            paste_last: "CommandOrControl+Shift+V".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `openai` (cloud, needs an API key) or `local` (whisper.cpp on this device).
    pub backend: String,
    /// Id from `local::MODELS`.
    pub local_model: String,
    /// The user's own OpenAI API key.
    pub api_key: String,
    /// `whisper-1`, `gpt-4o-transcribe` or `gpt-4o-mini-transcribe`.
    pub model: String,
    /// ISO-639-1 code such as `en` or `de`; empty means auto-detect.
    pub language: String,
    pub vocabulary: Vec<Term>,
    pub hotkeys: Hotkeys,
    /// Paste into the focused app after transcribing (desktop only).
    pub auto_paste: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            backend: "openai".into(),
            local_model: "base".into(),
            api_key: String::new(),
            model: "whisper-1".into(),
            language: String::new(),
            vocabulary: Vec::new(),
            hotkeys: Hotkeys::default(),
            auto_paste: true,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).context("settings file is corrupt"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            // The file contains the API key.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}
