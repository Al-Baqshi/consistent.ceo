//! Keep the installed app current without the user doing anything.
//!
//! A minute after launch, and every six hours after that, the app asks the
//! public repo's latest GitHub release for `latest.json`. When it lists a newer
//! version for this Mac, the update is downloaded in the background, checked
//! against the update-signing public key in tauri.conf.json (an update not
//! signed by the maintainer's key is refused), and installed over the app on
//! disk. Nothing restarts on its own: the app usually has missions and the
//! phone's connection running, so it posts a notification and adds "Restart to
//! update" to the menu-bar icon, and the new version runs from the next start.
//!
//! `CONSISTENT_UPDATE_ENDPOINT` overrides the feed URL, for testing a release
//! against a local `latest.json` before publishing it.

use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager, Url};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;

const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// The version installed on disk and waiting for a restart, if any.
#[derive(Default)]
pub struct PendingUpdate(pub Mutex<Option<String>>);

impl PendingUpdate {
    pub fn version(&self) -> Option<String> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
}

/// Start the background check loop. Stops once an update is installed: there
/// is nothing more to do until the user restarts into it.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(first_check_delay()).await;
        loop {
            match check_and_install(&app).await {
                Ok(Some(version)) => {
                    eprintln!("[updater] installed {version}; waiting for a restart");
                    break;
                }
                Ok(None) => {}
                // Offline, rate-limited, or a release without this platform:
                // all reasons to try again later, none to bother the user.
                Err(e) => eprintln!("[updater] check failed: {e}"),
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

fn first_check_delay() -> Duration {
    // A test endpoint wants an answer now, not in a minute.
    if std::env::var_os("CONSISTENT_UPDATE_ENDPOINT").is_some() {
        Duration::from_secs(3)
    } else {
        FIRST_CHECK_AFTER
    }
}

/// Check once; download and install a newer version if there is one.
/// Returns the installed version.
async fn check_and_install(app: &AppHandle) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut builder = app.updater_builder();
    if let Ok(url) = std::env::var("CONSISTENT_UPDATE_ENDPOINT") {
        builder = builder.endpoints(vec![Url::parse(&url)?])?;
    }
    let Some(update) = builder.build()?.check().await? else {
        return Ok(None);
    };
    let version = update.version.clone();
    eprintln!(
        "[updater] {} -> {version}: downloading",
        update.current_version
    );
    update.download_and_install(|_, _| {}, || {}).await?;

    if let Some(pending) = app.try_state::<PendingUpdate>() {
        if let Ok(mut slot) = pending.0.lock() {
            *slot = Some(version.clone());
        }
    }
    let _ = app
        .notification()
        .builder()
        .title("Consistent CEO update ready")
        .body(format!(
            "Version {version} is installed. Choose \"Restart to update\" from the menu-bar icon when it suits you."
        ))
        .show();
    crate::refresh_tray_menu(app);
    Ok(Some(version))
}
