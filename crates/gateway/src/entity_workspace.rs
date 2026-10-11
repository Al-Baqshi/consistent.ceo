//! "Does this thing have a repo?" — asked out loud, once, and remembered.
//!
//! An entity can be a codebase, a folder of drafts, or nothing but notes. Until
//! now the gateway never asked: [`crate::control_api::resolve_entity_agent_run_cwd`]
//! walked `local_path` → `default_workspace_root` → `setup.repo_path` and, when
//! all three were empty, quietly called [`crate::local_vault::ensure_entity_folder`]
//! — *creating* a notes folder under `~/.consistent-world/vault/...` and handing
//! it to an unattended coding agent as if the operator had chosen it. Nobody
//! chose it. With the task executor now running on its own, that guess is how a
//! coding agent ends up writing a product into a scratch directory.
//!
//! ### The distinction this module exists to make
//!
//! A path under app-owned storage and a path the operator typed are not the
//! same fact, and neither is "the operator said the vault is fine" the same as
//! "the operator was never asked". [`WorkspaceKind`] separates all four, and
//! [`EntitySetup::workspace_choice`](crate::state::EntitySetup::workspace_choice)
//! is what turns the third into a real answer rather than an absence.
//!
//! ### Shape
//!
//! [`workspace_status`] is pure — a function of an [`Entity`] and the
//! filesystem, with no clock, database, or network — so the whole matrix is
//! unit-testable. The HTTP surface in [`crate::control_api`] and the gate in
//! [`crate::task_executor`] both read it; neither re-derives the rules.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::Entity;

/// Recorded in `EntitySetup::workspace_choice`. Deliberately strings rather
/// than a bare enum in the DB: Surreal rows written by older builds carry no
/// value at all, and `None` has to keep meaning "never asked".
pub const CHOICE_REPO: &str = "repo";
pub const CHOICE_LOCAL_FOLDER: &str = "local_folder";
pub const CHOICE_VAULT_ONLY: &str = "vault_only";

/// Accepted on the decision endpoint but never stored as-is: both resolve to
/// one of the three settled choices above once a path is known.
pub const DECISION_NEW_PROJECT: &str = "new_project";
pub const DECISION_GITHUB: &str = "github";

/// Where an entity's work actually happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// A git checkout the operator pointed at.
    Repo,
    /// A plain directory the operator pointed at, outside app-owned storage.
    LocalFolder,
    /// The app's own vault folder — either chosen deliberately or the silent
    /// fallback, told apart by `needs_decision`.
    VaultOnly,
    /// Nothing usable: no path at all, or one configured that is not on disk.
    Unset,
}

/// One answer the operator can give, rendered by the client as a button.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOption {
    pub choice: String,
    pub label: String,
    pub detail: String,
    /// True when the client must collect a path before posting this choice.
    pub needs_path: bool,
    /// For choices this endpoint cannot finish on its own, the endpoint that
    /// can. GitHub's device-code flow already exists; it is not reimplemented
    /// here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// The answer to "where does this entity's work live, and does anyone know?".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub entity_id: String,
    pub kind: WorkspaceKind,
    /// The winning path, or the configured-but-missing one when `exists` is
    /// false. `None` only when nothing was ever configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub exists: bool,
    /// The only place to work is app-owned storage nobody picked.
    pub is_vault_fallback: bool,
    /// Autonomous work must stop and ask before touching anything.
    pub needs_decision: bool,
    /// What the operator answered previously, if ever.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    /// Set when the operator has already been asked and has not answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked_at: Option<String>,
    /// The question, phrased for a human. Present exactly when
    /// `needs_decision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    pub options: Vec<WorkspaceOption>,
}

/// Directories the app creates for itself. A path under one of these was never
/// an operator's decision — it is where the gateway puts things when nobody
/// told it anywhere better.
pub fn app_owned_roots() -> Vec<PathBuf> {
    let mut roots = vec![crate::local_vault::vault_root()];
    // `entity_creation::normalize_local_path_input` re-roots relative inputs
    // under `~/.consistent-world/entities/<slug>`, so plenty of entities carry
    // an app-owned `local_path` that looks operator-chosen. It is not.
    roots.push(PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world",
    )));
    roots.sort();
    roots.dedup();
    roots
}

/// Best-effort canonicalisation. Comparing `/tmp/x` against `/private/tmp/x` on
/// macOS otherwise makes an app-owned path look operator-chosen.
fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_app_owned(path: &Path, roots: &[PathBuf]) -> bool {
    let path = real(path);
    roots.iter().any(|root| path.starts_with(real(root)))
}

/// A directory with a `.git` in it is a repo, whichever field named it.
fn looks_like_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// The raw, un-validated paths an entity carries, most specific first.
fn configured_paths(entity: &Entity) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut push = |field: &'static str, value: Option<&str>| {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            out.push((field, v.to_string()));
        }
    };
    push("repoPath", entity.setup.repo_path.as_deref());
    push("localPath", entity.local_path.as_deref());
    push(
        "defaultWorkspaceRoot",
        entity.default_workspace_root.as_deref(),
    );
    out
}

/// Read the recorded choice, treating blank as absent.
pub fn recorded_choice(entity: &Entity) -> Option<String> {
    entity
        .setup
        .workspace_choice
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_ascii_lowercase())
}

/// The four states, decided against `app_roots` rather than the real vault so
/// tests can stand up a fake one.
pub fn workspace_status_with(entity: &Entity, app_roots: &[PathBuf]) -> WorkspaceStatus {
    let choice = recorded_choice(entity);
    let configured = configured_paths(entity);

    // Only paths that survive `existing_local_path`'s rules count: an http(s)
    // value is a website, not a directory, and a path that is not on disk is a
    // promise, not a workspace.
    let on_disk: Vec<(&'static str, PathBuf)> = configured
        .iter()
        .filter_map(|(field, raw)| {
            crate::control_api::existing_local_path(Some(raw.as_str())).map(|p| (*field, p))
        })
        .collect();

    let chosen = on_disk
        .iter()
        .find(|(_, path)| !is_app_owned(path, app_roots));

    let (kind, path, exists, is_vault_fallback) = match chosen {
        Some((field, path)) => {
            let kind = if *field == "repoPath" || looks_like_repo(path) {
                WorkspaceKind::Repo
            } else {
                WorkspaceKind::LocalFolder
            };
            (kind, Some(path.clone()), true, false)
        }
        // Everything that exists is app-owned, or the operator settled on the
        // vault and there is nothing else.
        None if !on_disk.is_empty() || choice.as_deref() == Some(CHOICE_VAULT_ONLY) => {
            // Fall back to the entity's notes folder so a settled vault-only
            // entity still reports *where* it lives. Without this, clearing a
            // stale path leaves the UI showing a workspace with no location at
            // all, even though the folder is right there on disk.
            let path = on_disk.first().map(|(_, p)| p.clone()).or_else(|| {
                Some(crate::local_vault::entity_dir(
                    crate::capture::entity_workspace_id(entity),
                    entity,
                ))
            });
            let exists = path.as_deref().is_some_and(std::path::Path::is_dir);
            (WorkspaceKind::VaultOnly, path, exists, true)
        }
        // Nothing usable. A path may still be configured — say so, because
        // "you pointed at a folder that isn't there" is a different fix from
        // "you never pointed anywhere".
        None => (
            WorkspaceKind::Unset,
            configured.first().map(|(_, raw)| PathBuf::from(raw)),
            false,
            false,
        ),
    };

    // The one rule that matters: a settled vault choice is an answer, and an
    // unsettled one is not.
    let needs_decision = match kind {
        WorkspaceKind::Repo | WorkspaceKind::LocalFolder => false,
        WorkspaceKind::VaultOnly => choice.as_deref() != Some(CHOICE_VAULT_ONLY),
        WorkspaceKind::Unset => true,
    };

    WorkspaceStatus {
        entity_id: entity.id.clone(),
        kind,
        path: path.map(|p| p.to_string_lossy().to_string()),
        exists,
        is_vault_fallback,
        needs_decision,
        choice,
        asked_at: entity
            .setup
            .workspace_asked_at
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string()),
        question: needs_decision.then(|| workspace_question(entity)),
        options: workspace_options(&entity.id),
    }
}

/// [`workspace_status_with`] against the real app-owned roots.
pub fn workspace_status(entity: &Entity) -> WorkspaceStatus {
    workspace_status_with(entity, &app_owned_roots())
}

/// The owner's own words, addressed to one entity.
pub fn workspace_question(entity: &Entity) -> String {
    format!(
        "Does {} have a repo? If not — should I just keep it in your local notes, \
         start a new project folder (I can `git init` it), or do you want to connect GitHub?",
        entity.name.trim()
    )
}

/// The fixed menu of answers rendered alongside [`workspace_question`]. Same
/// four choices for every entity; only the GitHub option's `via` URL varies.
fn workspace_options(entity_id: &str) -> Vec<WorkspaceOption> {
    vec![
        WorkspaceOption {
            choice: CHOICE_LOCAL_FOLDER.to_string(),
            label: "Point at a folder I already have".to_string(),
            detail:
                "An existing repo checkout or project directory. I'll work there and nowhere else."
                    .to_string(),
            needs_path: true,
            via: None,
        },
        WorkspaceOption {
            choice: DECISION_NEW_PROJECT.to_string(),
            label: "Start a new project folder".to_string(),
            detail: "I'll create the directory, and run `git init` in it if you want a repo."
                .to_string(),
            needs_path: true,
            via: None,
        },
        WorkspaceOption {
            choice: CHOICE_VAULT_ONLY.to_string(),
            label: "Keep it in notes only".to_string(),
            detail: "No code. This entity lives in its vault folder and agents won't build in it."
                .to_string(),
            needs_path: false,
            via: None,
        },
        WorkspaceOption {
            choice: DECISION_GITHUB.to_string(),
            label: "Connect GitHub".to_string(),
            detail: "Authorize GitHub first, then tell me which local checkout to work in."
                .to_string(),
            needs_path: false,
            via: Some(format!("/api/entities/{entity_id}/connections/sessions")),
        },
    ]
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// Everything that can go wrong answering the question, kept as data so the
/// HTTP layer picks the status code and the operator gets a sentence rather
/// than a bare 400.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    UnknownChoice(String),
    PathRequired(String),
    NotAbsolute(String),
    NotADirectory(String),
    Create(String),
    GitMissing,
    GitFailed(String),
}

impl WorkspaceError {
    pub fn message(&self) -> String {
        match self {
            Self::UnknownChoice(c) => format!(
                "'{c}' is not a workspace choice. Use local_folder, new_project, vault_only, or github."
            ),
            Self::PathRequired(c) => format!("The '{c}' choice needs a path."),
            Self::NotAbsolute(p) => format!(
                "'{p}' is a relative path. Give an absolute path or one starting with ~/ \
                 so it is unambiguous which folder you mean."
            ),
            Self::NotADirectory(p) => format!("'{p}' exists but is not a directory."),
            Self::Create(e) => format!("Could not create that directory: {e}"),
            Self::GitMissing => {
                "git is not installed (or not on PATH), so I cannot initialise a repo there. \
                 Install git, or create the folder without git init."
                    .to_string()
            }
            Self::GitFailed(e) => format!("git init failed: {e}"),
        }
    }

    /// Bad input from the client versus an environment problem on the machine.
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::UnknownChoice(_)
                | Self::PathRequired(_)
                | Self::NotAbsolute(_)
                | Self::NotADirectory(_)
        )
    }
}

/// A decision posted by the client.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDecision {
    pub choice: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub git_init: bool,
}

/// What a decision changes about the entity, decided without touching shared
/// state so the path handling can be tested on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePlan {
    /// Written to `entity.local_path`. `None` leaves it alone.
    pub local_path: Option<String>,
    /// Written to `entity.setup.repo_path`. `None` leaves it alone.
    pub repo_path: Option<String>,
    /// Recorded as the settled answer. `None` means the answer is not settled
    /// yet (GitHub, mid-flow) and the question should stay open.
    pub choice: Option<String>,
    /// The directory to create, when there is one.
    pub ensure_dir: Option<PathBuf>,
    pub git_init: bool,
    /// The client's next move, for choices this endpoint cannot finish.
    pub next_endpoint: Option<String>,
    pub message: String,
}

/// Expand `~`, and insist on an absolute result.
///
/// Deliberately *not* [`crate::entity_creation`]'s `normalize_local_path_input`:
/// that one silently re-roots a relative input under app storage, which is
/// exactly the trap here — someone answering "connect my repo" with `foo`
/// would get a brand new empty folder in the vault and no indication anything
/// was ignored. A relative path is an ambiguous answer, so it is refused.
pub fn resolve_decision_path(raw: &str) -> Result<PathBuf, WorkspaceError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorkspaceError::NotAbsolute(raw.to_string()));
    }
    let expanded = crate::config::expand_tilde_path(trimmed);
    let path = PathBuf::from(&expanded);
    if !path.is_absolute() {
        return Err(WorkspaceError::NotAbsolute(trimmed.to_string()));
    }
    if path.exists() && !path.is_dir() {
        return Err(WorkspaceError::NotADirectory(expanded));
    }
    Ok(path)
}

/// Turn a posted decision into a plan. Pure: no directory is created and no
/// process is spawned here.
pub fn plan_decision(
    entity_id: &str,
    decision: &WorkspaceDecision,
) -> Result<WorkspacePlan, WorkspaceError> {
    let choice = decision.choice.trim().to_ascii_lowercase();
    let raw_path = decision
        .path
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    match choice.as_str() {
        CHOICE_VAULT_ONLY => Ok(WorkspacePlan {
            local_path: None,
            repo_path: None,
            choice: Some(CHOICE_VAULT_ONLY.to_string()),
            ensure_dir: None,
            git_init: false,
            next_endpoint: None,
            message:
                "Notes only. Agents will use this entity's vault folder and won't build in it."
                    .to_string(),
        }),

        // GitHub is a two-step answer: authorize first (that device-code flow
        // already exists and is not rebuilt here), then say which local
        // checkout to work in. Without a path the question stays open.
        DECISION_GITHUB if raw_path.is_none() => Ok(WorkspacePlan {
            local_path: None,
            repo_path: None,
            choice: None,
            ensure_dir: None,
            git_init: false,
            next_endpoint: Some(format!("/api/entities/{entity_id}/connections/sessions")),
            message:
                "Authorize GitHub, then come back and tell me which local checkout to work in."
                    .to_string(),
        }),

        CHOICE_LOCAL_FOLDER | CHOICE_REPO | DECISION_GITHUB => {
            let raw = raw_path.ok_or_else(|| WorkspaceError::PathRequired(choice.clone()))?;
            let path = resolve_decision_path(raw)?;
            let display = path.to_string_lossy().to_string();
            // `looks_like_repo` is evaluated by the caller after the directory
            // is ensured; here we only know what was asked for.
            Ok(WorkspacePlan {
                local_path: Some(display.clone()),
                repo_path: None,
                choice: Some(CHOICE_LOCAL_FOLDER.to_string()),
                ensure_dir: Some(path),
                git_init: false,
                next_endpoint: None,
                message: format!("Working in {display}."),
            })
        }

        DECISION_NEW_PROJECT => {
            let raw = raw_path
                .ok_or_else(|| WorkspaceError::PathRequired(DECISION_NEW_PROJECT.to_string()))?;
            let path = resolve_decision_path(raw)?;
            let display = path.to_string_lossy().to_string();
            Ok(WorkspacePlan {
                local_path: Some(display.clone()),
                repo_path: decision.git_init.then(|| display.clone()),
                choice: Some(if decision.git_init {
                    CHOICE_REPO.to_string()
                } else {
                    CHOICE_LOCAL_FOLDER.to_string()
                }),
                ensure_dir: Some(path),
                git_init: decision.git_init,
                next_endpoint: None,
                message: if decision.git_init {
                    format!("New git project at {display}.")
                } else {
                    format!("New project folder at {display}.")
                },
            })
        }

        other => Err(WorkspaceError::UnknownChoice(other.to_string())),
    }
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// Locate the `git` binary the same way [`crate::agent_sessions`]' CLI lookup
/// does — PATH first, then the handful of places a Mac actually puts things,
/// or where Git for Windows installs when it was left off PATH.
pub fn locate_git() -> Option<PathBuf> {
    let mut extra = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                let base = if var == "LOCALAPPDATA" {
                    base.join("Programs")
                } else {
                    base
                };
                extra.push(base.join("Git").join("cmd"));
            }
        }
    } else {
        extra.push(PathBuf::from("/usr/bin"));
    }
    consistent_world_common::platform::find_executable_with("git", &extra)
}

/// `git init` in `dir`, and nothing else.
///
/// No shell, ever: the binary is an absolute path we located, the arguments are
/// two fixed strings, and the operator's path reaches the process only as
/// `current_dir` — where it is a value, not something that can be parsed as
/// syntax. A missing git is reported, not swallowed: a repo the operator asked
/// for and did not get is worse than an error.
pub async fn git_init(dir: &Path) -> Result<(), WorkspaceError> {
    let git = locate_git().ok_or(WorkspaceError::GitMissing)?;
    if dir.join(".git").exists() {
        return Ok(());
    }
    let output = consistent_world_common::platform::command(&git)
        .arg("init")
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| WorkspaceError::GitFailed(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(WorkspaceError::GitFailed(if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EntitySetup;

    fn entity() -> Entity {
        Entity {
            id: "ent-1".to_string(),
            name: "Acme".to_string(),
            ..Entity::default()
        }
    }

    /// A stand-in for `~/.consistent-world`, so tests never touch the real one.
    fn fake_app_root(dir: &Path) -> Vec<PathBuf> {
        vec![dir.join("app-owned")]
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cw-workspace-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_entity_with_nothing_at_all_needs_a_decision() {
        let base = tempdir("unset");
        let status = workspace_status_with(&entity(), &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::Unset);
        assert!(status.needs_decision);
        assert!(!status.exists);
        assert!(status.path.is_none());
        assert!(status.choice.is_none());
        // The question is what the whole feature is for; it must be present.
        assert!(status.question.unwrap().contains("Acme"));
        assert_eq!(status.options.len(), 4);
    }

    #[test]
    fn a_configured_path_that_is_not_on_disk_is_still_unset_but_reported() {
        let base = tempdir("missing");
        let mut e = entity();
        e.local_path = Some(base.join("gone").to_string_lossy().to_string());
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::Unset);
        assert!(status.needs_decision);
        assert!(!status.exists);
        // "you pointed at a folder that isn't there" is a different fix from
        // "you never pointed anywhere", so the path survives.
        assert!(status.path.unwrap().ends_with("gone"));
    }

    #[test]
    fn an_operator_chosen_folder_on_disk_is_settled() {
        let base = tempdir("folder");
        let work = base.join("projects/foo");
        std::fs::create_dir_all(&work).unwrap();
        let mut e = entity();
        e.local_path = Some(work.to_string_lossy().to_string());
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::LocalFolder);
        assert!(status.exists);
        assert!(!status.needs_decision);
        assert!(!status.is_vault_fallback);
        assert!(status.question.is_none());
    }

    #[test]
    fn a_repo_path_and_a_git_checkout_both_read_as_a_repo() {
        let base = tempdir("repo");
        let repo = base.join("projects/bar");
        std::fs::create_dir_all(&repo).unwrap();
        let mut e = entity();
        e.setup = EntitySetup {
            repo_path: Some(repo.to_string_lossy().to_string()),
            ..EntitySetup::default()
        };
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::Repo);
        assert!(!status.needs_decision);

        // Named as a plain local path, but it has a .git — still a repo.
        let checkout = base.join("projects/baz");
        std::fs::create_dir_all(checkout.join(".git")).unwrap();
        let mut e2 = entity();
        e2.local_path = Some(checkout.to_string_lossy().to_string());
        assert_eq!(
            workspace_status_with(&e2, &fake_app_root(&base)).kind,
            WorkspaceKind::Repo
        );
    }

    #[test]
    fn the_vault_fallback_is_a_question_until_the_operator_answers_it() {
        let base = tempdir("vault");
        let vault_dir = base.join("app-owned/vault/entities/acme");
        std::fs::create_dir_all(&vault_dir).unwrap();
        let mut e = entity();
        e.local_path = Some(vault_dir.to_string_lossy().to_string());

        // Unanswered: an app-owned folder is not a decision.
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::VaultOnly);
        assert!(status.is_vault_fallback);
        assert!(status.needs_decision, "nobody chose this folder yet");

        // Answered: the same path, now a deliberate answer.
        e.setup.workspace_choice = Some(CHOICE_VAULT_ONLY.to_string());
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::VaultOnly);
        assert!(status.is_vault_fallback);
        assert!(!status.needs_decision, "vault_only is a real answer");
        assert!(status.question.is_none());
    }

    #[test]
    fn vault_only_settles_the_question_even_with_no_folder_yet() {
        let base = tempdir("vault-empty");
        let mut e = entity();
        e.setup.workspace_choice = Some(CHOICE_VAULT_ONLY.to_string());
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::VaultOnly);
        assert!(!status.needs_decision);
    }

    #[test]
    fn an_operator_path_wins_over_an_app_owned_one() {
        let base = tempdir("both");
        let vault_dir = base.join("app-owned/vault/entities/acme");
        let work = base.join("projects/real");
        std::fs::create_dir_all(&vault_dir).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let mut e = entity();
        e.local_path = Some(vault_dir.to_string_lossy().to_string());
        e.default_workspace_root = Some(work.to_string_lossy().to_string());
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::LocalFolder);
        assert!(status.path.unwrap().ends_with("real"));
    }

    #[test]
    fn a_website_url_is_not_a_workspace() {
        let base = tempdir("url");
        let mut e = entity();
        e.local_path = Some("https://acme.example".to_string());
        let status = workspace_status_with(&e, &fake_app_root(&base));
        assert_eq!(status.kind, WorkspaceKind::Unset);
        assert!(status.needs_decision);
    }

    // --- decision path handling ---

    #[test]
    fn a_tilde_path_is_expanded_to_the_home_directory() {
        let home = consistent_world_common::platform::home_dir().expect("home directory");
        let resolved = resolve_decision_path("~/projects/foo").unwrap();
        assert_eq!(resolved, home.join("projects/foo"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn an_absolute_path_is_kept_exactly_never_re_rooted_under_the_vault() {
        // The trap `normalize_local_path_input` sets: this must not happen.
        let resolved = resolve_decision_path("  /tmp/entity-work  ").unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/entity-work"));
    }

    #[test]
    fn a_relative_path_is_refused_rather_than_quietly_relocated() {
        assert_eq!(
            resolve_decision_path("projects/foo"),
            Err(WorkspaceError::NotAbsolute("projects/foo".to_string()))
        );
        assert!(matches!(
            resolve_decision_path("   "),
            Err(WorkspaceError::NotAbsolute(_))
        ));
    }

    #[test]
    fn a_missing_directory_is_a_plan_to_create_it_not_an_error() {
        let base = tempdir("plan-missing");
        let target = base.join("not-yet");
        let plan = plan_decision(
            "ent-1",
            &WorkspaceDecision {
                choice: "new_project".to_string(),
                path: Some(target.to_string_lossy().to_string()),
                git_init: false,
            },
        )
        .unwrap();
        assert_eq!(plan.ensure_dir.as_deref(), Some(target.as_path()));
        assert!(!plan.git_init);
        assert_eq!(plan.choice.as_deref(), Some(CHOICE_LOCAL_FOLDER));
        assert_eq!(plan.repo_path, None);
    }

    #[test]
    fn a_file_where_a_directory_should_be_is_refused() {
        let base = tempdir("plan-file");
        let file = base.join("notes.md");
        std::fs::write(&file, "hi").unwrap();
        assert!(matches!(
            resolve_decision_path(&file.to_string_lossy()),
            Err(WorkspaceError::NotADirectory(_))
        ));
    }

    #[test]
    fn new_project_with_git_init_records_a_repo() {
        let plan = plan_decision(
            "ent-1",
            &WorkspaceDecision {
                choice: "new_project".to_string(),
                path: Some("/tmp/new-thing".to_string()),
                git_init: true,
            },
        )
        .unwrap();
        assert!(plan.git_init);
        assert_eq!(plan.choice.as_deref(), Some(CHOICE_REPO));
        assert_eq!(plan.local_path.as_deref(), Some("/tmp/new-thing"));
        assert_eq!(plan.repo_path.as_deref(), Some("/tmp/new-thing"));
    }

    #[test]
    fn local_folder_sets_the_local_path_and_settles_the_question() {
        let plan = plan_decision(
            "ent-1",
            &WorkspaceDecision {
                choice: "local_folder".to_string(),
                path: Some("/tmp/existing".to_string()),
                git_init: false,
            },
        )
        .unwrap();
        assert_eq!(plan.local_path.as_deref(), Some("/tmp/existing"));
        assert_eq!(plan.choice.as_deref(), Some(CHOICE_LOCAL_FOLDER));
        assert!(plan.next_endpoint.is_none());
    }

    #[test]
    fn vault_only_touches_no_paths() {
        let plan = plan_decision(
            "ent-1",
            &WorkspaceDecision {
                choice: "vault_only".to_string(),
                ..WorkspaceDecision::default()
            },
        )
        .unwrap();
        assert_eq!(plan.choice.as_deref(), Some(CHOICE_VAULT_ONLY));
        assert!(plan.local_path.is_none());
        assert!(plan.repo_path.is_none());
        assert!(plan.ensure_dir.is_none());
    }

    #[test]
    fn github_without_a_path_hands_the_client_the_existing_connection_flow() {
        let plan = plan_decision(
            "ent-9",
            &WorkspaceDecision {
                choice: "github".to_string(),
                ..WorkspaceDecision::default()
            },
        )
        .unwrap();
        // OAuth is not reimplemented here; the client drives the flow that exists.
        assert_eq!(
            plan.next_endpoint.as_deref(),
            Some("/api/entities/ent-9/connections/sessions")
        );
        // Nothing is settled until a checkout is named, so the question stays open.
        assert!(plan.choice.is_none());
    }

    #[test]
    fn github_with_a_checkout_behaves_like_a_local_folder() {
        let plan = plan_decision(
            "ent-9",
            &WorkspaceDecision {
                choice: "github".to_string(),
                path: Some("/tmp/checkout".to_string()),
                git_init: false,
            },
        )
        .unwrap();
        assert_eq!(plan.local_path.as_deref(), Some("/tmp/checkout"));
        assert!(plan.next_endpoint.is_none());
    }

    #[test]
    fn an_unknown_choice_is_refused_with_the_valid_ones_named() {
        let err = plan_decision(
            "ent-1",
            &WorkspaceDecision {
                choice: "somewhere".to_string(),
                ..WorkspaceDecision::default()
            },
        )
        .unwrap_err();
        assert!(err.is_client_error());
        assert!(err.message().contains("local_folder"));
    }

    #[test]
    fn a_path_choice_without_a_path_says_so() {
        let err = plan_decision(
            "ent-1",
            &WorkspaceDecision {
                choice: "local_folder".to_string(),
                ..WorkspaceDecision::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            WorkspaceError::PathRequired("local_folder".to_string())
        );
    }

    #[tokio::test]
    async fn git_init_creates_a_repo_and_is_idempotent() {
        let Some(_) = locate_git() else {
            // A machine without git still runs the rest of the suite; the
            // missing-git path is asserted by its own error type below.
            return;
        };
        let dir = tempdir("git-init");
        git_init(&dir).await.unwrap();
        assert!(dir.join(".git").exists());
        // Running twice must not blow up an existing repo.
        git_init(&dir).await.unwrap();
    }

    #[test]
    fn a_missing_git_is_a_clear_message_not_a_silent_skip() {
        assert!(
            WorkspaceError::GitMissing
                .message()
                .contains("git is not installed")
        );
        assert!(!WorkspaceError::GitMissing.is_client_error());
    }
}
