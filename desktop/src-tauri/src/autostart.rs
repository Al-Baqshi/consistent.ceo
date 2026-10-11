//! Start with the computer, quietly.
//!
//! Consistent CEO does its work in the background (missions, heartbeats,
//! reminders, the phone's connection), so it is only useful while the gateway
//! runs. On first launch the app registers itself to start at login (a
//! LaunchAgent on macOS, the Run key on Windows, an XDG autostart entry on
//! Linux) with [`HIDDEN_ARG`], which starts it in the tray with no window.
//! Settings can turn it off; that choice is never overridden, because the
//! default is applied exactly once per install (a marker file in the app's
//! data directory).

use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;

/// Passed by the login item so a start at login opens no window.
pub const HIDDEN_ARG: &str = "--hidden";

const DEFAULT_APPLIED_MARKER: &str = "autostart_default_applied";

/// Whether this process was started by the login item.
pub fn launched_hidden() -> bool {
    std::env::args().any(|a| a == HIDDEN_ARG)
}

/// Turn start-at-login on the first time the app runs, and never again.
pub fn apply_default(app: &tauri::AppHandle) {
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let marker = dir.join(DEFAULT_APPLIED_MARKER);
    if marker.exists() {
        return;
    }
    match app.autolaunch().enable() {
        Ok(()) => {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&marker, b"1");
        }
        Err(e) => eprintln!("Could not register start-at-login: {e}"),
    }
}

/// Whether the app starts at login.
#[tauri::command]
pub fn autostart_status(app: tauri::AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Turn start-at-login on or off. Returns the new state.
#[tauri::command]
pub fn autostart_set(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable().map_err(|e| e.to_string())?;
    } else {
        launcher.disable().map_err(|e| e.to_string())?;
    }
    launcher.is_enabled().map_err(|e| e.to_string())
}
