//! GitHub device-authorization flow for entity connections.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Persists a connection secret to the OS keyring and returns the ref to
/// store on the connection row, passing an explicit ref through unchanged
/// when no new secret value was given.
pub(super) fn store_connection_secret_and_ref(
    _state: &ServerState,
    connection_id: &str,
    explicit_secret_ref: Option<String>,
    secret_value: Option<String>,
) -> Result<Option<String>, StatusCode> {
    let provided_ref = explicit_secret_ref
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let Some(secret_value) = secret_value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(provided_ref);
    };
    let key = format!("entity.connection.{connection_id}");
    let store = consistent_world_credentials::keyring::KeyringStore::new("consistent-world");
    if store.set(&key, secret_value).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Some(provided_ref.unwrap_or_else(|| {
        format!("keyring://consistent-world/{key}")
    })))
}

/// Reads the GitHub OAuth app's client id from the environment.
pub(super) fn github_oauth_client_id() -> Option<String> {
    std::env::var("CONSISTENT_WORLD_GITHUB_CLIENT_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Stashes a device-flow code in the keyring under the connection session id.
pub(super) fn store_connection_session_secret(session_id: &str, value: &str) -> Result<String, StatusCode> {
    let key = format!("entity.connection.session.{session_id}");
    let store = consistent_world_credentials::keyring::KeyringStore::new("consistent-world");
    store
        .set(&key, value)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(format!("keyring://consistent-world/{key}"))
}

/// Reads back the device-flow code stored by [`store_connection_session_secret`].
pub(super) fn read_connection_session_secret(session_id: &str) -> Option<String> {
    let key = format!("entity.connection.session.{session_id}");
    consistent_world_credentials::keyring::KeyringStore::new("consistent-world")
        .get(&key)
        .ok()
}

/// Starts GitHub's OAuth device flow for a connection session: requests a
/// device/user code pair and stashes the device code for later polling.
/// Returns `Ok(None)` when no GitHub OAuth app is configured.
pub(super) async fn start_github_device_authorization(
    session_id: &str,
    requested_capabilities: &[String],
) -> Result<Option<(GitHubDeviceCodeResponse, String)>, StatusCode> {
    let Some(client_id) = github_oauth_client_id() else {
        return Ok(None);
    };
    let scope = if requested_capabilities.is_empty() {
        "repo read:user".to_string()
    } else {
        let requested = requested_capabilities
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if requested.is_empty() {
            "repo read:user".to_string()
        } else {
            requested.join(" ")
        }
    };
    let client = reqwest::Client::builder()
        .user_agent("consistent-world-gateway")
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let response = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", client_id.as_str()), ("scope", scope.as_str())])
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !response.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let body = response
        .json::<GitHubDeviceCodeResponse>()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let ref_url = store_connection_session_secret(session_id, &body.device_code)?;
    Ok(Some((body, ref_url)))
}

/// Polls GitHub for the device-flow token and, on success, fetches the
/// authenticated user to label the connection. Returns
/// `(access_token, external_account_id, external_handle, scopes)`.
pub(super) async fn complete_github_device_authorization(
    session_id: &str,
) -> Result<(String, Option<String>, Option<String>, Vec<String>), StatusCode> {
    let client_id = github_oauth_client_id().ok_or(StatusCode::FAILED_DEPENDENCY)?;
    let device_code = read_connection_session_secret(session_id).ok_or(StatusCode::BAD_REQUEST)?;
    let client = reqwest::Client::builder()
        .user_agent("consistent-world-gateway")
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let response = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id.as_str()),
            ("device_code", device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !response.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let token = response
        .json::<GitHubDeviceTokenResponse>()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if let Some(error) = token.error.as_deref() {
        let _provider_detail = token.error_description.as_deref();
        return match error {
            "authorization_pending" | "slow_down" => Err(StatusCode::CONFLICT),
            "expired_token" => Err(StatusCode::GONE),
            "access_denied" => Err(StatusCode::FORBIDDEN),
            _ => Err(StatusCode::BAD_REQUEST),
        };
    }
    let access_token = token.access_token.ok_or(StatusCode::BAD_GATEWAY)?;
    if token
        .token_type
        .as_deref()
        .map(|t| !t.eq_ignore_ascii_case("bearer"))
        .unwrap_or(false)
    {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let scopes = token
        .scope
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let user = client
        .get("https://api.github.com/user")
        .bearer_auth(&access_token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !user.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let user = user
        .json::<GitHubUserResponse>()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok((
        access_token,
        Some(user.id.to_string()),
        Some(user.login),
        scopes,
    ))
}
