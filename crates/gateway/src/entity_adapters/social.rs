//! Social-account connection check: confirms a stored bearer token is live.
//! No posting adapter exists yet — see [`crate::entity_actions::action_has_adapter`].

use serde_json::Value;

/// Minimal “social” adapter: validate Twitter/X bearer token via API v2.
pub async fn test_social_bearer(secret: Option<&str>, config: &Value) -> anyhow::Result<String> {
    let provider = config
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("twitter");
    let Some(token) = secret.map(str::trim).filter(|s| !s.is_empty()) else {
        anyhow::bail!("missing bearer token in connection secret");
    };
    if !provider.eq_ignore_ascii_case("twitter") && !provider.eq_ignore_ascii_case("x") {
        anyhow::bail!("only twitter/x bearer test is implemented; provider={provider}");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let resp = client
        .get("https://api.twitter.com/2/users/me")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await?;
    if resp.status().is_success() {
        Ok("twitter_bearer_ok".to_string())
    } else {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("twitter api {status}: {txt}")
    }
}
