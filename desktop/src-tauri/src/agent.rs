//! Bridge from a spoken transcript to the gateway's tool-calling agent.
//!
//! Dictation's "command mode" sends a committed transcript to
//! `POST /api/capture/chat` instead of typing it, so "remind me to ship the
//! invoice" creates a reminder rather than typing the words. The agent loop and
//! its workspace tools already live in the gateway
//! (`crates/gateway/src/chat_agent.rs`) — this is only the transport.
//!
//! Command mode is deliberately behind its own hotkey rather than being
//! inferred from what you said: acting on your workspace should be a choice you
//! made, not a guess we made.

use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager};

/// Where the local gateway lives and the token to talk to it.
///
/// The token is interior-mutable because it isn't known when the state is
/// registered — the gateway provisions its token file during startup, so setup
/// fills it in afterwards.
pub struct GatewayAuth {
    pub base_url: String,
    token: Mutex<Option<String>>,
}

impl Default for GatewayAuth {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:18800".to_string(),
            token: Mutex::new(None),
        }
    }
}

impl GatewayAuth {
    /// Replace the stored session token (or clear it with `None`).
    pub fn set_token(&self, token: Option<String>) {
        if let Ok(mut guard) = self.token.lock() {
            *guard = token;
        }
    }

    /// The current session token, if one has been set.
    pub fn token(&self) -> Option<String> {
        self.token.lock().ok().and_then(|guard| guard.clone())
    }
}

/// Stable per-run session id so consecutive spoken commands share the agent's
/// short-term memory buffer instead of each starting cold.
fn session_id() -> &'static str {
    static SESSION_ID: OnceLock<String> = OnceLock::new();
    SESSION_ID.get_or_init(|| format!("desktop-voice-{}", std::process::id()))
}

/// Subset of the gateway's `CaptureChatResponse` we care about here.
#[derive(Debug, Deserialize)]
struct CaptureChatResponse {
    #[serde(default)]
    message: String,
    #[serde(default)]
    actions: Vec<serde_json::Value>,
}

/// Send `text` to the agent and return its spoken-style reply.
#[tauri::command]
pub async fn send_to_agent(app: AppHandle, text: String) -> Result<String, String> {
    let (base_url, token) = {
        let auth = app.state::<GatewayAuth>();
        (auth.base_url.clone(), auth.token())
    };

    let mut req = reqwest::Client::new()
        .post(format!("{base_url}/api/capture/chat"))
        // The gateway's CaptureChatRequest is camelCase (`#[serde(rename_all]`),
        // so the key must be `sessionId`; `session_id` is silently dropped to
        // `None` and every command would start a cold session.
        .json(&serde_json::json!({
            "message": text,
            "sessionId": session_id(),
        }));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let res = req
        .send()
        .await
        .map_err(|e| format!("agent request failed: {e}"))?;

    let status = res.status();
    if !status.is_success() {
        return Err(format!("agent returned {status}"));
    }

    let body: CaptureChatResponse = res
        .json()
        .await
        .map_err(|e| format!("could not parse agent response: {e}"))?;

    // Let the UI show what the agent said and what it actually did.
    let _ = app.emit(
        "agent-reply",
        serde_json::json!({
            "message": body.message,
            "actions": body.actions,
        }),
    );

    Ok(body.message)
}

/// Fire-and-forget variant for the dictation reader task, which has no caller
/// to return an error to.
pub fn send_to_agent_detached(app: AppHandle, text: String) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = send_to_agent(app.clone(), text).await {
            eprintln!("[agent] {e}");
            let _ = app.emit("agent-reply", serde_json::json!({ "error": e }));
        }
    });
}
