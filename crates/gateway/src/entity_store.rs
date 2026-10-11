//! Surreal-backed persistence for [`crate::state::Entity`].
//!
//! The in-memory `ControlState::entities: RwLock<Vec<Entity>>` remains the live
//! working copy; this store is a periodic-sync persister so that a fresh
//! gateway process hydrates entities from SurrealDB rather than from a local
//! JSON snapshot file.
//!
//! Contract:
//!   - `list_all` is called once at startup. If it returns non-empty, it wins
//!     over whatever the JSON snapshot held.
//!   - `sync_all` is called after each periodic JSON persist (≤ every 60s).
//!     It upserts every entity. It does **not** delete rows just because the
//!     in-memory Vec lacks them; only ids passed to [`SurrealEntityStore::delete`]
//!     are ever removed (see "Deletes" below).
//!
//! ### Deletes are explicit (tombstones), never inferred
//!
//! `sync_all` used to prune every row whose id was missing from the Vec it was
//! handed. That turned three ordinary situations into permanent data loss:
//!
//!   1. hydration skips a row that fails to deserialize, so the next sync
//!      deleted the very row it could not read;
//!   2. if hydration failed outright, the gateway ran on the JSON snapshot and
//!      the next sync deleted every row newer than that snapshot;
//!   3. several callers sync concurrently with clones taken at different
//!      moments, so a stale clone deleted a row a fresher one had just written
//!      (and an overlapping redeploy's other instance can do the same).
//!
//! Now the delete path names what it deletes: [`SurrealEntityStore::delete`]
//! tombstones the id (a later sync carrying a stale copy will not resurrect it)
//! and removes the row. A bulk sync only retries tombstoned deletes that have
//! not landed yet, never touches a row it could not decode, and does no
//! deleting at all while the last hydration is known to have failed. Syncs and
//! deletes are serialized behind one async mutex per store. The same
//! [`SyncLedger`] backs the mission and task stores.
//!
//! ### SurrealDB v2 record-id handling
//!
//! Surreal v2 represents the row primary key as a `Thing`/`RecordId`, not a
//! plain string. Two consequences:
//!
//!   1. Reading `db.select("entities")` directly into `Vec<Entity>` fails
//!      with `expected string, found object` because `Entity.id: String` does
//!      not match the `Thing` shape Surreal returns.
//!   2. Writing `.content(entity.clone())` via upsert sends `id` as a string
//!      field in the body, which collides with the `Thing` Surreal sets from
//!      the path argument.
//!
//! We solve both by serialising via `serde_json::Value`: strip the `id`
//! field on writes, and rebuild it from the `RecordId` key on reads. This
//! keeps the `Entity` struct (`pub id: String`) untouched while staying
//! compatible with both JSON snapshot loads and Surreal hydration.
//!
//! Multi-tenancy: each row carries `workspace_id` (from `Entity.workspace_id`)
//! so future queries can be scoped. Phase 2c does not enforce scoping — it
//! only establishes persistence.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::{Arc, PoisonError};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::state::Entity;

/// What a store's bulk sync may write and delete: the bookkeeping that keeps a
/// periodic sync from destroying rows (see the module docs). Shared by the
/// entity, mission and task stores; one ledger per store instance, so every
/// caller must reach the store through the same `Arc` (they do: `main.rs`
/// builds one per table).
#[derive(Default)]
pub(crate) struct SyncLedger {
    /// Serializes a store's writes — bulk syncs, write-throughs, deletes — so a
    /// delete can never land between another writer's tombstone check and its
    /// upsert.
    write_lock: tokio::sync::Mutex<()>,
    /// Never held across an `await`.
    state: std::sync::Mutex<LedgerState>,
}

#[derive(Default)]
struct LedgerState {
    /// Ids deleted in this process. Upserts skip them for the life of the
    /// process, so a stale in-memory copy cannot write a deleted row back.
    tombstones: HashSet<String>,
    /// Tombstoned ids whose row delete has not succeeded yet.
    pending_deletes: HashSet<String>,
    /// Ids of rows that failed to deserialize when listed. Never deleted by a
    /// sync: an unreadable row is still someone's data.
    unreadable: HashSet<String>,
    /// The last full listing (the boot hydration) failed, so the in-memory
    /// copy came from a fallback and says nothing about what the table holds.
    hydration_failed: bool,
}

impl SyncLedger {
    fn state(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the store's write lock for the duration of one write (or one
    /// bulk sync).
    pub(crate) async fn serialize(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().await
    }

    /// Record the outcome of a full-table listing (the hydration call).
    pub(crate) fn record_full_listing(&self, ok: bool) {
        self.state().hydration_failed = !ok;
    }

    pub(crate) fn hydration_failed(&self) -> bool {
        self.state().hydration_failed
    }

    pub(crate) fn record_unreadable(&self, id: &str) {
        self.state().unreadable.insert(id.to_string());
    }

    pub(crate) fn is_tombstoned(&self, id: &str) -> bool {
        self.state().tombstones.contains(id)
    }

    /// Mark `id` deleted: upserts skip it from now on and its row delete is
    /// queued until one succeeds.
    pub(crate) fn tombstone(&self, id: &str) {
        let mut st = self.state();
        st.tombstones.insert(id.to_string());
        st.pending_deletes.insert(id.to_string());
    }

    /// The row delete for `id` succeeded.
    pub(crate) fn delete_landed(&self, id: &str) {
        self.state().pending_deletes.remove(id);
    }

    /// Tombstoned ids still waiting for a successful row delete — never one
    /// whose row could not be decoded.
    pub(crate) fn pending_deletes(&self) -> Vec<String> {
        let st = self.state();
        st.pending_deletes
            .iter()
            .filter(|id| !st.unreadable.contains(*id))
            .cloned()
            .collect()
    }
}

/// Handle to the `entities` table. Cheap to clone via the `Arc` it's always
/// wrapped in; every method opens its own query.
pub struct SurrealEntityStore {
    db: Arc<Surreal<Any>>,
    ledger: SyncLedger,
}

// Read-side shape: Surreal hands back the primary key as a `RecordId`
// (`Thing`), not the plain `String` `Entity.id` expects, so rows land here
// first and get converted by `row_to_entity`.
#[derive(Deserialize)]
struct EntityRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// Extract the key portion of a Surreal `RecordId`, e.g. `entities:ent_42` → `ent_42`.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

/// Convert a Surreal-shaped row into an `Entity` by re-injecting the record-id
/// key as the `id` string. Returns the original row id on parse error so the
/// caller can log and skip without panicking.
fn row_to_entity(row: EntityRow) -> Result<Entity, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<Entity>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

/// Owned, id-less projection of [`Entity`] used as the `.content(...)` body
/// for Surreal upserts.
///
/// We can't borrow from the source entity because the Surreal SDK requires
/// `T: Serialize + Send + 'static` on `.content`. Cloning is fine — the
/// persist path already clones the entire `Vec<Entity>` once per sync tick,
/// so a per-row clone here is in the noise.
///
/// We can't go through `serde_json::Value` either: the Surreal SDK
/// serialiser rejects `Value` because it's an externally-tagged enum, with
/// the misleading error `invalid type: enum, expected any valid JSON value`.
/// So we mirror Entity's shape manually and rely on the
/// `entity_field_completeness_guard` below to fail compilation if Entity
/// ever grows a new field that we'd silently drop here.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EntityUpsertBody {
    name: String,
    description: Option<String>,
    department: Option<String>,
    owner: Option<String>,
    status: String,
    #[serde(rename = "type")]
    entity_type: Option<String>,
    dept: String,
    file_count: u32,
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    visibility: crate::state::EntityVisibility,
    is_personal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    personal_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<crate::state::EntityFile>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<String>,
    domains: Vec<String>,
    markets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    goals: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operating_rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_pipeline: Option<String>,
    relationships: Vec<crate::state::Relationship>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enrichment: Option<crate::state::EntityEnrichment>,
    resources: Vec<crate::state::EntityResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_policies: Option<crate::state::ExecutionPolicies>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_workspace_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_model_tier: Option<String>,
    allowed_agent_groups: Vec<String>,
    setup: crate::state::EntitySetup,
    readiness: crate::state::EntityReadiness,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_runtime: Option<crate::state::EntityRuntimeConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    colour: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flow_diagrams: Option<crate::state::EntityFlowDiagrams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    knowledge_index: Option<crate::state::EntityKnowledgeIndex>,
}

fn entity_to_upsert_body(entity: &Entity) -> EntityUpsertBody {
    EntityUpsertBody {
        name: entity.name.clone(),
        description: entity.description.clone(),
        department: entity.department.clone(),
        owner: entity.owner.clone(),
        status: entity.status.clone(),
        entity_type: entity.entity_type.clone(),
        dept: entity.dept.clone(),
        file_count: entity.file_count,
        path: entity.path.clone(),
        workspace_id: entity.workspace_id.clone(),
        visibility: entity.visibility,
        is_personal: entity.is_personal,
        personal_user_id: entity.personal_user_id.clone(),
        local_path: entity.local_path.clone(),
        files: entity.files.clone(),
        stage: entity.stage.clone(),
        domains: entity.domains.clone(),
        markets: entity.markets.clone(),
        profile: entity.profile.clone(),
        goals: entity.goals.clone(),
        active_context: entity.active_context.clone(),
        operating_rules: entity.operating_rules.clone(),
        notes: entity.notes.clone(),
        content_pipeline: entity.content_pipeline.clone(),
        relationships: entity.relationships.clone(),
        enrichment: entity.enrichment.clone(),
        resources: entity.resources.clone(),
        execution_policies: entity.execution_policies.clone(),
        default_workspace_root: entity.default_workspace_root.clone(),
        memory_scope: entity.memory_scope.clone(),
        preferred_model_tier: entity.preferred_model_tier.clone(),
        allowed_agent_groups: entity.allowed_agent_groups.clone(),
        setup: entity.setup.clone(),
        readiness: entity.readiness.clone(),
        entity_runtime: entity.entity_runtime.clone(),
        colour: entity.colour.clone(),
        flow_diagrams: entity.flow_diagrams.clone(),
        knowledge_index: entity.knowledge_index.clone(),
    }
}

/// Compile-time guard: if a new field is added to `Entity`, the destructuring
/// pattern below fails to compile, forcing the dev to update
/// `EntityWithoutId` / `entity_to_upsert_body` so the new field is persisted.
/// This costs nothing at runtime — the function is `#[allow(dead_code)]`.
#[allow(dead_code)]
fn entity_field_completeness_guard(entity: &Entity) {
    let Entity {
        id: _,
        name: _,
        description: _,
        department: _,
        owner: _,
        status: _,
        entity_type: _,
        dept: _,
        file_count: _,
        path: _,
        workspace_id: _,
        visibility: _,
        is_personal: _,
        personal_user_id: _,
        local_path: _,
        files: _,
        stage: _,
        domains: _,
        markets: _,
        profile: _,
        goals: _,
        active_context: _,
        operating_rules: _,
        notes: _,
        content_pipeline: _,
        relationships: _,
        enrichment: _,
        resources: _,
        execution_policies: _,
        default_workspace_root: _,
        memory_scope: _,
        preferred_model_tier: _,
        allowed_agent_groups: _,
        setup: _,
        readiness: _,
        entity_runtime: _,
        colour: _,
        flow_diagrams: _,
        knowledge_index: _,
    } = entity;
}

impl SurrealEntityStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self {
            db,
            ledger: SyncLedger::default(),
        })
    }

    pub async fn init_schema(&self) -> Result<()> {
        // `Entity` serialises with `#[serde(rename_all = "camelCase")]`, so the
        // workspace key on disk is `workspaceId`, not `workspace_id`. The
        // pre-fix schema and the workspace-scoped query both used the
        // snake-case name, which silently broke per-workspace lookups (the
        // index never indexed anything, the query never matched). Aligning the
        // schema with the actual on-disk name fixes both.
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE entities SCHEMALESS;
                DEFINE FIELD workspaceId ON entities TYPE option<string>;
                DEFINE INDEX entities_workspace_idx ON entities FIELDS workspaceId;
                DEFINE FIELD personalUserId ON entities TYPE option<string>;
                DEFINE INDEX entities_personal_idx ON entities FIELDS personalUserId;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealEntityStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Returns every persisted entity. Used for cold-start hydration.
    ///
    /// Malformed rows (bad shape, missing required fields) are logged and
    /// skipped rather than aborting the whole hydration — losing one row is
    /// always better than refusing to start the gateway.
    ///
    /// Also records the outcome on the store: while the last listing failed,
    /// `sync_all` deletes nothing (see the module docs).
    pub async fn list_all(&self) -> Result<Vec<Entity>> {
        let rows: Vec<EntityRow> = match self.db.select("entities").await {
            Ok(rows) => rows,
            Err(e) => {
                self.ledger.record_full_listing(false);
                return Err(anyhow::anyhow!("select entities: {}", e));
            }
        };
        self.ledger.record_full_listing(true);
        Ok(self.rows_to_entities(rows))
    }

    /// Returns persisted entities for a specific workspace.
    ///
    /// Filters on `workspaceId` (the camelCase key produced by Entity's
    /// `#[serde(rename_all = "camelCase")]` — see `init_schema` for the full
    /// rationale).
    pub async fn list_by_workspace(&self, workspace_id: &str) -> Result<Vec<Entity>> {
        let wsid = workspace_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM entities WHERE workspaceId = $wsid")
            .bind(("wsid", wsid))
            .await
            .map_err(|e| anyhow::anyhow!("query entities by workspace: {}", e))?;
        let rows: Vec<EntityRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take entities: {}", e))?;
        Ok(self.rows_to_entities(rows))
    }

    /// Idempotent bulk sync: upserts every entity in `entities` that has not
    /// been deleted. One Surreal round-trip per row; fine for the expected <1k
    /// entities per workspace.
    ///
    /// Rows missing from `entities` are left alone — `entities` may be a stale
    /// clone or a fallback snapshot (module docs). The only deletes a sync
    /// performs are retries of [`Self::delete`] calls that failed, and none at
    /// all while the last hydration is known to have failed.
    pub async fn sync_all(&self, entities: &[Entity]) -> Result<()> {
        let _serial = self.ledger.serialize().await;
        for entity in entities {
            if self.ledger.is_tombstoned(&entity.id) {
                continue;
            }
            self.upsert_row(entity).await?;
        }
        if self.ledger.hydration_failed() {
            return Ok(());
        }
        for id in self.ledger.pending_deletes() {
            self.delete_row(&id).await?;
            self.ledger.delete_landed(&id);
        }
        Ok(())
    }

    /// Delete one entity row, explicitly. The id is tombstoned first, so a
    /// sync already holding a stale copy will skip it rather than write it
    /// back; if the row delete fails, later syncs retry it.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.ledger.tombstone(id);
        let _serial = self.ledger.serialize().await;
        self.delete_row(id).await?;
        self.ledger.delete_landed(id);
        Ok(())
    }

    // Callers hold the ledger's write lock.
    async fn upsert_row(&self, entity: &Entity) -> Result<()> {
        // Surreal sets the row id from the path argument; sending a string
        // `id` in the body collides with that. We strip it via the
        // `EntityUpsertBody` projection rather than going through
        // `serde_json::Value`, because the Surreal SDK serialiser cannot
        // round-trip `Value` (it rejects the externally-tagged enum).
        //
        // We discard the upsert response into `EntityRow` (typed against
        // `RecordId`) so the response deserialiser does not run into the
        // same Thing-vs-String mismatch that broke `list_all` before.
        let body = entity_to_upsert_body(entity);
        let _: Option<EntityRow> = self
            .db
            .upsert(("entities", entity.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert entity {}: {}", entity.id, e))?;
        Ok(())
    }

    // Callers hold the ledger's write lock.
    async fn delete_row(&self, id: &str) -> Result<()> {
        // Same reason as the upsert response: we read the deleted row back as
        // `EntityRow`, never as `Entity`, to avoid the Thing-vs-String mismatch.
        let _: Option<EntityRow> = self
            .db
            .delete(("entities", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete entity {}: {}", id, e))?;
        Ok(())
    }

    // Decode rows, skipping (and remembering) any that fail: losing one row
    // from the live copy beats refusing to start, and remembering it keeps a
    // sync from ever deleting it.
    fn rows_to_entities(&self, rows: Vec<EntityRow>) -> Vec<Entity> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            match row_to_entity(row) {
                Ok(e) => out.push(e),
                Err((id, err)) => {
                    tracing::warn!(
                        entity_id = %id,
                        error = %err,
                        "SurrealEntityStore: skipping malformed row (kept in the table)"
                    );
                    self.ledger.record_unreadable(&id);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_id_key_extracts_inner_key() {
        // RecordId::from_table_key is private/unstable across versions; build via SQL parsing instead.
        let parsed: surrealdb::RecordId = "entities:ent_42".parse().expect("parse");
        assert_eq!(record_id_key(&parsed), "ent_42");
    }

    #[test]
    fn upsert_body_strips_id_field() {
        let entity = Entity {
            id: "ent_123".to_string(),
            name: "Test".to_string(),
            status: "Active".to_string(),
            ..Default::default()
        };
        let body = entity_to_upsert_body(&entity);
        let serialised = serde_json::to_value(&body).expect("serialise upsert body");
        let obj = serialised.as_object().expect("object body");
        assert!(
            !obj.contains_key("id"),
            "id must be stripped from upsert body so Surreal sets it from the path arg"
        );
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("Test"));
    }

    async fn memory_store() -> (Arc<Surreal<Any>>, Arc<SurrealEntityStore>) {
        let db = surrealdb::engine::any::connect("memory")
            .await
            .expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let db = Arc::new(db);
        let store = SurrealEntityStore::new(db.clone());
        store.init_schema().await.expect("schema");
        (db, store)
    }

    fn entity(id: &str) -> Entity {
        Entity {
            id: id.to_string(),
            name: id.to_string(),
            status: "Active".to_string(),
            ..Default::default()
        }
    }

    async fn row_keys(store: &SurrealEntityStore) -> Vec<String> {
        let rows: Vec<EntityRow> = store.db.select("entities").await.expect("select");
        let mut keys: Vec<String> = rows.iter().map(|r| record_id_key(&r.id)).collect();
        keys.sort();
        keys
    }

    #[tokio::test]
    async fn a_row_that_fails_to_deserialize_survives_a_sync() {
        let (db, store) = memory_store().await;
        store.sync_all(&[entity("good")]).await.expect("seed");
        // `name` must be a string: this row cannot become an `Entity`.
        db.query("CREATE entities:broken CONTENT { name: 42, status: 'Active' }")
            .await
            .expect("insert malformed row");

        let hydrated = store.list_all().await.expect("hydrate");
        assert_eq!(hydrated.len(), 1, "the malformed row is skipped, not fatal");

        // The next periodic sync carries only what hydration could read.
        store.sync_all(&hydrated).await.expect("sync");
        assert_eq!(
            row_keys(&store).await,
            vec!["broken".to_string(), "good".to_string()],
            "a row hydration could not read must not be deleted by the next sync"
        );
    }

    #[tokio::test]
    async fn a_sync_after_failed_hydration_prunes_nothing() {
        // No namespace selected yet, so the hydration listing fails.
        let db = Arc::new(
            surrealdb::engine::any::connect("memory")
                .await
                .expect("connect"),
        );
        let store = SurrealEntityStore::new(db.clone());
        assert!(store.list_all().await.is_err(), "hydration must fail here");
        assert!(store.ledger.hydration_failed());

        db.use_ns("test").use_db("test").await.expect("use");
        db.query("CREATE entities:newer CONTENT { name: 'Newer', status: 'Active' }")
            .await
            .expect("row written by a previous life");

        // The gateway fell back to an older snapshot that lacks `newer`.
        store
            .sync_all(&[entity("from_snapshot")])
            .await
            .expect("sync");
        assert_eq!(
            row_keys(&store).await,
            vec!["from_snapshot".to_string(), "newer".to_string()],
            "a sync after a failed hydration must not delete rows it never saw"
        );
    }

    #[tokio::test]
    async fn only_explicit_deletes_remove_rows_and_stay_deleted() {
        let (_db, store) = memory_store().await;
        store.list_all().await.expect("hydrate");
        store
            .sync_all(&[entity("a"), entity("b")])
            .await
            .expect("seed");

        // A stale clone that lacks `b` no longer deletes it...
        store.sync_all(&[entity("a")]).await.expect("stale sync");
        assert_eq!(row_keys(&store).await, vec!["a", "b"]);

        // ...an explicit delete does, and a stale clone that still carries `b`
        // cannot write it back.
        store.delete("b").await.expect("delete");
        store
            .sync_all(&[entity("a"), entity("b")])
            .await
            .expect("stale sync after delete");
        assert_eq!(row_keys(&store).await, vec!["a"]);
    }
}
