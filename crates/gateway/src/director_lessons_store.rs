//! Surreal-backed append-only learning log for directors.
//!
//! Each lesson is scoped to a director (and optionally to an entity / mission
//! via the `scope` string) and gets folded into the director's system prompt
//! at recall time. Two recall modes:
//!
//! - **Semantic** — if a query embedding is supplied (dim 1536 cosine, matches
//!   the `memories` HNSW convention), we top-k by cosine distance.
//! - **Recency** — falls back to the most recent N lessons for the director.
//!
//! Recall is always filtered by `director_id` so one director's lessons can
//! never leak into another's prompt.

use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::state::DirectorLesson;

/// Vector width expected by the `director_lessons` HNSW index; also used to
/// reject an embedding provider whose output wouldn't fit it (see
/// [`crate::lessons::embeddable`]).
pub const LESSON_EMBEDDING_DIMENSION: usize = 1536;

/// Handle to the `director_lessons` table. Cheap to clone via `Arc`.
pub struct SurrealDirectorLessonsStore {
    db: Arc<Surreal<Any>>,
}

// Read-side shape: Surreal returns the id as a `RecordId`/`Thing` separate
// from the rest of the fields; re-flattened in `row_to_lesson`.
#[derive(Deserialize)]
struct LessonRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// Strip the `director_lessons:` table prefix (and any backtick-escaping) to
// get back the plain id.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

// Reassemble a `LessonRow` into a `DirectorLesson` by folding the resolved
// id back into the flattened field map before deserializing.
fn row_to_lesson(row: LessonRow) -> Result<DirectorLesson, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<DirectorLesson>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

// Write-side shape: mirrors `DirectorLesson` minus `id` (Surreal supplies
// the id via the `.upsert(("director_lessons", id))` call itself).
#[derive(serde::Serialize)]
struct LessonUpsertBody {
    director_id: String,
    scope: String,
    lesson: String,
    source: String,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding: Option<Vec<f32>>,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
}

// Field-by-field copy rather than a partial move so the compiler flags any
// future `DirectorLesson` field that isn't also threaded through here.
fn lesson_to_upsert_body(l: &DirectorLesson) -> LessonUpsertBody {
    LessonUpsertBody {
        director_id: l.director_id.clone(),
        scope: l.scope.clone(),
        lesson: l.lesson.clone(),
        source: l.source.clone(),
        severity: l.severity.clone(),
        embedding: l.embedding.clone(),
        created_at: l.created_at.clone(),
        created_by: l.created_by.clone(),
    }
}

/// Compile-time guard: a new `DirectorLesson` field forces an update to the
/// upsert body so it isn't silently dropped from persistence.
#[allow(dead_code)]
fn director_lesson_field_completeness_guard(l: &DirectorLesson) {
    let DirectorLesson {
        id: _,
        director_id: _,
        scope: _,
        lesson: _,
        source: _,
        severity: _,
        embedding: _,
        created_at: _,
        created_by: _,
    } = l;
}

impl SurrealDirectorLessonsStore {
    /// Wrap an existing Surreal connection; does not touch the schema.
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Define the `director_lessons` table, its indexes, and the HNSW
    /// vector index used for semantic recall. Safe to call more than once.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE director_lessons SCHEMALESS;
                DEFINE FIELD director_id ON director_lessons TYPE string;
                DEFINE FIELD scope ON director_lessons TYPE string;
                DEFINE FIELD embedding ON director_lessons TYPE option<array<float>>;
                DEFINE FIELD created_at ON director_lessons TYPE string;
                DEFINE INDEX director_lessons_director_idx ON director_lessons FIELDS director_id;
                DEFINE INDEX director_lessons_embed_idx ON director_lessons FIELDS embedding HNSW DIMENSION 1536 DIST COSINE;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealDirectorLessonsStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Append a lesson. The `id` on `lesson` is used as the row key — caller
    /// generates it (uuid, etc.). Upsert semantics so retries are safe.
    pub async fn append_lesson(&self, lesson: &DirectorLesson) -> Result<()> {
        let body = lesson_to_upsert_body(lesson);
        let _: Option<LessonRow> = self
            .db
            .upsert(("director_lessons", lesson.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("append lesson {}: {}", lesson.id, e))?;
        Ok(())
    }

    /// Recall lessons for a director. If `query_embedding` is non-empty, does
    /// HNSW top-k. Otherwise returns the most recent `k` by `created_at`.
    ///
    /// Always filters by `director_id` so cross-director leakage is impossible.
    pub async fn recall_lessons(
        &self,
        director_id: &str,
        query_embedding: Option<&[f32]>,
        k: usize,
    ) -> Result<Vec<DirectorLesson>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let did = director_id.to_string();

        if let Some(embedding) = query_embedding
            && !embedding.is_empty()
        {
            // Surreal requires K as a literal in <|K,COSINE|>, not a bound
            // parameter, and ORDER BY takes an alias, not a function call.
            // Two planner traps force the shape of this query:
            //   1. knn picks the K nearest across the WHOLE table before any
            //      other condition applies — so we over-fetch.
            //   2. an equality condition on an indexed field (director_id)
            //      silently short-circuits the knn operator to zero rows — so
            //      the director filter happens in Rust, which also keeps the
            //      no-leakage guarantee independent of the query planner.
            let fetch_k = (k.saturating_mul(8)).clamp(k, 256);
            let q = format!(
                "SELECT *, vector::distance::knn() AS knn_dist FROM director_lessons \
                 WHERE embedding != NONE \
                 AND embedding <|{},COSINE|> $embedding \
                 ORDER BY knn_dist",
                fetch_k
            );
            let mut res = self
                .db
                .query(&q)
                .bind(("embedding", embedding.to_vec()))
                .await
                .map_err(|e| anyhow::anyhow!("knn recall lessons: {}", e))?;
            let rows: Vec<LessonRow> = res
                .take(0)
                .map_err(|e| anyhow::anyhow!("take knn lessons: {}", e))?;
            let mut lessons = rows_to_lessons(rows);
            lessons.retain(|l| l.director_id == did);
            lessons.truncate(k);
            return Ok(lessons);
        }

        let mut res = self
            .db
            .query(
                "SELECT * FROM director_lessons \
                 WHERE director_id = $did \
                 ORDER BY created_at DESC LIMIT $k",
            )
            .bind(("did", did))
            .bind(("k", k as i64))
            .await
            .map_err(|e| anyhow::anyhow!("recall recent lessons: {}", e))?;
        let rows: Vec<LessonRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take recent lessons: {}", e))?;
        Ok(rows_to_lessons(rows))
    }

    /// Recency-only recall restricted to a set of `scope` values, projecting
    /// away `embedding`.
    ///
    /// Two differences from [`recall_lessons`]'s recency branch matter to the
    /// caller. The scope filter runs server-side, so this fetches exactly `k`
    /// rows instead of the `k * 4` the chat path over-fetches and then filters
    /// in Rust. And the projection omits `embedding`, which is the difference
    /// between moving ~1 KB and ~180 KB for six lessons — these rows travel
    /// into the architect's audit payload, where a vector per row would bury
    /// the text it describes.
    ///
    /// Deliberately no knn branch: an equality condition on the indexed
    /// `director_id` short-circuits the knn operator to zero rows (see the
    /// trap documented in `recall_lessons`), so semantic recall must keep
    /// filtering in Rust and must not be routed through here.
    ///
    /// [`recall_lessons`]: SurrealDirectorLessonsStore::recall_lessons
    pub async fn recall_recent_for_scopes(
        &self,
        director_id: &str,
        scopes: &[String],
        k: usize,
    ) -> Result<Vec<DirectorLesson>> {
        if k == 0 || scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut res = self
            .db
            .query(
                // `created_at` stays in the projection so ORDER BY resolves
                // ("Missing order idiom"); `embedding` is the one field left out.
                "SELECT id, director_id, scope, lesson, source, severity, created_at, created_by \
                 FROM director_lessons \
                 WHERE director_id = $did AND scope IN $scopes \
                 ORDER BY created_at DESC LIMIT $k",
            )
            .bind(("did", director_id.to_string()))
            .bind(("scopes", scopes.to_vec()))
            .bind(("k", k as i64))
            .await
            .map_err(|e| anyhow::anyhow!("recall lessons for scopes: {}", e))?;
        let rows: Vec<LessonRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take scoped lessons: {}", e))?;
        Ok(rows_to_lessons(rows))
    }

    /// List every lesson for a director, unfiltered and unranked (unlike
    /// [`Self::recall_lessons`], which caps and orders for prompt injection).
    pub async fn list_for_director(&self, director_id: &str) -> Result<Vec<DirectorLesson>> {
        let did = director_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM director_lessons WHERE director_id = $did")
            .bind(("did", did))
            .await
            .map_err(|e| anyhow::anyhow!("list lessons: {}", e))?;
        let rows: Vec<LessonRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take lessons: {}", e))?;
        Ok(rows_to_lessons(rows))
    }

    /// Lessons still missing an embedding — written while no embedding
    /// provider passed startup probe, or whose one-shot backfill failed.
    /// Not scoped by director: this backs a global sweep, not prompt recall.
    pub async fn list_missing_embeddings(&self) -> Result<Vec<DirectorLesson>> {
        let mut res = self
            .db
            .query("SELECT * FROM director_lessons WHERE embedding = NONE")
            .await
            .map_err(|e| anyhow::anyhow!("list lessons missing embeddings: {}", e))?;
        let rows: Vec<LessonRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take lessons missing embeddings: {}", e))?;
        Ok(rows_to_lessons(rows))
    }

    /// Remove a lesson row permanently by id.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: Option<LessonRow> = self
            .db
            .delete(("director_lessons", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete lesson {}: {}", id, e))?;
        Ok(())
    }
}

// Best-effort batch conversion: a malformed row is logged and dropped
// instead of failing the whole listing.
fn rows_to_lessons(rows: Vec<LessonRow>) -> Vec<DirectorLesson> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row_to_lesson(row) {
            Ok(l) => out.push(l),
            Err((id, err)) => {
                tracing::warn!(
                    lesson_id = %id,
                    error = %err,
                    "SurrealDirectorLessonsStore: dropping malformed row"
                );
            }
        }
    }
    out
}

/// Render a slice of lessons as a markdown block suitable for splicing into a
/// director's system prompt. Returns `None` when the slice is empty so the
/// caller can omit the section cleanly.
pub fn render_lessons_block(lessons: &[DirectorLesson]) -> Option<String> {
    if lessons.is_empty() {
        return None;
    }
    let mut out = String::from("## Lessons learned\n");
    for l in lessons {
        let prefix = match l.severity.as_str() {
            "hard" => "•!",
            _ => "•",
        };
        out.push_str(&format!("{} {} ({})\n", prefix, l.lesson.trim(), l.source));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lesson(id: &str, director: &str, lesson: &str, created_at: &str) -> DirectorLesson {
        DirectorLesson {
            id: id.to_string(),
            director_id: director.to_string(),
            scope: "global".to_string(),
            lesson: lesson.to_string(),
            source: "correction".to_string(),
            severity: "soft".to_string(),
            embedding: None,
            created_at: created_at.to_string(),
            created_by: Some("carlos".to_string()),
        }
    }

    #[test]
    fn upsert_body_strips_id_field() {
        let l = sample_lesson(
            "les_1",
            "dir_ops",
            "always check git status",
            "2026-05-24T00:00:00Z",
        );
        let body = lesson_to_upsert_body(&l);
        let v = serde_json::to_value(&body).expect("serialise");
        let obj = v.as_object().expect("object");
        assert!(!obj.contains_key("id"));
        assert_eq!(
            obj.get("director_id").and_then(|v| v.as_str()),
            Some("dir_ops")
        );
    }

    #[test]
    fn render_lessons_block_emits_severity_marker() {
        let mut hard = sample_lesson(
            "les_h",
            "dir_ops",
            "never push to remote",
            "2026-05-24T00:00:00Z",
        );
        hard.severity = "hard".to_string();
        let soft = sample_lesson(
            "les_s",
            "dir_ops",
            "prefer terse summaries",
            "2026-05-24T00:00:00Z",
        );
        let block = render_lessons_block(&[hard, soft]).expect("block");
        assert!(block.contains("•! never push to remote"));
        assert!(block.contains("• prefer terse summaries"));
    }

    #[test]
    fn render_lessons_block_empty_is_none() {
        assert!(render_lessons_block(&[]).is_none());
    }

    #[tokio::test]
    async fn recall_filters_by_director_and_falls_back_to_recency() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorLessonsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");
        store.init_schema().await.expect("idempotent");

        let l_old = sample_lesson("les_old", "dir_ops", "old lesson", "2026-05-20T00:00:00Z");
        let l_new = sample_lesson("les_new", "dir_ops", "new lesson", "2026-05-23T00:00:00Z");
        let l_other = sample_lesson(
            "les_other",
            "dir_fullstack",
            "other dir",
            "2026-05-24T00:00:00Z",
        );
        store.append_lesson(&l_old).await.expect("old");
        store.append_lesson(&l_new).await.expect("new");
        store.append_lesson(&l_other).await.expect("other");

        let recall = store
            .recall_lessons("dir_ops", None, 5)
            .await
            .expect("recall");
        let ids: Vec<_> = recall.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["les_new", "les_old"],
            "newest first, no leakage from other director"
        );

        let limited = store
            .recall_lessons("dir_ops", None, 1)
            .await
            .expect("limited");
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, "les_new");

        // Cross-director check: nothing from dir_fullstack ever appears for dir_ops.
        for l in &recall {
            assert_ne!(l.director_id, "dir_fullstack");
        }
    }

    // The scoped recall is what the architect's Inspect stage reads, and it must
    // agree with what `chat_agent::recall_chat_lessons` filters into the live
    // prompt — otherwise the audit trail describes a world the model never saw.
    // It must also never ship `embedding`: these rows land in `audit_trail.payload`,
    // and a 1536-float vector per lesson would bury the text it describes.
    #[tokio::test]
    async fn recall_recent_for_scopes_filters_scope_and_omits_embedding() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorLessonsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let mut global =
            sample_lesson("les_global", "operations", "global one", "2026-05-20T00:00:00Z");
        global.scope = "global".to_string();
        // Carries a real vector, so a regressed projection shows up here.
        global.embedding = Some(vec![0.5; LESSON_EMBEDDING_DIMENSION]);

        let mut mine = sample_lesson("les_mine", "operations", "for ent_a", "2026-05-23T00:00:00Z");
        mine.scope = "entity:ent_a".to_string();

        let mut theirs =
            sample_lesson("les_theirs", "operations", "for ent_b", "2026-05-24T00:00:00Z");
        theirs.scope = "entity:ent_b".to_string();

        let mut other_dir =
            sample_lesson("les_otherdir", "fullstack", "other dir", "2026-05-25T00:00:00Z");
        other_dir.scope = "entity:ent_a".to_string();

        for l in [&global, &mine, &theirs, &other_dir] {
            store.append_lesson(l).await.expect("append");
        }

        let scopes = vec!["global".to_string(), "entity:ent_a".to_string()];
        let got = store
            .recall_recent_for_scopes("operations", &scopes, 10)
            .await
            .expect("recall");

        // Exact ids, not just a count: `rows_to_lessons` silently drops malformed
        // rows, so a broken projection would otherwise pass as "filtered correctly".
        let ids: Vec<_> = got.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["les_mine", "les_global"], "newest first, scoped");
        for l in &got {
            assert!(
                l.embedding.is_none(),
                "embedding must not be projected: {}",
                l.id
            );
            assert_eq!(l.director_id, "operations");
        }

        assert!(
            store
                .recall_recent_for_scopes("operations", &[], 10)
                .await
                .expect("empty scopes")
                .is_empty(),
            "no scopes means no rows, not every row"
        );
        assert!(
            store
                .recall_recent_for_scopes("operations", &scopes, 0)
                .await
                .expect("zero k")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn semantic_recall_ranks_by_cosine_and_filters_director() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorLessonsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        // Orthogonal-ish unit vectors along distinct axes of the 1536-dim space.
        fn axis(i: usize) -> Vec<f32> {
            let mut v = vec![0.0; LESSON_EMBEDDING_DIMENSION];
            v[i] = 1.0;
            v
        }
        let mut near = sample_lesson("les_near", "dir_ops", "near", "2026-05-20T00:00:00Z");
        near.embedding = Some(axis(0));
        let mut far = sample_lesson("les_far", "dir_ops", "far", "2026-05-24T00:00:00Z");
        far.embedding = Some(axis(1));
        let mut other = sample_lesson("les_leak", "dir_fullstack", "leak", "2026-05-24T00:00:00Z");
        other.embedding = Some(axis(0));
        store.append_lesson(&near).await.expect("near");
        store.append_lesson(&far).await.expect("far");
        store.append_lesson(&other).await.expect("other");

        // Query along axis 0, slightly perturbed so it isn't an exact match.
        let mut q = axis(0);
        q[2] = 0.1;
        let recall = store
            .recall_lessons("dir_ops", Some(&q), 1)
            .await
            .expect("semantic recall");
        assert_eq!(recall.len(), 1);
        assert_eq!(
            recall[0].id, "les_near",
            "nearest-by-cosine wins over newest"
        );
        assert_ne!(recall[0].director_id, "dir_fullstack");
    }
}
