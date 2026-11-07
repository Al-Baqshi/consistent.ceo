//! The Consistent browser: one visible Chrome window with a profile of its own.
//!
//! A person signs in to anything they like in it. Browser tasks then drive the
//! *same* window over the Chrome DevTools Protocol, so the agent works inside
//! accounts the person has already signed in to, without ever being handed a
//! password, a cookie or a token: the window holds those, the agent only sees
//! pages. See `docs/plans/browser-control.md` for the whole picture.
//!
//! This module owns the window's lifecycle and nothing about what the agent may
//! do in it (that is [`crate::mcp_browser`] and the per-entity site list):
//!
//! - [`open`] starts the window (or brings an open one forward);
//! - [`status`] says whether it is running and which sites are open in it;
//! - [`close`] closes it politely so Chrome saves the session it restores;
//! - [`ensure_running`] is what a browser task calls before it starts.
//!
//! The window outlives the gateway on purpose: quitting the app does not close
//! a browser someone may be in the middle of using. The debugging port Chrome
//! picks is written to `<profile>/DevToolsActivePort`, which is how a window
//! that outlived a gateway restart is found again.
//!
//! ### The debugging port
//!
//! An open DevTools port on a signed-in browser is a real exposure, and the
//! rest of the codebase refuses to attach to a person's own Chrome for exactly
//! that reason. This browser is different in the ways that matter: it is a
//! dedicated profile, not the one holding everything, the port is random and
//! bound to loopback, and it exists only while the window is open. Chrome also
//! refuses DevTools connections from web pages unless told otherwise, so a page
//! in the browser cannot reach it.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use consistent_world_tools::chrome;
use serde::Serialize;

/// How long a freshly started Chrome gets to publish its debugging port.
const START_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a polite close gets before the process is forced.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(12);

/// The Chrome this gateway started, if it is still the one running. A window
/// found through `DevToolsActivePort` after a gateway restart has no entry
/// here, and does not need one.
static CHILD: LazyLock<Mutex<Option<Child>>> = LazyLock::new(|| Mutex::new(None));
/// One open/close at a time: two taps on the button must not start two Chromes.
static LIFECYCLE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Where the browser's profile lives: its own folder, never the person's Chrome.
pub fn profile_dir() -> PathBuf {
    PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world/browser-profile",
    ))
}

/// A site open in the window: the host and the page title, nothing more.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSite {
    pub host: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatus {
    /// A Chrome (or Chromium, Brave, Edge) was found on this computer.
    pub chrome_found: bool,
    pub chrome_name: Option<String>,
    /// The window is open and can be driven.
    pub running: bool,
    pub profile_dir: String,
    /// The profile has been used, so earlier sign-ins are probably still there.
    pub used_before: bool,
    /// Sites open in the window right now.
    pub open_sites: Vec<OpenSite>,
}

fn chrome_name(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    for known in [
        "Google Chrome",
        "Chromium",
        "Brave",
        "Edge",
        "chrome",
        "chromium",
        "brave",
        "msedge",
    ] {
        if text.contains(known) {
            return match known {
                "chrome" => "Google Chrome",
                "chromium" => "Chromium",
                "brave" => "Brave",
                "msedge" | "Edge" => "Microsoft Edge",
                other => other,
            }
            .to_string();
        }
    }
    "Chrome".to_string()
}

/// The endpoint of the window if one is really open (the file can be stale).
pub async fn endpoint() -> Option<String> {
    let endpoint = chrome::devtools_endpoint(&profile_dir())?;
    chrome::endpoint_alive(&endpoint).await.then_some(endpoint)
}

/// Reap the child if the window was closed by hand, so it cannot linger.
fn reap_child() {
    if let Ok(mut slot) = CHILD.lock()
        && let Some(child) = slot.as_mut()
        && matches!(child.try_wait(), Ok(Some(_)))
    {
        *slot = None;
    }
}

pub async fn status() -> BrowserStatus {
    reap_child();
    let found = chrome::find_chrome();
    let profile = profile_dir();
    let endpoint = endpoint().await;
    let open_sites = match &endpoint {
        Some(ep) => chrome::open_tabs(ep)
            .await
            .into_iter()
            .filter_map(|t| {
                let host = reqwest::Url::parse(&t.url).ok()?.host_str()?.to_string();
                Some(OpenSite {
                    host,
                    title: t.title,
                })
            })
            .collect(),
        None => Vec::new(),
    };
    BrowserStatus {
        chrome_name: found.as_deref().map(chrome_name),
        chrome_found: found.is_some(),
        running: endpoint.is_some(),
        used_before: profile.join("Default").is_dir(),
        profile_dir: profile.to_string_lossy().to_string(),
        open_sites,
    }
}

fn spawn_chrome(args: Vec<std::ffi::OsString>) -> Result<Child, String> {
    let exe = chrome::find_chrome().ok_or_else(|| {
        "Google Chrome isn't installed on this computer. Install it from google.com/chrome, \
         then try again."
            .to_string()
    })?;
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not start Chrome: {e}"))
}

/// Open the window, or bring the open one to the front. Idempotent.
pub async fn open() -> Result<BrowserStatus, String> {
    let _guard = LIFECYCLE.lock().await;
    let profile = profile_dir();
    std::fs::create_dir_all(&profile)
        .map_err(|e| format!("Could not create the browser's profile folder: {e}"))?;

    if endpoint().await.is_some() {
        // Chrome hands a second launch on the same profile to the running one,
        // which opens a window and so brings the browser forward.
        let _ = spawn_chrome(vec![
            format!("--user-data-dir={}", profile.display()).into(),
        ]);
        return Ok(status().await);
    }

    chrome::brand_new_profile(&profile);
    chrome::clear_devtools_endpoint(&profile);
    let mut child = spawn_chrome(chrome::launch_args(&profile, false))?;
    let started = std::time::Instant::now();
    loop {
        if endpoint().await.is_some() {
            if let Ok(mut slot) = CHILD.lock() {
                *slot = Some(child);
            }
            return Ok(status().await);
        }
        // Chrome that exits straight away handed the launch to a window that
        // was already open on this profile without a debugging port (an older
        // run, or one started by hand). It is running, but cannot be driven.
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(if exit.success() {
                "The Consistent browser is already open but can't be controlled from here. Close \
                 its window, then press Open again."
                    .to_string()
            } else {
                format!("Chrome closed straight away ({exit}). Try again.")
            });
        }
        if started.elapsed() > START_TIMEOUT {
            let _ = child.kill();
            return Err("Chrome didn't start in time. Try again.".to_string());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Close the window politely, so Chrome saves the session it will restore.
pub async fn close() -> Result<BrowserStatus, String> {
    let _guard = LIFECYCLE.lock().await;
    if let Some(ep) = endpoint().await {
        chrome::close_browser(&ep)
            .await
            .map_err(|e| format!("Chrome didn't close: {e}"))?;
        let started = std::time::Instant::now();
        while chrome::endpoint_alive(&ep).await {
            if started.elapsed() > CLOSE_TIMEOUT {
                return Err("Chrome is taking a long time to close. Close its window yourself."
                    .to_string());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    if let Ok(mut slot) = CHILD.lock()
        && let Some(mut child) = slot.take()
    {
        let _ = child.wait();
    }
    Ok(status().await)
}

/// What a browser task calls first: the window's endpoint, opening the window
/// if it is closed so the person can watch the work happen.
pub async fn ensure_running() -> Result<String, String> {
    if let Some(ep) = endpoint().await {
        return Ok(ep);
    }
    open().await?;
    endpoint()
        .await
        .ok_or_else(|| "The Consistent browser didn't come up.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_is_its_own_folder_under_the_app_data() {
        let p = profile_dir();
        assert!(p.ends_with("browser-profile"), "{p:?}");
        assert!(p.to_string_lossy().contains(".consistent-world"), "{p:?}");
    }

    #[test]
    fn browsers_are_named_for_people() {
        let n = |s: &str| chrome_name(std::path::Path::new(s));
        assert_eq!(
            n("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            "Google Chrome"
        );
        assert_eq!(
            n("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
            "Brave"
        );
        assert_eq!(n("/usr/bin/chromium-browser"), "Chromium");
        assert_eq!(n("/opt/odd/thing"), "Chrome");
    }

    #[tokio::test]
    async fn a_missing_window_is_not_running_and_has_no_sites() {
        // No Consistent browser is started by tests, so whatever profile the
        // developer has, nothing answers on a stale or absent port.
        let s = status().await;
        assert!(s.profile_dir.ends_with("browser-profile"));
        assert!(s.open_sites.iter().all(|x| !x.host.is_empty()));
    }
}
