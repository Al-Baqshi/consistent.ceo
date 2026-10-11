//! One-shot loader that converts `agents_seed.json` rows into [`Director`]
//! records and inserts the ones whose slug is **not already present**.
//!
//! Behaviour contract (locked by [`tests`] below):
//! - First run on a clean DB: every agent in the seed file is inserted.
//! - Subsequent runs: nothing is touched, regardless of how the user edited
//!   the existing rows. The seed is a *floor*, not a source of truth.
//! - Unknown / missing seed file is logged and treated as a no-op so a stripped
//!   deployment still boots.
//!
//! The mapping from the legacy `consistent-world-py` agent shape to the canonical
//! [`Director`] struct lives in [`SeedAgent::into_director`] — keep it in sync
//! with the seed file rather than embedding alternative defaults elsewhere.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::director_store::SurrealDirectorStore;
use crate::state::Director;

// Top-level shape of `agents_seed.json`; only the `agents` array matters here.
#[derive(Deserialize)]
struct SeedFile {
    #[serde(default)]
    agents: Vec<SeedAgent>,
}

// One row of the legacy `consistent-world-py` agent shape, as it appears in
// the seed JSON (camelCase keys) before being mapped to a `Director`.
#[derive(Deserialize)]
struct SeedAgent {
    id: String,
    #[serde(default, rename = "groupId")]
    group_id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    kind: String,
    #[serde(default, rename = "preferredProvider")]
    preferred_provider: Option<String>,
    #[serde(default, rename = "preferredModel")]
    preferred_model: Option<String>,
    #[serde(default)]
    mission: String,
    #[serde(default)]
    profile: String,
}

impl SeedAgent {
    // The legacy-shape → canonical-shape mapping described in the module
    // header. Fields with no seed equivalent (temperature, tool_allowlist,
    // rules, default_heartbeat) get empty/default values.
    fn into_director(self, now: &str) -> Director {
        Director {
            id: self.id.clone(),
            slug: self.id,
            name: self.name,
            kind: self.kind,
            description: self.mission,
            role: self.role,
            scope: "global".to_string(),
            workspace_id: None,
            group_id: self.group_id,
            prompt: self.profile,
            model_provider: self.preferred_provider,
            model_name: self.preferred_model,
            temperature: None,
            tool_allowlist: Vec::new(),
            rules_global: String::new(),
            rules_per_entity: std::collections::HashMap::new(),
            default_heartbeat: None,
            version: 1,
            is_active: true,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        }
    }
}

// RFC 3339 timestamp used for both `created_at` and `updated_at` on seeded
// directors.
fn current_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Parse `agents_seed.json` and return the implied [`Director`] rows.
/// Exposed for tests; the boot path uses [`seed_directors_if_absent`].
pub fn parse_seed(json: &str) -> Result<Vec<Director>> {
    let file: SeedFile = serde_json::from_str(json).context("parse agents_seed.json")?;
    let now = current_timestamp();
    Ok(file
        .agents
        .into_iter()
        .map(|a| a.into_director(&now))
        .collect())
}

/// The repo's canonical seed, baked into the binary so containers that don't
/// ship the JSON file (e.g. the docker image) still get their directors.
/// A file at the seed path always wins so operators can override.
pub(crate) const EMBEDDED_SEED: &str = include_str!("../../../agents_seed.json");

/// Read the seed file at `path` (falling back to the embedded copy) and
/// upsert every director whose slug is not yet present. Logs and skips on
/// parse errors so startup doesn't fail just because the seed file moved.
pub async fn seed_directors_if_absent(store: &SurrealDirectorStore, path: &Path) -> Result<usize> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(seed_path = %path.display(), error = %e, "director seed file unreadable; using embedded seed");
            EMBEDDED_SEED.to_string()
        }
    };
    let directors = match parse_seed(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(seed_path = %path.display(), error = %e, "director seed file malformed; skipping");
            return Ok(0);
        }
    };
    let mut inserted = 0usize;
    for d in directors {
        match store.get_by_slug(&d.slug).await {
            Ok(Some(_)) => continue,
            Ok(None) => {
                store
                    .upsert(&d)
                    .await
                    .with_context(|| format!("seed director slug={}", d.slug))?;
                inserted += 1;
            }
            Err(e) => {
                tracing::warn!(slug = %d.slug, error = %e, "lookup before seed failed; skipping director");
            }
        }
    }
    if inserted > 0 {
        tracing::info!(inserted, "seeded directors from agents_seed.json");
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SAMPLE: &str = r##"{
        "version": 1,
        "agents": [
            {
                "id": "ops-coordinator",
                "groupId": "operations",
                "name": "Ops Coordinator",
                "role": "Keeps systems organized.",
                "kind": "operations",
                "preferredProvider": "openai",
                "preferredModel": "brain-default",
                "mission": "Keep work flowing.",
                "profile": "# Ops Coordinator\n\nDo the thing."
            },
            {
                "id": "frontend-engineer",
                "groupId": "full-stack-development",
                "name": "Frontend Engineer",
                "role": "Builds UI.",
                "kind": "coding",
                "preferredProvider": "openai",
                "preferredModel": "brain-default",
                "mission": "Operator clarity.",
                "profile": "# Frontend Engineer"
            }
        ]
    }"##;

    #[test]
    fn parse_seed_maps_fields() {
        let directors = parse_seed(SAMPLE).expect("parse");
        assert_eq!(directors.len(), 2);
        let ops = directors
            .iter()
            .find(|d| d.slug == "ops-coordinator")
            .unwrap();
        assert_eq!(ops.id, "ops-coordinator");
        assert_eq!(ops.name, "Ops Coordinator");
        assert_eq!(ops.group_id.as_deref(), Some("operations"));
        assert_eq!(ops.model_provider.as_deref(), Some("openai"));
        assert_eq!(ops.model_name.as_deref(), Some("brain-default"));
        assert_eq!(ops.description, "Keep work flowing.");
        assert!(ops.prompt.contains("# Ops Coordinator"));
        assert_eq!(ops.scope, "global");
        assert!(ops.is_active);
        assert_eq!(ops.version, 1);
    }

    #[tokio::test]
    async fn seed_is_idempotent_and_preserves_edits() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        std::fs::write(tmp.path(), SAMPLE).expect("write seed");

        let n1 = seed_directors_if_absent(&store, tmp.path())
            .await
            .expect("seed 1");
        assert_eq!(n1, 2, "fresh DB inserts every seed row");

        // Simulate a user edit on a seeded row.
        let mut edited = store.get_by_slug("ops-coordinator").await.unwrap().unwrap();
        edited.prompt = "EDITED BY USER".to_string();
        edited.version = 2;
        store.upsert(&edited).await.unwrap();

        // Second run inserts nothing and leaves edits alone.
        let n2 = seed_directors_if_absent(&store, tmp.path())
            .await
            .expect("seed 2");
        assert_eq!(n2, 0, "second run is a no-op");
        let after = store.get_by_slug("ops-coordinator").await.unwrap().unwrap();
        assert_eq!(after.prompt, "EDITED BY USER");
        assert_eq!(after.version, 2);

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn missing_file_falls_back_to_embedded_seed() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let bogus =
            std::path::Path::new("/tmp/__definitely_does_not_exist_consistent_world__.json");
        let n = seed_directors_if_absent(&store, bogus)
            .await
            .expect("embedded seed");
        assert!(n > 0, "container images without the file still seed");
        let all = store.list_all().await.unwrap();
        assert!(
            all.iter().any(|d| d.slug == "ops-coordinator"),
            "ops-coordinator director present from embedded seed"
        );
        // Idempotent: second boot inserts nothing.
        let again = seed_directors_if_absent(&store, bogus).await.expect("noop");
        assert_eq!(again, 0);
    }
}
