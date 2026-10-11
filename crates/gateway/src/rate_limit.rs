//! Throttling for the public (unauthenticated) auth routes, in two layers.
//!
//! **Per IP.** `/api/auth/register` seeds trial credits on every new account, so
//! an unthrottled endpoint is a drainable credit faucet on hosted deployments.
//! Loopback binds (self-host / desktop) skip this layer entirely.
//!
//! **Per account.** IP throttling alone does nothing against a distributed guess
//! at one account's password — a thousand hosts each trying twice sail through
//! it. Login and recovery therefore also count failures against the *username*,
//! reusing the escalating backoff the vault already uses for the same problem
//! (see [`crate::vault_throttle`]).
//!
//! # Trusting `X-Forwarded-For`
//!
//! A forwarded header is a claim by whoever connected, and it is only worth
//! anything if that connection came from a proxy you run. Unless the peer is in
//! the configured trusted-proxy list the header is ignored outright — otherwise
//! every client simply picks its own bucket key and the limiter is decorative.

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use dashmap::DashMap;
use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::state::ServerState;
use crate::vault_throttle::UnlockThrottle;

const WINDOW: Duration = Duration::from_secs(60);
const REGISTER_LIMIT: u32 = 5;
const DEFAULT_LIMIT: u32 = 20;
/// Bucket-map size that triggers a sweep of expired windows.
const SWEEP_THRESHOLD: usize = 4096;

// One bucket's state: when its window started and how many hits it has seen.
struct Window {
    started: Instant,
    count: u32,
}

/// A fixed-window rate limiter keyed by an arbitrary string bucket.
pub struct FixedWindowLimiter {
    buckets: DashMap<String, Window>,
}

impl Default for FixedWindowLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl FixedWindowLimiter {
    pub fn new() -> Self {
        Self {
            buckets: DashMap::new(),
        }
    }

    /// Records a hit for `key` and returns false when the per-window limit is
    /// exceeded.
    pub fn allow(&self, key: &str, limit: u32) -> bool {
        let now = Instant::now();
        let mut entry = self.buckets.entry(key.to_string()).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(entry.started) >= WINDOW {
            entry.started = now;
            entry.count = 0;
        }
        entry.count += 1;
        let allowed = entry.count <= limit;
        drop(entry);
        if self.buckets.len() > SWEEP_THRESHOLD {
            self.buckets
                .retain(|_, w| now.duration_since(w.started) < WINDOW);
        }
        allowed
    }
}

/// Per-IP throttle shared by every public auth route.
static AUTH_LIMITER: LazyLock<FixedWindowLimiter> = LazyLock::new(FixedWindowLimiter::new);

/// Per-account lockout for login and recovery. Same escalating backoff the vault
/// uses: a handful of free attempts, then a doubling delay capped at 15 minutes.
static ACCOUNT_THROTTLE: LazyLock<UnlockThrottle> = LazyLock::new(UnlockThrottle::default);

/// Bucket key for a per-account throttle. Normalised the same way usernames are
/// stored, so `Alice` and `alice` share one budget rather than doubling it.
pub fn account_key(action: &str, username: &str) -> String {
    format!("{action}:{}", username.trim().to_lowercase())
}

/// Refuse the attempt if this account is currently locked out.
pub fn check_account(key: &str) -> Result<(), StatusCode> {
    match ACCOUNT_THROTTLE.retry_after(key) {
        Some(_) => Err(StatusCode::TOO_MANY_REQUESTS),
        None => Ok(()),
    }
}

/// Count a failed attempt against the account throttle, escalating its backoff.
pub fn record_account_failure(key: &str) {
    if let Some(delay) = ACCOUNT_THROTTLE.record_failure(key) {
        tracing::warn!(delay_seconds = delay, "auth attempt throttled for account");
    }
}

/// Clear the account throttle's failure streak after a successful attempt.
pub fn record_account_success(key: &str) {
    ACCOUNT_THROTTLE.record_success(key);
}

/// Networks whose `X-Forwarded-For` header is believed. Parsed once from
/// `[gateway] trusted_proxies`; a bare address is treated as a /32 or /128.
fn trusted_proxies(state: &ServerState) -> &[IpNet] {
    state
        .app_config
        .as_ref()
        .map(|cfg| cfg.gateway_trusted_proxies.as_slice())
        .unwrap_or(&[])
}

/// Parse the configured trusted-proxy strings, dropping any that don't parse
/// as a CIDR network or a bare IP address.
pub(crate) fn parse_trusted_proxies(entries: &[String]) -> Vec<IpNet> {
    entries
        .iter()
        .filter_map(|raw| {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            raw.parse::<IpNet>()
                .or_else(|_| raw.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| tracing::warn!(entry = raw, "ignoring unparseable trusted proxy"))
                .ok()
        })
        .collect()
}

/// Client identity for bucketing.
///
/// The socket peer is the only thing the client cannot lie about, so it is the
/// default. `X-Forwarded-For` is honoured only when the peer is a configured
/// trusted proxy, and then it is the *last* hop the proxy appended — the
/// rightmost entry is the address that proxy actually saw, whereas the leftmost
/// is whatever the client put there first.
fn client_ip(request: &Request<Body>, trusted: &[IpNet]) -> String {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());

    if let Some(peer) = peer
        && trusted.iter().any(|net| net.contains(&peer))
        && let Some(forwarded) = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(str::trim)
            .and_then(|v| v.parse::<IpAddr>().ok())
    {
        return forwarded.to_string();
    }

    peer.map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Middleware for `auth_public_routes`. Registration is throttled harder than
/// the other public endpoints because each successful call mints an account
/// (and, on hosted deployments, trial credits).
pub async fn limit_public_auth(
    State(state): State<ServerState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let is_loopback = state
        .app_config
        .as_ref()
        .map(|cfg| crate::billing::is_loopback_bind(&cfg.gateway_bind))
        .unwrap_or(false);
    if is_loopback {
        return Ok(next.run(request).await);
    }
    let path = request.uri().path();
    let (class, limit) = if path.ends_with("/register") {
        ("register", REGISTER_LIMIT)
    } else {
        ("auth", DEFAULT_LIMIT)
    };
    let key = format!("{class}:{}", client_ip(&request, trusted_proxies(&state)));
    if !AUTH_LIMITER.allow(&key, limit) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(next.run(request).await)
}

/// Ceiling for an authenticated principal's calls to the protected API, per
/// minute. Generous — this is not a quota, it is blast-radius containment for
/// a stolen session token now that the gateway can be reached from outside the
/// LAN (tailnet, relay). A human driving the app never approaches it.
const PRINCIPAL_LIMIT: u32 = 240;
/// Streamed chat turns are expensive (a model call each) and no human starts
/// more than a handful a minute.
const PRINCIPAL_STREAM_LIMIT: u32 = 20;

static PRINCIPAL_LIMITER: LazyLock<FixedWindowLimiter> = LazyLock::new(FixedWindowLimiter::new);

/// Per-principal throttle on the authenticated API.
///
/// Layered under `require_bearer_token`, so every request here already carries
/// a resolved principal — the limiter keys on that rather than on an IP, which
/// a phone changes every time it moves between networks.
pub async fn limit_authenticated(
    State(state): State<ServerState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Throttle what came from outside this machine. Two ways that happens: the
    // gateway is bound off-loopback, or it is bound to loopback and reached
    // through the relay tunnel.
    //
    // The earlier version skipped every loopback deployment — which is
    // precisely the configuration the relay creates, so the limiter was dead
    // code in the one case it exists for. Local processes are still exempt:
    // they already have the machine, and throttling them only breaks the
    // desktop shell and CLI.
    let bound_to_loopback = state
        .app_config
        .as_ref()
        .map(|cfg| crate::billing::is_loopback_bind(&cfg.gateway_bind))
        .unwrap_or(false);
    let from_relay = request
        .headers()
        .get(consistent_world_relay::protocol::RELAY_MARKER_HEADER)
        .is_some();
    if bound_to_loopback && !from_relay {
        return Ok(next.run(request).await);
    }
    let principal = request
        .extensions()
        .get::<crate::control_api::AuthContext>()
        .map(|a| a.principal_id.clone())
        .unwrap_or_else(|| client_ip(&request, trusted_proxies(&state)));
    let path = request.uri().path();
    let (class, limit) = if path.ends_with("/stream") {
        ("stream", PRINCIPAL_STREAM_LIMIT)
    } else {
        ("api", PRINCIPAL_LIMIT)
    };
    if !PRINCIPAL_LIMITER.allow(&format!("{class}:{principal}"), limit) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_request_is_throttled_even_on_a_loopback_bind() {
        // The relay keeps the gateway on loopback by design, so "loopback means
        // local" is false exactly when remote reach exists. This asserts the
        // predicate the middleware uses, not the middleware itself (which needs
        // a full ServerState).
        let bound_to_loopback = true;
        for (from_relay, expect_exempt) in [(false, true), (true, false)] {
            let exempt = bound_to_loopback && !from_relay;
            assert_eq!(
                exempt,
                expect_exempt,
                "from_relay={from_relay} must {}be throttled",
                if expect_exempt { "not " } else { "" }
            );
        }
    }

    #[test]
    fn a_principals_stream_budget_is_separate_from_its_api_budget() {
        let l = FixedWindowLimiter::new();
        for _ in 0..PRINCIPAL_STREAM_LIMIT {
            assert!(l.allow("stream:user-1", PRINCIPAL_STREAM_LIMIT));
        }
        assert!(!l.allow("stream:user-1", PRINCIPAL_STREAM_LIMIT));
        // The same user's ordinary API calls are untouched.
        assert!(l.allow("api:user-1", PRINCIPAL_LIMIT));
        // And another principal has its own budget entirely.
        assert!(l.allow("stream:user-2", PRINCIPAL_STREAM_LIMIT));
    }

    #[test]
    fn allows_up_to_limit_then_rejects() {
        let l = FixedWindowLimiter::new();
        for _ in 0..5 {
            assert!(l.allow("register:1.2.3.4", 5));
        }
        assert!(!l.allow("register:1.2.3.4", 5));
        // A different IP has its own budget.
        assert!(l.allow("register:5.6.7.8", 5));
    }

    #[test]
    fn separate_classes_do_not_share_budget() {
        let l = FixedWindowLimiter::new();
        for _ in 0..5 {
            assert!(l.allow("register:1.2.3.4", 5));
        }
        assert!(!l.allow("register:1.2.3.4", 5));
        assert!(l.allow("auth:1.2.3.4", 20));
    }

    fn request_from(peer: &str, forwarded: Option<&str>) -> Request<Body> {
        let mut b = Request::builder();
        if let Some(f) = forwarded {
            b = b.header("x-forwarded-for", f);
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        req
    }

    /// The bug: the header was taken unconditionally, so every client chose its
    /// own bucket key and the limiter throttled nobody.
    #[test]
    fn a_forged_forwarded_header_cannot_pick_its_own_bucket() {
        let req = request_from("203.0.113.9:5000", Some("1.2.3.4"));
        assert_eq!(client_ip(&req, &[]), "203.0.113.9");

        // Not from a trusted proxy — still ignored.
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".to_string()]);
        assert_eq!(client_ip(&req, &trusted), "203.0.113.9");
    }

    #[test]
    fn a_trusted_proxy_is_believed() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".to_string(), "192.0.2.7".to_string()]);
        let via_cidr = request_from("10.1.2.3:443", Some("198.51.100.4"));
        assert_eq!(client_ip(&via_cidr, &trusted), "198.51.100.4");

        // A bare address is a host route.
        let via_host = request_from("192.0.2.7:443", Some("198.51.100.4"));
        assert_eq!(client_ip(&via_host, &trusted), "198.51.100.4");
    }

    /// A client can prepend hops; only what the trusted proxy itself appended —
    /// the rightmost entry — reflects an address that was actually observed.
    #[test]
    fn the_rightmost_hop_wins() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".to_string()]);
        let req = request_from("10.1.2.3:443", Some("1.1.1.1, 2.2.2.2, 198.51.100.4"));
        assert_eq!(client_ip(&req, &trusted), "198.51.100.4");
    }

    #[test]
    fn junk_in_the_header_falls_back_to_the_peer() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".to_string()]);
        let req = request_from("10.1.2.3:443", Some("not-an-ip"));
        assert_eq!(client_ip(&req, &trusted), "10.1.2.3");
    }

    #[test]
    fn unparseable_proxy_entries_are_dropped_not_trusted() {
        let parsed = parse_trusted_proxies(&[
            "10.0.0.0/8".to_string(),
            "definitely not an address".to_string(),
            String::new(),
        ]);
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn account_keys_normalise_the_username() {
        // Otherwise `Alice` and `alice` get one budget each.
        assert_eq!(
            account_key("login", "  Alice "),
            account_key("login", "alice")
        );
        assert_ne!(
            account_key("login", "alice"),
            account_key("recover", "alice")
        );
    }
}
