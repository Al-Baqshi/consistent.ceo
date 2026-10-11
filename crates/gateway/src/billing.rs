//! Billing middleware + credit ledger.
//!
//! Per-workspace credit balance enforced at request time. Requests with a
//! bearer token mapped to a workspace are rejected with 402 Payment Required
//! when the balance is zero. LLM inference cost is debited after completion
//! at a configurable markup (100%–300%).
//!
//! Persistence: balances live in SurrealDB (`credit_balances` table, keyed by
//! workspace_id). A DashMap write-through cache avoids a DB round-trip for
//! hot-path reads. When no Surreal handle is attached (tests), the ledger is
//! pure in-memory.

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tracing::debug;

use crate::control_api::AuthContext;
use crate::state::ServerState;

#[derive(Debug, Deserialize, Serialize)]
struct BalanceRow {
    balance_cents: i64,
}

/// True when the gateway is bound to a loopback-only address — the self-host /
/// desktop case, where billing is never enforced.
pub fn is_loopback_bind(bind: &str) -> bool {
    let bind = bind.trim().to_ascii_lowercase();
    bind.is_empty() || bind == "127.0.0.1" || bind == "localhost" || bind == "::1"
}

/// Whether the credit gate is on. **Opt-in only**: a self-hosted gateway,
/// including one in docker bound to `0.0.0.0` so the phone can reach it, must
/// never answer 402 because nobody configured a ledger it does not use. A
/// hosted deployment that sells inference sets `CONSISTENT_WORLD_BILLING_ENFORCE=1`
/// and binds a non-loopback address; either missing keeps billing off.
pub fn billing_enforced(bind: &str, enforce_env: Option<&str>) -> bool {
    let opted_in = matches!(
        enforce_env
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    );
    opted_in && !is_loopback_bind(bind)
}

/// Per-workspace credit balance in USD cents (integer to avoid float drift).
#[derive(Debug)]
pub struct CreditLedger {
    cache: DashMap<String, i64>,
    db: Option<Arc<Surreal<Any>>>,
    /// Markup multiplier applied to raw LLM cost. 2.0 = 100% markup.
    pub markup: f64,
    /// Whether zero-balance workspaces are refused service. Decided once at
    /// startup by [`billing_enforced`]: off unless CONSISTENT_WORLD_BILLING_ENFORCE=1
    /// on a non-loopback bind.
    pub enforce: bool,
}

impl CreditLedger {
    /// In-memory only ledger. Used in tests. Values do not persist across restarts.
    pub fn new(markup: f64) -> Arc<Self> {
        Arc::new(Self {
            cache: DashMap::new(),
            db: None,
            markup: markup.max(1.0),
            enforce: false,
        })
    }

    /// In-memory ledger with enforcement on. Used in tests of the billing gate.
    pub fn new_enforced(markup: f64) -> Arc<Self> {
        Arc::new(Self {
            cache: DashMap::new(),
            db: None,
            markup: markup.max(1.0),
            enforce: true,
        })
    }

    /// Surreal-backed ledger. Balances survive restarts.
    pub fn new_with_surreal(markup: f64, db: Arc<Surreal<Any>>, enforce: bool) -> Arc<Self> {
        Arc::new(Self {
            cache: DashMap::new(),
            db: Some(db),
            markup: markup.max(1.0),
            enforce,
        })
    }

    /// No-op when this ledger has no Surreal handle (in-memory-only mode).
    pub async fn init_schema(&self) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let mut res = db
            .query(
                r#"
                DEFINE TABLE credit_balances SCHEMALESS;
                DEFINE FIELD balance_cents ON credit_balances TYPE int;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("CreditLedger schema warning: {}", err);
        }
        Ok(())
    }

    // No row, no db, or a query error all read as a zero balance rather than failing the caller.
    async fn load_from_surreal(&self, workspace_id: &str) -> i64 {
        let Some(db) = self.db.as_ref() else {
            return 0;
        };
        match db
            .select::<Option<BalanceRow>>(("credit_balances", workspace_id))
            .await
        {
            Ok(Some(row)) => row.balance_cents,
            Ok(None) => 0,
            Err(e) => {
                tracing::warn!(error = %e, workspace_id, "credit balance load failed");
                0
            }
        }
    }

    // Best-effort persist: a failure is logged, not propagated — the cache stays authoritative.
    async fn write_to_surreal(&self, workspace_id: &str, balance_cents: i64) {
        let Some(db) = self.db.as_ref() else {
            return;
        };
        let row = BalanceRow { balance_cents };
        if let Err(e) = db
            .upsert::<Option<BalanceRow>>(("credit_balances", workspace_id))
            .content(row)
            .await
        {
            tracing::warn!(error = %e, workspace_id, "credit balance persist failed");
        }
    }

    async fn mirror_wallet(&self, workspace_id: &str, balance_cents: i64) {
        crate::local_vault::mirror_wallet_balance(workspace_id, balance_cents, self.markup).await;
    }

    /// Read-through: cache → Surreal. On cache miss, loads and caches.
    pub async fn balance_cents(&self, workspace_id: &str) -> i64 {
        if let Some(v) = self.cache.get(workspace_id) {
            return *v.value();
        }
        let persisted = self.load_from_surreal(workspace_id).await;
        self.cache.insert(workspace_id.to_string(), persisted);
        persisted
    }

    /// Adds `cents` to the balance. Returns new balance.
    pub async fn topup_cents(&self, workspace_id: &str, cents: i64) -> i64 {
        let current = self.balance_cents(workspace_id).await;
        let new_balance = current.saturating_add(cents);
        self.cache.insert(workspace_id.to_string(), new_balance);
        self.write_to_surreal(workspace_id, new_balance).await;
        self.mirror_wallet(workspace_id, new_balance).await;
        new_balance
    }

    /// Returns Ok(new_balance) if debited, Err(balance) if insufficient.
    pub async fn try_debit_cents(&self, workspace_id: &str, cents: i64) -> Result<i64, i64> {
        let current = self.balance_cents(workspace_id).await;
        if current < cents {
            return Err(current);
        }
        let new_balance = current.saturating_sub(cents);
        self.cache.insert(workspace_id.to_string(), new_balance);
        self.write_to_surreal(workspace_id, new_balance).await;
        self.mirror_wallet(workspace_id, new_balance).await;
        Ok(new_balance)
    }

    /// Debit the marked-up equivalent of a raw USD cost. Returns the billed
    /// cents (>= 1) so callers can log/surface it. Debits are best-effort —
    /// if the balance has already hit 0 we don't go negative.
    pub async fn debit_for_cost(&self, workspace_id: &str, raw_cost_usd: f64) -> i64 {
        let billed_cents = (raw_cost_usd * 100.0 * self.markup).ceil() as i64;
        let billed_cents = billed_cents.max(1);
        let current = self.balance_cents(workspace_id).await;
        let taken = billed_cents.min(current);
        let new_balance = current.saturating_sub(taken);
        self.cache.insert(workspace_id.to_string(), new_balance);
        self.write_to_surreal(workspace_id, new_balance).await;
        self.mirror_wallet(workspace_id, new_balance).await;
        billed_cents
    }

    /// True when this workspace should be refused further LLM work: enforcement
    /// is on and the balance is gone. Always false for self-host/loopback
    /// deployments (enforce = false).
    pub async fn credit_exhausted(&self, workspace_id: &str) -> bool {
        self.enforce && self.balance_cents(workspace_id).await <= 0
    }
}

/// Body of `POST /api/billing/topup`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopupRequest {
    pub amount_cents: i64,
    /// Target workspace. Only honored for the admin (static-token) principal;
    /// everyone else can only top up their own workspace.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

/// Response shape shared by the balance and top-up endpoints.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceResponse {
    pub workspace_id: String,
    pub balance_cents: i64,
    pub markup: f64,
}

/// Balance read and top-up routes; neither is gated by [`require_credit`] itself.
pub fn billing_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/billing/balance", get(get_balance))
        .route("/api/billing/topup", post(post_topup))
        .with_state(state)
}

async fn get_balance(
    State(state): State<ServerState>,
    req: Request<Body>,
) -> Result<Json<BalanceResponse>, StatusCode> {
    let ctx = req
        .extensions()
        .get::<AuthContext>()
        .ok_or(StatusCode::UNAUTHORIZED)?
        .clone();
    let ledger = state
        .credit_ledger
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let balance_cents = ledger.balance_cents(&ctx.workspace_id).await;
    ledger.mirror_wallet(&ctx.workspace_id, balance_cents).await;
    Ok(Json(BalanceResponse {
        workspace_id: ctx.workspace_id.clone(),
        balance_cents,
        markup: ledger.markup,
    }))
}

// Grants credit to a workspace; only the admin principal may target a
// workspace other than their own (see the enforcement check below).
async fn post_topup(
    State(state): State<ServerState>,
    req: Request<Body>,
) -> Result<Json<BalanceResponse>, StatusCode> {
    let ctx = req
        .extensions()
        .get::<AuthContext>()
        .ok_or(StatusCode::UNAUTHORIZED)?
        .clone();
    let ledger = state
        .credit_ledger
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    // "bearer" is the principal minted for the raw static gateway token — the
    // operator. Session users and scoped tokens get their own principal ids.
    let is_admin = ctx.principal_id == "bearer";
    // On hosted deployments (enforcement on) credits are real money: only the
    // operator may grant them. Self-host/loopback keeps the open behavior.
    if ledger.enforce && !is_admin {
        return Err(StatusCode::FORBIDDEN);
    }
    let bytes = axum::body::to_bytes(req.into_body(), 1024 * 8)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let body: TopupRequest = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    if body.amount_cents <= 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let target_workspace = match body.workspace_id {
        Some(ws) if is_admin => ws,
        Some(ws) if ws == ctx.workspace_id => ws,
        Some(_) => return Err(StatusCode::FORBIDDEN),
        None => ctx.workspace_id,
    };
    let new_balance = ledger
        .topup_cents(&target_workspace, body.amount_cents)
        .await;
    Ok(Json(BalanceResponse {
        workspace_id: target_workspace,
        balance_cents: new_balance,
        markup: ledger.markup,
    }))
}

/// Middleware that rejects inference requests when the caller's workspace
/// has no credit. Pass-through for workspaces with any positive balance — the
/// exact cost is debited after the LLM call returns so billing reflects real
/// token usage.
pub async fn require_credit(
    State(state): State<ServerState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(ledger) = state.credit_ledger.clone() else {
        return Ok(next.run(request).await);
    };
    // Enforcement is decided once at startup (main.rs, `billing_enforced`): off
    // unless a hosted deployment opted in with CONSISTENT_WORLD_BILLING_ENFORCE=1.
    if !ledger.enforce {
        return Ok(next.run(request).await);
    }
    let ctx = request
        .extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let balance = ledger.balance_cents(&ctx.workspace_id).await;
    if balance <= 0 {
        debug!(
            "rejecting workspace={} balance=0 path={}",
            ctx.workspace_id,
            request.uri().path()
        );
        return Err(StatusCode::PAYMENT_REQUIRED);
    }
    Ok(next.run(request).await)
}

/// Shorthand to apply `require_credit` to a subrouter carrying a state.
pub fn with_credit_gate(router: Router, state: ServerState) -> Router {
    router.layer(middleware::from_fn_with_state(state, require_credit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn topup_then_debit_balances_correctly() {
        let l = CreditLedger::new(2.0);
        assert_eq!(l.balance_cents("ws").await, 0);
        l.topup_cents("ws", 1_000).await;
        assert_eq!(l.balance_cents("ws").await, 1_000);
        assert_eq!(l.try_debit_cents("ws", 300).await, Ok(700));
        assert_eq!(l.balance_cents("ws").await, 700);
    }

    #[tokio::test]
    async fn debit_rejects_when_insufficient() {
        let l = CreditLedger::new(2.0);
        l.topup_cents("ws", 100).await;
        assert_eq!(l.try_debit_cents("ws", 500).await, Err(100));
        assert_eq!(l.balance_cents("ws").await, 100);
    }

    #[tokio::test]
    async fn debit_for_cost_applies_markup() {
        let l = CreditLedger::new(2.0);
        l.topup_cents("ws", 10_000).await;
        // Raw $0.10 * 100 cents * 2x markup = 20 cents billed.
        let billed = l.debit_for_cost("ws", 0.10).await;
        assert_eq!(billed, 20);
        assert_eq!(l.balance_cents("ws").await, 9_980);
    }

    #[tokio::test]
    async fn debit_for_cost_never_goes_negative() {
        let l = CreditLedger::new(3.0);
        l.topup_cents("ws", 10).await;
        l.debit_for_cost("ws", 100.0).await;
        assert_eq!(l.balance_cents("ws").await, 0);
    }

    #[test]
    fn billing_is_opt_in_even_on_a_public_bind() {
        // Self-host defaults: never enforced, whatever the bind.
        assert!(!billing_enforced("127.0.0.1", None));
        assert!(!billing_enforced("0.0.0.0", None));
        assert!(!billing_enforced("::", Some("0")));
        assert!(!billing_enforced("0.0.0.0", Some("")));
        // Opting in still never bills a loopback-only gateway.
        assert!(!billing_enforced("127.0.0.1", Some("1")));
        // Hosted: explicit opt-in on a public bind.
        assert!(billing_enforced("0.0.0.0", Some("1")));
        assert!(billing_enforced("::", Some("true")));
    }

    #[tokio::test]
    async fn credit_exhausted_only_when_enforced() {
        let open = CreditLedger::new(2.0);
        assert!(!open.credit_exhausted("ws").await);

        let enforced = CreditLedger::new_enforced(2.0);
        assert!(enforced.credit_exhausted("ws").await);
        enforced.topup_cents("ws", 100).await;
        assert!(!enforced.credit_exhausted("ws").await);
        enforced.debit_for_cost("ws", 100.0).await;
        assert!(enforced.credit_exhausted("ws").await);
    }

    #[test]
    fn loopback_bind_detection() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind(""));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("::"));
        assert!(!is_loopback_bind("10.0.0.5"));
    }
}
