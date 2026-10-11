//! Surreal-backed persistence for [`crate::state::Director`].
//!
//! Directors are first-class editable rows: each one carries its own prompt,
//! model selection (the "muscle"), tool allowlist, and rules (global +
//! per-entity overlays). The legacy `agents_seed.json` / `director_schemas.rs`
//! pipeline becomes a *recommendation* layer that suggests which row in this
//! table best fits a request — it is no longer source of truth.
//!
//! ### SurrealDB v2 record-id handling
//!
//! Same round-trip dance as [`crate::entity_store`] / [`crate::mission_store`]:
//! Surreal stores the row id as a `Thing`/`RecordId`, so we read into a
//! `DirectorRow` (RecordId + flattened rest) and write through a
//! `DirectorUpsertBody` that intentionally omits `id`. The
//! `director_field_completeness_guard` below pins the projection so a future
//! `Director` field can't be silently dropped from persistence.
//!
//! ### Soft delete
//!
//! `soft_delete` clears `is_active` rather than removing the row. This keeps
//! existing references (lessons, mission assignments) intact while hiding the
//! director from default queries. Hard removal is intentionally not exposed
//! here — when we need it, it should go through a dedicated cleanup pass that
//! also reaps lessons.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::director_lessons_store::render_lessons_block;
use crate::state::{Director, DirectorLesson};

/// Handle to the `directors` table. Cheap to clone via `Arc`; holds the
/// shared Surreal connection, not a dedicated one.
pub struct SurrealDirectorStore {
    db: Arc<Surreal<Any>>,
}

// Read-side shape: Surreal returns the id as a `RecordId`/`Thing` separate
// from the rest of the fields, so we capture it here and re-flatten in
// `row_to_director` rather than fighting serde over a mixed id type.
#[derive(Deserialize)]
struct DirectorRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// Strip the `directors:` table prefix (and any backtick-escaping Surreal
// adds for ids containing special characters) to get back the plain id.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

// Reassemble a `DirectorRow` into a `Director` by folding the resolved id
// back into the flattened field map before deserializing.
fn row_to_director(row: DirectorRow) -> Result<Director, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<Director>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

// Write-side shape: mirrors `Director` minus `id` (Surreal supplies the id
// via the `.upsert(("directors", id))` call itself).
#[derive(serde::Serialize)]
struct DirectorUpsertBody {
    slug: String,
    name: String,
    kind: String,
    description: String,
    role: String,
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_id: Option<String>,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    tool_allowlist: Vec<String>,
    rules_global: String,
    rules_per_entity: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_heartbeat: Option<String>,
    version: u32,
    is_active: bool,
    created_at: String,
    updated_at: String,
}

// Field-by-field copy rather than a partial move so the compiler flags any
// future `Director` field that isn't also threaded through here.
fn director_to_upsert_body(d: &Director) -> DirectorUpsertBody {
    DirectorUpsertBody {
        slug: d.slug.clone(),
        name: d.name.clone(),
        kind: d.kind.clone(),
        description: d.description.clone(),
        role: d.role.clone(),
        scope: d.scope.clone(),
        workspace_id: d.workspace_id.clone(),
        group_id: d.group_id.clone(),
        prompt: d.prompt.clone(),
        model_provider: d.model_provider.clone(),
        model_name: d.model_name.clone(),
        temperature: d.temperature,
        tool_allowlist: d.tool_allowlist.clone(),
        rules_global: d.rules_global.clone(),
        rules_per_entity: d.rules_per_entity.clone(),
        default_heartbeat: d.default_heartbeat.clone(),
        version: d.version,
        is_active: d.is_active,
        created_at: d.created_at.clone(),
        updated_at: d.updated_at.clone(),
    }
}

/// Never called; exists purely so an exhaustive destructure of `Director`
/// fails to compile if a field is added without updating the upsert body.
#[allow(dead_code)]
fn director_field_completeness_guard(d: &Director) {
    let Director {
        id: _,
        slug: _,
        name: _,
        kind: _,
        description: _,
        role: _,
        scope: _,
        workspace_id: _,
        group_id: _,
        prompt: _,
        model_provider: _,
        model_name: _,
        temperature: _,
        tool_allowlist: _,
        rules_global: _,
        rules_per_entity: _,
        default_heartbeat: _,
        version: _,
        is_active: _,
        created_at: _,
        updated_at: _,
    } = d;
}

impl SurrealDirectorStore {
    /// Wrap an existing Surreal connection; does not touch the schema.
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Define the `directors` table and its indexes. Safe to call more than
    /// once — `DEFINE` statements are idempotent.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE directors SCHEMALESS;
                DEFINE FIELD slug ON directors TYPE string;
                DEFINE FIELD workspace_id ON directors TYPE option<string>;
                DEFINE FIELD is_active ON directors TYPE bool;
                DEFINE INDEX directors_workspace_idx ON directors FIELDS workspace_id;
                DEFINE INDEX directors_slug_idx ON directors FIELDS slug;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealDirectorStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Alias for [`Self::upsert`] — the table is `SCHEMALESS` so create and
    /// update share the same write path.
    pub async fn create(&self, director: &Director) -> Result<()> {
        self.upsert(director).await
    }

    /// Write (insert or replace) a director row keyed by `director.id`.
    pub async fn upsert(&self, director: &Director) -> Result<()> {
        let body = director_to_upsert_body(director);
        let _: Option<DirectorRow> = self
            .db
            .upsert(("directors", director.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert director {}: {}", director.id, e))?;
        Ok(())
    }

    /// Fetch a director by id. A malformed row is logged and treated as
    /// absent rather than failing the whole call.
    pub async fn get(&self, id: &str) -> Result<Option<Director>> {
        let row: Option<DirectorRow> = self
            .db
            .select(("directors", id))
            .await
            .map_err(|e| anyhow::anyhow!("select director {}: {}", id, e))?;
        Ok(row.and_then(|r| match row_to_director(r) {
            Ok(d) => Some(d),
            Err((rid, err)) => {
                tracing::warn!(director_id = %rid, error = %err, "SurrealDirectorStore: dropping malformed row");
                None
            }
        }))
    }

    /// Fetch the first director with the given human-facing `slug`.
    pub async fn get_by_slug(&self, slug: &str) -> Result<Option<Director>> {
        let s = slug.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM directors WHERE slug = $slug LIMIT 1")
            .bind(("slug", s))
            .await
            .map_err(|e| anyhow::anyhow!("query director by slug: {}", e))?;
        let rows: Vec<DirectorRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take director by slug: {}", e))?;
        Ok(rows.into_iter().next().and_then(|r| match row_to_director(r) {
            Ok(d) => Some(d),
            Err((rid, err)) => {
                tracing::warn!(director_id = %rid, error = %err, "SurrealDirectorStore: dropping malformed row");
                None
            }
        }))
    }

    /// List every director row, including inactive (soft-deleted) ones.
    pub async fn list_all(&self) -> Result<Vec<Director>> {
        let rows: Vec<DirectorRow> = self
            .db
            .select("directors")
            .await
            .map_err(|e| anyhow::anyhow!("select directors: {}", e))?;
        Ok(rows_to_directors(rows))
    }

    /// List directors scoped to a given workspace.
    pub async fn list_by_workspace(&self, workspace_id: &str) -> Result<Vec<Director>> {
        let wid = workspace_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM directors WHERE workspace_id = $wid")
            .bind(("wid", wid))
            .await
            .map_err(|e| anyhow::anyhow!("query directors by workspace: {}", e))?;
        let rows: Vec<DirectorRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take directors: {}", e))?;
        Ok(rows_to_directors(rows))
    }

    /// Alias for [`Self::upsert`]; see its comment for why update and create
    /// share one code path.
    pub async fn update(&self, director: &Director) -> Result<()> {
        // Same path as upsert: SCHEMALESS table + .content() replaces the row.
        self.upsert(director).await
    }

    /// Hide a director without removing the row. Preserves lesson + mission
    /// references. Caller is responsible for bumping `updated_at`.
    pub async fn soft_delete(&self, id: &str) -> Result<()> {
        if let Some(mut d) = self.get(id).await? {
            d.is_active = false;
            self.upsert(&d).await?;
        }
        Ok(())
    }

    /// Assemble the director-flavoured system prompt block for an outbound
    /// LLM turn. Order: `prompt` → `rules_global` → `rules_per_entity[entity_id]`
    /// → recalled lessons.
    ///
    /// Lessons must be pre-recalled (via [`crate::director_lessons_store::SurrealDirectorLessonsStore::recall_lessons`])
    /// so this function stays sync and easy to unit-test. The Phase 6 prompt
    /// assembly pipeline is the one that wires the two stores together.
    pub fn assemble_prompt(
        &self,
        director: &Director,
        entity_id: Option<&str>,
        lessons: &[DirectorLesson],
    ) -> String {
        assemble_director_block(director, entity_id, lessons)
    }
}

/// Free-function form so the prompt-assembly pipeline can compose a director
/// block without holding a `SurrealDirectorStore`. Mirrors
/// [`SurrealDirectorStore::assemble_prompt`].
pub fn assemble_director_block(
    director: &Director,
    entity_id: Option<&str>,
    lessons: &[DirectorLesson],
) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !director.prompt.trim().is_empty() {
        sections.push(director.prompt.trim().to_string());
    }
    if !director.rules_global.trim().is_empty() {
        sections.push(format!(
            "## Rules (global)\n{}",
            director.rules_global.trim()
        ));
    }
    if let Some(eid) = entity_id
        && let Some(rules) = director.rules_per_entity.get(eid)
        && !rules.trim().is_empty()
    {
        sections.push(format!("## Rules (this entity)\n{}", rules.trim()));
    }
    if let Some(block) = render_lessons_block(lessons) {
        sections.push(block);
    }
    sections.join("\n\n")
}

// Best-effort batch conversion: a malformed row is logged and dropped
// instead of failing the whole listing.
fn rows_to_directors(rows: Vec<DirectorRow>) -> Vec<Director> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row_to_director(row) {
            Ok(d) => out.push(d),
            Err((id, err)) => {
                tracing::warn!(
                    director_id = %id,
                    error = %err,
                    "SurrealDirectorStore: dropping malformed row"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_director(id: &str, slug: &str) -> Director {
        Director {
            id: id.to_string(),
            slug: slug.to_string(),
            name: "Operations".to_string(),
            kind: "director".to_string(),
            description: "Runs the project".to_string(),
            role: "operations".to_string(),
            scope: "global".to_string(),
            workspace_id: Some("ws_default".to_string()),
            group_id: Some("operations".to_string()),
            prompt: "You are operations.".to_string(),
            model_provider: Some("anthropic".to_string()),
            model_name: Some("claude-sonnet-4-6".to_string()),
            temperature: Some(0.4),
            tool_allowlist: vec!["search".to_string(), "memory".to_string()],
            rules_global: "Be terse.".to_string(),
            rules_per_entity: {
                let mut m = HashMap::new();
                m.insert("ent_qsme".to_string(), "Match QS Me tone.".to_string());
                m
            },
            default_heartbeat: Some("30m".to_string()),
            version: 1,
            is_active: true,
            created_at: "2026-05-24T03:00:00Z".to_string(),
            updated_at: "2026-05-24T03:00:00Z".to_string(),
        }
    }

    #[test]
    fn upsert_body_strips_id_field() {
        let d = sample_director("dir_1", "operations");
        let body = director_to_upsert_body(&d);
        let serialised = serde_json::to_value(&body).expect("serialise");
        let obj = serialised.as_object().expect("object");
        assert!(
            !obj.contains_key("id"),
            "id must be stripped from upsert body"
        );
        assert_eq!(obj.get("slug").and_then(|v| v.as_str()), Some("operations"));
        assert_eq!(
            obj.get("tool_allowlist")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn end_to_end_in_memory_surreal() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorStore::new(Arc::new(db));
        store.init_schema().await.expect("init");
        // init_schema must be idempotent.
        store.init_schema().await.expect("init twice");

        let d1 = sample_director("dir_a", "operations");
        let d2 = sample_director("dir_b", "full-stack");
        store.upsert(&d1).await.expect("upsert d1");
        store.upsert(&d2).await.expect("upsert d2");

        let all = store.list_all().await.expect("list_all");
        assert_eq!(all.len(), 2);

        let by_ws = store.list_by_workspace("ws_default").await.expect("by ws");
        assert_eq!(by_ws.len(), 2);

        let by_slug = store
            .get_by_slug("full-stack")
            .await
            .expect("by slug")
            .expect("present");
        assert_eq!(by_slug.id, "dir_b");

        store.soft_delete("dir_a").await.expect("soft delete");
        let after = store.get("dir_a").await.expect("get").expect("present");
        assert!(!after.is_active, "soft delete clears is_active");
        // Row still present, just inactive.
        let still_all = store.list_all().await.expect("list_all 2");
        assert_eq!(still_all.len(), 2);

        let assembled = store.assemble_prompt(&d1, Some("ent_qsme"), &[]);
        assert!(assembled.contains("You are operations."));
        assert!(assembled.contains("## Rules (global)\nBe terse."));
        assert!(assembled.contains("## Rules (this entity)\nMatch QS Me tone."));
        let idx_prompt = assembled.find("You are operations.").unwrap();
        let idx_global = assembled.find("## Rules (global)").unwrap();
        let idx_entity = assembled.find("## Rules (this entity)").unwrap();
        assert!(idx_prompt < idx_global);
        assert!(idx_global < idx_entity);

        // No entity selected => entity-rules section is omitted.
        let assembled_no_entity = store.assemble_prompt(&d1, None, &[]);
        assert!(!assembled_no_entity.contains("## Rules (this entity)"));

        // With lessons spliced in: appear after the entity-rules block.
        let lesson = DirectorLesson {
            id: "les_x".to_string(),
            director_id: "dir_a".to_string(),
            scope: "global".to_string(),
            lesson: "always check git status".to_string(),
            source: "correction".to_string(),
            severity: "hard".to_string(),
            embedding: None,
            created_at: "2026-05-24T00:00:00Z".to_string(),
            created_by: None,
        };
        let with_lessons = store.assemble_prompt(&d1, Some("ent_qsme"), &[lesson]);
        let idx_lessons = with_lessons
            .find("## Lessons learned")
            .expect("lessons block");
        assert!(idx_lessons > with_lessons.find("## Rules (this entity)").unwrap());
        assert!(with_lessons.contains("•! always check git status"));
    }
}
