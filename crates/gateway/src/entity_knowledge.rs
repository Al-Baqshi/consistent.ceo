//! Knowledge index: turn an entity's source into reference docs the brain can
//! actually use.
//!
//! Importing an entity used to end at a handful of narrative fields. The
//! README, the repository layout, the conventions files, the docs folder —
//! everything the operator pointed at — was read once for the draft and then
//! forgotten. This module keeps it: [`index_entity`] reads the source (a local
//! repo/folder, or a fetched URL) into `entity_docs` rows, which
//! [`crate::prompt_assembly`] injects into every chat turn and heartbeat for
//! that entity, and embeds them when an embedding provider is wired so
//! semantic recall has something to find.
//!
//! Deterministic on purpose: no model call is needed to index, so it works on
//! the local muscle alone and never gates on a cloud lane. The index is
//! rebuilt from scratch on every run — rows this module wrote before are
//! replaced, rows the operator added by hand are left alone.
//!
//! State lives on [`Entity::knowledge_index`] (`indexing` → `ready` /
//! `failed`, or `skipped` when there is nothing to index) so the UI can show
//! progress and the operator can re-index from the entity page.

use std::path::{Path, PathBuf};

use axum::{
    Json,
    extract::{Extension, Path as AxumPath, State},
    http::StatusCode,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::control_api::{
    WorkspaceCtx, existing_local_path, fetch_entity_source_with_token, github_owner_repo,
    github_token_for_source_ingest, is_entity_visible,
};
use crate::entity_docs_store::DOC_EMBEDDING_DIMENSION;
use crate::state::{
    ControlState, Entity, EntityDoc, EntityKnowledgeIndex, OperatorFeedEvent, OperatorFeedKind,
    ServerState,
};

/// `EntityDoc::source` for every row this module writes. Re-indexing deletes
/// exactly these and nothing else.
pub const DOC_SOURCE_IMPORT: &str = "import";

pub const STATUS_INDEXING: &str = "indexing";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_SKIPPED: &str = "skipped";

/// How long an `indexing` status may stand before it counts as abandoned.
///
/// The status is persisted on the entity, but the job that would finish it
/// lives only in this process: a restart mid-run (or a job that died) leaves
/// `indexing` behind forever, and reindex refused to start while it stood.
/// Indexing is deterministic local/HTTP work that finishes in seconds to a few
/// minutes, so past this age nothing is going to finish it.
pub const STALE_INDEXING_AFTER_MINUTES: i64 = 15;

/// What a client is told about an abandoned run.
const STALE_INDEXING_ERROR: &str =
    "Indexing was interrupted (the gateway restarted or the run stalled). Re-index to try again.";

/// Whether `index` says `indexing` but nothing can still be running it: it
/// started more than [`STALE_INDEXING_AFTER_MINUTES`] ago, or its start time
/// is missing or unreadable.
pub fn is_stale_indexing(index: &EntityKnowledgeIndex, now: DateTime<Utc>) -> bool {
    if index.status != STATUS_INDEXING {
        return false;
    }
    match DateTime::parse_from_rfc3339(&index.started_at) {
        Ok(started) => {
            now.signed_duration_since(started.with_timezone(&Utc))
                > Duration::minutes(STALE_INDEXING_AFTER_MINUTES)
        }
        Err(_) => true,
    }
}

/// The index status as clients should see it: an abandoned `indexing` reads
/// as `failed`, with a reason, so the UI offers re-index instead of a spinner
/// that never stops. The stored value is left alone; the next run replaces it.
pub fn effective_index(
    index: Option<EntityKnowledgeIndex>,
    now: DateTime<Utc>,
) -> Option<EntityKnowledgeIndex> {
    index.map(|mut i| {
        if is_stale_indexing(&i, now) {
            i.status = STATUS_FAILED.to_string();
            i.error = Some(STALE_INDEXING_ERROR.to_string());
        }
        i
    })
}

/// Per-doc ceiling. Docs ride the system prompt of every turn, so a README
/// that runs to 40k characters is cut, not carried.
const MAX_DOC_CHARS: usize = 6_000;
/// The README is the single most useful file; it gets a little more room.
const MAX_README_CHARS: usize = 8_000;
/// Conventions files plus `docs/*.md`, combined.
const MAX_EXTRA_DOCS: usize = 5;
const MAX_TREE_ENTRIES: usize = 300;
const TREE_DEPTH: usize = 3;
const EMBED_TIMEOUT_SECS: u64 = 20;
const EMBED_INPUT_CHARS: usize = 3_000;
const TRUNCATION_MARK: &str = "\n…(truncated)";

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".svelte-kit",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    "Pods",
    "DerivedData",
    ".idea",
    ".vscode",
    ".cache",
    ".turbo",
    "coverage",
    ".expo",
    ".gradle",
    ".dart_tool",
    ".pytest_mypy",
];
const README_CANDIDATES: &[&str] = &[
    "README.md",
    "README.MD",
    "readme.md",
    "Readme.md",
    "README",
    "README.rst",
    "README.txt",
];
const MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "Gemfile",
    "composer.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Package.swift",
    "app.json",
    "tauri.conf.json",
    "Dockerfile",
    "docker-compose.yml",
    "Makefile",
    "justfile",
    "fly.toml",
    "railway.json",
    "vercel.json",
    "netlify.toml",
    "wrangler.toml",
];
const CONVENTION_FILES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    "CONTRIBUTING.md",
    "ARCHITECTURE.md",
    "DESIGN.md",
    "ROADMAP.md",
    ".cursorrules",
];

// ─── Inputs ──────────────────────────────────────────────────────────────────

/// What to read. Mirrors the import classifier: a local directory is walked,
/// a URL is fetched.
#[derive(Debug, Clone)]
pub enum IndexSource {
    Local { path: PathBuf, kind: &'static str },
    Remote { url: String, kind: &'static str },
}

impl IndexSource {
    fn display(&self) -> String {
        match self {
            IndexSource::Local { path, .. } => path.display().to_string(),
            IndexSource::Remote { url, .. } => url.clone(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            IndexSource::Local { kind, .. } | IndexSource::Remote { kind, .. } => kind,
        }
    }

    /// Classify a raw operator value: an existing local directory, or an
    /// http(s) URL. Anything else is not indexable.
    pub fn from_hint(raw: &str) -> Option<IndexSource> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let kind = if github_owner_repo(trimmed).is_some() {
                "github_repo"
            } else {
                "website"
            };
            return Some(IndexSource::Remote {
                url: trimmed.to_string(),
                kind,
            });
        }
        let path = existing_local_path(Some(trimmed)).filter(|p| p.is_dir())?;
        let kind = if path.join(".git").exists() {
            "local_repo"
        } else {
            "local_folder"
        };
        Some(IndexSource::Local { path, kind })
    }
}

/// What the operator and the analysis said at import time, written as the
/// first doc so the entity's own framing survives later edits to its fields.
#[derive(Debug, Clone, Default)]
pub struct ImportBrief {
    pub summary: Option<String>,
    /// `(label, answer)` pairs from the context step, in display order.
    pub operator_context: Vec<(String, String)>,
    pub notes: Option<String>,
    /// Titles of the suggested next steps the operator kept.
    pub suggested: Vec<String>,
}

impl ImportBrief {
    fn is_empty(&self) -> bool {
        self.summary
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
            && self.operator_context.is_empty()
            && self
                .notes
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            && self.suggested.is_empty()
    }
}

/// Pick the best indexable source an entity already knows about, in order of
/// how directly it points at the work: an explicit hint, the recorded repo
/// path, an attached source URL, then the primary domain as a website.
pub fn resolve_index_source(entity: &Entity, hint: Option<&str>) -> Option<IndexSource> {
    if let Some(source) = hint.and_then(IndexSource::from_hint) {
        return Some(source);
    }
    if let Some(source) = entity
        .setup
        .repo_path
        .as_deref()
        .and_then(IndexSource::from_hint)
    {
        return Some(source);
    }
    if let Some(source) = entity
        .resources
        .iter()
        .map(|r| r.root.as_str())
        .filter(|root| root.starts_with("http://") || root.starts_with("https://"))
        .find_map(IndexSource::from_hint)
    {
        return Some(source);
    }
    entity
        .setup
        .primary_domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty() && d.contains('.') && !d.contains(' '))
        .map(|d| {
            if d.starts_with("http://") || d.starts_with("https://") {
                d.to_string()
            } else {
                format!("https://{d}")
            }
        })
        .and_then(|url| IndexSource::from_hint(&url))
}

// ─── Orchestration ───────────────────────────────────────────────────────────

/// Mark the entity `indexing` and run [`index_entity`] in the background.
/// Returns the status that was just recorded so callers can hand it straight
/// back to the client without a second lookup.
pub async fn begin_index(
    state: ServerState,
    workspace_id: &str,
    entity_id: &str,
    source: IndexSource,
    brief: Option<ImportBrief>,
) -> EntityKnowledgeIndex {
    let status = EntityKnowledgeIndex {
        status: STATUS_INDEXING.to_string(),
        source: source.display(),
        source_kind: source.kind().to_string(),
        doc_count: 0,
        embedded_count: 0,
        started_at: Utc::now().to_rfc3339(),
        indexed_at: None,
        error: None,
    };
    if let Some(control) = state.control_state.as_deref() {
        set_index(control, entity_id, status.clone()).await;
    }
    let workspace_id = workspace_id.to_string();
    let entity_id = entity_id.to_string();
    tokio::spawn(async move {
        index_entity(state, &workspace_id, &entity_id, source, brief).await;
    });
    status
}

/// Record that an entity has nothing to index. Distinct from never having
/// tried, so the UI can say *why* the knowledge card is empty.
pub async fn mark_skipped(
    control: &ControlState,
    entity_id: &str,
    reason: &str,
) -> EntityKnowledgeIndex {
    let status = EntityKnowledgeIndex {
        status: STATUS_SKIPPED.to_string(),
        started_at: Utc::now().to_rfc3339(),
        error: Some(reason.to_string()),
        ..Default::default()
    };
    set_index(control, entity_id, status.clone()).await;
    status
}

async fn set_index(control: &ControlState, entity_id: &str, status: EntityKnowledgeIndex) {
    let mut entities = control.entities.write().await;
    if let Some(entity) = entities.iter_mut().find(|e| e.id == entity_id) {
        entity.knowledge_index = Some(status);
    }
}

/// Build the index end to end. Failures land on the entity as `failed` with
/// a human-readable reason; nothing here panics or propagates.
pub async fn index_entity(
    state: ServerState,
    workspace_id: &str,
    entity_id: &str,
    source: IndexSource,
    brief: Option<ImportBrief>,
) {
    let Some(control) = state.control_state.clone() else {
        return;
    };
    let started_at = Utc::now();
    let outcome = build_and_store(
        &state,
        control.as_ref(),
        workspace_id,
        entity_id,
        &source,
        brief,
    )
    .await;
    let (entity_name, status) = {
        let mut entities = control.entities.write().await;
        let Some(entity) = entities.iter_mut().find(|e| e.id == entity_id) else {
            return;
        };
        let mut status = entity.knowledge_index.clone().unwrap_or_default();
        status.source = source.display();
        status.source_kind = source.kind().to_string();
        if status.started_at.is_empty() {
            status.started_at = started_at.to_rfc3339();
        }
        match &outcome {
            Ok(report) => {
                status.status = STATUS_READY.to_string();
                status.doc_count = report.titles.len();
                status.embedded_count = report.embedded;
                status.indexed_at = Some(Utc::now().to_rfc3339());
                status.error = None;
            }
            Err(message) => {
                status.status = STATUS_FAILED.to_string();
                status.indexed_at = Some(Utc::now().to_rfc3339());
                status.error = Some(message.clone());
            }
        }
        entity.knowledge_index = Some(status.clone());
        (entity.name.clone(), status)
    };

    match outcome {
        Ok(report) => {
            tracing::info!(
                entity_id,
                docs = report.titles.len(),
                embedded = report.embedded,
                source = %status.source,
                "entity knowledge indexed"
            );
            let line = format!(
                "Knowledge indexed from {} ({} docs: {}).",
                status.source,
                report.titles.len(),
                report.titles.join(", ")
            );
            if let Err(e) = crate::entity_memory::append_entity_short_line(
                state.memory.clone(),
                entity_id,
                &line,
            )
            .await
            {
                tracing::warn!(entity_id, error = %e, "knowledge index memory line failed");
            }
            control.operator_feed.write().await.push(OperatorFeedEvent {
                id: Uuid::new_v4().to_string(),
                timestamp: Utc::now().to_rfc3339(),
                kind: OperatorFeedKind::EntitySourceImported,
                task_id: None,
                mission_id: None,
                entity_id: Some(entity_id.to_string()),
                connection_id: None,
                title: format!("Knowledge indexed: {entity_name}"),
                detail: Some(
                    format!(
                        "{} docs from {}: {}",
                        report.titles.len(),
                        status.source,
                        report.titles.join(", ")
                    )
                    .chars()
                    .take(500)
                    .collect(),
                ),
                approval_id: None,
            });
        }
        Err(message) => {
            tracing::warn!(entity_id, source = %status.source, error = %message, "entity knowledge indexing failed");
        }
    }
}

struct IndexReport {
    titles: Vec<String>,
    embedded: usize,
}

async fn build_and_store(
    state: &ServerState,
    control: &ControlState,
    workspace_id: &str,
    entity_id: &str,
    source: &IndexSource,
    brief: Option<ImportBrief>,
) -> Result<IndexReport, String> {
    let store = state
        .entity_docs_store
        .clone()
        .ok_or_else(|| "The knowledge store is not available on this gateway.".to_string())?;

    let mut drafts: Vec<DraftDoc> = Vec::new();
    if let Some(brief) = brief.filter(|b| !b.is_empty()) {
        drafts.push(brief_doc(&brief, &source.display()));
    }
    match source {
        IndexSource::Local { path, kind } => {
            let path = path.clone();
            let is_repo = *kind == "local_repo";
            let collected = tokio::task::spawn_blocking(move || collect_local_docs(&path, is_repo))
                .await
                .map_err(|e| format!("Indexing task failed: {e}"))?;
            drafts.extend(collected);
        }
        IndexSource::Remote { url, .. } => {
            let token =
                github_token_for_source_ingest(control, workspace_id, Some(entity_id)).await;
            let context = fetch_entity_source_with_token(url, token.as_deref())
                .await
                .map_err(|status| format!("Could not fetch {url} (HTTP {status})."))?;
            if context.text.trim().is_empty() {
                return Err(format!("{url} returned no readable content."));
            }
            let title = context
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| format!("Source: {t}"))
                .unwrap_or_else(|| format!("Source: {}", context.url));
            drafts.push(DraftDoc {
                title,
                content: truncate_chars(&context.text, MAX_README_CHARS),
                url: Some(context.url.clone()),
            });
        }
    }
    if drafts.is_empty() {
        return Err("Nothing readable was found at the source.".to_string());
    }

    // Replace what a previous run wrote; hand-written docs are not ours to touch.
    match store.list_for_entity(entity_id).await {
        Ok(existing) => {
            for doc in existing.iter().filter(|d| d.source == DOC_SOURCE_IMPORT) {
                if let Err(e) = store.delete(&doc.id).await {
                    tracing::warn!(entity_id, doc_id = %doc.id, error = %e, "stale knowledge doc delete failed");
                }
            }
        }
        Err(e) => {
            tracing::warn!(entity_id, error = %e, "knowledge doc listing failed; not pruning")
        }
    }

    let embedder = state
        .embedding_provider
        .as_ref()
        .map(|p| p.0.clone())
        .filter(|p| p.dimensions() == DOC_EMBEDDING_DIMENSION);

    // Newest-first is the store's order and the prompt's order. Stamp the
    // docs so the brief and README come first, then the map, then the rest.
    let base = Utc::now();
    let mut titles = Vec::with_capacity(drafts.len());
    let mut embedded = 0usize;
    for (i, draft) in drafts.into_iter().enumerate() {
        let stamp = (base - Duration::milliseconds(i as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut doc = EntityDoc {
            id: Uuid::new_v4().to_string(),
            entity_id: entity_id.to_string(),
            title: draft.title.clone(),
            content: draft.content,
            source: DOC_SOURCE_IMPORT.to_string(),
            url: draft.url,
            enabled: true,
            embedding: None,
            created_at: stamp.clone(),
            updated_at: stamp,
            created_by: Some("entity-import".to_string()),
        };
        if let Some(provider) = embedder.as_ref() {
            let input: String = format!("{}\n\n{}", doc.title, doc.content)
                .chars()
                .take(EMBED_INPUT_CHARS)
                .collect();
            match tokio::time::timeout(
                std::time::Duration::from_secs(EMBED_TIMEOUT_SECS),
                provider.embed(&input),
            )
            .await
            {
                Ok(Ok(vector)) if vector.len() == DOC_EMBEDDING_DIMENSION => {
                    doc.embedding = Some(vector);
                    embedded += 1;
                }
                Ok(Ok(vector)) => {
                    tracing::warn!(
                        entity_id,
                        dims = vector.len(),
                        "knowledge doc embedding has the wrong dimension; stored without one"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(entity_id, title = %doc.title, error = %e, "knowledge doc embedding failed; stored without one");
                }
                Err(_) => {
                    tracing::warn!(entity_id, title = %doc.title, "knowledge doc embedding timed out; stored without one");
                }
            }
        }
        store
            .upsert_doc(&doc)
            .await
            .map_err(|e| format!("Could not save “{}”: {e}", doc.title))?;
        titles.push(draft.title);
    }
    Ok(IndexReport { titles, embedded })
}

// ─── Collection ──────────────────────────────────────────────────────────────

/// A doc before it has an id or a home.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DraftDoc {
    pub title: String,
    pub content: String,
    pub url: Option<String>,
}

fn brief_doc(brief: &ImportBrief, source: &str) -> DraftDoc {
    let mut out = format!(
        "Indexed from {source} on {}.\n",
        Utc::now().format("%Y-%m-%d")
    );
    if !brief.operator_context.is_empty() {
        out.push_str("\n## Operator's framing (authoritative)\n");
        for (label, answer) in &brief.operator_context {
            out.push_str(&format!("- {label}: {}\n", answer.trim()));
        }
    }
    if let Some(summary) = brief
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\n## Analysis summary at import\n");
        out.push_str(summary);
        out.push('\n');
    }
    if let Some(notes) = brief
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\n## Notes\n");
        out.push_str(notes);
        out.push('\n');
    }
    if !brief.suggested.is_empty() {
        out.push_str("\n## Next steps kept at import\n");
        for title in &brief.suggested {
            out.push_str(&format!("- {}\n", title.trim()));
        }
    }
    DraftDoc {
        title: "Import brief".to_string(),
        content: truncate_chars(out.trim_end(), MAX_DOC_CHARS),
        url: None,
    }
}

/// Read everything worth keeping from a local project: README, a repository
/// map (tree + manifests + git state), conventions files, and `docs/*.md`.
/// Synchronous — run it on a blocking thread.
pub(crate) fn collect_local_docs(root: &Path, is_repo: bool) -> Vec<DraftDoc> {
    let mut out = Vec::new();

    if let Some((name, text)) = README_CANDIDATES
        .iter()
        .find_map(|c| std::fs::read_to_string(root.join(c)).ok().map(|t| (*c, t)))
    {
        out.push(DraftDoc {
            title: format!("README ({name})"),
            content: truncate_chars(&text, MAX_README_CHARS),
            url: None,
        });
    }

    out.push(DraftDoc {
        title: "Repository map".to_string(),
        content: truncate_chars(&repository_map(root, is_repo), MAX_DOC_CHARS),
        url: None,
    });

    let mut extras = 0usize;
    for name in CONVENTION_FILES {
        if extras >= MAX_EXTRA_DOCS {
            break;
        }
        if let Ok(text) = std::fs::read_to_string(root.join(name))
            && !text.trim().is_empty()
        {
            out.push(DraftDoc {
                title: format!("Conventions: {name}"),
                content: truncate_chars(&text, MAX_DOC_CHARS),
                url: None,
            });
            extras += 1;
        }
    }
    let docs_dir = root.join("docs");
    if extras < MAX_EXTRA_DOCS
        && let Ok(entries) = std::fs::read_dir(&docs_dir)
    {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_ascii_lowercase().ends_with(".md"))
            .collect();
        names.sort();
        for name in names {
            if extras >= MAX_EXTRA_DOCS {
                break;
            }
            if let Ok(text) = std::fs::read_to_string(docs_dir.join(&name))
                && !text.trim().is_empty()
            {
                out.push(DraftDoc {
                    title: format!("docs/{name}"),
                    content: truncate_chars(&text, MAX_DOC_CHARS),
                    url: None,
                });
                extras += 1;
            }
        }
    }
    out
}

fn repository_map(root: &Path, is_repo: bool) -> String {
    let mut out = format!(
        "Root: {}\nKind: {}\n",
        root.display(),
        if is_repo { "git repository" } else { "folder" }
    );
    if is_repo {
        if let Some(branch) = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            out.push_str(&format!("Branch: {}\n", branch.trim()));
        }
        if let Some(remote) = git_output(root, &["remote", "get-url", "origin"]) {
            out.push_str(&format!("Remote: {}\n", remote.trim()));
        }
    }
    let manifests: Vec<&str> = MANIFESTS
        .iter()
        .copied()
        .filter(|m| root.join(m).is_file())
        .collect();
    if !manifests.is_empty() {
        out.push_str(&format!("Manifests: {}\n", manifests.join(", ")));
    }
    let mut entries = Vec::new();
    walk_tree(root, root, 0, &mut entries);
    let shown = entries.len().min(MAX_TREE_ENTRIES);
    out.push_str(&format!(
        "\nTree (depth ≤ {TREE_DEPTH}, {shown} of {} entries):\n",
        entries.len()
    ));
    for line in entries.iter().take(MAX_TREE_ENTRIES) {
        out.push_str(line);
        out.push('\n');
    }
    if is_repo && let Some(log) = git_output(root, &["log", "--oneline", "-20", "--no-decorate"]) {
        let log = log.trim();
        if !log.is_empty() {
            out.push_str("\nRecent commits (newest first):\n");
            out.push_str(log);
            out.push('\n');
        }
    }
    out
}

fn walk_tree(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth >= TREE_DEPTH || out.len() >= MAX_TREE_ENTRIES * 2 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<(bool, String, PathBuf)> = read
        .filter_map(|e| e.ok())
        .map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            (path.is_dir(), name, path)
        })
        .filter(|(is_dir, name, _)| {
            if *is_dir && SKIP_DIRS.contains(&name.as_str()) {
                return false;
            }
            !name.starts_with('.') || name == ".github"
        })
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (is_dir, _name, path) in items {
        let rel = path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        if is_dir {
            out.push(format!("{rel}/"));
            walk_tree(root, &path, depth + 1, out);
        } else {
            out.push(rel);
        }
    }
}

/// One git read, no shell: the binary is an absolute path we located and the
/// project directory only ever reaches the process as `current_dir`.
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let git = crate::entity_workspace::locate_git()?;
    let output = consistent_world_common::platform::std_command(git)
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Cut on a char boundary and say so. A doc that was cut still ends in a
/// visible marker, so the model never mistakes the cut for the end.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut cut: String = trimmed.chars().take(max).collect();
    cut.push_str(TRUNCATION_MARK);
    cut
}

// ─── HTTP surface ────────────────────────────────────────────────────────────

/// One indexed doc as the UI sees it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeDocView {
    pub id: String,
    pub title: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub enabled: bool,
    pub chars: usize,
    pub has_embedding: bool,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<EntityDoc> for KnowledgeDocView {
    fn from(doc: EntityDoc) -> Self {
        KnowledgeDocView {
            id: doc.id,
            title: doc.title,
            source: doc.source,
            url: doc.url,
            enabled: doc.enabled,
            chars: doc.content.chars().count(),
            has_embedding: doc.embedding.as_ref().is_some_and(|v| !v.is_empty()),
            content: doc.content,
            created_at: doc.created_at,
            updated_at: doc.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<EntityKnowledgeIndex>,
    pub docs: Vec<KnowledgeDocView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReindexResponse {
    pub started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<EntityKnowledgeIndex>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReindexRequest {
    /// Optional override: index this path/URL instead of what the entity
    /// already records.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDocEnabledRequest {
    pub enabled: bool,
}

async fn visible_entity(
    state: &ServerState,
    workspace: &WorkspaceCtx,
    entity_id: &str,
) -> Result<Entity, StatusCode> {
    let control = state
        .control_state
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let entities = control.entities.read().await;
    entities
        .iter()
        .find(|e| {
            e.id == entity_id && is_entity_visible(e, &workspace.id, Some(&workspace.principal_id))
        })
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)
}

/// `GET /api/entities/{id}/knowledge`
pub(crate) async fn get_entity_knowledge(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    AxumPath(entity_id): AxumPath<String>,
) -> Result<Json<KnowledgeView>, StatusCode> {
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let docs = match state.entity_docs_store.as_ref() {
        Some(store) => store.list_for_entity(&entity.id).await.map_err(|e| {
            tracing::warn!(entity_id = %entity.id, error = %e, "knowledge doc listing failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?,
        None => Vec::new(),
    };
    Ok(Json(KnowledgeView {
        index: effective_index(entity.knowledge_index, Utc::now()),
        docs: docs.into_iter().map(KnowledgeDocView::from).collect(),
    }))
}

/// `POST /api/entities/{id}/knowledge/reindex`
pub(crate) async fn reindex_entity_knowledge(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    AxumPath(entity_id): AxumPath<String>,
    body: Option<Json<ReindexRequest>>,
) -> Result<Json<ReindexResponse>, StatusCode> {
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let now = Utc::now();
    if state.entity_docs_store.is_none() {
        return Ok(Json(ReindexResponse {
            started: false,
            reason: Some("The knowledge store is not available on this gateway.".to_string()),
            index: effective_index(entity.knowledge_index, now),
        }));
    }
    // Only a *live* run blocks a reindex. A stale `indexing` (see
    // `STALE_INDEXING_AFTER_MINUTES`) was left by a restart or a dead job and
    // would otherwise refuse every reindex forever.
    if entity
        .knowledge_index
        .as_ref()
        .is_some_and(|k| k.status == STATUS_INDEXING && !is_stale_indexing(k, now))
    {
        return Ok(Json(ReindexResponse {
            started: false,
            reason: Some("Indexing is already running for this entity.".to_string()),
            index: entity.knowledge_index,
        }));
    }
    let hint = body.and_then(|Json(b)| b.source);
    let Some(source) = resolve_index_source(&entity, hint.as_deref()) else {
        let control = state
            .control_state
            .as_deref()
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let reason = "No source to index: set a repo path or folder that exists on this machine, or attach a URL.";
        let index = mark_skipped(control, &entity.id, reason).await;
        return Ok(Json(ReindexResponse {
            started: false,
            reason: Some(reason.to_string()),
            index: Some(index),
        }));
    };
    let index = begin_index(state.clone(), &workspace.id, &entity.id, source, None).await;
    Ok(Json(ReindexResponse {
        started: true,
        reason: None,
        index: Some(index),
    }))
}

/// `PATCH /api/entities/{id}/knowledge/docs/{doc_id}`
pub(crate) async fn set_knowledge_doc_enabled(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    AxumPath((entity_id, doc_id)): AxumPath<(String, String)>,
    Json(body): Json<SetDocEnabledRequest>,
) -> Result<Json<KnowledgeDocView>, StatusCode> {
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let store = state
        .entity_docs_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mut doc = store
        .get(&doc_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|d| d.entity_id == entity.id)
        .ok_or(StatusCode::NOT_FOUND)?;
    doc.enabled = body.enabled;
    doc.updated_at = Utc::now().to_rfc3339();
    store
        .upsert_doc(&doc)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(KnowledgeDocView::from(doc)))
}

/// `DELETE /api/entities/{id}/knowledge/docs/{doc_id}`
pub(crate) async fn delete_knowledge_doc(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    AxumPath((entity_id, doc_id)): AxumPath<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    let entity = visible_entity(&state, &workspace, &entity_id).await?;
    let store = state
        .entity_docs_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let doc = store
        .get(&doc_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|d| d.entity_id == entity.id)
        .ok_or(StatusCode::NOT_FOUND)?;
    store
        .delete(&doc.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(control) = state.control_state.as_deref() {
        let mut entities = control.entities.write().await;
        if let Some(e) = entities.iter_mut().find(|e| e.id == entity.id)
            && let Some(index) = e.knowledge_index.as_mut()
            && doc.source == DOC_SOURCE_IMPORT
        {
            index.doc_count = index.doc_count.saturating_sub(1);
            if doc.embedding.as_ref().is_some_and(|v| !v.is_empty()) {
                index.embedded_count = index.embedded_count.saturating_sub(1);
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EntityResource, EntitySetup};

    #[test]
    fn an_old_indexing_status_is_stale_and_reads_as_failed() {
        let now = Utc::now();
        let at = |minutes_ago: i64| EntityKnowledgeIndex {
            status: STATUS_INDEXING.to_string(),
            started_at: (now - Duration::minutes(minutes_ago)).to_rfc3339(),
            ..Default::default()
        };
        // A run that just started is live: reindex stays refused.
        assert!(!is_stale_indexing(&at(2), now));
        assert_eq!(
            effective_index(Some(at(2)), now).unwrap().status,
            STATUS_INDEXING
        );
        // One left behind by a restart is stale and reported as failed.
        let stale = at(STALE_INDEXING_AFTER_MINUTES + 1);
        assert!(is_stale_indexing(&stale, now));
        let shown = effective_index(Some(stale), now).unwrap();
        assert_eq!(shown.status, STATUS_FAILED);
        assert!(shown.error.is_some());
        // No readable start time: nothing can be shown to be running it.
        let unknown = EntityKnowledgeIndex {
            status: STATUS_INDEXING.to_string(),
            ..Default::default()
        };
        assert!(is_stale_indexing(&unknown, now));
        // Other statuses are never stale.
        let ready = EntityKnowledgeIndex {
            status: STATUS_READY.to_string(),
            ..Default::default()
        };
        assert!(!is_stale_indexing(&ready, now));
    }

    fn scratch_project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cw-knowledge-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/left-pad")).unwrap();
        std::fs::write(dir.join("README.md"), "# Widget\n\nA widget for widgets.\n").unwrap();
        std::fs::write(dir.join("package.json"), "{\"name\":\"widget\"}").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "Never push on Fridays.\n").unwrap();
        std::fs::write(dir.join("docs/plan.md"), "Ship v1.\n").unwrap();
        std::fs::write(dir.join("docs/notes.txt"), "not markdown\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("node_modules/left-pad/index.js"), "x").unwrap();
        dir
    }

    #[test]
    fn local_collection_keeps_readme_map_conventions_and_docs() {
        let dir = scratch_project("collect");
        let docs = collect_local_docs(&dir, false);
        let titles: Vec<&str> = docs.iter().map(|d| d.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "README (README.md)",
                "Repository map",
                "Conventions: CLAUDE.md",
                "docs/plan.md"
            ],
            "one doc per source, in prompt order; non-markdown docs are skipped"
        );
        let map = &docs[1].content;
        assert!(
            map.contains("Manifests: package.json"),
            "manifests are listed: {map}"
        );
        assert!(map.contains("src/"), "tree lists directories: {map}");
        assert!(
            map.contains("src/main.rs"),
            "tree descends into directories: {map}"
        );
        assert!(
            !map.contains("node_modules"),
            "dependency folders are skipped: {map}"
        );
        assert!(map.contains("Kind: folder"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn truncation_marks_the_cut_on_a_char_boundary() {
        let text = "héllo wörld ".repeat(100);
        let cut = truncate_chars(&text, 50);
        assert!(cut.ends_with(TRUNCATION_MARK));
        assert_eq!(cut.chars().count(), 50 + TRUNCATION_MARK.chars().count());
        assert_eq!(truncate_chars("  short  ", 50), "short");
    }

    #[test]
    fn brief_doc_carries_the_operators_framing_first() {
        let brief = ImportBrief {
            summary: Some("It is a widget.".to_string()),
            operator_context: vec![("What it is".to_string(), "A widget".to_string())],
            notes: Some("Be careful.".to_string()),
            suggested: vec!["Add tests".to_string()],
        };
        let doc = brief_doc(&brief, "/tmp/widget");
        assert_eq!(doc.title, "Import brief");
        let framing = doc.content.find("Operator's framing").unwrap();
        let summary = doc.content.find("Analysis summary").unwrap();
        assert!(
            framing < summary,
            "operator answers precede the model's summary"
        );
        assert!(doc.content.contains("- What it is: A widget"));
        assert!(doc.content.contains("- Add tests"));
        assert!(ImportBrief::default().is_empty(), "an empty brief is empty");
    }

    #[test]
    fn source_resolution_prefers_hint_then_repo_then_url_then_domain() {
        let dir = scratch_project("resolve");
        let mut entity = Entity {
            setup: EntitySetup {
                repo_path: Some(dir.display().to_string()),
                primary_domain: Some("widget.example".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        entity.resources.push(EntityResource {
            id: "res_1".to_string(),
            kind: "url".to_string(),
            root: "https://github.com/acme/widget".to_string(),
            read_allowed: true,
            write_allowed: false,
            exec_allowed: false,
        });

        match resolve_index_source(&entity, Some("https://acme.example")).unwrap() {
            IndexSource::Remote { url, kind } => {
                assert_eq!(url, "https://acme.example");
                assert_eq!(kind, "website");
            }
            other => panic!("hint should win: {other:?}"),
        }
        match resolve_index_source(&entity, None).unwrap() {
            IndexSource::Local { kind, .. } => assert_eq!(kind, "local_folder"),
            other => panic!("repo path should be next: {other:?}"),
        }
        entity.setup.repo_path = None;
        match resolve_index_source(&entity, None).unwrap() {
            IndexSource::Remote { url, kind } => {
                assert_eq!(url, "https://github.com/acme/widget");
                assert_eq!(kind, "github_repo");
            }
            other => panic!("attached URL should be next: {other:?}"),
        }
        entity.resources.clear();
        match resolve_index_source(&entity, None).unwrap() {
            IndexSource::Remote { url, .. } => assert_eq!(url, "https://widget.example"),
            other => panic!("domain is the last resort: {other:?}"),
        }
        entity.setup.primary_domain = None;
        assert!(resolve_index_source(&entity, None).is_none());
        assert!(
            resolve_index_source(&entity, Some("/definitely/not/here")).is_none(),
            "a missing path is not a source"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
