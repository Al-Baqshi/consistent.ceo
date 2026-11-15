//! Each entity's own browser: a visible Chrome window with a profile of its own.
//!
//! A person signs in to an entity's accounts in its window (its social media, its
//! dashboards, anything with a login). Browser tasks for that entity then drive
//! the *same* window over the Chrome DevTools Protocol, so the agent works inside
//! those accounts without ever being handed a password, a cookie or a token: the
//! window holds those, the agent only sees pages. One entity's sign-ins never
//! reach another's, because every entity has its own profile folder under
//! `~/.consistent-world/browsers/`. See `docs/plans/browser-control.md` for the
//! whole picture.
//!
//! This module owns the windows' lifecycle and nothing about what the agent may
//! do in them (that is [`crate::mcp_browser`] and the per-entity site list):
//!
//! - [`open`] starts an entity's window (or brings an open one forward);
//! - [`status`] says whether it is running and which sites are open in it;
//! - [`close`] closes it politely so Chrome saves the session it restores;
//! - [`ensure_running`] is what a browser task calls before it starts.
//!
//! A window outlives the gateway on purpose: quitting the app does not close a
//! browser someone may be in the middle of using. On macOS that takes starting
//! Chrome through Launch Services. A Chrome spawned as the gateway's child
//! counts, to macOS, as part of Consistent CEO: it was ended along with the app
//! (a "Quit & Reopen" after a permission change closed it mid sign-in), and its
//! own permission requests, such as updating itself, were put to the person in
//! the app's name. The debugging port Chrome picks is written to
//! `<profile>/DevToolsActivePort`, which is how a window that outlived a gateway
//! restart is found again.
//!
//! ### The debugging port
//!
//! An open DevTools port on a signed-in browser is a real exposure, and the
//! rest of the codebase refuses to attach to a person's own Chrome for exactly
//! that reason. These browsers are different in the ways that matter: each is a
//! dedicated profile, not the one holding everything, the port is random and
//! bound to loopback, and it exists only while the window is open. Chrome also
//! refuses DevTools connections from web pages unless told otherwise, so a page
//! in the browser cannot reach it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use consistent_world_tools::chrome;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// How long a freshly started Chrome gets to publish its debugging port.
const START_TIMEOUT: Duration = Duration::from_secs(25);
/// How long a launch may go without a new Chrome taking the profile's lock
/// before it is read as "handed to a window already open on this profile".
const HANDOFF_GRACE: Duration = Duration::from_secs(8);
/// How long a polite close gets before the person is asked to close it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(12);

/// Chromes this gateway started as its own children (Linux, Windows), by
/// profile, so a window closed by hand is reaped rather than left as a zombie.
/// A window started through Launch Services, or found again through
/// `DevToolsActivePort` after a restart, has no entry here and needs none.
static CHILDREN: LazyLock<Mutex<HashMap<PathBuf, Child>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// One open/close at a time: two taps on the button must not start two Chromes.
static LIFECYCLE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Where every entity's browser profile lives.
pub fn browsers_root() -> PathBuf {
    PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world/browsers",
    ))
}

/// The folder name for an entity's profile: the id itself when it is a plain
/// one (every id the app makes is), otherwise a digest of it, marked with a `.`
/// that no plain id contains. Either way no id reaches outside
/// [`browsers_root`] or lands on another entity's sign-ins.
fn profile_folder_name(entity_id: &str) -> String {
    let plain = !entity_id.is_empty()
        && entity_id.len() <= 64
        && entity_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if plain {
        entity_id.to_string()
    } else {
        let digest = Sha256::digest(entity_id.as_bytes());
        format!("id.{}", hex::encode(&digest[..16]))
    }
}

/// An entity's own browser profile: its folder, never the person's Chrome and
/// never another entity's.
pub fn profile_dir(entity_id: &str) -> PathBuf {
    browsers_root().join(profile_folder_name(entity_id))
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
    /// The window has been opened before, so earlier sign-ins are probably still there.
    pub used_before: bool,
    /// Sites open in the window right now.
    pub open_sites: Vec<OpenSite>,
}

fn chrome_name(path: &Path) -> String {
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

/// Which Chrome this computer has, for people: `(found, name)`.
pub fn chrome_available() -> (bool, Option<String>) {
    let found = chrome::find_chrome();
    (found.is_some(), found.as_deref().map(chrome_name))
}

async fn endpoint_at(profile: &Path) -> Option<String> {
    let endpoint = chrome::devtools_endpoint(profile)?;
    chrome::endpoint_alive(&endpoint).await.then_some(endpoint)
}

/// The endpoint of an entity's window if one is really open (the file can be stale).
pub async fn endpoint(entity_id: &str) -> Option<String> {
    endpoint_at(&profile_dir(entity_id)).await
}

/// Reap the children whose windows were closed by hand, so none lingers.
fn reap_children() {
    if let Ok(mut children) = CHILDREN.lock() {
        children.retain(|_, child| !matches!(child.try_wait(), Ok(Some(_))));
    }
}

fn has_child(profile: &Path) -> bool {
    CHILDREN
        .lock()
        .map(|children| children.contains_key(profile))
        .unwrap_or(false)
}

/// How the child started for `profile` ended, if it has (and forget it).
fn child_exited(profile: &Path) -> Option<std::process::ExitStatus> {
    let mut children = CHILDREN.lock().ok()?;
    let exit = children.get_mut(profile)?.try_wait().ok()??;
    children.remove(profile);
    Some(exit)
}

fn kill_child(profile: &Path) {
    let child = CHILDREN
        .lock()
        .ok()
        .and_then(|mut children| children.remove(profile));
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Wait for a child on a thread of its own, so it is reaped without blocking.
fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

async fn status_at(profile: &Path) -> BrowserStatus {
    reap_children();
    let (chrome_found, chrome_name) = chrome_available();
    let endpoint = endpoint_at(profile).await;
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
        chrome_name,
        chrome_found,
        running: endpoint.is_some(),
        // Chrome writes `Local State` the first time it runs on a profile.
        used_before: profile.join("Local State").is_file(),
        profile_dir: profile.to_string_lossy().to_string(),
        open_sites,
    }
}

/// Whether an entity's window is running, and what is open in it.
pub async fn status(entity_id: &str) -> BrowserStatus {
    status_at(&profile_dir(entity_id)).await
}

/// Start Chrome. On macOS, through Launch Services when Chrome is an app
/// bundle (see the module docs for why), which leaves nothing to reap.
/// Elsewhere as a child in a process group of its own, so a signal meant for
/// the gateway's group does not reach the window; the child is returned so the
/// caller can reap it.
async fn launch(exe: &Path, args: Vec<OsString>) -> Result<Option<Child>, String> {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = chrome::app_bundle(exe) {
        let status = tokio::process::Command::new("/usr/bin/open")
            // A new instance, even while the person's own Chrome is running:
            // without `-n` the launch would only bring their Chrome forward.
            .arg("-n")
            .arg("-a")
            .arg(&bundle)
            .arg("--args")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| format!("Could not start Chrome: {e}"))?;
        return if status.success() {
            Ok(None)
        } else {
            Err(format!("Could not start Chrome ({status})."))
        };
    }
    let mut command = Command::new(exe);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .map(Some)
        .map_err(|e| format!("Could not start Chrome: {e}"))
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A page saying whose window this is, shown the first time it opens. With a
/// window per entity, the first thing a person needs to know is which one they
/// are signing in to. Written beside the profile, outside Chrome's own files;
/// returns its `file://` address.
fn write_start_page(profile: &Path, name: &str) -> Option<String> {
    let name = html_escape(name.trim());
    let page = profile.join("consistent-start.html");
    let html = format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>{name} · Consistent</title>\n\
         <style>body{{font:15px/1.55 -apple-system,system-ui,sans-serif;max-width:560px;\
         margin:12vh auto;padding:0 24px;color:#222}}h1{{font-size:22px;margin:0 0 10px}}\
         p{{color:#555;margin:0 0 12px}}</style>\n\
         <h1>{name}’s browser</h1>\n\
         <p>This window belongs to {name}. Sign in here to {name}’s own accounts: its social media, \
         its dashboards, anything with a login. The sign-ins stay in this window, apart from your \
         everyday browser and from your other entities.</p>\n\
         <p>When Consistent CEO works for {name}, it works in this window, in a tab of its own, and \
         only on the sites you allowed in {name}’s Browser tab.</p>\n"
    );
    std::fs::write(&page, html).ok()?;
    reqwest::Url::from_file_path(&page)
        .ok()
        .map(|url| url.to_string())
}

async fn open_at(profile: &Path, name: &str) -> Result<BrowserStatus, String> {
    let _guard = LIFECYCLE.lock().await;
    std::fs::create_dir_all(profile)
        .map_err(|e| format!("Could not create the browser's profile folder: {e}"))?;
    reap_children();
    let exe = chrome::find_chrome().ok_or_else(|| {
        "Google Chrome isn't installed on this computer. Install it from google.com/chrome, \
         then try again."
            .to_string()
    })?;

    if endpoint_at(profile).await.is_some() {
        // Chrome hands a second launch on the same profile to the running one,
        // which opens a window and so brings the browser forward.
        if let Ok(Some(forwarder)) = launch(
            &exe,
            vec![format!("--user-data-dir={}", profile.display()).into()],
        )
        .await
        {
            reap_in_background(forwarder);
        }
        return Ok(status_at(profile).await);
    }

    let first_run = !profile.join("Local State").is_file();
    chrome::brand_new_profile(profile, name);
    chrome::clear_devtools_endpoint(profile);
    let owner_before = chrome::profile_lock_owner(profile);
    let mut args = chrome::launch_args(profile, false);
    if first_run && let Some(page) = write_start_page(profile, name) {
        args.push(page.into());
    }
    if let Some(child) = launch(&exe, args).await?
        && let Ok(mut children) = CHILDREN.lock()
    {
        children.insert(profile.to_path_buf(), child);
    }
    let started = Instant::now();
    loop {
        if endpoint_at(profile).await.is_some() {
            return Ok(status_at(profile).await);
        }
        // A launch that finds a window already open on this profile without a
        // debugging port (an older run, or one started by hand) hands itself to
        // that window and exits: the child ends at once, or, through Launch
        // Services, the profile's lock never changes hands. The window is
        // running, but cannot be driven.
        let handed_off = match child_exited(profile) {
            Some(exit) if !exit.success() => {
                return Err(format!("Chrome closed straight away ({exit}). Try again."));
            }
            Some(_) => true,
            None => {
                !has_child(profile)
                    && owner_before.is_some()
                    && started.elapsed() > HANDOFF_GRACE
                    && chrome::profile_lock_owner(profile) == owner_before
            }
        };
        if handed_off {
            return Err(format!(
                "{}’s browser is already open but can't be controlled from here. Close its \
                 window, then press Open again.",
                name.trim()
            ));
        }
        if started.elapsed() > START_TIMEOUT {
            kill_child(profile);
            return Err("Chrome didn't start in time. Try again.".to_string());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Open an entity's window, or bring its open one to the front. Idempotent.
pub async fn open(entity_id: &str, entity_name: &str) -> Result<BrowserStatus, String> {
    open_at(&profile_dir(entity_id), entity_name).await
}

async fn close_at(profile: &Path) -> Result<BrowserStatus, String> {
    let _guard = LIFECYCLE.lock().await;
    if let Some(ep) = endpoint_at(profile).await {
        chrome::close_browser(&ep)
            .await
            .map_err(|e| format!("Chrome didn't close: {e}"))?;
        let started = Instant::now();
        while chrome::endpoint_alive(&ep).await {
            if started.elapsed() > CLOSE_TIMEOUT {
                return Err(
                    "Chrome is taking a long time to close. Close its window yourself.".to_string(),
                );
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    let child = CHILDREN
        .lock()
        .ok()
        .and_then(|mut children| children.remove(profile));
    if let Some(child) = child {
        reap_in_background(child);
    }
    Ok(status_at(profile).await)
}

/// Close an entity's window politely, so Chrome saves the session it will restore.
pub async fn close(entity_id: &str) -> Result<BrowserStatus, String> {
    close_at(&profile_dir(entity_id)).await
}

/// What a browser task calls first: the endpoint of the entity's window,
/// opening the window if it is closed so the person can watch the work happen.
pub async fn ensure_running(entity_id: &str, entity_name: &str) -> Result<String, String> {
    let profile = profile_dir(entity_id);
    if let Some(ep) = endpoint_at(&profile).await {
        return Ok(ep);
    }
    open_at(&profile, entity_name).await?;
    endpoint_at(&profile)
        .await
        .ok_or_else(|| format!("{}’s browser didn't come up.", entity_name.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entity_has_its_own_profile_under_the_app_data() {
        let root = browsers_root();
        assert!(root.ends_with("browsers"), "{root:?}");
        assert!(root.to_string_lossy().contains(".consistent-world"), "{root:?}");
        let a = profile_dir("a10f4f8a-2070-41de-897e-43cd9c28b6cf");
        let b = profile_dir("personal_default_bearer");
        assert_eq!(a, root.join("a10f4f8a-2070-41de-897e-43cd9c28b6cf"));
        assert_eq!(b, root.join("personal_default_bearer"));
        assert_ne!(a, b);
    }

    #[test]
    fn no_id_reaches_outside_the_root_or_onto_another_entity() {
        let root = browsers_root();
        let too_long = "x".repeat(65);
        for odd in ["", "../escape", "a/b", "..", "with space", too_long.as_str(), "dots.in.id"] {
            let dir = profile_dir(odd);
            assert_eq!(dir.parent(), Some(root.as_path()), "{odd:?} -> {dir:?}");
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("id."), "{odd:?} -> {name}");
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'), "{name}");
        }
        // Different odd ids never share a folder, and never land on a plain one.
        assert_ne!(profile_dir("a/b"), profile_dir("a_b"));
        assert_ne!(profile_dir("a.b"), profile_dir("a_b"));
    }

    #[test]
    fn browsers_are_named_for_people() {
        let n = |s: &str| chrome_name(Path::new(s));
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

    #[test]
    fn the_start_page_says_whose_window_it_is_and_escapes_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let url = write_start_page(dir.path(), "Acme <script>alert(1)</script> & Co").unwrap();
        assert!(url.starts_with("file://"), "{url}");
        let html = std::fs::read_to_string(dir.path().join("consistent-start.html")).unwrap();
        assert!(html.contains("Acme &lt;script&gt;alert(1)&lt;/script&gt; &amp; Co’s browser"));
        assert!(!html.contains("<script>"));
    }

    #[tokio::test]
    async fn a_missing_window_is_not_running_and_has_no_sites() {
        // No window is started by tests, so nothing answers for an entity that
        // has never had one.
        let s = status("an-entity-with-no-browser").await;
        assert!(!s.running);
        assert!(s.open_sites.is_empty());
        assert!(s.profile_dir.ends_with("an-entity-with-no-browser"));
    }

    /// Opens a real, visible window for a few seconds.
    #[tokio::test]
    #[ignore = "requires a real Chrome; opens a visible window briefly"]
    async fn a_window_opens_outlives_its_opener_and_closes_politely() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("entity");
        let opened = open_at(&profile, "Test entity").await.expect("opens");
        assert!(opened.running);
        assert!(opened.used_before, "Chrome has written Local State");
        let ep = endpoint_at(&profile).await.expect("an endpoint");

        // Opening again brings the same window forward instead of starting a second Chrome.
        let owner = chrome::profile_lock_owner(&profile).expect("Chrome holds the profile");
        let again = open_at(&profile, "Test entity").await.expect("brings it forward");
        assert!(again.running);
        assert_eq!(chrome::profile_lock_owner(&profile).as_deref(), Some(owner.as_str()));

        #[cfg(target_os = "macos")]
        {
            // An app of its own: started by launchd, not by whoever opened it, so
            // quitting Consistent CEO cannot take it down.
            let pid = owner.rsplit('-').next().unwrap();
            let out = Command::new("ps").args(["-o", "ppid=", "-p", pid]).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
        }

        let closed = close_at(&profile).await.expect("closes");
        assert!(!closed.running);
        assert!(!chrome::endpoint_alive(&ep).await);
    }
}
