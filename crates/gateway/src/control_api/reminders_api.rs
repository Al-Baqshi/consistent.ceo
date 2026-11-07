//! Reminders as their own surface: list what's open, act on one (done,
//! snooze, …), create one from a device that captured it offline, and tell
//! the gateway a phone scheduled the nudges itself.
//!
//! Every client renders the same [`crate::reminders::ReminderView`], including
//! the planned follow-up times, so a phone can schedule local notifications
//! that keep reminding with no network.
//!
//! Also serves `GET /api/local-model`: which on-device model the download
//! buttons should fetch.
//!
//! A child module of `control_api`: `use super::*` reaches its helpers.

use super::*;
use crate::reminders::{self, ReminderAction, ReminderState, ReminderView};

const SNOOZE_DEFAULT_MINUTES: f64 = 60.0;
const SNOOZE_MAX_MINUTES: f64 = 366.0 * 24.0 * 60.0;

#[derive(Debug, Default, Deserialize)]
pub(super) struct ListRemindersQuery {
    /// `open` (default) or `all`.
    #[serde(default)]
    include: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemindersResponse {
    items: Vec<ReminderView>,
    server_time: String,
}

async fn visible_entities(
    s: &crate::state::ControlState,
    workspace: &WorkspaceCtx,
) -> (HashSet<String>, HashMap<String, String>) {
    let entities = s.entities.read().await;
    let ids = entity_ids_for_workspace(&entities, &workspace.id, Some(&workspace.principal_id));
    let names = entities
        .iter()
        .filter(|e| ids.contains(&e.id))
        .map(|e| (e.id.clone(), e.name.clone()))
        .collect();
    (ids, names)
}

fn view_of(task: &Task, names: &HashMap<String, String>) -> ReminderView {
    reminders::view(
        task,
        task.entity_id.as_ref().and_then(|e| names.get(e).cloned()),
    )
}

pub(super) async fn list_reminders(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<ListRemindersQuery>,
) -> Result<Json<RemindersResponse>, StatusCode> {
    let s = control(&state)?;
    let all = q.include.as_deref() == Some("all");
    let (ids, names) = visible_entities(s, &workspace).await;
    let tasks = s.tasks.read().await;
    // Follow-ups land at each owner's learned hour; the phone schedules
    // these exact times, so they must match what the ticker will do.
    let hours = reminders::preferred_hours(&tasks);
    let mut items: Vec<ReminderView> = tasks
        .iter()
        .filter(|t| reminders::is_reminder_like(t))
        .filter(|t| task_visible_in_workspace(t, &ids, &workspace.id))
        .filter(|t| {
            all || !matches!(
                reminders::reminder_state(t),
                ReminderState::Done | ReminderState::Cancelled
            )
        })
        .map(|t| {
            let hour = t
                .entity_id
                .as_ref()
                .and_then(|e| hours.get(e).copied())
                .unwrap_or(reminders::MORNING_HOUR);
            reminders::view_at(
                t,
                t.entity_id.as_ref().and_then(|e| names.get(e).cloned()),
                hour,
            )
        })
        .collect();
    // Soonest first; nothing-scheduled (parked) at the end.
    items.sort_by(|a, b| match (&a.fire_at, &b.fire_at) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.created_at.cmp(&a.created_at),
    });
    Ok(Json(RemindersResponse {
        items,
        server_time: Utc::now().to_rfc3339(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateReminderRequest {
    #[serde(default)]
    what: String,
    /// One captured sentence ("send ABT the photo tomorrow at 9am"); the
    /// gateway splits out the time. Used when `what` is empty — the path for
    /// Siri Shortcuts, share sheets, widgets, and `consistent://remind` links.
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    in_minutes: Option<f64>,
    /// Absolute fire time, e.g. resolved on the phone while offline.
    #[serde(default)]
    fire_at: Option<String>,
    #[serde(default)]
    entity_id: Option<String>,
    /// Device-side id; repeating a create with the same id is a no-op.
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    tz_offset_minutes: Option<i32>,
    /// `false` = fire once, no follow-ups.
    #[serde(default)]
    follow_up: Option<bool>,
}

fn resolve_time(
    in_minutes: Option<f64>,
    when: Option<&str>,
    fire_at: Option<&str>,
    tz: Option<i32>,
) -> Option<chrono::DateTime<Utc>> {
    let now = Utc::now();
    fire_at
        .and_then(|f| chrono::DateTime::parse_from_rfc3339(f.trim()).ok())
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            in_minutes
                .filter(|m| m.is_finite() && *m >= 0.0 && *m <= SNOOZE_MAX_MINUTES)
                .map(|m| now + chrono::Duration::seconds((m * 60.0).round() as i64))
        })
        .or_else(|| when.and_then(|w| reminders::parse_when_in_timezone(w, now, tz)))
}

pub(super) async fn create_reminder(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateReminderRequest>,
) -> Result<Json<ReminderView>, StatusCode> {
    let s = control(&state)?;
    let mut body = body;
    if body.what.trim().is_empty()
        && let Some(text) = body
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    {
        let (what, due) = reminders::split_when(text, Utc::now(), body.tz_offset_minutes);
        body.what = what;
        if body.fire_at.is_none() && body.in_minutes.is_none() && body.when.is_none() {
            // No time in the sentence: it still comes back, tomorrow morning.
            let due = due
                .unwrap_or_else(|| reminders::tomorrow_morning(Utc::now(), body.tz_offset_minutes));
            body.fire_at = Some(due.to_rfc3339());
        }
    }
    let what = body.what.trim().to_string();
    if what.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let client_id = body
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty() && c.len() <= 128)
        .map(str::to_string);
    let (ids, names) = visible_entities(s, &workspace).await;

    if let Some(cid) = client_id.as_deref() {
        let tasks = s.tasks.read().await;
        if let Some(existing) = tasks.iter().find(|t| {
            crate::state::task_meta_str(t, reminders::CLIENT_ID_KEY).as_deref() == Some(cid)
                && task_visible_in_workspace(t, &ids, &workspace.id)
        }) {
            return Ok(Json(view_of(existing, &names)));
        }
    }

    // No entity given: the caller's Personal Space, like loose chat.
    let entity_id = match body.entity_id.filter(|e| !e.trim().is_empty()) {
        Some(e) => Some(e),
        None => {
            s.ensure_personal_space(&workspace.principal_id, &workspace.id)
                .await;
            let entities = s.entities.read().await;
            entities
                .iter()
                .find(|e| {
                    e.is_personal
                        && e.personal_user_id.as_deref() == Some(workspace.principal_id.as_str())
                        && entity_workspace_id(e) == workspace.id
                })
                .map(|e| e.id.clone())
        }
    };

    let due = resolve_time(
        body.in_minutes,
        body.when.as_deref(),
        body.fire_at.as_deref(),
        body.tz_offset_minutes,
    );
    let mut metadata = due
        .map(|d| reminders::schedule_metadata(d, body.tz_offset_minutes))
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = metadata.as_object_mut() {
        if let Some(cid) = &client_id {
            obj.insert(reminders::CLIENT_ID_KEY.into(), serde_json::json!(cid));
        }
        if body.follow_up == Some(false) {
            obj.insert(reminders::FOLLOW_UP_KEY.into(), serde_json::json!(false));
        }
    }
    let title = match body
        .when
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        Some(w) => format!("Reminder ({w}): {what}"),
        None => format!("Reminder: {what}"),
    };
    let task = internal_create_task(
        s,
        &workspace.id,
        Some(&workspace.principal_id),
        CreateTaskRequest {
            title,
            dept: Some("General".into()),
            roles: None,
            entity_id,
            mission_id: None,
            priority: None,
            requires_human_approval: None,
            task_kind: Some("reminder".into()),
            content: Some(what),
            director_id: Some("operations".into()),
            agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            metadata: Some(metadata),
            due_at: due.map(|d| d.to_rfc3339()),
        },
    )
    .await?;
    let names = visible_entities(s, &workspace).await.1;
    Ok(Json(view_of(&task, &names)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReminderActionRequest {
    /// `done` | `cancel` | `snooze` | `reschedule` | `reopen` | `delegate`
    action: String,
    /// `delegate` only: extra detail for the computer agent.
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    minutes: Option<f64>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    fire_at: Option<String>,
    #[serde(default)]
    tz_offset_minutes: Option<i32>,
}

/// Turn an action request into a [`ReminderAction`]; `Err` = bad request.
fn parse_action(body: &ReminderActionRequest) -> Result<ReminderAction, StatusCode> {
    match body.action.trim().to_ascii_lowercase().as_str() {
        "done" | "complete" => Ok(ReminderAction::Done),
        "cancel" | "dismiss" | "drop" => Ok(ReminderAction::Cancel),
        "snooze" => {
            let at = resolve_time(
                Some(body.minutes.unwrap_or(SNOOZE_DEFAULT_MINUTES)),
                None,
                None,
                None,
            );
            let at = if body.when.is_some() || body.fire_at.is_some() {
                resolve_time(
                    None,
                    body.when.as_deref(),
                    body.fire_at.as_deref(),
                    body.tz_offset_minutes,
                )
            } else {
                at
            };
            at.map(ReminderAction::At).ok_or(StatusCode::BAD_REQUEST)
        }
        "reschedule" | "reopen" => resolve_time(
            body.minutes,
            body.when.as_deref(),
            body.fire_at.as_deref(),
            body.tz_offset_minutes,
        )
        .or_else(|| (body.action == "reopen").then(|| Utc::now() + chrono::Duration::hours(1)))
        .map(ReminderAction::At)
        .ok_or(StatusCode::BAD_REQUEST),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

pub(super) async fn reminder_action(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<ReminderActionRequest>,
) -> Result<Json<ReminderView>, StatusCode> {
    let s = control(&state)?;
    if body.action.trim().eq_ignore_ascii_case("delegate") {
        return delegate_reminder(s, &workspace, &id, body.instruction.as_deref()).await;
    }
    let action = parse_action(&body)?;
    let (ids, names) = visible_entities(s, &workspace).await;
    let updated = {
        let mut tasks = s.tasks.write().await;
        let t = tasks
            .iter_mut()
            .find(|t| t.id == id && task_visible_in_workspace(t, &ids, &workspace.id))
            .ok_or(StatusCode::NOT_FOUND)?;
        reminders::apply_action(t, &action, body.tz_offset_minutes);
        t.clone()
    };
    crate::training::record_outcome(&updated, &body.action.trim().to_ascii_lowercase(), "api");
    s.persist_task_row(&updated).await;
    let detail = match &action {
        ReminderAction::Done => "Done".to_string(),
        ReminderAction::Cancel => "Dropped".to_string(),
        ReminderAction::At(d) => format!("Next nudge {}", d.to_rfc3339()),
    };
    // Timeline detail, not an interruption: TaskRetryScheduled isn't pushed.
    s.push_operator_feed(OperatorFeedKind::TaskRetryScheduled, &updated, Some(detail))
        .await;
    Ok(Json(view_of(&updated, &names)))
}

/// "Do it for me": hand the reminder to the owner's computer as an
/// approval-gated task. The reminder stops nudging until that task finishes;
/// the reminder ticker then closes it ("Done for you") or revives it.
async fn delegate_reminder(
    s: &crate::state::ControlState,
    workspace: &WorkspaceCtx,
    id: &str,
    extra: Option<&str>,
) -> Result<Json<ReminderView>, StatusCode> {
    let (ids, names) = visible_entities(s, workspace).await;
    let (instruction, entity_id) = {
        let tasks = s.tasks.read().await;
        let t = tasks
            .iter()
            .find(|t| {
                t.id == id
                    && reminders::is_reminder_like(t)
                    && task_visible_in_workspace(t, &ids, &workspace.id)
            })
            .ok_or(StatusCode::NOT_FOUND)?;
        if matches!(
            reminders::reminder_state(t),
            ReminderState::Done | ReminderState::Cancelled | ReminderState::Delegated
        ) {
            return Err(StatusCode::CONFLICT);
        }
        (
            reminders::delegation_instruction(t, extra),
            t.entity_id.clone().ok_or(StatusCode::BAD_REQUEST)?,
        )
    };
    let (task_id, _, approval_id) = crate::chat_tools::queue_machine_action(
        s,
        &workspace.id,
        &workspace.principal_id,
        &entity_id,
        &instruction,
        "general",
        "reminder",
        Some(id),
    )
    .await?;
    let updated = {
        let mut tasks = s.tasks.write().await;
        let t = tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(StatusCode::NOT_FOUND)?;
        reminders::mark_delegated(t, &task_id, &approval_id);
        t.clone()
    };
    crate::training::record_outcome(&updated, "delegate", "api");
    s.persist_task_row(&updated).await;
    Ok(Json(view_of(&updated, &names)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalScheduledRequest {
    /// `{ id, fireAt }` for every reminder the phone scheduled locally.
    items: Vec<LocalScheduledItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalScheduledItem {
    id: String,
    fire_at: String,
}

/// A phone scheduled these nudges as local notifications; when the gateway
/// fires the same one it skips the push so the phone doesn't ring twice.
pub(super) async fn reminders_local_scheduled(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<LocalScheduledRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = control(&state)?;
    let (ids, _) = visible_entities(s, &workspace).await;
    let mut marked = 0usize;
    let mut tasks = s.tasks.write().await;
    for item in body.items.iter().take(500) {
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(item.fire_at.trim()) else {
            continue;
        };
        if let Some(t) = tasks
            .iter_mut()
            .find(|t| t.id == item.id && task_visible_in_workspace(t, &ids, &workspace.id))
        {
            crate::state::task_meta_set_str(
                t,
                reminders::LOCAL_SCHEDULED_KEY,
                &at.with_timezone(&Utc).to_rfc3339(),
            );
            marked += 1;
        }
    }
    Ok(Json(serde_json::json!({ "ok": true, "marked": marked })))
}

/// The model the "download" buttons fetch. Desktop pulls `ollama` through
/// `POST /api/ollama/pull`; phones download `mobile.url` directly.
pub(super) async fn get_local_model() -> Json<serde_json::Value> {
    let pick = |key: &str, default: &str| -> String {
        std::env::var(key)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    Json(serde_json::json!({
        "name": "Consistent AI",
        "ollama": {
            "model": pick("CONSISTENT_LOCAL_MODEL", super::ollama_api::DEFAULT_LOCAL_CHAT_MODEL),
            "approxBytes": 7_200_000_000u64,
        },
        "mobile": {
            "file": pick("CONSISTENT_MOBILE_MODEL_FILE", MOBILE_MODEL_FILE),
            "url": pick("CONSISTENT_MOBILE_MODEL_URL", MOBILE_MODEL_URL),
            "approxBytes": MOBILE_MODEL_BYTES,
            "minRamGb": 6,
            "format": "gguf",
        },
    }))
}

/// Default phone model: Gemma 4 E2B, 4-bit GGUF, text only. Swapped for the
/// fine-tuned `consistent-gemma` once it is published (env overrides above).
pub(crate) const MOBILE_MODEL_FILE: &str = "gemma-4-E2B-it-Q4_K_M.gguf";
pub(crate) const MOBILE_MODEL_URL: &str =
    "https://huggingface.co/unsloth/gemma-4-E2B-it-GGUF/resolve/main/gemma-4-E2B-it-Q4_K_M.gguf";
pub(crate) const MOBILE_MODEL_BYTES: u64 = 3_100_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn req(action: &str) -> ReminderActionRequest {
        ReminderActionRequest {
            action: action.into(),
            instruction: None,
            minutes: None,
            when: None,
            fire_at: None,
            tz_offset_minutes: None,
        }
    }

    #[test]
    fn snooze_defaults_to_an_hour() {
        let before = Utc::now();
        match parse_action(&req("snooze")).unwrap() {
            ReminderAction::At(d) => {
                let mins = (d - before).num_minutes();
                assert!((59..=60).contains(&mins), "got {mins}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn done_cancel_and_bad_actions() {
        assert_eq!(parse_action(&req("done")).unwrap(), ReminderAction::Done);
        assert_eq!(parse_action(&req("drop")).unwrap(), ReminderAction::Cancel);
        assert!(parse_action(&req("explode")).is_err());
        assert!(
            parse_action(&req("reschedule")).is_err(),
            "reschedule needs a time"
        );
    }
}
