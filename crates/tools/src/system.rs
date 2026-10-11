//! OS-level integrations on macOS: calendar, reminders, notifications, clipboard.
//! All actions can be gated by a deny list (e.g. from config.denyCommands).

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::types::ToolResult;
// Only the macOS code paths shell out; on other platforms this would be an
// unused import (which CI's `-D warnings` rightly rejects).
#[cfg(target_os = "macos")]
use tokio::process::Command;

/// The `system` tool: macOS calendar/reminders/notification/clipboard actions, each checked
/// against `denied_commands` before it runs.
pub struct SystemTool {
    /// If an action (e.g. "calendar.add", "clipboard.write") is in this list, it is rejected.
    pub denied_commands: Vec<String>,
}

impl SystemTool {
    pub fn new(denied_commands: Vec<String>) -> Self {
        Self { denied_commands }
    }

    // Deny by prefix match, so a single entry like "calendar" can block every calendar.* action.
    fn is_denied(&self, action: &str) -> bool {
        self.denied_commands
            .iter()
            .any(|d| !d.is_empty() && action.starts_with(d.as_str()))
    }
}

#[async_trait]
impl Tool for SystemTool {
    fn name(&self) -> &'static str {
        "system"
    }

    fn description(&self) -> &'static str {
        "macOS: calendar list/add, reminders list/add, notification.send, clipboard read/write (gated by deny list)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": [
                    "calendar.list", "calendar.add",
                    "reminders.list", "reminders.add",
                    "notification.send",
                    "clipboard.read", "clipboard.write"
                ]},
                "params": { "type": "object" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("system tool missing `action`"))?;

        if self.is_denied(action) {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Action denied by policy: {}", action),
                is_error: true,
            });
        }

        #[cfg(not(target_os = "macos"))]
        {
            return Ok(ToolResult {
                call_id: String::new(),
                output: "System tool is only available on macOS".to_string(),
                is_error: true,
            });
        }

        #[cfg(target_os = "macos")]
        {
            let params = arguments.get("params").and_then(|v| v.as_object());
            let output = match action {
                "calendar.list" => {
                    run_osascript(
                        r#"
                    tell application "Calendar" to get name of every calendar
                    return result as text
                    "#,
                    )
                    .await
                }
                "calendar.add" => {
                    // Escape quotes before interpolating into the AppleScript string literal
                    // below, so a title containing `"` cannot break out of it.
                    let title = params
                        .and_then(|p| p.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .replace('"', "\\\"");
                    let start = params
                        .and_then(|p| p.get("start"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if title.is_empty() {
                        "calendar.add requires params.title".to_string()
                    } else {
                        run_osascript(&format!(
                            r#"
                            tell application "Calendar"
                                set cal to first calendar
                                make new event at cal with properties {{summary:"{}", start date:date "{}"}}
                            end tell
                            return "OK"
                            "#,
                            title,
                            if start.is_empty() { "2025-01-01 12:00:00" } else { start }
                        ))
                        .await
                    }
                }
                "reminders.list" => {
                    run_osascript(
                        r#"
                    tell application "Reminders" to get name of every list
                    return result as text
                    "#,
                    )
                    .await
                }
                "reminders.add" => {
                    let title = params
                        .and_then(|p| p.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .replace('"', "\\\"");
                    let list_name = params
                        .and_then(|p| p.get("list"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Reminders");
                    if title.is_empty() {
                        "reminders.add requires params.title".to_string()
                    } else {
                        run_osascript(&format!(
                            r#"
                            tell application "Reminders"
                                tell list "{}"
                                    make new reminder with properties {{name:"{}"}}
                                end tell
                            end tell
                            return "OK"
                            "#,
                            list_name.replace('"', "\\\""),
                            title
                        ))
                        .await
                    }
                }
                "notification.send" => {
                    let title = params
                        .and_then(|p| p.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Consistent")
                        .replace('"', "\\\"");
                    let body = params
                        .and_then(|p| p.get("body"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .replace('"', "\\\"");
                    run_osascript(&format!(
                        r#"
                        display notification "{}" with title "{}"
                        return "OK"
                        "#,
                        body, title
                    ))
                    .await
                }
                "clipboard.read" => run_cmd("pbpaste", &[]).await,
                "clipboard.write" => {
                    let text = params
                        .and_then(|p| p.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        "clipboard.write requires params.text".to_string()
                    } else {
                        run_pipe_stdin("pbcopy", text.as_bytes()).await
                    }
                }
                _ => format!("Unknown action: {}", action),
            };

            // osascript has no structured success/failure signal here (stdout and stderr were
            // already merged above), so failure is inferred from the text AppleScript reports.
            let is_error = output.contains("error") || output.contains("Execution error");
            Ok(ToolResult {
                call_id: String::new(),
                output: output.trim().to_string(),
                is_error,
            })
        }
    }
}

// Run one AppleScript snippet via `osascript -e`. Newlines are flattened to spaces because
// `-e` takes the script as a single argument.
#[cfg(target_os = "macos")]
async fn run_osascript(script: &str) -> String {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script.trim().replace('\n', " "))
        .output()
        .await;
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if !o.stderr.is_empty() {
                s.push_str(&String::from_utf8_lossy(&o.stderr));
            }
            if !o.status.success() {
                s.push_str(&format!(" (exit {})", o.status));
            }
            s
        }
        Err(e) => format!("osascript failed: {}", e),
    }
}

// Run a plain command (e.g. `pbpaste`) and return its combined stdout/stderr.
#[cfg(target_os = "macos")]
async fn run_cmd(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd).args(args).output().await;
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            if !o.stderr.is_empty() {
                s.push_str(&String::from_utf8_lossy(&o.stderr));
            }
            s
        }
        Err(e) => format!("{} failed: {}", cmd, e),
    }
}

// Run a command (e.g. `pbcopy`) that reads its input from stdin instead of an argument.
#[cfg(target_os = "macos")]
async fn run_pipe_stdin(cmd: &str, stdin: &[u8]) -> String {
    use tokio::io::AsyncWriteExt;
    let child = Command::new(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn();
    match child {
        Ok(mut c) => {
            if let Some(mut stdin_handle) = c.stdin.take() {
                let _ = stdin_handle.write_all(stdin).await;
                let _ = stdin_handle.shutdown().await;
            }
            match c.wait_with_output().await {
                Ok(o) => {
                    if o.status.success() {
                        "OK".to_string()
                    } else {
                        String::from_utf8_lossy(&o.stderr).into_owned()
                    }
                }
                Err(e) => format!("{} failed: {}", cmd, e),
            }
        }
        Err(e) => format!("{} spawn failed: {}", cmd, e),
    }
}
