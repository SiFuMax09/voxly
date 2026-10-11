use std::path::Path;

use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

use crate::settings::Settings;
use crate::vocabulary::{apply_replacements, build_prompt};

const ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";

#[derive(Deserialize)]
struct Response {
    text: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: String,
}

/// Transcribes WAV audio with the configured backend and applies the user's
/// vocabulary. `models_dir` is where local models are stored.
pub async fn transcribe(settings: &Settings, wav: Vec<u8>, models_dir: &Path) -> Result<String> {
    if settings.backend == "local" {
        let text = crate::local::transcribe(settings, wav, models_dir).await?;
        let text = crate::vocabulary::strip_hallucinations(&text);
        return Ok(apply_replacements(text.trim(), &settings.vocabulary));
    }
    transcribe_openai(settings, wav).await
}

/// Transcribes one chunk of a longer local recording. `context` is the text
/// before it. Applies the vocabulary like [`transcribe`].
pub async fn transcribe_chunk(
    settings: &Settings,
    wav: Vec<u8>,
    models_dir: &Path,
    context: &str,
) -> Result<String> {
    let text = crate::local::transcribe_with_context(settings, wav, models_dir, context).await?;
    let text = crate::vocabulary::strip_hallucinations(&text);
    Ok(apply_replacements(text.trim(), &settings.vocabulary))
}

async fn transcribe_openai(settings: &Settings, wav: Vec<u8>) -> Result<String> {
    if settings.api_key.trim().is_empty() {
        bail!("No OpenAI API key set. Add one in the settings.");
    }

    let part = Part::bytes(wav)
        .file_name("recording.wav")
        .mime_str("audio/wav")?;
    let mut form = Form::new()
        .part("file", part)
        .text("model", settings.model.clone())
        .text("response_format", "json");
    if !settings.language.trim().is_empty() {
        form = form.text("language", settings.language.trim().to_owned());
    }
    if let Some(prompt) = build_prompt(&settings.vocabulary) {
        form = form.text("prompt", prompt);
    }

    let res = reqwest::Client::new()
        .post(ENDPOINT)
        .bearer_auth(settings.api_key.trim())
        .multipart(form)
        .send()
        .await
        .context("could not reach the OpenAI API")?;

    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        let msg = serde_json::from_str::<ErrorBody>(&body)
            .map(|e| e.error.message)
            .unwrap_or(body);
        bail!("OpenAI returned {status}: {msg}");
    }

    let parsed: Response = serde_json::from_str(&body).context("unexpected API response")?;
    Ok(apply_replacements(parsed.text.trim(), &settings.vocabulary))
}
