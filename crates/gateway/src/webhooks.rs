//! Outbound webhooks: the operator feed, pushed to other systems.
//!
//! The inbound half of connectivity is scoped API keys (see
//! [`crate::auth_store::AuthStore::create_api_key`]) — any external platform
//! that holds one can drive `/api/*`. This module is the mirror image: a
//! workspace registers a URL, and every matching operator-feed event is POSTed
//! to it as `{"event": {...}, "workspaceId": "..."}`.
//!
//! Deliberately small:
//!
//! - **One table, no queue.** Delivery is best-effort from inside
//!   [`crate::notify::NotificationDispatcher`], on the same task that handles
//!   pushes: 10s timeout, failures logged and swallowed (exactly the posture in
//!   [`crate::alerts`]). A dead subscriber must never become a gateway failure.
//! - **Filtering is a list of kind names**, matched against the snake_case serde
//!   names of [`OperatorFeedKind`]. An empty list means "everything that reaches
//!   the dispatcher", which today is the notifiable allowlist (see
//!   [`crate::notify::is_notifiable`]).
//! - **Routes live here**, merged in `server.rs`, so the webhook surface doesn't
//!   have to thread through `ServerState`.
//!
//! This is not `ALERT_WEBHOOK_URL` ([`crate::alerts`]) — that is deployment
//! alerting for the operator of the *server*, a separate concern with its own
//! lifetime.

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use uuid::Uuid;

use crate::control_api::WorkspaceCtx;
use crate::state::OperatorFeedKind;

/// A registered outbound delivery target for one workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookSubscription {
    pub id: String,
    pub workspace_id: String,
    /// Absolute http(s) URL. Receives a POST per matching event.
    pub url: String,
    /// Snake_case [`OperatorFeedKind`] names. Empty = every delivered kind.
    #[serde(default)]
    pub kinds: Vec<String>,
    pub created_at: String,
}

/// The serde name of a feed kind (`OperatorFeedKind::TaskOverdue` →
/// `"task_overdue"`), which is what a subscription's `kinds` filter holds.
pub fn kind_name(kind: &OperatorFeedKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Does a subscription want this event? An empty filter means "all".
///
/// Pure on purpose: this is the one piece of webhook logic worth testing
/// directly, and it must never depend on a live store.
pub fn subscription_matches(kinds: &[String], kind: &OperatorFeedKind) -> bool {
    if kinds.is_empty() {
        return true;
    }
    let name = kind_name(kind);
    kinds
        .iter()
        .any(|k| k.trim().eq_ignore_ascii_case(name.as_str()))
}

/// Only absolute http(s) URLs. Keeps a typo from becoming a silent no-op and
/// keeps schemes like `file:` out of the delivery path entirely.
pub fn normalize_webhook_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.len() < 12 || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    Some(url.to_string())
}

/// Surreal-backed CRUD for [`WebhookSubscription`] rows.
pub struct SurrealWebhookStore {
    db: Arc<Surreal<Any>>,
}

// Read projection: Surreal v2 returns the key as a RecordId, so it's captured
// separately and re-injected before deserialising into `WebhookSubscription`.
#[derive(Deserialize)]
struct WebhookRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// Strips the Surreal `table:` prefix off a RecordId's string form.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1.trim_matches('`').to_string())
        .unwrap_or(s)
}

// A malformed row is logged and dropped (returns `None`) rather than failing
// the whole list.
fn row_to_subscription(row: WebhookRow) -> Option<WebhookSubscription> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    match serde_json::from_value::<WebhookSubscription>(serde_json::Value::Object(map)) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(webhook_id = %id, error = %e, "SurrealWebhookStore: dropping malformed row");
            None
        }
    }
}

// Id-less write body: the Surreal path argument is the single source of the
// record key.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebhookUpsertBody {
    workspace_id: String,
    url: String,
    kinds: Vec<String>,
    created_at: String,
}

impl SurrealWebhookStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Defines the schemaless table and its workspace index. Safe to call
    /// repeatedly — it runs on every boot.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE webhook_subscriptions SCHEMALESS;
                DEFINE FIELD workspaceId ON webhook_subscriptions TYPE option<string>;
                DEFINE INDEX webhook_subscriptions_workspace_idx
                    ON webhook_subscriptions FIELDS workspaceId;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealWebhookStore schema warning: {}", err);
        }
        Ok(())
    }

    pub async fn insert(&self, sub: &WebhookSubscription) -> Result<()> {
        let body = WebhookUpsertBody {
            workspace_id: sub.workspace_id.clone(),
            url: sub.url.clone(),
            kinds: sub.kinds.clone(),
            created_at: sub.created_at.clone(),
        };
        let _: Option<WebhookRow> = self
            .db
            .upsert(("webhook_subscriptions", sub.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert webhook {}: {}", sub.id, e))?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<WebhookSubscription>> {
        let row: Option<WebhookRow> = self
            .db
            .select(("webhook_subscriptions", id))
            .await
            .map_err(|e| anyhow::anyhow!("select webhook {}: {}", id, e))?;
        Ok(row.and_then(row_to_subscription))
    }

    pub async fn list_by_workspace(&self, workspace_id: &str) -> Result<Vec<WebhookSubscription>> {
        let mut res = self
            .db
            .query("SELECT * FROM webhook_subscriptions WHERE workspaceId = $wid")
            .bind(("wid", workspace_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("query webhooks by workspace: {}", e))?;
        let rows: Vec<WebhookRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take webhooks: {}", e))?;
        Ok(rows.into_iter().filter_map(row_to_subscription).collect())
    }

    pub async fn list_all(&self) -> Result<Vec<WebhookSubscription>> {
        let rows: Vec<WebhookRow> = self
            .db
            .select("webhook_subscriptions")
            .await
            .map_err(|e| anyhow::anyhow!("select webhooks: {}", e))?;
        Ok(rows.into_iter().filter_map(row_to_subscription).collect())
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: Option<WebhookRow> = self
            .db
            .delete(("webhook_subscriptions", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete webhook {}: {}", id, e))?;
        Ok(())
    }
}

// ===== HTTP =====

pub type WebhookState = Option<Arc<SurrealWebhookStore>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWebhookRequest {
    pub url: String,
    /// Feed kinds to receive; omit or leave empty for all delivered kinds.
    #[serde(default)]
    pub kinds: Vec<String>,
}

/// Webhook CRUD. Merged into the protected tree in `server.rs` (so the bearer
/// layer above it applies) rather than into `control_api`'s route block.
pub fn routes(store: WebhookState) -> Router {
    Router::new()
        .route("/api/webhooks", get(list_webhooks).post(create_webhook))
        .route("/api/webhooks/{id}", delete(delete_webhook))
        .layer(middleware::from_fn(
            crate::control_api::workspace_context_middleware,
        ))
        .with_state(store)
}

fn require_store(store: &WebhookState) -> Result<&Arc<SurrealWebhookStore>, StatusCode> {
    store.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

async fn list_webhooks(
    State(store): State<WebhookState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<WebhookSubscription>>, StatusCode> {
    let subs = require_store(&store)?
        .list_by_workspace(&workspace.id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "webhook list failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(subs))
}

async fn create_webhook(
    State(store): State<WebhookState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<WebhookSubscription>), StatusCode> {
    let s = require_store(&store)?;
    let url = normalize_webhook_url(&body.url).ok_or(StatusCode::BAD_REQUEST)?;
    let kinds: Vec<String> = body
        .kinds
        .into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    let sub = WebhookSubscription {
        id: Uuid::new_v4().simple().to_string(),
        workspace_id: workspace.id.clone(),
        url,
        kinds,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    s.insert(&sub).await.map_err(|e| {
        tracing::warn!(error = %e, "webhook create failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok((StatusCode::CREATED, Json(sub)))
}

async fn delete_webhook(
    State(store): State<WebhookState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let s = require_store(&store)?;
    // Scope the delete to the caller's workspace: an id alone is not authority.
    let existing = s
        .get(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match existing {
        Some(sub) if sub.workspace_id == workspace.id => {}
        _ => return Err(StatusCode::NOT_FOUND),
    }
    s.delete(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_the_snake_case_serde_names() {
        assert_eq!(kind_name(&OperatorFeedKind::TaskOverdue), "task_overdue");
        assert_eq!(
            kind_name(&OperatorFeedKind::ApprovalRequestCreated),
            "approval_request_created"
        );
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(subscription_matches(&[], &OperatorFeedKind::TaskOverdue));
        assert!(subscription_matches(&[], &OperatorFeedKind::DailyDigest));
    }

    #[test]
    fn filter_matches_only_listed_kinds() {
        let kinds = vec!["task_overdue".to_string(), "daily_digest".to_string()];
        assert!(subscription_matches(&kinds, &OperatorFeedKind::TaskOverdue));
        assert!(subscription_matches(&kinds, &OperatorFeedKind::DailyDigest));
        assert!(!subscription_matches(
            &kinds,
            &OperatorFeedKind::HeartbeatResult
        ));
    }

    #[test]
    fn filter_is_whitespace_and_case_tolerant() {
        let kinds = vec![" Task_Overdue ".to_string()];
        assert!(subscription_matches(&kinds, &OperatorFeedKind::TaskOverdue));
    }

    #[test]
    fn urls_must_be_absolute_http() {
        assert_eq!(
            normalize_webhook_url("  https://example.com/hook  ").as_deref(),
            Some("https://example.com/hook")
        );
        assert!(normalize_webhook_url("example.com/hook").is_none());
        assert!(normalize_webhook_url("file:///etc/passwd").is_none());
        assert!(normalize_webhook_url("").is_none());
    }

    #[tokio::test]
    async fn store_round_trip_in_memory_surreal() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealWebhookStore::new(Arc::new(db));
        store.init_schema().await.expect("init");
        // Schema init must be idempotent (it runs on every boot).
        store.init_schema().await.expect("init twice");

        let sub = WebhookSubscription {
            id: "hook_a".to_string(),
            workspace_id: "ws_default".to_string(),
            url: "https://example.com/hook".to_string(),
            kinds: vec!["task_overdue".to_string()],
            created_at: "2026-08-01T09:00:00Z".to_string(),
        };
        let other = WebhookSubscription {
            id: "hook_b".to_string(),
            workspace_id: "ws_other".to_string(),
            url: "https://other.example.com/hook".to_string(),
            kinds: vec![],
            created_at: "2026-08-01T09:00:00Z".to_string(),
        };
        store.insert(&sub).await.expect("insert a");
        store.insert(&other).await.expect("insert b");

        let mine = store
            .list_by_workspace("ws_default")
            .await
            .expect("list by workspace");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, "hook_a");
        assert_eq!(mine[0].url, "https://example.com/hook");
        assert_eq!(mine[0].kinds, vec!["task_overdue".to_string()]);

        assert_eq!(store.list_all().await.expect("list all").len(), 2);
        assert_eq!(
            store.get("hook_a").await.expect("get").map(|s| s.url),
            Some("https://example.com/hook".to_string())
        );

        store.delete("hook_a").await.expect("delete");
        assert!(store.get("hook_a").await.expect("get").is_none());
        assert_eq!(store.list_all().await.expect("list all").len(), 1);
    }
}
