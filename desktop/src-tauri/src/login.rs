//! Loopback browser login — the desktop app's way of getting a real user
//! session instead of holding the gateway's static deploy token.
//!
//! The shape is the GitHub CLI / Warp one, with the device-authorization grant
//! underneath (RFC 8628):
//!
//! 1. The app starts an approval request and gets back a request id and six
//!    digits, which it shows in its own window.
//! 2. The app opens the **system browser** at `/login?approval=<id>`. The user
//!    signs in there — with their password, or a passkey, or a backup code —
//!    and types the six digits they can see in the app.
//! 3. The app polls until the request is approved and collects the session.
//!
//! Everything is `localhost`. The browser talks to the same local gateway the
//! app does, so there is no hosted service, no CORS, no email, and no deep-link
//! handler. A machine with no internet logs in exactly the same way.
//!
//! # Why the verifier lives here
//!
//! A request id travels through a browser URL and a polling loop, so it is not
//! a secret. The PKCE verifier is generated in this process, never leaves it,
//! and is what proves the client collecting the session is the one that asked
//! for it. Keeping it in Rust — rather than handing it to the webview — means a
//! stray script in the page cannot lift it.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

use crate::agent::GatewayAuth;

/// The origin the app points the browser at.
///
/// `localhost`, not `127.0.0.1`, and that is not cosmetic: WebAuthn scopes a
/// credential to an origin, and the gateway's passkey RP id is `localhost`. A
/// passkey enrolled at `http://localhost:18800` simply does not exist at
/// `http://127.0.0.1:18800` — they are different origins to the browser, even
/// though they are the same machine. The webview loads `localhost` for the same
/// reason, so a passkey works in both halves of this flow.
pub const GATEWAY_ORIGIN: &str = "http://localhost:18800";

/// An in-flight login. One at a time: a second attempt replaces the first,
/// which is what a user pressing the button again means.
#[derive(Default)]
pub struct LoginState(Mutex<Option<Pending>>);

struct Pending {
    request_id: String,
    /// Never sent to the webview, and never in a URL.
    verifier: String,
}

/// What the app's window shows while it waits: the code to type in the browser.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStarted {
    pub request_id: String,
    pub code: String,
    pub expires_at: String,
    /// The page the browser was sent to, so the window can offer it again if
    /// the browser failed to open.
    pub verification_url: String,
}

/// Result of one `login_poll` call.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginPoll {
    /// `pending` | `approved` | `denied` | `expired` | `unknown`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Deserialize)]
struct StartResponse {
    #[serde(rename = "requestId")]
    request_id: String,
    code: String,
    #[serde(rename = "expiresAt")]
    expires_at: String,
}

#[derive(Deserialize)]
struct PollResponse {
    status: String,
    #[serde(default)]
    token: Option<String>,
}

/// A URL-safe high-entropy verifier, per RFC 7636 (43–128 unreserved chars).
/// Two v4 UUIDs is 244 bits of CSPRNG output, hex-encoded — comfortably past
/// the 256-bit entropy the RFC asks for and trivially URL-safe.
fn gen_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// PKCE `S256`: base64url-no-pad of the verifier's SHA-256.
fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

/// Open a verification URL in the user's real system browser.
///
/// The webview must not navigate this link itself: on macOS Tauri can treat an
/// ordinary `<a target="_blank">` as a download. Keep this command deliberately
/// narrow so the renderer can only ask us to open the local login page.
fn open_system_browser(url: &str) -> Result<(), String> {
    if !url.starts_with(GATEWAY_ORIGIN) || url.contains('\r') || url.contains('\n') {
        return Err("refusing to open an unexpected verification URL".to_string());
    }
    launch_system_browser(url)
}

/// Hand a URL to the OS default browser, with no check of its own.
///
/// The check belongs to the caller, because each one has a different answer
/// to "which URLs": `open_system_browser` only ever passes the local login
/// page, and `pty::open_signin_url` only the vendor sign-in hosts a terminal
/// flow prints. Keeping the spawn in one place means the OS-specific part is
/// written once and both gates stay narrow.
pub(crate) fn launch_system_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open the system browser: {e}"))
}

/// Re-open the pending login page when the automatic browser launch was
/// blocked or the user closed it.
#[tauri::command]
pub fn open_verification_url(url: String) -> Result<(), String> {
    open_system_browser(&url)
}

/// Sign in as the owner of this computer, with no form and no browser.
///
/// This is the first-run path, and the one almost everybody stays on: you
/// double-clicked an app on your own machine to reach your own data, and being
/// asked to invent a password first would be friction protecting nothing.
///
/// It trades the gateway's local token — which this process already holds, and
/// which the webview used to be handed outright — for an ordinary session. The
/// gateway only honours it on a loopback bind, so nothing about this reaches a
/// hosted deployment.
///
/// Returns `None` when there is no local token to trade (a gateway configured
/// with real accounts), and the caller falls back to the browser flow.
#[tauri::command]
pub async fn login_local(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let auth = app.state::<GatewayAuth>();
    let Some(token) = auth.token() else {
        return Ok(None);
    };
    let res = reqwest::Client::new()
        .post(format!("{GATEWAY_ORIGIN}/api/auth/local-session"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;
    if !res.status().is_success() {
        // 403 is the ordinary answer on a gateway that wants real accounts.
        return Ok(None);
    }
    let session: SessionResponse = res
        .json()
        .await
        .map_err(|e| format!("unexpected reply from the gateway: {e}"))?;

    // From here the app acts as the user, not as the machine.
    auth.set_token(Some(session.token.clone()));
    Ok(Some(session.token))
}

#[derive(Deserialize)]
struct SessionResponse {
    token: String,
}

/// Start a login: register the request, show the code, open the browser.
#[tauri::command]
pub async fn login_start(
    state: State<'_, LoginState>,
) -> Result<LoginStarted, String> {
    let verifier = gen_verifier();
    let challenge = pkce_challenge(&verifier);

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{GATEWAY_ORIGIN}/api/auth/approval/start"))
        .json(&serde_json::json!({
            "context": "Consistent on this computer",
            "codeChallenge": challenge,
        }))
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;
    if !res.status().is_success() {
        return Err(format!(
            "gateway refused the login request: {}",
            res.status()
        ));
    }
    let start: StartResponse = res
        .json()
        .await
        .map_err(|e| format!("unexpected reply from the gateway: {e}"))?;

    let verification_url = format!(
        "{GATEWAY_ORIGIN}/login?approval={}",
        urlencode(&start.request_id)
    );

    if let Ok(mut guard) = state.0.lock() {
        *guard = Some(Pending {
            request_id: start.request_id.clone(),
            verifier,
        });
    }

    // Best-effort: if the browser doesn't open, the window still shows the URL.
    if let Err(e) = open_system_browser(&verification_url) {
        eprintln!("could not open the browser for sign-in: {e}");
    }

    Ok(LoginStarted {
        request_id: start.request_id,
        code: start.code,
        expires_at: start.expires_at,
        verification_url,
    })
}

/// Poll the in-flight login. Returns the session token exactly once, and hands
/// it to `GatewayAuth` on the way out so the app's own gateway calls (spoken
/// commands, the vault) run as the signed-in user rather than as the token.
#[tauri::command]
pub async fn login_poll(
    app: tauri::AppHandle,
    state: State<'_, LoginState>,
) -> Result<LoginPoll, String> {
    let Some((request_id, verifier)) = state.0.lock().ok().and_then(|g| {
        g.as_ref()
            .map(|p| (p.request_id.clone(), p.verifier.clone()))
    }) else {
        return Ok(LoginPoll {
            status: "unknown".to_string(),
            token: None,
        });
    };

    let url = format!(
        "{GATEWAY_ORIGIN}/api/auth/approval/{}?verifier={}",
        urlencode(&request_id),
        urlencode(&verifier)
    );
    let res = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("could not reach the gateway: {e}"))?;
    let poll: PollResponse = res
        .json()
        .await
        .map_err(|e| format!("unexpected reply from the gateway: {e}"))?;

    if let Some(token) = poll.token.as_deref() {
        app.state::<GatewayAuth>()
            .set_token(Some(token.to_string()));
        if let Ok(mut guard) = state.0.lock() {
            *guard = None; // collected; the verifier has done its job
        }
    }
    Ok(LoginPoll {
        status: poll.status,
        token: poll.token,
    })
}

/// Abandon an in-flight login (the user closed the pane).
#[tauri::command]
pub fn login_cancel(state: State<'_, LoginState>) {
    if let Ok(mut guard) = state.0.lock() {
        *guard = None;
    }
}

/// Percent-encode the characters that matter in a query value. The ids and
/// verifiers here are hex, but a URL builder that only works for its current
/// inputs is a bug waiting for the inputs to change.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        // Appendix B's worked example — the same vector the gateway asserts,
        // which is the point: two independent implementations, one answer.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_unique_and_url_safe() {
        let a = gen_verifier();
        let b = gen_verifier();
        assert_ne!(a, b);
        assert!(a.len() >= 43, "RFC 7636 wants at least 43 characters");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }

    #[test]
    fn urlencode_escapes_what_matters() {
        assert_eq!(urlencode("abc-123_x.y~z"), "abc-123_x.y~z");
        assert_eq!(urlencode("a&b=c d"), "a%26b%3Dc%20d");
    }
}
