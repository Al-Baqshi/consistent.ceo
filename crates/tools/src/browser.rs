//! Browser automation via the Chrome DevTools Protocol (chromiumoxide).
//!
//! One browser process and one *current page* are kept alive across tool calls, so a model can
//! `navigate` once and then `click` / `type` / `text` / `screenshot` against that same page. The
//! session lives behind a `Mutex` because the tool is stored as `Box<dyn Tool>` and `execute`
//! only gets `&self`; the lock also serialises actions, which is what we want for a single page.
//!
//! Every action is bounded by a timeout (same style as `shell.rs`) so a hung page cannot wedge
//! the agent, and the allow/deny domain policy (shaped like the one in `http.rs`) is enforced on
//! **every request the page makes**, not just on the URL the model asked for.
//!
//! The policy used to be a guardrail: `navigate` checked the target URL and nothing was checked
//! again afterwards, so a link click, a `location.href = ...`, a `<meta refresh>`, an iframe or
//! any subresource (script, image, XHR, fetch) could reach a denied host freely. It is now a real
//! boundary: the CDP `Fetch` domain is enabled for the lifetime of the page and every paused
//! request is run through the *same* [`check_navigation_url`] the model-facing `navigate` action
//! uses, then either continued or failed with `BlockedByClient`. Blocked URLs are recorded and
//! reported in the action's JSON result, so "the policy stopped this" never looks like "the site
//! is down".
//!
//! **What is enforced** (each verified against a real Chrome by an `#[ignore]`d test below):
//! subresources (script / image / `fetch` / XHR), server-side 3xx redirects off an allowed URL,
//! `location.href` navigations, link clicks, `<meta http-equiv="refresh">`, and requests made
//! from inside cross-site iframes.
//!
//! **What is not** — measured, not guessed, and each pinned by a `known_hole_*` test:
//! * **WebSocket handshakes.** The `Fetch` domain does not pause them and CDP has no verb to
//!   refuse one, so `new WebSocket("ws://denied/")` reaches the network unchecked.
//! * **Popups.** `window.open` creates a separate CDP target with its own session; its first
//!   navigation is not checked. The tool never *drives* popups, but a hostile page can open one.
//! * **Service workers.** Registration never completes here (chromiumoxide detaches from
//!   `service_worker` targets), so an SW never gets far enough to make a request — unverified
//!   rather than proven safe.
//! * **`Browser::connect` (`CONSISTENT_WORLD_BROWSER_CDP_URL`).** Interception is still armed on
//!   the page, but the launch flags below are not ours to set, so the iframe coverage depends on
//!   how that browser was started.

use crate::Tool;
use async_trait::async_trait;
use base64::Engine;
use chromiumoxide::browser::{Browser, BrowserConfig, HeadlessMode};
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams as FetchEnableParams, EventRequestPaused,
    FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::element::Element;
use chromiumoxide::page::{Page, ScreenshotParams};
use consistent_world_common::types::ToolResult;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time;

/// Default per-action timeout, and the ceiling a caller may ask for.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 300;
/// Short timeout for the "is this browser/page still alive?" probes.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Extracted page text is capped so a huge page cannot blow up the model context.
const DEFAULT_MAX_TEXT_CHARS: usize = 50_000;
const HARD_MAX_TEXT_CHARS: usize = 500_000;
/// Screenshots are always written to disk; the base64 copy is only inlined into the JSON result
/// when the PNG is at most this big (768 KiB raw, ~1 MiB once base64-encoded).
const MAX_SCREENSHOT_INLINE_BYTES: usize = 768 * 1024;
/// How long we will wait for Chrome to acknowledge a `Fetch.continueRequest` / `Fetch.failRequest`
/// before giving up on that one request. A paused request that is never answered hangs the page,
/// so answering must never be allowed to block indefinitely.
const INTERCEPT_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// At most this many blocked URLs are listed in an action result (the total count is reported
/// separately when it overflows), so a page that fires hundreds of denied requests cannot flood
/// the model's context.
const MAX_BLOCKED_REPORTED: usize = 25;

/// Monotonic suffix so two screenshots taken in the same nanosecond cannot collide.
static SCREENSHOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// A tool error, rendered as JSON so the model gets a stable machine-readable `code`
/// (same convention as `shell.rs`'s policy errors).
type ActionError = (&'static str, String);

/// The set of actions the tool understands. Parsing is separated from execution so it can be
/// unit-tested without a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Navigate,
    Click,
    Type,
    Evaluate,
    Text,
    WaitFor,
    Screenshot,
    Close,
}

impl Action {
    /// Parse an action name. The four original names (`navigate`, `click`, `type`, `evaluate`,
    /// `screenshot`) keep working; the rest are new actions plus a few obvious aliases, because
    /// models reach for `goto` / `content` / `eval` constantly.
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "navigate" | "goto" | "open" | "visit" => Some(Self::Navigate),
            "click" => Some(Self::Click),
            "type" | "fill" | "input" => Some(Self::Type),
            "evaluate" | "eval" | "script" => Some(Self::Evaluate),
            "text" | "content" | "read" | "extract" => Some(Self::Text),
            "wait_for" | "waitfor" | "wait" => Some(Self::WaitFor),
            "screenshot" | "capture" => Some(Self::Screenshot),
            "close" | "quit" => Some(Self::Close),
            _ => None,
        }
    }

    /// Actions that need a page that is already open. `navigate` creates one, `close` tears
    /// everything down, everything else operates on the current page.
    fn requires_page(self) -> bool {
        !matches!(self, Self::Navigate | Self::Close)
    }
}

/// Per-call policy override injected by the task-execution path (`run_agent.rs` adds a `_policy`
/// object to the tool arguments). Mirrors the camelCase shape used by `shell.rs`/`filesystem.rs`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserPolicyOverride {
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    denied_domains: Vec<String>,
    #[serde(default)]
    screenshot_dir: Option<String>,
}

/// The live domain policy plus the per-action record of what it stopped.
///
/// Shared (via `Arc`) between the action path — which refreshes the policy before every action,
/// because `_policy` can change per call — and the request-interception task, which consults it
/// for every request Chrome pauses. Deliberately `std` locks, never held across an `.await`, so
/// the interception task can make a decision without any chance of blocking on the event pump.
#[derive(Default)]
struct Interceptor {
    policy: RwLock<(Option<Vec<String>>, Vec<String>)>,
    blocked: std::sync::Mutex<BlockedLog>,
}

/// URLs the policy refused during the current action.
#[derive(Default)]
struct BlockedLog {
    /// Distinct URLs, capped at [`MAX_BLOCKED_REPORTED`].
    urls: Vec<String>,
    /// Every refusal, including duplicates and anything past the cap.
    total: usize,
}

impl Interceptor {
    /// Point the interception task at the policy this action is running under.
    fn set_policy(&self, allowed: Option<Vec<String>>, denied: Vec<String>) {
        *self
            .policy
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = (allowed, denied);
    }

    /// The single source of truth for allow/deny. This is the *same* function the model-facing
    /// `navigate` action calls, so the two can never drift: identical scheme rules, identical
    /// host matching, identical deny-beats-allow ordering.
    fn allows(&self, url: &str) -> bool {
        let policy = self
            .policy
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        check_navigation_url(&policy.0, &policy.1, url).is_ok()
    }

    fn record_block(&self, url: &str) {
        let mut log = self
            .blocked
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        log.total += 1;
        if log.urls.len() < MAX_BLOCKED_REPORTED && !log.urls.iter().any(|u| u == url) {
            log.urls.push(url.to_string());
        }
    }

    /// Drain the record. Called once per action, *after* it finishes, so a request refused while
    /// no action was running (a `<meta refresh>` that fires between two tool calls, say) is
    /// reported by the next action instead of being silently dropped.
    fn take_blocked(&self) -> BlockedLog {
        std::mem::take(
            &mut *self
                .blocked
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }
}

/// A live browser plus the page the model is currently working on.
struct BrowserSession {
    browser: Browser,
    /// Pumps the CDP event stream; nothing works if this task is not running.
    handler: JoinHandle<()>,
    page: Option<Page>,
    /// Answers `Fetch.requestPaused` for `page`. Armed when the page is created and torn down
    /// with it; `None` means the page has no boundary and must be re-armed.
    intercept: Option<JoinHandle<()>>,
}

impl BrowserSession {
    /// Graceful shutdown: close the browser (which lets Chrome exit on its own), reap the child,
    /// then stop the event pump. Falls back to SIGKILL if the polite path does not work.
    async fn shutdown(mut self) {
        if let Some(intercept) = self.intercept.take() {
            intercept.abort();
        }
        self.page = None;
        let closed = matches!(
            time::timeout(HEALTH_TIMEOUT, self.browser.close()).await,
            Ok(Ok(_))
        );
        if closed {
            // Reap the child so it cannot linger as a zombie.
            let _ = time::timeout(HEALTH_TIMEOUT, self.browser.wait()).await;
        } else {
            let _ = time::timeout(HEALTH_TIMEOUT, self.browser.kill()).await;
        }
        self.handler.abort();
    }

    /// Synchronous best-effort kill, for use from `Drop` where we cannot await.
    /// `tokio::process::Child::start_kill` is non-async, which is exactly why this crate pins
    /// chromiumoxide to the tokio runtime.
    fn kill_blocking(mut self) {
        if let Some(intercept) = self.intercept.take() {
            intercept.abort();
        }
        self.handler.abort();
        if let Some(child) = self.browser.get_mut_child() {
            let _ = child.inner.start_kill();
        }
    }
}

/// The `browser` tool: launches (or attaches to) a Chrome instance and drives one page across
/// calls, enforcing the domain policy on every request the page makes.
pub struct BrowserTool {
    /// If set, connect to this CDP URL instead of launching a browser.
    pub cdp_url: Option<String>,
    /// Launch headless. Ignored when `cdp_url` is set (we do not own that browser).
    pub headless: bool,
    /// If set, only these domains may be navigated to. `None`/empty means no allowlist.
    pub allowed_domains: Option<Vec<String>>,
    /// Domains that are always blocked.
    pub denied_domains: Vec<String>,
    /// Where screenshots are written. Defaults to the system temp dir.
    pub screenshot_dir: Option<String>,
    session: Mutex<Option<BrowserSession>>,
    /// Live policy + blocked-request record, shared with the interception task.
    interceptor: Arc<Interceptor>,
}

impl BrowserTool {
    /// `cdp_url` overrides `CONSISTENT_WORLD_BROWSER_CDP_URL` when set; headless mode and the
    /// screenshot directory are read from environment/defaults and can still be set afterwards.
    pub fn new(
        cdp_url: Option<String>,
        allowed_domains: Option<Vec<String>>,
        denied_domains: Vec<String>,
    ) -> Self {
        let cdp_url = cdp_url.or_else(|| env_non_empty("CONSISTENT_WORLD_BROWSER_CDP_URL"));
        let headless = env_non_empty("CONSISTENT_WORLD_BROWSER_HEADLESS")
            .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        Self {
            cdp_url,
            headless,
            allowed_domains,
            denied_domains,
            screenshot_dir: None,
            session: Mutex::new(None),
            interceptor: Arc::new(Interceptor::default()),
        }
    }

    /// Allow/deny lists after applying a per-call `_policy` override. An allowlist in the
    /// override replaces the configured one; denials are always additive (tighter wins).
    fn effective_domains(
        &self,
        policy: Option<&BrowserPolicyOverride>,
    ) -> (Option<Vec<String>>, Vec<String>) {
        let mut allowed = self.allowed_domains.clone();
        let mut denied = self.denied_domains.clone();
        if let Some(p) = policy {
            if let Some(a) = &p.allowed_domains
                && !a.is_empty()
            {
                allowed = Some(a.clone());
            }
            denied.extend(p.denied_domains.iter().cloned());
        }
        (allowed, denied)
    }

    fn screenshot_dir(&self, policy: Option<&BrowserPolicyOverride>) -> PathBuf {
        policy
            .and_then(|p| p.screenshot_dir.clone())
            .or_else(|| self.screenshot_dir.clone())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    async fn launch(&self) -> Result<BrowserSession, ActionError> {
        let (browser, mut handler) = if let Some(url) = &self.cdp_url {
            Browser::connect(url.clone()).await.map_err(|e| {
                (
                    "BROWSER_LAUNCH_FAILED",
                    format!("could not connect to CDP endpoint {url}: {e}"),
                )
            })?
        } else {
            let mut builder = BrowserConfig::builder().headless_mode(if self.headless {
                HeadlessMode::True
            } else {
                HeadlessMode::False
            });
            if let Some(exe) = env_non_empty("CONSISTENT_WORLD_BROWSER_EXECUTABLE") {
                builder = builder.chrome_executable(exe);
            }
            if env_non_empty("CONSISTENT_WORLD_BROWSER_NO_SANDBOX").is_some() {
                builder = builder.no_sandbox();
            }
            // Keep the whole frame tree in one renderer/target. `Fetch` interception is armed on
            // the page's CDP session; with site isolation on, a cross-site iframe becomes an
            // out-of-process iframe with its *own* target and session, and requests it makes
            // after it has loaded never reach our interception task. That is not a theory: with
            // these flags absent, `interception_covers_requests_made_inside_a_cross_site_iframe`
            // fails and the denied request lands on the fixture server. Both flags are needed —
            // `--disable-features=...` alone did not disable it on Chrome 150.
            //
            // This trades a Chrome-internal *renderer* boundary for a complete *network* one.
            // `TranslateUI` is repeated because chromiumoxide's own default args disable it via
            // a `--disable-features` switch that this one supersedes.
            builder = builder.args([
                "--disable-features=IsolateOrigins,site-per-process,TranslateUI",
                "--disable-site-isolation-trials",
            ]);
            // A dedicated profile directory, never the operator's own Chrome
            // profile: sessions the operator deliberately signs into here
            // persist across runs, while their real cookies, passwords, and
            // logged-in tabs stay out of an unattended agent's reach.
            if let Some(dir) = env_non_empty("CONSISTENT_WORLD_BROWSER_PROFILE_DIR") {
                builder = builder.user_data_dir(dir);
            }
            let config = builder.build().map_err(|e| {
                (
                    "BROWSER_LAUNCH_FAILED",
                    format!("invalid browser config: {e}"),
                )
            })?;
            Browser::launch(config).await.map_err(|e| {
                (
                    "BROWSER_LAUNCH_FAILED",
                    format!(
                        "could not launch Chrome: {e}. Install Chrome/Chromium, or set \
                         CONSISTENT_WORLD_BROWSER_EXECUTABLE / CONSISTENT_WORLD_BROWSER_CDP_URL."
                    ),
                )
            })?
        };

        // Pump the CDP event stream for the life of the browser. Individual errors must NOT stop
        // the loop: chromiumoxide 0.7 fails to deserialize some perfectly normal Chrome events
        // ("data did not match any variant of untagged enum Message"), and the previous version
        // of this tool broke out of the loop on the first one. Once the pump stops, every later
        // CDP command hangs forever waiting on a response that can never arrive. Only a closed
        // stream (`None`) means the browser is actually gone.
        let handler = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(e) = event {
                    tracing::debug!(error = %e, "ignoring CDP handler event error");
                }
            }
            tracing::debug!("CDP handler stream ended; browser connection closed");
        });

        Ok(BrowserSession {
            browser,
            handler,
            page: None,
            intercept: None,
        })
    }

    /// Open a fresh tab with request interception armed *before* anything can be loaded into it.
    ///
    /// The page is deliberately created at `about:blank` (which makes no network requests) rather
    /// than at the target URL: the interception task has to be listening before the first byte is
    /// requested, otherwise the very first navigation — the one most worth checking — races the
    /// arming and either escapes the policy or hangs paused with nobody to answer it.
    async fn open_intercepted_page(
        &self,
        session: &mut BrowserSession,
    ) -> Result<Page, ActionError> {
        if let Some(old) = session.intercept.take() {
            old.abort();
        }
        session.page = None;

        let page = session.browser.new_page("about:blank").await.map_err(|e| {
            (
                "BROWSER_LAUNCH_FAILED",
                format!("could not open a browser tab: {e}"),
            )
        })?;
        let intercept = arm_interception(&page, self.interceptor.clone()).await?;
        session.intercept = Some(intercept);
        Ok(page)
    }

    /// Return a live session, relaunching if the previous browser died. This is what stops a
    /// crashed Chrome from wedging the tool forever.
    async fn ensure_session<'a>(
        &self,
        slot: &'a mut Option<BrowserSession>,
    ) -> Result<&'a mut BrowserSession, ActionError> {
        let stale = match slot.as_ref() {
            Some(s) => !browser_alive(&s.browser).await,
            None => false,
        };
        if stale && let Some(dead) = slot.take() {
            tracing::warn!("browser session is dead; relaunching");
            dead.shutdown().await;
        }
        if slot.is_none() {
            *slot = Some(self.launch().await?);
        }
        Ok(slot.as_mut().expect("session was just populated"))
    }
}

/// Killing the child Chrome is the whole point of this impl: the previous version dropped the
/// `Browser` with no explicit shutdown, leaving cleanup to chromiumoxide's `kill_on_drop`
/// backstop (and logging a warning every time).
impl Drop for BrowserTool {
    fn drop(&mut self) {
        // `try_lock` cannot fail in practice: `&mut self` means no other borrow is executing.
        let Ok(mut slot) = self.session.try_lock() else {
            return;
        };
        let Some(session) = slot.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            // Inside a runtime we can still do the polite close (and reap the child).
            Ok(handle) => {
                handle.spawn(session.shutdown());
            }
            Err(_) => session.kill_blocking(),
        }
    }
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Turn the domain policy into a real boundary for `page`.
///
/// Subscribes to `Fetch.requestPaused`, then enables the `Fetch` domain with a catch-all pattern.
/// The order matters: the listener is registered first so no paused request can arrive before
/// anyone is listening for it.
///
/// The returned task owns the answering. Two rules keep it from wedging the browser:
///
/// * it runs on its own task, so it never shares a thread of control with the CDP event pump —
///   the pump only ever pushes into unbounded channels and can never be blocked by a decision;
/// * the reply for each request is sent from a short-lived detached task with its own timeout, so
///   one slow or failed `continueRequest` cannot stop the next request from being answered. A
///   paused request that is never answered stalls the page forever, which is exactly the class of
///   bug that already bit this module once (a dead event pump made every command hang).
async fn arm_interception(
    page: &Page,
    interceptor: Arc<Interceptor>,
) -> Result<JoinHandle<()>, ActionError> {
    let mut paused = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|e| {
            (
                "BROWSER_INTERCEPT_FAILED",
                format!("could not subscribe to intercepted requests: {e}"),
            )
        })?;

    let page_for_task = page.clone();
    let task = tokio::spawn(async move {
        while let Some(event) = paused.next().await {
            let url = event.request.url.clone();
            let request_id = event.request_id.clone();
            let allow = interceptor.allows(&url);
            if !allow {
                interceptor.record_block(&url);
                tracing::debug!(url = %url, "browser policy blocked a request");
            }
            let page = page_for_task.clone();
            tokio::spawn(async move {
                let reply = async {
                    if allow {
                        page.execute(ContinueRequestParams::new(request_id))
                            .await
                            .map(|_| ())
                    } else {
                        page.execute(FailRequestParams::new(
                            request_id,
                            ErrorReason::BlockedByClient,
                        ))
                        .await
                        .map(|_| ())
                    }
                };
                match time::timeout(INTERCEPT_REPLY_TIMEOUT, reply).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::debug!(url = %url, error = %e, "could not answer paused request")
                    }
                    Err(_) => {
                        tracing::warn!(url = %url, "timed out answering an intercepted request")
                    }
                }
            });
        }
        tracing::debug!("request interception stream ended");
    });

    // Catch-all pattern at the Request stage: every scheme, every resource type, checked before
    // the fetch actually happens.
    if let Err(e) = page
        .execute(
            FetchEnableParams::builder()
                .pattern(RequestPattern::builder().url_pattern("*").build())
                .build(),
        )
        .await
    {
        // Without `Fetch.enable` the task would sit on a stream that never yields; more
        // importantly, the caller must not be handed a page it believes is guarded.
        task.abort();
        return Err((
            "BROWSER_INTERCEPT_FAILED",
            format!("could not enable request interception: {e}"),
        ));
    }

    Ok(task)
}

async fn browser_alive(browser: &Browser) -> bool {
    matches!(
        time::timeout(HEALTH_TIMEOUT, browser.version()).await,
        Ok(Ok(_))
    )
}

async fn page_alive(page: &Page) -> bool {
    matches!(time::timeout(HEALTH_TIMEOUT, page.url()).await, Ok(Ok(_)))
}

/// Host matching copied from `http.rs`: exact match or a dot-suffix match, case-insensitive,
/// deny list checked first.
fn host_allowed(allowed: &Option<Vec<String>>, denied: &[String], host: &str) -> bool {
    let host_lower = host.to_lowercase();
    let matches = |pattern: &String| {
        let p = pattern.trim().to_lowercase();
        !p.is_empty() && (host_lower == p || host_lower.ends_with(&format!(".{}", p)))
    };
    if denied.iter().any(matches) {
        return false;
    }
    if let Some(allow) = allowed
        && !allow.iter().any(|a| !a.trim().is_empty())
    {
        // An allowlist of only blank entries is treated as "no allowlist".
        return true;
    }
    if let Some(allow) = allowed
        && !allow.is_empty()
        && !allow.iter().any(matches)
    {
        return false;
    }
    true
}

/// Validate (and lightly normalise) a navigation target before any browser work happens, so a
/// blocked URL never launches Chrome.
fn check_navigation_url(
    allowed: &Option<Vec<String>>,
    denied: &[String],
    raw: &str,
) -> Result<String, ActionError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err((
            "BROWSER_MISSING_ARGUMENT",
            "the `navigate` action requires a non-empty `url`".to_string(),
        ));
    }

    // Models routinely pass a bare host; treat it as https rather than failing.
    let candidate = match raw.parse::<reqwest::Url>() {
        Ok(u) => u,
        Err(_) if !raw.contains("://") && !raw.contains(' ') => format!("https://{raw}")
            .parse::<reqwest::Url>()
            .map_err(|e| ("BROWSER_INVALID_URL", format!("invalid URL `{raw}`: {e}")))?,
        Err(e) => {
            return Err(("BROWSER_INVALID_URL", format!("invalid URL `{raw}`: {e}")));
        }
    };

    match candidate.scheme() {
        // No network access, and `about:blank` is needed for a blank tab.
        "about" | "data" => Ok(candidate.to_string()),
        // Reading local files through the browser is only allowed when no allowlist is in force.
        "file" => {
            if allowed.as_ref().is_some_and(|a| !a.is_empty()) {
                Err((
                    "BROWSER_DOMAIN_DENIED",
                    "file:// URLs are not allowed while a domain allowlist is configured"
                        .to_string(),
                ))
            } else {
                Ok(candidate.to_string())
            }
        }
        "http" | "https" => {
            let Some(host) = candidate.host_str() else {
                return Err(("BROWSER_INVALID_URL", format!("URL has no host: {raw}")));
            };
            if !host_allowed(allowed, denied, host) {
                return Err((
                    "BROWSER_DOMAIN_DENIED",
                    format!("navigation to `{host}` is blocked by the browser domain policy"),
                ));
            }
            Ok(candidate.to_string())
        }
        other => Err((
            "BROWSER_SCHEME_DENIED",
            format!("`{other}:` URLs are not allowed; use http, https, file, data or about"),
        )),
    }
}

fn error_result(code: &str, message: &str) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: json!({ "type": "error", "code": code, "message": message }).to_string(),
        is_error: true,
    }
}

/// Fold the per-action blocked-request record into a result body.
///
/// `blocked` is always present (an empty array included) on any action that touched a page: an
/// absent key would be indistinguishable from "nothing was blocked", and the whole point is that
/// an operator reading a transcript can tell a policy refusal from a broken site. `blocked_count`
/// only appears when more requests were refused than are listed.
fn blocked_error_result(code: &str, message: &str, log: BlockedLog) -> ToolResult {
    let mut value = json!({ "type": "error", "code": code, "message": message });
    attach_blocked(&mut value, log);
    ToolResult {
        call_id: String::new(),
        output: value.to_string(),
        is_error: true,
    }
}

fn attach_blocked(value: &mut Value, log: BlockedLog) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let listed = log.urls.len();
    obj.insert("blocked".to_string(), json!(log.urls));
    if log.total > listed {
        obj.insert("blocked_count".to_string(), json!(log.total));
    }
}

fn ok_result(value: Value) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: serde_json::to_string(&value).unwrap_or_else(|_| value.to_string()),
        is_error: false,
    }
}

fn required_str<'a>(arguments: &'a Value, key: &str, action: &str) -> Result<&'a str, ActionError> {
    match arguments.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err((
            "BROWSER_MISSING_ARGUMENT",
            format!("the `{action}` action requires a non-empty `{key}`"),
        )),
    }
}

/// Poll for a selector until it exists or the deadline passes.
async fn wait_for_element(
    page: &Page,
    selector: &str,
    deadline: Instant,
) -> Result<Element, ActionError> {
    loop {
        match page.find_element(selector).await {
            Ok(el) => return Ok(el),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err((
                        "BROWSER_ELEMENT_NOT_FOUND",
                        format!(
                            "no element matched selector `{selector}` before the timeout ({e})"
                        ),
                    ));
                }
            }
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

async fn page_url(page: &Page) -> String {
    page.url().await.ok().flatten().unwrap_or_default()
}

async fn page_title(page: &Page) -> String {
    page.get_title().await.ok().flatten().unwrap_or_default()
}

impl BrowserTool {
    /// Run one action against the (already validated) session slot.
    async fn run_action(
        &self,
        slot: &mut Option<BrowserSession>,
        action: Action,
        arguments: &Value,
        policy: Option<&BrowserPolicyOverride>,
        deadline: Instant,
    ) -> Result<Value, ActionError> {
        if action == Action::Close {
            return match slot.take() {
                Some(session) => {
                    session.shutdown().await;
                    Ok(json!({ "action": "close", "closed": true }))
                }
                None => Ok(json!({
                    "action": "close",
                    "closed": false,
                    "note": "no browser was running"
                })),
            };
        }

        // Publish the policy this action runs under *before* any page work: `_policy` can differ
        // per call, and the interception task has to be checking against the same lists the
        // action itself would.
        let (allowed, denied) = self.effective_domains(policy);
        self.interceptor.set_policy(allowed.clone(), denied.clone());

        if action == Action::Navigate {
            let url = check_navigation_url(
                &allowed,
                &denied,
                arguments.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            )?;
            let session = self.ensure_session(slot).await?;

            // Reuse the current tab when it is still alive *and* still guarded, so repeated
            // navigation does not accumulate tabs. A page whose interception task has exited is
            // treated as unusable: continuing on it would silently drop the boundary.
            let reusable = match session.page.as_ref() {
                Some(p) => {
                    session.intercept.as_ref().is_some_and(|t| !t.is_finished())
                        && page_alive(p).await
                }
                None => false,
            };
            let page = if reusable {
                let page = session.page.clone().expect("page checked above");
                page.goto(url.clone()).await.map_err(|e| {
                    (
                        "BROWSER_NAVIGATION_FAILED",
                        format!("navigation to {url} failed: {e}"),
                    )
                })?;
                page
            } else {
                let page = self.open_intercepted_page(session).await?;
                page.goto(url.clone()).await.map_err(|e| {
                    (
                        "BROWSER_NAVIGATION_FAILED",
                        format!("navigation to {url} failed: {e}"),
                    )
                })?;
                page
            };

            // A failed load-event wait is not fatal (some pages never fully settle); report it.
            let settled = page.wait_for_navigation().await.is_ok();
            session.page = Some(page.clone());
            return Ok(json!({
                "action": "navigate",
                "url": page_url(&page).await,
                "title": page_title(&page).await,
                "load_event": settled,
            }));
        }

        // Every remaining action needs the current page. `execute` has already guaranteed a
        // session exists, so this only fails when the page itself went away.
        let session = slot.as_mut().ok_or_else(no_page_error)?;
        let page = session.page.clone().ok_or_else(no_page_error)?;
        if !page_alive(&page).await {
            session.page = None;
            if let Some(intercept) = session.intercept.take() {
                intercept.abort();
            }
            return Err((
                "BROWSER_PAGE_GONE",
                "the current page was closed or crashed; run the `navigate` action again"
                    .to_string(),
            ));
        }

        match action {
            Action::Click => {
                let selector = required_str(arguments, "selector", "click")?;
                let element = wait_for_element(&page, selector, deadline).await?;
                let _ = element.scroll_into_view().await;
                element.click().await.map_err(|e| {
                    (
                        "BROWSER_CLICK_FAILED",
                        format!("click on `{selector}` failed: {e}"),
                    )
                })?;
                // Give a click that triggers navigation or a DOM update a moment to land.
                time::sleep(Duration::from_millis(250)).await;
                Ok(json!({
                    "action": "click",
                    "selector": selector,
                    "clicked": true,
                    "url": page_url(&page).await,
                }))
            }
            Action::Type => {
                let selector = required_str(arguments, "selector", "type")?;
                let text = arguments.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let clear = arguments
                    .get("clear")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let element = wait_for_element(&page, selector, deadline).await?;
                let _ = element.scroll_into_view().await;
                element.click().await.map_err(|e| {
                    (
                        "BROWSER_TYPE_FAILED",
                        format!("could not focus `{selector}`: {e}"),
                    )
                })?;
                if clear {
                    element
                        .call_js_fn("function() { this.value = ''; }", false)
                        .await
                        .map_err(|e| {
                            (
                                "BROWSER_TYPE_FAILED",
                                format!("could not clear `{selector}`: {e}"),
                            )
                        })?;
                }
                element.type_str(text).await.map_err(|e| {
                    (
                        "BROWSER_TYPE_FAILED",
                        format!("typing into `{selector}` failed: {e}"),
                    )
                })?;
                Ok(json!({
                    "action": "type",
                    "selector": selector,
                    "typed_chars": text.chars().count(),
                    "cleared": clear,
                }))
            }
            Action::Evaluate => {
                let script = required_str(arguments, "script", "evaluate")?;
                let result = page
                    .evaluate(script)
                    .await
                    .map_err(|e| ("BROWSER_EVALUATE_FAILED", format!("evaluate failed: {e}")))?;
                Ok(json!({
                    "action": "evaluate",
                    "value": result.value().cloned().unwrap_or(Value::Null),
                }))
            }
            Action::Text => {
                let max_chars = arguments
                    .get("max_chars")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(DEFAULT_MAX_TEXT_CHARS)
                    .min(HARD_MAX_TEXT_CHARS);
                let result = page
                    .evaluate(
                        "document.body ? document.body.innerText : (document.documentElement ? document.documentElement.innerText : '')",
                    )
                    .await
                    .map_err(|e| ("BROWSER_TEXT_FAILED", format!("text extraction failed: {e}")))?;
                let full = result
                    .value()
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let total = full.chars().count();
                let truncated = total > max_chars;
                let text: String = if truncated {
                    full.chars().take(max_chars).collect()
                } else {
                    full
                };
                Ok(json!({
                    "action": "text",
                    "url": page_url(&page).await,
                    "title": page_title(&page).await,
                    "chars": text.chars().count(),
                    "truncated": truncated,
                    "text": text,
                }))
            }
            Action::WaitFor => {
                let selector = required_str(arguments, "selector", "wait_for")?;
                wait_for_element(&page, selector, deadline).await?;
                Ok(json!({
                    "action": "wait_for",
                    "selector": selector,
                    "found": true,
                }))
            }
            Action::Screenshot => {
                let full_page = arguments
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let params = ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .full_page(full_page)
                    .build();
                let bytes = page.screenshot(params).await.map_err(|e| {
                    (
                        "BROWSER_SCREENSHOT_FAILED",
                        format!("screenshot failed: {e}"),
                    )
                })?;

                let path = match arguments.get("path").and_then(|v| v.as_str()) {
                    Some(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
                    _ => {
                        let seq = SCREENSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        self.screenshot_dir(policy)
                            .join(format!("browser-screenshot-{stamp}-{seq}.png"))
                    }
                };
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, &bytes).map_err(|e| {
                    (
                        "BROWSER_SCREENSHOT_FAILED",
                        format!("could not write screenshot to {}: {e}", path.display()),
                    )
                })?;

                let mut out = json!({
                    "action": "screenshot",
                    "path": path.to_string_lossy(),
                    "format": "png",
                    "full_page": full_page,
                    "bytes": bytes.len(),
                    "url": page_url(&page).await,
                });
                if bytes.len() <= MAX_SCREENSHOT_INLINE_BYTES {
                    out["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
                } else {
                    out["base64_omitted"] = json!(format!(
                        "image is {} bytes (> {} byte inline cap); read it from `path` instead",
                        bytes.len(),
                        MAX_SCREENSHOT_INLINE_BYTES
                    ));
                }
                Ok(out)
            }
            Action::Navigate | Action::Close => unreachable!("handled above"),
        }
    }
}

fn no_page_error() -> ActionError {
    (
        "BROWSER_NO_PAGE",
        "no page is open: run the `navigate` action with a `url` first, then retry this action"
            .to_string(),
    )
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &'static str {
        "browser"
    }

    fn description(&self) -> &'static str {
        "Drive a real Chrome browser over CDP. The browser and the current page persist across \
         calls: `navigate` first, then `text` to read the page, `click`/`type` to interact, \
         `evaluate` to run JS, `wait_for` to await an element, `screenshot` to capture it, and \
         `close` to shut the browser down. Results are JSON. A domain allow/deny policy is \
         enforced on every request the page makes — not just the URL you navigate to, but also \
         redirects, link clicks, JS navigations, iframes and subresources. Anything it refuses is \
         listed in the result's `blocked` array, so a missing script or a navigation that did not \
         happen can be told apart from a broken site."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "click", "type", "text", "evaluate", "wait_for", "screenshot", "close"],
                    "description": "navigate: open a URL (required first). text: extract visible page text. click/type: act on a CSS selector. evaluate: run JS and return its value. wait_for: block until a selector appears. screenshot: PNG to disk (+ base64). close: shut the browser down."
                },
                "url": { "type": "string", "description": "navigate: URL to open (http/https/file/data/about)" },
                "selector": { "type": "string", "description": "click/type/wait_for: CSS selector" },
                "text": { "type": "string", "description": "type: text to enter" },
                "clear": { "type": "boolean", "description": "type: clear the field first (default false)" },
                "script": { "type": "string", "description": "evaluate: JavaScript expression or function" },
                "max_chars": { "type": "integer", "description": "text: cap on returned characters (default 50000)" },
                "full_page": { "type": "boolean", "description": "screenshot: capture beyond the viewport (default false)" },
                "path": { "type": "string", "description": "screenshot: explicit output file path" },
                "timeout_secs": { "type": "integer", "description": "Per-action timeout in seconds (default 30, max 300)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(action_raw) = arguments.get("action").and_then(|v| v.as_str()) else {
            return Ok(error_result(
                "BROWSER_MISSING_ARGUMENT",
                "browser tool requires an `action`",
            ));
        };
        let Some(action) = Action::parse(action_raw) else {
            return Ok(error_result(
                "BROWSER_UNKNOWN_ACTION",
                &format!(
                    "unknown action `{action_raw}`; expected one of navigate, click, type, text, \
                     evaluate, wait_for, screenshot, close"
                ),
            ));
        };

        let policy = arguments
            .get("_policy")
            .cloned()
            .and_then(|v| serde_json::from_value::<BrowserPolicyOverride>(v).ok());

        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let duration = Duration::from_secs(timeout_secs);
        let deadline = Instant::now() + duration;

        let mut slot = self.session.lock().await;

        // Fail fast, without launching a browser, when the model tries to act on a page that was
        // never opened. The old code silently opened a blank tab here, which is why click/type/
        // evaluate/screenshot appeared to succeed while doing nothing.
        if action.requires_page() && slot.as_ref().and_then(|s| s.page.as_ref()).is_none() {
            let (code, message) = no_page_error();
            return Ok(error_result(code, &message));
        }

        let result = time::timeout(
            duration,
            self.run_action(&mut slot, action, &arguments, policy.as_ref(), deadline),
        )
        .await;

        // Report what the policy stopped, on success *and* on failure — a `wait_for` that times
        // out because the script that would have created the element was blocked is precisely
        // where the model needs to see the blocked list.
        // Always drain, so a refusal never carries over into an unrelated later action; only
        // `close` (which does not touch a page) omits the field.
        let log = self.interceptor.take_blocked();
        if action == Action::Close {
            return Ok(match result {
                Ok(Ok(value)) => ok_result(value),
                Ok(Err((code, message))) => error_result(code, &message),
                Err(_) => error_result(
                    "BROWSER_TIMEOUT",
                    &format!("browser action `{action_raw}` timed out after {timeout_secs}s"),
                ),
            });
        }

        Ok(match result {
            Ok(Ok(mut value)) => {
                attach_blocked(&mut value, log);
                ok_result(value)
            }
            Ok(Err((code, message))) => blocked_error_result(code, &message, log),
            Err(_) => blocked_error_result(
                "BROWSER_TIMEOUT",
                &format!("browser action `{action_raw}` timed out after {timeout_secs}s"),
                log,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BrowserTool {
        BrowserTool::new(None, None, Vec::new())
    }

    fn parse_output(result: &ToolResult) -> Value {
        serde_json::from_str(&result.output).expect("tool output should be JSON")
    }

    #[test]
    fn parses_every_documented_action_and_alias() {
        assert_eq!(Action::parse("navigate"), Some(Action::Navigate));
        assert_eq!(Action::parse("goto"), Some(Action::Navigate));
        assert_eq!(Action::parse("click"), Some(Action::Click));
        assert_eq!(Action::parse("type"), Some(Action::Type));
        assert_eq!(Action::parse("fill"), Some(Action::Type));
        assert_eq!(Action::parse("evaluate"), Some(Action::Evaluate));
        assert_eq!(Action::parse("eval"), Some(Action::Evaluate));
        assert_eq!(Action::parse("text"), Some(Action::Text));
        assert_eq!(Action::parse("content"), Some(Action::Text));
        assert_eq!(Action::parse("wait_for"), Some(Action::WaitFor));
        assert_eq!(Action::parse("screenshot"), Some(Action::Screenshot));
        assert_eq!(Action::parse("close"), Some(Action::Close));
        // Case and padding insensitive.
        assert_eq!(Action::parse("  NAVIGATE "), Some(Action::Navigate));
        assert_eq!(Action::parse("frobnicate"), None);
    }

    #[test]
    fn only_navigate_and_close_may_run_without_a_page() {
        assert!(!Action::Navigate.requires_page());
        assert!(!Action::Close.requires_page());
        for a in [
            Action::Click,
            Action::Type,
            Action::Evaluate,
            Action::Text,
            Action::WaitFor,
            Action::Screenshot,
        ] {
            assert!(a.requires_page(), "{a:?} should require a page");
        }
    }

    #[test]
    fn host_allowed_matches_http_tool_semantics() {
        // No policy: everything passes.
        assert!(host_allowed(&None, &[], "example.com"));
        // Deny list wins, including subdomains and mixed case.
        let denied = vec!["localhost".to_string(), "evil.com".to_string()];
        assert!(!host_allowed(&None, &denied, "localhost"));
        assert!(!host_allowed(&None, &denied, "api.evil.com"));
        assert!(!host_allowed(&None, &denied, "API.EVIL.COM"));
        assert!(host_allowed(&None, &denied, "notevil.com"));
        // Allow list restricts everything else.
        let allowed = Some(vec!["example.com".to_string()]);
        assert!(host_allowed(&allowed, &[], "example.com"));
        assert!(host_allowed(&allowed, &[], "docs.example.com"));
        assert!(!host_allowed(&allowed, &[], "example.com.evil.net"));
        assert!(!host_allowed(&allowed, &[], "other.org"));
        // Deny beats allow.
        assert!(!host_allowed(
            &allowed,
            &["example.com".to_string()],
            "example.com"
        ));
    }

    #[test]
    fn navigation_url_policy() {
        // Schemes with no network reach are always fine.
        assert!(check_navigation_url(&None, &[], "about:blank").is_ok());
        assert!(check_navigation_url(&None, &[], "data:text/html,<h1>hi</h1>").is_ok());
        // Bare hosts are normalised to https rather than rejected.
        let normalised = check_navigation_url(&None, &[], "example.com").expect("should normalise");
        assert!(
            normalised.starts_with("https://example.com"),
            "{normalised}"
        );
        // Deny list.
        let err = check_navigation_url(&None, &["evil.com".to_string()], "https://evil.com/x")
            .expect_err("denied host must fail");
        assert_eq!(err.0, "BROWSER_DOMAIN_DENIED");
        // Allow list.
        let allow = Some(vec!["example.com".to_string()]);
        assert!(check_navigation_url(&allow, &[], "https://example.com/a").is_ok());
        assert_eq!(
            check_navigation_url(&allow, &[], "https://other.org")
                .expect_err("off-allowlist host must fail")
                .0,
            "BROWSER_DOMAIN_DENIED"
        );
        // file:// is allowed only without an allowlist.
        assert!(check_navigation_url(&None, &[], "file:///tmp/x.html").is_ok());
        assert_eq!(
            check_navigation_url(&allow, &[], "file:///tmp/x.html")
                .expect_err("file must be blocked under an allowlist")
                .0,
            "BROWSER_DOMAIN_DENIED"
        );
        // Dangerous schemes are refused outright.
        assert_eq!(
            check_navigation_url(&None, &[], "javascript:alert(1)")
                .expect_err("javascript: must fail")
                .0,
            "BROWSER_SCHEME_DENIED"
        );
        // Empty URL.
        assert_eq!(
            check_navigation_url(&None, &[], "  ")
                .expect_err("empty url must fail")
                .0,
            "BROWSER_MISSING_ARGUMENT"
        );
    }

    #[test]
    fn policy_override_replaces_allowlist_and_unions_denylist() {
        let t = BrowserTool::new(
            None,
            Some(vec!["a.com".to_string()]),
            vec!["bad.com".to_string()],
        );
        let policy: BrowserPolicyOverride = serde_json::from_value(json!({
            "allowedDomains": ["b.com"],
            "deniedDomains": ["worse.com"],
            "screenshotDir": "/tmp/shots"
        }))
        .expect("camelCase policy should deserialize");
        let (allowed, denied) = t.effective_domains(Some(&policy));
        assert_eq!(allowed, Some(vec!["b.com".to_string()]));
        assert!(denied.contains(&"bad.com".to_string()));
        assert!(denied.contains(&"worse.com".to_string()));
        assert_eq!(t.screenshot_dir(Some(&policy)), PathBuf::from("/tmp/shots"));
        // Without an override the tool's own configuration is used.
        let (allowed, denied) = t.effective_domains(None);
        assert_eq!(allowed, Some(vec!["a.com".to_string()]));
        assert_eq!(denied, vec!["bad.com".to_string()]);
        assert_eq!(t.screenshot_dir(None), std::env::temp_dir());
    }

    // --- Request-interception policy decisions (pure; no browser) --------------------------

    #[test]
    fn interceptor_decisions_match_the_navigate_policy_exactly() {
        let i = Interceptor::default();
        i.set_policy(None, vec!["evil.example".to_string()]);

        // Same host matching as `navigate`, applied to a subresource URL.
        assert!(i.allows("https://good.example/app.js"));
        assert!(!i.allows("https://evil.example/x.js"));
        assert!(!i.allows("https://cdn.evil.example/x.js"));
        assert!(!i.allows("http://EVIL.EXAMPLE/x.js"));
        // Same scheme rules as `navigate`.
        assert!(i.allows("about:blank"));
        assert!(i.allows("data:text/html,<h1>hi</h1>"));
        assert!(i.allows("file:///tmp/x.js"));
        assert!(!i.allows("chrome://settings"));
        assert!(!i.allows("javascript:alert(1)"));

        // An allowlist restricts subresources exactly as it restricts navigation.
        i.set_policy(Some(vec!["good.example".to_string()]), Vec::new());
        assert!(i.allows("https://good.example/a.js"));
        assert!(i.allows("https://cdn.good.example/a.js"));
        assert!(!i.allows("https://other.example/a.js"));
        // ...and `file:` loses its exemption under an allowlist, just like in `navigate`.
        assert!(!i.allows("file:///tmp/x.js"));

        // Whatever `check_navigation_url` says is what the interceptor does — property-checked
        // over a spread of URLs so the two can never drift apart.
        let allowed = Some(vec!["a.example".to_string()]);
        let denied = vec!["b.example".to_string()];
        i.set_policy(allowed.clone(), denied.clone());
        for url in [
            "https://a.example/x",
            "https://sub.a.example/x",
            "https://b.example/x",
            "https://a.example.b.example/x",
            "http://c.example/x",
            "about:blank",
            "data:text/plain,hi",
            "file:///etc/passwd",
            "ftp://a.example/x",
            "not a url",
            "",
        ] {
            assert_eq!(
                i.allows(url),
                check_navigation_url(&allowed, &denied, url).is_ok(),
                "interceptor disagreed with check_navigation_url on {url:?}"
            );
        }
    }

    #[test]
    fn blocked_log_dedupes_caps_and_counts() {
        let i = Interceptor::default();
        i.set_policy(None, vec!["evil.example".to_string()]);
        // Same URL refused three times: listed once, counted three times.
        for _ in 0..3 {
            i.record_block("https://evil.example/a.js");
        }
        for n in 0..MAX_BLOCKED_REPORTED + 10 {
            i.record_block(&format!("https://evil.example/{n}.js"));
        }
        let log = i.take_blocked();
        assert_eq!(log.urls.len(), MAX_BLOCKED_REPORTED);
        assert_eq!(log.total, 3 + MAX_BLOCKED_REPORTED + 10);
        assert_eq!(log.urls[0], "https://evil.example/a.js");
        // Draining leaves the record empty for the next action.
        let log = i.take_blocked();
        assert!(log.urls.is_empty());
        assert_eq!(log.total, 0);
    }

    #[test]
    fn blocked_reporting_json_shape() {
        // Nothing blocked: the key is still present, so an empty list is unambiguous.
        let mut value = json!({ "action": "navigate" });
        attach_blocked(&mut value, BlockedLog::default());
        assert_eq!(value["blocked"], json!([]));
        assert!(value.get("blocked_count").is_none());

        // A few blocked, all listed: no count needed.
        let i = Interceptor::default();
        i.record_block("https://evil.example/a.js");
        i.record_block("https://evil.example/b.js");
        let mut value = json!({ "action": "click" });
        attach_blocked(&mut value, i.take_blocked());
        assert_eq!(
            value["blocked"],
            json!(["https://evil.example/a.js", "https://evil.example/b.js"])
        );
        assert!(value.get("blocked_count").is_none());

        // Overflowing the cap adds the true total.
        let i = Interceptor::default();
        for n in 0..MAX_BLOCKED_REPORTED + 5 {
            i.record_block(&format!("https://evil.example/{n}.js"));
        }
        let mut value = json!({ "action": "text" });
        attach_blocked(&mut value, i.take_blocked());
        assert_eq!(
            value["blocked"].as_array().unwrap().len(),
            MAX_BLOCKED_REPORTED
        );
        assert_eq!(value["blocked_count"], json!(MAX_BLOCKED_REPORTED + 5));
    }

    #[tokio::test]
    async fn per_call_policy_override_reaches_the_interceptor() {
        // The `_policy` override must retarget the boundary, not just the navigate check: the
        // action path publishes the effective lists before doing any page work.
        let t = BrowserTool::new(None, None, Vec::new());
        assert!(t.interceptor.allows("https://later-denied.example/x.js"));
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": "https://later-denied.example/",
                "_policy": { "deniedDomains": ["later-denied.example"] }
            }))
            .await
            .unwrap();
        assert_eq!(parse_output(&res)["code"], "BROWSER_DOMAIN_DENIED");
        assert!(
            !t.interceptor.allows("https://later-denied.example/x.js"),
            "the override should now be enforced on page requests too"
        );
    }

    // --- Behaviour of `execute` that must hold with no browser installed -------------------

    #[tokio::test]
    async fn actions_without_a_page_report_no_page_instead_of_opening_a_blank_one() {
        let t = tool();
        for action in [
            "click",
            "type",
            "evaluate",
            "text",
            "wait_for",
            "screenshot",
        ] {
            let res = t
                .execute(json!({ "action": action, "selector": "#a", "script": "1" }))
                .await
                .expect("execute should not error");
            assert!(res.is_error, "{action} should be an error without a page");
            let body = parse_output(&res);
            assert_eq!(body["code"], "BROWSER_NO_PAGE", "{action}: {body}");
            assert!(
                body["message"].as_str().unwrap().contains("navigate"),
                "{action} message should tell the model to navigate first"
            );
        }
        // Nothing was launched.
        assert!(t.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn unknown_and_missing_actions_are_rejected() {
        let t = tool();
        let res = t.execute(json!({})).await.unwrap();
        assert!(res.is_error);
        assert_eq!(parse_output(&res)["code"], "BROWSER_MISSING_ARGUMENT");

        let res = t.execute(json!({ "action": "teleport" })).await.unwrap();
        assert!(res.is_error);
        assert_eq!(parse_output(&res)["code"], "BROWSER_UNKNOWN_ACTION");
        assert!(t.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn blocked_navigation_never_launches_a_browser() {
        let t = BrowserTool::new(None, None, vec!["blocked.example".to_string()]);
        let res = t
            .execute(json!({ "action": "navigate", "url": "https://blocked.example/page" }))
            .await
            .unwrap();
        assert!(res.is_error);
        assert_eq!(parse_output(&res)["code"], "BROWSER_DOMAIN_DENIED");
        assert!(
            t.session.lock().await.is_none(),
            "a denied navigation must not start Chrome"
        );

        // Same for a bad scheme and a missing url.
        let res = t
            .execute(json!({ "action": "navigate", "url": "javascript:alert(1)" }))
            .await
            .unwrap();
        assert_eq!(parse_output(&res)["code"], "BROWSER_SCHEME_DENIED");
        let res = t.execute(json!({ "action": "navigate" })).await.unwrap();
        assert_eq!(parse_output(&res)["code"], "BROWSER_MISSING_ARGUMENT");
        assert!(t.session.lock().await.is_none());
    }

    #[tokio::test]
    async fn close_with_no_browser_is_a_successful_no_op() {
        let t = tool();
        let res = t.execute(json!({ "action": "close" })).await.unwrap();
        assert!(!res.is_error);
        let body = parse_output(&res);
        assert_eq!(body["closed"], false);
    }

    #[tokio::test]
    async fn timeout_argument_is_clamped() {
        // Only observable indirectly, but a zero/huge value must not panic or hang.
        let t = tool();
        let res = t
            .execute(json!({ "action": "text", "timeout_secs": 0 }))
            .await
            .unwrap();
        assert_eq!(parse_output(&res)["code"], "BROWSER_NO_PAGE");
        let res = t
            .execute(json!({ "action": "text", "timeout_secs": 10_000_000 }))
            .await
            .unwrap();
        assert_eq!(parse_output(&res)["code"], "BROWSER_NO_PAGE");
    }

    // --- Real-browser integration check ----------------------------------------------------
    //
    // Ignored so CI stays green without Chrome. Run with:
    //   cargo test -p consistent-world-tools -- --ignored --nocapture

    fn fixture_url() -> String {
        let html = r#"<!doctype html>
<html><head><title>Browser Tool Fixture</title></head>
<body>
  <h1 id="heading">Consistent World fixture</h1>
  <p>The quick brown fox reads the page.</p>
  <input id="field" value="" />
  <button id="go" onclick="document.getElementById('out').textContent = 'clicked:' + document.getElementById('field').value;">Go</button>
  <div id="out">not clicked</div>
  <script>
    setTimeout(function () {
      var d = document.createElement('div');
      d.id = 'late';
      d.textContent = 'late element arrived';
      document.body.appendChild(d);
    }, 700);
  </script>
</body></html>"#;
        format!(
            "data:text/html;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(html)
        )
    }

    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn drives_a_real_page_end_to_end() {
        let t = tool();

        // 1. Navigate — this establishes the page every later action must reuse.
        let res = t
            .execute(json!({ "action": "navigate", "url": fixture_url(), "timeout_secs": 60 }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);
        let body = parse_output(&res);
        assert_eq!(body["title"], "Browser Tool Fixture", "{body}");

        // 2. Text extraction reads the page we navigated to (not a blank tab).
        let res = t.execute(json!({ "action": "text" })).await.unwrap();
        assert!(!res.is_error, "text failed: {}", res.output);
        let body = parse_output(&res);
        let text = body["text"].as_str().unwrap();
        assert!(
            text.contains("Consistent World fixture"),
            "text was: {text}"
        );
        assert!(text.contains("quick brown fox"), "text was: {text}");
        assert_eq!(body["title"], "Browser Tool Fixture");

        // 3. wait_for an element injected 700ms after load.
        let res = t
            .execute(json!({ "action": "wait_for", "selector": "#late", "timeout_secs": 15 }))
            .await
            .unwrap();
        assert!(!res.is_error, "wait_for failed: {}", res.output);
        assert_eq!(parse_output(&res)["found"], true);

        // 4. Type into the field, then read it back via evaluate — proves both act on the
        //    same live page.
        let res = t
            .execute(json!({ "action": "type", "selector": "#field", "text": "hello-agent" }))
            .await
            .unwrap();
        assert!(!res.is_error, "type failed: {}", res.output);
        let res = t
            .execute(
                json!({ "action": "evaluate", "script": "document.getElementById('field').value" }),
            )
            .await
            .unwrap();
        assert!(!res.is_error, "evaluate failed: {}", res.output);
        assert_eq!(parse_output(&res)["value"], "hello-agent");

        // 5. Click the button, which mutates the DOM using the text we typed.
        let res = t
            .execute(json!({ "action": "click", "selector": "#go" }))
            .await
            .unwrap();
        assert!(!res.is_error, "click failed: {}", res.output);
        let res = t.execute(json!({ "action": "text" })).await.unwrap();
        let text = parse_output(&res)["text"].as_str().unwrap().to_string();
        assert!(
            text.contains("clicked:hello-agent"),
            "click did not take effect; page text was: {text}"
        );

        // 6. Screenshot returns real PNG bytes, on disk and inline.
        let res = t.execute(json!({ "action": "screenshot" })).await.unwrap();
        assert!(!res.is_error, "screenshot failed: {}", res.output);
        let body = parse_output(&res);
        let path = PathBuf::from(body["path"].as_str().unwrap());
        let on_disk = std::fs::read(&path).expect("screenshot file should exist");
        assert!(on_disk.len() > 1000, "screenshot suspiciously small");
        assert_eq!(&on_disk[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
        let inline = base64::engine::general_purpose::STANDARD
            .decode(body["base64"].as_str().expect("base64 should be inlined"))
            .expect("base64 should decode");
        assert_eq!(inline, on_disk);
        let _ = std::fs::remove_file(&path);

        // 7. Close, then confirm the tool reports "navigate first" rather than silently
        //    opening a blank page.
        let res = t.execute(json!({ "action": "close" })).await.unwrap();
        assert_eq!(parse_output(&res)["closed"], true, "{}", res.output);
        let res = t
            .execute(json!({ "action": "click", "selector": "#go" }))
            .await
            .unwrap();
        assert!(res.is_error);
        assert_eq!(parse_output(&res)["code"], "BROWSER_NO_PAGE");
    }

    // --- Local fixture server for the interception tests -----------------------------------
    //
    // Deliberately loopback-only and hand-rolled (no extra dev-dependency, no public internet).
    // One server is enough to prove the boundary: `127.0.0.1` and `localhost` are two different
    // *hosts* for policy matching but the same listening socket, so denying `localhost` while
    // allowing `127.0.0.1` lets the same server both serve the page and observe, in its own hit
    // log, that the denied requests never arrived.

    type Routes = Arc<std::sync::Mutex<Vec<(String, String, String)>>>;

    /// A minimal loopback HTTP server for the interception tests: serves fixed routes and
    /// records every request path it actually receives, so a test can assert a denied request
    /// never arrived rather than just that the tool reported blocking it.
    struct FixtureServer {
        port: u16,
        hits: Arc<std::sync::Mutex<Vec<String>>>,
        routes: Routes,
        task: JoinHandle<()>,
    }

    impl Drop for FixtureServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FixtureServer {
        /// Bind first, then install routes built from the port we actually got, so fixture HTML
        /// can point at itself. Routes are settable after the bind ([`set_routes`]) because two
        /// servers that reference each other cannot both know the other's port at bind time.
        async fn start(
            make_routes: impl FnOnce(u16) -> Vec<(&'static str, &'static str, String)>,
        ) -> Self {
            let server = Self::start_on("127.0.0.1:0").await.expect("bind loopback");
            let routes = make_routes(server.port);
            server.set_routes(routes);
            server
        }

        async fn start_on(bind: &str) -> Option<Self> {
            let listener = tokio::net::TcpListener::bind(bind).await.ok()?;
            let port = listener.local_addr().ok()?.port();
            let hits: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(vec![]));
            let routes: Routes = Arc::new(std::sync::Mutex::new(Vec::new()));
            let hits_task = hits.clone();
            let routes_task = routes.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((sock, _)) = listener.accept().await else {
                        return;
                    };
                    let routes = routes_task.clone();
                    let hits = hits_task.clone();
                    tokio::spawn(async move {
                        let _ = serve_one(sock, routes, hits).await;
                    });
                }
            });
            Some(Self {
                port,
                hits,
                routes,
                task,
            })
        }

        fn set_routes(&self, routes: Vec<(&'static str, &'static str, String)>) {
            *self.routes.lock().unwrap() = routes
                .into_iter()
                .map(|(p, ct, body)| (p.to_string(), ct.to_string(), body))
                .collect();
        }

        fn saw(&self, path: &str) -> bool {
            self.hits.lock().unwrap().iter().any(|p| p == path)
        }

        fn hit_log(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }

    // Handle one connection: read just the request line/headers (no bodies in this fixture),
    // log the path, then respond from `routes` or 404.
    async fn serve_one(
        mut sock: tokio::net::TcpStream,
        routes: Routes,
        hits: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        // Read just the request head; the fixture has no request bodies.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 64 * 1024 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        // Chrome opens speculative sockets it never writes to; those are not requests and must
        // not pollute the hit log the assertions read.
        let Some(path) = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .map(str::to_string)
        else {
            return Ok(());
        };
        hits.lock().unwrap().push(path.clone());

        let route = routes
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _, _)| *p == path)
            .cloned();
        let response = match route {
            // `content_type == "redirect"` is a fixture shorthand: the body is the Location.
            Some((_, content_type, location)) if content_type == "redirect" => format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            ),
            Some((_, content_type, body)) => format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                 Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
            None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        };
        sock.write_all(response.as_bytes()).await?;
        sock.flush().await
    }

    /// The fixture: an allowed page on `127.0.0.1` that tries very hard to reach `localhost`.
    fn interception_routes(port: u16) -> Vec<(&'static str, &'static str, String)> {
        let index = format!(
            r#"<!doctype html>
<html><head><title>Interception Fixture</title>
  <script src="http://127.0.0.1:{port}/allowed.js"></script>
  <script src="http://localhost:{port}/denied.js"></script>
</head>
<body>
  <h1>fixture</h1>
  <img id="pic" src="http://localhost:{port}/denied.png" alt="denied image">
  <a id="evil" href="http://localhost:{port}/evil.html">go somewhere denied</a>
  <div id="xhr">pending</div>
  <script>
    window.__xhrError = null;
    fetch("http://localhost:{port}/denied-xhr")
      .then(function (r) {{ window.__xhrOk = r.status; }})
      .catch(function (e) {{ window.__xhrError = String(e); }});
    window.__imgError = null;
    document.getElementById('pic').onerror = function () {{ window.__imgError = 'error'; }};
  </script>
</body></html>"#
        );
        vec![
            ("/index.html", "text/html", index),
            (
                "/allowed.js",
                "application/javascript",
                "window.__allowed = true;".to_string(),
            ),
            (
                "/denied.js",
                "application/javascript",
                "window.__leaked = true;".to_string(),
            ),
            (
                "/evil.html",
                "text/html",
                "<!doctype html><title>LEAKED</title><h1>leaked</h1>".to_string(),
            ),
            ("/denied.png", "image/png", "not-really-a-png".to_string()),
            ("/denied-xhr", "text/plain", "leaked".to_string()),
            // An *allowed* URL whose server-side redirect lands on the denied host.
            (
                "/redirect-to-denied",
                "redirect",
                format!("http://localhost:{port}/evil.html"),
            ),
            // An *allowed* page that tries to bounce itself to the denied host.
            (
                "/meta-refresh.html",
                "text/html",
                format!(
                    r#"<!doctype html><title>Refresher</title>
                       <meta http-equiv="refresh" content="0;url=http://localhost:{port}/evil.html">
                       <p>bouncing</p>"#
                ),
            ),
        ]
    }

    /// Drive a real Chrome and prove the domain policy is enforced on requests the *page* makes,
    /// not just on the URL the model asked for.
    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn policy_is_enforced_on_every_request_the_page_makes() {
        let server = FixtureServer::start(interception_routes).await;
        let port = server.port;

        // `localhost` is denied; `127.0.0.1` — the very same socket — is not.
        let t = BrowserTool::new(None, None, vec!["localhost".to_string()]);

        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/index.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);
        let body = parse_output(&res);
        assert_eq!(body["title"], "Interception Fixture", "{body}");

        // (c) The allowed subresource really went through — interception is not just breaking
        //     everything.
        let res = t
            .execute(json!({ "action": "evaluate", "script": "window.__allowed === true" }))
            .await
            .unwrap();
        assert_eq!(
            parse_output(&res)["value"],
            json!(true),
            "the ALLOWED script did not execute; interception is over-blocking: {}",
            res.output
        );
        assert!(
            server.saw("/allowed.js"),
            "server never served the allowed script; hits: {:?}",
            server.hit_log()
        );

        // (a) Subresources aimed at the denied host were stopped before they left the browser.
        let res = t
            .execute(json!({ "action": "evaluate", "script": "window.__leaked === true" }))
            .await
            .unwrap();
        assert_eq!(
            parse_output(&res)["value"],
            json!(false),
            "the DENIED script executed: the policy is still only a guardrail"
        );
        for path in ["/denied.js", "/denied.png", "/denied-xhr"] {
            assert!(
                !server.saw(path),
                "denied request {path} reached the server; hits: {:?}",
                server.hit_log()
            );
        }

        // The navigate result named what it stopped, so a missing script is distinguishable from
        // a broken site.
        let blocked = body["blocked"]
            .as_array()
            .expect("navigate result must carry a `blocked` array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            blocked.contains("denied.js"),
            "blocked list should name the refused script, was: {blocked}"
        );
        eprintln!("navigate reported blocked: {}", body["blocked"]);

        // (b') A real link click to the denied host does not succeed either.
        let res = t
            .execute(json!({ "action": "click", "selector": "#evil" }))
            .await
            .unwrap();
        assert!(!res.is_error, "click failed: {}", res.output);
        time::sleep(Duration::from_millis(1000)).await;
        let res = t
            .execute(json!({ "action": "evaluate", "script": "location.href" }))
            .await
            .unwrap();
        let after = parse_output(&res);
        let landed = after["value"].as_str().unwrap_or("").to_string();
        assert!(
            !landed.contains("evil.html"),
            "a link click escaped the policy: now at {landed}"
        );
        assert!(
            !server.saw("/evil.html"),
            "the clicked denied link reached the server; hits: {:?}",
            server.hit_log()
        );
        eprintln!("after blocked link click, location is: {landed}");

        // Back to the fixture: a blocked main-frame navigation leaves Chrome on its own
        // `chrome-error://` page, so the previous document is gone.
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/index.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "re-navigate failed: {}", res.output);

        // (b) A JS-driven navigation to the denied host does not succeed.
        let _ = t
            .execute(json!({
                "action": "evaluate",
                "script": format!("location.href = 'http://localhost:{port}/evil.html'; 1")
            }))
            .await
            .unwrap();
        time::sleep(Duration::from_millis(1500)).await;
        let res = t
            .execute(json!({ "action": "evaluate", "script": "location.href" }))
            .await
            .unwrap();
        let after = parse_output(&res);
        let landed = after["value"].as_str().unwrap_or("").to_string();
        assert!(
            !landed.contains("evil.html"),
            "a JS navigation escaped the policy: now at {landed}"
        );
        assert!(
            !server.saw("/evil.html"),
            "the denied navigation reached the server; hits: {:?}",
            server.hit_log()
        );
        let blocked = after["blocked"].as_array().cloned().unwrap_or_default();
        assert!(
            blocked
                .iter()
                .any(|v| v.as_str().unwrap_or("").contains("evil.html")),
            "the blocked navigation should be reported; blocked was {blocked:?}"
        );
        eprintln!("after blocked JS navigation, location is: {landed}");

        // A server-side redirect from an allowed URL onto the denied host is checked too: the
        // policy has to see the redirect *target*, not just the URL the model typed.
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/redirect-to-denied"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        let body = parse_output(&res);
        assert!(
            !server.saw("/evil.html"),
            "a 302 redirect escaped the policy; hits: {:?}",
            server.hit_log()
        );
        eprintln!("redirect result: {body}");

        // `<meta http-equiv="refresh">` is a navigation the model never asked for.
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/meta-refresh.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(
            !res.is_error,
            "meta-refresh navigate failed: {}",
            res.output
        );
        time::sleep(Duration::from_millis(1500)).await;
        let res = t
            .execute(json!({ "action": "evaluate", "script": "location.href" }))
            .await
            .unwrap();
        let after = parse_output(&res);
        assert!(
            !after["value"].as_str().unwrap_or("").contains("evil.html"),
            "a <meta refresh> escaped the policy: now at {}",
            after["value"]
        );
        assert!(
            !server.saw("/evil.html"),
            "the <meta refresh> reached the denied host; hits: {:?}",
            server.hit_log()
        );
        eprintln!("after <meta refresh>, location is: {}", after["value"]);

        // Navigating the model itself to the denied host is still refused up front.
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://localhost:{port}/evil.html")
            }))
            .await
            .unwrap();
        assert_eq!(parse_output(&res)["code"], "BROWSER_DOMAIN_DENIED");

        let _ = t.execute(json!({ "action": "close" })).await;
        eprintln!("fixture server hit log: {:?}", server.hit_log());
    }

    /// KNOWN HOLE, measured rather than assumed: the CDP `Fetch` domain does not pause
    /// WebSocket handshakes, so `new WebSocket("ws://denied-host/")` reaches the network with no
    /// policy check and nothing appears in `blocked`. Closing this needs a different mechanism
    /// (`Network.webSocketWillSendHandshakeRequest` is observe-only; there is no CDP verb to
    /// refuse one), so it is recorded here instead of being papered over.
    ///
    /// Also probed here: service workers. In this setup `navigator.serviceWorker.register()`
    /// never resolves — chromiumoxide detaches from `service_worker` targets — so a service
    /// worker never reaches the point of making requests. That makes the SW path *unverified*
    /// rather than proven safe: nothing here intercepts a service worker's own fetches.
    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn known_hole_websocket_handshakes_are_not_intercepted() {
        let server = FixtureServer::start(|port| {
            vec![
                (
                    "/ws-probe.html",
                    "text/html",
                    format!(
                        r#"<!doctype html><title>WS probe</title><script>
                           try {{ new WebSocket("ws://localhost:{port}/ws-denied"); }} catch (e) {{}}
                           </script>"#
                    ),
                ),
                ("/ws-denied", "text/plain", "reached".to_string()),
            ]
        })
        .await;
        let port = server.port;

        let t = BrowserTool::new(None, None, vec!["localhost".to_string()]);
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/ws-probe.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);
        time::sleep(Duration::from_millis(1500)).await;

        assert!(
            server.saw("/ws-denied"),
            "WebSocket handshakes now appear to be covered; remove this known-hole test. \
             hits: {:?}",
            server.hit_log()
        );
        eprintln!(
            "KNOWN HOLE confirmed: WebSocket handshake reached the denied host; hits {:?}",
            server.hit_log()
        );
        let _ = t.execute(json!({ "action": "close" })).await;
    }

    /// KNOWN HOLE, recorded deliberately rather than left as folklore.
    ///
    /// `window.open` creates a brand-new CDP target with its own session. Interception is armed
    /// on the page's own session, so the popup's first navigation is never checked. The tool
    /// itself never drives popups (there is exactly one *current page*), so this is not reachable
    /// by the model — but it is reachable by a hostile page, and it is an exfiltration channel.
    /// Chrome 150 ignores `--block-new-web-contents`; that was measured, not assumed.
    ///
    /// If this test starts failing, the hole has been closed — delete it and update the docs.
    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn known_hole_popup_windows_escape_the_policy() {
        let server = FixtureServer::start(|port| {
            vec![
                (
                    "/opener.html",
                    "text/html",
                    format!(
                        r#"<!doctype html><title>Opener</title>
                           <script>window.open("http://localhost:{port}/popup.html");</script>"#
                    ),
                ),
                (
                    "/popup.html",
                    "text/html",
                    "<!doctype html><title>POPUP</title>".to_string(),
                ),
            ]
        })
        .await;
        let port = server.port;

        let t = BrowserTool::new(None, None, vec!["localhost".to_string()]);
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/opener.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);
        time::sleep(Duration::from_millis(1500)).await;

        assert!(
            server.saw("/popup.html"),
            "popups appear to be covered now; remove this known-hole test. hits: {:?}",
            server.hit_log()
        );
        eprintln!("KNOWN HOLE confirmed: popup hits {:?}", server.hit_log());
        let _ = t.execute(json!({ "action": "close" })).await;
    }

    /// A cross-site iframe is the interesting case: with Chrome's site isolation on it would get
    /// its own renderer *and its own CDP target*, and the requests it makes after it has loaded
    /// would never reach interception armed on the main page's session. `127.0.0.1` and `[::1]`
    /// are different sites to Chrome, so this exercises exactly that path — two allowed hosts
    /// serving a nested document, and a denied host the nested document tries to reach.
    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn interception_covers_requests_made_inside_a_cross_site_iframe() {
        // The inner document lives on IPv6 loopback; skip if this machine has none. Both
        // servers are bound *before* either gets routes, so each can reference the other's port.
        let Some(inner) = FixtureServer::start_on("[::1]:0").await else {
            eprintln!("no IPv6 loopback on this machine; skipping cross-site iframe check");
            return;
        };
        let outer = FixtureServer::start_on("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let (inner_port, outer_port) = (inner.port, outer.port);

        inner.set_routes(vec![(
            "/frame.html",
            "text/html",
            format!(
                r#"<!doctype html><title>Inner</title>
                   <script src="http://localhost:{outer_port}/iframe-denied.js"></script>
                   <script src="http://127.0.0.1:{outer_port}/iframe-allowed.js"></script>
                   <p>inner</p>"#
            ),
        )]);
        outer.set_routes(vec![
            (
                "/outer.html",
                "text/html",
                format!(
                    r#"<!doctype html><title>Outer</title>
                       <iframe id="f" src="http://[::1]:{inner_port}/frame.html"></iframe>
                       <p>outer</p>"#
                ),
            ),
            (
                "/iframe-allowed.js",
                "application/javascript",
                "window.top.__iframeAllowed = true;".to_string(),
            ),
            (
                "/iframe-denied.js",
                "application/javascript",
                "window.top.__iframeLeaked = true;".to_string(),
            ),
        ]);

        let t = BrowserTool::new(None, None, vec!["localhost".to_string()]);
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{outer_port}/outer.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);
        time::sleep(Duration::from_millis(1500)).await;

        assert!(
            inner.saw("/frame.html"),
            "the allowed cross-site iframe never loaded; inner hits: {:?}",
            inner.hit_log()
        );
        assert!(
            outer.saw("/iframe-allowed.js"),
            "the iframe's ALLOWED request was over-blocked; outer hits: {:?}",
            outer.hit_log()
        );
        assert!(
            !outer.saw("/iframe-denied.js"),
            "a request from inside a cross-site iframe escaped the policy; outer hits: {:?}",
            outer.hit_log()
        );
        eprintln!(
            "cross-site iframe: outer hits {:?}, inner hits {:?}",
            outer.hit_log(),
            inner.hit_log()
        );
        let _ = t.execute(json!({ "action": "close" })).await;
    }

    /// The boundary must be re-armed after the tool relaunches a dead browser.
    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn interception_survives_a_browser_relaunch() {
        let server = FixtureServer::start(interception_routes).await;
        let port = server.port;

        let t = BrowserTool::new(None, None, vec!["localhost".to_string()]);
        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/index.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);

        // Kill Chrome behind the tool's back, then forget what the fixture server has seen.
        {
            let mut slot = t.session.lock().await;
            let session = slot.as_mut().expect("session exists");
            session.page = None;
            let _ = session.browser.kill().await;
        }
        server.hits.lock().unwrap().clear();

        let res = t
            .execute(json!({
                "action": "navigate",
                "url": format!("http://127.0.0.1:{port}/index.html"),
                "timeout_secs": 60
            }))
            .await
            .unwrap();
        assert!(!res.is_error, "should have relaunched, got: {}", res.output);
        assert_eq!(parse_output(&res)["title"], "Interception Fixture");
        assert!(
            server.saw("/allowed.js"),
            "the relaunched browser did not load the page properly"
        );
        assert!(
            !server.saw("/denied.js"),
            "the relaunched browser lost the boundary; hits: {:?}",
            server.hit_log()
        );
        let _ = t.execute(json!({ "action": "close" })).await;
    }

    #[tokio::test]
    #[ignore = "requires a real Chrome/Chromium installation"]
    async fn recovers_when_the_browser_dies_underneath_it() {
        let t = tool();
        let res = t
            .execute(json!({ "action": "navigate", "url": fixture_url(), "timeout_secs": 60 }))
            .await
            .unwrap();
        assert!(!res.is_error, "navigate failed: {}", res.output);

        // Kill Chrome behind the tool's back.
        {
            let mut slot = t.session.lock().await;
            let session = slot.as_mut().expect("session exists");
            session.page = None;
            let _ = session.browser.kill().await;
        }

        // A fresh navigate must relaunch rather than erroring forever.
        let res = t
            .execute(json!({ "action": "navigate", "url": fixture_url(), "timeout_secs": 60 }))
            .await
            .unwrap();
        assert!(!res.is_error, "should have relaunched, got: {}", res.output);
        assert_eq!(parse_output(&res)["title"], "Browser Tool Fixture");

        let _ = t.execute(json!({ "action": "close" })).await;
    }
}
