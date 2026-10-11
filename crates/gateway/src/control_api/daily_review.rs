//! The day view: what the operator should look at this morning, and the two
//! things they can do about it (complete, snooze).
//!
//! A child module of `control_api` rather than a sibling, so `use super::*`
//! reaches the parent's private helpers and request bodies — which is what
//! makes this a pure move rather than a refactor with a visibility rewrite
//! bolted on.

use super::*;

/// Heartbeat beats older than this are yesterday's news for a morning review.
pub(super) const HIGHLIGHT_LOOKBACK_HOURS: i64 = 24;
pub(super) const HIGHLIGHT_MAX: usize = 3;
pub(super) const SNOOZE_DEFAULT_HOURS: u32 = 24;
pub(super) const SNOOZE_MIN_HOURS: u32 = 1;
/// A week. Past this, the operator is rescheduling, not snoozing — that's an
/// edit to `due_at`, which `PATCH /api/tasks/{id}` already does.
pub(super) const SNOOZE_MAX_HOURS: u32 = 168;

/// A task as the day view renders it. The full `Task` carries orchestration
/// bookkeeping (roles, tries, retry markers) that a morning review never shows,
/// and the payload is fetched on every poll.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskLite {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) entity_id: Option<String>,
    pub(super) entity_name: Option<String>,
    pub(super) mission_id: Option<String>,
    pub(super) priority: Option<String>,
    pub(super) due_at: Option<String>,
    pub(super) status: TaskStatus,
    /// Whole hours past the deadline; absent unless the task is overdue.
    pub(super) overdue_hours: Option<i64>,
}

/// A pending approval as the day view renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PendingApprovalLite {
    pub(super) id: String,
    pub(super) entity_id: String,
    pub(super) title: String,
    pub(super) created_at: String,
}

/// A mission waiting on the operator, as the day view renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MissionAwaitingInputLite {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) entity_id: String,
}

/// A recent heartbeat beat worth surfacing in the day view.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HeartbeatHighlight {
    pub(super) entity_id: Option<String>,
    pub(super) entity_name: Option<String>,
    pub(super) summary: String,
    pub(super) fired_at: String,
}

/// A heartbeat due within the lookahead window, as the day view renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpcomingHeartbeatLite {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) target_id: Option<String>,
    pub(super) target_name: Option<String>,
    pub(super) next_fire_at: Option<String>,
}

/// Full payload for the Today / daily-review screen.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DailyReviewResponse {
    pub(super) generated_at: String,
    pub(super) overdue: Vec<TaskLite>,
    pub(super) due_today: Vec<TaskLite>,
    pub(super) pending_approvals: Vec<PendingApprovalLite>,
    pub(super) missions_awaiting_input: Vec<MissionAwaitingInputLite>,
    pub(super) heartbeat_highlights: Vec<HeartbeatHighlight>,
    pub(super) upcoming_heartbeats: Vec<UpcomingHeartbeatLite>,
    pub(super) streak: crate::accountability::StreakSummary,
    pub(super) suggested_focus: Vec<TaskLite>,
}

/// Projects a [`Task`] down to the [`TaskLite`] shape the day view needs.
pub(super) fn task_lite(task: &Task, entity_names: &HashMap<String, String>, now: DateTime<Utc>) -> TaskLite {
    TaskLite {
        id: task.id.clone(),
        title: task.title.clone(),
        entity_id: task.entity_id.clone(),
        entity_name: task
            .entity_id
            .as_deref()
            .and_then(|id| entity_names.get(id).cloned()),
        mission_id: task.mission_id.clone(),
        priority: task.priority.clone(),
        // Resolved, not raw: a reminder-era task keeps its deadline in metadata.
        due_at: crate::state::task_due_at(task).map(|d| d.to_rfc3339()),
        status: task.status,
        overdue_hours: crate::accountability::overdue_hours(task, now),
    }
}

/// Recent beats worth reading, scoped to the caller's workspace. Absent store
/// (tests, or a gateway booted without Surreal) means no highlights, never an
/// error — the rest of the review is still worth rendering.
pub(super) async fn heartbeat_highlights(
    state: &ServerState,
    entity_ids: &HashSet<String>,
    entity_names: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> Vec<HeartbeatHighlight> {
    let Some(store) = state.heartbeat_results_store.as_ref() else {
        return Vec::new();
    };
    // Over-fetch: the newest rows are cross-entity and most will belong to
    // other workspaces.
    let rows = match store.list_recent(50).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "daily review: heartbeat highlights unavailable");
            return Vec::new();
        }
    };
    let cutoff = now - chrono::Duration::hours(HIGHLIGHT_LOOKBACK_HOURS);
    rows.into_iter()
        .filter(|r| {
            r.entity_id
                .as_deref()
                .is_some_and(|id| entity_ids.contains(id))
        })
        .filter(|r| {
            DateTime::parse_from_rfc3339(&r.fired_at)
                .map(|t| t.with_timezone(&Utc) >= cutoff)
                .unwrap_or(false)
        })
        .take(HIGHLIGHT_MAX)
        .map(|r| HeartbeatHighlight {
            entity_name: r
                .entity_id
                .as_deref()
                .and_then(|id| entity_names.get(id).cloned()),
            entity_id: r.entity_id,
            summary: r.summary,
            fired_at: r.fired_at,
        })
        .collect()
}

/// One call that renders a day: what slipped, what's due, what's waiting on a
/// decision, and whether the operator kept the streak. Deliberately free of LLM
/// calls — this is polled, and the Today screen must open instantly.
pub(super) async fn get_daily_review(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<DailyReviewResponse>, StatusCode> {
    let s = control(&state)?;
    let now = Utc::now();
    let workspace_id = workspace.id;
    let (entity_ids, entity_names) = {
        let entities = s.entities.read().await;
        let ids = entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        );
        let names: HashMap<String, String> = entities
            .iter()
            .filter(|e| ids.contains(&e.id))
            .map(|e| (e.id.clone(), e.name.clone()))
            .collect();
        (ids, names)
    };

    // Internal kinds (reminders, notes, beats) carry due dates for their own
    // schedulers; they are not commitments the operator owes anyone.
    let tasks: Vec<Task> = {
        let tasks = s.tasks.read().await;
        tasks
            .iter()
            .filter(|t| task_visible_in_workspace(t, &entity_ids, &workspace_id))
            .filter(|t| !crate::state::task_kind_is_internal(t))
            .cloned()
            .collect()
    };

    let mut overdue: Vec<&Task> = tasks
        .iter()
        .filter(|t| crate::state::task_is_overdue(t, now))
        .collect();
    overdue.sort_by_key(|t| crate::state::task_due_at(t));
    let mut due_today: Vec<&Task> = tasks
        .iter()
        .filter(|t| crate::accountability::task_due_today(t, now))
        .collect();
    due_today.sort_by_key(|t| crate::state::task_due_at(t));

    let pending_approvals: Vec<PendingApprovalLite> = {
        let approvals = s.approvals.read().await;
        approvals
            .iter()
            .filter(|a| a.status == ApprovalStatus::Pending)
            .filter(|a| entity_ids.contains(&a.entity_id))
            .map(|a| PendingApprovalLite {
                id: a.id.clone(),
                entity_id: a.entity_id.clone(),
                title: a
                    .detail
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .unwrap_or(a.action.as_str())
                    .to_string(),
                created_at: a.created_at.clone(),
            })
            .collect()
    };

    let missions_awaiting_input: Vec<MissionAwaitingInputLite> = {
        let missions = s.missions.read().await;
        missions
            .iter()
            .filter(|m| m.status == MissionStatus::UserInputNeeded)
            .filter(|m| entity_ids.contains(&m.entity_id))
            .map(|m| MissionAwaitingInputLite {
                id: m.id.clone(),
                title: m.title.clone(),
                entity_id: m.entity_id.clone(),
            })
            .collect()
    };

    let highlights = heartbeat_highlights(&state, &entity_ids, &entity_names, now).await;
    let upcoming_heartbeats = if let Some(store) = state.heartbeat_store.as_ref() {
        use crate::heartbeat_store::HeartbeatStore;
        store
            .list_all()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|h| {
                h.enabled
                    && h.workspace_id.as_deref() == Some(workspace_id.as_str())
                    && h.target_id
                        .as_deref()
                        .is_none_or(|id| entity_ids.contains(id))
                    && h.next_fire_at
                        .as_deref()
                        .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                        .is_some_and(|v| v.with_timezone(&Utc) <= now + chrono::Duration::days(7))
            })
            .take(12)
            .map(|h| UpcomingHeartbeatLite {
                id: h.id,
                name: h.name,
                target_name: h
                    .target_id
                    .as_deref()
                    .and_then(|id| entity_names.get(id).cloned()),
                target_id: h.target_id,
                next_fire_at: h.next_fire_at,
            })
            .collect()
    } else {
        Vec::new()
    };
    let focus = crate::accountability::select_focus(&tasks, now, crate::accountability::FOCUS_CAP);

    Ok(Json(DailyReviewResponse {
        generated_at: now.to_rfc3339(),
        overdue: overdue
            .iter()
            .map(|t| task_lite(t, &entity_names, now))
            .collect(),
        due_today: due_today
            .iter()
            .map(|t| task_lite(t, &entity_names, now))
            .collect(),
        pending_approvals,
        missions_awaiting_input,
        heartbeat_highlights: highlights,
        upcoming_heartbeats,
        streak: crate::accountability::compute_streak(&tasks, now),
        suggested_focus: focus
            .iter()
            .map(|t| task_lite(t, &entity_names, now))
            .collect(),
    }))
}

/// Body for snoozing a task's deadline. Defaults to [`SNOOZE_DEFAULT_HOURS`],
/// clamped between [`SNOOZE_MIN_HOURS`] and [`SNOOZE_MAX_HOURS`].
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnoozeTaskRequest {
    #[serde(default)]
    pub(super) hours: Option<u32>,
}

/// Push a deadline out and forget that it ever slipped.
///
/// Clearing the overdue markers is the point: a snoozed task that stays marked
/// would never notify again, so "remind me tomorrow" would silently mean
/// "never remind me".
pub(super) async fn snooze_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    body: Option<Json<SnoozeTaskRequest>>,
) -> Result<Json<Task>, StatusCode> {
    let s = control(&state)?;
    let hours = body
        .and_then(|Json(b)| b.hours)
        .unwrap_or(SNOOZE_DEFAULT_HOURS)
        .clamp(SNOOZE_MIN_HOURS, SNOOZE_MAX_HOURS) as i64;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let updated = {
        let mut tasks = s.tasks.write().await;
        let t = tasks
            .iter_mut()
            .find(|t| t.id == id && task_visible_in_workspace(t, &entity_ids, &workspace_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        if matches!(
            t.status,
            TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
        ) {
            return Err(StatusCode::CONFLICT);
        }
        // Snoozing an already-late task means "from now", not "from the
        // deadline I already blew" — otherwise a week-old task snoozes into the
        // past and re-fires immediately.
        let base = crate::state::task_due_at(t)
            .map(|due| due.max(Utc::now()))
            .unwrap_or_else(Utc::now);
        t.due_at = Some((base + chrono::Duration::hours(hours)).to_rfc3339());
        crate::state::task_meta_clear(t, crate::accountability::OVERDUE_NOTIFIED_KEY);
        crate::state::task_meta_clear(t, crate::accountability::OVERDUE_ESCALATED_KEY);
        t.clone()
    };
    // Not a notifiable kind: rescheduling one's own work is timeline detail,
    // not an interruption.
    s.push_operator_feed(
        OperatorFeedKind::TaskRetryScheduled,
        &updated,
        Some(format!("Snoozed {hours}h")),
    )
    .await;
    Ok(Json(updated))
}
