//! File uploads attached to an entity.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Metadata for a file uploaded to an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityFile {
    pub id: String,
    pub entity_id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub uploaded_at: String,
    pub path: String,
}

/// Handler: saves a multipart-uploaded file to disk under the entity's
/// upload directory and records its metadata. `Multipart` is axum's
/// extractor for `multipart/form-data` bodies.
pub(super) async fn upload_entity_file(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<EntityFile>, StatusCode> {
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

    let uploads_dir = std::path::PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world/uploads",
    ));

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let raw_filename = field
            .file_name()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();
        if raw_filename.is_empty() {
            continue;
        }

        let content_type = field
            .content_type()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let bytes = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
        let size_bytes = bytes.len() as u64;

        let id = Uuid::new_v4().to_string();
        let (entity_upload_dir, file_path, filename) =
            entity_upload_target(&uploads_dir, &entity_id, &id, &raw_filename)
                .ok_or(StatusCode::BAD_REQUEST)?;
        tokio::fs::create_dir_all(&entity_upload_dir)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        tokio::fs::write(&file_path, bytes)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let now = chrono::Utc::now().to_rfc3339();
        let file_meta = EntityFile {
            id: id.clone(),
            entity_id: entity_id.clone(),
            filename: filename.clone(),
            content_type,
            size_bytes,
            uploaded_at: now.clone(),
            path: file_path.to_string_lossy().to_string(),
        };

        let stored = UploadedEntityFile {
            id: file_meta.id.clone(),
            entity_id: file_meta.entity_id.clone(),
            filename: file_meta.filename.clone(),
            content_type: file_meta.content_type.clone(),
            size_bytes: file_meta.size_bytes,
            uploaded_at: file_meta.uploaded_at.clone(),
            path: file_meta.path.clone(),
        };
        s.entity_files.write().await.push(stored);

        return Ok(Json(file_meta));
    }

    Err(StatusCode::BAD_REQUEST)
}

/// Where an upload lands: `(directory, file path, display name)`.
///
/// The client-supplied filename is reduced to one plain path component, so
/// `../../.ssh/authorized_keys` (or `/etc/cron.d/x`, which `Path::join` would
/// have taken as a whole new absolute path) can no longer leave the entity's
/// folder, and the file is stored as `<upload id>-<name>` so two uploads of
/// `image.png` never overwrite each other. The entity id is a path component
/// too, so it gets the same treatment. The returned display name is the
/// cleaned basename, without the id prefix.
pub(super) fn entity_upload_target(
    uploads_dir: &std::path::Path,
    entity_id: &str,
    upload_id: &str,
    raw_filename: &str,
) -> Option<(std::path::PathBuf, std::path::PathBuf, String)> {
    let name = single_path_component(raw_filename)?;
    let entity_dir = single_path_component(entity_id).filter(|d| d == entity_id)?;
    let dir = uploads_dir.join(entity_dir);
    let path = dir.join(format!("{upload_id}-{name}"));
    Some((dir, path, name))
}

/// `GET` handler: lists files uploaded to an entity.
pub(super) async fn list_entity_files(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<Vec<EntityFile>>, StatusCode> {
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

    let files = s.entity_files.read().await;
    let out: Vec<EntityFile> = files
        .iter()
        .filter(|f| f.entity_id == entity_id)
        .map(|f| EntityFile {
            id: f.id.clone(),
            entity_id: f.entity_id.clone(),
            filename: f.filename.clone(),
            content_type: f.content_type.clone(),
            size_bytes: f.size_bytes,
            uploaded_at: f.uploaded_at.clone(),
            path: f.path.clone(),
        })
        .collect();

    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::entity_upload_target;
    use std::path::Path;

    #[test]
    fn an_upload_filename_cannot_escape_the_entity_folder() {
        let root = Path::new("/srv/uploads");
        for evil in [
            "../../.ssh/authorized_keys",
            "/etc/cron.d/owned",
            "..\\..\\Windows\\win.ini",
            "C:\\x\\authorized_keys",
        ] {
            let (dir, path, name) = entity_upload_target(root, "ent-1", "u1", evil).unwrap();
            assert_eq!(dir, root.join("ent-1"));
            assert_eq!(path.parent(), Some(dir.as_path()), "{evil:?} escaped: {path:?}");
            assert!(!name.contains('/') && !name.contains('\\'));
            assert!(path.file_name().unwrap().to_str().unwrap().starts_with("u1-"));
        }
        assert!(entity_upload_target(root, "ent-1", "u1", "..").is_none());
        assert!(entity_upload_target(root, "ent-1", "u1", "a/..").is_none());
    }

    #[test]
    fn uploads_keep_a_clean_display_name_and_never_collide() {
        let root = Path::new("/srv/uploads");
        let (_, a, name) = entity_upload_target(root, "ent-1", "u1", "image.png").unwrap();
        let (_, b, _) = entity_upload_target(root, "ent-1", "u2", "image.png").unwrap();
        assert_eq!(name, "image.png");
        assert_ne!(a, b);
    }

    #[test]
    fn a_hostile_entity_id_is_refused() {
        let root = Path::new("/srv/uploads");
        assert!(entity_upload_target(root, "../x", "u1", "a.txt").is_none());
        assert!(entity_upload_target(root, "..", "u1", "a.txt").is_none());
    }
}
