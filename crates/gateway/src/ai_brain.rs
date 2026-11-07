//! The one AI everything runs on.
//!
//! The product promises three ways to think — the owner's subscription (the
//! `claude` or `codex` CLI), a local model (Ollama), or an API key — and for a
//! long time each of them could be "connected" in Settings while the rest of
//! the gateway kept calling something else. Chat honored the settings page;
//! heartbeats, tasks, entity analysis and enrichment resolved through the TOML
//! and its aliases, which on a desktop install named a `claude` CLI that was
//! not on `PATH` and an Ollama model that had never been downloaded. Every
//! lane was dead, the UI said "connected", and analysis reported no AI.
//!
//! This module closes that gap in one place:
//!
//! - **The chosen brain.** What the owner picked on the AI page, persisted as
//!   `ai.json` beside `control_state.json`. Once chosen, it runs every LLM call
//!   the gateway makes, whatever lane the caller resolved — "the AI you pick
//!   is the AI that runs" is the whole contract.
//! - **Detection.** A snapshot of what this computer can actually use right
//!   now: CLIs on disk and signed in, Ollama running with a *chat* model (an
//!   embedding model cannot hold a conversation), API keys saved. Refreshed in
//!   the background and on every visit to the AI page.
//! - **The safety net.** With nothing chosen, a caller whose lane cannot work
//!   is quietly handed the first lane that can, instead of failing.
//!
//! The hook is [`crate::capture::build_llm_client`], the funnel every LLM call
//! already goes through, so no call site needed to learn about any of this.

use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How a provider is paid for, which is also how the AI page groups it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    Subscription,
    Local,
    ApiKey,
}

/// One provider the AI page offers. The list is fixed on purpose: every entry
/// is a lane the client builder knows how to drive, with the right base URL
/// and key variable — the old free-form provider form let a Gemini key land
/// under a name nothing read and a base URL nothing could call.
#[derive(Debug, Clone, Copy)]
pub struct Catalog {
    pub id: &'static str,
    pub kind: BrainKind,
    pub label: &'static str,
    pub base_url: &'static str,
    pub api_key_env: &'static str,
    pub default_model: &'static str,
    /// Models offered before (or without) a live model list.
    pub models: &'static [&'static str],
    /// Where the owner gets a key or the app, shown on the AI page.
    pub help_url: &'static str,
}

pub const CATALOG: &[Catalog] = &[
    Catalog {
        id: "claude-code",
        kind: BrainKind::Subscription,
        label: "Claude",
        base_url: "claude-code://",
        api_key_env: "CLAUDE_CODE_CLI",
        default_model: "sonnet",
        models: &["sonnet", "opus", "haiku"],
        help_url: "https://claude.com/download",
    },
    Catalog {
        id: "codex",
        kind: BrainKind::Subscription,
        label: "ChatGPT (Codex)",
        base_url: "codex://",
        api_key_env: "CODEX_CLI",
        default_model: "default",
        models: &["default"],
        help_url: "https://developers.openai.com/codex/cli",
    },
    Catalog {
        id: "ollama",
        kind: BrainKind::Local,
        label: "Ollama",
        base_url: "http://127.0.0.1:11434",
        api_key_env: "OLLAMA_API_KEY",
        default_model: crate::config::LOCAL_FALLBACK_MODEL,
        models: &[],
        help_url: "https://ollama.com/download",
    },
    Catalog {
        id: "anthropic",
        kind: BrainKind::ApiKey,
        label: "Anthropic (Claude API)",
        base_url: "https://api.anthropic.com/v1",
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-5-5",
        models: &["claude-sonnet-5-5", "claude-opus-5-5", "claude-haiku-5-5"],
        help_url: "https://console.anthropic.com/settings/keys",
    },
    Catalog {
        id: "openai",
        kind: BrainKind::ApiKey,
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        default_model: "gpt-4o",
        models: &["gpt-4o", "gpt-4o-mini"],
        help_url: "https://platform.openai.com/api-keys",
    },
    Catalog {
        id: "google",
        kind: BrainKind::ApiKey,
        label: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        api_key_env: "GEMINI_API_KEY",
        default_model: "gemini-2.5-flash",
        models: &["gemini-2.5-flash", "gemini-2.5-pro"],
        help_url: "https://aistudio.google.com/apikey",
    },
    Catalog {
        id: "deepseek",
        kind: BrainKind::ApiKey,
        label: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        api_key_env: "DEEPSEEK_API_KEY",
        default_model: "deepseek-chat",
        models: &["deepseek-chat", "deepseek-reasoner"],
        help_url: "https://platform.deepseek.com/api_keys",
    },
];

/// Ollama's native root for this install: the configured `ollama` provider
/// with any trailing `/v1` removed, else the local default. Detection, the
/// connect test, and every call the chosen brain makes must agree on it, or a
/// page that says "ready" tests one server and runs on another.
pub fn ollama_base(cfg: &crate::config::AppConfig) -> String {
    let base = cfg
        .llm_providers_map
        .get("ollama")
        .map(|(b, _)| b.trim().trim_end_matches('/'))
        .filter(|b| !b.is_empty())
        .unwrap_or("http://127.0.0.1:11434");
    base.strip_suffix("/v1").unwrap_or(base).to_string()
}

pub fn catalog(id: &str) -> Option<&'static Catalog> {
    CATALOG
        .iter()
        .find(|c| c.id.eq_ignore_ascii_case(id.trim()))
}

/// A concrete, callable choice: one provider and one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Brain {
    pub provider: String,
    pub model: String,
}

impl Brain {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    pub fn catalog(&self) -> Option<&'static Catalog> {
        catalog(&self.provider)
    }

    pub fn base_url(&self) -> &'static str {
        self.catalog().map(|c| c.base_url).unwrap_or_default()
    }

    pub fn api_key_env(&self) -> &'static str {
        self.catalog().map(|c| c.api_key_env).unwrap_or_default()
    }
}

/// Which catalog lane a resolved `(base_url, api_key_env)` pair belongs to —
/// the same hints the client builder dispatches on.
pub fn lane_of(base_url: &str, api_key_env: &str) -> Option<&'static str> {
    let base = base_url.trim().to_ascii_lowercase();
    let env = api_key_env.trim();
    if base.starts_with("claude-code") || env == "CLAUDE_CODE_CLI" {
        Some("claude-code")
    } else if base.starts_with("codex") || env == "CODEX_CLI" {
        Some("codex")
    } else if crate::capture::is_ollama_target(&base, env) {
        Some("ollama")
    } else if env == "ANTHROPIC_API_KEY" || base.contains("api.anthropic.com") {
        Some("anthropic")
    } else if env == "GEMINI_API_KEY"
        || env == "GOOGLE_API_KEY"
        || base.contains("generativelanguage.googleapis.com")
    {
        Some("google")
    } else if env == "DEEPSEEK_API_KEY" || base.contains("deepseek.com") {
        Some("deepseek")
    } else if env == "OPENAI_API_KEY" || base.contains("api.openai.com") {
        Some("openai")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// A subscription CLI as this computer has it.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliState {
    pub installed: bool,
    /// Credentials exist on disk. Only a real turn proves the sign-in is live.
    pub signed_in: bool,
    pub path: Option<String>,
}

/// What this computer can run right now.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub claude: CliState,
    pub codex: CliState,
    pub ollama_running: bool,
    /// Chat models only; embedding models are filtered out.
    pub ollama_models: Vec<String>,
    /// Catalog ids whose API key is saved (keyring) or set in the environment.
    pub keys: Vec<String>,
}

impl Detection {
    /// Can this lane take a call right now?
    pub fn usable(&self, lane: &str) -> bool {
        match lane {
            "claude-code" => self.claude.installed && self.claude.signed_in,
            "codex" => self.codex.installed && self.codex.signed_in,
            "ollama" => self.ollama_running && !self.ollama_models.is_empty(),
            other => self.keys.iter().any(|k| k == other),
        }
    }

    /// The best lane this computer can run, in the order a person would
    /// expect: the subscription they already pay for, then the free local
    /// model, then a saved key.
    pub fn best(&self) -> Option<Brain> {
        if self.usable("claude-code") {
            return Some(Brain::new("claude-code", "sonnet"));
        }
        if self.usable("codex") {
            return Some(Brain::new("codex", "default"));
        }
        if self.usable("ollama") {
            return Some(Brain::new(
                "ollama",
                &preferred_local_model(&self.ollama_models),
            ));
        }
        CATALOG
            .iter()
            .filter(|c| c.kind == BrainKind::ApiKey)
            .find(|c| self.usable(c.id))
            .map(|c| Brain::new(c.id, c.default_model))
    }
}

/// The installed chat model to use when the owner has not picked one: the
/// one onboarding pulls if present, otherwise the first.
pub fn preferred_local_model(models: &[String]) -> String {
    let wanted = crate::config::LOCAL_FALLBACK_MODEL;
    let base = wanted.split(':').next().unwrap_or(wanted);
    models
        .iter()
        .find(|m| m.as_str() == wanted)
        .or_else(|| models.iter().find(|m| m.starts_with(base)))
        .or_else(|| models.first())
        .cloned()
        .unwrap_or_else(|| wanted.to_string())
}

struct Snapshot {
    detection: Detection,
    at: Option<Instant>,
}

static DETECTION: LazyLock<RwLock<Snapshot>> = LazyLock::new(|| {
    RwLock::new(Snapshot {
        detection: Detection::default(),
        at: None,
    })
});

/// The last detection, `None` before the first one finished. Before that the
/// safety net stays out of the way: guessing from an empty snapshot would
/// call every lane dead.
pub fn detection() -> Option<Detection> {
    let snap = DETECTION.read().ok()?;
    snap.at.map(|_| snap.detection.clone())
}

/// Look at the machine again and store the answer.
pub async fn refresh(ollama_base: &str) -> Detection {
    let (claude, codex) = tokio::join!(
        crate::agent_cli_probe::probe("claude"),
        crate::agent_cli_probe::probe("codex"),
    );
    let ollama =
        crate::llm_provider_probe::fetch_model_ids(ollama_base, "", "OLLAMA_API_KEY").await;
    let keys = tokio::task::spawn_blocking(|| {
        CATALOG
            .iter()
            .filter(|c| c.kind == BrainKind::ApiKey)
            .filter(|c| crate::credentials_resolver::llm_secret_configured(c.api_key_env))
            .map(|c| c.id.to_string())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let detection = Detection {
        claude: CliState {
            installed: claude.installed,
            signed_in: claude.installed && claude.credentials_likely,
            path: claude.path,
        },
        codex: CliState {
            installed: codex.installed,
            signed_in: codex.installed && codex.credentials_likely,
            path: codex.path,
        },
        ollama_running: ollama.is_ok(),
        ollama_models: ollama
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !crate::llm_provider_probe::is_embedding_model(m))
            .collect(),
        keys,
    };
    if let Ok(mut snap) = DETECTION.write() {
        snap.detection = detection.clone();
        snap.at = Some(Instant::now());
    }
    detection
}

/// Keep detection fresh so a lane that dies (Ollama quit, CLI signed out) or
/// appears (a model finished downloading) is noticed without a restart.
pub fn spawn_refresher(ollama_base: String) {
    tokio::spawn(async move {
        loop {
            refresh(&ollama_base).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// The chosen brain
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Chosen {
    brain: Option<Brain>,
    path: Option<PathBuf>,
}

static CHOSEN: LazyLock<RwLock<Chosen>> = LazyLock::new(|| RwLock::new(Chosen::default()));

/// Load the owner's choice from `<state_dir>/ai.json` and remember where to
/// save the next one. Without this call (tests) choices live in memory only.
pub fn init(state_dir: PathBuf) {
    let path = state_dir.join("ai.json");
    let brain = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Brain>(&raw).ok())
        .filter(|b| b.catalog().is_some());
    if let Ok(mut chosen) = CHOSEN.write() {
        chosen.brain = brain;
        chosen.path = Some(path);
    }
}

pub fn chosen() -> Option<Brain> {
    CHOSEN.read().ok()?.brain.clone()
}

/// Make `brain` the AI everything runs on (or clear the choice with `None`,
/// which hands the decision back to detection), and write it to disk.
pub fn set_chosen(brain: Option<Brain>) -> std::io::Result<()> {
    let mut chosen = CHOSEN
        .write()
        .map_err(|_| std::io::Error::other("ai choice lock poisoned"))?;
    if let Some(path) = &chosen.path {
        match &brain {
            Some(b) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, serde_json::to_vec_pretty(b)?)?;
            }
            None => match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            },
        }
    }
    chosen.brain = brain;
    Ok(())
}

/// The brain that will actually answer: the owner's choice while it works,
/// otherwise the best lane detection found.
pub fn effective() -> Option<Brain> {
    let detection = detection();
    if let Some(brain) = chosen() {
        let works = detection.as_ref().is_none_or(|d| d.usable(&brain.provider));
        if works {
            return Some(brain);
        }
    }
    detection.and_then(|d| d.best())
}

/// Should a call the caller resolved to `(base_url, api_key_env)` run on a
/// different brain? `Some` names the replacement.
///
/// - A chosen brain wins over every lane and model: the settings page is
///   the source of truth, not the TOML's aliases.
/// - With nothing chosen (or the choice broken), a lane that cannot work is
///   replaced by one that can; a working lane is left alone.
pub fn replacement_for(base_url: &str, api_key_env: &str) -> Option<Brain> {
    let lane = lane_of(base_url, api_key_env);
    let detection = detection();
    if let Some(brain) = chosen() {
        let works = detection.as_ref().is_none_or(|d| d.usable(&brain.provider));
        // Model included: the TOML's tiers pin `opus` on the same lane, and a
        // spent Opus window while the chosen Sonnet answers fine is exactly
        // the failure this page exists to end.
        if works {
            return Some(brain);
        }
    }
    let detection = detection?;
    match lane {
        Some(lane) if detection.usable(lane) => None,
        // An unknown lane (a custom OpenAI-compatible server) is the owner's
        // own configuration; nothing here can judge it.
        None => None,
        Some(_) => detection.best(),
    }
}

/// Is there any AI at all that a call could land on?
pub fn any_ready() -> bool {
    effective().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> Detection {
        Detection {
            claude: CliState::default(),
            codex: CliState::default(),
            ollama_running: true,
            ollama_models: vec!["llama3.2:3b".into()],
            keys: vec![],
        }
    }

    #[test]
    fn lanes_follow_the_client_builder_hints() {
        assert_eq!(lane_of("claude-code://", ""), Some("claude-code"));
        assert_eq!(lane_of("", "CODEX_CLI"), Some("codex"));
        assert_eq!(lane_of("http://127.0.0.1:11434", ""), Some("ollama"));
        assert_eq!(
            lane_of("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY"),
            Some("anthropic")
        );
        assert_eq!(
            lane_of(
                "https://generativelanguage.googleapis.com/v1beta",
                "GOOGLE_API_KEY"
            ),
            Some("google")
        );
        assert_eq!(lane_of("http://my-server:8000/v1", "MY_API_KEY"), None);
    }

    #[test]
    fn best_prefers_subscription_then_local_then_key() {
        let mut d = det();
        d.keys = vec!["openai".into()];
        assert_eq!(d.best(), Some(Brain::new("ollama", "llama3.2:3b")));
        d.claude = CliState {
            installed: true,
            signed_in: true,
            path: None,
        };
        assert_eq!(d.best().unwrap().provider, "claude-code");
        d.claude.signed_in = false;
        d.ollama_models.clear();
        assert_eq!(d.best().unwrap().provider, "openai");
    }

    #[test]
    fn embedding_only_ollama_is_not_a_brain() {
        let mut d = det();
        d.ollama_models.clear();
        assert!(!d.usable("ollama"));
        assert_eq!(d.best(), None);
    }

    #[test]
    fn ollama_base_follows_the_configured_provider() {
        let mut cfg = crate::config::AppConfig::default();
        assert_eq!(ollama_base(&cfg), "http://127.0.0.1:11434");
        cfg.llm_providers_map.insert(
            "ollama".into(),
            ("http://gpu-box:11434/v1/".into(), "OLLAMA_API_KEY".into()),
        );
        assert_eq!(ollama_base(&cfg), "http://gpu-box:11434");
        cfg.llm_providers_map
            .insert("ollama".into(), ("  ".into(), String::new()));
        assert_eq!(ollama_base(&cfg), "http://127.0.0.1:11434");
    }

    #[test]
    fn local_model_preference() {
        let models = vec!["qwen3:8b".to_string(), "gemma4:latest".to_string()];
        assert_eq!(preferred_local_model(&models), "gemma4:latest");
        assert_eq!(preferred_local_model(&["qwen3:8b".to_string()]), "qwen3:8b");
    }
}
