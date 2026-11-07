//! The Consistent browser: open it, close it, see what is open, and say which
//! sites the agent may use in it for an entity.
//!
//! `GET /api/browser`, `POST /api/browser/open`, `POST /api/browser/close`,
//! `PUT /api/entities/{id}/browser-sites`. The window itself is
//! [`crate::browser_manager`]; the rules for what the agent may do in it are in
//! [`crate::mcp_browser`]. A child module of `control_api` (`use super::*`).
//!
//! Opening a browser window only makes sense on the person's own computer, so
//! open and close answer 409 on a gateway that is not one. Everything here is
//! the operator's: the open tabs are private, and widening a site list widens
//! what an unattended run can reach.

use super::*;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn require_local(state: &ServerState) -> Result<(), ApiError> {
    if state.local_files.is_none() {
        return Err(api_error(
            StatusCode::CONFLICT,
            "The Consistent browser opens on your own computer, and this gateway is not running on one.",
        ));
    }
    Ok(())
}

pub(super) async fn get_browser(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<crate::browser_manager::BrowserStatus>, ApiError> {
    require_admin_json(&auth, &state)?;
    Ok(Json(crate::browser_manager::status().await))
}

pub(super) async fn open_browser(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<crate::browser_manager::BrowserStatus>, ApiError> {
    require_admin_json(&auth, &state)?;
    require_local(&state)?;
    crate::browser_manager::open()
        .await
        .map(Json)
        .map_err(|m| api_error(StatusCode::BAD_GATEWAY, m))
}

pub(super) async fn close_browser(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<crate::browser_manager::BrowserStatus>, ApiError> {
    require_admin_json(&auth, &state)?;
    require_local(&state)?;
    crate::browser_manager::close()
        .await
        .map(Json)
        .map_err(|m| api_error(StatusCode::BAD_GATEWAY, m))
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
    let mut entities = s.entities.write().await;
    let entity = entities
        .iter_mut()
        .find(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
        })
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "No such entity."))?;
    entity.browser_sites = sites.clone();
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
