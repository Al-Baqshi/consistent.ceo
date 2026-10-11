//! SendGrid-backed email adapter: the one channel [`crate::entity_actions`]
//! can actually dispatch today (`send_email` on an `email_account` connection).

use serde_json::Value;

/// Validate a SendGrid API key without sending mail.
pub async fn test_sendgrid_key(secret: Option<&str>) -> anyhow::Result<String> {
    let Some(api_key) = secret.map(str::trim).filter(|s| !s.is_empty()) else {
        anyhow::bail!("missing SendGrid API key in connection secret");
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let resp = client
        .get("https://api.sendgrid.com/v3/scopes")
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await?;
    if resp.status().is_success() {
        Ok("sendgrid_key_ok".to_string())
    } else {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("sendgrid {status}: {txt}")
    }
}

/// Send email via SendGrid HTTP API when `secret` is a SendGrid API key and config has `from` + `to`.
pub async fn send_email_via_adapter(
    secret: Option<&str>,
    config: &Value,
    subject: &str,
    body_text: &str,
) -> anyhow::Result<String> {
    let Some(api_key) = secret.map(str::trim).filter(|s| !s.is_empty()) else {
        anyhow::bail!("missing email API secret (store SendGrid key in connection secret)");
    };
    let from = config
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("config.from required for email send"))?;
    let to = config
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("config.to required for email send"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .post("https://api.sendgrid.com/v3/mail/send")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "personalizations": [{ "to": [{ "email": to }] }],
            "from": { "email": from },
            "subject": subject,
            "content": [{ "type": "text/plain", "value": body_text }]
        }))
        .send()
        .await?;
    if resp.status().is_success() {
        Ok("email_accepted_by_provider".to_string())
    } else {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("sendgrid error {status}: {txt}")
    }
}
