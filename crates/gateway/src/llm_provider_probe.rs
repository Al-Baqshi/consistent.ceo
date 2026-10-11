//! OpenAI-compatible and Ollama HTTP probes for provider test + model discovery.

use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

/// Heuristic: does this provider look like a local Ollama server, based on
/// its configured API-key env var or the base URL (default port or hostname)?
pub fn is_ollama_style_base(base_url: &str, api_key_env: &str) -> bool {
    api_key_env == "OLLAMA_API_KEY"
        || base_url.contains(":11434")
        || base_url.to_lowercase().contains("ollama")
}

// Trims and shortens a response body with an ellipsis, so an error message
// embedding it stays a reasonable size.
pub(crate) fn truncate_body(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        // `max` is a byte budget; never cut inside a multi-byte char.
        format!("{}…", &t[..t.floor_char_boundary(max)])
    }
}

/// True for models that turn text into vectors and cannot hold a chat:
/// `nomic-embed-text` (which the gateway pulls for semantic recall),
/// `mxbai-embed-large`, `text-embedding-3-small`, `bge-m3`, `all-minilm`.
/// Offering one as a chat model makes every reply fail.
pub fn is_embedding_model(id: &str) -> bool {
    let id = id.trim().to_ascii_lowercase();
    let name = id.rsplit('/').next().unwrap_or(&id);
    name.contains("embed") || name.starts_with("bge-") || name.starts_with("all-minilm")
}

/// Fetches model ids from `/v1/models` (OpenAI-style) or `/api/tags` (Ollama).
pub async fn fetch_model_ids(
    base_url: &str,
    api_key: &str,
    api_key_env: &str,
) -> Result<Vec<String>, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let base = base_url.trim_end_matches('/');
    if is_ollama_style_base(base_url, api_key_env) {
        // Ollama: GET /api/tags, extract models[].name.
        let url = format!("{base}/api/tags");
        let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate_body(&text, 240)
            ));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let models = v["models"]
            .as_array()
            .ok_or_else(|| "unexpected Ollama /api/tags shape".to_string())?;
        let mut ids = Vec::new();
        for m in models {
            if let Some(name) = m["name"].as_str() {
                ids.push(name.to_string());
            }
        }
        Ok(ids)
    } else {
        // OpenAI-compatible: GET /v1/models (bearer auth), extract data[].id.
        let url = format!("{base}/v1/models");
        let mut req = client.get(url);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate_body(&text, 240)
            ));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let data = v["data"]
            .as_array()
            .ok_or_else(|| "unexpected OpenAI-style /v1/models shape".to_string())?;
        let mut ids = Vec::new();
        for m in data {
            if let Some(id) = m["id"].as_str() {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod embedding_filter_tests {
    use super::is_embedding_model;

    #[test]
    fn embedding_models_are_not_chat_models() {
        for id in [
            "nomic-embed-text:latest",
            "mxbai-embed-large",
            "text-embedding-3-small",
            "bge-m3:latest",
            "all-minilm:l6-v2",
            "models/text-embedding-004",
        ] {
            assert!(is_embedding_model(id), "{id}");
        }
        for id in [
            "gemma4:latest",
            "llama3.2:3b",
            "gpt-4o",
            "consistent-beat",
            "qwen3:8b",
        ] {
            assert!(!is_embedding_model(id), "{id}");
        }
    }
}
