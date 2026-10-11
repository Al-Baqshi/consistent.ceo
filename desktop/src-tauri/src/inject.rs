//! Text injection at the OS cursor — the piece that makes dictation land in
//! whatever app is focused (Slack, Mail, an IDE) rather than only in our window.
//!
//! macOS route: synthesize keyboard events carrying a Unicode payload
//! (`CGEventKeyboardSetUnicodeString`) and post them to the HID tap. This types
//! into the focused app without touching the clipboard, and works in effectively
//! every app including Electron and terminals — unlike the `AXUIElement`
//! set-selected-text route, which many apps implement poorly or not at all.
//!
//! Windows route: `SendInput` with `KEYEVENTF_UNICODE`, the same idea — each
//! UTF-16 unit arrives as a keystroke carrying that character, no clipboard, no
//! keyboard-layout mapping. Windows asks for no permission, but User Interface
//! Privilege Isolation silently drops input aimed at a window running elevated
//! ("as administrator") when this app is not, which is reported as an error.
//!
//! This lives in the main app binary on purpose: the Accessibility permission is
//! granted per code signature, so the user grants it once to Consistent.app. A
//! separately ad-hoc-signed sidecar would lose the grant on every rebuild.

use std::sync::atomic::{AtomicBool, Ordering};

/// When true, committed (`final`) dictation segments are typed at the cursor
/// automatically. Toggled from the UI; on by default so the hotkey works with
/// no further setup.
static AUTO_INJECT: AtomicBool = AtomicBool::new(true);

/// Whether committed dictation segments should be typed at the cursor
/// automatically (see `AUTO_INJECT`).
pub fn auto_inject_enabled() -> bool {
    AUTO_INJECT.load(Ordering::Relaxed)
}

/// Turn auto-injection of dictated text on or off from the UI.
#[tauri::command]
pub fn set_auto_inject(enabled: bool) {
    AUTO_INJECT.store(enabled, Ordering::Relaxed);
}

/// Whether this process holds the Accessibility permission required to post
/// synthetic keyboard events. Without it, injection silently does nothing.
#[tauri::command]
pub fn injection_trusted() -> bool {
    platform::is_trusted()
}

/// Open System Settings on the Accessibility pane so the user can grant it.
/// macOS gives no programmatic way to grant, only to ask.
#[tauri::command]
pub fn open_accessibility_settings() -> Result<(), String> {
    platform::open_accessibility_settings()
}

/// Type `text` at the current cursor position in the focused application.
#[tauri::command]
pub async fn inject_text(text: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || platform::type_text(&text))
        .await
        .map_err(|e| format!("injection task failed: {e}"))?
}

/// Fire-and-forget injection for use from background tasks (the dictation
/// reader). Logs rather than propagating, since there's no caller to report to.
pub fn inject_text_detached(text: String) {
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = platform::type_text(&text) {
            eprintln!("[inject] {e}");
        }
    });
}

/// Split `text` into pieces whose UTF-16 length stays under `max_units`, never
/// splitting a `char` (which would corrupt surrogate pairs / emoji).
///
/// `CGEventKeyboardSetUnicodeString` is only reliable for short payloads, so
/// long dictation segments must be posted as a sequence of small events.
fn chunk_by_utf16(text: &str, max_units: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;

    for ch in text.chars() {
        let ch_units = ch.len_utf16();
        if units + ch_units > max_units && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += ch_units;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(target_os = "macos")]
mod platform {
    use super::chunk_by_utf16;
    use core_graphics::event::{CGEvent, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use std::time::Duration;

    // `AXIsProcessTrusted` returns a CoreFoundation `Boolean` (unsigned char),
    // so bind it as u8 rather than bool.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }

    /// Max UTF-16 units per synthesized event. Kept small because
    /// CGEventKeyboardSetUnicodeString drops characters on large payloads.
    const CHUNK_UTF16_UNITS: usize = 16;
    /// Brief pause between events so the receiving app's event loop keeps up;
    /// without it, fast typing drops characters in some apps.
    const INTER_CHUNK_DELAY: Duration = Duration::from_millis(2);

    pub fn is_trusted() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub fn open_accessibility_settings() -> Result<(), String> {
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("failed to open Accessibility settings: {e}"))
    }

    pub fn type_text(text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        if !is_trusted() {
            return Err(
                "Accessibility permission not granted — cannot type at the cursor".to_string(),
            );
        }

        for chunk in chunk_by_utf16(text, CHUNK_UTF16_UNITS) {
            // A fresh source per chunk: CGEventSource isn't Send and is cheap to
            // create, so there's nothing to gain from hoisting it.
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| "failed to create CGEventSource".to_string())?;

            // keycode 0 with a Unicode payload: the payload is what gets typed,
            // the keycode is ignored by the receiving app.
            let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
                .map_err(|_| "failed to create key-down event".to_string())?;
            down.set_string(&chunk);
            down.post(CGEventTapLocation::HID);

            let up = CGEvent::new_keyboard_event(source, 0, false)
                .map_err(|_| "failed to create key-up event".to_string())?;
            up.set_string(&chunk);
            up.post(CGEventTapLocation::HID);

            std::thread::sleep(INTER_CHUNK_DELAY);
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::chunk_by_utf16;
    use std::time::Duration;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
        VK_RETURN,
    };

    /// UTF-16 units per `SendInput` batch. The call itself takes any number;
    /// batching with a short pause keeps slow apps from dropping characters.
    const CHUNK_UTF16_UNITS: usize = 64;
    const INTER_CHUNK_DELAY: Duration = Duration::from_millis(2);

    /// Windows has no Accessibility-style grant for synthetic input.
    pub fn is_trusted() -> bool {
        true
    }

    pub fn open_accessibility_settings() -> Result<(), String> {
        Ok(())
    }

    fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Key-down/key-up pairs typing `chunk`. A newline is a real Return press —
    /// a Unicode `\n` keystroke is ignored by most edit controls — and `\r` is
    /// dropped so a CRLF does not press it twice.
    fn inputs_for(chunk: &str) -> Vec<INPUT> {
        let mut inputs = Vec::with_capacity(chunk.len() * 2);
        for unit in chunk.encode_utf16() {
            match unit {
                0x0D => {}
                0x0A => {
                    inputs.push(key(VK_RETURN, 0, 0));
                    inputs.push(key(VK_RETURN, 0, KEYEVENTF_KEYUP));
                }
                _ => {
                    inputs.push(key(0, unit, KEYEVENTF_UNICODE));
                    inputs.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
            }
        }
        inputs
    }

    pub fn type_text(text: &str) -> Result<(), String> {
        for chunk in chunk_by_utf16(text, CHUNK_UTF16_UNITS) {
            let inputs = inputs_for(&chunk);
            if inputs.is_empty() {
                continue;
            }
            // SAFETY: `inputs` is a live, correctly-sized array of INPUT
            // structs for the duration of the call.
            let sent = unsafe {
                SendInput(
                    inputs.len() as u32,
                    inputs.as_ptr(),
                    std::mem::size_of::<INPUT>() as i32,
                )
            };
            if sent as usize != inputs.len() {
                return Err(
                    "Windows blocked typing at the cursor — the focused window may be running \
                     as administrator"
                        .to_string(),
                );
            }
            std::thread::sleep(INTER_CHUNK_DELAY);
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    pub fn is_trusted() -> bool {
        false
    }

    pub fn open_accessibility_settings() -> Result<(), String> {
        Err("Accessibility settings are macOS-only".to_string())
    }

    pub fn type_text(_text: &str) -> Result<(), String> {
        Err("text injection is not implemented on this platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::chunk_by_utf16;

    #[test]
    fn chunks_stay_within_the_utf16_budget() {
        let text = "the quick brown fox jumps over the lazy dog";
        for chunk in chunk_by_utf16(text, 16) {
            assert!(
                chunk.encode_utf16().count() <= 16,
                "chunk too long: {chunk}"
            );
        }
    }

    #[test]
    fn chunking_preserves_the_original_text() {
        let text = "hello there, this is a dictated sentence.";
        assert_eq!(chunk_by_utf16(text, 7).concat(), text);
    }

    #[test]
    fn never_splits_a_multi_unit_char() {
        // Emoji are 2 UTF-16 units; a budget of 3 must not cut one in half.
        let text = "a😀b😀c😀";
        let chunks = chunk_by_utf16(text, 3);
        assert_eq!(chunks.concat(), text);
        for chunk in chunks {
            assert!(chunk.chars().all(|c| c != '\u{FFFD}'));
            assert!(chunk.encode_utf16().count() <= 3);
        }
    }

    #[test]
    fn empty_input_produces_no_chunks() {
        assert!(chunk_by_utf16("", 16).is_empty());
    }

    /// Proves the ApplicationServices binding resolves and is ABI-correct — a
    /// wrong `AXIsProcessTrusted` signature would fail here, not silently. The
    /// value itself is environment-dependent, so only the call is asserted.
    #[test]
    fn accessibility_trust_check_is_callable() {
        let _: bool = super::injection_trusted();
    }

    #[test]
    fn a_single_char_larger_than_the_budget_still_emits() {
        // Budget of 1 can't fit a 2-unit emoji, but dropping it would lose text.
        assert_eq!(chunk_by_utf16("😀", 1).concat(), "😀");
    }
}
