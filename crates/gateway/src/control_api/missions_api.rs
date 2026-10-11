//! Missions: create, read, cancel, report, and the recommendation metrics over them.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists missions visible in the workspace, filtered and paginated.
pub(super) async fn list_missions(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<MissionQuery>,
) -> Result<Json<PaginatedResponse<Mission>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let missions = s.missions.read().await;
    let filtered: Vec<Mission> = missions
        .iter()
        .filter(|m| entity_ids.contains(&m.entity_id))
        .filter(|m| {
            if let Some(ref entity_id) = q.entity_id {
                m.entity_id == *entity_id
            } else {
                true
            }
        })
        .filter(|m| {
            if let Some(ref status) = q.status {
                format!("{:?}", m.status).to_lowercase() == *status.to_lowercase()
            } else {
                true
            }
        })
        .cloned()
        .collect();
    let total = filtered.len();
    let pag = apply_pagination(
        filtered,
        total,
        &PaginationQuery {
            limit: q.limit,
            offset: q.offset,
        },
    );
    Ok(Json(pag))
}

/// `POST` handler: creates a mission and, when mission planning is enabled,
/// kicks off task decomposition (inline without a loaded LLM config, else
/// spawned so the LLM call doesn't block the response).
pub(super) async fn create_mission(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateMissionRequest>,
) -> Result<Json<Mission>, StatusCode> {
    let s = control(&state)?;
    let mission = internal_create_mission(
        s,
        &workspace.id,
        Some(workspace.principal_id.as_str()),
        body,
    )
    .await?;

    if mission_planning_enabled(&state) {
        // Planning makes an LLM call with a 1-3s floor on the Claude CLI path,
        // so it must not sit inside the response. Without a loaded config there
        // is no brain to call and planning is pure CPU — run it inline so
        // mission creation stays atomic for embedders and tests.
        let control = state
            .control_state
            .clone()
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        if state.app_config.is_some() {
            let state = state.clone();
            let mission = mission.clone();
            tokio::spawn(async move {
                crate::mission_planner::plan_and_attach_tasks(&state, &control, &mission).await;
            });
        } else {
            crate::mission_planner::plan_and_attach_tasks(&state, &control, &mission).await;
        }
    }
    Ok(Json(mission))
}

/// `GET` handler: fetches a single mission by id, scoped to the workspace.
pub(super) async fn get_mission(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Mission>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let missions = s.missions.read().await;
    missions
        .iter()
        .find(|m| m.id == id && entity_ids.contains(&m.entity_id))
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Goal-level Definition of Done: is this mission's work finished? Aggregates the
/// operational state of the mission's tasks into a done/blocked/in-progress signal.
pub(super) async fn get_mission_dod(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<crate::mission_dod::DodAssessment>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let mission = {
        let missions = s.missions.read().await;
        missions
            .iter()
            .find(|m| m.id == id && entity_ids.contains(&m.entity_id))
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let tasks = s.tasks.read().await;
    let mission_tasks: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.mission_id.as_deref() == Some(mission.id.as_str()))
        .collect();
    Ok(Json(crate::mission_dod::assess_dod(
        &mission,
        &mission_tasks,
    )))
}

/// `GET` handler: returns a mission's final report, generating it on demand
/// if it hasn't been built yet.
pub(super) async fn get_mission_report(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<MissionReportResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    // Access check: the mission must be visible in this workspace.
    let existing = {
        let missions = s.missions.read().await;
        let m = missions
            .iter()
            .find(|m| m.id == id && entity_ids.contains(&m.entity_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        m.final_report.clone()
    };
    // Return the stored report, or generate one on demand.
    let report = match existing {
        Some(r) => r,
        None => generate_mission_report(s, &id)
            .await
            .ok_or(StatusCode::NOT_FOUND)?,
    };
    Ok(Json(MissionReportResponse {
        mission_id: id,
        report,
    }))
}

/// Handler: cancels a mission and its non-terminal tasks, and marks any
/// running external agent runs on those tasks as cancelled (best-effort —
/// the underlying CLI process cannot actually be killed here).
pub(super) async fn cancel_mission(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Mission>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let mission = {
        let mut missions = s.missions.write().await;
        let m = missions
            .iter_mut()
            .find(|m| m.id == id && entity_ids.contains(&m.entity_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        if matches!(
            m.status,
            MissionStatus::Completed | MissionStatus::Cancelled
        ) {
            return Err(StatusCode::CONFLICT);
        }
        m.status = MissionStatus::Cancelled;
        m.updated_at = chrono::Utc::now().to_rfc3339();
        m.clone()
    };
    // Cancel the mission's non-terminal tasks so the loop stops working on it.
    let (cancelled_tasks, mission_task_ids) = {
        let mut tasks = s.tasks.write().await;
        let mut n = 0;
        let mut ids = std::collections::HashSet::new();
        for t in tasks.iter_mut() {
            if t.mission_id.as_deref() == Some(id.as_str()) {
                ids.insert(t.id.clone());
                if matches!(
                    t.status,
                    TaskStatus::Backlog | TaskStatus::Queued | TaskStatus::InProgress
                ) {
                    crate::state::set_task_status_by_human(t, TaskStatus::Cancelled);
                    t.progress = 100;
                    crate::state::task_meta_clear(t, "nextRetryAt");
                    t.assigned_director_id = None;
                    t.assigned_agent_id = None;
                    t.result = Some("Cancelled because its mission was cancelled.".to_string());
                    n += 1;
                }
            }
        }
        (n, ids)
    };
    // Cancel in-flight external runs for this mission's tasks (honest: the CLI
    // process cannot be killed, its result will be ignored).
    {
        let mut runs = s.external_agent_runs.write().await;
        for run in runs.iter_mut() {
            if run.status == "running"
                && run
                    .task_id
                    .as_ref()
                    .is_some_and(|tid| mission_task_ids.contains(tid))
            {
                run.status = "cancelled".to_string();
                run.completed_at = Some(chrono::Utc::now().to_rfc3339());
                run.error = Some(
                    "Cancelled with its mission; the process may still be running.".to_string(),
                );
            }
        }
    }
    s.push_mission_feed(
        OperatorFeedKind::MissionCancelled,
        &mission,
        Some(format!(
            "Cancelled by user; {cancelled_tasks} task(s) cancelled."
        )),
    )
    .await;
    Ok(Json(mission))
}

/// `PATCH` handler: partial-updates a mission's fields.
pub(super) async fn update_mission(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMissionRequest>,
) -> Result<Json<Mission>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    if let Some(priority) = body.priority.as_deref()
        && !is_valid_priority(priority)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut missions = s.missions.write().await;
    let mission = missions
        .iter_mut()
        .find(|m| m.id == id && entity_ids.contains(&m.entity_id))
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(title) = body.title {
        if title.trim().is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        mission.title = title.trim().to_string();
    }
    if let Some(summary) = body.summary {
        mission.summary = summary;
    }
    if let Some(status) = body.status {
        mission.status = status;
    }
    if let Some(priority) = body.priority {
        mission.priority = priority;
    }
    if let Some(goal) = body.goal {
        mission.goal = goal;
    }
    if let Some(owner) = body.owner {
        mission.owner = Some(owner);
    }
    if let Some(last_reviewed_at) = body.last_reviewed_at {
        mission.last_reviewed_at = Some(last_reviewed_at);
    }
    if let Some(review_cadence_hours) = body.review_cadence_hours {
        mission.review_cadence_hours = Some(review_cadence_hours);
    }
    if let Some(narrative) = body.narrative {
        mission.narrative = Some(narrative);
    }
    if let Some(canonical_notes) = body.canonical_notes {
        mission.canonical_notes = canonical_notes;
    }
    if let Some(assigned_director_id) = body.assigned_director_id {
        mission.assigned_director_id = Some(assigned_director_id);
    }
    mission.updated_at = chrono::Utc::now().to_rfc3339();
    Ok(Json(mission.clone()))
}

/// `GET` handler: mission-recommendation counters plus stale-recommendation
/// stats derived live from the operator feed.
pub(super) async fn get_mission_metrics(
    State(state): State<ServerState>,
) -> Result<Json<MissionMetricsView>, StatusCode> {
    let s = control(&state)?;
    let metrics = s.mission_metrics.read().await;
    let mut by_branch: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for (key, value) in metrics.iter() {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() == 4 && parts[0] == "outcome" && parts[2] == "branch" {
            by_branch
                .entry(parts[3].to_string())
                .or_default()
                .insert(parts[1].to_string(), *value);
        }
    }
    let stale = derive_stale_recommendations(&s.operator_feed.read().await, 24);
    for (branch, count) in stale.by_branch {
        by_branch
            .entry(branch)
            .or_default()
            .insert("stale".to_string(), count);
    }
    Ok(Json(MissionMetricsView {
        recommendation_emitted: *metrics
            .get("recommendation_emitted")
            .unwrap_or(metrics.get("outcome.emitted").unwrap_or(&0)),
        recommendation_accepted: *metrics
            .get("recommendation_accepted")
            .unwrap_or(metrics.get("outcome.accepted").unwrap_or(&0)),
        recommendation_dismissed: *metrics
            .get("recommendation_dismissed")
            .unwrap_or(metrics.get("outcome.dismissed").unwrap_or(&0)),
        duplicate_suppressed: *metrics
            .get("duplicate_suppressed")
            .unwrap_or(metrics.get("outcome.suppressed_duplicate").unwrap_or(&0)),
        stale_unresolved: stale.total,
        time_to_first_completed_mission_seconds: metrics
            .get(TIME_TO_FIRST_COMPLETED_MISSION_METRIC)
            .copied(),
        by_branch,
    }))
}

/// Scans the operator feed for recommendation-emitting events older than
/// `stale_after_hours` that have no later accept/dismiss/snooze/input-needed
/// event for the same mission.
pub(super) fn derive_stale_recommendations(
    feed: &[OperatorFeedEvent],
    stale_after_hours: i64,
) -> StaleRecommendationStats {
    let now = Utc::now();
    let mut total = 0_u64;
    let mut by_branch: HashMap<String, u64> = HashMap::new();
    for (idx, event) in feed.iter().enumerate() {
        let is_reco_emit = matches!(
            event.kind,
            OperatorFeedKind::MissionMissingNextTask | OperatorFeedKind::MissionStalled
        );
        if !is_reco_emit {
            continue;
        }
        let Some(mission_id) = event.mission_id.as_deref() else {
            continue;
        };
        let emitted_at = DateTime::parse_from_rfc3339(&event.timestamp)
            .map(|dt| dt.with_timezone(&Utc))
            .ok();
        let Some(emitted_at) = emitted_at else {
            continue;
        };
        if now.signed_duration_since(emitted_at).num_hours() < stale_after_hours {
            continue;
        }
        let branch = event
            .detail
            .as_deref()
            .map(parse_detail_kv)
            .and_then(|m| m.get("branch").cloned())
            .unwrap_or_else(|| "unknown".to_string());
        let resolved = feed[idx + 1..].iter().any(|later| {
            later.mission_id.as_deref() == Some(mission_id)
                && matches!(
                    later.kind,
                    OperatorFeedKind::MissionRecommendationAccepted
                        | OperatorFeedKind::MissionRecommendationDismissed
                        | OperatorFeedKind::MissionSnoozed
                        | OperatorFeedKind::MissionUserInputNeeded
                )
        });
        if !resolved {
            total += 1;
            *by_branch.entry(branch).or_insert(0) += 1;
        }
    }
    StaleRecommendationStats { total, by_branch }
}

/// Parses a feed event's `detail` string of `key=value` lines into a map.
pub(super) fn parse_detail_kv(detail: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in detail.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let value = v.trim();
            if !key.is_empty() && !value.is_empty() {
                out.insert(key.to_string(), value.to_string());
            }
        }
    }
    out
}

/// The `branch` detail of the most recent recommendation/stall/suppression
/// event for this mission, if any.
pub(super) async fn latest_recommendation_branch(
    s: &crate::state::ControlState,
    mission_id: &str,
) -> Option<String> {
    let feed = s.operator_feed.read().await;
    feed.iter()
        .rev()
        .find(|e| {
            e.mission_id.as_deref() == Some(mission_id)
                && matches!(
                    e.kind,
                    OperatorFeedKind::MissionMissingNextTask
                        | OperatorFeedKind::MissionStalled
                        | OperatorFeedKind::MissionFollowupSuppressed
                )
        })
        .and_then(|e| e.detail.as_deref())
        .and_then(|detail| parse_detail_kv(detail).get("branch").cloned())
}

/// Longest review snooze: one year. Anything larger is a 400, not a panic.
const MAX_SNOOZE_HOURS: u32 = 8_760;

/// Handler: applies a human review action to a mission's recommendation
/// (review now, snooze, accept the proposed next step, dismiss, or mark
/// user-input-needed), each emitting the matching feed event.
pub(super) async fn apply_mission_action(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<MissionActionRequest>,
) -> Result<Json<Mission>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let now = chrono::Utc::now();
    let now_rfc3339 = now.to_rfc3339();
    // Validated before any lock is taken: an unbounded `hours` used to overflow
    // `now + Duration::hours` and panic while holding `missions.write()`.
    let snooze = match &body.action {
        MissionActionType::SnoozeReview => {
            let hours = body.hours.unwrap_or(24).max(1);
            if hours > MAX_SNOOZE_HOURS {
                return Err(StatusCode::BAD_REQUEST);
            }
            let until = now
                .checked_add_signed(chrono::Duration::hours(i64::from(hours)))
                .ok_or(StatusCode::BAD_REQUEST)?;
            Some((hours, until))
        }
        _ => None,
    };

    // Lock order (rule 1 on `ControlState`): mutate under the guard, clone the
    // result, and drop the guard before any `await`. Every side effect below —
    // feed pushes (a Surreal write, vault I/O, then `entities.read()`), the new
    // task (`tasks.write()`), metrics — runs with no mission guard held. Holding
    // `missions.write()` across them deadlocked against readers that hold
    // `tasks`/`entities` and then wait on `missions` (canvas hydrate, the voice
    // context) as soon as a writer queued on `entities`.
    let mission: Mission = {
        let mut missions = s.missions.write().await;
        let mission = missions
            .iter_mut()
            .find(|m| m.id == id && entity_ids.contains(&m.entity_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        mission.updated_at = now_rfc3339.clone();
        match &body.action {
            MissionActionType::ReviewNow => {
                mission.last_reviewed_at = Some(now_rfc3339.clone());
            }
            MissionActionType::SnoozeReview => {
                if let Some((_, until)) = snooze {
                    mission.last_reviewed_at = Some(until.to_rfc3339());
                }
            }
            MissionActionType::AcceptProposedNextStep | MissionActionType::DismissNoAction => {}
            MissionActionType::MarkUserInputNeeded => {
                mission.status = MissionStatus::UserInputNeeded;
            }
        }
        mission.clone()
    };
    s.persist_mission(&mission).await;

    match &body.action {
        MissionActionType::ReviewNow => {
            s.push_mission_feed(
                OperatorFeedKind::MissionReviewDue,
                &mission,
                Some("review_action=review_now".to_string()),
            )
            .await;
        }
        MissionActionType::SnoozeReview => {
            let hours = snooze.map(|(h, _)| h).unwrap_or(24);
            s.push_mission_feed(
                OperatorFeedKind::MissionSnoozed,
                &mission,
                Some(format!("review_action=snooze_review hours={hours}")),
            )
            .await;
            let branch = latest_recommendation_branch(s, &mission.id).await;
            s.increment_recommendation_metric("snoozed", branch.as_deref(), None)
                .await;
        }
        MissionActionType::AcceptProposedNextStep => {
            let task = Task {
                id: Uuid::new_v4().to_string(),
                title: format!("Execute next mission step: {}", mission.title),
                dept: "General".to_string(),
                roles: vec![],
                status: TaskStatus::Queued,
                tries: "0".to_string(),
                progress: 0,
                entity_id: Some(mission.entity_id.clone()),
                mission_id: Some(mission.id.clone()),
                priority: Some(mission.priority.clone()),
                requires_human_approval: false,
                assigned_director_id: None,
                assigned_agent_id: None,
                connection_id: None,
                external_source: None,
                external_id: None,
                result: Some(
                    body.note
                        .clone()
                        .unwrap_or_else(|| "Accepted from mission recommendation".to_string()),
                ),
                task_kind: Some("mission-next-step".to_string()),
                metadata: None,
                outcome: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                due_at: None,
                completed_at: None,
                status_changed_at: None,
                source_task_id: None,
            };
            // Statement-scoped guard: dropped before the write-through below.
            s.tasks.write().await.push(task.clone());
            s.persist_task_row(&task).await;
            s.push_operator_feed(
                OperatorFeedKind::TaskCreated,
                &task,
                Some("source=mission_action action=accept_proposed_next_step".to_string()),
            )
            .await;
            s.push_mission_feed(
                OperatorFeedKind::MissionRecommendationAccepted,
                &mission,
                Some(format!("accepted_task_id={}", task.id)),
            )
            .await;
            s.increment_mission_metric("recommendation_accepted").await;
            let branch = latest_recommendation_branch(s, &mission.id).await;
            s.increment_recommendation_metric(
                "accepted",
                branch.as_deref(),
                Some("mission-next-step"),
            )
            .await;
        }
        MissionActionType::DismissNoAction => {
            s.push_mission_feed(
                OperatorFeedKind::MissionRecommendationDismissed,
                &mission,
                Some(
                    body.note
                        .clone()
                        .unwrap_or_else(|| "action=dismiss_no_action".to_string()),
                ),
            )
            .await;
            s.increment_mission_metric("recommendation_dismissed").await;
            let branch = latest_recommendation_branch(s, &mission.id).await;
            s.increment_recommendation_metric("dismissed", branch.as_deref(), None)
                .await;
        }
        MissionActionType::MarkUserInputNeeded => {
            s.push_mission_feed(
                OperatorFeedKind::MissionUserInputNeeded,
                &mission,
                body.note
                    .clone()
                    .or_else(|| Some("status=user_input_needed".to_string())),
            )
            .await;
        }
    }
    Ok(Json(mission))
}
