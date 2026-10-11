//! The entity ledger: records attached to an entity, and the rollups over them.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists an entity's ledger records, most recently fetched
/// first, optionally narrowed to one `kind`.
pub(super) async fn list_entity_records(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Query(q): Query<EntityRecordsListQuery>,
) -> Result<Json<Vec<EntityRecord>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let store = state
        .entity_records_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let out = match q.kind.as_deref() {
        Some(kind) => store.list_for_entity_kind(&entity_id, kind).await,
        None => store.list_for_entity(&entity_id).await,
    }
    .map_err(|e| {
        tracing::warn!(error = %e, "list_entity_records failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(out))
}

/// `POST` handler: bulk-upserts ledger records for an entity. This is the one
/// door every source writes through — an API sync, a browser extraction, or a
/// push from an external system (the Baqshi CRM's content ideas and
/// customers, for instance). Rows are keyed on `(source, kind, sourceRef)` via
/// [`crate::entity_records_store::record_key`], so re-posting the same source
/// row overwrites it instead of piling up a duplicate — a sync job can run
/// nightly without ever needing to know what it wrote last time.
pub(super) async fn upsert_entity_records(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<BulkUpsertEntityRecordsRequest>,
) -> Result<Json<BulkUpsertEntityRecordsResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    if body.records.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let store = state
        .entity_records_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let now = Utc::now().to_rfc3339();
    let mut out = Vec::with_capacity(body.records.len());
    for item in body.records {
        let kind = item.kind.trim();
        let source = item.source.trim();
        let source_ref = item.source_ref.trim();
        if kind.is_empty() || source.is_empty() || source_ref.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        let id = crate::entity_records_store::record_key(&entity_id, source, kind, source_ref);
        // A resync overwrites `updatedAt`/`fetchedAt`/`fields` but keeps the
        // row's original `createdAt` — it is the same fact confirmed again,
        // not a new one, and losing that timestamp on every sync would make
        // "how long have we known this" unanswerable.
        let created_at = match store.get(&id).await {
            Ok(Some(existing)) => existing.created_at,
            Ok(None) => now.clone(),
            Err(e) => {
                tracing::warn!(error = %e, record_id = %id, "upsert_entity_records: get failed, treating as new");
                now.clone()
            }
        };
        let record = EntityRecord {
            id,
            entity_id: entity_id.clone(),
            kind: kind.to_string(),
            source: source.to_string(),
            source_ref: source_ref.to_string(),
            fields: item.fields,
            evidence: item.evidence,
            fetched_at: item.fetched_at.unwrap_or_else(|| now.clone()),
            created_at,
            updated_at: now.clone(),
            created_by: item.created_by,
        };
        store.upsert_record(&record).await.map_err(|e| {
            tracing::warn!(error = %e, "upsert_entity_records: upsert failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        out.push(record);
    }

    Ok(Json(BulkUpsertEntityRecordsResponse {
        upserted: out.len(),
        records: out,
    }))
}

/// Handler: turns an inbound OpenClaw order into a task on the connection's
/// entity, requiring an `api_service`/`openclaw` connection to exist first.
pub(super) async fn ingest_openclaw_order(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, connection_id)): Path<(String, String)>,
    Json(body): Json<OpenClawOrderIngestRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }

    let connection = {
        let connections = s.entity_connections.read().await;
        connections
            .iter()
            .find(|c| c.id == connection_id && c.entity_id == entity_id)
            .cloned()
    }
    .ok_or(StatusCode::NOT_FOUND)?;

    if connection.kind != "api_service" || !connection.provider.eq_ignore_ascii_case("openclaw") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let external_id = body
        .order_id
        .or(body.id)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let title = body
        .title
        .clone()
        .or_else(|| body.summary.clone())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("OpenClaw order {external_id}"));
    let content = body
        .description
        .clone()
        .or_else(|| body.summary.clone())
        .or_else(|| {
            body.payload
                .as_ref()
                .and_then(|v| serde_json::to_string_pretty(v).ok())
        });
    let metadata = Some(serde_json::json!({
        "source": "openclaw",
        "orderId": external_id,
        "connectionId": connection_id,
        "payload": body.payload.unwrap_or_else(|| serde_json::json!({}))
    }));

    let task = internal_create_task(
        s,
        &workspace_id,
        Some(workspace.principal_id.as_str()),
        CreateTaskRequest {
            title,
            dept: Some("OpenClaw".to_string()),
            roles: None,
            entity_id: Some(entity_id.clone()),
            mission_id: body.mission_id,
            priority: None,
            requires_human_approval: body.requires_human_approval,
            task_kind: Some(
                body.task_kind
                    .unwrap_or_else(|| "openclaw-order".to_string()),
            ),
            content,
            director_id: body.director_id.or_else(|| Some("operations".to_string())),
            agent_id: None,
            connection_id: Some(connection_id.clone()),
            external_source: Some("openclaw".to_string()),
            external_id: Some(external_id.clone()),
            metadata,
            due_at: None,
        },
    )
    .await?;

    s.append_entity_connection_event(EntityConnectionEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        entity_id,
        connection_id,
        kind: EntityConnectionEventKind::ActionSucceeded,
        detail: Some(format!(
            "openclaw_order_ingested external_id={external_id} task_id={}",
            task.id
        )),
    })
    .await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "task": task,
        "externalSource": "openclaw",
        "externalId": external_id
    })))
}

/// `PATCH` handler: partial-updates a connection, logging a `SecretRotated`
/// event when the secret ref/value changed.
pub(super) async fn update_entity_connection(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, connection_id)): Path<(String, String)>,
    Json(body): Json<UpdateEntityConnectionRequest>,
) -> Result<Json<EntityConnection>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let mut connections = s.entity_connections.write().await;
    let c = connections
        .iter_mut()
        .find(|c| c.id == connection_id && c.entity_id == entity_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(label) = body.label {
        c.label = label;
    }
    if let Some(status) = body.status {
        c.status = status;
    }
    if let Some(capability_state) = body.capability_state {
        c.capability_state = capability_state;
    }
    if let Some(config) = body.config {
        c.config = config;
    }
    if let Some(external_account_id) = body.external_account_id {
        c.external_account_id = Some(external_account_id);
    }
    if let Some(external_handle) = body.external_handle {
        c.external_handle = Some(external_handle);
    }
    if let Some(scopes) = body.scopes {
        c.scopes = scopes;
    }
    let secret_rotated = body.secret_ref.is_some() || body.secret_value.is_some();
    if secret_rotated {
        c.secret_ref =
            store_connection_secret_and_ref(&state, &c.id, body.secret_ref, body.secret_value)?;
    }
    c.updated_at = chrono::Utc::now().to_rfc3339();
    let out = c.clone();
    drop(connections);
    if secret_rotated {
        s.append_entity_connection_event(EntityConnectionEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            entity_id: entity_id.clone(),
            connection_id: connection_id.clone(),
            kind: EntityConnectionEventKind::SecretRotated,
            detail: Some("credentials updated".to_string()),
        })
        .await;
    }
    Ok(Json(out))
}

/// Handler: probes a connection's live credentials with a provider-specific
/// check (social bearer token, SendGrid key, domain verification file, or a
/// generic connected-state check), recording the result as an event and feed item.
pub(super) async fn test_entity_connection(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, connection_id)): Path<(String, String)>,
) -> Result<Json<EntityConnection>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let snapshot = {
        let connections = s.entity_connections.read().await;
        connections
            .iter()
            .find(|c| c.id == connection_id && c.entity_id == entity_id)
            .cloned()
    };
    let Some(c) = snapshot else {
        return Err(StatusCode::NOT_FOUND);
    };
    let secret = credentials_resolver::read_entity_connection_secret(&c.id);
    let adapter_result: Result<String, String> = match c.kind.as_str() {
        "social_account"
            if c.provider.eq_ignore_ascii_case("twitter")
                || c.provider.eq_ignore_ascii_case("x") =>
        {
            crate::entity_adapters::test_social_bearer(secret.as_deref(), &c.config)
                .await
                .map_err(|e| e.to_string())
        }
        "email_account" => crate::entity_adapters::test_sendgrid_key(secret.as_deref())
            .await
            .map_err(|e| e.to_string()),
        "domain" => {
            if let Some(host) = crate::entity_adapters::domain_from_config(&c.config) {
                let tok = c
                    .config
                    .get("verification_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if tok.is_empty() {
                    Err(
                        "Set config.verification_token to the value you will host publicly"
                            .to_string(),
                    )
                } else {
                    match crate::entity_adapters::verify_domain_ownership(&host, tok).await {
                        Ok(()) => Ok("domain_verification_file_ok".to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                }
            } else {
                Err("config.domain required".to_string())
            }
        }
        _ if c.status == "connected" || c.status == "limited" => {
            Ok("connected_state_ok".to_string())
        }
        _ => Err("Connection is not connected; complete OAuth or verification first".to_string()),
    };

    let now = chrono::Utc::now().to_rfc3339();
    let mut connections = s.entity_connections.write().await;
    let c = connections
        .iter_mut()
        .find(|c| c.id == connection_id && c.entity_id == entity_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    c.last_checked_at = Some(now.clone());
    match adapter_result {
        Ok(detail) => {
            c.last_error = None;
            if c.status == "reauth_required" {
                c.status = "connected".to_string();
            }
            let detail_cp = detail.clone();
            s.append_entity_connection_event(EntityConnectionEvent {
                id: Uuid::new_v4().to_string(),
                timestamp: now.clone(),
                entity_id: entity_id.clone(),
                connection_id: connection_id.clone(),
                kind: EntityConnectionEventKind::ConnectionValidated,
                detail: Some(detail),
            })
            .await;
            s.push_operator_feed_surface(
                OperatorFeedKind::ConnectionVerified,
                format!("Connection OK · {}", c.label),
                Some(detail_cp),
                Some(entity_id.clone()),
                Some(connection_id.clone()),
            )
            .await;
        }
        Err(e) => {
            let msg = crate::entity_adapters::user_facing_adapter_error(&e);
            c.last_error = Some(msg.clone());
            let pdec = crate::planner::decision_from_connection_error_message(&msg);
            if matches!(
                pdec,
                crate::planner::PlannerDecision::ReconnectRequired { .. }
            ) {
                c.status = "reauth_required".to_string();
            }
            s.append_entity_connection_event(EntityConnectionEvent {
                id: Uuid::new_v4().to_string(),
                timestamp: now,
                entity_id: entity_id.clone(),
                connection_id: connection_id.clone(),
                kind: EntityConnectionEventKind::ConnectionTestFailed,
                detail: c.last_error.clone(),
            })
            .await;
            s.push_operator_feed_surface(
                OperatorFeedKind::ConnectionCheckFailed,
                format!("Connection check failed · {}", c.label),
                Some(msg),
                Some(entity_id.clone()),
                Some(connection_id.clone()),
            )
            .await;
        }
    }
    Ok(Json(c.clone()))
}

/// Handler: verifies a connection's ownership proof. Only `domain`
/// connections are currently supported (a hosted-file DNS/HTTP check);
/// any other kind is marked `verification_required`.
pub(super) async fn verify_entity_connection(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, connection_id)): Path<(String, String)>,
    Json(body): Json<VerifyEntityConnectionRequest>,
) -> Result<Json<EntityConnection>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let mut connections = s.entity_connections.write().await;
    let c = connections
        .iter_mut()
        .find(|c| c.id == connection_id && c.entity_id == entity_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let method = body
        .verification_method
        .unwrap_or_else(|| "manual_dns".to_string());
    let token = body.token.unwrap_or_default();
    let now = chrono::Utc::now().to_rfc3339();
    if c.kind == "domain" {
        let host =
            crate::entity_adapters::domain_from_config(&c.config).ok_or(StatusCode::BAD_REQUEST)?;
        if token.trim().is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if let Err(e) = crate::entity_adapters::verify_domain_ownership(&host, &token).await {
            let msg = crate::entity_adapters::user_facing_adapter_error(&e.to_string());
            c.last_error = Some(msg.clone());
            c.updated_at = now.clone();
            s.append_entity_connection_event(EntityConnectionEvent {
                id: Uuid::new_v4().to_string(),
                timestamp: now.clone(),
                entity_id: entity_id.clone(),
                connection_id: connection_id.clone(),
                kind: EntityConnectionEventKind::ConnectionTestFailed,
                detail: c.last_error.clone(),
            })
            .await;
            s.push_operator_feed_surface(
                OperatorFeedKind::ConnectionCheckFailed,
                format!("Domain verification failed · {}", c.label),
                Some(msg),
                Some(entity_id.clone()),
                Some(connection_id.clone()),
            )
            .await;
            return Ok(Json(c.clone()));
        }
        let cfg = c.config.as_object_mut();
        if let Some(cfg) = cfg {
            cfg.insert(
                "verification".to_string(),
                serde_json::json!({
                    "verification_method": method,
                    "verification_token": token,
                    "verified_at": now,
                    "status": "verified"
                }),
            );
        }
        c.status = "connected".to_string();
        c.last_error = None;
        c.last_checked_at = Some(now.clone());
        c.updated_at = now.clone();
        s.append_entity_connection_event(EntityConnectionEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: now.clone(),
            entity_id: entity_id.clone(),
            connection_id: connection_id.clone(),
            kind: EntityConnectionEventKind::VerificationSucceeded,
            detail: Some("verification=domain_http".to_string()),
        })
        .await;
        s.push_operator_feed_surface(
            OperatorFeedKind::ConnectionVerified,
            format!("Domain verified · {}", c.label),
            Some("verification=domain_http".to_string()),
            Some(entity_id.clone()),
            Some(connection_id.clone()),
        )
        .await;
        return Ok(Json(c.clone()));
    }
    c.last_error =
        Some("Verification is currently supported for domain connections only".to_string());
    c.status = "verification_required".to_string();
    c.updated_at = now;
    Ok(Json(c.clone()))
}

/// Handler: deletes a connection and logs a disconnect event.
pub(super) async fn delete_entity_connection(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, connection_id)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let mut connections = s.entity_connections.write().await;
    let before = connections.len();
    connections.retain(|c| !(c.id == connection_id && c.entity_id == entity_id));
    if connections.len() == before {
        return Err(StatusCode::NOT_FOUND);
    }
    drop(connections);
    s.append_entity_connection_event(EntityConnectionEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        entity_id,
        connection_id,
        kind: EntityConnectionEventKind::ConnectionDisconnected,
        detail: Some("source=delete_endpoint".to_string()),
    })
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET` handler: the most recent 200 connection events for an entity, newest first.
pub(super) async fn list_entity_connection_events(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<Vec<EntityConnectionEvent>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let mut out = s
        .entity_connection_events
        .read()
        .await
        .iter()
        .filter(|e| e.entity_id == entity_id)
        .cloned()
        .collect::<Vec<_>>();
    out.reverse();
    out.truncate(200);
    Ok(Json(out))
}

/// Handler: starts an OAuth-style connection session. For GitHub this kicks
/// off the device-code flow and returns the user code to show; other
/// providers get a bare `pending` session for the client to drive.
pub(super) async fn create_entity_connection_session(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<CreateEntityConnectionSessionRequest>,
) -> Result<Json<EntityConnectionSession>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    if body.provider.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let now = Utc::now();
    let session_id = Uuid::new_v4().to_string();
    let provider = body.provider.trim().to_lowercase();
    let requested_capabilities = body.requested_capabilities.unwrap_or_default();
    let mut status = "pending".to_string();
    let mut expires_at = (now + chrono::Duration::minutes(15)).to_rfc3339();
    let mut pkce_verifier_ref = None;
    let mut authorization_url = None;
    let mut verification_uri = None;
    let mut user_code = None;
    let mut poll_interval_seconds = None;
    let mut provider_message = None;
    if provider == "github" {
        match start_github_device_authorization(&session_id, &requested_capabilities).await? {
            Some((auth, secret_ref)) => {
                status = "pending_auth".to_string();
                expires_at = (now + chrono::Duration::seconds(auth.expires_in as i64)).to_rfc3339();
                pkce_verifier_ref = Some(secret_ref);
                authorization_url = Some(format!(
                    "{}?user_code={}",
                    auth.verification_uri, auth.user_code
                ));
                verification_uri = Some(auth.verification_uri);
                user_code = Some(auth.user_code);
                poll_interval_seconds = auth.interval;
                provider_message = Some(
                    "Authorize Consistent in GitHub, then return here to complete the connection."
                        .to_string(),
                );
            }
            None => {
                status = "config_required".to_string();
                provider_message = Some(
                    "Set CONSISTENT_WORLD_GITHUB_CLIENT_ID to enable GitHub's permission flow."
                        .to_string(),
                );
            }
        }
    }
    let session = EntityConnectionSession {
        id: session_id,
        entity_id: entity_id.clone(),
        provider,
        requested_capabilities,
        state: Uuid::new_v4().to_string(),
        pkce_verifier_ref,
        authorization_url,
        verification_uri,
        user_code,
        poll_interval_seconds,
        provider_message,
        status,
        expires_at,
        created_by: body.created_by,
        created_at: now.to_rfc3339(),
    };
    s.entity_connection_sessions
        .write()
        .await
        .push(session.clone());
    s.append_entity_connection_event(EntityConnectionEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        entity_id,
        connection_id: session.id.clone(),
        kind: EntityConnectionEventKind::OAuthStarted,
        detail: Some(format!("provider={}", session.provider)),
    })
    .await;
    Ok(Json(session))
}

/// `GET` handler: fetches a connection session (for the client to poll).
pub(super) async fn get_entity_connection_session(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, session_id)): Path<(String, String)>,
) -> Result<Json<EntityConnectionSession>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let sessions = s.entity_connection_sessions.read().await;
    sessions
        .iter()
        .find(|x| x.id == session_id && x.entity_id == entity_id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Handler: completes a connection session into a real [`EntityConnection`].
/// Rejects an expired or not-yet-configured session; for GitHub, exchanges
/// the device code for a token when the caller didn't supply one directly.
pub(super) async fn finalize_entity_connection_session(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, session_id)): Path<(String, String)>,
    Json(body): Json<FinalizeEntityConnectionSessionRequest>,
) -> Result<Json<EntityConnection>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let status = body.status.unwrap_or_else(|| "completed".to_string());
    let session = {
        let mut sessions = s.entity_connection_sessions.write().await;
        let session = sessions
            .iter_mut()
            .find(|x| x.id == session_id && x.entity_id == entity_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if session.status == "config_required" {
            return Err(StatusCode::FAILED_DEPENDENCY);
        }
        if DateTime::parse_from_rfc3339(&session.expires_at)
            .map(|expires| expires.with_timezone(&Utc) < Utc::now())
            .unwrap_or(false)
        {
            session.status = "expired".to_string();
            return Err(StatusCode::GONE);
        }
        if status != "completed" {
            session.status = status;
            return Err(StatusCode::BAD_REQUEST);
        }
        session.clone()
    };
    let connection_id = Uuid::new_v4().to_string();
    let mut secret_value = body.secret_value;
    let mut external_account_id = body.external_account_id;
    let mut external_handle = body.external_handle;
    let mut scopes = body.scopes.unwrap_or_default();
    let mut config = serde_json::json!({});
    if session.provider == "github" && secret_value.is_none() {
        let (access_token, account_id, handle, granted_scopes) =
            complete_github_device_authorization(&session.id).await?;
        secret_value = Some(access_token);
        external_account_id = external_account_id.or(account_id);
        external_handle = external_handle.or(handle);
        if scopes.is_empty() {
            scopes = granted_scopes;
        }
        config = serde_json::json!({
            "authType": "github_device",
            "accepts": ["repo", "readme"],
            "tokenType": "oauth"
        });
    }
    let secret_ref =
        store_connection_secret_and_ref(&state, &connection_id, body.secret_ref, secret_value)?;
    let now = Utc::now().to_rfc3339();
    let connection = EntityConnection {
        id: connection_id.clone(),
        entity_id: entity_id.clone(),
        kind: "api_service".to_string(),
        provider: session.provider.clone(),
        label: format!("{} connection", session.provider),
        status: "connected".to_string(),
        capability_state: serde_json::json!({
            "available": session.requested_capabilities,
            "granted": session.requested_capabilities,
            "verification": { "status": "verified" }
        }),
        config,
        secret_ref,
        external_account_id,
        external_handle,
        scopes,
        last_checked_at: Some(now.clone()),
        last_error: None,
        created_by: session.created_by.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    s.entity_connections.write().await.push(connection.clone());
    {
        let mut sessions = s.entity_connection_sessions.write().await;
        if let Some(session) = sessions
            .iter_mut()
            .find(|x| x.id == session_id && x.entity_id == entity_id)
        {
            session.status = "completed".to_string();
        }
    }
    s.append_entity_connection_event(EntityConnectionEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: now,
        entity_id,
        connection_id,
        kind: EntityConnectionEventKind::OAuthCompleted,
        detail: Some(format!("session_id={session_id}")),
    })
    .await;
    Ok(Json(connection))
}

/// Handler: runs an ad hoc action on an entity through a connection channel
/// (see `crate::entity_actions::perform_entity_action`), respecting the
/// global policy's approval gating.
///
/// Tenant-scoped like every other entity route: the action runs through the
/// entity's *stored* connections (an email account, an API key), so an entity
/// the caller cannot see is reported as missing — otherwise any principal could
/// send mail through another tenant's credentials. A linked `taskId` must
/// belong to the same entity, or approving the resulting approval would ungate
/// someone else's task.
pub(super) async fn perform_entity_action_endpoint(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<PerformEntityActionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = control(&state)?;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    if let Some(tid) = body.task_id.as_deref() {
        let tasks = s.tasks.read().await;
        if !tasks
            .iter()
            .any(|t| t.id == tid && t.entity_id.as_deref() == Some(entity_id.as_str()))
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let global_policy = state
        .app_config
        .as_ref()
        .map(|c| c.global_policy.clone())
        .unwrap_or_default();
    let result = crate::entity_actions::perform_entity_action(
        &state
            .control_state
            .clone()
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?,
        &global_policy,
        crate::entity_actions::PerformEntityActionInput {
            entity_id,
            action: body.action,
            channel: body.channel,
            payload: body.payload.unwrap_or_else(|| serde_json::json!({})),
            task_id: body.task_id,
            agent_id: body.agent_id,
        },
    )
    .await;
    Ok(Json(serde_json::json!({
        "ok": result.ok,
        "status": result.status,
        "message": result.message,
        "connectionId": result.connection_id,
        "requiresApproval": result.requires_approval
    })))
}
