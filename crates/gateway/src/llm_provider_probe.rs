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
    } else if is_anthropic_base(base) {
        // Anthropic: GET {base}/models with `x-api-key`, not a bearer token.
        let resp = client
            .get(models_url(base))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        ids_from(resp, "data", "id").await
    } else if is_google_native_base(base) {
        // Gemini's native API: GET {base}/models?key=…, names are `models/<id>`.
        let resp = client
            .get(format!("{base}/models"))
            .query(&[("key", api_key)])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let ids = ids_from(resp, "models", "name").await?;
        Ok(ids
            .into_iter()
            .map(|n| n.trim_start_matches("models/").to_string())
            .collect())
    } else {
        // OpenAI-compatible: GET {base}/models (bearer auth), extract data[].id.
        let mut req = client.get(models_url(base));
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        ids_from(resp, "data", "id").await
    }
}

/// The model-list URL for an OpenAI-style base. Every hosted base the product
/// ships already ends in a version segment (`…/v1`, `…/v1beta/openai`), so the
/// list lives directly under it; only a bare host (`http://localhost:8000`)
/// gets `/v1` added. Appending `/v1/models` unconditionally produced
/// `/v1/v1/models`, a 404 that made every API-key save fail.
pub fn models_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let versioned = base.rsplit('/').take(2).any(|seg| {
        seg.len() >= 2 && seg.starts_with('v') && seg[1..2].chars().all(|c| c.is_ascii_digit())
    });
    if versioned {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// True for Anthropic's API, which authenticates with `x-api-key`.
pub fn is_anthropic_base(base: &str) -> bool {
    base.to_ascii_lowercase().contains("anthropic.com")
}

/// True for Gemini's native endpoint (not its `/openai` compatibility layer).
pub fn is_google_native_base(base: &str) -> bool {
    let b = base.to_ascii_lowercase();
    b.contains("generativelanguage.googleapis.com") && !b.trim_end_matches('/').ends_with("/openai")
}

// Reads a JSON list response and pulls `field` out of each entry of `list`.
async fn ids_from(resp: reqwest::Response, list: &str, field: &str) -> Result<Vec<String>, String> {
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
    let entries = v[list]
        .as_array()
        .ok_or_else(|| format!("unexpected model list shape (no `{list}` array)"))?;
    Ok(entries
        .iter()
        .filter_map(|m| m[field].as_str().map(str::to_string))
        .collect())
}

#[cfg(test)]
mod models_url_tests {
    use super::*;

    #[test]
    fn versioned_bases_do_not_double_v1() {
        assert_eq!(
            models_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            models_url("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai/models"
        );
        assert_eq!(
            models_url("http://localhost:8000"),
            "http://localhost:8000/v1/models"
        );
        assert_eq!(
            models_url("https://openrouter.ai/api"),
            "https://openrouter.ai/api/v1/models"
        );
    }

    #[test]
    fn provider_shapes() {
        assert!(is_anthropic_base("https://api.anthropic.com/v1"));
        assert!(is_google_native_base(
            "https://generativelanguage.googleapis.com/v1beta"
        ));
        assert!(!is_google_native_base(
            "https://generativelanguage.googleapis.com/v1beta/openai"
        ));
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
