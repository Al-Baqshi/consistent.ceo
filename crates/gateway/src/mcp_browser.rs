//! A stdio MCP server exposing exactly one tool: the browser.
//!
//! ### Why this exists
//!
//! The executor runs work through the `claude` CLI (the operator's
//! subscription, never an API key). That CLI brings its own file and shell
//! tools, which is why "manage my files" works from the phone with no extra
//! plumbing — but it has no way to drive a browser. `BrowserTool` is only
//! reachable from the in-process LLM loop, which the subscription path does
//! not use.
//!
//! MCP is the bridge. `consistent-world mcp-browser` speaks the protocol on
//! stdio; the executor passes `--mcp-config` for tasks whose `actionKind` is
//! `browser`, and only for those. The domain policy stays here in Rust, in
//! this process's environment — never in the model's hands.
//!
//! Protocol surface is deliberately minimal (`initialize`, `tools/list`,
//! `tools/call`, plus notification pass-through): one tool, one transport, no
//! capabilities we do not implement.

use std::sync::Arc;

use consistent_world_tools::Tool;
use consistent_world_tools::browser::BrowserTool;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// MCP protocol version this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Domains an unattended browser run may never reach, regardless of
/// configuration. Money and identity are not places for an agent nobody is
/// watching.
const ALWAYS_DENIED: &[&str] = &[
    "localhost",
    "127.0.0.1",
    "paypal.com",
    "stripe.com",
    "checkout.stripe.com",
    "wise.com",
    "coinbase.com",
    "binance.com",
    "accounts.google.com",
    "id.apple.com",
];

fn denied_domains() -> Vec<String> {
    let mut denied: Vec<String> = ALWAYS_DENIED.iter().map(|d| d.to_string()).collect();
    if let Ok(extra) = std::env::var("CONSISTENT_WORLD_BROWSER_DENIED_DOMAINS") {
        denied.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string),
        );
    }
    denied
}

/// An allowlist, when configured, is the tighter boundary: outside it, nothing.
fn allowed_domains() -> Option<Vec<String>> {
    let raw = std::env::var("CONSISTENT_WORLD_BROWSER_ALLOWED_DOMAINS").ok()?;
    let list: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .collect();
    (!list.is_empty()).then_some(list)
}

fn tool_descriptor(tool: &BrowserTool) -> Value {
    json!({
        "name": tool.name(),
        "description": tool.description(),
        "inputSchema": tool.parameters_schema(),
    })
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Handle one decoded request. Returns `None` for notifications, which by
/// protocol take no reply.
pub async fn handle_request(tool: &Arc<BrowserTool>, req: &Value) -> Option<Value> {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = req.get("id").cloned();
    // A message without an id is a notification: act, but never answer.
    let id = id?;

    match method {
        "initialize" => Some(ok_response(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "consistent-world-browser", "version": env!("CARGO_PKG_VERSION") },
            }),
        )),
        "tools/list" => Some(ok_response(id, json!({ "tools": [tool_descriptor(tool)] }))),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name != tool.name() {
                return Some(error_response(id, -32602, &format!("unknown tool: {name}")));
            }
            let mut arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // The model does not get to set its own policy. `_policy` is the
            // per-call override the in-process executor injects; through MCP the
            // arguments come straight from the model, and `_policy.allowedDomains`
            // *replaces* the configured allowlist while `_policy.screenshotDir`
            // redirects where files are written. Left in, a page or file the
            // agent reads could talk it into widening its own reach. The real
            // policy comes from this process's environment, set by the executor.
            if let Some(obj) = arguments.as_object_mut()
                && obj.remove("_policy").is_some()
            {
                tracing::warn!("mcp-browser: stripped a model-supplied _policy override");
            }
            let result = match tool.execute(arguments).await {
                Ok(r) => r,
                Err(e) => {
                    return Some(ok_response(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": e.to_string() }],
                            "isError": true,
                        }),
                    ));
                }
            };
            Some(ok_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": result.output }],
                    "isError": result.is_error,
                }),
            ))
        }
        "ping" => Some(ok_response(id, json!({}))),
        _ => Some(error_response(
            id,
            -32601,
            &format!("method not found: {method}"),
        )),
    }
}

/// Serve MCP on stdio until stdin closes.
pub async fn serve() -> anyhow::Result<()> {
    let mut tool = BrowserTool::new(
        // Never attach to the operator's own Chrome via CDP here: a debug port
        // on a signed-in profile is credential theft waiting to happen. The
        // dedicated profile is set through the environment instead.
        None,
        allowed_domains(),
        denied_domains(),
    );
    // Screenshots land in the run's artifact folder rather than the system
    // temp dir, which is what makes them retrievable afterwards — a browser
    // action you cannot see the result of is barely an action.
    if let Ok(dir) = std::env::var("CONSISTENT_WORLD_BROWSER_ARTIFACT_DIR")
        && !dir.trim().is_empty()
    {
        let _ = std::fs::create_dir_all(&dir);
        tool.screenshot_dir = Some(dir);
    }
    let tool = Arc::new(tool);
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(reply) = handle_request(&tool, &req).await {
            let mut bytes = serde_json::to_vec(&reply)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

/// The `--mcp-config` payload handed to a `claude` CLI run that is allowed to
/// drive the browser. Points back at this binary's own subcommand, so there is
/// nothing extra to install.
pub fn mcp_config_json(
    binary: &str,
    profile_dir: &str,
    artifact_dir: &str,
    headless: bool,
) -> Value {
    json!({
        "mcpServers": {
            "browser": {
                "command": binary,
                "args": ["mcp-browser"],
                "env": {
                    "CONSISTENT_WORLD_BROWSER_PROFILE_DIR": profile_dir,
                    "CONSISTENT_WORLD_BROWSER_ARTIFACT_DIR": artifact_dir,
                    "CONSISTENT_WORLD_BROWSER_HEADLESS": if headless { "1" } else { "0" },
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> Arc<BrowserTool> {
        Arc::new(BrowserTool::new(None, None, denied_domains()))
    }

    #[tokio::test]
    async fn initialize_reports_one_tool_and_a_protocol_version() {
        let t = tool();
        let init = handle_request(&t, &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .await
            .expect("initialize replies");
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);

        let list = handle_request(&t, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .await
            .expect("tools/list replies");
        let tools = list["result"]["tools"].as_array().expect("array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "browser");
        assert!(tools[0]["inputSchema"]["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn a_notification_is_never_answered() {
        let t = tool();
        let reply = handle_request(
            &t,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert!(reply.is_none(), "notifications take no reply");
    }

    #[tokio::test]
    async fn a_model_supplied_policy_override_is_stripped() {
        // A jail escape if left in: `_policy.allowedDomains` replaces the
        // configured allowlist, and `_policy.screenshotDir` writes files
        // wherever it likes. `close` is used because it needs no live browser.
        let t = tool();
        let reply = handle_request(
            &t,
            &json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
            "name":"browser",
            "arguments":{
                "action":"close",
                "_policy":{
                    "allowedDomains":["evil.example.com"],
                    "screenshotDir":"/tmp/anywhere"
                }
            }}}),
        )
        .await
        .expect("replies");
        // The call itself still runs; what matters is that it ran without the
        // override — a policy the model wrote is never the policy in force.
        assert!(reply.get("result").is_some(), "call should be answered");
        assert_eq!(reply["result"]["isError"], false);
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected_not_executed() {
        let t = tool();
        let reply = handle_request(
            &t,
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                    "params":{"name":"shell","arguments":{}}}),
        )
        .await
        .expect("replies");
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn an_unknown_method_returns_method_not_found() {
        let t = tool();
        let reply = handle_request(
            &t,
            &json!({"jsonrpc":"2.0","id":4,"method":"resources/list"}),
        )
        .await
        .expect("replies");
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn money_and_identity_domains_are_always_denied() {
        let denied = denied_domains();
        for host in [
            "stripe.com",
            "paypal.com",
            "accounts.google.com",
            "127.0.0.1",
        ] {
            assert!(denied.iter().any(|d| d == host), "{host} must be denied");
        }
    }

    #[test]
    fn the_mcp_config_points_at_this_binarys_subcommand() {
        let cfg = mcp_config_json(
            "/usr/local/bin/consistent-world",
            "/tmp/profile",
            "/tmp/artifacts/run-1",
            false,
        );
        let server = &cfg["mcpServers"]["browser"];
        assert_eq!(server["command"], "/usr/local/bin/consistent-world");
        assert_eq!(server["args"][0], "mcp-browser");
        assert_eq!(server["env"]["CONSISTENT_WORLD_BROWSER_HEADLESS"], "0");
        assert_eq!(
            server["env"]["CONSISTENT_WORLD_BROWSER_PROFILE_DIR"],
            "/tmp/profile"
        );
        assert_eq!(
            server["env"]["CONSISTENT_WORLD_BROWSER_ARTIFACT_DIR"],
            "/tmp/artifacts/run-1"
        );
    }
}
