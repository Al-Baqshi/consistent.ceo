//! Locating and (one-time) downloading the on-device Whisper model.
//!
//! Whisper ships as a single self-contained GGML file. We cache it under
//! `~/.consistent-world/models/` — the same home the gateway uses — so a fresh
//! machine pays one download and every later run is instant and offline.
//!
//! Defaults to `base.en` (~148 MB): a good accuracy/speed balance that stays
//! near real time on an Intel CPU. Override the file with
//! `CONSISTENT_WHISPER_MODEL` (e.g. `ggml-tiny.en.bin` for speed) and the source
//! with `CONSISTENT_WHISPER_MODEL_URL`.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::PathBuf;

/// The model file we fetch when nothing is cached. English-only base model.
const DEFAULT_MODEL: &str = "ggml-base.en.bin";

/// whisper.cpp's canonical model mirror. `resolve/main` streams the raw file.
const HF_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Which model filename to use, honouring the env override.
pub fn model_filename() -> String {
    std::env::var("CONSISTENT_WHISPER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string())
}

/// `~/.consistent-world/models/<file>` — where the cached model lives.
pub fn model_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot resolve home directory"))?;
    Ok(home
        .join(".consistent-world")
        .join("models")
        .join(model_filename()))
}

/// Full download URL for the chosen model.
fn model_url() -> String {
    if let Ok(url) = std::env::var("CONSISTENT_WHISPER_MODEL_URL") {
        return url;
    }
    format!("{HF_BASE}/{}", model_filename())
}

/// Return the cached model path, downloading it first if absent.
///
/// `on_download_start` fires only when a fetch actually happens, so the caller
/// can emit a `{"type":"model","status":"downloading"}` event exactly once.
/// The download streams to a `.part` file and renames on success, so an
/// interrupted download never leaves a corrupt model in place.
pub fn ensure_model(on_download_start: impl FnOnce()) -> Result<PathBuf> {
    let path = model_path()?;
    if path.exists() && std::fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(path);
    }

    on_download_start();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create model cache dir")?;
    }

    let url = model_url();
    let tmp = path.with_extension("part");

    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("build http client")?
        .get(&url)
        .send()
        .with_context(|| format!("request model from {url}"))?;

    if !resp.status().is_success() {
        return Err(anyhow!("model download failed: HTTP {} from {url}", resp.status()));
    }

    let bytes = resp.bytes().context("read model body")?;
    if bytes.len() < 1_000_000 {
        // A real Whisper model is tens of MB; anything tiny is an error page.
        return Err(anyhow!(
            "downloaded model is implausibly small ({} bytes) — wrong URL?",
            bytes.len()
        ));
    }

    {
        let mut f = std::fs::File::create(&tmp).context("create temp model file")?;
        f.write_all(&bytes).context("write model bytes")?;
        f.flush().ok();
    }
    std::fs::rename(&tmp, &path).context("move model into place")?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_path_lives_under_the_consistent_world_home() {
        let p = model_path().unwrap();
        assert!(p.ends_with(model_filename()));
        assert!(p.to_string_lossy().contains(".consistent-world"));
        assert!(p.parent().unwrap().ends_with("models"));
    }

    #[test]
    fn default_model_url_points_at_the_whisper_cpp_mirror() {
        // With no override the URL is the HF whisper.cpp repo + the filename.
        if std::env::var("CONSISTENT_WHISPER_MODEL_URL").is_err() {
            let url = model_url();
            assert!(url.starts_with(HF_BASE));
            assert!(url.ends_with(&model_filename()));
        }
    }
}
