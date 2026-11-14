//! Each entity's own browser: open it, close it, see what is open in it, and
//! say which sites the agent may use in it.
//!
//! `GET /api/browser` (every entity's browser at a glance),
//! `GET /api/entities/{id}/browser`, `POST /api/entities/{id}/browser/open`,
//! `POST /api/entities/{id}/browser/close`, `PUT /api/entities/{id}/browser-sites`.
//! The windows themselves are [`crate::browser_manager`]; the rules for what the
//! agent may do in them are in [`crate::mcp_browser`]. A child module of
//! `control_api` (`use super::*`).
//!
//! Opening a browser window only makes sense on the person's own computer, so
//! open and close answer 409 on a gateway that is not one. Everything here is
//! the operator's: the open tabs are private, and widening a site list widens
//! what an unattended run can reach.

use super::*;
use crate::browser_manager::BrowserStatus;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn require_local(state: &ServerState) -> Result<(), ApiError> {
    if state.local_files.is_none() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "An entity's browser opens on your own computer, and this gateway is not running on one.",
        ));
    }
    Ok(())
}

/// One entity's browser, as the dashboard shows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EntityBrowser {
    entity_id: String,
    entity_name: String,
    /// The sites the agent may use in it.
    sites: Vec<String>,
    #[serde(flatten)]
    status: BrowserStatus,
}

impl EntityBrowser {
    fn new(entity: &Entity, status: BrowserStatus) -> Self {
        Self {
            entity_id: entity.id.clone(),
            entity_name: entity.name.clone(),
            sites: entity.browser_sites.clone(),
            status,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowsersOverview {
    chrome_found: bool,
    chrome_name: Option<String>,
    browsers: Vec<EntityBrowser>,
}

async fn visible_entities(s: &ControlState, workspace: &WorkspaceCtx) -> Vec<Entity> {
    s.entities
        .read()
        .await
        .iter()
        .filter(|e| is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str())))
        .cloned()
        .collect()
}

async fn visible_entity(
    state: &ServerState,
    workspace: &WorkspaceCtx,
    entity_id: &str,
) -> Result<Entity, ApiError> {
    let s = control(state).map_err(|c| api_error(c, "The app isn't ready yet."))?;
    visible_entities(s, workspace)
        .await
        .into_iter()
        .find(|e| e.id == entity_id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "No such entity."))
}

/// `GET /api/browser`: every entity's browser at a glance.
pub(super) async fn get_browsers(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<BrowsersOverview>, ApiError> {
    require_admin_json(&auth, &state)?;
    let s = control(&state).map_err(|c| api_error(c, "The app isn't ready yet."))?;
    let entities = visible_entities(s, &workspace).await;
    let statuses = futures::future::join_all(
        entities
            .iter()
            .map(|e| crate::browser_manager::status(&e.id)),
    )
    .await;
    let browsers = entities
        .iter()
        .zip(statuses)
        .map(|(e, status)| EntityBrowser::new(e, status))
        .collect();
    let (chrome_found, chrome_name) = crate::browser_manager::chrome_available();
    Ok(Json(BrowsersOverview {
        chrome_found,
        chrome_name,
        browsers,
    }))
}

/// `GET /api/entities/{id}/browser`.
pub(super) async fn get_entity_browser(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<EntityBrowser>, ApiError> {
    require_admin_json(&auth, &state)?;
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let status = crate::browser_manager::status(&entity.id).await;
    Ok(Json(EntityBrowser::new(&entity, status)))
}

/// `POST /api/entities/{id}/browser/open`: open the entity's window, or bring it forward.
pub(super) async fn open_entity_browser(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<EntityBrowser>, ApiError> {
    require_admin_json(&auth, &state)?;
    require_local(&state)?;
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let status = crate::browser_manager::open(&entity.id, &entity.name)
        .await
        .map_err(|m| api_error(StatusCode::BAD_GATEWAY, m))?;
    Ok(Json(EntityBrowser::new(&entity, status)))
}

/// `POST /api/entities/{id}/browser/close`: close the entity's window politely.
pub(super) async fn close_entity_browser(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<EntityBrowser>, ApiError> {
    require_admin_json(&auth, &state)?;
    require_local(&state)?;
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let status = crate::browser_manager::close(&entity.id)
        .await
        .map_err(|m| api_error(StatusCode::BAD_GATEWAY, m))?;
    Ok(Json(EntityBrowser::new(&entity, status)))
}

#[derive(Debug, Deserialize)]
pub(super) struct BrowserSitesBody {
    sites: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct BrowserSitesResponse {
    sites: Vec<String>,
}

/// At most this many sites per entity: a list longer than that is an allow-all
/// by another name.
const MAX_SITES: usize = 50;

/// What a person typed, as the bare domain the browser policy matches on:
/// `https://www.GitHub.com/acme?x=1` becomes `github.com`. A site must be a real
/// domain and must not be one the agent is never allowed to use.
pub(crate) fn normalize_site(raw: &str) -> Result<String, String> {
    let mut host = raw.trim().to_lowercase();
    if host.is_empty() {
        return Err("A site can't be empty.".to_string());
    }
    for scheme in ["https://", "http://"] {
        if let Some(rest) = host.strip_prefix(scheme) {
            host = rest.to_string();
        }
    }
    host = host
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    // A port or credentials in the address are not part of the site.
    if let Some((_, after)) = host.rsplit_once('@') {
        host = after.to_string();
    }
    if let Some((before, port)) = host.rsplit_once(':')
        && port.chars().all(|c| c.is_ascii_digit())
    {
        host = before.to_string();
    }
    let host = host
        .trim_matches('.')
        .strip_prefix("www.")
        .unwrap_or(host.trim_matches('.'))
        .to_string();
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let labels: Vec<&str> = host.split('.').collect();
    if host.len() > 253 || labels.len() < 2 || !labels.iter().all(|l| label_ok(l)) {
        return Err(format!("“{}” doesn't look like a website address.", raw.trim()));
    }
    if labels.iter().all(|l| l.chars().all(|c| c.is_ascii_digit())) {
        return Err("Use a website name, not an IP address.".to_string());
    }
    if crate::mcp_browser::always_denied(&host) {
        return Err(format!(
            "{host} is never available to the agent: banks, payments and sign-in pages are always off limits."
        ));
    }
    Ok(host)
}

/// `PUT /api/entities/{id}/browser-sites`: replace the list of sites the agent
/// may use for this entity. The whole list is validated before any of it is kept.
pub(super) async fn set_entity_browser_sites(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<BrowserSitesBody>,
) -> Result<Json<BrowserSitesResponse>, ApiError> {
    require_admin_json(&auth, &state)?;
    let s = control(&state).map_err(|c| api_error(c, "The app isn't ready yet."))?;
    if body.sites.len() > MAX_SITES {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("Up to {MAX_SITES} sites per entity."),
        ));
    }
    let mut sites: Vec<String> = Vec::new();
    for raw in &body.sites {
        let site = normalize_site(raw).map_err(|m| api_error(StatusCode::BAD_REQUEST, m))?;
        if !sites.contains(&site) {
            sites.push(site);
        }
    }
    {
        let mut entities = s.entities.write().await;
        let entity = entities
            .iter_mut()
            .find(|e| {
                e.id == entity_id
                    && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
            })
            .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "No such entity."))?;
        entity.browser_sites = sites.clone();
    }
    // Written through now rather than on the next minute's sync: the desktop
    // app ends the gateway without a shutdown save, and a site list that
    // quietly reverts is a permission that changed without anyone deciding.
    if let Err(e) = s.sync_entities_to_store().await {
        warn!(error = %e, "could not save the entity's browser sites");
    }
    Ok(Json(BrowserSitesResponse { sites }))
}

#[cfg(test)]
mod tests {
    use super::normalize_site;

    #[test]
    fn what_people_type_becomes_a_bare_domain() {
        for (typed, site) in [
            ("github.com", "github.com"),
            ("  GitHub.com ", "github.com"),
            ("https://www.github.com/acme/widget?tab=readme#top", "github.com"),
            ("http://gist.github.com:8080/x", "gist.github.com"),
            ("me@example.org", "example.org"),
            ("x.com/", "x.com"),
            ("shop.example.co.nz.", "shop.example.co.nz"),
        ] {
            assert_eq!(normalize_site(typed).as_deref(), Ok(site), "{typed}");
        }
    }

    #[test]
    fn what_is_not_a_site_is_refused_with_a_reason() {
        for typed in ["", "  ", "localhost", "not a site", "-bad.com", "a..b.com", "http://"] {
            assert!(normalize_site(typed).is_err(), "{typed:?}");
        }
        assert!(normalize_site("192.168.1.10").unwrap_err().contains("IP address"));
    }

    #[test]
    fn money_and_sign_in_sites_are_never_available() {
        for typed in ["paypal.com", "https://checkout.stripe.com/pay", "accounts.google.com", "www.wise.com"] {
            let err = normalize_site(typed).unwrap_err();
            assert!(err.contains("never available"), "{typed}: {err}");
        }
        // A neighbour with a similar name is a different site.
        assert!(normalize_site("notpaypal.com").is_ok());
        assert!(normalize_site("google.com").is_ok());
    }
}
