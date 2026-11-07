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

/// `POST /api/entities/{id}/connections/github/token` body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConnectGithubTokenBody {
    /// A personal access token to verify and keep. Absent: use the GitHub CLI.
    #[serde(default)]
    token: Option<String>,
}

fn json_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// The token the signed-in GitHub CLI (`gh`) holds, if there is one.
async fn github_cli_token() -> Result<String, String> {
    let Some(gh) = consistent_world_tools::coding_agent::locate_program("gh") else {
        return Err(
            "The GitHub CLI (gh) isn't installed here. Paste a token instead, or install gh \
             from https://cli.github.com and run `gh auth login`."
                .to_string(),
        );
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        consistent_world_common::platform::command(&gh)
            .args(["auth", "token"])
            .output(),
    )
    .await
    .map_err(|_| "The GitHub CLI didn't answer. Try again, or paste a token.".to_string())?
    .map_err(|e| format!("Could not run the GitHub CLI: {e}"))?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        return Err(
            "The GitHub CLI isn't signed in. Run `gh auth login` in Terminal, or paste a token."
                .to_string(),
        );
    }
    Ok(token)
}

/// Ask GitHub who this token belongs to. `(login, numeric id, scopes)`.
async fn verify_github_token(token: &str) -> Result<(String, String, Vec<String>), String> {
    let client = reqwest::Client::builder()
        .user_agent("consistent-world-gateway")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get("https://api.github.com/user")
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| "Could not reach GitHub. Check the internet connection.".to_string())?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err("GitHub rejected that token. Check it hasn't expired and try again.".to_string());
    }
    if !status.is_success() {
        return Err(format!("GitHub answered {status}. Try again in a moment."));
    }
    let scopes: Vec<String> = response
        .headers()
        .get("x-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "GitHub's answer wasn't readable.".to_string())?;
    let login = body["login"]
        .as_str()
        .ok_or_else(|| "GitHub didn't say whose token this is.".to_string())?
        .to_string();
    let id = body["id"]
        .as_u64()
        .map(|n| n.to_string())
        .unwrap_or_default();
    Ok((login, id, scopes))
}

/// Connect GitHub to an entity without the OAuth app: verify a pasted token or
/// the GitHub CLI's, keep it in the keychain, and record the account. Private
/// repository imports read the token from the same place the browser sign-in
/// would have put it, so nothing downstream cares which path was used.
pub(super) async fn connect_entity_github_token(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<ConnectGithubTokenBody>,
) -> Result<Json<EntityConnection>, (StatusCode, Json<serde_json::Value>)> {
    let s = control(&state).map_err(|c| json_error(c, "The app isn't ready yet."))?;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(json_error(StatusCode::NOT_FOUND, "No such entity."));
        }
    }
    let (token, source) = match body.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => (t.to_string(), "token"),
        None => (
            github_cli_token()
                .await
                .map_err(|m| json_error(StatusCode::BAD_REQUEST, m))?,
            "gh_cli",
        ),
    };
    let (login, account_id, scopes) = verify_github_token(&token)
        .await
        .map_err(|m| json_error(StatusCode::BAD_REQUEST, m))?;

    // One GitHub connection per entity: a second connect replaces the first's
    // secret rather than stacking duplicates.
    let existing_id = s
        .entity_connections
        .read()
        .await
        .iter()
        .find(|c| c.entity_id == entity_id && c.provider.eq_ignore_ascii_case("github"))
        .map(|c| c.id.clone());
    let connection_id = existing_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let secret_ref =
        store_connection_secret_and_ref(&state, &connection_id, None, Some(token))
            .map_err(|_| {
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "The token was fine, but saving it to the keychain failed.",
                )
            })?;
    let now = Utc::now().to_rfc3339();
    let config = serde_json::json!({
        "authType": if source == "gh_cli" { "github_cli" } else { "github_token" },
        "accepts": ["repo", "readme"],
        "tokenType": "personal"
    });
    let connection = {
        let mut connections = s.entity_connections.write().await;
        if let Some(c) = connections.iter_mut().find(|c| c.id == connection_id) {
            c.status = "connected".to_string();
            c.secret_ref = secret_ref;
            c.external_account_id = Some(account_id).filter(|v| !v.is_empty());
            c.external_handle = Some(login.clone());
            c.scopes = scopes;
            c.config = config;
            c.last_error = None;
            c.last_checked_at = Some(now.clone());
            c.updated_at = now.clone();
            c.clone()
        } else {
            let connection = EntityConnection {
                id: connection_id.clone(),
                entity_id: entity_id.clone(),
                kind: "api_service".to_string(),
                provider: "github".to_string(),
                label: format!("GitHub · @{login}"),
                status: "connected".to_string(),
                capability_state: serde_json::json!({
                    "available": ["repo", "readme"],
                    "granted": ["repo", "readme"],
                    "verification": { "status": "verified" }
                }),
                config,
                secret_ref,
                external_account_id: Some(account_id).filter(|v| !v.is_empty()),
                external_handle: Some(login.clone()),
                scopes,
                last_checked_at: Some(now.clone()),
                last_error: None,
                created_by: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            connections.push(connection.clone());
            connection
        }
    };
    s.append_entity_connection_event(EntityConnectionEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: now,
        entity_id,
        connection_id,
        kind: if existing_id.is_some() {
            EntityConnectionEventKind::SecretRotated
        } else {
            EntityConnectionEventKind::ConnectionCreated
        },
        detail: Some(format!("github @{login} via {source}")),
    })
    .await;
    Ok(Json(connection))
}
