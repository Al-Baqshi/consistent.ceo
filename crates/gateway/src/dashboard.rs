//! Serve the optional SvelteKit dashboard as static files, or a minimal built-in status page.

use axum::{
    Router,
    body::Body,
    http::{StatusCode, header},
    response::Response,
    routing::get,
};

/// Loader page for native shell: shows "Loading..." then redirects to /. Avoids white screen in WebView.
const LOADER_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Consistent</title>
  <style>
    body { font-family: system-ui, sans-serif; background: #0c0a09; color: #e7e5e4; display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
    .box { text-align: center; padding: 2rem; }
    h1 { color: #f59e0b; }
    .spinner { width: 32px; height: 32px; border: 3px solid #444; border-top-color: #f59e0b; border-radius: 50%; animation: spin 0.8s linear infinite; margin: 1rem auto; }
    @keyframes spin { to { transform: rotate(360deg); } }
  </style>
</head>
<body>
  <div class="box">
    <div class="spinner" aria-hidden="true"></div>
    <h1>Consistent</h1>
    <p>Loading…</p>
  </div>
  <script>setTimeout(function(){ window.location.href='/'; }, 600);</script>
</body>
</html>
"##;

/// Minimal HTML status page when dashboard is not compiled in.
const FALLBACK_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Consistent</title>
  <style>
    body { font-family: system-ui, sans-serif; background: #0c0a09; color: #e7e5e4; display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; }
    .box { text-align: center; padding: 2rem; }
    h1 { color: #f59e0b; }
    a { color: #f59e0b; }
  </style>
</head>
<body>
  <div class="box">
    <h1>Consistent</h1>
    <p>Gateway is running. Dashboard not embedded.</p>
    <p>No API key configured? Run <code>consistent-world --onboard</code> to set up credentials.</p>
    <p><a href="/health">/health</a> · <a href="/api/metrics">/api/metrics</a> (use Bearer token)</p>
  </div>
</body>
</html>
"##;

#[cfg(feature = "dashboard")]
// Populated by build.rs; path relative to workspace root (cwd when cargo build runs)
static DASHBOARD_DIR: include_dir::Dir<'_> =
    include_dir::include_dir!("crates/gateway/dashboard_build");

/// Returns a Router that serves the dashboard (or fallback) under the given prefix.
/// Mount at "/" to serve dashboard at root. Uses same auth as API when protected is true.
async fn serve_loader() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(LOADER_HTML))
        .unwrap()
}

/// Router that serves the dashboard UI: the loader/fallback pages when the
/// `dashboard` feature is off, or the built SvelteKit assets when it's on.
pub fn dashboard_routes() -> Router {
    #[cfg(feature = "dashboard")]
    {
        Router::new()
            .route("/loader", get(serve_loader))
            .route("/", get(serve_dashboard_index))
            .route("/{*path}", get(serve_dashboard_file))
    }

    #[cfg(not(feature = "dashboard"))]
    {
        Router::new()
            .route("/loader", get(serve_loader))
            .route("/", get(serve_fallback))
            .route("/{*path}", get(serve_fallback))
    }
}

// Serves the minimal built-in status page when the dashboard isn't compiled in.
#[cfg(not(feature = "dashboard"))]
async fn serve_fallback() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(FALLBACK_HTML))
        .unwrap()
}

// Serves the SPA entry point for the dashboard root route.
#[cfg(feature = "dashboard")]
async fn serve_dashboard_index() -> Response {
    serve_file("index.html").await
}

// Serves one embedded dashboard asset by its request path.
#[cfg(feature = "dashboard")]
async fn serve_dashboard_file(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return serve_file("index.html").await;
    }
    serve_file(path).await
}

// Resolves a request path to an embedded dashboard file, falling back to the
// SPA's index.html for client-side routes (paths with no file extension).
#[cfg(feature = "dashboard")]
fn serve_file(path: &str) -> impl std::future::Future<Output = Response> + Send {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let path_owned = path.to_string();
    let file = DASHBOARD_DIR
        .get_file(path)
        .or_else(|| DASHBOARD_DIR.get_file(format!("{path}/index.html")))
        .or_else(|| {
            if path.contains('.') {
                None
            } else {
                DASHBOARD_DIR.get_file("index.html")
            }
        });
    async move {
        match file {
            Some(f) => {
                let content_type = mime_guess::from_path(&path_owned)
                    .first_raw()
                    .unwrap_or("application/octet-stream");
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(f.contents()))
                    .unwrap()
            }
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Not found"))
                .unwrap(),
        }
    }
}
