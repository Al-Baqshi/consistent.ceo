//! Deterministic capture routing: given a raw capture and the workspace's
//! visible entities, derive a suggested `entity_id` with a confidence score and
//! a human-readable reason. No LLM — this uses only structured signals already
//! present on the capture and the entities (explicit `entity_id`, GitHub repo
//! URLs, domains/URLs in the text, repo paths, and the selected-entity context).
//!
//! The suggestion is advisory. Callers decide whether to apply it; captures are
//! never auto-promoted here.

use serde::Serialize;

use crate::state::{Entity, RawCapture};

/// Minimum confidence at which a suggestion is eligible for `auto_apply`.
pub const AUTO_APPLY_THRESHOLD: f64 = 0.8;

/// A routing suggestion: which entity a capture likely belongs to, and why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteSuggestion {
    /// Suggested entity, or `None` when no signal matched.
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    /// 0.0–1.0. 1.0 means the capture is already attached.
    pub confidence: f64,
    /// Short explanation the UI can show to justify the suggestion.
    pub reason: String,
    /// The concrete signals that matched (e.g. `github:acme/widgets`).
    pub signals: Vec<String>,
}

impl RouteSuggestion {
    fn none() -> Self {
        RouteSuggestion {
            entity_id: None,
            entity_name: None,
            confidence: 0.0,
            reason: "No routing signal found. Attach manually or leave in the inbox.".to_string(),
            signals: Vec::new(),
        }
    }
}

// Structured signals extracted from capture text: GitHub repo slugs and domains.
#[derive(Default)]
struct TextSignals {
    domains: Vec<String>,
    github_repos: Vec<String>,
}

/// Compute a routing suggestion for `capture` against the given visible
/// `entities`. `selected_entity_id` is the entity currently focused in the UI,
/// used only as a low-confidence fallback.
pub fn suggest_route(
    capture: &RawCapture,
    entities: &[Entity],
    selected_entity_id: Option<&str>,
) -> RouteSuggestion {
    // 1. Already attached — highest confidence.
    if let Some(eid) = capture.entity_id.as_deref().filter(|v| !v.is_empty())
        && let Some(entity) = entities.iter().find(|e| e.id == eid)
    {
        return RouteSuggestion {
            entity_id: Some(entity.id.clone()),
            entity_name: Some(entity.name.clone()),
            confidence: 1.0,
            reason: "Capture is already attached to this entity.".to_string(),
            signals: vec!["capture.entity_id".to_string()],
        };
    }

    // 2. Content signals: GitHub repos and domains in the capture text/ref.
    let mut haystack = capture.raw_text.clone();
    if let Some(ref sref) = capture.source_ref {
        haystack.push(' ');
        haystack.push_str(sref);
    }
    if let Some(summary) = capture.summary.as_deref() {
        haystack.push(' ');
        haystack.push_str(summary);
    }
    // Structured hints supplied by external tools (repo_url, url, cwd) are stored
    // in metadata; fold them into the routing haystack so they contribute signals.
    for hint in metadata_hints(&capture.metadata) {
        haystack.push(' ');
        haystack.push_str(&hint);
    }
    let signals = extract_signals(&haystack);

    let mut best: Option<(f64, &Entity, String)> = None;
    for entity in entities {
        let (score, matched) = score_entity(entity, &signals);
        if score <= 0.0 {
            continue;
        }
        let replace = match &best {
            Some((best_score, _, _)) => score > *best_score,
            None => true,
        };
        if replace {
            best = Some((score, entity, matched));
        }
    }

    if let Some((score, entity, matched)) = best {
        let reason = match matched.split_once(':') {
            Some(("github", repo)) => {
                format!("Text references GitHub repo `{repo}` for this entity.")
            }
            Some(("domain", dom)) => format!("Text references `{dom}`, a domain for this entity."),
            Some(("repo", name)) => format!("Text references repo path `{name}` for this entity."),
            _ => "Content matches this entity.".to_string(),
        };
        return RouteSuggestion {
            entity_id: Some(entity.id.clone()),
            entity_name: Some(entity.name.clone()),
            confidence: score,
            reason,
            signals: vec![matched],
        };
    }

    // 3. Selected-entity fallback — weak context only.
    if let Some(sel) = selected_entity_id.filter(|v| !v.is_empty())
        && let Some(entity) = entities.iter().find(|e| e.id == sel)
    {
        return RouteSuggestion {
            entity_id: Some(entity.id.clone()),
            entity_name: Some(entity.name.clone()),
            confidence: 0.4,
            reason: "Using the currently selected entity as context.".to_string(),
            signals: vec!["selected_entity".to_string()],
        };
    }

    RouteSuggestion::none()
}

/// Score how well an entity matches the extracted text signals. Returns the
/// confidence and the matched-signal tag (e.g. `github:acme/widgets`).
fn score_entity(entity: &Entity, signals: &TextSignals) -> (f64, String) {
    let repo_fields = entity_repo_fields(entity);

    // GitHub repo URL is the strongest structured signal.
    for repo in &signals.github_repos {
        for field in &repo_fields {
            if field.contains(repo.as_str()) {
                return (0.9, format!("github:{repo}"));
            }
        }
        // Fall back to matching the repo name against a repo-path basename.
        if let Some((_, name)) = repo.split_once('/') {
            for field in &repo_fields {
                if path_basename(field) == name {
                    return (0.7, format!("github:{repo}"));
                }
            }
        }
    }

    // Domains: exact host match or subdomain of an entity domain.
    let entity_domains = entity_domains(entity);
    for dom in &signals.domains {
        for ed in &entity_domains {
            if dom == ed || dom.ends_with(&format!(".{ed}")) {
                return (0.85, format!("domain:{dom}"));
            }
        }
    }

    // Repo path / working-root basename mentioned verbatim in the text.
    for field in &repo_fields {
        let base = path_basename(field);
        if base.len() >= 3 && signals.contains_repo_basename(base) {
            return (0.65, format!("repo:{base}"));
        }
    }

    (0.0, String::new())
}

impl TextSignals {
    fn contains_repo_basename(&self, base: &str) -> bool {
        self.github_repos
            .iter()
            .any(|r| r.split_once('/').map(|(_, n)| n) == Some(base))
    }
}

/// Repo/path fields for an entity, lowercased, for substring matching.
fn entity_repo_fields(entity: &Entity) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: Option<&str>| {
        if let Some(v) = v {
            let v = v.trim().to_ascii_lowercase();
            if !v.is_empty() {
                out.push(v);
            }
        }
    };
    push(entity.setup.repo_path.as_deref());
    push(entity.local_path.as_deref());
    push(entity.path.as_deref());
    push(entity.default_workspace_root.as_deref());
    for res in &entity.resources {
        push(Some(res.root.as_str()));
    }
    out
}

/// Domains associated with an entity, normalized (lowercased, `www.` stripped).
fn entity_domains(entity: &Entity) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: &str| {
        if let Some(host) = normalize_host(v) {
            out.push(host);
        }
    };
    if let Some(d) = entity.setup.primary_domain.as_deref() {
        push(d);
    }
    for d in &entity.domains {
        push(d);
    }
    out
}

/// Metadata keys that external tools use to carry routable location hints.
const METADATA_HINT_KEYS: &[&str] = &[
    "repoUrl",
    "repo_url",
    "repo",
    "url",
    "sourceUrl",
    "source_url",
    "cwd",
];

/// Pull the string values of known location-hint keys from a capture's metadata
/// object. Only a fixed allowlist is scanned, so arbitrary metadata cannot inject
/// spurious routing signals.
fn metadata_hints(metadata: &serde_json::Value) -> Vec<String> {
    let Some(obj) = metadata.as_object() else {
        return Vec::new();
    };
    METADATA_HINT_KEYS
        .iter()
        .filter_map(|key| obj.get(*key).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .collect()
}

// Tokenize the haystack and pull out GitHub repo slugs and bare/URL domains.
fn extract_signals(text: &str) -> TextSignals {
    let mut signals = TextSignals::default();
    for raw in text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
            )
    }) {
        let token = raw.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ':' | '|'));
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if let Some(idx) = lower.find("github.com") {
            let rest = &lower[idx + "github.com".len()..];
            let rest = rest.trim_start_matches([':', '/']);
            let mut parts = rest.split('/').filter(|s| !s.is_empty());
            if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
                let repo = repo.trim_end_matches(".git");
                let slug = format!("{owner}/{repo}");
                if !signals.github_repos.contains(&slug) {
                    signals.github_repos.push(slug);
                }
            }
            continue;
        }
        if let Some(host) = normalize_host(&lower)
            && !signals.domains.contains(&host)
        {
            signals.domains.push(host);
        }
    }
    signals
}

/// Extract and normalize a host from a URL or bare domain token. Returns `None`
/// when the token does not look like a host.
fn normalize_host(token: &str) -> Option<String> {
    let mut host = token.trim().to_ascii_lowercase();
    if let Some(pos) = host.find("://") {
        host = host[pos + 3..].to_string();
    }
    // Drop any path, query, port, or userinfo.
    host = host.split(['/', '?', '#']).next().unwrap_or("").to_string();
    if let Some(at) = host.rfind('@') {
        host = host[at + 1..].to_string();
    }
    if let Some(colon) = host.find(':') {
        host = host[..colon].to_string();
    }
    host = host.trim_matches('.').to_string();
    host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    if !is_domain_like(&host) {
        return None;
    }
    Some(host)
}

/// A minimal "looks like a domain" check: at least one dot, a TLD of 2+ ASCII
/// letters, and only domain-safe characters.
fn is_domain_like(host: &str) -> bool {
    if !host.contains('.') {
        return false;
    }
    if host
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    {
        return false;
    }
    match host.rsplit('.').next() {
        Some(tld) => tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()),
        None => false,
    }
}

fn path_basename(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(raw_text: &str, entity_id: Option<&str>) -> RawCapture {
        RawCapture {
            id: "cap-1".to_string(),
            workspace_id: "default".to_string(),
            entity_id: entity_id.map(|s| s.to_string()),
            source: "chat".to_string(),
            source_ref: None,
            session_id: None,
            provider: None,
            raw_text: raw_text.to_string(),
            summary: None,
            status: crate::state::RawCaptureStatus::Raw,
            promoted_task_id: None,
            metadata: serde_json::Value::Null,
            created_at: "2026-07-03T00:00:00Z".to_string(),
            updated_at: "2026-07-03T00:00:00Z".to_string(),
        }
    }

    fn entity_with_repo(id: &str, name: &str, repo: &str) -> Entity {
        let mut e = Entity {
            id: id.to_string(),
            name: name.to_string(),
            ..Entity::default()
        };
        e.setup.repo_path = Some(repo.to_string());
        e
    }

    #[test]
    fn explicit_entity_id_wins() {
        let entities = vec![entity_with_repo("ent-a", "Alpha", "acme/widgets")];
        let cap = capture("random text", Some("ent-a"));
        let s = suggest_route(&cap, &entities, None);
        assert_eq!(s.entity_id.as_deref(), Some("ent-a"));
        assert_eq!(s.confidence, 1.0);
    }

    #[test]
    fn github_url_matches_repo_path() {
        let entities = vec![
            entity_with_repo("ent-a", "Alpha", "https://github.com/acme/widgets"),
            entity_with_repo("ent-b", "Beta", "other/thing"),
        ];
        let cap = capture(
            "please review https://github.com/acme/widgets/pull/12",
            None,
        );
        let s = suggest_route(&cap, &entities, None);
        assert_eq!(s.entity_id.as_deref(), Some("ent-a"));
        assert!(s.confidence >= 0.8);
        assert!(s.signals.iter().any(|x| x.contains("acme/widgets")));
    }

    #[test]
    fn domain_in_text_matches_entity_domain() {
        let mut e = Entity {
            id: "ent-a".to_string(),
            name: "Alpha".to_string(),
            ..Entity::default()
        };
        e.domains = vec!["acme.com".to_string()];
        let cap = capture(
            "ship the landing page at https://www.acme.com/pricing",
            None,
        );
        let s = suggest_route(&cap, &[e], None);
        assert_eq!(s.entity_id.as_deref(), Some("ent-a"));
        assert!(s.confidence >= 0.8);
    }

    #[test]
    fn unresolved_capture_returns_no_suggestion() {
        let entities = vec![entity_with_repo("ent-a", "Alpha", "acme/widgets")];
        let cap = capture("just a plain thought with no links", None);
        let s = suggest_route(&cap, &entities, None);
        assert!(s.entity_id.is_none());
        assert_eq!(s.confidence, 0.0);
    }

    #[test]
    fn repo_url_in_metadata_matches_entity() {
        let entities = vec![entity_with_repo(
            "ent-a",
            "Alpha",
            "https://github.com/acme/widgets",
        )];
        let mut cap = capture("did some work", None);
        cap.metadata = serde_json::json!({ "repoUrl": "https://github.com/acme/widgets" });
        let s = suggest_route(&cap, &entities, None);
        assert_eq!(s.entity_id.as_deref(), Some("ent-a"));
        assert!(s.confidence >= AUTO_APPLY_THRESHOLD);
    }

    #[test]
    fn selected_entity_is_low_confidence_fallback() {
        let entities = vec![entity_with_repo("ent-a", "Alpha", "acme/widgets")];
        let cap = capture("no signal here", None);
        let s = suggest_route(&cap, &entities, Some("ent-a"));
        assert_eq!(s.entity_id.as_deref(), Some("ent-a"));
        assert!(s.confidence < AUTO_APPLY_THRESHOLD);
    }
}
