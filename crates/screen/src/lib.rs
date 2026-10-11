//! `consistent-world-screen` — screen perception for the companion loop.
//!
//! Provides the capture primitives the vision/interpreter layer needs:
//! [`capture_active_display`] (the whole primary monitor) and
//! [`capture_focused_window`] (just the frontmost app window).
//!
//! Capture is cross-platform via [`xcap`], but the **Screen Recording
//! permission** on macOS is handled explicitly: a missing grant surfaces as a
//! typed [`ScreenError::PermissionDenied`] — never a panic and never a silently
//! black frame.

use xcap::image::RgbaImage;
use xcap::{Monitor, Window};

/// Errors that capture can fail with. Every failure mode is typed so callers
/// (and the voice loop) can react — especially the macOS permission case.
#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    /// macOS Screen Recording permission has not been granted. The user must
    /// grant it in System Settings → Privacy & Security → Screen Recording.
    #[error(
        "screen recording permission denied — grant it in System Settings → \
         Privacy & Security → Screen Recording, then restart the app"
    )]
    PermissionDenied,

    /// No display was found to capture.
    #[error("no display available to capture")]
    NoDisplay,

    /// No focused/frontmost window could be identified.
    #[error("no focused window found")]
    NoFocusedWindow,

    /// The underlying capture backend failed for some other reason.
    #[error("screen capture failed: {0}")]
    Capture(String),
}

impl From<xcap::XCapError> for ScreenError {
    fn from(e: xcap::XCapError) -> Self {
        ScreenError::Capture(e.to_string())
    }
}

/// Returns `true` if screen capture is permitted right now.
///
/// On macOS this preflights the Screen Recording TCC grant without prompting.
/// On other platforms it is always `true`.
pub fn permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Safe: CGPreflightScreenCaptureAccess is a pure, side-effect-free query.
        unsafe { CGPreflightScreenCaptureAccess() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Asks the OS to prompt the user for Screen Recording permission (macOS only).
///
/// Returns whether access is granted *after* the request. The first call on
/// macOS pops the system dialog; the grant only takes effect after an app
/// restart, so treat a `false` here as "ask the user to restart".
pub fn request_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Safe: CGRequestScreenCaptureAccess only triggers the system prompt.
        unsafe { CGRequestScreenCaptureAccess() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Capture the primary display as an RGBA image.
///
/// Falls back to the first available monitor if none reports itself primary.
pub fn capture_active_display() -> Result<RgbaImage, ScreenError> {
    ensure_permission()?;

    let monitors = Monitor::all()?;
    if monitors.is_empty() {
        return Err(ScreenError::NoDisplay);
    }

    // Prefer the primary monitor; fall back to the first one.
    let mut chosen: Option<&Monitor> = None;
    for m in &monitors {
        if m.is_primary().unwrap_or(false) {
            chosen = Some(m);
            break;
        }
    }
    let monitor = chosen.unwrap_or(&monitors[0]);

    Ok(monitor.capture_image()?)
}

/// Capture just the currently focused (frontmost) window as an RGBA image.
pub fn capture_focused_window() -> Result<RgbaImage, ScreenError> {
    ensure_permission()?;

    let windows = Window::all()?;

    // Find the focused window; skip minimized ones.
    let mut focused: Option<&Window> = None;
    for w in &windows {
        if w.is_minimized().unwrap_or(false) {
            continue;
        }
        if w.is_focused().unwrap_or(false) {
            focused = Some(w);
            break;
        }
    }

    let window = focused.ok_or(ScreenError::NoFocusedWindow)?;
    Ok(window.capture_image()?)
}

/// Short-circuit with a typed permission error before touching the backend,
/// so a missing macOS grant never turns into a black frame or a panic.
fn ensure_permission() -> Result<(), ScreenError> {
    if permission_granted() {
        Ok(())
    } else {
        Err(ScreenError::PermissionDenied)
    }
}
