//! Integration tests for `HttpTool` against real network endpoints (example.com, httpbin.org):
//! a plain GET, and domain allow/deny list enforcement.

use consistent_world_tools::Tool;
use consistent_world_tools::http::HttpTool;

#[tokio::test]
async fn test_http_get() {
    let tool = HttpTool::new(None, vec![]);
    let res = tool
        .execute(serde_json::json!({
            "method": "GET",
            "url": "https://example.com"
        }))
        .await
        .unwrap();
    assert!(!res.is_error, "GET failed: {}", res.output);
    assert!(
        res.output.contains("200"),
        "expected status 200: {}",
        res.output
    );
    assert!(
        res.output.to_lowercase().contains("example"),
        "expected example.com body or headers"
    );
}

#[tokio::test]
async fn test_http_domain_denylist() {
    let tool = HttpTool::new(None, vec!["example.com".to_string()]);
    let res = tool
        .execute(serde_json::json!({
            "method": "GET",
            "url": "https://example.com"
        }))
        .await
        .unwrap();
    assert!(res.is_error, "request should be rejected by denylist");
    assert!(
        res.output.contains("not allowed") || res.output.contains("Domain"),
        "expected denylist message: {}",
        res.output
    );
}

#[tokio::test]
async fn test_http_allowlist() {
    // Only example.com allowed; request to other host should be rejected
    let tool = HttpTool::new(Some(vec!["example.com".to_string()]), vec![]);
    let res = tool
        .execute(serde_json::json!({
            "method": "GET",
            "url": "https://httpbin.org/get"
        }))
        .await
        .unwrap();
    assert!(res.is_error);
    assert!(res.output.contains("not allowed") || res.output.contains("Domain"));
}
