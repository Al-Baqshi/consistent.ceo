//! Evidence: what actually changed on disk after an unattended run.
//!
//! The trailer parser can only read what the agent *says*; this module reads
//! what the agent *did*. A `done` claim from a run whose jail directory shows
//! zero commits, zero diff, and zero new dirty files is a claim nobody can
//! vouch for — it gets downgraded to NeedsReview instead of silently trusted
//! (`plan_task_run_update`), and the pack is handed to the reviewer so a
//! critic judges claims against diffs, not prose.
//!
//! Deliberately git-only: a directory without git history can prove presence
//! of change but never absence, so packs from non-git directories are
//! informational and trigger no downgrade.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// The pre-run state the post-run collection diffs against.
#[derive(Debug, Clone)]
pub struct GitBaseline {
    pub head: Option<String>,
    pub dirty: BTreeSet<String>,
    /// Fingerprint of the uncommitted diff at baseline, so a later edit to an
    /// already-dirty file is still visible as change.
    pub dirty_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidencePack {
    /// False when the run directory has no git history; such a pack proves
    /// nothing about absence of change and never downgrades a verdict.
    pub is_git: bool,
    /// Commits the run added on top of the baseline HEAD.
    pub commits: u32,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// Paths that became dirty (or newly untracked) during the run without
    /// being committed.
    pub dirty_delta: Vec<String>,
    /// The working tree's uncommitted *content* changed, even if the set of
    /// dirty paths did not. Catches the run that edited a file which was
    /// already modified before it started.
    #[serde(default)]
    pub dirty_content_changed: bool,
}

impl EvidencePack {
    /// True when the pack can positively say "nothing on disk changed".
    ///
    /// `dirty_delta` is a set *difference* against the baseline, so editing a
    /// file that was already dirty before the run adds nothing to it. Without
    /// `dirty_content_changed`, such a run — real work, real edits — would look
    /// identical to a run that did nothing and get its verdict downgraded.
    pub fn shows_no_observable_change(&self) -> bool {
        self.is_git
            && self.commits == 0
            && self.files_changed == 0
            && self.dirty_delta.is_empty()
            && !self.dirty_content_changed
    }
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let git = crate::entity_workspace::locate_git().unwrap_or_else(|| "git".into());
    let out = consistent_world_common::platform::command(git)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn porcelain_paths(porcelain: &str) -> BTreeSet<String> {
    porcelain
        .lines()
        .filter(|l| l.len() > 3)
        .map(|l| l[3..].trim().to_string())
        .collect()
}

/// Snapshot the run directory before the agent starts. `None` means "not a
/// git repository" — collection then produces an informational-only pack.
pub async fn snapshot(cwd: &Path) -> Option<GitBaseline> {
    let head = git(cwd, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string());
    // An empty repo (no commits yet) still has a porcelain; only a missing
    // .git makes status fail.
    let porcelain = git(cwd, &["status", "--porcelain"]).await?;
    Some(GitBaseline {
        head,
        dirty: porcelain_paths(&porcelain),
        dirty_fingerprint: dirty_fingerprint(cwd).await,
    })
}

/// A cheap stand-in for "what do the uncommitted changes look like right now":
/// the diff stat of tracked edits. Compared before and after, it reveals work
/// done to files that were already dirty.
async fn dirty_fingerprint(cwd: &Path) -> Option<String> {
    git(cwd, &["diff", "--shortstat"])
        .await
        .map(|s| s.trim().to_string())
}

/// Parse `git diff --shortstat` output like
/// ` 3 files changed, 10 insertions(+), 2 deletions(-)`.
fn parse_shortstat(s: &str) -> (u32, u32, u32) {
    let mut files = 0;
    let mut ins = 0;
    let mut del = 0;
    for part in s.split(',') {
        let part = part.trim();
        let Some(n) = part
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u32>().ok())
        else {
            continue;
        };
        if part.contains("file") {
            files = n;
        } else if part.contains("insertion") {
            ins = n;
        } else if part.contains("deletion") {
            del = n;
        }
    }
    (files, ins, del)
}

/// Collect what changed since the baseline. A `None` baseline (non-git dir)
/// yields an informational pack with `is_git: false`.
pub async fn collect(cwd: &Path, baseline: Option<&GitBaseline>) -> EvidencePack {
    let Some(baseline) = baseline else {
        return EvidencePack::default();
    };
    let head_now = git(cwd, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string());
    let commits = match (&baseline.head, &head_now) {
        (Some(old), Some(_)) => git(cwd, &["rev-list", "--count", &format!("{old}..HEAD")])
            .await
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0),
        // No baseline HEAD (fresh repo): every commit now present is new.
        (None, Some(_)) => git(cwd, &["rev-list", "--count", "HEAD"])
            .await
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0),
        _ => 0,
    };
    let (files_changed, insertions, deletions) = match (&baseline.head, commits) {
        (Some(old), c) if c > 0 => git(cwd, &["diff", "--shortstat", &format!("{old}..HEAD")])
            .await
            .map(|s| parse_shortstat(&s))
            .unwrap_or((0, 0, 0)),
        _ => (0, 0, 0),
    };
    let dirty_now = git(cwd, &["status", "--porcelain"])
        .await
        .map(|s| porcelain_paths(&s))
        .unwrap_or_default();
    let dirty_delta: Vec<String> = dirty_now.difference(&baseline.dirty).cloned().collect();
    let fingerprint_now = dirty_fingerprint(cwd).await;
    let dirty_content_changed = match (&baseline.dirty_fingerprint, &fingerprint_now) {
        (Some(before), Some(after)) => before != after,
        // Unknown either side: say nothing rather than claim absence.
        _ => false,
    };
    EvidencePack {
        is_git: true,
        commits,
        files_changed,
        insertions,
        deletions,
        dirty_delta,
        dirty_content_changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(cwd: &Path, cmd: &str) {
        let (program, args) = consistent_world_common::platform::shell_command(cmd, false);
        let out = consistent_world_common::platform::std_command(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(cwd: &Path) {
        sh(cwd, "git init -q");
        sh(
            cwd,
            "git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init",
        );
    }

    #[tokio::test]
    async fn a_non_git_directory_never_claims_absence_of_change() {
        let tmp = tempfile::tempdir().unwrap();
        let baseline = snapshot(tmp.path()).await;
        assert!(baseline.is_none());
        let pack = collect(tmp.path(), baseline.as_ref()).await;
        assert!(!pack.is_git);
        assert!(!pack.shows_no_observable_change());
    }

    #[tokio::test]
    async fn an_untouched_repo_shows_no_observable_change() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let baseline = snapshot(tmp.path()).await;
        let pack = collect(tmp.path(), baseline.as_ref()).await;
        assert!(pack.is_git);
        assert!(pack.shows_no_observable_change());
    }

    #[tokio::test]
    async fn a_commit_and_a_dirty_file_both_count_as_change() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let baseline = snapshot(tmp.path()).await.expect("baseline");

        sh(
            tmp.path(),
            "echo hello > committed.txt && git add . && git -c user.email=t@t -c user.name=t commit -q -m work",
        );
        sh(tmp.path(), "echo draft > dirty.txt");

        let pack = collect(tmp.path(), Some(&baseline)).await;
        assert!(pack.is_git);
        assert_eq!(pack.commits, 1);
        assert!(pack.files_changed >= 1);
        assert_eq!(pack.dirty_delta, vec!["dirty.txt".to_string()]);
        assert!(!pack.shows_no_observable_change());
    }

    #[tokio::test]
    async fn editing_an_already_dirty_file_still_counts_as_work() {
        // The false-positive that mattered: `dirty_delta` is a set difference,
        // so a run that edits a file which was ALREADY modified adds no new
        // path — and the run's real work looked like nothing at all.
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        sh(
            tmp.path(),
            "echo one > tracked.txt && git add . && git -c user.email=t@t -c user.name=t commit -q -m base",
        );
        sh(tmp.path(), "echo two >> tracked.txt");
        let baseline = snapshot(tmp.path()).await.expect("baseline");

        // The run appends more to the same already-dirty file.
        sh(tmp.path(), "echo three >> tracked.txt");

        let pack = collect(tmp.path(), Some(&baseline)).await;
        assert!(pack.dirty_delta.is_empty(), "no NEW path became dirty");
        assert!(pack.dirty_content_changed, "but the content did change");
        assert!(
            !pack.shows_no_observable_change(),
            "this run must not be downgraded as having done nothing"
        );
    }

    #[tokio::test]
    async fn pre_existing_dirt_is_not_credited_to_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        sh(tmp.path(), "echo old > already_dirty.txt");
        let baseline = snapshot(tmp.path()).await.expect("baseline");
        let pack = collect(tmp.path(), Some(&baseline)).await;
        assert!(pack.shows_no_observable_change());
    }

    #[test]
    fn shortstat_parses_all_three_fields() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)"),
            (3, 10, 2)
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 1 insertion(+)"),
            (1, 1, 0)
        );
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }
}

// ─── Chat-turn evidence posture ────────────────────────────────────────────
//
// The same instinct as the git-diff packs above, applied to a conversation
// instead of a directory: say what the answer is actually grounded in, and be
// honest when the answer is that it is grounded in nothing but the model.
//
// Deliberately cheap. It reads provenance — did a tool produce something this
// turn? — and nothing else. It does not resolve claims; that is the Architect's
// Verify stage, which runs in shadow mode and is far more expensive to be wrong
// about. The two share one vocabulary (`EvidenceState`) so that when Verify
// eventually reaches the wire it *upgrades* this label rather than competing
// with it.

use consistent_world_architect::EvidenceState;

/// What a chat turn's answer rests on, as far as provenance can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvidence {
    pub state: EvidenceState,
    /// Tools that returned something usable this turn, in call order and
    /// de-duplicated. Empty for an `Inferred` turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl ChatEvidence {
    /// Label a turn from the tools that ran during it.
    ///
    /// `Observed` when at least one tool returned a non-error result — VISION's
    /// definition is "backed by a tool result or ingested source in this turn".
    /// `Inferred` otherwise, which is the honest label for an answer the model
    /// reasoned out.
    ///
    /// A failed tool call grounds nothing, so it does not count. Nor does it
    /// downgrade the turn below `Inferred`: the model may well have answered
    /// correctly without it, and `Unverified` is a claim about resolution that
    /// this function is not entitled to make.
    pub fn from_tool_calls<'a>(
        results: impl IntoIterator<Item = (&'a str, bool)>,
    ) -> Self {
        let mut tools: Vec<String> = Vec::new();
        for (name, is_error) in results {
            if is_error {
                continue;
            }
            if !tools.iter().any(|t| t == name) {
                tools.push(name.to_string());
            }
        }
        let state = if tools.is_empty() {
            EvidenceState::Inferred
        } else {
            EvidenceState::Observed
        };
        ChatEvidence { state, tools }
    }
}

#[cfg(test)]
mod chat_evidence_tests {
    use super::*;

    #[test]
    fn a_turn_with_no_tools_is_inferred() {
        let e = ChatEvidence::from_tool_calls([]);
        assert_eq!(e.state, EvidenceState::Inferred);
        assert!(e.tools.is_empty());
    }

    #[test]
    fn a_successful_tool_makes_the_turn_observed() {
        let e = ChatEvidence::from_tool_calls([("list_entities", false)]);
        assert_eq!(e.state, EvidenceState::Observed);
        assert_eq!(e.tools, vec!["list_entities"]);
    }

    #[test]
    fn a_failed_tool_grounds_nothing() {
        // It must not claim Observed — but nor may it claim the answer is
        // unverified, which is a statement about resolution this cannot make.
        let e = ChatEvidence::from_tool_calls([("list_entities", true)]);
        assert_eq!(e.state, EvidenceState::Inferred);
        assert!(e.tools.is_empty());
    }

    #[test]
    fn one_good_tool_among_failures_still_grounds_the_turn() {
        let e = ChatEvidence::from_tool_calls([
            ("broken_tool", true),
            ("recall_recent", false),
        ]);
        assert_eq!(e.state, EvidenceState::Observed);
        assert_eq!(e.tools, vec!["recall_recent"]);
    }

    #[test]
    fn repeated_calls_to_one_tool_are_named_once() {
        let e = ChatEvidence::from_tool_calls([
            ("list_entities", false),
            ("list_entities", false),
            ("review_today", false),
        ]);
        assert_eq!(e.tools, vec!["list_entities", "review_today"]);
    }

    #[test]
    fn the_wire_shape_is_snake_case_and_omits_empty_tools() {
        // Feed and evidence tokens cross to desktop and mobile as snake_case;
        // an absent tool list must not ship as `null`.
        let v = serde_json::to_value(ChatEvidence::from_tool_calls([])).expect("serialise");
        assert_eq!(v["state"], "inferred");
        assert!(v.get("tools").is_none(), "empty tools must be omitted: {v}");
    }
}
