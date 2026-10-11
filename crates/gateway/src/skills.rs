//! Skills — reusable procedures the agent loads on demand, in Claude's format.
//!
//! A skill is a folder holding a `SKILL.md`: YAML frontmatter with a `name`
//! (lowercase letters, digits, hyphens) and a `description` that says what it
//! does and *when to use it*, then Markdown instructions, plus any supporting
//! files the instructions point at. That is exactly the layout Claude Code
//! reads from `~/.claude/skills`, so a skill written here works there and vice
//! versa — importing one is a copy, not a conversion.
//!
//! Three scopes; when two share a name the most specific one wins:
//!
//! - **built-in** — the Consistent CEO playbook, compiled into the binary from
//!   `crates/gateway/skills/`. Read-only. "Customizing" one copies it into
//!   global, where it then shadows the original.
//! - **global** — the workspace's own skills, offered on every turn.
//! - **entity** — one venture's or project's skills, offered only while that
//!   entity is active.
//!
//! On disk, under [`SkillLibrary::default_root`] (`~/.consistent-world/skills`):
//!
//! ```text
//! builtin/<name>/SKILL.md                         # materialized for agent runs
//! workspaces/<ws>/global/<name>/SKILL.md
//! workspaces/<ws>/entities/<entity_id>/<name>/SKILL.md
//! workspaces/<ws>/state.json                      # which skills are switched off
//! ```
//!
//! Skills live on disk rather than in Surreal because they are file bundles
//! and because the `claude` CLI that runs agent sessions reads them by path.
//! Built-ins are only written out (under `builtin/`) so such a session has a
//! real file to open; the compiled copy stays authoritative.
//!
//! Progressive disclosure, the way Claude does it: only `name — description`
//! rides the system prompt ([`render_skills_index`]); the model calls the
//! `load_skill` tool for the body when a request matches. A dozen skills cost
//! a few hundred tokens instead of a dozen documents.
//!
//! The library is synchronous file I/O on small files. Async callers go
//! through `tokio::task::spawn_blocking`.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use consistent_world_common::types::ToolResult;
use consistent_world_tools::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Claude's limit for a skill `name`.
pub const MAX_NAME_CHARS: usize = 64;
/// Claude's limit for a skill `description`.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// A `SKILL.md` is instructions, not a dataset.
pub const MAX_SKILL_MD_BYTES: usize = 256 * 1024;
const MAX_SUPPORT_FILES: usize = 64;
const MAX_SUPPORT_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SKILL_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TREE_DEPTH: usize = 6;
/// Ceiling for the skills index in the system prompt, in characters (~2k
/// tokens). Past it, skills are listed by count only.
pub const MAX_INDEX_CHARS: usize = 8_000;
const MAX_INDEX_SKILLS: usize = 60;
/// Per-skill description budget inside the index.
const MAX_INDEX_DESCRIPTION_CHARS: usize = 500;
/// What `load_skill` hands back at most, so one skill can't flood a turn.
pub const MAX_LOADED_INSTRUCTION_CHARS: usize = 40_000;

const SKILL_FILE: &str = "SKILL.md";

struct Builtin {
    slug: &'static str,
    raw: &'static str,
}

/// The Consistent CEO playbook. Each is written against the gateway's own
/// chat tools, so following one acts inside the product instead of only
/// advising.
const BUILTINS: &[Builtin] = &[
    Builtin {
        slug: "ceo-daily-brief",
        raw: include_str!("../skills/ceo-daily-brief/SKILL.md"),
    },
    Builtin {
        slug: "venture-weekly-review",
        raw: include_str!("../skills/venture-weekly-review/SKILL.md"),
    },
    Builtin {
        slug: "decision-memo",
        raw: include_str!("../skills/decision-memo/SKILL.md"),
    },
    Builtin {
        slug: "investor-update",
        raw: include_str!("../skills/investor-update/SKILL.md"),
    },
    Builtin {
        slug: "mission-planner",
        raw: include_str!("../skills/mission-planner/SKILL.md"),
    },
    Builtin {
        slug: "meeting-to-actions",
        raw: include_str!("../skills/meeting-to-actions/SKILL.md"),
    },
    Builtin {
        slug: "delegation-brief",
        raw: include_str!("../skills/delegation-brief/SKILL.md"),
    },
    Builtin {
        slug: "customer-signal-synthesis",
        raw: include_str!("../skills/customer-signal-synthesis/SKILL.md"),
    },
    Builtin {
        slug: "skill-author",
        raw: include_str!("../skills/skill-author/SKILL.md"),
    },
];

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SkillError {
    /// The request is malformed (bad name, missing description, bad path).
    Invalid(String),
    NotFound,
    /// A skill with that name already exists in the target scope.
    Conflict(String),
    /// Built-ins can be switched off or copied, never edited or deleted.
    ReadOnly,
    Io(io::Error),
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::Invalid(m) | SkillError::Conflict(m) => f.write_str(m),
            SkillError::NotFound => f.write_str("skill not found"),
            SkillError::ReadOnly => f.write_str(
                "built-in skills can't be edited or deleted — customize it to make your own copy",
            ),
            SkillError::Io(e) => write!(f, "skill storage error: {e}"),
        }
    }
}

impl std::error::Error for SkillError {}

impl From<io::Error> for SkillError {
    fn from(e: io::Error) -> Self {
        SkillError::Io(e)
    }
}

// ─── Scope + ids ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillScopeKind {
    Builtin,
    Global,
    Entity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillScope {
    Builtin,
    Global,
    Entity(String),
}

impl SkillScope {
    pub fn kind(&self) -> SkillScopeKind {
        match self {
            SkillScope::Builtin => SkillScopeKind::Builtin,
            SkillScope::Global => SkillScopeKind::Global,
            SkillScope::Entity(_) => SkillScopeKind::Entity,
        }
    }

    pub fn entity_id(&self) -> Option<&str> {
        match self {
            SkillScope::Entity(e) => Some(e),
            _ => None,
        }
    }

    /// Stable public id: `builtin:<name>`, `global:<name>`,
    /// `entity:<entity_id>:<name>`.
    pub fn id_for(&self, slug: &str) -> String {
        match self {
            SkillScope::Builtin => format!("builtin:{slug}"),
            SkillScope::Global => format!("global:{slug}"),
            SkillScope::Entity(e) => format!("entity:{e}:{slug}"),
        }
    }

    /// Specificity: a higher rank shadows a lower one of the same name.
    fn rank(&self) -> u8 {
        match self {
            SkillScope::Builtin => 0,
            SkillScope::Global => 1,
            SkillScope::Entity(_) => 2,
        }
    }

    /// Build a scope from the wire pair `(scope, entityId)`.
    pub fn from_parts(scope: &str, entity_id: Option<&str>) -> Result<Self, SkillError> {
        match scope {
            "global" => Ok(SkillScope::Global),
            "entity" | "project" => {
                let e = entity_id
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .ok_or_else(|| SkillError::Invalid("a project skill needs an entityId".into()))?;
                validate_entity_id(e)?;
                Ok(SkillScope::Entity(e.to_string()))
            }
            "builtin" => Err(SkillError::ReadOnly),
            other => Err(SkillError::Invalid(format!(
                "unknown scope `{other}` (use `global` or `entity`)"
            ))),
        }
    }
}

/// Parse a public skill id back into its scope and name, rejecting anything
/// that could address a path outside the library.
pub fn parse_skill_id(id: &str) -> Result<(SkillScope, String), SkillError> {
    let invalid = || SkillError::Invalid(format!("unknown skill id `{id}`"));
    let (prefix, rest) = id.split_once(':').ok_or_else(invalid)?;
    let (scope, slug) = match prefix {
        "builtin" => (SkillScope::Builtin, rest),
        "global" => (SkillScope::Global, rest),
        "entity" => {
            let (entity, slug) = rest.rsplit_once(':').ok_or_else(invalid)?;
            validate_entity_id(entity)?;
            (SkillScope::Entity(entity.to_string()), slug)
        }
        _ => return Err(invalid()),
    };
    validate_slug(slug)?;
    Ok((scope, slug.to_string()))
}

/// Claude's rule for a skill name: lowercase letters, digits and hyphens, no
/// leading or trailing hyphen, at most 64 characters. It doubles as the folder
/// name, so it must also be a safe single path component — which it is.
pub fn validate_slug(slug: &str) -> Result<(), SkillError> {
    let ok = !slug.is_empty()
        && slug.len() <= MAX_NAME_CHARS
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(SkillError::Invalid(format!(
            "skill names use lowercase letters, digits and hyphens (max {MAX_NAME_CHARS} characters), e.g. `weekly-review` — got `{slug}`"
        )))
    }
}

fn validate_entity_id(entity_id: &str) -> Result<(), SkillError> {
    let ok = !entity_id.is_empty()
        && entity_id.len() <= 128
        && entity_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(SkillError::Invalid(format!("invalid entity id `{entity_id}`")))
    }
}

fn validate_description(description: &str) -> Result<(), SkillError> {
    let d = description.trim();
    if d.is_empty() {
        return Err(SkillError::Invalid(
            "a skill needs a description: what it does and when to use it — it's how the agent decides to load it".into(),
        ));
    }
    if d.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(SkillError::Invalid(format!(
            "description is over {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    Ok(())
}

/// Turn a human name ("Weekly Review!") into a skill name (`weekly-review`).
/// Non-ASCII letters are dropped, matching Claude's ASCII-only names; a name
/// with nothing left is rejected by [`validate_slug`].
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in input.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= MAX_NAME_CHARS {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Workspace ids come from the auth layer, not the user, but they still
/// become a directory name — map anything unusual to `_`.
fn path_safe(component: &str) -> String {
    let s: String = component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s == "_" {
        "default".to_string()
    } else {
        s
    }
}

// ─── Frontmatter ─────────────────────────────────────────────────────────────

/// Split a `SKILL.md` into its frontmatter (without the `---` fences) and its
/// body. A file without a leading `---` block is all body.
pub fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return (None, text);
    };
    if first.trim_end() != "---" {
        return (None, text);
    }
    let fm_start = first.len();
    let mut offset = fm_start;
    for line in lines {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            return (Some(&text[fm_start..offset]), &text[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// The top-level scalar keys of a YAML frontmatter block.
///
/// Deliberately small: it understands what `SKILL.md` files actually use —
/// `key: value`, quoted values, folded (`>`) and literal (`|`) block scalars,
/// and plain values that wrap onto indented lines. Nested mappings and lists
/// (`metadata:`, `allowed-tools:` as a list) are skipped, not misread. Pulling
/// in a YAML crate for two fields wasn't worth a dependency.
pub fn parse_frontmatter(fm: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = fm.lines().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let is_indented = |l: &str| l.starts_with(' ') || l.starts_with('\t');
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || is_indented(line) {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let rest = rest.trim();
        if matches!(rest, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            let folded = rest.starts_with('>');
            let mut parts: Vec<&str> = Vec::new();
            while i < lines.len() && (is_indented(lines[i]) || lines[i].trim().is_empty()) {
                parts.push(lines[i].trim());
                i += 1;
            }
            while parts.last().is_some_and(|p| p.is_empty()) {
                parts.pop();
            }
            let value = if folded {
                fold_lines(&parts)
            } else {
                parts.join("\n")
            };
            out.push((key, value));
        } else if rest.is_empty() {
            // A nested mapping or list: skip its block.
            while i < lines.len()
                && (is_indented(lines[i])
                    || lines[i].trim_start().starts_with("- ")
                    || lines[i].trim().is_empty())
            {
                i += 1;
            }
        } else {
            let mut value = unquote(rest);
            // A plain scalar may wrap onto indented continuation lines.
            while i < lines.len() && is_indented(lines[i]) && !lines[i].trim().is_empty() {
                value.push(' ');
                value.push_str(lines[i].trim());
                i += 1;
            }
            out.push((key, value));
        }
    }
    out
}

fn fold_lines(parts: &[&str]) -> String {
    let mut out = String::new();
    for p in parts {
        if p.is_empty() {
            out.push('\n');
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(p);
        }
    }
    out
}

fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        out
    } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].replace("''", "'")
    } else {
        v.to_string()
    }
}

fn frontmatter_value(fields: &[(String, String)], key: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A description as a single double-quoted YAML scalar — always valid YAML,
/// whatever the user typed.
fn yaml_quote(value: &str) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "\"{}\"",
        one_line.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// Replace (or append) one top-level key in a frontmatter block, keeping every
/// other key — `allowed-tools`, `license`, `metadata` — byte for byte.
fn replace_frontmatter_key(fm: &str, key: &str, quoted_value: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut skipping = false;
    for line in fm.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if skipping {
            if indented || line.trim().is_empty() {
                continue;
            }
            skipping = false;
        }
        if !indented
            && let Some((k, _)) = line.split_once(':')
            && k.trim() == key
        {
            out.push(format!("{key}: {quoted_value}"));
            replaced = true;
            skipping = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        out.push(format!("{key}: {quoted_value}"));
    }
    out.join("\n")
}

fn compose_skill_md(frontmatter: &str, body: &str) -> String {
    format!(
        "---\n{}\n---\n\n{}\n",
        frontmatter.trim_matches('\n'),
        body.trim()
    )
}

fn new_skill_md(slug: &str, description: &str, body: &str) -> String {
    compose_skill_md(
        &format!("name: {slug}\ndescription: {}", yaml_quote(description)),
        body,
    )
}

fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# "))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

fn titleize(slug: &str) -> String {
    slug.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

// ─── Public shapes ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    /// `builtin:<name>`, `global:<name>` or `entity:<entity_id>:<name>`.
    pub id: String,
    /// The folder name, which is the skill's identity.
    pub slug: String,
    /// The frontmatter `name` (normally equal to `slug`).
    pub name: String,
    /// A human title: the first `# ` heading, else the name title-cased.
    pub title: String,
    pub description: String,
    pub scope: SkillScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// Switched on by the user.
    pub enabled: bool,
    /// False for built-ins.
    pub editable: bool,
    /// Enabled and not shadowed by a more specific skill of the same name —
    /// i.e. what the agent actually sees.
    pub active: bool,
    /// Set when a more specific skill of the same name takes precedence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
    /// Supporting files, relative to the skill folder.
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Problems worth showing the user (missing description, name mismatch).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDetail {
    #[serde(flatten)]
    pub summary: SkillSummary,
    /// The instructions (everything after the frontmatter).
    pub body: String,
    /// The whole `SKILL.md`.
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct NewSkill {
    pub name: String,
    pub description: String,
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillPatch {
    pub description: Option<String>,
    pub body: Option<String>,
    /// A whole replacement `SKILL.md`; wins over `description` / `body`.
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadedFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedSkill {
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub imported: Vec<SkillSummary>,
    pub skipped: Vec<SkippedSkill>,
}

/// A skill found in a folder outside the library (e.g. `~/.claude/skills`),
/// offered for import.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSkill {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub path: String,
}

/// Which skills a listing covers.
#[derive(Debug, Clone)]
pub enum ListView {
    /// What one chat context sees: built-in + global + (optionally) one entity.
    Context { entity_id: Option<String> },
    /// Built-in + global + every entity in `entity_ids` (management view).
    All { entity_ids: Vec<String> },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SkillState {
    #[serde(default)]
    disabled: BTreeSet<String>,
}

/// One `SKILL.md` as read from a scope.
struct Loaded {
    scope: SkillScope,
    slug: String,
    raw: String,
    files: Vec<String>,
    updated_at: Option<String>,
}

// ─── The library ─────────────────────────────────────────────────────────────

pub struct SkillLibrary {
    root: PathBuf,
    /// Serializes mutations (create/update/delete/toggle/import) so two
    /// requests can't interleave writes to the same folder or `state.json`.
    write_lock: Mutex<()>,
}

impl SkillLibrary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            write_lock: Mutex::new(()),
        }
    }

    /// `~/.consistent-world/skills`, falling back to the temp dir only in a
    /// broken environment with no home directory.
    pub fn default_root() -> PathBuf {
        consistent_world_common::platform::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".consistent-world")
            .join("skills")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn workspace_dir(&self, workspace_id: &str) -> PathBuf {
        self.root.join("workspaces").join(path_safe(workspace_id))
    }

    fn scope_dir(&self, workspace_id: &str, scope: &SkillScope) -> PathBuf {
        match scope {
            SkillScope::Builtin => self.root.join("builtin"),
            SkillScope::Global => self.workspace_dir(workspace_id).join("global"),
            SkillScope::Entity(e) => self.workspace_dir(workspace_id).join("entities").join(e),
        }
    }

    fn skill_dir(&self, workspace_id: &str, scope: &SkillScope, slug: &str) -> PathBuf {
        self.scope_dir(workspace_id, scope).join(slug)
    }

    fn state_path(&self, workspace_id: &str) -> PathBuf {
        self.workspace_dir(workspace_id).join("state.json")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        // The guard protects files, not shared memory, so a lock poisoned by a
        // panicking writer is still safe to take.
        self.write_lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn load_state(&self, workspace_id: &str) -> SkillState {
        let path = self.state_path(workspace_id);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "skills: unreadable state.json, treating every skill as enabled");
                SkillState::default()
            }),
            Err(_) => SkillState::default(),
        }
    }

    fn save_state(&self, workspace_id: &str, state: &SkillState) -> io::Result<()> {
        let path = self.state_path(workspace_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_atomic(&path, &serde_json::to_vec_pretty(state).map_err(io::Error::other)?)
    }

    // ── Reading ──────────────────────────────────────────────────────────

    fn builtin_loaded(b: &Builtin) -> Loaded {
        Loaded {
            scope: SkillScope::Builtin,
            slug: b.slug.to_string(),
            raw: b.raw.to_string(),
            files: Vec::new(),
            updated_at: None,
        }
    }

    fn read_scope(&self, workspace_id: &str, scope: &SkillScope) -> Vec<Loaded> {
        if *scope == SkillScope::Builtin {
            return BUILTINS.iter().map(Self::builtin_loaded).collect();
        }
        let dir = self.scope_dir(workspace_id, scope);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if validate_slug(&name).is_err() {
                continue;
            }
            if let Some(loaded) = load_skill_dir(&entry.path(), scope.clone(), name) {
                out.push(loaded);
            }
        }
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        out
    }

    fn read_one(
        &self,
        workspace_id: &str,
        scope: &SkillScope,
        slug: &str,
    ) -> Result<Loaded, SkillError> {
        if *scope == SkillScope::Builtin {
            return BUILTINS
                .iter()
                .find(|b| b.slug == slug)
                .map(Self::builtin_loaded)
                .ok_or(SkillError::NotFound);
        }
        load_skill_dir(
            &self.skill_dir(workspace_id, scope, slug),
            scope.clone(),
            slug.to_string(),
        )
        .ok_or(SkillError::NotFound)
    }

    /// Entity ids that have a skills folder in this workspace. The API layer
    /// intersects this with what the caller may see.
    pub fn entity_ids_with_skills(&self, workspace_id: &str) -> Vec<String> {
        let dir = self.workspace_dir(workspace_id).join("entities");
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|id| validate_entity_id(id).is_ok())
            .collect();
        ids.sort();
        ids
    }

    /// Every skill in `view`, with `enabled`, `active` and `shadowed_by`
    /// computed. Order: built-in, global, then entities; by name within each.
    pub fn list(&self, workspace_id: &str, view: &ListView) -> Vec<SkillSummary> {
        let state = self.load_state(workspace_id);
        let mut loaded = self.read_scope(workspace_id, &SkillScope::Builtin);
        loaded.extend(self.read_scope(workspace_id, &SkillScope::Global));
        match view {
            ListView::Context { entity_id } => {
                if let Some(e) = entity_id
                    .as_deref()
                    .filter(|e| validate_entity_id(e).is_ok())
                {
                    loaded.extend(
                        self.read_scope(workspace_id, &SkillScope::Entity(e.to_string())),
                    );
                }
            }
            ListView::All { entity_ids } => {
                for e in entity_ids.iter().filter(|e| validate_entity_id(e).is_ok()) {
                    loaded.extend(self.read_scope(workspace_id, &SkillScope::Entity(e.clone())));
                }
            }
        }

        let mut summaries: Vec<SkillSummary> = loaded
            .iter()
            .map(|l| summarize(l, &state.disabled))
            .collect();

        // Shadowing: an enabled skill hides enabled, less specific skills of
        // the same name that it can see. A global skill shadows a built-in
        // everywhere; an entity skill shadows global/built-in only inside its
        // own entity, so in the management view it shadows nothing.
        let entity_in_context = match view {
            ListView::Context { entity_id } => entity_id.clone(),
            ListView::All { .. } => None,
        };
        let shadowers: Vec<(usize, &Loaded)> = loaded
            .iter()
            .enumerate()
            .filter(|(i, _)| summaries[*i].enabled)
            .collect();
        let mut shadowed: Vec<Option<String>> = vec![None; loaded.len()];
        for (i, l) in loaded.iter().enumerate() {
            if !summaries[i].enabled {
                continue;
            }
            let winner = shadowers
                .iter()
                .filter(|(j, other)| {
                    *j != i
                        && other.slug == l.slug
                        && other.scope.rank() > l.scope.rank()
                        && match other.scope.entity_id() {
                            Some(e) => entity_in_context.as_deref() == Some(e),
                            None => true,
                        }
                })
                .max_by_key(|(_, other)| other.scope.rank());
            if let Some((j, _)) = winner {
                shadowed[i] = Some(summaries[*j].id.clone());
            }
        }
        for (summary, by) in summaries.iter_mut().zip(shadowed) {
            if by.is_some() {
                summary.active = false;
                summary.shadowed_by = by;
            }
        }
        summaries
    }

    /// The skills an agent in this context actually has: active only, most
    /// specific first (this project, then workspace, then built-in).
    pub fn resolve_active(&self, workspace_id: &str, entity_id: Option<&str>) -> Vec<SkillSummary> {
        let mut skills: Vec<SkillSummary> = self
            .list(
                workspace_id,
                &ListView::Context {
                    entity_id: entity_id.map(str::to_string),
                },
            )
            .into_iter()
            .filter(|s| s.active)
            .collect();
        skills.sort_by(|a, b| {
            let rank = |s: &SkillSummary| match s.scope {
                SkillScopeKind::Entity => 0,
                SkillScopeKind::Global => 1,
                SkillScopeKind::Builtin => 2,
            };
            rank(a).cmp(&rank(b)).then_with(|| a.slug.cmp(&b.slug))
        });
        skills
    }

    pub fn get(&self, workspace_id: &str, id: &str) -> Result<SkillDetail, SkillError> {
        let (scope, slug) = parse_skill_id(id)?;
        let loaded = self.read_one(workspace_id, &scope, &slug)?;
        let view = ListView::Context {
            entity_id: scope.entity_id().map(str::to_string),
        };
        // Recompute active/shadowed in the skill's own context.
        let summary = self
            .list(workspace_id, &view)
            .into_iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| summarize(&loaded, &self.load_state(workspace_id).disabled));
        let (_, body) = split_frontmatter(&loaded.raw);
        Ok(SkillDetail {
            summary,
            body: body.trim().to_string(),
            raw: loaded.raw,
        })
    }

    /// The active skill an agent means by `name` (its folder name or its
    /// frontmatter name, case-insensitive), as `load_skill` sees it.
    pub fn find_active(
        &self,
        workspace_id: &str,
        entity_id: Option<&str>,
        name: &str,
    ) -> Option<SkillDetail> {
        let wanted = name.trim().trim_matches('`').to_ascii_lowercase();
        let hit = self
            .resolve_active(workspace_id, entity_id)
            .into_iter()
            .find(|s| s.slug == wanted || s.name.to_ascii_lowercase() == wanted)?;
        self.get(workspace_id, &hit.id).ok()
    }

    // ── Writing ──────────────────────────────────────────────────────────

    pub fn create(
        &self,
        workspace_id: &str,
        scope: &SkillScope,
        input: NewSkill,
    ) -> Result<SkillSummary, SkillError> {
        if *scope == SkillScope::Builtin {
            return Err(SkillError::ReadOnly);
        }
        let slug = slugify(&input.name);
        validate_slug(&slug)?;
        validate_description(&input.description)?;
        let raw = new_skill_md(&slug, input.description.trim(), &input.body);
        if raw.len() > MAX_SKILL_MD_BYTES {
            return Err(SkillError::Invalid(format!(
                "SKILL.md is over {} KB — keep instructions short and move reference material into supporting files",
                MAX_SKILL_MD_BYTES / 1024
            )));
        }
        {
            let _guard = self.lock();
            let dir = self.skill_dir(workspace_id, scope, &slug);
            if dir.exists() {
                return Err(SkillError::Conflict(format!(
                    "a skill named `{slug}` already exists here"
                )));
            }
            fs::create_dir_all(&dir)?;
            write_atomic(&dir.join(SKILL_FILE), raw.as_bytes())?;
            self.forget_disabled(workspace_id, &scope.id_for(&slug))?;
        }
        self.summary(workspace_id, scope, &slug)
    }

    pub fn update(
        &self,
        workspace_id: &str,
        id: &str,
        patch: SkillPatch,
    ) -> Result<SkillSummary, SkillError> {
        let (scope, slug) = parse_skill_id(id)?;
        if scope == SkillScope::Builtin {
            return Err(SkillError::ReadOnly);
        }
        {
            let _guard = self.lock();
            let path = self.skill_dir(workspace_id, &scope, &slug).join(SKILL_FILE);
            let current = fs::read_to_string(&path).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => SkillError::NotFound,
                _ => SkillError::Io(e),
            })?;

            let next = if let Some(raw) = patch.raw {
                let (fm, _) = split_frontmatter(&raw);
                let fields = fm.map(parse_frontmatter).unwrap_or_default();
                if let Some(name) = frontmatter_value(&fields, "name")
                    && slugify(&name) != slug
                {
                    return Err(SkillError::Invalid(format!(
                        "renaming isn't supported — keep `name: {slug}`, or create a new skill"
                    )));
                }
                validate_description(
                    &frontmatter_value(&fields, "description").unwrap_or_default(),
                )?;
                raw
            } else {
                let (fm, body) = split_frontmatter(&current);
                let mut fm = fm.unwrap_or("").to_string();
                if fm.trim().is_empty() {
                    fm = format!("name: {slug}");
                }
                if let Some(d) = patch.description.as_deref() {
                    validate_description(d)?;
                    fm = replace_frontmatter_key(&fm, "description", &yaml_quote(d.trim()));
                }
                let body = patch.body.as_deref().unwrap_or(body);
                compose_skill_md(&fm, body)
            };
            if next.len() > MAX_SKILL_MD_BYTES {
                return Err(SkillError::Invalid(format!(
                    "SKILL.md is over {} KB",
                    MAX_SKILL_MD_BYTES / 1024
                )));
            }
            write_atomic(&path, next.as_bytes())?;
        }
        self.summary(workspace_id, &scope, &slug)
    }

    pub fn delete(&self, workspace_id: &str, id: &str) -> Result<(), SkillError> {
        let (scope, slug) = parse_skill_id(id)?;
        if scope == SkillScope::Builtin {
            return Err(SkillError::ReadOnly);
        }
        let _guard = self.lock();
        let dir = self.skill_dir(workspace_id, &scope, &slug);
        if !dir.join(SKILL_FILE).exists() {
            return Err(SkillError::NotFound);
        }
        fs::remove_dir_all(&dir)?;
        self.forget_disabled(workspace_id, id)?;
        Ok(())
    }

    pub fn set_enabled(
        &self,
        workspace_id: &str,
        id: &str,
        enabled: bool,
    ) -> Result<SkillSummary, SkillError> {
        let (scope, slug) = parse_skill_id(id)?;
        // Confirms it exists before recording anything.
        self.read_one(workspace_id, &scope, &slug)?;
        {
            let _guard = self.lock();
            let mut state = self.load_state(workspace_id);
            if enabled {
                state.disabled.remove(id);
            } else {
                state.disabled.insert(id.to_string());
            }
            self.save_state(workspace_id, &state)?;
        }
        self.summary(workspace_id, &scope, &slug)
    }

    /// Copy any skill (typically a built-in being customized) into another
    /// scope under the same name, where it then shadows the original.
    pub fn copy_to(
        &self,
        workspace_id: &str,
        id: &str,
        target: &SkillScope,
    ) -> Result<SkillSummary, SkillError> {
        if *target == SkillScope::Builtin {
            return Err(SkillError::ReadOnly);
        }
        let (scope, slug) = parse_skill_id(id)?;
        if scope == *target {
            return Err(SkillError::Conflict(
                "that skill is already in this scope".into(),
            ));
        }
        let loaded = self.read_one(workspace_id, &scope, &slug)?;
        {
            let _guard = self.lock();
            let dest = self.skill_dir(workspace_id, target, &slug);
            if dest.exists() {
                return Err(SkillError::Conflict(format!(
                    "a skill named `{slug}` already exists there"
                )));
            }
            if scope == SkillScope::Builtin {
                fs::create_dir_all(&dest)?;
                write_atomic(&dest.join(SKILL_FILE), loaded.raw.as_bytes())?;
            } else {
                let src = self.skill_dir(workspace_id, &scope, &slug);
                if let Err(e) = copy_skill_tree_into(&src, &dest) {
                    let _ = fs::remove_dir_all(&dest);
                    return Err(e);
                }
            }
            self.forget_disabled(workspace_id, &target.id_for(&slug))?;
        }
        self.summary(workspace_id, target, &slug)
    }

    /// Import one skill folder (it holds a `SKILL.md`) or a folder of skill
    /// folders (`~/.claude/skills`) into `scope`. Symlinks inside a skill are
    /// skipped, never followed, and size limits apply per skill.
    pub fn import_dir(
        &self,
        workspace_id: &str,
        scope: &SkillScope,
        src: &Path,
        overwrite: bool,
    ) -> Result<ImportReport, SkillError> {
        if *scope == SkillScope::Builtin {
            return Err(SkillError::ReadOnly);
        }
        let src = fs::canonicalize(src)
            .map_err(|_| SkillError::Invalid(format!("no folder at {}", src.display())))?;
        if !src.is_dir() {
            return Err(SkillError::Invalid(format!(
                "{} is not a folder",
                src.display()
            )));
        }
        let candidates: Vec<PathBuf> = if src.join(SKILL_FILE).is_file() {
            vec![src.clone()]
        } else {
            let mut dirs: Vec<PathBuf> = fs::read_dir(&src)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join(SKILL_FILE).is_file())
                .collect();
            dirs.sort();
            dirs
        };
        if candidates.is_empty() {
            return Err(SkillError::Invalid(format!(
                "no SKILL.md in {} or its subfolders",
                src.display()
            )));
        }

        let mut imported_slugs = Vec::new();
        let mut skipped = Vec::new();
        {
            let _guard = self.lock();
            for candidate in candidates {
                let source = candidate.display().to_string();
                match self.import_one(workspace_id, scope, &candidate, overwrite) {
                    Ok(slug) => imported_slugs.push(slug),
                    Err(e) => skipped.push(SkippedSkill {
                        source,
                        reason: e.to_string(),
                    }),
                }
            }
        }
        let imported = imported_slugs
            .iter()
            .filter_map(|slug| self.summary(workspace_id, scope, slug).ok())
            .collect();
        Ok(ImportReport { imported, skipped })
    }

    /// Caller holds the write lock.
    fn import_one(
        &self,
        workspace_id: &str,
        scope: &SkillScope,
        candidate: &Path,
        overwrite: bool,
    ) -> Result<String, SkillError> {
        let raw = read_skill_md(&candidate.join(SKILL_FILE))
            .ok_or_else(|| SkillError::Invalid("SKILL.md is unreadable or too large".into()))?;
        let (fm, _) = split_frontmatter(&raw);
        let fields = fm.map(parse_frontmatter).unwrap_or_default();
        let folder = candidate
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let slug = slugify(&frontmatter_value(&fields, "name").unwrap_or(folder));
        validate_slug(&slug)?;
        validate_description(&frontmatter_value(&fields, "description").unwrap_or_default())?;

        let dest = self.skill_dir(workspace_id, scope, &slug);
        if dest.exists() && !overwrite {
            return Err(SkillError::Conflict(format!(
                "`{slug}` already exists — import with overwrite to replace it"
            )));
        }
        let staging = dest.with_file_name(format!(".{slug}.importing"));
        let _ = fs::remove_dir_all(&staging);
        if let Err(e) = copy_skill_tree_into(candidate, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        fs::rename(&staging, &dest)?;
        self.forget_disabled(workspace_id, &scope.id_for(&slug))?;
        Ok(slug)
    }

    /// Create a skill from an uploaded `SKILL.md` plus optional text files
    /// (the dashboard's "upload a skill" path, which works on any gateway).
    pub fn import_upload(
        &self,
        workspace_id: &str,
        scope: &SkillScope,
        skill_md: &str,
        files: &[UploadedFile],
        overwrite: bool,
    ) -> Result<SkillSummary, SkillError> {
        if *scope == SkillScope::Builtin {
            return Err(SkillError::ReadOnly);
        }
        if skill_md.len() > MAX_SKILL_MD_BYTES {
            return Err(SkillError::Invalid("SKILL.md is too large".into()));
        }
        if files.len() > MAX_SUPPORT_FILES {
            return Err(SkillError::Invalid(format!(
                "at most {MAX_SUPPORT_FILES} supporting files per skill"
            )));
        }
        let (fm, _) = split_frontmatter(skill_md);
        let fields = fm.map(parse_frontmatter).unwrap_or_default();
        let name = frontmatter_value(&fields, "name").ok_or_else(|| {
            SkillError::Invalid(
                "SKILL.md needs frontmatter with a `name` and a `description`".into(),
            )
        })?;
        let slug = slugify(&name);
        validate_slug(&slug)?;
        validate_description(&frontmatter_value(&fields, "description").unwrap_or_default())?;

        let mut total = skill_md.len() as u64;
        let mut cleaned: Vec<(PathBuf, &str)> = Vec::new();
        for f in files {
            let rel = safe_relative_path(&f.path)?;
            total += f.content.len() as u64;
            if f.content.len() as u64 > MAX_SUPPORT_FILE_BYTES || total > MAX_SKILL_TOTAL_BYTES {
                return Err(SkillError::Invalid(format!("{} is too large", f.path)));
            }
            cleaned.push((rel, f.content.as_str()));
        }

        {
            let _guard = self.lock();
            let dest = self.skill_dir(workspace_id, scope, &slug);
            if dest.exists() && !overwrite {
                return Err(SkillError::Conflict(format!(
                    "a skill named `{slug}` already exists here"
                )));
            }
            let staging = dest.with_file_name(format!(".{slug}.uploading"));
            let _ = fs::remove_dir_all(&staging);
            let write = || -> io::Result<()> {
                fs::create_dir_all(&staging)?;
                fs::write(staging.join(SKILL_FILE), skill_md)?;
                for (rel, content) in &cleaned {
                    let path = staging.join(rel);
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(path, content)?;
                }
                Ok(())
            };
            if let Err(e) = write() {
                let _ = fs::remove_dir_all(&staging);
                return Err(e.into());
            }
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
            }
            fs::rename(&staging, &dest)?;
            self.forget_disabled(workspace_id, &scope.id_for(&slug))?;
        }
        self.summary(workspace_id, scope, &slug)
    }

    /// Remove every skill of an entity (called when the entity is deleted).
    pub fn delete_entity_skills(
        &self,
        workspace_id: &str,
        entity_id: &str,
    ) -> Result<(), SkillError> {
        validate_entity_id(entity_id)?;
        let _guard = self.lock();
        let dir = self.scope_dir(workspace_id, &SkillScope::Entity(entity_id.to_string()));
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    /// Absolute `SKILL.md` paths for the active skills of a context, writing
    /// built-ins out first — what an agent session's `claude` process needs
    /// to read them.
    pub fn agent_skill_paths(
        &self,
        workspace_id: &str,
        entity_id: Option<&str>,
    ) -> Vec<(SkillSummary, PathBuf)> {
        self.resolve_active(workspace_id, entity_id)
            .into_iter()
            .filter_map(|s| {
                let path = match s.scope {
                    SkillScopeKind::Builtin => self.materialize_builtin(&s.slug).ok()?,
                    SkillScopeKind::Global => self
                        .skill_dir(workspace_id, &SkillScope::Global, &s.slug)
                        .join(SKILL_FILE),
                    SkillScopeKind::Entity => self
                        .skill_dir(
                            workspace_id,
                            &SkillScope::Entity(s.entity_id.clone()?),
                            &s.slug,
                        )
                        .join(SKILL_FILE),
                };
                Some((s, path))
            })
            .collect()
    }

    /// The directories an agent session must be allowed to read skills from.
    pub fn agent_readable_dirs(&self, workspace_id: &str) -> Vec<PathBuf> {
        vec![self.root.join("builtin"), self.workspace_dir(workspace_id)]
    }

    fn materialize_builtin(&self, slug: &str) -> io::Result<PathBuf> {
        let builtin = BUILTINS
            .iter()
            .find(|b| b.slug == slug)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such built-in"))?;
        let path = self.root.join("builtin").join(slug).join(SKILL_FILE);
        if fs::read_to_string(&path).ok().as_deref() != Some(builtin.raw) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_atomic(&path, builtin.raw.as_bytes())?;
        }
        Ok(path)
    }

    fn summary(
        &self,
        workspace_id: &str,
        scope: &SkillScope,
        slug: &str,
    ) -> Result<SkillSummary, SkillError> {
        self.get(workspace_id, &scope.id_for(slug)).map(|d| d.summary)
    }

    /// Drop a stale "disabled" flag so a newly created or imported skill
    /// starts switched on. Caller holds the write lock.
    fn forget_disabled(&self, workspace_id: &str, id: &str) -> io::Result<()> {
        let mut state = self.load_state(workspace_id);
        if state.disabled.remove(id) {
            self.save_state(workspace_id, &state)?;
        }
        Ok(())
    }
}

fn summarize(loaded: &Loaded, disabled: &BTreeSet<String>) -> SkillSummary {
    let (fm, body) = split_frontmatter(&loaded.raw);
    let fields = fm.map(parse_frontmatter).unwrap_or_default();
    let mut warnings = Vec::new();
    let name = frontmatter_value(&fields, "name").unwrap_or_else(|| loaded.slug.clone());
    if slugify(&name) != loaded.slug {
        warnings.push(format!(
            "frontmatter name `{name}` doesn't match the folder `{}`",
            loaded.slug
        ));
    }
    let description = frontmatter_value(&fields, "description").unwrap_or_default();
    if description.is_empty() {
        warnings.push(
            "no description — the agent can't tell when to use this skill, so it is never offered"
                .to_string(),
        );
    }
    let id = loaded.scope.id_for(&loaded.slug);
    let enabled = !disabled.contains(&id);
    SkillSummary {
        title: first_heading(body).unwrap_or_else(|| titleize(&loaded.slug)),
        id,
        slug: loaded.slug.clone(),
        name,
        description,
        scope: loaded.scope.kind(),
        entity_id: loaded.scope.entity_id().map(str::to_string),
        enabled,
        editable: loaded.scope != SkillScope::Builtin,
        active: enabled,
        shadowed_by: None,
        files: loaded.files.clone(),
        updated_at: loaded.updated_at.clone(),
        warnings,
    }
}

// ─── Filesystem helpers ──────────────────────────────────────────────────────

fn read_skill_md(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() as usize > MAX_SKILL_MD_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn load_skill_dir(dir: &Path, scope: SkillScope, slug: String) -> Option<Loaded> {
    let skill_md = dir.join(SKILL_FILE);
    let raw = read_skill_md(&skill_md)?;
    let updated_at = fs::metadata(&skill_md)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
    Some(Loaded {
        scope,
        slug,
        raw,
        files: list_support_files(dir),
        updated_at,
    })
}

fn skip_entry(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "__pycache__" | "target")
}

fn list_support_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if skip_entry(&name) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() && depth < MAX_TREE_DEPTH {
                stack.push((entry.path(), depth + 1));
            } else if meta.is_file()
                && let Ok(rel) = entry.path().strip_prefix(dir)
            {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if rel != SKILL_FILE {
                    out.push(rel);
                }
            }
            if out.len() >= MAX_SUPPORT_FILES {
                out.sort();
                return out;
            }
        }
    }
    out.sort();
    out
}

/// Copy a skill folder into `dest`: files and directories only — symlinks are
/// skipped so a crafted skill can't pull in a file from elsewhere on disk —
/// and within the size limits.
fn copy_skill_tree_into(src: &Path, dest: &Path) -> Result<(), SkillError> {
    let mut total: u64 = 0;
    let mut count: usize = 0;
    let mut stack: Vec<(PathBuf, PathBuf, usize)> =
        vec![(src.to_path_buf(), dest.to_path_buf(), 0)];
    fs::create_dir_all(dest)?;
    while let Some((from, to, depth)) = stack.pop() {
        for entry in fs::read_dir(&from)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if skip_entry(&name) {
                continue;
            }
            let meta = fs::symlink_metadata(entry.path())?;
            let target = to.join(&name);
            if meta.is_dir() {
                if depth >= MAX_TREE_DEPTH {
                    continue;
                }
                fs::create_dir_all(&target)?;
                stack.push((entry.path(), target, depth + 1));
            } else if meta.is_file() {
                count += 1;
                total += meta.len();
                let limit = if depth == 0 && name == SKILL_FILE {
                    MAX_SKILL_MD_BYTES as u64
                } else {
                    MAX_SUPPORT_FILE_BYTES
                };
                if meta.len() > limit {
                    return Err(SkillError::Invalid(format!("{name} is too large")));
                }
                if count > MAX_SUPPORT_FILES + 1 || total > MAX_SKILL_TOTAL_BYTES {
                    return Err(SkillError::Invalid(format!(
                        "skill is too large (max {MAX_SUPPORT_FILES} files, {} MB)",
                        MAX_SKILL_TOTAL_BYTES / (1024 * 1024)
                    )));
                }
                fs::copy(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

/// A relative path for an uploaded supporting file: plain components only,
/// no hidden segments, never `SKILL.md` itself.
fn safe_relative_path(path: &str) -> Result<PathBuf, SkillError> {
    let invalid = || SkillError::Invalid(format!("invalid file path `{path}`"));
    let normalized = path.replace('\\', "/");
    let p = Path::new(normalized.trim());
    if p.as_os_str().is_empty() || p.is_absolute() {
        return Err(invalid());
    }
    let mut out = PathBuf::new();
    let mut depth = 0;
    for c in p.components() {
        match c {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.starts_with('.') || s.is_empty() {
                    return Err(invalid());
                }
                out.push(seg);
                depth += 1;
            }
            _ => return Err(invalid()),
        }
    }
    if depth == 0
        || depth > MAX_TREE_DEPTH
        || out.to_string_lossy().eq_ignore_ascii_case(SKILL_FILE)
    {
        return Err(invalid());
    }
    Ok(out)
}

fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

/// Skills found in an outside folder (e.g. `~/.claude/skills`) — each child
/// folder holding a `SKILL.md`.
pub fn list_skill_folder(dir: &Path) -> Vec<ExternalSkill> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<ExternalSkill> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let raw = read_skill_md(&path.join(SKILL_FILE))?;
            let (fm, _) = split_frontmatter(&raw);
            let fields = fm.map(parse_frontmatter).unwrap_or_default();
            let folder = e.file_name().to_string_lossy().to_string();
            let name = frontmatter_value(&fields, "name").unwrap_or_else(|| folder.clone());
            Some(ExternalSkill {
                slug: slugify(&name),
                name,
                description: frontmatter_value(&fields, "description").unwrap_or_default(),
                path: path.to_string_lossy().to_string(),
            })
        })
        .filter(|s| validate_slug(&s.slug).is_ok())
        .collect();
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// `~/.claude/skills` — where Claude Code keeps the user's personal skills.
pub fn claude_skills_dir() -> Option<PathBuf> {
    consistent_world_common::platform::home_dir().map(|h| h.join(".claude").join("skills"))
}

// ─── Prompt rendering ────────────────────────────────────────────────────────

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (cut, truncated) = truncate_chars(&flat, max);
    if truncated { format!("{cut}…") } else { cut }
}

/// The skills block for a chat system prompt: one line per active skill,
/// bounded by [`MAX_INDEX_CHARS`]. `None` when there is nothing to offer.
pub fn render_skills_index(skills: &[SkillSummary]) -> Option<String> {
    let live: Vec<&SkillSummary> = skills
        .iter()
        .filter(|s| s.active && !s.description.trim().is_empty())
        .collect();
    if live.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Skills\n\
         You have skills: proven procedures for recurring work. Each line is `name` — what it does and when to use it. \
         When a request matches a skill, call `load_skill` with its name before you start, then follow the instructions it returns. \
         Load only what the request needs, and don't bring up skills the user didn't ask about.\n",
    );
    let mut used = out.chars().count();
    let mut shown = 0usize;
    for s in &live {
        if shown >= MAX_INDEX_SKILLS {
            break;
        }
        let tag = match s.scope {
            SkillScopeKind::Entity => " (this project)",
            _ => "",
        };
        let line = format!(
            "- `{}`{tag} — {}\n",
            s.slug,
            one_line(&s.description, MAX_INDEX_DESCRIPTION_CHARS)
        );
        let len = line.chars().count();
        if used + len > MAX_INDEX_CHARS {
            break;
        }
        out.push_str(&line);
        used += len;
        shown += 1;
    }
    if shown < live.len() {
        out.push_str(&format!(
            "({} more skills not listed — the prompt budget is full)\n",
            live.len() - shown
        ));
    }
    Some(out.trim_end().to_string())
}

/// The skills block for an agent session (a `claude` CLI run): it reads files
/// by path rather than calling `load_skill`.
pub fn render_agent_skills_index(skills: &[(SkillSummary, PathBuf)]) -> Option<String> {
    let live: Vec<&(SkillSummary, PathBuf)> = skills
        .iter()
        .filter(|(s, _)| !s.description.trim().is_empty())
        .collect();
    if live.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Skills\n\
         These files hold proven procedures for recurring work. If your task matches one, read its SKILL.md \
         (path below) before you start and follow it. Ignore the ones that don't apply.\n",
    );
    let mut used = out.chars().count();
    for (s, path) in live.iter().take(MAX_INDEX_SKILLS) {
        let line = format!(
            "- `{}` — {} — {}\n",
            s.slug,
            one_line(&s.description, MAX_INDEX_DESCRIPTION_CHARS),
            path.display()
        );
        let len = line.chars().count();
        if used + len > MAX_INDEX_CHARS {
            break;
        }
        out.push_str(&line);
        used += len;
    }
    Some(out.trim_end().to_string())
}

// ─── Chat tools ──────────────────────────────────────────────────────────────

fn tool_ok(value: Value) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: value.to_string(),
        is_error: false,
    }
}

fn tool_error(message: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: json!({ "ok": false, "error": message.into() }).to_string(),
        is_error: true,
    }
}

/// `load_skill`: hand the model one skill's instructions. Read-only.
pub struct LoadSkillTool {
    pub library: Arc<SkillLibrary>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
}

#[derive(Deserialize)]
struct LoadSkillArgs {
    name: String,
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &'static str {
        "load_skill"
    }

    fn description(&self) -> &'static str {
        "Load the full instructions of one of your skills (they are listed under \"Skills\" in your instructions). Call it before doing work a skill covers, then follow the instructions it returns."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The skill's name, e.g. \"venture-weekly-review\"." }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: LoadSkillArgs = match serde_json::from_value(arguments) {
            Ok(a) => a,
            Err(_) => return Ok(tool_error("load_skill needs a `name`")),
        };
        let library = self.library.clone();
        let workspace_id = self.workspace_id.to_string();
        let entity_id = self.entity_id.as_deref().map(str::to_string);
        let wanted = args.name.clone();
        let (found, available) = tokio::task::spawn_blocking(move || {
            let found = library.find_active(&workspace_id, entity_id.as_deref(), &wanted);
            let available: Vec<String> = if found.is_none() {
                library
                    .resolve_active(&workspace_id, entity_id.as_deref())
                    .into_iter()
                    .map(|s| s.slug)
                    .collect()
            } else {
                Vec::new()
            };
            (found, available)
        })
        .await?;
        let Some(skill) = found else {
            return Ok(tool_error(format!(
                "no active skill named `{}`; available: {}",
                args.name,
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )));
        };
        let (instructions, truncated) = truncate_chars(&skill.body, MAX_LOADED_INSTRUCTION_CHARS);
        Ok(tool_ok(json!({
            "ok": true,
            "name": skill.summary.slug,
            "title": skill.summary.title,
            "scope": skill.summary.scope,
            "instructions": instructions,
            "truncated": truncated,
            "files": skill.summary.files,
        })))
    }
}

/// `save_skill`: write a procedure the user approved as a skill.
pub struct SaveSkillTool {
    pub library: Arc<SkillLibrary>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
}

#[derive(Deserialize)]
struct SaveSkillArgs {
    name: String,
    description: String,
    instructions: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    replace: bool,
}

#[async_trait]
impl Tool for SaveSkillTool {
    fn name(&self) -> &'static str {
        "save_skill"
    }

    fn description(&self) -> &'static str {
        "Save a reusable procedure as a skill so it is used automatically on future requests. Only call this after the user has seen the draft and agreed. scope \"project\" saves it for the active project only; \"global\" makes it available for every venture."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "lowercase-hyphenated, e.g. \"vendor-invoice-review\"" },
                "description": { "type": "string", "description": "What the skill does and when to use it, in the words a user would type. Max 1024 characters." },
                "instructions": { "type": "string", "description": "Markdown instructions: a short why, numbered steps naming the tools to call, an output format, and rules." },
                "scope": { "type": "string", "enum": ["project", "global"], "description": "Defaults to project when a project is active, else global." },
                "replace": { "type": "boolean", "description": "Overwrite an existing skill of the same name. Only after the user confirmed." }
            },
            "required": ["name", "description", "instructions"]
        })
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: SaveSkillArgs = match serde_json::from_value(arguments) {
            Ok(a) => a,
            Err(_) => {
                return Ok(tool_error(
                    "save_skill needs `name`, `description` and `instructions`",
                ));
            }
        };
        let scope = match (args.scope.as_deref(), self.entity_id.as_deref()) {
            (Some("global"), _) | (None, None) => SkillScope::Global,
            (Some("project") | None, Some(e)) => SkillScope::Entity(e.to_string()),
            (Some("project"), None) => {
                return Ok(tool_error(
                    "no project is active — save it as a global skill, or ask the user to pick a project first",
                ));
            }
            (Some(other), _) => {
                return Ok(tool_error(format!(
                    "unknown scope `{other}` (use project or global)"
                )));
            }
        };
        let library = self.library.clone();
        let workspace_id = self.workspace_id.to_string();
        let slug = slugify(&args.name);
        let id = scope.id_for(&slug);
        let scope_for_write = scope.clone();
        let replace = args.replace;
        let result = tokio::task::spawn_blocking(move || {
            let created = library.create(
                &workspace_id,
                &scope_for_write,
                NewSkill {
                    name: args.name,
                    description: args.description.clone(),
                    body: args.instructions.clone(),
                },
            );
            match created {
                Err(SkillError::Conflict(_)) if replace => library.update(
                    &workspace_id,
                    &id,
                    SkillPatch {
                        description: Some(args.description),
                        body: Some(args.instructions),
                        raw: None,
                    },
                ),
                other => other,
            }
        })
        .await?;
        match result {
            Ok(summary) => Ok(tool_ok(json!({
                "ok": true,
                "kind": "skill_saved",
                "id": summary.id,
                "name": summary.slug,
                "title": summary.title,
                "scope": if matches!(scope, SkillScope::Global) { "global" } else { "project" },
            }))),
            Err(SkillError::Conflict(_)) => Ok(tool_error(format!(
                "a skill named `{slug}` already exists — ask the user whether to replace it, then call again with replace=true"
            ))),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> (tempfile::TempDir, SkillLibrary) {
        let dir = tempfile::tempdir().unwrap();
        let lib = SkillLibrary::new(dir.path());
        (dir, lib)
    }

    fn new(name: &str, description: &str) -> NewSkill {
        NewSkill {
            name: name.to_string(),
            description: description.to_string(),
            body: format!("# {name}\n\nDo the thing."),
        }
    }

    fn placeholder(slug: &str, active: bool, description: &str) -> SkillSummary {
        SkillSummary {
            id: SkillScope::Global.id_for(slug),
            slug: slug.to_string(),
            name: slug.to_string(),
            title: titleize(slug),
            description: description.to_string(),
            scope: SkillScopeKind::Global,
            entity_id: None,
            enabled: active,
            editable: true,
            active,
            shadowed_by: None,
            files: Vec::new(),
            updated_at: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn every_builtin_parses_with_a_matching_name_and_a_description() {
        for b in BUILTINS {
            let (fm, body) = split_frontmatter(b.raw);
            let fields = parse_frontmatter(fm.expect("builtin has frontmatter"));
            assert_eq!(frontmatter_value(&fields, "name").as_deref(), Some(b.slug));
            let d = frontmatter_value(&fields, "description").expect("description");
            assert!(
                d.chars().count() <= MAX_DESCRIPTION_CHARS,
                "{} description too long",
                b.slug
            );
            assert!(
                d.contains("Use when"),
                "{} description should say when to use it",
                b.slug
            );
            assert!(first_heading(body).is_some(), "{} has a title", b.slug);
            validate_slug(b.slug).unwrap();
        }
    }

    #[test]
    fn frontmatter_handles_quotes_folded_blocks_and_nested_maps() {
        let fm = "name: deploy-check\n\
                  description: >\n  Checks a deploy.\n  Use when shipping.\n\
                  allowed-tools:\n  - Bash\n  - Read\n\
                  metadata:\n  owner: ops\n\
                  license: \"MIT \\\"x\\\"\"\n\
                  note: 'it''s fine'\n";
        let fields = parse_frontmatter(fm);
        assert_eq!(
            frontmatter_value(&fields, "name").as_deref(),
            Some("deploy-check")
        );
        assert_eq!(
            frontmatter_value(&fields, "description").as_deref(),
            Some("Checks a deploy. Use when shipping.")
        );
        assert_eq!(
            frontmatter_value(&fields, "license").as_deref(),
            Some("MIT \"x\"")
        );
        assert_eq!(
            frontmatter_value(&fields, "note").as_deref(),
            Some("it's fine")
        );
        assert!(frontmatter_value(&fields, "allowed-tools").is_none());
        assert!(
            frontmatter_value(&fields, "owner").is_none(),
            "nested keys are not top-level"
        );
    }

    #[test]
    fn plain_scalars_may_wrap_and_files_without_frontmatter_are_all_body() {
        let fields = parse_frontmatter("description: first part\n  second part\nname: x\n");
        assert_eq!(
            frontmatter_value(&fields, "description").as_deref(),
            Some("first part second part")
        );
        let (fm, body) = split_frontmatter("# Just markdown\n");
        assert!(fm.is_none());
        assert_eq!(body, "# Just markdown\n");
    }

    #[test]
    fn slugs_and_ids_reject_anything_path_like() {
        assert_eq!(slugify("Weekly Review!"), "weekly-review");
        assert_eq!(slugify("  --A__b  "), "a-b");
        assert_eq!(slugify("مراجعة"), "");
        assert!(validate_slug("").is_err());
        assert!(validate_slug("../x").is_err());
        assert!(validate_slug("Upper").is_err());
        assert!(parse_skill_id("entity:../../etc:passwd").is_err());
        assert!(parse_skill_id("entity:ent/x:ok").is_err());
        assert!(parse_skill_id("global:../x").is_err());
        assert!(parse_skill_id("weird:x").is_err());
        let (scope, slug) = parse_skill_id("entity:ent_123:weekly-review").unwrap();
        assert_eq!(scope, SkillScope::Entity("ent_123".into()));
        assert_eq!(slug, "weekly-review");
    }

    #[test]
    fn create_get_update_delete_round_trip_preserves_extra_frontmatter() {
        let (_d, lib) = lib();
        let s = lib
            .create(
                "ws1",
                &SkillScope::Global,
                new("Pricing Rules", "Price deals. Use when quoting."),
            )
            .unwrap();
        assert_eq!(s.id, "global:pricing-rules");
        assert!(s.active && s.enabled && s.editable);

        // A hand-edited extra key must survive a description edit.
        let path = lib
            .skill_dir("ws1", &SkillScope::Global, "pricing-rules")
            .join(SKILL_FILE);
        let raw = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            raw.replacen("---\n\n", "allowed-tools: Read\n---\n\n", 1),
        )
        .unwrap();

        let updated = lib
            .update(
                "ws1",
                &s.id,
                SkillPatch {
                    description: Some("Price deals \"carefully\". Use when quoting.".into()),
                    body: None,
                    raw: None,
                },
            )
            .unwrap();
        assert_eq!(
            updated.description,
            "Price deals \"carefully\". Use when quoting."
        );
        let detail = lib.get("ws1", &s.id).unwrap();
        assert!(detail.raw.contains("allowed-tools: Read"));
        assert!(detail.body.contains("Do the thing."));

        // Renaming through a raw edit is refused.
        let renamed = detail.raw.replace("name: pricing-rules", "name: other");
        assert!(matches!(
            lib.update(
                "ws1",
                &s.id,
                SkillPatch {
                    raw: Some(renamed),
                    ..Default::default()
                }
            ),
            Err(SkillError::Invalid(_))
        ));

        assert!(matches!(
            lib.create(
                "ws1",
                &SkillScope::Global,
                new("pricing rules", "Dup. Use when x.")
            ),
            Err(SkillError::Conflict(_))
        ));
        lib.delete("ws1", &s.id).unwrap();
        assert!(matches!(lib.get("ws1", &s.id), Err(SkillError::NotFound)));
    }

    #[test]
    fn builtins_are_read_only_but_can_be_switched_off_and_customized() {
        let (_d, lib) = lib();
        let id = "builtin:decision-memo";
        assert!(matches!(lib.delete("ws", id), Err(SkillError::ReadOnly)));
        assert!(matches!(
            lib.update(
                "ws",
                id,
                SkillPatch {
                    body: Some("x".into()),
                    ..Default::default()
                }
            ),
            Err(SkillError::ReadOnly)
        ));

        let off = lib.set_enabled("ws", id, false).unwrap();
        assert!(!off.enabled && !off.active);
        assert!(!lib.resolve_active("ws", None).iter().any(|s| s.id == id));
        lib.set_enabled("ws", id, true).unwrap();

        let copy = lib.copy_to("ws", id, &SkillScope::Global).unwrap();
        assert_eq!(copy.id, "global:decision-memo");
        let builtin = lib.get("ws", id).unwrap().summary;
        assert!(!builtin.active, "the customized copy shadows the built-in");
        assert_eq!(builtin.shadowed_by.as_deref(), Some("global:decision-memo"));
    }

    #[test]
    fn entity_skills_shadow_only_inside_their_entity_and_workspaces_are_isolated() {
        let (_d, lib) = lib();
        lib.create(
            "ws",
            &SkillScope::Global,
            new("release", "Global release. Use when shipping."),
        )
        .unwrap();
        lib.create(
            "ws",
            &SkillScope::Entity("ent_a".into()),
            new("release", "Acme release. Use when shipping Acme."),
        )
        .unwrap();

        let in_a = lib.resolve_active("ws", Some("ent_a"));
        let release_a: Vec<_> = in_a.iter().filter(|s| s.slug == "release").collect();
        assert_eq!(release_a.len(), 1);
        assert_eq!(release_a[0].id, "entity:ent_a:release");
        assert_eq!(
            in_a[0].scope,
            SkillScopeKind::Entity,
            "project skills come first"
        );

        let in_b = lib.resolve_active("ws", Some("ent_b"));
        assert!(in_b.iter().any(|s| s.id == "global:release"));

        let all = lib.list(
            "ws",
            &ListView::All {
                entity_ids: vec!["ent_a".into()],
            },
        );
        let global = all.iter().find(|s| s.id == "global:release").unwrap();
        assert!(global.active, "an entity skill doesn't shadow globally");

        // Disabling the entity copy falls back to the global one.
        lib.set_enabled("ws", "entity:ent_a:release", false)
            .unwrap();
        assert!(
            lib.resolve_active("ws", Some("ent_a"))
                .iter()
                .any(|s| s.id == "global:release")
        );

        assert!(
            !lib.resolve_active("other-ws", None)
                .iter()
                .any(|s| s.slug == "release"),
            "another workspace never sees these skills"
        );
        assert_eq!(lib.entity_ids_with_skills("ws"), vec!["ent_a".to_string()]);
    }

    #[test]
    fn find_active_matches_folder_or_frontmatter_name() {
        let (_d, lib) = lib();
        assert!(lib.find_active("ws", None, "ceo-daily-brief").is_some());
        assert!(lib.find_active("ws", None, "`CEO-Daily-Brief`").is_some());
        assert!(lib.find_active("ws", None, "nope").is_none());
    }

    #[test]
    fn import_copies_files_skips_symlinks_and_respects_overwrite() {
        let (_d, lib) = lib();
        let src = tempfile::tempdir().unwrap();
        let skill = src.path().join("Vendor_Review");
        fs::create_dir_all(skill.join("templates")).unwrap();
        fs::write(
            skill.join(SKILL_FILE),
            "---\nname: vendor-review\ndescription: Review vendors. Use when onboarding a vendor.\n---\n\n# Vendor review\n",
        )
        .unwrap();
        fs::write(skill.join("templates/checklist.md"), "- [ ] W-9").unwrap();
        fs::create_dir_all(skill.join(".git")).unwrap();
        fs::write(skill.join(".git/config"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", skill.join("hosts")).unwrap();
        // A folder without a description is skipped, not imported.
        let bad = src.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(SKILL_FILE), "---\nname: bad\n---\nbody").unwrap();

        let report = lib
            .import_dir("ws", &SkillScope::Global, src.path(), false)
            .unwrap();
        assert_eq!(report.imported.len(), 1);
        assert_eq!(report.imported[0].id, "global:vendor-review");
        assert_eq!(
            report.imported[0].files,
            vec!["templates/checklist.md".to_string()]
        );
        assert_eq!(report.skipped.len(), 1);
        let dest = lib.skill_dir("ws", &SkillScope::Global, "vendor-review");
        assert!(!dest.join("hosts").exists(), "symlinks are never copied");
        assert!(!dest.join(".git").exists());

        let again = lib
            .import_dir("ws", &SkillScope::Global, &skill, false)
            .unwrap();
        assert!(again.imported.is_empty() && again.skipped.len() == 1);
        let forced = lib
            .import_dir("ws", &SkillScope::Global, &skill, true)
            .unwrap();
        assert_eq!(forced.imported.len(), 1);
    }

    #[test]
    fn uploads_reject_escaping_paths() {
        let (_d, lib) = lib();
        let md = "---\nname: up\ndescription: Uploaded. Use when testing.\n---\nbody";
        for bad in ["../x.md", "/abs.md", ".hidden/x", "a/../../b", "SKILL.md", ""] {
            let r = lib.import_upload(
                "ws",
                &SkillScope::Global,
                md,
                &[UploadedFile {
                    path: bad.into(),
                    content: "x".into(),
                }],
                false,
            );
            assert!(
                matches!(r, Err(SkillError::Invalid(_))),
                "{bad:?} should be rejected"
            );
        }
        let ok = lib
            .import_upload(
                "ws",
                &SkillScope::Global,
                md,
                &[UploadedFile {
                    path: "ref/notes.md".into(),
                    content: "x".into(),
                }],
                false,
            )
            .unwrap();
        assert_eq!(ok.files, vec!["ref/notes.md".to_string()]);
    }

    #[test]
    fn index_is_bounded_and_skips_inactive_or_undescribed_skills() {
        let skills = vec![
            placeholder("one", true, "Does one. Use when one."),
            placeholder("off", false, "Disabled. Use when never."),
            placeholder("blank", true, ""),
        ];
        let block = render_skills_index(&skills).unwrap();
        assert!(block.starts_with("## Skills"));
        assert!(block.contains("`one`"));
        assert!(!block.contains("`off`") && !block.contains("`blank`"));

        let many: Vec<SkillSummary> = (0..200)
            .map(|i| placeholder(&format!("skill-{i}"), true, &"word ".repeat(150)))
            .collect();
        let block = render_skills_index(&many).unwrap();
        assert!(block.chars().count() <= MAX_INDEX_CHARS + 120);
        assert!(block.contains("more skills not listed"));
        assert!(render_skills_index(&[]).is_none());
    }

    #[test]
    fn agent_paths_point_at_real_files_including_builtins() {
        let (_d, lib) = lib();
        lib.create(
            "ws",
            &SkillScope::Global,
            new("ops", "Ops. Use when operating."),
        )
        .unwrap();
        let paths = lib.agent_skill_paths("ws", None);
        assert!(paths.iter().all(|(_, p)| p.is_file()), "{paths:?}");
        assert!(paths.iter().any(|(s, _)| s.id == "builtin:ceo-daily-brief"));
        let block = render_agent_skills_index(&paths).unwrap();
        assert!(block.contains("SKILL.md"));
    }

    #[tokio::test]
    async fn tools_load_and_save_skills_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        let library = Arc::new(SkillLibrary::new(dir.path()));
        let save = SaveSkillTool {
            library: library.clone(),
            workspace_id: Arc::from("ws"),
            entity_id: Some(Arc::from("ent_a")),
        };
        let out = save
            .execute(json!({
                "name": "Vendor Review",
                "description": "Review a vendor. Use when onboarding a vendor.",
                "instructions": "1. Check the W-9."
            }))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.output);
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(v["id"], "entity:ent_a:vendor-review");
        assert_eq!(v["scope"], "project");

        let dup = save
            .execute(json!({"name":"vendor-review","description":"x. Use when y.","instructions":"z"}))
            .await
            .unwrap();
        assert!(dup.is_error && dup.output.contains("replace=true"));

        let load = LoadSkillTool {
            library: library.clone(),
            workspace_id: Arc::from("ws"),
            entity_id: Some(Arc::from("ent_a")),
        };
        let out = load
            .execute(json!({"name": "vendor-review"}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out.output).unwrap();
        assert!(
            v["instructions"]
                .as_str()
                .unwrap()
                .contains("Check the W-9")
        );

        // Another project can't load it; the error lists what is available.
        let other = LoadSkillTool {
            library,
            workspace_id: Arc::from("ws"),
            entity_id: Some(Arc::from("ent_b")),
        };
        let out = other
            .execute(json!({"name": "vendor-review"}))
            .await
            .unwrap();
        assert!(out.is_error && out.output.contains("ceo-daily-brief"));

        let no_project = SaveSkillTool {
            library: Arc::new(SkillLibrary::new(dir.path())),
            workspace_id: Arc::from("ws"),
            entity_id: None,
        };
        let out = no_project
            .execute(json!({"name":"x","description":"x. Use when y.","instructions":"z","scope":"project"}))
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
