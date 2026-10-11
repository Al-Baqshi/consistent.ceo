//! Download a local model into Ollama from the dashboard.
//!
//! First run on a machine with Ollama installed but no model used to dead-end
//! at "Ollama isn't answering" (it was answering; it had nothing to run). This
//! lets the "Ollama (local)" card offer one button instead of a terminal
//! command: `POST /api/ollama/pull` starts the download in the background and
//! `GET /api/ollama/pull` reports progress, so the page can show a bar and
//! connect the model the moment it lands.
//!
//! After the chat model it also pulls the embedding model semantic recall
//! uses (`[memory] embedding_model`), so the next start has local recall too.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers.

use super::*;
use std::sync::{LazyLock, Mutex};

/// What a fresh install downloads when asked for "a local model": the base
/// of the repo's tuned `consistent-beat` (local-models/README.md).
pub(crate) const DEFAULT_LOCAL_CHAT_MODEL: &str = "gemma4:latest";

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullState {
    /// Model being (or last) downloaded; `None` before the first request.
    model: Option<String>,
    /// `idle`, `pulling`, `done`, or `failed`.
    status: &'static str,
    /// Ollama's own step text: "pulling manifest", "verifying sha256 digest", ...
    step: Option<String>,
    completed: u64,
    total: u64,
    error: Option<String>,
}

static PULL: LazyLock<Mutex<PullState>> = LazyLock::new(|| {
    Mutex::new(PullState {
        status: "idle",
        ..PullState::default()
    })
});

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PullRequest {
    #[serde(default)]
    model: Option<String>,
}

/// Ollama's native root for the configured `ollama` provider.
fn ollama_root(state: &ServerState) -> String {
    let base = state
        .app_config
        .as_ref()
        .and_then(|c| c.llm_providers_map.get("ollama").map(|(b, _)| b.clone()))
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
    let base = base.trim().trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_string()
}

/// Model names Ollama accepts: letters, digits, and `:._-/`. Refuses
/// anything else so a request body cannot smuggle a URL or a path.
fn valid_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.contains("://")
        && !name.contains("..")
        && !name.starts_with('/')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-' | '/'))
}

pub(super) async fn get_ollama_pull() -> Json<PullState> {
    Json(PULL.lock().unwrap().clone())
}

pub(super) async fn start_ollama_pull(
    State(state): State<ServerState>,
    body: Option<Json<PullRequest>>,
) -> Result<Json<PullState>, StatusCode> {
    let model = body
        .and_then(|Json(b)| b.model)
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_LOCAL_CHAT_MODEL.to_string());
    if !valid_model_name(&model) {
        return Err(StatusCode::BAD_REQUEST);
    }
    {
        let mut s = PULL.lock().unwrap();
        if s.status == "pulling" {
            // One download at a time; report the running one.
            return Ok(Json(s.clone()));
        }
        *s = PullState {
            model: Some(model.clone()),
            status: "pulling",
            step: Some("starting".into()),
            ..PullState::default()
        };
    }
    let root = ollama_root(&state);
    let embed_model = state
        .app_config
        .as_ref()
        .map(|c| c.embedding_model.clone())
        .unwrap_or_else(|| consistent_world_memory::embedding::DEFAULT_OLLAMA_EMBED_MODEL.into());
    tokio::spawn(async move {
        let result = pull_streaming(&root, &model).await;
        {
            let mut s = PULL.lock().unwrap();
            match result {
                Ok(()) => {
                    s.status = "done";
                    s.step = Some("ready".into());
                    s.error = None;
                }
                Err(e) => {
                    tracing::warn!(model = %model, error = %e, "ollama pull failed");
                    s.status = "failed";
                    s.error = Some(e);
                }
            }
        }
        // Semantic recall's model, quietly and after the chat model, so the
        // bar the person is watching is the one they asked for.
        let embed = consistent_world_memory::embedding::OllamaEmbedding::new(&root, &embed_model);
        if matches!(embed.has_model().await, Ok(false))
            && let Err(e) = embed.pull().await
        {
            tracing::warn!(error = %e, "embedding model pull failed");
        }
    });
    Ok(Json(PULL.lock().unwrap().clone()))
}

/// Stream `POST /api/pull` and mirror its progress into [`PULL`].
async fn pull_streaming(root: &str, model: &str) -> Result<(), String> {
    use futures::StreamExt;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(format!("{root}/api/pull"))
        .json(&serde_json::json!({ "model": model, "stream": true }))
        .send()
        .await
        .map_err(|_| {
            "Ollama isn't running on this computer. Install it from https://ollama.com/download \
             and open it, then try again."
                .to_string()
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Ollama refused the download ({status}): {}",
            body.trim()
        ));
    }
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::<u8>::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(err) = v["error"].as_str() {
                return Err(err.to_string());
            }
            let mut s = PULL.lock().unwrap();
            if let Some(step) = v["status"].as_str() {
                s.step = Some(step.to_string());
            }
            if let (Some(done), Some(total)) = (v["completed"].as_u64(), v["total"].as_u64()) {
                s.completed = done;
                s.total = total;
            }
            if v["status"] == "success" {
                return Ok(());
            }
        }
    }
    Err("the download ended before Ollama said it was complete".into())
}

#[cfg(test)]
mod tests {
    use super::valid_model_name;

    #[test]
    fn only_plain_model_names_reach_ollama() {
        for ok in [
            "gemma4:latest",
            "llama3.2:3b",
            "hf.co/org/model:Q4_K_M",
            "nomic-embed-text",
        ] {
            assert!(valid_model_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "http://x",
            "../../etc",
            "x;rm -rf",
            &"a".repeat(129),
        ] {
            assert!(!valid_model_name(bad), "{bad}");
        }
    }
}
