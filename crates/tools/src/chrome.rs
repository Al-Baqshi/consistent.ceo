//! Finding, launching and re-adopting the Consistent browser's Chrome.
//!
//! The Consistent browser is an ordinary, visible Chrome window with a profile
//! of its own. A person signs in to whatever they like in it; the agent drives
//! the same window afterwards over the Chrome DevTools Protocol
//! (`BrowserTool` with `cdp_url`). This module is the plumbing for that and
//! nothing else: where Chrome is, which flags the window starts with, and how to
//! find a window that is already running.
//!
//! Choices worth knowing about:
//!
//! - **No automation flags.** `--enable-automation` makes Chrome show an
//!   "automated software" bar and sets `navigator.webdriver`, which is exactly
//!   what makes sites refuse a sign-in. A person signing in here should see a
//!   normal browser.
//! - **Site isolation stays on.** `BrowserTool` switches it off when it launches
//!   its own throwaway Chrome so request interception covers cross-site iframes.
//!   That is a trade this browser does not make: it holds real sessions, and a
//!   renderer boundary is worth more than a complete network boundary here.
//! - **A random debugging port, loopback only.** `--remote-debugging-port=0`
//!   lets Chrome pick, and write the port into `<profile>/DevToolsActivePort`;
//!   that file is also how a window that outlived the gateway is found again.
//!   The port exists only while the window is open.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

/// Where a Chrome (or a close relative) lives, most specific first. The
/// `CONSISTENT_WORLD_BROWSER_EXECUTABLE` override beats all of these.
fn candidate_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        for app in [
            "Google Chrome",
            "Google Chrome Beta",
            "Chromium",
            "Brave Browser",
            "Microsoft Edge",
        ] {
            out.push(PathBuf::from(format!(
                "/Applications/{app}.app/Contents/MacOS/{app}"
            )));
        }
        if let Some(home) = consistent_world_common::platform::home_dir() {
            for app in ["Google Chrome", "Chromium", "Brave Browser", "Microsoft Edge"] {
                out.push(home.join(format!("Applications/{app}.app/Contents/MacOS/{app}")));
            }
        }
    } else if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                out.push(base.join("Google/Chrome/Application/chrome.exe"));
                out.push(base.join("Microsoft/Edge/Application/msedge.exe"));
                out.push(base.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
            }
        }
    } else {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "brave-browser",
            "microsoft-edge",
        ] {
            for dir in ["/usr/bin", "/usr/local/bin", "/snap/bin", "/opt/google/chrome"] {
                out.push(PathBuf::from(dir).join(name));
            }
        }
    }
    out
}

/// The Chrome executable to use, if there is one.
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("CONSISTENT_WORLD_BROWSER_EXECUTABLE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        return explicit.is_file().then_some(explicit);
    }
    candidate_paths().into_iter().find(|p| p.is_file())
}

/// The command-line a Consistent browser window starts with. Pure, so the flags
/// are checked by a test rather than by remembering them.
pub fn launch_args(profile_dir: &Path, headless: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        format!("--user-data-dir={}", profile_dir.display()).into(),
        "--remote-debugging-port=0".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--restore-last-session".into(),
    ];
    if headless {
        args.push("--headless=new".into());
    }
    args
}

/// Name the profile so the window is recognisable ("Consistent browser" in the
/// profile menu). Written only when the profile has no preferences yet; once
/// Chrome owns the file, it is Chrome's.
pub fn brand_new_profile(profile_dir: &Path) {
    let default_dir = profile_dir.join("Default");
    let prefs = default_dir.join("Preferences");
    if prefs.exists() || std::fs::create_dir_all(&default_dir).is_err() {
        return;
    }
    let _ = std::fs::write(
        prefs,
        br#"{"profile":{"name":"Consistent browser","using_default_name":false}}"#,
    );
}

/// `http://127.0.0.1:<port>` of a window launched with [`launch_args`], read
/// from the file Chrome writes. `None` before Chrome has written it. The file can
/// outlive a crashed Chrome, so a caller confirms with [`endpoint_alive`].
pub fn devtools_endpoint(profile_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(profile_dir.join("DevToolsActivePort")).ok()?;
    let port: u16 = text.lines().next()?.trim().parse().ok()?;
    (port > 0).then(|| format!("http://127.0.0.1:{port}"))
}

/// Forget a stale `DevToolsActivePort` so the next launch's file can be told
/// apart from the last one's.
pub fn clear_devtools_endpoint(profile_dir: &Path) {
    let _ = std::fs::remove_file(profile_dir.join("DevToolsActivePort"));
}

fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .no_proxy()
        .build()
        .ok()
}

/// Does a Chrome answer at this endpoint right now?
pub async fn endpoint_alive(endpoint: &str) -> bool {
    let Some(client) = client() else {
        return false;
    };
    matches!(
        client.get(format!("{endpoint}/json/version")).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// One open tab, as the browser reports it. Page info only: nothing here is a
/// cookie, a token or a password.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Tab {
    pub url: String,
    pub title: String,
}

/// The tabs open in the window, for the status line ("open now: github.com").
pub async fn open_tabs(endpoint: &str) -> Vec<Tab> {
    let Some(client) = client() else {
        return Vec::new();
    };
    let Ok(resp) = client.get(format!("{endpoint}/json/list")).send().await else {
        return Vec::new();
    };
    let Ok(items) = resp.json::<Vec<serde_json::Value>>().await else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter(|t| t["type"] == "page")
        .filter_map(|t| {
            let url = t["url"].as_str()?.to_string();
            // New-tab and internal pages say nothing about where the person is signed in.
            (url.starts_with("http://") || url.starts_with("https://")).then(|| Tab {
                url,
                title: t["title"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Ask the window to close, the polite way (`Browser.close`), so Chrome saves
/// the session it will restore next time.
pub async fn close_browser(endpoint: &str) -> Result<(), String> {
    let (mut browser, mut handler) = chromiumoxide::Browser::connect(endpoint.to_string())
        .await
        .map_err(|e| e.to_string())?;
    let pump = tokio::spawn(async move {
        use futures::StreamExt;
        while handler.next().await.is_some() {}
    });
    let outcome = browser.close().await.map(|_| ()).map_err(|e| e.to_string());
    pump.abort();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_a_normal_browser_that_can_be_found_again() {
        let args: Vec<String> = launch_args(Path::new("/tmp/p"), false)
            .into_iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"--user-data-dir=/tmp/p".to_string()));
        // A random loopback port, so the window can be found through the file Chrome writes.
        assert!(args.contains(&"--remote-debugging-port=0".to_string()));
        assert!(args.contains(&"--remote-debugging-address=127.0.0.1".to_string()));
        // What would make a site refuse a sign-in, or weaken the browser a person signs in with.
        for forbidden in [
            "--enable-automation",
            "--disable-site-isolation-trials",
            "--headless=new",
            "--no-sandbox",
        ] {
            assert!(
                !args.iter().any(|a| a.starts_with(forbidden)),
                "{forbidden} must not be passed to the Consistent browser"
            );
        }
        assert!(!args.iter().any(|a| a.contains("disable-features")));
        let headless: Vec<String> = launch_args(Path::new("/tmp/p"), true)
            .into_iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(headless.contains(&"--headless=new".to_string()));
    }

    #[test]
    fn the_endpoint_comes_from_the_file_chrome_writes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(devtools_endpoint(dir.path()), None);
        std::fs::write(
            dir.path().join("DevToolsActivePort"),
            "40123\n/devtools/browser/abc\n",
        )
        .unwrap();
        assert_eq!(
            devtools_endpoint(dir.path()).as_deref(),
            Some("http://127.0.0.1:40123")
        );
        // A file that is not a port is not an endpoint.
        std::fs::write(dir.path().join("DevToolsActivePort"), "nonsense\n").unwrap();
        assert_eq!(devtools_endpoint(dir.path()), None);
        std::fs::write(dir.path().join("DevToolsActivePort"), "0\n").unwrap();
        assert_eq!(devtools_endpoint(dir.path()), None);
        clear_devtools_endpoint(dir.path());
    }

    #[test]
    fn a_new_profile_is_named_and_an_existing_one_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        brand_new_profile(dir.path());
        let prefs = dir.path().join("Default/Preferences");
        assert!(std::fs::read_to_string(&prefs).unwrap().contains("Consistent browser"));
        std::fs::write(&prefs, "{\"chrome\":\"owns this\"}").unwrap();
        brand_new_profile(dir.path());
        assert_eq!(
            std::fs::read_to_string(&prefs).unwrap(),
            "{\"chrome\":\"owns this\"}"
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_not_alive_and_has_no_tabs() {
        // Port 1 is never a Chrome.
        assert!(!endpoint_alive("http://127.0.0.1:1").await);
        assert!(open_tabs("http://127.0.0.1:1").await.is_empty());
    }
}
