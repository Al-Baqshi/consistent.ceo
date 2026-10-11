//! Off-box database backups: SurrealDB `/export` → S3-compatible bucket.
//!
//! A container volume protects data against container restarts; this module
//! protects it against volume loss. A daily task (plus `POST /api/admin/backup`
//! for on-demand runs) pulls a full SurrealQL export over the database's HTTP
//! endpoint and PUTs it into the bucket configured via `BACKUP_S3_*` env vars,
//! pruning to the newest [`RETAIN`] objects.
//!
//! S3 auth is a minimal hand-rolled AWS Signature V4 (PUT/GET/DELETE with
//! simple keys only) — three requests didn't justify an SDK dependency.
//! Virtual-host style addressing (`https://{bucket}.{endpoint_host}/{key}`).

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::alerts::AlertConfig;
use crate::config::AppConfig;

/// How many of the newest backup objects to keep; older ones are pruned
/// after each successful run.
const RETAIN: usize = 14;
/// Object-key prefix every backup is stored under, so listing/pruning stays
/// scoped to this app's own objects in a bucket that may hold other things.
const PREFIX: &str = "surreal/";

type HmacSha256 = Hmac<Sha256>;

/// S3-compatible bucket + credentials a backup run uploads to. Built once
/// from `BACKUP_S3_*` env vars via [`BackupConfig::from_env`].
#[derive(Clone)]
pub struct BackupConfig {
    endpoint_host: String,
    scheme: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
}

impl BackupConfig {
    /// Read `BACKUP_S3_*` from the environment. Returns `None` (backups
    /// disabled) unless every required var is present and non-empty.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let endpoint = get("BACKUP_S3_ENDPOINT")?;
        let (scheme, host) = endpoint
            .split_once("://")
            .map(|(s, h)| (s.to_string(), h.trim_end_matches('/').to_string()))
            .unwrap_or_else(|| {
                (
                    "https".to_string(),
                    endpoint.trim_end_matches('/').to_string(),
                )
            });
        Some(Self {
            endpoint_host: host,
            scheme,
            bucket: get("BACKUP_S3_BUCKET")?,
            region: get("BACKUP_S3_REGION").unwrap_or_else(|| "auto".to_string()),
            access_key: get("BACKUP_S3_ACCESS_KEY_ID")?,
            secret_key: get("BACKUP_S3_SECRET_ACCESS_KEY")?,
        })
    }

    // Virtual-hosted-style addressing: bucket name goes in front of the
    // endpoint host, per the module header.
    fn host(&self) -> String {
        format!("{}.{}", self.bucket, self.endpoint_host)
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Sign and send one S3 request. `canonical_query` must already be
/// canonically encoded (sorted, RFC 3986) — we only use simple queries.
async fn s3_request(
    cfg: &BackupConfig,
    method: &str,
    key_path: &str,
    canonical_query: &str,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>)> {
    let host = cfg.host();
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(&body);

    let canonical_uri = format!("/{}", key_path.trim_start_matches('/'));
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, amz_date
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{}/{}/s3/aws4_request", date, cfg.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        scope,
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(
        format!("AWS4{}", cfg.secret_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, cfg.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        cfg.access_key, scope, signed_headers, signature
    );

    let url = if canonical_query.is_empty() {
        format!("{}://{}{}", cfg.scheme, host, canonical_uri)
    } else {
        format!(
            "{}://{}{}?{}",
            cfg.scheme, host, canonical_uri, canonical_query
        )
    };
    let client = reqwest::Client::new();
    let req = match method {
        "PUT" => client.put(&url).body(body),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    let resp = req
        .header("host", &host)
        .header("x-amz-content-sha256", &payload_hash)
        .header("x-amz-date", &amz_date)
        .header("authorization", &authorization)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .with_context(|| format!("s3 {method} {url}"))?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap_or_default().to_vec();
    Ok((status, bytes))
}

/// Fetch a full SurrealQL export over the database's HTTP surface.
async fn surreal_export(cfg: &AppConfig) -> Result<Vec<u8>> {
    let url = cfg.surreal_url.trim();
    let http_base = if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{}", rest)
    } else if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{}", rest)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        anyhow::bail!("backups need a remote SurrealDB (ws/http); surreal_url is {url:?}");
    };
    let export_url = format!("{}/export", http_base.trim_end_matches('/'));
    let creds = crate::startup::surreal_root_credentials(cfg);
    let mut req = reqwest::Client::new().get(&export_url);
    if let Some((user, pass)) = creds {
        req = req.basic_auth(user, Some(pass));
    }
    let resp = req
        .header("Surreal-NS", &cfg.surreal_ns)
        .header("Surreal-DB", &cfg.surreal_db)
        .header("Accept", "application/octet-stream")
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .with_context(|| format!("surreal export from {export_url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("surreal export HTTP {}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// List backup object keys under [`PREFIX`], oldest first (keys embed a
/// sortable UTC timestamp, so lexicographic order is chronological).
async fn list_backup_keys(s3: &BackupConfig) -> Result<Vec<String>> {
    // %2F = '/' in the prefix; the canonical query and the real query must match.
    let query = format!("list-type=2&prefix={}", PREFIX.replace('/', "%2F"));
    let (status, body) = s3_request(s3, "GET", "/", &query, Vec::new()).await?;
    if status != 200 {
        anyhow::bail!(
            "s3 list HTTP {}: {}",
            status,
            String::from_utf8_lossy(&body[..body.len().min(300)])
        );
    }
    let xml = String::from_utf8_lossy(&body);
    let mut keys: Vec<String> = xml
        .split("<Key>")
        .skip(1)
        .filter_map(|s| s.split("</Key>").next())
        .map(|s| s.to_string())
        .collect();
    keys.sort();
    Ok(keys)
}

/// Run one full backup: export → PUT → prune to [`RETAIN`] newest.
pub async fn run_backup(cfg: &AppConfig, s3: &BackupConfig) -> Result<(String, usize)> {
    let data = surreal_export(cfg).await?;
    let size = data.len();
    let key = format!(
        "{}main-{}.surql",
        PREFIX,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    let (status, body) = s3_request(s3, "PUT", &key, "", data).await?;
    if status != 200 {
        anyhow::bail!(
            "s3 put HTTP {}: {}",
            status,
            String::from_utf8_lossy(&body[..body.len().min(300)])
        );
    }
    match list_backup_keys(s3).await {
        Ok(keys) if keys.len() > RETAIN => {
            for old in &keys[..keys.len() - RETAIN] {
                if let Err(e) = s3_request(s3, "DELETE", old, "", Vec::new()).await {
                    tracing::warn!(key = %old, error = %e, "backup prune failed");
                }
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "backup listing for prune failed"),
    }
    tracing::info!(key = %key, bytes = size, "database backup uploaded");
    Ok((key, size))
}

/// Spawn the daily backup loop. First run shortly after boot (so a broken
/// pipeline is noticed immediately), then every 24h.
pub fn spawn_daily(cfg: Arc<AppConfig>, s3: BackupConfig, alerts: Option<AlertConfig>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        loop {
            match run_backup(&cfg, &s3).await {
                Ok((key, bytes)) => {
                    tracing::info!(key = %key, bytes, "daily backup complete");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "daily backup failed");
                    crate::alerts::notify_failure(
                        alerts.as_ref(),
                        "database backup",
                        &e.to_string(),
                    )
                    .await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_signing_key_matches_aws_test_vector() {
        // AWS documented example: secret, date 20150830, us-east-1, service iam.
        let k_date = hmac_sha256(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20150830");
        let k_region = hmac_sha256(&k_date, b"us-east-1");
        let k_service = hmac_sha256(&k_region, b"iam");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        assert_eq!(
            hex::encode(k_signing),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn backup_config_requires_all_vars() {
        // Runs in-process env mutation; keys are test-unique to avoid clashes.
        // SAFETY: test-only env keys, no concurrent readers of these names.
        unsafe {
            std::env::remove_var("BACKUP_S3_ENDPOINT");
        }
        assert!(BackupConfig::from_env().is_none());
    }
}
