//! User-configurable global shortcuts, Wispr-Flow style.
//!
//! The three actions (show/hide window, dictate, command mode) ship with the
//! same defaults they always had, but the bindings now live in
//! `<app-data>/shortcuts.json` and can be rebound from the Settings page via
//! the `shortcuts_*` commands. Dictation bindings additionally carry a `hold`
//! flag: `false` is hands-free (press toggles start/stop), `true` is
//! push-to-talk (listen while the key is down, stop on release).
//!
//! Registration is dynamic: the plugin's handler closure never captures a
//! shortcut — it looks the pressed key up in [`ShortcutsState`], so a rebind
//! only has to swap OS registrations and update that table.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::dictation;

/// The three hotkey-triggered actions the user can rebind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleWindow,
    Dictate,
    Command,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::ToggleWindow => "Show/hide window",
            Action::Dictate => "Dictation",
            Action::Command => "Voice command",
        }
    }
}

/// One shortcut's key combination plus its hold behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub accelerator: String,
    /// Push-to-talk: listen while held, stop on release. Ignored for the
    /// window toggle, which has no meaningful "hold" behaviour.
    #[serde(default)]
    pub hold: bool,
}

/// The full set of user-configurable bindings, persisted to `shortcuts.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShortcutsConfig {
    pub toggle_window: Binding,
    pub dictate: Binding,
    pub command: Binding,
}

impl Default for ShortcutsConfig {
    fn default() -> Self {
        Self {
            toggle_window: Binding {
                accelerator: "CmdOrCtrl+Shift+M".into(),
                hold: false,
            },
            dictate: Binding {
                accelerator: "CmdOrCtrl+Shift+D".into(),
                hold: false,
            },
            command: Binding {
                accelerator: "CmdOrCtrl+Shift+A".into(),
                hold: false,
            },
        }
    }
}

impl ShortcutsConfig {
    fn binding(&self, action: Action) -> &Binding {
        match action {
            Action::ToggleWindow => &self.toggle_window,
            Action::Dictate => &self.dictate,
            Action::Command => &self.command,
        }
    }

    fn binding_mut(&mut self, action: Action) -> &mut Binding {
        match action {
            Action::ToggleWindow => &mut self.toggle_window,
            Action::Dictate => &mut self.dictate,
            Action::Command => &mut self.command,
        }
    }
}

// Every configurable action, for iterating registration and lookup together.
const ACTIONS: [Action; 3] = [Action::ToggleWindow, Action::Dictate, Action::Command];

/// Active config plus the parsed lookup table the hotkey handler dispatches on.
#[derive(Default)]
pub struct ShortcutsState {
    config: Mutex<ShortcutsConfig>,
    registered: Mutex<Vec<(Shortcut, Action, bool)>>,
}

fn config_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("shortcuts.json"))
}

fn load(app: &AppHandle) -> ShortcutsConfig {
    config_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save(app: &AppHandle, cfg: &ShortcutsConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let raw = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| e.to_string())
}

fn parse(accelerator: &str) -> Result<Shortcut, String> {
    accelerator
        .parse::<Shortcut>()
        .map_err(|e| format!("Unrecognised shortcut '{accelerator}': {e}"))
}

/// A bare letter as a global shortcut would swallow that key in every app.
/// Require a modifier unless the key stands alone by design (F1–F24).
fn validate(sc: &Shortcut) -> Result<(), String> {
    if !sc.mods.is_empty() {
        return Ok(());
    }
    let key = format!("{:?}", sc.key);
    let is_fn_key = key
        .strip_prefix('F')
        .map(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);
    if is_fn_key {
        Ok(())
    } else {
        Err(
            "Global shortcuts need at least one modifier (⌘/Ctrl/Alt/Shift), or a function key."
                .into(),
        )
    }
}

/// Swap OS registrations to match `cfg`. Returns one error string per action
/// that could not be bound (parse failure or the OS refusing the key); the
/// others still work.
fn apply(app: &AppHandle, cfg: &ShortcutsConfig) -> Vec<(Action, String)> {
    let state = app.state::<ShortcutsState>();
    let gs = app.global_shortcut();
    let mut errors = Vec::new();

    let mut registered = state.registered.lock().unwrap();
    for (sc, _, _) in registered.drain(..) {
        let _ = gs.unregister(sc);
    }

    for action in ACTIONS {
        let binding = cfg.binding(action);
        let sc = match parse(&binding.accelerator) {
            Ok(sc) => sc,
            Err(e) => {
                errors.push((action, e));
                continue;
            }
        };
        if let Err(e) = gs.register(sc) {
            errors.push((action, format!("could not register: {e}")));
            continue;
        }
        registered.push((sc, action, binding.hold));
    }
    errors
}

/// Load the persisted config and bind everything. Called once from setup.
pub fn init(app: &AppHandle) {
    let cfg = load(app);
    for (action, e) in apply(app, &cfg) {
        eprintln!("[shortcuts] {}: {e}", action.label());
    }
    *app.state::<ShortcutsState>().config.lock().unwrap() = cfg;
}

/// The plugin's single handler: look the shortcut up and dispatch.
pub fn on_shortcut(
    app: &AppHandle,
    shortcut: &Shortcut,
    event: tauri_plugin_global_shortcut::ShortcutEvent,
) {
    let entry = app
        .state::<ShortcutsState>()
        .registered
        .lock()
        .unwrap()
        .iter()
        .find(|(sc, _, _)| sc == shortcut)
        .copied();
    let Some((_, action, hold)) = entry else {
        return;
    };

    match event.state() {
        ShortcutState::Pressed => match action {
            Action::ToggleWindow => crate::toggle_main_window(app),
            Action::Dictate | Action::Command => {
                let mode = if action == Action::Dictate {
                    dictation::Mode::Dictate
                } else {
                    dictation::Mode::Command
                };
                let _ = app.emit(
                    "dictation-hotkey",
                    serde_json::json!({
                        "mode": if action == Action::Dictate { "dictate" } else { "command" },
                    }),
                );
                if hold {
                    dictation::start(app, mode);
                } else {
                    dictation::toggle(app, mode);
                }
            }
        },
        ShortcutState::Released => {
            // Only push-to-talk cares about release: let go → stop listening.
            if hold && matches!(action, Action::Dictate | Action::Command) {
                dictation::stop(app);
            }
        }
    }
}

/// Return the currently active shortcut bindings.
#[tauri::command]
pub fn shortcuts_get(app: AppHandle) -> ShortcutsConfig {
    app.state::<ShortcutsState>().config.lock().unwrap().clone()
}

/// Rebind one action to a new accelerator, validating it and re-registering
/// with the OS before persisting.
#[tauri::command]
pub fn shortcuts_set(
    app: AppHandle,
    action: String,
    accelerator: String,
    hold: bool,
) -> Result<ShortcutsConfig, String> {
    let action = match action.as_str() {
        "toggleWindow" => Action::ToggleWindow,
        "dictate" => Action::Dictate,
        "command" => Action::Command,
        other => return Err(format!("Unknown shortcut action '{other}'")),
    };
    let sc = parse(&accelerator)?;
    validate(&sc)?;

    let previous = app.state::<ShortcutsState>().config.lock().unwrap().clone();

    // Refuse a key already bound to a different action — silently stealing it
    // would leave the other action dead with no feedback.
    for other in ACTIONS {
        if other == action {
            continue;
        }
        if let Ok(other_sc) = parse(&previous.binding(other).accelerator) {
            if other_sc == sc {
                return Err(format!("Already used by \"{}\".", other.label()));
            }
        }
    }

    let mut cfg = previous.clone();
    *cfg.binding_mut(action) = Binding { accelerator, hold };

    let errors = apply(&app, &cfg);
    if let Some((_, e)) = errors.iter().find(|(a, _)| *a == action) {
        // The OS rejected the new key (likely taken by another app): put the
        // old bindings back so nothing is left unbound.
        let e = e.clone();
        apply(&app, &previous);
        return Err(e);
    }

    save(&app, &cfg)?;
    *app.state::<ShortcutsState>().config.lock().unwrap() = cfg.clone();
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_validate() {
        let cfg = ShortcutsConfig::default();
        for action in ACTIONS {
            let sc = parse(&cfg.binding(action).accelerator).expect("default must parse");
            validate(&sc).expect("default must validate");
        }
    }

    #[test]
    fn raw_keyboard_event_codes_parse() {
        // The frontend recorder sends KeyboardEvent.code tokens verbatim.
        for acc in ["CmdOrCtrl+Shift+KeyD", "Alt+Space", "Ctrl+Digit3", "F5"] {
            parse(acc).unwrap_or_else(|e| panic!("{acc}: {e}"));
        }
    }

    #[test]
    fn bare_keys_are_rejected_but_function_keys_pass() {
        validate(&parse("KeyD").unwrap()).expect_err("bare letter must be rejected");
        validate(&parse("F6").unwrap()).expect("bare function key is fine");
    }

    #[test]
    fn config_survives_a_json_round_trip() {
        let mut cfg = ShortcutsConfig::default();
        cfg.dictate = Binding {
            accelerator: "Alt+Space".into(),
            hold: true,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<ShortcutsConfig>(&json).unwrap(), cfg);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg: ShortcutsConfig =
            serde_json::from_str(r#"{"dictate":{"accelerator":"Alt+KeyZ"}}"#).unwrap();
        assert_eq!(cfg.dictate.accelerator, "Alt+KeyZ");
        assert!(!cfg.dictate.hold);
        assert_eq!(cfg.command, ShortcutsConfig::default().command);
    }
}

/// Restore every shortcut to its default binding.
#[tauri::command]
pub fn shortcuts_reset(app: AppHandle) -> Result<ShortcutsConfig, String> {
    let cfg = ShortcutsConfig::default();
    for (action, e) in apply(&app, &cfg) {
        eprintln!("[shortcuts] reset: {}: {e}", action.label());
    }
    save(&app, &cfg)?;
    *app.state::<ShortcutsState>().config.lock().unwrap() = cfg.clone();
    Ok(cfg)
}
