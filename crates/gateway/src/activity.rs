//! Mission Control activity timeline — a read-only, unified view over existing
//! data sources. It merges the operator feed (which already unifies task
//! lifecycle, verification warnings, approvals/questions, mission Definition-of-
//! Done events, and next-action signals) with raw captures and external agent
//! runs, normalizing everything into one `ActivityItem` stream sorted newest
//! first.
//!
//! This is a read-model only: it does NOT introduce a persisted activity table.
//! Callers pass workspace-visible slices (visibility is enforced at the handler,
//! reusing the existing predicates) and this module handles normalization,
//! filtering, ordering, and limiting.

use serde::Serialize;

use crate::state::{ExternalAgentRun, OperatorFeedEvent, RawCapture, VerificationRun};

/// Default cap on returned items when the caller does not specify a limit.
pub const DEFAULT_LIMIT: usize = 100;

/// One normalized entry in the merged activity timeline, regardless of which
/// underlying source it came from.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    /// Stable, source-prefixed id (e.g. `feed:...`, `capture:...`, `run:...`).
    pub id: String,
    /// RFC3339 timestamp used for ordering.
    pub timestamp: String,
    /// Origin data source: `feed` | `capture` | `agent_run`.
    pub source: String,
    /// Normalized kind (feed kind, or `capture` / `agent_run`).
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Underlying record id (capture id, run id, feed event id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
}

/// Query filters applied after normalization. All are optional.
#[derive(Debug, Default, Clone)]
pub struct ActivityFilter<'a> {
    pub entity_id: Option<&'a str>,
    pub mission_id: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub status: Option<&'a str>,
    pub source: Option<&'a str>,
    pub limit: Option<usize>,
}

/// Serialize a snake_case enum value to its string form (e.g. an
/// `OperatorFeedKind` or `RawCaptureStatus`).
fn snake_str<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

// Prefers the summary, else the first line of the raw text; falls back to a
// generic label when there's nothing usable.
fn capture_title(capture: &RawCapture) -> String {
    let base = capture
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| capture.raw_text.trim());
    let first_line = base.lines().next().unwrap_or("").trim();
    let title: String = first_line.chars().take(120).collect();
    if title.is_empty() {
        "Raw capture".to_string()
    } else {
        title
    }
}

// One normalizer per source below, each mapping its own record shape into
// the common ActivityItem fields.
fn feed_item(e: &OperatorFeedEvent) -> ActivityItem {
    ActivityItem {
        id: format!("feed:{}", e.id),
        timestamp: e.timestamp.clone(),
        source: "feed".to_string(),
        kind: snake_str(&e.kind),
        title: e.title.clone(),
        detail: e.detail.clone(),
        status: None,
        entity_id: e.entity_id.clone(),
        mission_id: e.mission_id.clone(),
        task_id: e.task_id.clone(),
        ref_id: Some(e.id.clone()),
    }
}

fn capture_item(c: &RawCapture) -> ActivityItem {
    ActivityItem {
        id: format!("capture:{}", c.id),
        // Use updated_at so a promoted/dismissed capture surfaces its latest move.
        timestamp: c.updated_at.clone(),
        source: "capture".to_string(),
        kind: "capture".to_string(),
        title: capture_title(c),
        detail: c.provider.clone(),
        status: Some(snake_str(&c.status)),
        entity_id: c.entity_id.clone(),
        mission_id: None,
        task_id: c.promoted_task_id.clone(),
        ref_id: Some(c.id.clone()),
    }
}

fn verification_item(v: &VerificationRun) -> ActivityItem {
    let target = v
        .command
        .clone()
        .or_else(|| v.url.clone())
        .unwrap_or_default();
    ActivityItem {
        id: format!("verification:{}", v.id),
        timestamp: v
            .completed_at
            .clone()
            .unwrap_or_else(|| v.created_at.clone()),
        source: "verification".to_string(),
        kind: format!("verification_{}", v.kind),
        title: format!("{} check: {}", v.kind, target),
        detail: if v.logs.trim().is_empty() {
            None
        } else {
            Some(v.logs.lines().next().unwrap_or("").to_string())
        },
        status: Some(v.status.clone()),
        entity_id: v.entity_id.clone(),
        mission_id: v.mission_id.clone(),
        task_id: v.task_id.clone(),
        ref_id: Some(v.id.clone()),
    }
}

fn run_item(r: &ExternalAgentRun) -> ActivityItem {
    ActivityItem {
        id: format!("run:{}", r.id),
        timestamp: r
            .completed_at
            .clone()
            .unwrap_or_else(|| r.started_at.clone()),
        source: "agent_run".to_string(),
        kind: "agent_run".to_string(),
        title: format!("{} agent run", r.provider),
        detail: r.error.clone(),
        status: Some(r.status.clone()),
        entity_id: Some(r.entity_id.clone()),
        mission_id: None,
        task_id: r.task_id.clone(),
        ref_id: Some(r.id.clone()),
    }
}

fn matches(item: &ActivityItem, f: &ActivityFilter) -> bool {
    if let Some(eid) = f.entity_id
        && item.entity_id.as_deref() != Some(eid)
    {
        return false;
    }
    if let Some(mid) = f.mission_id
        && item.mission_id.as_deref() != Some(mid)
    {
        return false;
    }
    if let Some(kind) = f.kind
        && item.kind != kind
    {
        return false;
    }
    if let Some(status) = f.status
        && item.status.as_deref() != Some(status)
    {
        return false;
    }
    if let Some(source) = f.source
        && item.source != source
    {
        return false;
    }
    true
}

/// Merge the workspace-visible slices into one normalized, newest-first timeline.
/// Visibility must already have been applied by the caller.
pub fn build_activity(
    feed: &[OperatorFeedEvent],
    captures: &[RawCapture],
    runs: &[ExternalAgentRun],
    verifications: &[VerificationRun],
    filter: &ActivityFilter,
) -> Vec<ActivityItem> {
    let mut items: Vec<ActivityItem> =
        Vec::with_capacity(feed.len() + captures.len() + runs.len() + verifications.len());
    items.extend(feed.iter().map(feed_item));
    items.extend(captures.iter().map(capture_item));
    items.extend(runs.iter().map(run_item));
    items.extend(verifications.iter().map(verification_item));

    items.retain(|i| matches(i, filter));
    // RFC3339 timestamps in a consistent format sort lexically = chronologically.
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    items.truncate(filter.limit.unwrap_or(DEFAULT_LIMIT));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{OperatorFeedKind, RawCaptureStatus};

    fn feed(id: &str, ts: &str, kind: OperatorFeedKind, entity: Option<&str>) -> OperatorFeedEvent {
        OperatorFeedEvent {
            id: id.to_string(),
            timestamp: ts.to_string(),
            kind,
            task_id: None,
            mission_id: None,
            entity_id: entity.map(str::to_string),
            connection_id: None,
            title: format!("event {id}"),
            detail: None,
            approval_id: None,
        }
    }

    fn capture(id: &str, ts: &str, entity: Option<&str>) -> RawCapture {
        RawCapture {
            id: id.to_string(),
            workspace_id: "default".to_string(),
            entity_id: entity.map(str::to_string),
            source: "chat".to_string(),
            source_ref: None,
            session_id: None,
            provider: None,
            raw_text: "captured thought".to_string(),
            summary: None,
            status: RawCaptureStatus::Raw,
            promoted_task_id: None,
            metadata: serde_json::Value::Null,
            created_at: ts.to_string(),
            updated_at: ts.to_string(),
        }
    }

    fn run(id: &str, started: &str, entity: &str) -> ExternalAgentRun {
        ExternalAgentRun {
            id: id.to_string(),
            workspace_id: "default".to_string(),
            entity_id: entity.to_string(),
            provider: "claude".to_string(),
            prompt: "do it".to_string(),
            cwd: "/tmp".to_string(),
            status: "completed".to_string(),
            task_id: None,
            output: String::new(),
            error: None,
            started_at: started.to_string(),
            completed_at: None,
            metadata: serde_json::Value::Null,
            review_policy: None,
            review_status: None,
            review_verdict: None,
            review_run_id: None,
            reviewed_run_id: None,
        }
    }

    #[test]
    fn merges_and_sorts_newest_first() {
        let feed = [feed(
            "f1",
            "2026-07-04T10:00:00+00:00",
            OperatorFeedKind::TaskCompleted,
            Some("e1"),
        )];
        let caps = [capture("c1", "2026-07-04T12:00:00+00:00", Some("e1"))];
        let runs = [run("r1", "2026-07-04T11:00:00+00:00", "e1")];
        let out = build_activity(&feed, &caps, &runs, &[], &ActivityFilter::default());
        assert_eq!(out.len(), 3);
        // Newest (capture 12:00) first, then run 11:00, then feed 10:00.
        assert_eq!(out[0].source, "capture");
        assert_eq!(out[1].source, "agent_run");
        assert_eq!(out[2].source, "feed");
    }

    #[test]
    fn filters_by_source_and_entity() {
        let feed = [
            feed(
                "f1",
                "2026-07-04T10:00:00+00:00",
                OperatorFeedKind::MissionCompleted,
                Some("e1"),
            ),
            feed(
                "f2",
                "2026-07-04T10:30:00+00:00",
                OperatorFeedKind::TaskCompleted,
                Some("e2"),
            ),
        ];
        let caps = [capture("c1", "2026-07-04T12:00:00+00:00", Some("e1"))];
        let runs = [run("r1", "2026-07-04T11:00:00+00:00", "e2")];

        let only_feed = build_activity(
            &feed,
            &caps,
            &runs,
            &[],
            &ActivityFilter {
                source: Some("feed"),
                ..Default::default()
            },
        );
        assert_eq!(only_feed.len(), 2);
        assert!(only_feed.iter().all(|i| i.source == "feed"));

        let only_e1 = build_activity(
            &feed,
            &caps,
            &runs,
            &[],
            &ActivityFilter {
                entity_id: Some("e1"),
                ..Default::default()
            },
        );
        assert_eq!(only_e1.len(), 2); // f1 + c1
        assert!(only_e1.iter().all(|i| i.entity_id.as_deref() == Some("e1")));
    }

    #[test]
    fn filters_by_kind_and_status() {
        let feed = [feed(
            "f1",
            "2026-07-04T10:00:00+00:00",
            OperatorFeedKind::VerificationNeedsReview,
            Some("e1"),
        )];
        let caps = [capture("c1", "2026-07-04T12:00:00+00:00", Some("e1"))];
        let runs: [ExternalAgentRun; 0] = [];

        let by_kind = build_activity(
            &feed,
            &caps,
            &runs,
            &[],
            &ActivityFilter {
                kind: Some("verification_needs_review"),
                ..Default::default()
            },
        );
        assert_eq!(by_kind.len(), 1);
        assert_eq!(by_kind[0].kind, "verification_needs_review");

        let by_status = build_activity(
            &feed,
            &caps,
            &runs,
            &[],
            &ActivityFilter {
                status: Some("raw"),
                ..Default::default()
            },
        );
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0].source, "capture");
    }

    #[test]
    fn respects_limit() {
        let feed: Vec<OperatorFeedEvent> = (0..5)
            .map(|i| {
                feed(
                    &format!("f{i}"),
                    &format!("2026-07-04T10:0{i}:00+00:00"),
                    OperatorFeedKind::TaskCreated,
                    Some("e1"),
                )
            })
            .collect();
        let out = build_activity(
            &feed,
            &[],
            &[],
            &[],
            &ActivityFilter {
                limit: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(out.len(), 2);
        // Newest two.
        assert_eq!(out[0].ref_id.as_deref(), Some("f4"));
        assert_eq!(out[1].ref_id.as_deref(), Some("f3"));
    }
}
