//! Screen-capture probe — the A1 done-when check.
//!
//! Captures the active display (and, if available, the focused window), writes
//! real PNGs to disk, and prints where they went. A missing macOS Screen
//! Recording grant is reported as a clear message + non-zero exit — never a
//! panic.
//!
//! Run with: `cargo run -p consistent-world-screen --bin screen-probe`

use std::process::ExitCode;

use consistent_world_screen::{
    ScreenError, capture_active_display, capture_focused_window, permission_granted,
    request_permission,
};

fn main() -> ExitCode {
    if !permission_granted() {
        eprintln!("Screen Recording permission is not granted.");
        eprintln!("Requesting it now (a system dialog may appear)...");
        request_permission();
        eprintln!(
            "If you just granted it, fully quit and re-run this probe — \
             the grant only applies to newly launched processes."
        );
        return ExitCode::FAILURE;
    }

    let display_path = "screen-probe-display.png";
    match capture_active_display() {
        Ok(img) => {
            let (w, h) = (img.width(), img.height());
            if let Err(e) = img.save(display_path) {
                eprintln!("captured display but failed to write PNG: {e}");
                return ExitCode::FAILURE;
            }
            println!("✓ active display: {w}x{h} → {display_path}");
        }
        Err(ScreenError::PermissionDenied) => {
            eprintln!("✗ display capture: permission denied (typed, not a panic)");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("✗ display capture failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Focused-window capture is best-effort — absence isn't a probe failure.
    let window_path = "screen-probe-window.png";
    match capture_focused_window() {
        Ok(img) => {
            let (w, h) = (img.width(), img.height());
            match img.save(window_path) {
                Ok(()) => println!("✓ focused window: {w}x{h} → {window_path}"),
                Err(e) => eprintln!("focused window captured but PNG write failed: {e}"),
            }
        }
        Err(e) => println!("• focused window: skipped ({e})"),
    }

    ExitCode::SUCCESS
}
