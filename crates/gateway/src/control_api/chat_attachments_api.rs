//! Attachments uploaded alongside a chat turn.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Shape of the multipart form fields for a chat attachment upload
/// (currently unused directly — fields are read off the multipart stream by
/// name in [`upload_chat_attachment`]).
#[derive(Debug, Deserialize)]
pub(super) struct ChatAttachmentUploadForm {
    pub(super) session_id: Option<String>,
}

/// The per-workspace directory segment under the chat uploads root.
///
/// `ChatAttachment` (in `state.rs`) has no owner field, so the owner is
/// recorded in the one place the server alone chooses: the stored path,
/// `uploads/chat/<owner key>/<attachment id>/<name>`. The key is a SHA-256 of
/// the workspace id, so any workspace id — including ones with separators —
/// becomes one fixed-shape, unguessable path component.
pub(super) fn attachment_owner_key(workspace_id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(workspace_id.as_bytes()))
}

/// Whether a directory name is an owner key (64 lowercase hex chars) rather
/// than a legacy session-id directory.
fn is_owner_key(segment: &str) -> bool {
    segment.len() == 64
        && segment
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Who may read an attachment stored at `relative` (its path under the chat
/// uploads root). Files in an owner-key directory belong to that workspace.
/// Files written before ownership was recorded (`<session>/<file>`) carry no
/// owner; they stay readable by the default workspace — the single-tenant era
/// that wrote them — and by the instance admin.
pub(super) fn attachment_readable_by(
    relative: &std::path::Path,
    workspace_id: &str,
    admin: bool,
) -> bool {
    let first = relative
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .unwrap_or_default();
    if is_owner_key(first) {
        first == attachment_owner_key(workspace_id)
    } else {
        admin || workspace_id == DEFAULT_WORKSPACE_ID
    }
}

/// Handler: saves a multipart-uploaded chat attachment to disk under its own
/// directory, owned by the caller's workspace, and records its metadata.
///
/// Files used to land at `uploads/chat/<session_id>/<filename>`. The web
/// client sends `session_id = "default"` and every pasted screenshot is
/// `image.png`, so one user's paste overwrote another's — and the GET handed
/// any attachment to anyone. Each upload now gets a fresh directory keyed by
/// its own id, under its workspace's owner key, and the GET checks the key.
pub(super) async fn upload_chat_attachment(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    mut multipart: Multipart,
) -> Result<Json<ChatAttachment>, StatusCode> {
    let s = control(&state)?;

    let mut session_id = String::from("default");
    let mut file_bytes: Option<(String, String, Vec<u8>)> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "session_id" {
            let value = field.text().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                session_id = trimmed;
            }
        } else if name == "file" {
            let filename = field
                .file_name()
                .ok_or(StatusCode::BAD_REQUEST)?
                .to_string();
            if filename.is_empty() {
                continue;
            }
            let content_type = field
                .content_type()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let bytes = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            file_bytes = Some((filename, content_type, bytes.to_vec()));
        }
    }

    let (filename, content_type, bytes) = file_bytes.ok_or(StatusCode::BAD_REQUEST)?;
    // The filename comes straight from the client and is joined onto the
    // uploads root, so it must be exactly one plain path component. Unchecked,
    // a `../../.ssh/authorized_keys` filename wrote wherever the gateway's user
    // could — and on Windows `..\` and `C:\` do the same. The session id is
    // metadata only now; it no longer names a directory.
    let filename = single_path_component(&filename).ok_or(StatusCode::BAD_REQUEST)?;
    if session_id.chars().count() > 256 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let size_bytes = bytes.len() as u64;

    let id = Uuid::new_v4().to_string();
    let uploads_dir = std::path::PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world/uploads/chat",
    ));
    let attachment_dir = uploads_dir
        .join(attachment_owner_key(&workspace.id))
        .join(&id);
    tokio::fs::create_dir_all(&attachment_dir)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let file_path = attachment_dir.join(&filename);
    tokio::fs::write(&file_path, &bytes)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let now = chrono::Utc::now().to_rfc3339();
    let attachment = ChatAttachment {
        id: id.clone(),
        filename: filename.clone(),
        content_type,
        size_bytes,
        session_id: session_id.clone(),
        uploaded_at: now.clone(),
        path: file_path.to_string_lossy().to_string(),
    };

    s.chat_attachments.write().await.push(attachment.clone());

    Ok(Json(attachment))
}

/// Reduce a client-supplied name to one safe path component: the last segment
/// after either separator, with characters Windows forbids in a file name
/// replaced, and trailing dots and spaces (which Windows silently strips)
/// removed. `None` when nothing usable is left, or it is `.` / `..`.
pub(super) fn single_path_component(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then_some(cleaned)
}

/// Serve an uploaded chat attachment's bytes (`GET /api/chat/attachments/{id}`
/// and `/thumb`, which the chat UI uses for inline image previews — the
/// original is small enough that a separate thumbnail isn't worth a resize
/// pipeline). The stored path is the one the upload handler wrote, under the
/// uploads root; anything that resolves elsewhere is refused.
pub(super) async fn get_chat_attachment(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, StatusCode> {
    let s = control(&state)?;
    let attachment = {
        let attachments = s.chat_attachments.read().await;
        attachments
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let uploads_root = std::path::PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world/uploads/chat",
    ))
    .canonicalize()
    .map_err(|_| StatusCode::NOT_FOUND)?;
    let resolved = std::path::PathBuf::from(&attachment.path)
        .canonicalize()
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let Ok(relative) = resolved.strip_prefix(&uploads_root) else {
        return Err(StatusCode::FORBIDDEN);
    };
    let admin = crate::server::is_instance_admin(
        &auth,
        crate::server::gateway_is_self_hosted(&state),
    );
    // Another workspace's file is a 404, same as a missing one.
    if !attachment_readable_by(relative, &workspace.id, admin) {
        return Err(StatusCode::NOT_FOUND);
    }
    let bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let content_type = if attachment.content_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        attachment.content_type.clone()
    };
    use axum::response::IntoResponse;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=3600".to_string(),
            ),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::{attachment_owner_key, attachment_readable_by, single_path_component};
    use std::path::Path;

    #[test]
    fn an_attachment_is_readable_only_by_its_workspace() {
        let alice = attachment_owner_key("ws-alice");
        let stored = Path::new(&alice).join("att-1").join("image.png");
        assert!(attachment_readable_by(&stored, "ws-alice", false));
        assert!(!attachment_readable_by(&stored, "ws-bob", false));
        // Ownership is the workspace's, admin or not.
        assert!(!attachment_readable_by(&stored, "default", true));
        // Same pasted `image.png`, different workspaces: different directories.
        assert_ne!(alice, attachment_owner_key("ws-bob"));
    }

    #[test]
    fn legacy_session_directory_attachments_stay_with_the_operator() {
        let legacy = Path::new("default").join("image.png");
        assert!(attachment_readable_by(&legacy, "default", false));
        assert!(attachment_readable_by(&legacy, "ws-x", true));
        assert!(!attachment_readable_by(&legacy, "ws-x", false));
    }

    #[test]
    fn an_upload_name_cannot_climb_out_of_the_uploads_root() {
        assert_eq!(
            single_path_component("../../.ssh/authorized_keys").as_deref(),
            Some("authorized_keys")
        );
        assert_eq!(
            single_path_component("..\\..\\Windows\\win.ini").as_deref(),
            Some("win.ini")
        );
        assert_eq!(single_path_component("C:\\x.txt").as_deref(), Some("x.txt"));
        assert_eq!(single_path_component("..").as_deref(), None);
        assert_eq!(single_path_component("a/..").as_deref(), None);
        assert_eq!(single_path_component("").as_deref(), None);
    }

    #[test]
    fn ordinary_names_survive() {
        assert_eq!(
            single_path_component("Quarterly report (v2).pdf").as_deref(),
            Some("Quarterly report (v2).pdf")
        );
        assert_eq!(single_path_component("default").as_deref(), Some("default"));
        assert_eq!(
            single_path_component("what?.png").as_deref(),
            Some("what_.png")
        );
        assert_eq!(
            single_path_component("trailing. ").as_deref(),
            Some("trailing")
        );
    }
}
