//! Mission planner: one LLM call that turns a mission goal into concrete tasks.
//!
//! This replaces the keyword-template "decomposition" that produced tasks named
//! `"{Mission} - backend execution plan"` regardless of what the mission was
//! actually about. Those templates survive as [`fallback_tasks`]: the planner
//! degrades to them whenever the LLM errors, answers empty, or returns something
//! unparseable, so mission creation never depends on a reachable model — the same
//! shape as the canned-heartbeat fallback in `heartbeat_dispatcher`.
//!
//! The prompt is deliberately small. Heavy prompts come back empty on the local
//! Ollama fallback tier, which would silently reduce every plan to the templates.

use std::sync::Arc;

use consistent_world_llm::client::{ChatMessage, CompletionRequest, Role};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::capture::{build_llm_client, resolve_operations_llm};
use crate::credentials_resolver::resolve_llm_api_key;
use crate::orchestrator::mission_autorun_block_reason;
use crate::state::{
    ControlState, Entity, Mission, OperatorFeedKind, ServerState, Task, TaskStatus,
};

/// Upper bound on tasks accepted from one plan. A model that ignores the range
/// and emits thirty items would bury the operator, so the tail is dropped.
const MAX_PLANNED_TASKS: usize = 8;
/// Days. Anything beyond this is a hallucinated horizon, not a deadline.
const MAX_DUE_OFFSET_DAYS: u32 = 90;
/// Staggered deadlines for the template fallback, which carries no timing of its own.
const FALLBACK_DUE_OFFSETS: [u32; 5] = [2, 4, 7, 10, 14];

/// Ceiling on the planning call. Long enough for a cold `claude` CLI start,
/// short enough that a stuck local model still yields a plan the same minute.
const PLANNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// One task as proposed by the planner. Every field is serde-defaulted and
/// aliased: a sloppy reply that omits flags or uses camelCase still lands
/// rather than collapsing the whole array.
#[derive(Debug, Clone, Deserialize)]
pub struct PlannedTask {
    #[serde(default, alias = "name")]
    pub title: String,
    #[serde(default, alias = "detail", alias = "details", alias = "summary")]
    pub description: String,
    #[serde(
        default,
        alias = "dueOffsetDays",
        alias = "due_in_days",
        alias = "dueInDays",
        alias = "offset_days"
    )]
    pub due_offset_days: u32,
    #[serde(default, alias = "isExecutable")]
    pub executable: bool,
    #[serde(
        default,
        alias = "requiresApproval",
        alias = "needs_approval",
        alias = "needsApproval"
    )]
    pub requires_approval: bool,
    /// Routing kind, carried only by the template fallback so the historical
    /// `mission-*` kinds keep matching `task_kind_routing`. The LLM never sets it.
    #[serde(default)]
    pub task_kind: Option<String>,
}

/// The whole planner prompt. Kept under ~300 words on purpose — see module docs.
pub fn build_planner_prompt(
    mission_title: &str,
    mission_goal: &str,
    entity_name: &str,
    entity_narrative: Option<&str>,
) -> String {
    let goal = if mission_goal.trim().is_empty() {
        mission_title.trim()
    } else {
        mission_goal.trim()
    };
    let mut prompt = format!(
        "You are the mission planner for {entity}. Break the mission below into 3-8 concrete next tasks.\n\n\
Mission: {title}\n\
Goal: {goal}\n",
        entity = entity_name.trim(),
        title = mission_title.trim(),
        goal = goal,
    );
    if let Some(context) = entity_narrative.map(str::trim).filter(|v| !v.is_empty()) {
        let truncated: String = context.chars().take(600).collect();
        prompt.push_str(&format!("Context: {truncated}\n"));
    }
    prompt.push_str(
        "\nRules:\n\
- Each task is one specific, checkable piece of work. Name the real artifact or step; never a generic label like \"planning\" or \"execution plan\".\n\
- Order them so earlier tasks unblock later ones.\n\
- due_offset_days: whole days from today, 1 to 30, staggered across the list.\n\
- executable: true only for code or file work an unattended coding agent could finish on its own inside the project repo. Everything else is false.\n\
- requires_approval: true for anything outward-facing — sending, publishing, paying, posting, or contacting a person. Otherwise false.\n\
\n\
Reply with ONLY the JSON array. No prose, no markdown fences.\n\
[{\"title\":\"...\",\"description\":\"...\",\"due_offset_days\":2,\"executable\":false,\"requires_approval\":false}]\n",
    );
    prompt
}

/// Pull a task list out of whatever the model said. Tolerant by design: this is
/// the planner's only failure surface, and a parse miss costs a real plan.
pub fn parse_planned_tasks(raw: &str) -> Option<Vec<PlannedTask>> {
    let cleaned = strip_markdown_fences(raw);
    let start = cleaned.find('[')?;
    let end = cleaned.rfind(']')?;
    if end < start {
        return None;
    }
    // Element-at-a-time so one malformed entry (a float offset, a null title)
    // does not discard the tasks around it.
    let values: Vec<serde_json::Value> = serde_json::from_str(&cleaned[start..=end]).ok()?;
    let mut planned = Vec::new();
    for value in values {
        let Ok(mut task) = serde_json::from_value::<PlannedTask>(value) else {
            continue;
        };
        task.title = task.title.trim().to_string();
        if task.title.is_empty() {
            continue;
        }
        task.description = task.description.trim().to_string();
        task.due_offset_days = task.due_offset_days.min(MAX_DUE_OFFSET_DAYS);
        planned.push(task);
        if planned.len() == MAX_PLANNED_TASKS {
            break;
        }
    }
    if planned.is_empty() {
        None
    } else {
        Some(planned)
    }
}

// Drop ```/```json fence lines a chatty model wraps the JSON array in.
fn strip_markdown_fences(raw: &str) -> String {
    raw.lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n")
}

// One candidate task in the keyword-template fallback plan.
#[derive(Debug, Clone)]
struct StarterWorkstream {
    label: &'static str,
    task_kind: &'static str,
    rationale: &'static str,
    keywords: &'static [&'static str],
}

// Score every candidate workstream against the mission's title/goal text and
// return them ranked, most relevant first.
fn mission_starter_workstreams(title: &str, goal: &str) -> Vec<StarterWorkstream> {
    let mut items = vec![
        StarterWorkstream {
            label: "architecture decision map",
            task_kind: "mission-architecture",
            rationale: "Create boundaries and sequencing before implementation tasks.",
            keywords: &["architecture", "design", "system", "plan"],
        },
        StarterWorkstream {
            label: "backend execution plan",
            task_kind: "mission-backend-plan",
            rationale: "Define API, service and data flows for reliable delivery.",
            keywords: &["backend", "api", "service", "rust", "database"],
        },
        StarterWorkstream {
            label: "frontend execution plan",
            task_kind: "mission-frontend-plan",
            rationale: "Define UI states and integration points to prevent rework.",
            keywords: &["frontend", "ui", "svelte", "mobile", "ux"],
        },
        StarterWorkstream {
            label: "data model baseline",
            task_kind: "mission-data-model",
            rationale: "Lock key entities/contracts early to avoid downstream blockers.",
            keywords: &["schema", "data", "model", "contract", "state"],
        },
    ];
    let hay = format!(
        "{} {}",
        title.to_ascii_lowercase(),
        goal.to_ascii_lowercase()
    );
    if hay.contains("mobile") || hay.contains("ios") || hay.contains("android") {
        items.push(StarterWorkstream {
            label: "mobile platform plan",
            task_kind: "mission-mobile-plan",
            rationale: "Align mobile scope with backend/API contract before build.",
            keywords: &["mobile", "ios", "android", "touch", "responsive"],
        });
    }
    if hay.contains("auth") || hay.contains("security") {
        items.push(StarterWorkstream {
            label: "auth and security baseline",
            task_kind: "mission-auth-security",
            rationale: "Address identity, permissions and security constraints early.",
            keywords: &["auth", "security", "permission", "access", "policy"],
        });
    }
    if hay.contains("deploy") || hay.contains("infra") || hay.contains("launch") {
        items.push(StarterWorkstream {
            label: "infrastructure and deployment path",
            task_kind: "mission-infra-deploy",
            rationale: "Ensure execution path to production is defined up front.",
            keywords: &["infra", "deploy", "release", "ops", "production"],
        });
    }
    items.push(StarterWorkstream {
        label: "milestone review gate",
        task_kind: "mission-milestone-review",
        rationale: "Create explicit review checkpoints to maintain momentum and quality.",
        keywords: &["review", "milestone", "checkpoint"],
    });
    // A short title/goal signals a tight mission, where a whole architecture
    // pass is likely overkill.
    let narrow_scope = hay.split_whitespace().count() <= 8 && goal.split_whitespace().count() <= 12;
    let mut scored: Vec<(i32, StarterWorkstream)> = items
        .into_iter()
        .map(|w| {
            let mut score = 1;
            for kw in w.keywords {
                if hay.contains(kw) {
                    score += 3;
                }
            }
            if narrow_scope && w.task_kind == "mission-architecture" {
                score -= 1;
            }
            (score, w)
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, w)| w).collect()
}

/// Tight missions get fewer starter workstreams — the pre-LLM heuristic, preserved.
fn starter_limit(title: &str, goal: &str) -> usize {
    if title.split_whitespace().count() <= 6 && goal.split_whitespace().count() <= 10 {
        3
    } else {
        5
    }
}

/// The keyword templates, as tasks. Never executable and never approval-gated:
/// they describe planning work, and a template has no standing to claim either.
fn fallback_tasks(title: &str, goal: &str) -> Vec<PlannedTask> {
    mission_starter_workstreams(title, goal)
        .into_iter()
        .take(starter_limit(title, goal))
        .enumerate()
        .map(|(index, workstream)| PlannedTask {
            title: format!("{} - {}", title, workstream.label),
            description: workstream.rationale.to_string(),
            due_offset_days: FALLBACK_DUE_OFFSETS
                .get(index)
                .copied()
                .unwrap_or(*FALLBACK_DUE_OFFSETS.last().unwrap()),
            executable: false,
            requires_approval: false,
            task_kind: Some(workstream.task_kind.to_string()),
        })
        .collect()
}

/// Profile/goals/active-context, folded into one grounding blob. Mirrors what
/// `run_agent::augment_system_with_entity_narrative` injects for agent runs.
fn entity_narrative_snippet(entity: &Entity) -> Option<String> {
    let mut parts = Vec::new();
    for (label, value) in [
        ("Profile", &entity.profile),
        ("Goals", &entity.goals),
        ("Active context", &entity.active_context),
    ] {
        if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            parts.push(format!("{label}: {v}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

// Ask the configured LLM for a plan; any failure (no config, timeout, empty
// or unparseable reply) returns None so the caller falls back to templates.
async fn plan_with_llm(
    state: &ServerState,
    control: &Arc<ControlState>,
    mission: &Mission,
    entity_name: &str,
    entity_narrative: Option<&str>,
) -> Option<Vec<PlannedTask>> {
    // No loaded config means no configured brain: planning is pure CPU and the
    // caller can run it inline. Never reach for ANTHROPIC_API_KEY here — the
    // resolution below routes the subscription through the `claude` CLI.
    let config = state.app_config.as_ref()?;
    let resolved = resolve_operations_llm(config, control).await?;
    let api_key = resolve_llm_api_key(&resolved.api_key_env);
    let provider = build_llm_client(config, &resolved.base_url, &api_key, &resolved.api_key_env);

    let prompt = build_planner_prompt(&mission.title, &mission.goal, entity_name, entity_narrative);
    let request = CompletionRequest {
        model: resolved.model,
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        max_tokens: Some(1200),
        temperature: Some(0.3),
        stream: false,
    };

    // A local model on a CPU-only machine can grind on this prompt for many
    // minutes. Without a ceiling the spawned planner never returns and the
    // mission sits at zero tasks forever, which reads to the user as "the
    // product silently did nothing" — strictly worse than template fallback.
    let response = match tokio::time::timeout(PLANNER_TIMEOUT, provider.complete(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            tracing::warn!(mission_id = %mission.id, error = %e, "mission planner LLM failed; using template fallback");
            return None;
        }
        Err(_) => {
            tracing::warn!(
                mission_id = %mission.id,
                timeout_secs = PLANNER_TIMEOUT.as_secs(),
                "mission planner timed out; using template fallback"
            );
            return None;
        }
    };
    let raw = response.content.unwrap_or_default();
    if raw.trim().is_empty() {
        tracing::warn!(mission_id = %mission.id, "mission planner returned an empty reply; using template fallback");
        return None;
    }
    let parsed = parse_planned_tasks(&raw);
    if parsed.is_none() {
        tracing::warn!(mission_id = %mission.id, "mission planner reply did not parse as a task array; using template fallback");
    }
    parsed
}

/// Plan the mission and attach the resulting tasks. Awaitable on purpose: the
/// HTTP handler spawns it so `POST /api/missions` stays fast (the Claude CLI
/// provider has a 1-3s floor), while tests and the chat tool await it directly.
pub async fn plan_and_attach_tasks(
    state: &ServerState,
    control: &Arc<ControlState>,
    mission: &Mission,
) {
    let (entity_name, narrative, dept) = {
        let entities = control.entities.read().await;
        match entities.iter().find(|e| e.id == mission.entity_id) {
            Some(entity) => (
                entity.name.clone(),
                entity_narrative_snippet(entity),
                if entity.dept.is_empty() {
                    "General".to_string()
                } else {
                    entity.dept.clone()
                },
            ),
            None => (mission.entity_id.clone(), None, "General".to_string()),
        }
    };

    let (planned, planner) = match plan_with_llm(
        state,
        control,
        mission,
        &entity_name,
        narrative.as_deref(),
    )
    .await
    {
        Some(tasks) => (tasks, "llm"),
        // A mission that drives itself must never be seeded with invented
        // work. The templates are generic by construction ("frontend
        // execution plan" for a Python script), non-executable, and can
        // never satisfy a definition of done — but they *are* unfinished
        // tasks, so `loop_gate` reads them as work in flight and pins the
        // whole loop at Busy behind them, permanently.
        //
        // The replanner already refuses to invent work on a degraded model
        // (it returns None and the loop waits a beat). Seeding fallbacks
        // here would undo that guarantee before the loop even starts, so a
        // loop mission gets no plan at all and its first Drive beat does
        // the planning once the model is reachable.
        None if mission.loop_config.is_some() => {
            tracing::warn!(
                mission_id = %mission.id,
                "mission planner unavailable for a loop-enabled mission; seeding nothing so the drive beat can plan it properly"
            );
            control
                .push_mission_feed(
                    OperatorFeedKind::GoalLoopDecision,
                    mission,
                    Some(
                        "decision=waiting reason=planner_unavailable \
                             detail=No plan was invented; the drive beat will plan this mission \
                             when the model is reachable."
                            .to_string(),
                    ),
                )
                .await;
            return;
        }
        None => (fallback_tasks(&mission.title, &mission.goal), "fallback"),
    };

    let now = chrono::Utc::now();
    let tasks: Vec<Task> = planned
        .into_iter()
        .map(|p| Task {
            id: Uuid::new_v4().to_string(),
            title: p.title.clone(),
            dept: dept.clone(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some(mission.entity_id.clone()),
            mission_id: Some(mission.id.clone()),
            priority: Some(mission.priority.clone()),
            requires_human_approval: p.requires_approval,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: Some(format!(
                "source=mission_creation_auto_queue\nreason=mission_plan\nplanner={}\ndescription={}",
                planner, p.description
            )),
            task_kind: Some(
                p.task_kind
                    .clone()
                    .unwrap_or_else(|| "mission-plan".to_string()),
            ),
            metadata: Some(json!({
                "planner": planner,
                "executable": p.executable,
                "description": p.description,
            })),
            outcome: None,
            created_at: now.to_rfc3339(),
            due_at: Some((now + chrono::Duration::days(p.due_offset_days as i64)).to_rfc3339()),
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        })
        .collect();

    {
        let mut all = control.tasks.write().await;
        all.extend(tasks.iter().cloned());
    }
    tracing::info!(
        mission_id = %mission.id,
        planner,
        task_count = tasks.len(),
        "mission plan attached"
    );
    for task in &tasks {
        // Write-through before the feed event, matching internal_create_task.
        control.persist_task_row(task).await;
        control
            .push_operator_feed(OperatorFeedKind::TaskCreated, task, None)
            .await;
    }

    let all_tasks = control.tasks.read().await.clone();
    let default_cfg;
    let app_cfg = if let Some(cfg) = state.app_config.as_ref() {
        cfg
    } else {
        default_cfg = crate::config::AppConfig::default();
        &default_cfg
    };
    if let Some(reason) = mission_autorun_block_reason(control, app_cfg, mission, &all_tasks).await
    {
        control
            .push_mission_feed(
                OperatorFeedKind::MissionStalled,
                mission,
                Some(format!(
                    "reason=autorun_blocked\naction=mission_create\nblockers={}\nstarter_task_count={}",
                    reason,
                    tasks.len()
                )),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_the_instructions_the_parser_depends_on() {
        let prompt = build_planner_prompt(
            "Ship the beta",
            "Get ten paying users on the beta",
            "Acme",
            Some("Profile: solo founder"),
        );
        assert!(prompt.contains("Acme"));
        assert!(prompt.contains("Ship the beta"));
        assert!(prompt.contains("Get ten paying users on the beta"));
        assert!(prompt.contains("Profile: solo founder"));
        assert!(prompt.contains("3-8"));
        assert!(prompt.contains("due_offset_days"));
        assert!(prompt.contains("executable"));
        assert!(prompt.contains("requires_approval"));
        assert!(prompt.contains("ONLY the JSON array"));
        assert!(
            prompt.split_whitespace().count() < 400,
            "planner prompt must stay modest; the local fallback tier returns empty on heavy prompts"
        );
    }

    #[test]
    fn prompt_falls_back_to_the_title_when_the_goal_is_blank() {
        let prompt = build_planner_prompt("Ship the beta", "   ", "Acme", None);
        assert!(prompt.contains("Goal: Ship the beta"));
        assert!(!prompt.contains("Context:"));
    }

    #[test]
    fn parses_a_plain_json_array() {
        let tasks = parse_planned_tasks(
            r#"[{"title":"Write the pricing page","description":"Three tiers","due_offset_days":2,"executable":true,"requires_approval":false}]"#,
        )
        .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Write the pricing page");
        assert_eq!(tasks[0].description, "Three tiers");
        assert_eq!(tasks[0].due_offset_days, 2);
        assert!(tasks[0].executable);
        assert!(!tasks[0].requires_approval);
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n[{\"title\":\"A\"},{\"title\":\"B\"}]\n```";
        let tasks = parse_planned_tasks(raw).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn parses_prose_wrapped_json() {
        let raw = "Sure! Here is the plan:\n[{\"title\":\"A\"}]\nLet me know if you want changes.";
        let tasks = parse_planned_tasks(raw).unwrap();
        assert_eq!(tasks[0].title, "A");
    }

    #[test]
    fn accepts_camel_case_aliases() {
        let raw = r#"[{"title":"Email the list","dueOffsetDays":5,"requiresApproval":true}]"#;
        let tasks = parse_planned_tasks(raw).unwrap();
        assert_eq!(tasks[0].due_offset_days, 5);
        assert!(tasks[0].requires_approval);
    }

    #[test]
    fn missing_optional_fields_default() {
        let tasks = parse_planned_tasks(r#"[{"title":"Only a title"}]"#).unwrap();
        assert_eq!(tasks[0].description, "");
        assert_eq!(tasks[0].due_offset_days, 0);
        assert!(!tasks[0].executable);
        assert!(!tasks[0].requires_approval);
        assert!(tasks[0].task_kind.is_none());
    }

    #[test]
    fn drops_empty_titles_and_unparseable_entries() {
        let raw = r#"[{"title":"  "},{"title":null},"just a string",{"title":"Real work"}]"#;
        let tasks = parse_planned_tasks(raw).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Real work");
    }

    #[test]
    fn clamps_to_eight_tasks_and_a_sane_horizon() {
        let items: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"title":"T{i}","due_offset_days":9999}}"#))
            .collect();
        let tasks = parse_planned_tasks(&format!("[{}]", items.join(","))).unwrap();
        assert_eq!(tasks.len(), MAX_PLANNED_TASKS);
        assert!(
            tasks
                .iter()
                .all(|t| t.due_offset_days == MAX_DUE_OFFSET_DAYS)
        );
    }

    #[test]
    fn rejects_garbage_and_empty_arrays() {
        assert!(parse_planned_tasks("").is_none());
        assert!(parse_planned_tasks("I could not do that.").is_none());
        assert!(parse_planned_tasks("[]").is_none());
        assert!(parse_planned_tasks("[{\"title\":\"unterminated\"").is_none());
        assert!(parse_planned_tasks("see the docs [here] for details").is_none());
        assert!(parse_planned_tasks("] backwards [").is_none());
    }

    #[test]
    fn starter_workstreams_rank_relevant_items_first() {
        let ranked = mission_starter_workstreams(
            "Mobile auth hardening",
            "Improve mobile auth and permission flow",
        );
        let top_kinds: Vec<&str> = ranked.iter().take(2).map(|w| w.task_kind).collect();
        assert!(
            top_kinds.contains(&"mission-mobile-plan")
                || top_kinds.contains(&"mission-auth-security")
        );
    }

    #[test]
    fn fallback_preserves_the_old_starter_behavior() {
        // Narrow scope → 3 tasks; the pre-planner limit rule.
        let narrow = fallback_tasks("Ship beta", "Get users");
        assert_eq!(narrow.len(), 3);
        // Broad scope → 5.
        let broad = fallback_tasks(
            "Rebuild the mobile auth and deployment story end to end",
            "Ship a hardened mobile auth flow with a documented deployment path and review gates",
        );
        assert_eq!(broad.len(), 5);

        let expected = mission_starter_workstreams("Ship beta", "Get users");
        for (task, workstream) in narrow.iter().zip(expected.iter()) {
            assert_eq!(task.title, format!("Ship beta - {}", workstream.label));
            assert_eq!(task.description, workstream.rationale);
            assert_eq!(task.task_kind.as_deref(), Some(workstream.task_kind));
        }
        assert_eq!(
            narrow.iter().map(|t| t.due_offset_days).collect::<Vec<_>>(),
            vec![2, 4, 7]
        );
        assert!(narrow.iter().all(|t| !t.executable && !t.requires_approval));
    }
}
