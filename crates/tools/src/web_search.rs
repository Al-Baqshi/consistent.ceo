//! The `web_search` tool: queries whichever search provider has an API key configured (Brave,
//! then Tavily) and returns a plain-text ranked result list for the model to read or follow up
//! on with the `http`/`browser` tools.

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::types::ToolResult;
use serde::Deserialize;

/// Web search tool. Selects a provider at construction time based on which API key is configured.
/// Returns a compact text list of `title — url\nsnippet` lines for the LLM to consume.
pub enum WebSearchProvider {
    Brave { api_key: String },
    Tavily { api_key: String },
}

/// Runs searches against the configured [`WebSearchProvider`].
pub struct WebSearchTool {
    client: reqwest::Client,
    provider: WebSearchProvider,
}

impl WebSearchTool {
    /// Construct from environment variables. Tries `BRAVE_SEARCH_API_KEY` first, then `TAVILY_API_KEY`.
    /// Returns `None` if neither is set.
    pub fn from_env() -> Option<Self> {
        if let Ok(k) = std::env::var("BRAVE_SEARCH_API_KEY")
            && !k.trim().is_empty()
        {
            return Some(Self {
                client: reqwest::Client::new(),
                provider: WebSearchProvider::Brave { api_key: k },
            });
        }
        if let Ok(k) = std::env::var("TAVILY_API_KEY")
            && !k.trim().is_empty()
        {
            return Some(Self {
                client: reqwest::Client::new(),
                provider: WebSearchProvider::Tavily { api_key: k },
            });
        }
        None
    }
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWebSection>,
}

#[derive(Deserialize)]
struct BraveWebSection {
    results: Vec<BraveResult>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
    #[serde(default)]
    answer: Option<String>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the public web for current information. Returns a ranked list of results (title, url, snippet). Use for research, fact-checking, or finding sources to read further with the http or browser tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("web_search: missing `query`"))?;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 10) as usize;

        match &self.provider {
            WebSearchProvider::Brave { api_key } => {
                let res = self
                    .client
                    .get("https://api.search.brave.com/res/v1/web/search")
                    .header("Accept", "application/json")
                    .header("X-Subscription-Token", api_key)
                    .query(&[("q", query), ("count", &limit.to_string())])
                    .send()
                    .await;
                let res = match res {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!("web_search Brave request failed: {e}"),
                            is_error: true,
                        });
                    }
                };
                if !res.status().is_success() {
                    let status = res.status();
                    let body = res.text().await.unwrap_or_default();
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("web_search Brave HTTP {status}: {body}"),
                        is_error: true,
                    });
                }
                let parsed: BraveResponse = match res.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!("web_search Brave parse failed: {e}"),
                            is_error: true,
                        });
                    }
                };
                let mut out = String::new();
                if let Some(web) = parsed.web {
                    for (i, r) in web.results.into_iter().take(limit).enumerate() {
                        out.push_str(&format!(
                            "{}. {} — {}\n   {}\n",
                            i + 1,
                            r.title,
                            r.url,
                            r.description
                        ));
                    }
                }
                if out.is_empty() {
                    out.push_str("(no results)");
                }
                Ok(ToolResult {
                    call_id: String::new(),
                    output: out,
                    is_error: false,
                })
            }
            WebSearchProvider::Tavily { api_key } => {
                let body = serde_json::json!({
                    "api_key": api_key,
                    "query": query,
                    "max_results": limit,
                    "include_answer": true
                });
                let res = self
                    .client
                    .post("https://api.tavily.com/search")
                    .json(&body)
                    .send()
                    .await;
                let res = match res {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!("web_search Tavily request failed: {e}"),
                            is_error: true,
                        });
                    }
                };
                if !res.status().is_success() {
                    let status = res.status();
                    let body = res.text().await.unwrap_or_default();
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("web_search Tavily HTTP {status}: {body}"),
                        is_error: true,
                    });
                }
                let parsed: TavilyResponse = match res.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!("web_search Tavily parse failed: {e}"),
                            is_error: true,
                        });
                    }
                };
                let mut out = String::new();
                if let Some(ans) = parsed.answer.filter(|s| !s.trim().is_empty()) {
                    out.push_str("Answer: ");
                    out.push_str(&ans);
                    out.push_str("\n\n");
                }
                for (i, r) in parsed.results.into_iter().take(limit).enumerate() {
                    out.push_str(&format!(
                        "{}. {} — {}\n   {}\n",
                        i + 1,
                        r.title,
                        r.url,
                        r.content
                    ));
                }
                if out.is_empty() {
                    out.push_str("(no results)");
                }
                Ok(ToolResult {
                    call_id: String::new(),
                    output: out,
                    is_error: false,
                })
            }
        }
    }
}
