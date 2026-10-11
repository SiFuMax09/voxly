//! On-device transcription with whisper.cpp (feature `local`) and model
//! management. Without the feature, listing models still works but
//! transcribing returns an error.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

use crate::settings::Settings;

pub struct ModelInfo {
    pub id: &'static str,
    pub file: &'static str,
    pub label: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo { id: "base", file: "ggml-base.bin", label: "Base (142 MB, fast, ok accuracy)" },
    ModelInfo { id: "small", file: "ggml-small.bin", label: "Small (466 MB, good)" },
    ModelInfo {
        id: "large-v3-turbo",
        file: "ggml-large-v3-turbo-q5_0.bin",
        label: "Large v3 Turbo, quantized (574 MB, best)",
    },
];

#[derive(Serialize)]
pub struct ModelStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub downloaded: bool,
}

pub fn model_path(models_dir: &Path, id: &str) -> Result<PathBuf> {
    let m = MODELS
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow!("unknown model '{id}'"))?;
    Ok(models_dir.join(m.file))
}

pub fn list_models(models_dir: &Path) -> Vec<ModelStatus> {
    MODELS
        .iter()
        .map(|m| ModelStatus {
            id: m.id,
            label: m.label,
            downloaded: models_dir.join(m.file).exists(),
        })
        .collect()
}

/// Downloads a model from the whisper.cpp repository on Hugging Face.
/// `progress` receives (bytes_done, bytes_total).
pub async fn download_model(
    models_dir: &Path,
    id: &str,
    progress: impl Fn(u64, u64),
) -> Result<()> {
    let dest = model_path(models_dir, id)?;
    let file = dest.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(models_dir)?;

    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{file}");
    let mut res = reqwest::get(&url).await?.error_for_status()?;
    let total = res.content_length().unwrap_or(0);

    // Write to a temp file so an interrupted download is never mistaken for a model.
    let tmp = dest.with_extension("part");
    let mut out = std::fs::File::create(&tmp)?;
    let mut done = 0u64;
    {
        use std::io::Write;
        while let Some(chunk) = res.chunk().await? {
            out.write_all(&chunk)?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        out.flush()?;
    }
    if total > 0 && done != total {
        bail!("download incomplete ({done} of {total} bytes)");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

/// Converts the 16-bit WAV we record into the f32 samples whisper.cpp wants.
#[cfg_attr(not(feature = "local"), allow(dead_code))]
fn wav_to_f32(wav: &[u8]) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(wav))?;
    reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / i16::MAX as f32).map_err(Into::into))
        .collect()
}

pub async fn transcribe(settings: &Settings, wav: Vec<u8>, models_dir: &Path) -> Result<String> {
    let path = model_path(models_dir, &settings.local_model)?;
    if !path.exists() {
        bail!("The local model '{}' is not downloaded yet. Download it in the settings.", settings.local_model);
    }
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || run(&settings, &wav, &path))
        .await
        .map_err(|e| anyhow!("transcription thread died: {e}"))?
}

/// True when this build has a GPU backend compiled in.
#[cfg(feature = "local")]
const GPU_BUILD: bool = cfg!(any(
    feature = "hip",
    feature = "vulkan",
    feature = "cuda",
    all(target_os = "macos", target_arch = "aarch64")
));

/// A loaded model plus its decoding state. Creating the state allocates the
/// compute buffers (and, on Vulkan, compiles shaders), so it is reused too.
#[cfg(feature = "local")]
struct Engine {
    state: std::sync::Mutex<whisper_rs::WhisperState>,
}

/// Loading a model takes seconds (more with a GPU), so keep the last one.
#[cfg(feature = "local")]
fn engine(model: &Path, use_gpu: bool) -> Result<std::sync::Arc<Engine>> {
    use std::sync::{Arc, Mutex, OnceLock};
    use whisper_rs::{WhisperContext, WhisperContextParameters};

    static CACHE: OnceLock<Mutex<Option<(PathBuf, bool, Arc<Engine>)>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((p, gpu, e)) = &*cache {
        if p == model && *gpu == use_gpu {
            return Ok(e.clone());
        }
    }
    let mut params = WhisperContextParameters::default();
    params.use_gpu(use_gpu);
    params.flash_attn(use_gpu);
    let ctx = WhisperContext::new_with_params(
        model.to_str().ok_or_else(|| anyhow!("non-UTF-8 model path"))?,
        params,
    )
    .map_err(|e| anyhow!("could not load model: {e}"))?;
    let state = ctx.create_state().map_err(|e| anyhow!("{e}"))?;
    let e = Arc::new(Engine { state: Mutex::new(state) });
    *cache = Some((model.to_path_buf(), use_gpu, e.clone()));
    Ok(e)
}

/// Loads the configured local model ahead of the first recording so that
/// dictation does not wait for it. Blocking; call from a background thread.
pub fn preload(settings: &Settings, models_dir: &Path) {
    #[cfg(feature = "local")]
    if settings.backend == "local" {
        if let Ok(path) = model_path(models_dir, &settings.local_model) {
            if path.exists() {
                if let Err(e) = engine(&path, settings.use_gpu) {
                    eprintln!("could not preload model: {e:#}");
                }
            }
        }
    }
    #[cfg(not(feature = "local"))]
    let _ = (settings, models_dir);
}

#[cfg(feature = "local")]
fn run(settings: &Settings, wav: &[u8], model: &Path) -> Result<String> {
    use whisper_rs::{FullParams, SamplingStrategy};

    let engine = engine(model, settings.use_gpu)?;

    let samples = wav_to_f32(wav)?;
    let mut state = engine.state.lock().unwrap();
    // Beam search is only affordable on a GPU; on the CPU greedy decoding is
    // several times faster for short dictation.
    let strategy = if GPU_BUILD {
        SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 }
    } else {
        SamplingStrategy::Greedy { best_of: 1 }
    };
    let mut params = FullParams::new(strategy);
    params.set_n_threads(std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32);
    let lang = settings.language.trim();
    params.set_language(if lang.is_empty() { None } else { Some(lang) });
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    let prompt = crate::vocabulary::build_prompt(&settings.vocabulary);
    if let Some(p) = &prompt {
        params.set_initial_prompt(p);
    }
    state.full(params, &samples).map_err(|e| anyhow!("transcription failed: {e}"))?;

    let mut text = String::new();
    for seg in state.as_iter() {
        text.push_str(&seg.to_str().map_err(|e| anyhow!("{e}"))?);
    }
    Ok(text.trim().to_owned())
}

#[cfg(not(feature = "local"))]
fn run(_: &Settings, _: &[u8], _: &Path) -> Result<String> {
    Err(anyhow!("this build does not include local transcription"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_is_an_error() {
        assert!(model_path(Path::new("/tmp"), "nope").is_err());
    }

    #[test]
    fn wav_roundtrip_length() {
        let mut buf = std::io::Cursor::new(Vec::new());
        let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
        for _ in 0..100 { w.write_sample(0i16).unwrap(); }
        w.finalize().unwrap();
        assert_eq!(wav_to_f32(&buf.into_inner()).unwrap().len(), 100);
    }
}
