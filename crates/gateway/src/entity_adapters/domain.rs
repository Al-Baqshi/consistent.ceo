//! Domain-ownership verification adapter: proves control of a domain over
//! HTTP rather than through an authenticated API call.

use serde_json::Value;

// Strip a URL down to a bare, lowercased host: drop the scheme, then keep
// only what's before the first `/`.
fn normalize_host(raw: &str) -> String {
    let t = raw.trim();
    let t = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    t.split('/').next().unwrap_or(t).trim().to_lowercase()
}

/// Verify `token` appears in `/.well-known/consistent-world-verification.txt` over HTTPS (then HTTP fallback).
pub async fn verify_domain_ownership(hostname_or_url: &str, token: &str) -> anyhow::Result<()> {
    let host = normalize_host(hostname_or_url);
    if host.is_empty() {
        anyhow::bail!("empty hostname");
    }
    let path = "/.well-known/consistent-world-verification.txt";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .danger_accept_invalid_certs(false)
        .build()?;
    for base in [format!("https://{host}"), format!("http://{host}")] {
        let url = format!("{base}{path}");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                if body.trim().contains(token.trim()) {
                    return Ok(());
                }
            }
            _ => continue,
        }
    }
    anyhow::bail!(
        "Token not found at https://{host}{path} (and http fallback). Place the token in that file.",
        host = host,
        path = path
    )
}

/// Read the connection's configured domain, accepting either key name.
pub fn domain_from_config(config: &Value) -> Option<String> {
    config
        .get("domain")
        .or_else(|| config.get("hostname"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
