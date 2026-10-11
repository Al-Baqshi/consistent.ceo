//! The `http` tool: makes outbound HTTP requests via `reqwest`, gated by a domain allow/deny
//! list and with the response body capped so a huge payload cannot blow up the model's context.
//! The host-matching rules mirror `browser.rs`'s so the two policies read the same way.

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::types::ToolResult;

const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024; // 1MB

/// HTTP tool: execute requests via reqwest. Respects domain allowlist/denylist and caps response at 1MB.
pub struct HttpTool {
    client: reqwest::Client,
    /// If set, only these domains (e.g. "api.example.com") are allowed. If empty/None, no allowlist.
    pub allowed_domains: Option<Vec<String>>,
    /// Domains that are always blocked (e.g. "localhost", "169.254.0.0").
    pub denied_domains: Vec<String>,
}

impl HttpTool {
    pub fn new(allowed_domains: Option<Vec<String>>, denied_domains: Vec<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            allowed_domains,
            denied_domains,
        }
    }

    // Exact host match or dot-suffix (subdomain) match, case-insensitive; deny list checked
    // before the allow list so a denial always wins.
    fn host_allowed(&self, host: &str) -> bool {
        let host_lower = host.to_lowercase();
        if self.denied_domains.iter().any(|d| {
            host_lower == d.to_lowercase()
                || host_lower.ends_with(&format!(".{}", d.to_lowercase()))
        }) {
            return false;
        }
        if let Some(ref allow) = self.allowed_domains {
            let ok = allow.iter().any(|a| {
                let a = a.to_lowercase();
                host_lower == a || host_lower.ends_with(&format!(".{}", a))
            });
            if !ok {
                return false;
            }
        }
        true
    }
}

#[async_trait]
impl Tool for HttpTool {
    fn name(&self) -> &'static str {
        "http"
    }

    fn description(&self) -> &'static str {
        "Make HTTP requests (GET, POST, etc.). Respects domain allow/deny list; response body capped at 1MB."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE", "PATCH"] },
                "url": { "type": "string" },
                "headers": { "type": "object" },
                "body": { "type": "string" }
            },
            "required": ["method", "url"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let method = arguments
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("http tool missing `method`"))?;
        let url_str = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("http tool missing `url`"))?;

        let url = match url_str.parse::<reqwest::Url>() {
            Ok(u) => u,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Invalid URL: {}", e),
                    is_error: true,
                });
            }
        };
        let host = match url.host_str() {
            Some(h) => h,
            None => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "URL has no host".to_string(),
                    is_error: true,
                });
            }
        };
        if !self.host_allowed(host) {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Domain not allowed: {}", host),
                is_error: true,
            });
        }

        let mut req = match method.to_uppercase().as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            _ => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Unsupported method: {}", method),
                    is_error: true,
                });
            }
        };

        if let Some(obj) = arguments.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str()
                    && let Ok(hv) = s.parse::<reqwest::header::HeaderValue>()
                {
                    req = req.header(k.as_str(), hv);
                }
            }
        }

        if matches!(method.to_uppercase().as_str(), "POST" | "PUT" | "PATCH")
            && let Some(body) = arguments.get("body").and_then(|v| v.as_str())
        {
            req = req.body(body.to_string());
        }

        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Request failed: {}", e),
                    is_error: true,
                });
            }
        };

        let status = res.status();
        let is_success = res.status().is_success();
        let headers: Vec<String> = res
            .headers()
            .iter()
            .map(|(k, v)| format!("{}: {}", k.as_str(), v.to_str().unwrap_or("(binary)")))
            .collect();
        let body = res.bytes().await;
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Failed to read body: {}", e),
                    is_error: true,
                });
            }
        };
        // Cap the body so a huge response can't be handed whole to the model; the model still
        // sees that it happened via the trailing marker below.
        let (_truncated, body_str) = if body.len() > MAX_RESPONSE_BODY_BYTES {
            (
                true,
                String::from_utf8_lossy(&body[..MAX_RESPONSE_BODY_BYTES]).into_owned()
                    + "\n...[truncated, max 1MB]",
            )
        } else {
            (false, String::from_utf8_lossy(&body).into_owned())
        };

        let mut output = format!(
            "{} {}\n",
            status.as_str(),
            status.canonical_reason().unwrap_or("")
        );
        output.push_str(&headers.join("\n"));
        if !headers.is_empty() {
            output.push('\n');
        }
        output.push('\n');
        output.push_str(&body_str);

        Ok(ToolResult {
            call_id: String::new(),
            output,
            is_error: !is_success,
        })
    }
}
