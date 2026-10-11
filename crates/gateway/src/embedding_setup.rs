//! Which embedding provider backs semantic recall, and keeping the stored
//! vectors in one vector space.
//!
//! # Choosing (`[memory] embedding_provider`)
//!
//! - `auto` (default): a local Ollama model first, then the hosted
//!   candidates the gateway always tried (an OpenAI-compatible default base,
//!   an `[llm.providers.openai]` entry, Gemini). A fresh install with only
//!   Ollama gets semantic recall with no key and no network.
//! - `ollama`, `openai`, `gemini`: only that family.
//! - `off`: no semantic recall; every store falls back to recency.
//!
//! When Ollama is running but the model (`[memory] embedding_model`, default
//! `nomic-embed-text`) is missing, the gateway starts the download in the
//! background and uses the model once it lands. Until then embeds fail and
//! callers fall back to recency; the lesson backfill sweep fills the gaps.
//!
//! Hosted candidates are never contacted under `--offline`.
//!
//! # One vector space per database
//!
//! Every HNSW index is 1536 wide, and a local model is fitted to that width
//! (see `consistent_world_memory::embedding::fit_to_index_width`). Width is
//! therefore never the problem. Mixing *models* is: a query embedded by one
//! model against vectors from another ranks rows at random. So the database
//! records the identity of the provider that wrote its vectors
//! (`system_meta:embedding`), and:
//!
//! - **Sticky.** In `auto` mode the recorded provider is tried first. A
//!   machine that was using OpenAI keeps using OpenAI after Ollama appears.
//! - **Never switch by accident.** If the recorded provider is still
//!   configured but fails its probe (offline, outage), semantic recall is off
//!   *for this run* and the vectors are kept. Switching on a blip would throw
//!   them away.
//! - **Switch cleanly.** When the recorded provider is gone (its key removed)
//!   or the operator named a different one, the old vectors are cleared from
//!   every vector table so nothing compares across spaces. Lessons are
//!   re-embedded by their sweep; older chat memories stay readable by recency.
//! - A database with no record adopts the chosen provider without clearing:
//!   its vectors were written by whatever ran before, which is the best guess.

use std::sync::Arc;
use std::time::Duration;

use consistent_world_memory::embedding::{
    ApiEmbedding, EmbeddingProvider, GeminiEmbedding, OllamaEmbedding,
};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::credentials_resolver;

/// Tables that hold `embedding` vectors behind an HNSW index.
const VECTOR_TABLES: &[&str] = &["memories", "director_lessons", "entity_docs"];

/// Default Ollama root when no `[llm.providers.ollama]` entry says otherwise.
const DEFAULT_OLLAMA_BASE: &str = "http://127.0.0.1:11434";

const LOCAL_PROBE: Duration = Duration::from_secs(8);
const HOSTED_PROBE: Duration = Duration::from_secs(5);

/// One provider the gateway could use.
pub struct Candidate {
    pub label: String,
    pub provider: Arc<dyn EmbeddingProvider>,
    /// Sends text off this machine.
    pub hosted: bool,
    /// Ollama has no copy of the model yet; a download is running.
    pub pending: bool,
}

/// The outcome of [`choose`].
pub enum Choice {
    Use(Candidate),
    /// The recorded provider is configured but did not answer. Recall is off
    /// for this run and the stored vectors are kept.
    KeepVectorsDisabled {
        recorded: String,
    },
    None,
}

/// What `[memory] embedding_provider` allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Auto,
    Ollama,
    OpenAi,
    Gemini,
    Off,
}

fn mode(cfg: &AppConfig) -> Mode {
    match cfg.embedding_provider.trim().to_ascii_lowercase().as_str() {
        "ollama" | "local" => Mode::Ollama,
        "openai" => Mode::OpenAi,
        "gemini" | "google" => Mode::Gemini,
        "off" | "none" | "disabled" => Mode::Off,
        _ => Mode::Auto,
    }
}

/// Ollama's root from `[llm.providers.ollama]`, else the default port.
fn ollama_base(cfg: &AppConfig) -> String {
    cfg.llm_providers_map
        .get("ollama")
        .map(|(base, _)| base.clone())
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE.to_string())
}

/// Build every candidate the config allows, local first. Contacts only the
/// local Ollama (one `GET /api/tags`); hosted providers are not called here.
pub async fn candidates(cfg: &AppConfig) -> Vec<Candidate> {
    let mode = mode(cfg);
    let mut out = Vec::new();
    if mode == Mode::Off {
        return out;
    }

    if matches!(mode, Mode::Auto | Mode::Ollama) {
        let ollama = OllamaEmbedding::new(&ollama_base(cfg), &cfg.embedding_model);
        match ollama.has_model().await {
            Ok(true) => out.push(Candidate {
                label: format!("ollama ({})", cfg.embedding_model),
                provider: Arc::new(ollama),
                hosted: false,
                pending: false,
            }),
            Ok(false) => {
                info!(
                    model = %cfg.embedding_model,
                    "Ollama is running without the embedding model; downloading it in the background"
                );
                let puller = OllamaEmbedding::new(&ollama_base(cfg), &cfg.embedding_model);
                tokio::spawn(async move {
                    match puller.pull().await {
                        Ok(()) => {
                            info!(model = %puller.model, "embedding model downloaded; semantic recall is live")
                        }
                        Err(e) => {
                            warn!(error = %e, "embedding model download failed; semantic recall stays off")
                        }
                    }
                });
                out.push(Candidate {
                    label: format!("ollama ({}, downloading)", cfg.embedding_model),
                    provider: Arc::new(ollama),
                    hosted: false,
                    pending: true,
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "no local Ollama for embeddings");
            }
        }
    }

    // The default LLM provider may have no embeddings endpoint (Anthropic,
    // Google chat base, Ollama's OpenAI shim) — blindly reusing
    // cfg.llm_base_url there yields 404s on every embed.
    if matches!(mode, Mode::Auto | Mode::OpenAi) {
        let default_key = credentials_resolver::resolve_llm_api_key(&cfg.llm_api_key_env);
        let default_base = cfg.llm_base_url.to_ascii_lowercase();
        let default_is_openai_compatible = !default_base.contains("api.anthropic.com")
            && !default_base.contains("generativelanguage.googleapis.com")
            && !default_base.contains(":11434")
            && !default_base.contains("ollama");
        if default_is_openai_compatible && usable_key(&default_key) {
            out.push(Candidate {
                label: "default LLM base".into(),
                provider: Arc::new(ApiEmbedding::new(
                    default_key,
                    Some(cfg.llm_base_url.clone()),
                )),
                hosted: true,
                pending: false,
            });
        }
        let openai_entry = cfg
            .llm_providers_map
            .get("openai")
            .map(|(base, key_env)| {
                (
                    base.clone(),
                    credentials_resolver::resolve_llm_api_key(key_env),
                )
            })
            .filter(|(_, key)| usable_key(key));
        if let Some((base, key)) = openai_entry
            && (!default_is_openai_compatible || base.to_ascii_lowercase() != default_base)
        {
            out.push(Candidate {
                label: "openai provider entry".into(),
                provider: Arc::new(ApiEmbedding::new(key, Some(base))),
                hosted: true,
                pending: false,
            });
        }
    }
    if matches!(mode, Mode::Auto | Mode::Gemini) {
        let gemini_key = credentials_resolver::resolve_llm_api_key("GEMINI_API_KEY");
        if usable_key(&gemini_key) {
            out.push(Candidate {
                label: "gemini embedContent".into(),
                provider: Arc::new(GeminiEmbedding::new(gemini_key, None)),
                hosted: true,
                pending: false,
            });
        }
    }
    out
}

fn usable_key(key: &str) -> bool {
    !key.is_empty() && key != "dummy"
}

/// Probe one candidate with a tiny embed. A pending download counts as
/// usable (same vector space, just not ready); a hosted one under
/// `--offline` counts as failed without being contacted.
async fn probe(c: &Candidate, offline: bool) -> bool {
    if c.pending {
        return true;
    }
    if c.hosted && offline {
        return false;
    }
    let budget = if c.hosted { HOSTED_PROBE } else { LOCAL_PROBE };
    match tokio::time::timeout(budget, c.provider.embed("startup probe")).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            warn!(candidate = %c.label, error = %e, "embedding candidate failed probe");
            false
        }
        Err(_) => {
            warn!(candidate = %c.label, "embedding candidate probe timed out");
            false
        }
    }
}

/// Pick a provider. `recorded` is the identity stored with the vectors;
/// `explicit` is true when the operator named a provider family.
pub async fn choose(
    mut cands: Vec<Candidate>,
    recorded: Option<&str>,
    explicit: bool,
    offline: bool,
) -> Choice {
    // Sticky: the provider that wrote the stored vectors goes first, and if it
    // is still configured but not answering, recall waits for it.
    if !explicit
        && let Some(recorded) = recorded
        && let Some(i) = cands.iter().position(|c| c.provider.identity() == recorded)
    {
        let c = cands.swap_remove(i);
        if probe(&c, offline).await {
            return Choice::Use(c);
        }
        return Choice::KeepVectorsDisabled {
            recorded: recorded.to_string(),
        };
    }

    let mut pending = None;
    for c in cands {
        if c.pending {
            pending.get_or_insert(c);
            continue;
        }
        if probe(&c, offline).await {
            return Choice::Use(c);
        }
    }
    pending.map(Choice::Use).unwrap_or(Choice::None)
}

/// The identity recorded with this database's vectors, if any.
pub async fn recorded_identity(db: &Surreal<Any>) -> Option<String> {
    let mut res = db
        .query("SELECT VALUE identity FROM system_meta:embedding")
        .await
        .ok()?;
    let rows: Vec<Option<String>> = res.take(0).ok()?;
    rows.into_iter().flatten().next()
}

/// Record `identity` as the vector space of this database. Clears every
/// stored vector first when a different identity was recorded. Returns true
/// when vectors were cleared.
pub async fn adopt_identity(db: &Surreal<Any>, identity: &str) -> anyhow::Result<bool> {
    let previous = recorded_identity(db).await;
    let cleared = match previous.as_deref() {
        Some(prev) if prev != identity => {
            for table in VECTOR_TABLES {
                db.query(format!(
                    "UPDATE {table} SET embedding = NONE WHERE embedding != NONE"
                ))
                .await?
                .check()?;
            }
            warn!(
                from = prev,
                to = identity,
                "embedding provider changed; cleared stored vectors so no query compares across models"
            );
            true
        }
        _ => false,
    };
    db.query("UPSERT system_meta:embedding SET identity = $identity, updated_at = time::now()")
        .bind(("identity", identity.to_string()))
        .await?
        .check()?;
    Ok(cleared)
}

/// The whole startup step: build candidates, honour the recorded identity,
/// probe, record the winner. Returns the provider to install, if any.
pub async fn select(
    cfg: &AppConfig,
    db: Option<&Arc<Surreal<Any>>>,
) -> Option<(String, Arc<dyn EmbeddingProvider>)> {
    let explicit = !matches!(mode(cfg), Mode::Auto);
    let offline = crate::config::offline_mode();
    let cands = candidates(cfg).await;
    let recorded = match db {
        Some(db) => recorded_identity(db).await,
        None => None,
    };
    match choose(cands, recorded.as_deref(), explicit, offline).await {
        Choice::Use(c) => {
            if let Some(db) = db
                && let Err(e) = adopt_identity(db, &c.provider.identity()).await
            {
                warn!(error = %e, "could not record the embedding provider; recall still works");
            }
            Some((c.label, c.provider))
        }
        Choice::KeepVectorsDisabled { recorded } => {
            warn!(
                provider = %recorded,
                "the embedding provider that wrote this database's vectors did not answer; \
                 semantic recall is off for this run and the vectors are kept"
            );
            None
        }
        Choice::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Fake {
        id: &'static str,
        ok: bool,
    }

    #[async_trait]
    impl EmbeddingProvider for Fake {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            if self.ok {
                Ok(vec![0.0; 1536])
            } else {
                anyhow::bail!("down")
            }
        }
        fn dimensions(&self) -> usize {
            1536
        }
        fn identity(&self) -> String {
            self.id.to_string()
        }
    }

    fn cand(id: &'static str, ok: bool, hosted: bool, pending: bool) -> Candidate {
        Candidate {
            label: id.to_string(),
            provider: Arc::new(Fake { id, ok }),
            hosted,
            pending,
        }
    }

    fn used(choice: Choice) -> Option<String> {
        match choice {
            Choice::Use(c) => Some(c.provider.identity()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn fresh_install_prefers_local() {
        let c = vec![
            cand("ollama:nomic", true, false, false),
            cand("openai", true, true, false),
        ];
        assert_eq!(
            used(choose(c, None, false, false).await).as_deref(),
            Some("ollama:nomic")
        );
    }

    #[tokio::test]
    async fn recorded_provider_stays_first() {
        let c = vec![
            cand("ollama:nomic", true, false, false),
            cand("openai", true, true, false),
        ];
        assert_eq!(
            used(choose(c, Some("openai"), false, false).await).as_deref(),
            Some("openai")
        );
    }

    #[tokio::test]
    async fn a_blip_never_switches_spaces() {
        // Recorded OpenAI, started offline: keep the vectors, no recall this run.
        let c = vec![
            cand("ollama:nomic", true, false, false),
            cand("openai", true, true, false),
        ];
        assert!(matches!(
            choose(c, Some("openai"), false, true).await,
            Choice::KeepVectorsDisabled { .. }
        ));
    }

    #[tokio::test]
    async fn removed_provider_lets_the_next_one_take_over() {
        let c = vec![cand("ollama:nomic", true, false, false)];
        assert_eq!(
            used(choose(c, Some("openai"), false, false).await).as_deref(),
            Some("ollama:nomic")
        );
    }

    #[tokio::test]
    async fn explicit_choice_overrides_the_record() {
        let c = vec![cand("ollama:nomic", true, false, false)];
        assert_eq!(
            used(choose(c, Some("openai"), true, false).await).as_deref(),
            Some("ollama:nomic")
        );
    }

    #[tokio::test]
    async fn offline_never_contacts_hosted_and_a_download_is_a_fallback() {
        let c = vec![
            cand("openai", true, true, false),
            cand("ollama:nomic", false, false, true),
        ];
        assert_eq!(
            used(choose(c, None, false, true).await).as_deref(),
            Some("ollama:nomic")
        );
        let c = vec![cand("openai", true, true, false)];
        assert!(matches!(choose(c, None, false, true).await, Choice::None));
    }

    #[tokio::test]
    async fn a_working_provider_beats_a_download() {
        let c = vec![
            cand("ollama:nomic", false, false, true),
            cand("openai", true, true, false),
        ];
        assert_eq!(
            used(choose(c, None, false, false).await).as_deref(),
            Some("openai")
        );
    }

    #[tokio::test]
    async fn changing_identity_clears_vectors_and_first_adoption_does_not() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        for table in VECTOR_TABLES {
            db.query(format!("CREATE {table}:one SET embedding = [0.1, 0.2]"))
                .await
                .expect("seed")
                .check()
                .expect("seed ok");
        }
        let count = |db: Surreal<Any>| async move {
            let mut total = 0usize;
            for table in VECTOR_TABLES {
                let mut r = db
                    .query(format!(
                        "SELECT VALUE id FROM {table} WHERE embedding != NONE"
                    ))
                    .await
                    .expect("count");
                let ids: Vec<surrealdb::RecordId> = r.take(0).expect("take");
                total += ids.len();
            }
            total
        };

        // No record yet: adopt without clearing.
        assert!(!adopt_identity(&db, "openai").await.unwrap());
        assert_eq!(recorded_identity(&db).await.as_deref(), Some("openai"));
        assert_eq!(count(db.clone()).await, VECTOR_TABLES.len());

        // Same identity: nothing happens.
        assert!(!adopt_identity(&db, "openai").await.unwrap());
        assert_eq!(count(db.clone()).await, VECTOR_TABLES.len());

        // New identity: every vector table is cleared, rows survive.
        assert!(adopt_identity(&db, "ollama:nomic").await.unwrap());
        assert_eq!(
            recorded_identity(&db).await.as_deref(),
            Some("ollama:nomic")
        );
        assert_eq!(count(db.clone()).await, 0);
        let mut r = db.query("SELECT VALUE id FROM memories").await.unwrap();
        let rows: Vec<surrealdb::RecordId> = r.take(0).unwrap();
        assert_eq!(rows.len(), 1);
    }
}
