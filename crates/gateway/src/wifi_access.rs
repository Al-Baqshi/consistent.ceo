//! Phone access on Wi-Fi: a second listener on this computer's home-network
//! address, so "Pair a phone" works on the same Wi-Fi with nothing to install.
//!
//! # Why a second listener
//!
//! The main listener stays on loopback. Several trust decisions read "bound to
//! loopback" as "only this machine can call me" — the machine token turning
//! into the owner's session (`/api/auth/local-session`, pairing's
//! `owner_user_id`), "every signed-in user is the admin" on a self-hosted
//! install (`server::gateway_is_self_hosted`), and the rate limiters' local
//! exemption. Rebinding the main listener to `0.0.0.0` would quietly hand all of
//! that to anyone on the network. A separate listener lets every request that
//! arrives from the Wi-Fi be told apart from one that arrives from this machine.
//!
//! # What a Wi-Fi request may not do
//!
//! Requests through this listener carry the [`ViaWifi`] extension and pass
//! through [`wifi_guard`] first, which:
//!
//! - refuses everything when the gateway has no auth token (an open API must
//!   never reach the network; the desktop always has a token);
//! - refuses the machine token (the static principal) — it is a credential for
//!   this computer's own shell and CLI, the same rule the relay path enforces;
//! - refuses account sign-up (`/api/auth/register`, `/check-username`): a phone
//!   joins by pairing, which the owner allows on this screen, never by minting
//!   a fresh account from across the room;
//! - stops answering at once when Wi-Fi access is switched off, even on a
//!   connection that was already open.
//!
//! While Wi-Fi access is on, the gateway no longer counts as "self-hosted" for
//! the admin rule (exactly as with the relay), and the rate limiters treat
//! Wi-Fi requests as remote. The owner's own account (`bearer`) stays admin
//! either way, and the phone the owner pairs signs in as that account.
//!
//! # Lifecycle
//!
//! Off until the owner first makes a pairing code with no other route (relay,
//! Tailscale Serve) available, or switches it on explicitly. The choice is
//! remembered in a small `phone_wifi_access` file next to the config and
//! restored at boot. A supervisor re-checks the Wi-Fi address every
//! [`RECHECK`] and rebinds when the laptop moves networks; creating a pairing
//! code re-checks immediately.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
};

use crate::state::ServerState;

/// The file that remembers the owner's choice, next to the config.
pub const FLAG_FILE: &str = "phone_wifi_access";
/// How often the supervisor looks for a changed Wi-Fi address.
const RECHECK: Duration = Duration::from_secs(30);

/// Marker on every request that arrived through the Wi-Fi listener. Set by the
/// server on the way in, so a client cannot add or remove it.
#[derive(Clone, Copy, Debug)]
pub struct ViaWifi;

/// Whether this request came in over the Wi-Fi listener.
pub fn arrived_over_wifi<B>(request: &Request<B>) -> bool {
    request.extensions().get::<ViaWifi>().is_some()
}

/// Paths a Wi-Fi caller may never reach: account sign-up.
fn is_signup_path(path: &str) -> bool {
    matches!(
        path.trim_end_matches('/'),
        "/api/auth/register" | "/api/auth/check-username"
    )
}

#[derive(Clone)]
struct GuardState {
    state: ServerState,
    active: Arc<AtomicBool>,
}

/// Outermost middleware on the Wi-Fi listener. See the module docs.
async fn wifi_guard(
    State(guard): State<GuardState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if !guard.active.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    request.extensions_mut().insert(ViaWifi);
    let Some(secret) = guard.state.auth_token.as_deref() else {
        tracing::warn!("refused a Wi-Fi request: this gateway has no auth token");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    if is_signup_path(request.uri().path()) {
        return Err(StatusCode::FORBIDDEN);
    }
    let allow_principals = guard
        .state
        .app_config
        .as_ref()
        .map(|c| c.gateway_static_principals)
        .unwrap_or(false);
    let bearer = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if let Some(bearer) = bearer
        && crate::server::static_principal_from_token(bearer, secret, allow_principals).is_some()
    {
        tracing::warn!("refused the machine token arriving over Wi-Fi");
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// The router the Wi-Fi listener serves: the gateway's own router behind
/// [`wifi_guard`]. Public so tests can drive it without opening a socket.
pub fn wifi_router(router: Router, state: ServerState, active: Arc<AtomicBool>) -> Router {
    router.layer(middleware::from_fn_with_state(
        GuardState { state, active },
        wifi_guard,
    ))
}

struct Running {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

struct WifiAccess {
    router: Router,
    port: u16,
    flag_path: PathBuf,
    /// True while the owner wants Wi-Fi access on. Read by [`wifi_guard`]
    /// on every request and by [`is_enabled`].
    active: Arc<AtomicBool>,
    running: Mutex<Option<Running>>,
}

static WIFI: OnceLock<WifiAccess> = OnceLock::new();

impl WifiAccess {
    /// Make the listener match the current Wi-Fi address: start it, keep it,
    /// move it, or stop it. Returns where it listens.
    fn reconcile(&self) -> Option<SocketAddr> {
        let mut running = self.running.lock().unwrap();
        if !self.active.load(Ordering::SeqCst) {
            if let Some(r) = running.take() {
                r.task.abort();
            }
            return None;
        }
        let ip = crate::remote_doctor::lan_ip().and_then(|ip| ip.parse::<std::net::IpAddr>().ok());
        if let (Some(ip), Some(r)) = (ip, running.as_ref())
            && r.addr.ip() == ip
            && !r.task.is_finished()
        {
            return Some(r.addr);
        }
        if let Some(r) = running.take() {
            tracing::info!(old = %r.addr, "Wi-Fi address changed; moving the phone listener");
            r.task.abort();
        }
        let ip = ip?;
        let addr = SocketAddr::new(ip, self.port);
        // Bound synchronously so no lock is held across an await.
        let listener = match std::net::TcpListener::bind(addr).and_then(|l| {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)
        }) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(%addr, error = %e, "could not open phone access on Wi-Fi");
                return None;
            }
        };
        let service = self
            .router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, service).await {
                tracing::warn!(error = %e, "phone listener on Wi-Fi stopped");
            }
        });
        tracing::info!(%addr, "phone access on Wi-Fi is listening");
        *running = Some(Running { addr, task });
        Some(addr)
    }

    fn remember(&self, on: bool) {
        let result = if on {
            std::fs::write(&self.flag_path, "on\n")
        } else {
            match std::fs::remove_file(&self.flag_path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        };
        if let Err(e) = result {
            tracing::warn!(path = %self.flag_path.display(), error = %e, "could not remember the Wi-Fi access choice");
        }
    }
}

/// Wire Wi-Fi access into a running gateway. Called once from `main` with the
/// finished router. Does nothing (and Wi-Fi access stays unavailable) when the
/// main listener is not on loopback — it already faces the network — or when
/// there is no auth token to demand.
pub fn install(router: Router, state: ServerState, config_path: &str) {
    let Some(cfg) = state.app_config.clone() else {
        return;
    };
    if !crate::billing::is_loopback_bind(&cfg.gateway_bind) {
        return;
    }
    if state.auth_token.is_none() {
        tracing::warn!("phone access on Wi-Fi is unavailable: no gateway auth token");
        return;
    }
    let flag_path = std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(FLAG_FILE);
    let remembered = std::fs::read_to_string(&flag_path)
        .map(|s| s.trim() == "on")
        .unwrap_or(false);
    let active = Arc::new(AtomicBool::new(remembered));
    let access = WifiAccess {
        router: wifi_router(router, state, active.clone()),
        port: cfg.gateway_port,
        flag_path,
        active,
        running: Mutex::new(None),
    };
    if WIFI.set(access).is_err() {
        return;
    }
    tokio::spawn(async {
        loop {
            if let Some(w) = WIFI.get() {
                w.reconcile();
            }
            tokio::time::sleep(RECHECK).await;
        }
    });
}

/// Whether this gateway can offer Wi-Fi access at all.
pub fn is_available() -> bool {
    WIFI.get().is_some()
}

/// Whether the owner has Wi-Fi access switched on (listening or not).
pub fn is_enabled() -> bool {
    WIFI.get()
        .map(|w| w.active.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Where the Wi-Fi listener is accepting connections right now.
pub fn listening_addr() -> Option<SocketAddr> {
    let w = WIFI.get()?;
    let running = w.running.lock().unwrap();
    running
        .as_ref()
        .filter(|r| !r.task.is_finished())
        .map(|r| r.addr)
}

/// Switch Wi-Fi access on (and remember it), or re-check the address when it
/// is already on. Returns where it listens; `None` when unavailable or when
/// this computer has no Wi-Fi address right now.
pub fn enable() -> Option<SocketAddr> {
    let w = WIFI.get()?;
    if !w.active.swap(true, Ordering::SeqCst) {
        w.remember(true);
    }
    w.reconcile()
}

/// Re-check the address if Wi-Fi access is on; never switches it on.
pub fn refresh() -> Option<SocketAddr> {
    WIFI.get()?.reconcile()
}

/// Switch Wi-Fi access off and forget it. Requests already in flight on an
/// open connection are refused by [`wifi_guard`] from this moment.
pub fn disable() {
    if let Some(w) = WIFI.get() {
        w.active.store(false, Ordering::SeqCst);
        w.remember(false);
        w.reconcile();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signup_paths_are_recognised() {
        assert!(is_signup_path("/api/auth/register"));
        assert!(is_signup_path("/api/auth/register/"));
        assert!(is_signup_path("/api/auth/check-username"));
        assert!(!is_signup_path("/api/auth/login"));
        assert!(!is_signup_path("/api/pair/enroll"));
    }

    #[test]
    fn nothing_is_listening_before_install() {
        // Tests never install the listener, so the global stays empty.
        assert!(!is_available());
        assert!(!is_enabled());
        assert_eq!(listening_addr(), None);
        assert_eq!(enable(), None);
    }
}
