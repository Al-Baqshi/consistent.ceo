//! Workspace tools the chat agent can call.
//!
//! Each tool wraps an existing `internal_*` action (create task/note, list entities, recall),
//! captured behind a `Tool` implementation so the LLM can choose when to call it.
//! All tools are workspace-scoped: each carries the active `workspace_id` and an
//! `Arc<ControlState>` so calls execute against the correct tenant.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use consistent_world_common::types::ToolResult;
use consistent_world_tools::Tool;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::control_api::{
    CreateEntityRequest, CreateMissionRequest, CreateTaskRequest, internal_create_entity,
    internal_create_mission, internal_create_task, internal_create_task_queued,
    internal_create_workspace,
};
use crate::state::{ApprovalRequest, ApprovalStatus, ControlState, ServerState, TaskStatus};

/// Result describing a side-effect a tool performed. Captured per chat turn and
/// surfaced back to the frontend as `ChatAction`s so the UI can render "Note saved",
/// "Task created", etc., alongside the natural reply.
#[derive(Debug, Clone)]
pub struct ToolEffect {
    pub action_type: String,
    pub description: String,
    pub result_id: Option<String>,
    pub status: String,
}

/// Tool: create a new top-level workspace/tenant from chat.
#[derive(Clone)]
pub struct CreateWorkspaceTool {
    pub state: ServerState,
    pub current_workspace_id: Arc<str>,
    /// Who is asking: the tenant store only lets an existing id be reused by
    /// its own primary user, so chat can create a workspace but never take
    /// over (rename) someone else's.
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct CreateWorkspaceArgs {
    name: String,
    #[serde(default)]
    id: Option<String>,
}

#[async_trait]
impl Tool for CreateWorkspaceTool {
    fn name(&self) -> &'static str {
        "create_workspace"
    }
    fn description(&self) -> &'static str {
        "Create a new top-level Consistent workspace/tenant. Use when the user explicitly asks for a new workspace, tenant, area, or separate space. Do not use this for normal notes, tasks, or project/entity creation."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Human-readable workspace name." },
                "id": { "type": "string", "description": "Optional stable slug/id. If omitted, the backend derives it from the name." }
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateWorkspaceArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("invalid args: {}", e) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        let name = args.name.trim();
        if name.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: r#"{"ok":false,"error":"workspace name was empty"}"#.to_string(),
                is_error: true,
            });
        }
        let id = args
            .id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(name)
            .to_string();
        match internal_create_workspace(&self.state, &id, name, Some(&self.principal_id)).await {
            Ok(workspace) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({
                    "ok": true,
                    "id": workspace.id,
                    "name": workspace.name,
                    "status": workspace.status,
                    "local_path": workspace.local_path,
                    "current_workspace": self.current_workspace_id.as_ref()
                })
                .to_string(),
                is_error: false,
            }),
            Err(status) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": format!("create_workspace failed: HTTP {}", status.as_u16()) })
                    .to_string(),
                is_error: true,
            }),
        }
    }
}

/// Create an entity from chat. Entities previously came only from the UI's
/// conversational onboarding, so a chat request like "create an entity for my
/// bakery" dead-ended with the model explaining it had no such tool — the
/// single most natural thing to ask a companion for. Wraps the same
/// [`internal_create_entity`] the HTTP endpoint uses, so enrichment, the local
/// vault folder, and the default heartbeat all behave exactly as if the entity
/// had been made in the UI.
pub struct CreateEntityTool {
    pub state: ServerState,
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
}

#[derive(Deserialize)]
struct CreateEntityArgs {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    department: Option<String>,
}

#[async_trait]
impl Tool for CreateEntityTool {
    fn name(&self) -> &'static str {
        "create_entity"
    }
    fn description(&self) -> &'static str {
        "Create a new entity — a project, business, or goal hub — in the current workspace. Use when the user asks to create, start, or set up a new entity/project/venture. Pass the user's stated goal or context as the description; it seeds the entity's profile. Not for plain notes or tasks, and not for a whole new workspace."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Entity name, e.g. \"Copper Kitchen\"." },
                "description": { "type": "string", "description": "What this entity is and what it is trying to achieve, in the user's words." },
                "department": { "type": "string", "description": "Optional grouping, e.g. \"Business\", \"Learning\"." }
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateEntityArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("invalid args: {}", e) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        if args.name.trim().is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: r#"{"ok":false,"error":"entity name was empty"}"#.to_string(),
                is_error: true,
            });
        }
        let body = CreateEntityRequest {
            name: args.name.trim().to_string(),
            description: args.description,
            department: args.department,
            owner: None,
            status: None,
            entity_type: None,
            default_workspace_root: None,
            stage: None,
            setup: None,
        };
        match internal_create_entity(&self.state, &self.control, self.workspace_id.as_ref(), body)
            .await
        {
            Ok(entity) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": true, "id": entity.id, "name": entity.name }).to_string(),
                is_error: false,
            }),
            Err(status) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": format!("create_entity failed: HTTP {}", status.as_u16()) })
                    .to_string(),
                is_error: true,
            }),
        }
    }
}

/// Tool: save a note as a task with kind "note".
#[derive(Clone)]
pub struct CreateNoteTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct CreateNoteArgs {
    #[serde(default)]
    content: String,
    #[serde(default)]
    title: Option<String>,
}

#[async_trait]
impl Tool for CreateNoteTool {
    fn name(&self) -> &'static str {
        "create_note"
    }
    fn description(&self) -> &'static str {
        "Save a note in the user's workspace. Use ONLY when the user explicitly asks to write down, capture, save, or remember something. Do NOT call this for normal chat or questions."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The full note text to save." },
                "title": { "type": "string", "description": "Optional short title; if omitted, derived from content." }
            },
            "required": ["content"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateNoteArgs = serde_json::from_value(arguments).unwrap_or(CreateNoteArgs {
            content: String::new(),
            title: None,
        });
        let body = args.content.trim();
        if body.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: r#"{"ok":false,"error":"content was empty; ask the user what to capture"}"#
                    .to_string(),
                is_error: true,
            });
        }
        let title = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| body.chars().take(80).collect());

        let req = CreateTaskRequest {
            title,
            dept: Some("General".to_string()),
            roles: None,
            entity_id: self.entity_id.as_ref().map(|e| e.to_string()),
            mission_id: None,
            priority: None,
            requires_human_approval: None,
            task_kind: Some("note".to_string()),
            content: Some(body.to_string()),
            director_id: Some("operations".to_string()),
            agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            metadata: None,
            due_at: None,
        };
        match internal_create_task(&self.control, &self.workspace_id, Some(&self.principal_id), req).await {
            Ok(task) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": true, "id": task.id, "kind": "note" }).to_string(),
                is_error: false,
            }),
            Err(status) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": format!("create_note failed: HTTP {}", status.as_u16()) })
                    .to_string(),
                is_error: true,
            }),
        }
    }
}

/// Tool: record a durable behavioral lesson (a correction, success, or
/// observation) so it shapes the director's future behavior.
#[derive(Clone)]
pub struct RecordLessonTool {
    pub state: ServerState,
    /// Scope for a lesson recorded with no entity active. Lessons used to land
    /// as `global`, which every workspace's chat recalled — one tenant could
    /// plant standing instructions in everyone else's prompts.
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct RecordLessonArgs {
    #[serde(default)]
    lesson: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    severity: Option<String>,
}

#[async_trait]
impl Tool for RecordLessonTool {
    fn name(&self) -> &'static str {
        "record_lesson"
    }
    fn description(&self) -> &'static str {
        "Record a durable behavioral lesson when the user corrects HOW you work (\"don't do X\", \"always/never Y\") or confirms an approach worked. Lessons shape future behavior across sessions. NOT for content the user wants saved — use create_note for that."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "lesson": { "type": "string", "description": "The behavioral rule, phrased as a standing instruction (e.g. \"Always confirm units before pricing\")." },
                "source": { "type": "string", "enum": ["correction", "success", "observation"], "description": "correction = user corrected a mistake; success = user confirmed an approach worked; observation = pattern noticed. Default: observation." },
                "severity": { "type": "string", "enum": ["hard", "soft"], "description": "hard = must always hold; soft = preference. Default: soft." }
            },
            "required": ["lesson"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: RecordLessonArgs =
            serde_json::from_value(arguments).unwrap_or(RecordLessonArgs {
                lesson: String::new(),
                source: None,
                severity: None,
            });
        if args.lesson.trim().is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: r#"{"ok":false,"error":"lesson was empty; state the behavioral rule to record"}"#
                    .to_string(),
                is_error: true,
            });
        }
        let Some(store) = self.state.director_lessons_store.clone() else {
            return Ok(ToolResult {
                call_id: String::new(),
                output: r#"{"ok":false,"error":"lesson store unavailable"}"#.to_string(),
                is_error: true,
            });
        };
        let source = args
            .source
            .as_deref()
            .filter(|s| matches!(*s, "correction" | "success" | "observation"))
            .unwrap_or("observation");
        let severity = args
            .severity
            .as_deref()
            .filter(|s| matches!(*s, "hard" | "soft"))
            .unwrap_or("soft");
        // Chat has no director concept yet; lessons land on the default
        // "operations" director like the other chat tools' tasks do.
        let scope = match self.entity_id.as_deref() {
            Some(eid) => format!("entity:{}", eid),
            None => format!("workspace:{}", self.workspace_id),
        };
        let embedder = self.state.embedding_provider.as_ref().map(|p| p.0.clone());
        match crate::lessons::record_lesson(
            store,
            embedder,
            "operations",
            &scope,
            &args.lesson,
            source,
            severity,
            Some(&self.principal_id),
        )
        .await
        {
            Ok(lesson) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": true, "id": lesson.id, "kind": "lesson" }).to_string(),
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": format!("record_lesson failed: {}", e) })
                    .to_string(),
                is_error: true,
            }),
        }
    }
}

/// Tool: create an actionable task in the workspace.
#[derive(Clone)]
pub struct CreateTaskTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct CreateTaskArgs {
    title: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    dept: Option<String>,
}

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &'static str {
        "create_task"
    }
    fn description(&self) -> &'static str {
        "Create an actionable task in the user's workspace. Use when the user asks to do, build, ship, follow up on, or schedule a piece of work. Provide a short imperative title."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title":    { "type": "string", "description": "Short imperative title, e.g. 'Draft landing page copy'." },
                "content":  { "type": "string", "description": "Optional longer description / acceptance criteria." },
                "priority": { "type": "string", "enum": ["low","normal","high","urgent"], "description": "Optional priority." },
                "dept":     { "type": "string", "description": "Optional department, e.g. 'Engineering', 'Marketing'. Defaults to General." }
            },
            "required": ["title"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateTaskArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("invalid args: {}", e) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        let req = CreateTaskRequest {
            title: args.title.trim().to_string(),
            dept: args.dept.or_else(|| Some("General".to_string())),
            roles: None,
            entity_id: self.entity_id.as_ref().map(|e| e.to_string()),
            mission_id: None,
            priority: args.priority,
            requires_human_approval: None,
            task_kind: Some("task".to_string()),
            content: args.content,
            director_id: Some("operations".to_string()),
            agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            metadata: None,
            due_at: None,
        };
        match internal_create_task(&self.control, &self.workspace_id, Some(&self.principal_id), req).await {
            Ok(task) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": true, "id": task.id, "kind": "task", "title": task.title })
                    .to_string(),
                is_error: false,
            }),
            Err(status) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": format!("create_task failed: HTTP {}", status.as_u16()) })
                    .to_string(),
                is_error: true,
            }),
        }
    }
}

/// Queue a real action on the machine the gateway runs on — file work, shell
/// work, browser work — as an approval-gated executor task. This is the phone's
/// path to "do it on my computer": chat enqueues, the operator approves, the
/// unattended executor runs it inside the entity's directory jail, and the
/// result comes back through the feed and push.
///
/// Deliberately NOT a direct tool: giving the chat loop shell powers would
/// execute inside a streamed HTTP turn with no approval gate and no run record.
#[derive(Clone)]
pub struct RunOnMyComputerTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct RunOnMyComputerArgs {
    instruction: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    entity_id: Option<String>,
    #[serde(default)]
    entity_name: Option<String>,
}

impl RunOnMyComputerTool {
    /// Same resolution order as missions: explicit id, name, chat scope, only
    /// visible entity. Machine actions are entity-bound because the entity's
    /// workspace directory is the jail the run executes in.
    async fn resolve_entity_id(&self, args: &RunOnMyComputerArgs) -> Option<String> {
        let entities = self.control.entities.read().await;
        let visible: Vec<&crate::state::Entity> = entities
            .iter()
            .filter(|e| {
                crate::control_api::is_entity_visible(
                    e,
                    &self.workspace_id,
                    Some(&self.principal_id),
                )
            })
            .collect();
        if let Some(id) = args
            .entity_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            && let Some(hit) = visible.iter().find(|e| e.id == id)
        {
            return Some(hit.id.clone());
        }
        if let Some(name) = args
            .entity_name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            && let Some(hit) = visible.iter().find(|e| e.name.eq_ignore_ascii_case(name))
        {
            return Some(hit.id.clone());
        }
        if let Some(scoped) = self.entity_id.as_deref()
            && let Some(hit) = visible.iter().find(|e| e.id == scoped)
        {
            return Some(hit.id.clone());
        }
        match visible.as_slice() {
            [only] => Some(only.id.clone()),
            _ => None,
        }
    }
}

#[async_trait]
impl Tool for RunOnMyComputerTool {
    fn name(&self) -> &'static str {
        "run_on_computer"
    }
    fn description(&self) -> &'static str {
        "Queue a real action on the user's computer: organise or edit files, run project/shell work, or drive the browser. The action waits for the user's explicit approval, then an unattended agent runs it inside the entity's project directory. Use when the user asks you to DO something on their machine (not just record it). Tell the user it is queued and waiting for their approval."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "instruction": { "type": "string", "description": "Exactly what to do, in enough detail for an agent working alone, e.g. 'Move every PDF on the Desktop into Documents/Invoices and rename them by date'." },
                "kind":        { "type": "string", "enum": ["files", "shell", "browser", "general"], "description": "What the action mostly touches. Defaults to general." },
                "entity_id":   { "type": "string", "description": "Optional entity id whose project directory the action runs in." },
                "entity_name": { "type": "string", "description": "Optional entity name, when the user named a project instead of giving an id." }
            },
            "required": ["instruction"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: RunOnMyComputerArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("invalid args: {}", e) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        let instruction = args.instruction.trim().to_string();
        if instruction.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": "instruction is empty" }).to_string(),
                is_error: true,
            });
        }
        let Some(entity_id) = self.resolve_entity_id(&args).await else {
            return Ok(ToolResult {
                call_id: String::new(),
                output: json!({
                    "ok": false,
                    "error": "no entity resolved: ask the user which project/entity this action belongs to"
                })
                .to_string(),
                is_error: true,
            });
        };
        let action_kind = match args.kind.as_deref().map(str::trim) {
            Some("files") => "files",
            Some("shell") => "shell",
            Some("browser") => "browser",
            _ => "general",
        };
        let title: String = {
            let first = instruction
                .lines()
                .next()
                .unwrap_or("Machine action")
                .trim();
            first.chars().take(120).collect()
        };
        let req = CreateTaskRequest {
            title,
            dept: Some("General".to_string()),
            roles: None,
            entity_id: Some(entity_id.clone()),
            mission_id: None,
            priority: None,
            // The whole safety story: the task is executor-shaped but stands
            // at the approval gate until the operator says go.
            requires_human_approval: Some(true),
            task_kind: Some("machine_action".to_string()),
            content: None,
            director_id: None,
            agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            metadata: Some(json!({
                crate::task_executor::META_EXECUTABLE: true,
                crate::task_executor::META_DESCRIPTION: instruction,
                "source": "chat",
                "actionKind": action_kind,
            })),
            due_at: None,
        };
        let task = match internal_create_task_queued(
            &self.control,
            &self.workspace_id,
            Some(&self.principal_id),
            req,
        )
        .await
        {
            Ok(t) => t,
            Err(status) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("run_on_computer failed: HTTP {}", status.as_u16()) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        let now = Utc::now().to_rfc3339();
        let approval = ApprovalRequest {
            id: Uuid::new_v4().to_string(),
            created_at: now.clone(),
            updated_at: now,
            entity_id,
            connection_id: None,
            task_id: Some(task.id.clone()),
            action: "run_on_computer".to_string(),
            status: ApprovalStatus::Pending,
            detail: Some(instruction.clone()),
            created_by: Some(self.principal_id.to_string()),
        };
        let approval_id = approval.id.clone();
        self.control.enqueue_approval_request(approval).await;
        Ok(ToolResult {
            call_id: String::new(),
            output: json!({
                "ok": true,
                "kind": "action_request",
                "taskId": task.id,
                "approvalId": approval_id,
                "actionKind": action_kind,
                "status": "awaiting_approval",
                "title": task.title,
            })
            .to_string(),
            is_error: false,
        })
    }
}

/// Turn a stated goal into a mission plus a real, dated plan. Without this the
/// only way to get planned work was the missions UI, so "I want to launch the
/// beta next month" produced at most a single flat task.
#[derive(Clone)]
pub struct CreateMissionTool {
    pub state: ServerState,
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct CreateMissionArgs {
    title: String,
    goal: String,
    #[serde(default)]
    entity_id: Option<String>,
    #[serde(default)]
    entity_name: Option<String>,
    #[serde(default)]
    definition_of_done: Option<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    budget_usd: Option<f64>,
    #[serde(default)]
    critic_director_id: Option<String>,
}

impl CreateMissionTool {
    /// Explicit id wins, then a case-insensitive name match, then the entity the
    /// chat is already scoped to, then the only visible entity. Missions are
    /// entity-bound, so an unresolvable entity is a hard error, not a default.
    async fn resolve_entity_id(&self, args: &CreateMissionArgs) -> Option<String> {
        let entities = self.control.entities.read().await;
        let visible: Vec<&crate::state::Entity> = entities
            .iter()
            .filter(|e| {
                crate::control_api::is_entity_visible(
                    e,
                    &self.workspace_id,
                    Some(&self.principal_id),
                )
            })
            .collect();
        if let Some(id) = args
            .entity_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            && let Some(hit) = visible.iter().find(|e| e.id == id)
        {
            return Some(hit.id.clone());
        }
        if let Some(name) = args
            .entity_name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            && let Some(hit) = visible.iter().find(|e| e.name.eq_ignore_ascii_case(name))
        {
            return Some(hit.id.clone());
        }
        if let Some(scoped) = self.entity_id.as_deref()
            && let Some(hit) = visible.iter().find(|e| e.id == scoped)
        {
            return Some(hit.id.clone());
        }
        match visible.as_slice() {
            [only] => Some(only.id.clone()),
            _ => None,
        }
    }
}

#[async_trait]
impl Tool for CreateMissionTool {
    fn name(&self) -> &'static str {
        "create_mission"
    }
    fn description(&self) -> &'static str {
        "Create a mission — a goal big enough to need several steps — and decompose it into concrete dated tasks. Use when the user states an outcome they want ('launch the beta', 'get to 100 customers') rather than a single piece of work. For one-off work use create_task instead."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title":       { "type": "string", "description": "Short mission name, e.g. 'Launch the paid beta'." },
                "goal":        { "type": "string", "description": "The outcome in the user's own words, including any deadline or success measure they gave." },
                "entity_id":   { "type": "string", "description": "Optional entity id to attach the mission to." },
                "entity_name": { "type": "string", "description": "Optional entity name, when the user named a project instead of giving an id." },
                "definition_of_done": { "type": "string", "description": "Set ONLY when the user wants the system to keep working autonomously until the goal is verifiably met: a concrete, checkable statement of what finished looks like. This turns on the iterate-until-done loop (develop → verify → develop) with iteration and budget caps." },
                "max_iterations": { "type": "integer", "description": "Optional cap on autonomous develop-verify rounds (default 5, max 25). Only with definition_of_done." },
                "budget_usd": { "type": "number", "description": "Optional spend cap for the loop in USD. Only with definition_of_done." },
                "critic_director_id": { "type": "string", "description": "Optional director whose standards judge each iteration's work. Defaults to the generic reviewer. Only with definition_of_done." }
            },
            "required": ["title", "goal"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateMissionArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("invalid args: {}", e) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        let Some(entity_id) = self.resolve_entity_id(&args).await else {
            return Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": "no entity to attach the mission to; create one first" })
                    .to_string(),
                is_error: true,
            });
        };
        let loop_config = args
            .definition_of_done
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|dod| crate::state::MissionLoopConfig {
                definition_of_done: dod.to_string(),
                max_iterations: args
                    .max_iterations
                    .unwrap_or_else(crate::state::default_loop_max_iterations),
                budget_usd: args.budget_usd,
                critic_director_id: args
                    .critic_director_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
            });
        let req = CreateMissionRequest {
            entity_id,
            title: args.title.trim().to_string(),
            summary: None,
            status: None,
            priority: None,
            goal: Some(args.goal.trim().to_string()),
            owner: None,
            created_by: Some(self.principal_id.to_string()),
            source: Some("chat".to_string()),
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: None,
            assigned_director_id: None,
            loop_config,
        };
        let mission = match internal_create_mission(
            &self.control,
            &self.workspace_id,
            Some(&self.principal_id),
            req,
        )
        .await
        {
            Ok(mission) => mission,
            Err(status) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("create_mission failed: HTTP {}", status.as_u16()) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        // Awaited, not spawned: the chat reply should be able to say how many
        // tasks landed.
        if crate::control_api::mission_planning_enabled(&self.state) {
            crate::mission_planner::plan_and_attach_tasks(&self.state, &self.control, &mission)
                .await;
        }
        let tasks_created = self
            .control
            .tasks
            .read()
            .await
            .iter()
            .filter(|t| t.mission_id.as_deref() == Some(mission.id.as_str()))
            .count();
        Ok(ToolResult {
            call_id: String::new(),
            output: json!({
                "ok": true,
                "id": mission.id,
                "title": mission.title,
                "tasks_created": tasks_created
            })
            .to_string(),
            is_error: false,
        })
    }
}

/// Tool: create a reminder, resolving its fire time from an explicit minute
/// offset or natural-language phrasing.
#[derive(Clone)]
pub struct CreateReminderTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
    pub timezone_offset_minutes: Option<i32>,
}

#[derive(Deserialize)]
struct CreateReminderArgs {
    what: String,
    #[serde(default)]
    when: Option<String>,
    /// LLM-resolved relative time. Wins over parsing `when` when present.
    #[serde(default)]
    in_minutes: Option<f64>,
}

#[async_trait]
impl Tool for CreateReminderTool {
    fn name(&self) -> &'static str {
        "set_reminder"
    }
    fn description(&self) -> &'static str {
        "Set a reminder for the user. Use when the user asks to be reminded, nudged, or pinged about something. Pass the natural-language 'when' verbatim AND, when the user gave a relative time (e.g. 'in 20 minutes', 'in 2 hours'), also pass in_minutes with the resolved number of minutes."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "what": { "type": "string", "description": "What to remind the user about." },
                "when": { "type": "string", "description": "Natural-language time, e.g. 'tomorrow at 9am', 'in 2 hours'. Optional." },
                "in_minutes": { "type": "number", "description": "Minutes from now until the reminder, when the user's phrasing is relative. E.g. 'in 2 hours' → 120." }
            },
            "required": ["what"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateReminderArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: json!({ "ok": false, "error": format!("invalid args: {}", e) })
                        .to_string(),
                    is_error: true,
                });
            }
        };
        let title = match &args.when {
            Some(w) if !w.trim().is_empty() => {
                format!("Reminder ({}): {}", w.trim(), args.what.trim())
            }
            _ => format!("Reminder: {}", args.what.trim()),
        };
        // Resolve when the reminder fires: explicit in_minutes beats parsing
        // the natural-language phrase. Unresolvable reminders still persist
        // as tasks, they just never auto-fire.
        let now = chrono::Utc::now();
        let due_at = args
            .in_minutes
            .filter(|m| m.is_finite() && *m >= 0.0 && *m <= 366.0 * 24.0 * 60.0)
            .map(|m| now + chrono::Duration::seconds((m * 60.0).round() as i64))
            .or_else(|| {
                args.when.as_deref().and_then(|w| {
                    crate::reminders::parse_when_in_timezone(w, now, self.timezone_offset_minutes)
                })
            });
        let metadata = due_at.map(|d| {
            json!({
                crate::reminders::REMINDER_DUE_AT_KEY: d.to_rfc3339(),
                crate::reminders::REMINDER_FIRED_KEY: false,
            })
        });
        let scheduled_at = due_at.map(|d| d.to_rfc3339());
        let req = CreateTaskRequest {
            title,
            dept: Some("General".to_string()),
            roles: None,
            entity_id: self.entity_id.as_ref().map(|e| e.to_string()),
            mission_id: None,
            priority: None,
            requires_human_approval: None,
            task_kind: Some("reminder".to_string()),
            content: Some(args.what.trim().to_string()),
            director_id: Some("operations".to_string()),
            agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            metadata,
            due_at: scheduled_at.clone(),
        };
        match internal_create_task(&self.control, &self.workspace_id, Some(&self.principal_id), req).await {
            Ok(task) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({
                    "ok": true,
                    "id": task.id,
                    "kind": "reminder",
                    "fires_at": scheduled_at,
                    "note": if scheduled_at.is_none() {
                        Some("time not understood; reminder saved but will not auto-fire — ask the user for a concrete time")
                    } else {
                        None
                    }
                })
                .to_string(),
                is_error: false,
            }),
            Err(status) => Ok(ToolResult {
                call_id: String::new(),
                output: json!({ "ok": false, "error": format!("set_reminder failed: HTTP {}", status.as_u16()) })
                    .to_string(),
                is_error: true,
            }),
        }
    }
}

/// Tool: list the workspace's visible entities/projects.
#[derive(Clone)]
pub struct ListEntitiesTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub principal_id: Arc<str>,
}

#[async_trait]
impl Tool for ListEntitiesTool {
    fn name(&self) -> &'static str {
        "list_entities"
    }
    fn description(&self) -> &'static str {
        "List the projects/hubs (entities) in the user's workspace. Useful when the user asks 'what am I working on' or wants to switch context to a specific entity."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<ToolResult> {
        let entities = self.control.entities.read().await;
        let ws = self.workspace_id.as_ref();
        let mut listed: Vec<Value> = Vec::new();
        for e in entities.iter() {
            if !crate::control_api::is_entity_visible(e, ws, Some(&self.principal_id)) {
                continue;
            }
            listed.push(json!({
                "id": e.id,
                "name": e.name,
                "description": e.description,
                "readiness_percent": e.readiness.percent,
            }));
        }
        Ok(ToolResult {
            call_id: String::new(),
            output: json!({ "ok": true, "entities": listed }).to_string(),
            is_error: false,
        })
    }
}

/// Tool: recall the user's most recent tasks, notes, and reminders.
#[derive(Clone)]
pub struct RecallRecentTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct RecallArgs {
    #[serde(default = "default_recall_limit")]
    limit: usize,
}
fn default_recall_limit() -> usize {
    10
}

#[async_trait]
impl Tool for RecallRecentTool {
    fn name(&self) -> &'static str {
        "recall_recent"
    }
    fn description(&self) -> &'static str {
        "Recall the user's most recent tasks, notes, and reminders. Use when the user asks 'what did I save', 'what are my tasks', or wants a quick recap. Returns titles + kinds."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "description": "Max items (default 10).", "minimum": 1, "maximum": 50 }
            }
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: RecallArgs =
            serde_json::from_value(arguments).unwrap_or(RecallArgs { limit: 10 });
        let visible_entity_ids: std::collections::HashSet<String> = {
            let entities = self.control.entities.read().await;
            entities
                .iter()
                .filter(|e| {
                    crate::control_api::is_entity_visible(
                        e,
                        &self.workspace_id,
                        Some(&self.principal_id),
                    )
                })
                .map(|e| e.id.clone())
                .collect()
        };
        let tasks = self.control.tasks.read().await;
        let entity_filter = self.entity_id.as_ref().map(|s| s.to_string());
        let mut items: Vec<Value> = tasks
            .iter()
            .rev()
            .filter(|t| match &entity_filter {
                Some(eid) => t.entity_id.as_deref() == Some(eid),
                None => t
                    .entity_id
                    .as_deref()
                    .is_some_and(|id| visible_entity_ids.contains(id)),
            })
            .take(args.limit.clamp(1, 50))
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": t.title,
                    "kind": t.task_kind.clone().unwrap_or_else(|| "task".to_string()),
                    "status": format!("{:?}", t.status),
                    "created_at": t.created_at,
                })
            })
            .collect();
        if items.is_empty() {
            items = vec![];
        }
        Ok(ToolResult {
            call_id: String::new(),
            output: json!({ "ok": true, "items": items }).to_string(),
            is_error: false,
        })
    }
}

/// Read-only executive review assembled from stored state. This is deliberately
/// deterministic: the model summarizes these facts instead of reconstructing a
/// "today" view from memory or inventing priorities.
#[derive(Clone)]
pub struct ReviewTodayTool {
    pub control: Arc<ControlState>,
    pub state: ServerState,
    pub workspace_id: Arc<str>,
    pub principal_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
}

#[async_trait]
impl Tool for ReviewTodayTool {
    fn name(&self) -> &'static str {
        "review_today"
    }
    fn description(&self) -> &'static str {
        "Build a factual daily operating brief: open and overdue work, active missions, upcoming reminders, heartbeats, and decisions. Use this before answering what the user should focus on or what changed."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<ToolResult> {
        let entity_ids: std::collections::HashSet<String> = {
            let entities = self.control.entities.read().await;
            entities
                .iter()
                .filter(|e| {
                    crate::control_api::is_entity_visible(
                        e,
                        &self.workspace_id,
                        Some(&self.principal_id),
                    ) && self.entity_id.as_deref().is_none_or(|id| e.id == id)
                })
                .map(|e| e.id.clone())
                .collect()
        };
        let now = Utc::now();
        let tasks = self.control.tasks.read().await;
        let mut open = Vec::new();
        let mut overdue = Vec::new();
        for task in tasks.iter().filter(|t| {
            t.entity_id
                .as_deref()
                .is_some_and(|id| entity_ids.contains(id))
        }) {
            if matches!(task.status, TaskStatus::Completed | TaskStatus::Cancelled) {
                continue;
            }
            let row = json!({"id": task.id, "title": task.title, "status": task.status, "priority": task.priority, "due_at": task.due_at, "entity_id": task.entity_id});
            if crate::state::task_is_overdue(task, now) {
                overdue.push(row);
            } else {
                open.push(row);
            }
        }
        drop(tasks);
        let missions = self.control.missions.read().await;
        let active_missions: Vec<Value> = missions.iter()
            .filter(|m| entity_ids.contains(&m.entity_id) && matches!(m.status, crate::state::MissionStatus::Planned | crate::state::MissionStatus::Active | crate::state::MissionStatus::Blocked | crate::state::MissionStatus::UserInputNeeded))
            .map(|m| json!({"id": m.id, "title": m.title, "status": m.status, "goal": m.goal, "entity_id": m.entity_id})).collect();
        drop(missions);
        let heartbeats = if let Some(store) = self.state.heartbeat_store.as_ref() {
            use crate::heartbeat_store::HeartbeatStore;
            store.list_all().await.unwrap_or_default().into_iter().filter(|h| {
                h.workspace_id.as_deref().is_none_or(|ws| ws == self.workspace_id.as_ref())
                    && h.enabled
                    && h.target_id.as_deref().is_none_or(|id| entity_ids.contains(id))
            }).map(|h| json!({"id": h.id, "name": h.name, "next_fire_at": h.next_fire_at, "target_id": h.target_id, "condition": h.condition})).collect()
        } else {
            Vec::new()
        };
        Ok(ToolResult {
            call_id: String::new(),
            output: json!({
                "ok": true, "generated_at": now.to_rfc3339(), "open_tasks": open,
                "overdue_tasks": overdue, "active_missions": active_missions,
                "upcoming_heartbeats": heartbeats
            })
            .to_string(),
            is_error: false,
        })
    }
}

/// Tool: mutate an existing task's status or due time (complete, cancel,
/// reopen, block, snooze, or reschedule).
#[derive(Clone)]
pub struct UpdateTaskTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
    pub principal_id: Arc<str>,
    pub timezone_offset_minutes: Option<i32>,
}

#[derive(Deserialize)]
struct UpdateTaskArgs {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    action: String,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    in_minutes: Option<f64>,
    #[serde(default)]
    result: Option<String>,
}

// Find the task an update targets: by exact id, or by a unique
// case-insensitive title match among the caller's visible tasks.
fn task_match<'a>(
    tasks: &'a [crate::state::Task],
    args: &UpdateTaskArgs,
    visible: &std::collections::HashSet<String>,
    entity_id: Option<&str>,
) -> Result<&'a crate::state::Task, String> {
    let candidates: Vec<&crate::state::Task> = tasks
        .iter()
        .filter(|t| {
            t.entity_id
                .as_deref()
                .is_some_and(|id| visible.contains(id))
                && entity_id.is_none_or(|id| t.entity_id.as_deref() == Some(id))
        })
        .collect();
    if let Some(id) = args
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return candidates
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| "task id was not found in this workspace".to_string());
    }
    let title = args
        .title
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "provide task_id or an exact task title".to_string())?;
    let matches: Vec<_> = candidates
        .into_iter()
        .filter(|t| t.title.eq_ignore_ascii_case(title))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err("no task matched that exact title".to_string()),
        _ => Err("multiple tasks matched; use task_id".to_string()),
    }
}

#[async_trait]
impl Tool for UpdateTaskTool {
    fn name(&self) -> &'static str {
        "update_task"
    }
    fn description(&self) -> &'static str {
        "Manage an existing task by exact id or unique exact title. Actions: complete, cancel, reopen, block, snooze, or reschedule. Never claim a task changed unless this tool returns ok=true."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
        "task_id":{"type":"string"},"title":{"type":"string"},
        "action":{"type":"string","enum":["complete","cancel","reopen","block","snooze","reschedule"]},
        "when":{"type":"string","description":"When to reschedule, e.g. tomorrow at 9am or an RFC3339 timestamp."},
        "in_minutes":{"type":"number"},"result":{"type":"string"}
    },"required":["action"]})
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: UpdateTaskArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => return Ok(tool_err(format!("invalid args: {e}"))),
        };
        let visible: std::collections::HashSet<String> = {
            let entities = self.control.entities.read().await;
            entities
                .iter()
                .filter(|e| {
                    crate::control_api::is_entity_visible(
                        e,
                        &self.workspace_id,
                        Some(&self.principal_id),
                    )
                })
                .map(|e| e.id.clone())
                .collect()
        };
        let entity_id = self.entity_id.as_deref();
        // Resolve the task to an *id* under the read guard, then mutate it in place
        // under the write guard. This used to clone the task, mutate the clone, and
        // write it back with `*slot = clone` — a whole-row overwrite that reverted
        // every field a background loop had changed in the meantime (progress, the
        // evidence pack, the active run id, attempt markers), because an overwrite
        // cannot tell "I did not touch this" from "set it back to what I read".
        let task_id = {
            let tasks = self.control.tasks.read().await;
            match task_match(&tasks, &args, &visible, entity_id) {
                Ok(t) => t.id.clone(),
                Err(e) => return Ok(tool_err(e)),
            }
        };

        let action = args.action.trim().to_ascii_lowercase();
        if !matches!(
            action.as_str(),
            "complete" | "cancel" | "reopen" | "block" | "snooze" | "reschedule"
        ) {
            return Ok(tool_err(format!("unsupported task action '{action}'")));
        }

        // Resolve a reschedule's target time before taking the write guard: it needs
        // no task state, and it is the only branch that can fail.
        let due: Option<String> = if action == "snooze" || action == "reschedule" {
            let now = Utc::now();
            let resolved = args
                .in_minutes
                .filter(|m| m.is_finite() && *m >= 0.0)
                .map(|m| now + chrono::Duration::seconds((m * 60.0).round() as i64))
                .map(|d| d.to_rfc3339())
                .or_else(|| {
                    args.when.as_deref().and_then(|w| {
                        crate::reminders::parse_when_in_timezone(
                            w,
                            now,
                            self.timezone_offset_minutes,
                        )
                        .map(|d| d.to_rfc3339())
                    })
                });
            match resolved {
                Some(d) => Some(d),
                None => {
                    return Ok(tool_err(
                        "could not resolve the new time; ask for a concrete time",
                    ));
                }
            }
        } else {
            None
        };

        // Chat is a human asking, so every transition here goes through the
        // by-human setter — including "reopen", which deliberately outranks a cancel.
        let updated = self
            .control
            .update_task(&task_id, |task| {
                let before = format!("{:?}", task.status);
                match action.as_str() {
                    "complete" => {
                        crate::state::set_task_status_by_human(task, TaskStatus::Completed);
                        task.progress = 100;
                        task.outcome = Some(crate::state::TaskOutcome::Completed);
                    }
                    "cancel" => {
                        crate::state::set_task_status_by_human(task, TaskStatus::Cancelled);
                        task.progress = 100;
                    }
                    "reopen" => {
                        crate::state::set_task_status_by_human(task, TaskStatus::Queued);
                        task.progress = 0;
                        task.outcome = None;
                    }
                    "block" => {
                        crate::state::set_task_status_by_human(task, TaskStatus::Failed);
                        task.outcome = Some(crate::state::TaskOutcome::Failed);
                        task.result = args.result.clone();
                    }
                    _ => {
                        // snooze | reschedule — `due` is resolved above.
                        let due = due.clone().unwrap_or_default();
                        task.due_at = Some(due.clone());
                        if task.task_kind.as_deref() == Some("reminder") {
                            crate::state::task_meta_set_str(
                                task,
                                crate::reminders::REMINDER_DUE_AT_KEY,
                                &due,
                            );
                            crate::state::task_meta_set_str(
                                task,
                                crate::reminders::REMINDER_FIRED_KEY,
                                "false",
                            );
                        }
                        if task.status == TaskStatus::Completed {
                            crate::state::set_task_status_by_human(task, TaskStatus::Queued);
                            task.progress = 0;
                        }
                    }
                }
                (before, task.clone())
            })
            .await;

        let Some((before, task)) = updated else {
            return Ok(tool_err("task changed while it was being updated; retry"));
        };
        self.control.persist_task_row(&task).await;
        Ok(tool_ok(
            json!({"ok":true,"id":task.id,"kind":"task_update","title":task.title,"before_status":before,"status":task.status,"due_at":task.due_at}),
        ))
    }
}

/// Tool: update the active entity's durable goals, context, rules, or notes.
#[derive(Clone)]
pub struct UpdateEntityTool {
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub principal_id: Arc<str>,
    pub entity_id: Option<Arc<str>>,
}

#[derive(Deserialize)]
struct UpdateEntityArgs {
    #[serde(default)]
    entity_id: Option<String>,
    #[serde(default)]
    entity_name: Option<String>,
    #[serde(default)]
    goals: Option<String>,
    #[serde(default)]
    active_context: Option<String>,
    #[serde(default)]
    operating_rules: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[async_trait]
impl Tool for UpdateEntityTool {
    fn name(&self) -> &'static str {
        "update_entity"
    }
    fn description(&self) -> &'static str {
        "Update the active entity's durable goals, current context, operating rules, or notes. Use only when the user explicitly asks to change what the entity is working toward or remembers."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "entity_id":{"type":"string"},"entity_name":{"type":"string"},"goals":{"type":"string"},"active_context":{"type":"string"},"operating_rules":{"type":"string"},"notes":{"type":"string"}
        }})
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: UpdateEntityArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => return Ok(tool_err(format!("invalid args: {e}"))),
        };
        let target = args.entity_id.as_deref().or(self.entity_id.as_deref());
        let mut entities = self.control.entities.write().await;
        let matches: Vec<usize> = entities
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                crate::control_api::is_entity_visible(
                    e,
                    &self.workspace_id,
                    Some(&self.principal_id),
                ) && target.is_none_or(|id| e.id == id)
                    && args
                        .entity_name
                        .as_deref()
                        .is_none_or(|name| e.name.eq_ignore_ascii_case(name.trim()))
            })
            .map(|(i, _)| i)
            .collect();
        let Some(index) = matches.first().copied() else {
            return Ok(tool_err("entity was not found in this workspace"));
        };
        if matches.len() > 1 {
            return Ok(tool_err("multiple entities matched; provide entity_id"));
        }
        let entity = &mut entities[index];
        if let Some(v) = args.goals {
            entity.goals = Some(v.trim().to_string());
        }
        if let Some(v) = args.active_context {
            entity.active_context = Some(v.trim().to_string());
        }
        if let Some(v) = args.operating_rules {
            entity.operating_rules = Some(v.trim().to_string());
        }
        if let Some(v) = args.notes {
            entity.notes = Some(v.trim().to_string());
        }
        crate::control_api::refresh_entity_readiness(entity);
        let out = entity.clone();
        drop(entities);
        let _ = self.control.sync_entities_to_store().await;
        Ok(tool_ok(
            json!({"ok":true,"id":out.id,"kind":"entity_update","name":out.name,"goals":out.goals,"active_context":out.active_context,"readiness_percent":out.readiness.percent}),
        ))
    }
}

/// Tool: list recurring heartbeat automations visible in this workspace.
#[derive(Clone)]
pub struct ListHeartbeatsTool {
    pub state: ServerState,
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub principal_id: Arc<str>,
}

#[async_trait]
impl Tool for ListHeartbeatsTool {
    fn name(&self) -> &'static str {
        "list_heartbeats"
    }
    fn description(&self) -> &'static str {
        "List recurring automations visible in this workspace, including whether they are enabled and their next fire time."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _arguments: Value) -> anyhow::Result<ToolResult> {
        let Some(store) = self.state.heartbeat_store.as_ref() else {
            return Ok(tool_err("heartbeat store unavailable"));
        };
        use crate::heartbeat_store::HeartbeatStore;
        let entities = self.control.entities.read().await;
        let rows: Vec<Value> = store.list_all().await.unwrap_or_default().into_iter().filter(|h| {
            h.workspace_id.as_deref().is_none_or(|ws| ws == self.workspace_id.as_ref()) && h.target_id.as_deref().is_none_or(|id| entities.iter().any(|e| e.id == id && crate::control_api::is_entity_visible(e, &self.workspace_id, Some(&self.principal_id))))
        }).map(|h| json!({"id":h.id,"name":h.name,"enabled":h.enabled,"target_kind":h.target_kind,"target_id":h.target_id,"cron":h.cron,"cadence_secs":h.cadence_secs,"next_fire_at":h.next_fire_at,"condition":h.condition})).collect();
        Ok(tool_ok(json!({"ok":true,"heartbeats":rows})))
    }
}

/// Tool: create, pause, resume, reschedule, run now, or delete a heartbeat.
#[derive(Clone)]
pub struct ManageHeartbeatTool {
    pub state: ServerState,
    pub control: Arc<ControlState>,
    pub workspace_id: Arc<str>,
    pub principal_id: Arc<str>,
}

#[derive(Deserialize)]
struct ManageHeartbeatArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    action: String,
    #[serde(default)]
    target_kind: Option<String>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    director_id: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    cadence_secs: Option<u64>,
    #[serde(default)]
    condition: Option<String>,
}

#[async_trait]
impl Tool for ManageHeartbeatTool {
    fn name(&self) -> &'static str {
        "manage_heartbeat"
    }
    fn description(&self) -> &'static str {
        "Create, pause, resume, reschedule, run now, or delete a recurring heartbeat. Use list_heartbeats first when the user refers to one by name; destructive deletion requires an explicit ask."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{
        "id":{"type":"string"},"name":{"type":"string"},"action":{"type":"string","enum":["create","pause","resume","reschedule","run_now","delete"]},"target_kind":{"type":"string","enum":["entity","mission","global"]},"target_id":{"type":"string"},"director_id":{"type":"string"},"cron":{"type":"string","description":"Six-field UTC cron expression."},"cadence_secs":{"type":"integer","minimum":1},"condition":{"type":"string"}
    },"required":["action"]})
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: ManageHeartbeatArgs = match serde_json::from_value(arguments) {
            Ok(v) => v,
            Err(e) => return Ok(tool_err(format!("invalid args: {e}"))),
        };
        let Some(store) = self.state.heartbeat_store.as_ref() else {
            return Ok(tool_err("heartbeat store unavailable"));
        };
        use crate::heartbeat_store::{HeartbeatStore, next_fire_at};
        // "create" builds a fresh heartbeat row; every other action looks up
        // an existing one by id or name first.
        let mut hb = if args.action.eq_ignore_ascii_case("create") {
            let name = args
                .name
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow::anyhow!("name is required"))?;
            if args.cron.is_none() && args.cadence_secs.is_none() {
                return Ok(tool_err("provide cron or cadence_secs"));
            }
            if args.cadence_secs.is_some_and(|v| v == 0) {
                return Ok(tool_err("cadence_secs must be greater than zero"));
            }
            if let Some(cron) = args.cron.as_deref()
                && cron::Schedule::from_str(cron).is_err()
            {
                return Ok(tool_err("invalid cron expression"));
            }
            let now = Utc::now().to_rfc3339();
            crate::state::Heartbeat {
                id: format!("hb_{}", Uuid::new_v4()),
                workspace_id: Some(self.workspace_id.to_string()),
                name: name.to_string(),
                enabled: true,
                target_kind: args.target_kind.unwrap_or_else(|| "global".to_string()),
                target_id: args.target_id,
                director_id: args.director_id.unwrap_or_else(|| "operations".to_string()),
                cron: args.cron.clone(),
                cadence_secs: args.cadence_secs,
                condition: args.condition,
                last_fired_at: None,
                next_fire_at: None,
                lease_until: None,
                lease_token: None,
                created_at: now.clone(),
                updated_at: now,
            }
        } else {
            let all = store.list_all().await.unwrap_or_default();
            let matches: Vec<_> = all
                .into_iter()
                .filter(|h| {
                    h.workspace_id
                        .as_deref()
                        .is_none_or(|ws| ws == self.workspace_id.as_ref())
                        && (args.id.as_deref().is_some_and(|id| h.id == id)
                            || args
                                .name
                                .as_deref()
                                .is_some_and(|name| h.name.eq_ignore_ascii_case(name.trim())))
                })
                .collect();
            match matches.as_slice() {
                [one] => one.clone(),
                [] => return Ok(tool_err("heartbeat was not found in this workspace")),
                _ => return Ok(tool_err("multiple heartbeats matched; use id")),
            }
        };
        match args.action.trim().to_ascii_lowercase().as_str() {
            "create" => {}
            "pause" => hb.enabled = false,
            "resume" => hb.enabled = true,
            "run_now" => {
                hb.enabled = true;
                hb.next_fire_at = None;
            }
            "reschedule" => {
                if args.cron.is_none() && args.cadence_secs.is_none() {
                    return Ok(tool_err("reschedule requires cron or cadence_secs"));
                }
                if let Some(cron) = args.cron {
                    if cron::Schedule::from_str(&cron).is_err() {
                        return Ok(tool_err("invalid cron expression"));
                    }
                    hb.cron = Some(cron);
                }
                if args.cadence_secs.is_some_and(|v| v == 0) {
                    return Ok(tool_err("cadence_secs must be greater than zero"));
                }
                if args.cadence_secs.is_some() {
                    hb.cadence_secs = args.cadence_secs;
                }
                hb.next_fire_at = None;
            }
            "delete" => {
                store.delete(&hb.id).await?;
                return Ok(tool_ok(
                    json!({"ok":true,"id":hb.id,"kind":"heartbeat_delete"}),
                ));
            }
            other => return Ok(tool_err(format!("unsupported heartbeat action '{other}'"))),
        }
        if hb.next_fire_at.is_none() && hb.enabled {
            hb.next_fire_at = next_fire_at(&hb, Utc::now()).map(|dt| dt.to_rfc3339());
        }
        hb.updated_at = Utc::now().to_rfc3339();
        store.upsert(&hb).await?;
        Ok(tool_ok(
            json!({"ok":true,"id":hb.id,"kind":"heartbeat_update","name":hb.name,"enabled":hb.enabled,"next_fire_at":hb.next_fire_at}),
        ))
    }
}

// ===== The three tools that touch the real filesystem =====
//
// Every other tool here writes to the database. These write to your disk, so
// they are jailed to two roots by `local_files` and are only registered at all
// when the gateway is running locally — see `build_workspace_tools`.

/// Tool: write a note as a Markdown file on the user's Desktop.
#[derive(Clone)]
pub struct SaveNoteToDesktopTool {
    pub files: Arc<crate::local_files::LocalFiles>,
}

#[derive(Deserialize)]
struct SaveNoteToDesktopArgs {
    content: String,
    #[serde(default)]
    title: Option<String>,
}

#[async_trait]
impl Tool for SaveNoteToDesktopTool {
    fn name(&self) -> &'static str {
        "save_note_to_desktop"
    }
    fn description(&self) -> &'static str {
        "Write a note as a Markdown file on the user's Desktop, where they will see it. \
         Use when the user asks to put a note on their desktop, or to leave/write something \
         somewhere they can find it outside the app. For notes that only need to live inside \
         the workspace, use create_note instead."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The full note text." },
                "title": { "type": "string", "description": "Short title; becomes the filename." }
            },
            "required": ["content"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: SaveNoteToDesktopArgs = match serde_json::from_value(arguments) {
            Ok(a) => a,
            Err(e) => return Ok(tool_err(format!("bad arguments: {e}"))),
        };
        match self.files.save_note(args.title.as_deref(), &args.content) {
            Ok(path) => Ok(tool_ok(json!({
                "ok": true,
                "path": path.to_string_lossy(),
                "file": path.file_name().map(|f| f.to_string_lossy().to_string()),
            }))),
            Err(e) => Ok(tool_err(e.to_string())),
        }
    }
}

/// Tool: create a folder inside the user's local Consistent folder.
#[derive(Clone)]
pub struct CreateFolderTool {
    pub files: Arc<crate::local_files::LocalFiles>,
}

#[derive(Deserialize)]
struct CreateFolderArgs {
    path: String,
}

#[async_trait]
impl Tool for CreateFolderTool {
    fn name(&self) -> &'static str {
        "create_folder"
    }
    fn description(&self) -> &'static str {
        "Create a folder on the user's machine, inside their Consistent folder. \
         Nested paths like \"clients/acme\" are fine. Use when the user asks for a folder \
         to be made locally."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Folder path relative to the Consistent folder, e.g. \"clients/acme\"."
                }
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: CreateFolderArgs = match serde_json::from_value(arguments) {
            Ok(a) => a,
            Err(e) => return Ok(tool_err(format!("bad arguments: {e}"))),
        };
        match self.files.create_folder(&args.path) {
            Ok(path) => Ok(tool_ok(
                json!({ "ok": true, "path": path.to_string_lossy() }),
            )),
            Err(e) => Ok(tool_err(e.to_string())),
        }
    }
}

/// Tool: write an arbitrary file inside the user's local Consistent folder.
#[derive(Clone)]
pub struct WriteLocalFileTool {
    pub files: Arc<crate::local_files::LocalFiles>,
}

#[derive(Deserialize)]
struct WriteLocalFileArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteLocalFileTool {
    fn name(&self) -> &'static str {
        "write_local_file"
    }
    fn description(&self) -> &'static str {
        "Write a file on the user's machine, inside their Consistent folder. Parent folders \
         are created as needed. Use for saving a document, list, or record the user asked to \
         keep as a real file."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the Consistent folder, e.g. \"clients/acme/brief.md\"."
                },
                "content": { "type": "string", "description": "The file's full contents." }
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolResult> {
        let args: WriteLocalFileArgs = match serde_json::from_value(arguments) {
            Ok(a) => a,
            Err(e) => return Ok(tool_err(format!("bad arguments: {e}"))),
        };
        match self.files.write_file(&args.path, &args.content) {
            Ok(path) => Ok(tool_ok(
                json!({ "ok": true, "path": path.to_string_lossy() }),
            )),
            Err(e) => Ok(tool_err(e.to_string())),
        }
    }
}

// Success reply mirrored by `tool_err` below.
fn tool_ok(payload: Value) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: payload.to_string(),
        is_error: false,
    }
}

/// A refusal the model can act on. The jail's message says *why* ("path escapes
/// the allowed folder"), which is what lets the assistant explain itself
/// instead of just failing.
fn tool_err(message: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: json!({ "ok": false, "error": message.into() }).to_string(),
        is_error: true,
    }
}

/// Convenience: build the default workspace toolset for a chat request.
pub fn build_workspace_tools(
    state: ServerState,
    control: Arc<ControlState>,
    workspace_id: &str,
    entity_id: Option<&str>,
    principal_id: &str,
    timezone_offset_minutes: Option<i32>,
) -> Vec<Box<dyn Tool>> {
    let ws: Arc<str> = Arc::from(workspace_id);
    let eid: Option<Arc<str>> = entity_id.map(Arc::from);
    let pid: Arc<str> = Arc::from(principal_id);
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(CreateWorkspaceTool {
            state: state.clone(),
            current_workspace_id: ws.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(CreateEntityTool {
            state: state.clone(),
            control: control.clone(),
            workspace_id: ws.clone(),
        }),
        Box::new(CreateNoteTool {
            control: control.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(RecordLessonTool {
            state: state.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(CreateTaskTool {
            control: control.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(CreateMissionTool {
            state: state.clone(),
            control: control.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(CreateReminderTool {
            control: control.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
            timezone_offset_minutes,
        }),
        Box::new(ListEntitiesTool {
            control: control.clone(),
            workspace_id: ws.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(RecallRecentTool {
            control: control.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
        }),
        Box::new(ReviewTodayTool {
            control: control.clone(),
            state: state.clone(),
            workspace_id: Arc::from(workspace_id),
            principal_id: Arc::from(principal_id),
            entity_id: entity_id.map(Arc::from),
        }),
        Box::new(UpdateTaskTool {
            control: control.clone(),
            workspace_id: Arc::from(workspace_id),
            entity_id: entity_id.map(Arc::from),
            principal_id: Arc::from(principal_id),
            timezone_offset_minutes,
        }),
        Box::new(UpdateEntityTool {
            control: control.clone(),
            workspace_id: Arc::from(workspace_id),
            principal_id: Arc::from(principal_id),
            entity_id: entity_id.map(Arc::from),
        }),
        Box::new(ListHeartbeatsTool {
            state: state.clone(),
            control: control.clone(),
            workspace_id: Arc::from(workspace_id),
            principal_id: Arc::from(principal_id),
        }),
        Box::new(ManageHeartbeatTool {
            state: state.clone(),
            control: control.clone(),
            workspace_id: Arc::from(workspace_id),
            principal_id: Arc::from(principal_id),
        }),
    ];

    // Skills: `load_skill` reads one on demand (the index rides the system
    // prompt), `save_skill` turns an approved procedure into a new one.
    if let Some(library) = state.skills.clone() {
        tools.push(Box::new(crate::skills::LoadSkillTool {
            library: library.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
        }));
        tools.push(Box::new(crate::skills::SaveSkillTool {
            library,
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
        }));
    }

    // Filesystem access is offered only when the gateway is the user's own
    // machine. On a hosted deployment "your Desktop" is a server's disk that
    // nobody will ever look at, and handing a model write access to it buys
    // nothing and risks something. Machine actions follow the same rule: "run
    // on my computer" only means something when this IS the user's computer.
    if let Some(files) = state.local_files.clone() {
        tools.push(Box::new(SaveNoteToDesktopTool {
            files: files.clone(),
        }));
        tools.push(Box::new(CreateFolderTool {
            files: files.clone(),
        }));
        tools.push(Box::new(WriteLocalFileTool { files }));
        tools.push(Box::new(RunOnMyComputerTool {
            control: control.clone(),
            workspace_id: ws.clone(),
            entity_id: eid.clone(),
            principal_id: pid.clone(),
        }));
    }

    tools
}

#[cfg(test)]
mod local_file_tool_tests {
    use super::*;
    use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

    async fn tools_for(with_files: bool) -> Vec<String> {
        let memory: Arc<dyn MemoryStore> =
            Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        let mut state = ServerState::new("t".into(), Some("secret".into()), memory)
            .with_control_state(control.clone());
        if with_files {
            let dir = tempfile::tempdir().unwrap();
            let mut files = crate::local_files::LocalFiles::new(
                dir.path().join("Desktop"),
                dir.path().join("Consistent"),
            );
            files.ensure_roots().unwrap();
            state = state.with_local_files(Arc::new(files));
            // Leak the tempdir: the roots must outlive this call, and the test
            // only cares about which tools exist.
            std::mem::forget(dir);
        }
        build_workspace_tools(state, control, "default", None, "user", None)
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    /// The file tools exist only when the gateway is the user's own machine.
    #[tokio::test]
    async fn file_tools_are_absent_on_a_gateway_with_no_local_files() {
        let names = tools_for(false).await;
        for tool in ["save_note_to_desktop", "create_folder", "write_local_file"] {
            assert!(
                !names.contains(&tool.to_string()),
                "{tool} must not be offered when the gateway has no local root — \
                 on a hosted deployment that is a server's disk"
            );
        }
        // The workspace tools are unaffected either way.
        assert!(names.contains(&"create_entity".to_string()));
        assert!(names.contains(&"set_reminder".to_string()));
    }

    #[tokio::test]
    async fn file_tools_are_present_when_running_locally() {
        let names = tools_for(true).await;
        for tool in ["save_note_to_desktop", "create_folder", "write_local_file"] {
            assert!(names.contains(&tool.to_string()), "{tool} missing");
        }
    }
}

/// Map a tool name + parsed JSON output to a user-facing `ChatAction`-ish summary.
/// Returns `None` for tools that performed no side effect (e.g. read-only `list_entities`).
pub fn tool_effect_from_output(tool_name: &str, output_json: &str) -> Option<ToolEffect> {
    let val: Value = serde_json::from_str(output_json).ok()?;
    let ok = val.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let id = val.get("id").and_then(Value::as_str).map(str::to_string);
    let status = if ok { "completed" } else { "failed" };
    match tool_name {
        "create_note" => Some(ToolEffect {
            action_type: "create_note".to_string(),
            description: if ok {
                "Note captured".to_string()
            } else {
                "Note capture failed".to_string()
            },
            result_id: id,
            status: status.to_string(),
        }),
        "record_lesson" => Some(ToolEffect {
            action_type: "record_lesson".to_string(),
            description: if ok {
                "Lesson recorded".to_string()
            } else {
                "Lesson recording failed".to_string()
            },
            result_id: id,
            status: status.to_string(),
        }),
        "save_skill" => {
            let title = val.get("title").and_then(Value::as_str).unwrap_or("skill");
            Some(ToolEffect {
                action_type: "skill_saved".to_string(),
                description: if ok {
                    format!("Skill saved: {}", title)
                } else {
                    "Skill not saved".to_string()
                },
                result_id: id,
                status: status.to_string(),
            })
        }
        "create_task" => {
            let title = val.get("title").and_then(Value::as_str).unwrap_or("task");
            Some(ToolEffect {
                action_type: "create_task".to_string(),
                description: if ok {
                    format!("Task created: {}", title)
                } else {
                    "Task creation failed".to_string()
                },
                result_id: id,
                status: status.to_string(),
            })
        }
        "create_mission" => {
            let title = val
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("mission");
            let tasks = val
                .get("tasks_created")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            Some(ToolEffect {
                action_type: "create_mission".to_string(),
                description: if ok {
                    format!("Mission created: {} ({} tasks planned)", title, tasks)
                } else {
                    "Mission creation failed".to_string()
                },
                result_id: id,
                status: status.to_string(),
            })
        }
        "run_on_computer" => {
            let title = val
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("machine action");
            Some(ToolEffect {
                action_type: "action_request".to_string(),
                description: if ok {
                    format!("Waiting for your approval: {}", title)
                } else {
                    "Machine action failed to queue".to_string()
                },
                // The approval is what the phone acts on; the task rides along
                // in the approval row itself.
                result_id: val
                    .get("approvalId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(id),
                status: if ok {
                    "awaiting_approval".to_string()
                } else {
                    "failed".to_string()
                },
            })
        }
        "set_reminder" => Some(ToolEffect {
            action_type: "set_reminder".to_string(),
            description: if ok {
                "Reminder set".to_string()
            } else {
                "Reminder failed".to_string()
            },
            result_id: id,
            status: status.to_string(),
        }),
        "create_entity" => {
            let name = val.get("name").and_then(Value::as_str).unwrap_or("entity");
            Some(ToolEffect {
                action_type: "create_entity".to_string(),
                description: if ok {
                    format!("Entity created: {}", name)
                } else {
                    "Entity creation failed".to_string()
                },
                result_id: id,
                status: status.to_string(),
            })
        }
        "update_task" => Some(ToolEffect {
            action_type: "task_update".to_string(),
            description: if ok {
                "Task updated".to_string()
            } else {
                "Task update failed".to_string()
            },
            result_id: id,
            status: status.to_string(),
        }),
        "update_entity" => Some(ToolEffect {
            action_type: "entity_update".to_string(),
            description: if ok {
                "Entity context updated".to_string()
            } else {
                "Entity update failed".to_string()
            },
            result_id: id,
            status: status.to_string(),
        }),
        "manage_heartbeat" => Some(ToolEffect {
            action_type: "heartbeat_update".to_string(),
            description: if ok {
                "Heartbeat automation updated".to_string()
            } else {
                "Heartbeat update failed".to_string()
            },
            result_id: id,
            status: status.to_string(),
        }),
        "create_workspace" => {
            let name = val
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("workspace");
            Some(ToolEffect {
                action_type: "create_workspace".to_string(),
                description: if ok {
                    format!("Workspace created: {}", name)
                } else {
                    "Workspace creation failed".to_string()
                },
                result_id: id,
                status: status.to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

    async fn test_state_with_lessons() -> (
        ServerState,
        Arc<crate::director_lessons_store::SurrealDirectorLessonsStore>,
    ) {
        let memory: Arc<dyn MemoryStore> =
            Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
        let db = surrealdb::engine::any::connect("memory").await.unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        let lessons = crate::director_lessons_store::SurrealDirectorLessonsStore::new(Arc::new(db));
        lessons.init_schema().await.unwrap();
        let state = ServerState::new("test".into(), None, memory)
            .with_director_lessons_store(lessons.clone());
        (state, lessons)
    }

    #[tokio::test]
    async fn record_lesson_tool_persists_scoped_lesson() {
        let (state, lessons) = test_state_with_lessons().await;
        let tool = RecordLessonTool {
            state,
            workspace_id: Arc::from("ws_1"),
            entity_id: Some(Arc::from("ent_x")),
            principal_id: Arc::from("user_1"),
        };
        let result = tool
            .execute(json!({
                "lesson": "Always confirm units before pricing",
                "source": "correction",
                "severity": "hard"
            }))
            .await
            .unwrap();
        assert!(!result.is_error);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["kind"], "lesson");

        let rows = lessons.list_for_director("operations").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scope, "entity:ent_x");
        assert_eq!(rows[0].source, "correction");
        assert_eq!(rows[0].severity, "hard");
        assert_eq!(rows[0].created_by.as_deref(), Some("user_1"));

        let effect = tool_effect_from_output("record_lesson", &result.output).unwrap();
        assert_eq!(effect.description, "Lesson recorded");
        assert_eq!(effect.status, "completed");
    }

    #[tokio::test]
    async fn record_lesson_tool_rejects_empty_and_defaults_enums() {
        let (state, lessons) = test_state_with_lessons().await;
        let tool = RecordLessonTool {
            state,
            workspace_id: Arc::from("ws_1"),
            entity_id: None,
            principal_id: Arc::from("user_1"),
        };
        let empty = tool.execute(json!({ "lesson": "  " })).await.unwrap();
        assert!(empty.is_error);

        let bad_enums = tool
            .execute(json!({ "lesson": "prefer terse replies", "source": "bogus", "severity": "extreme" }))
            .await
            .unwrap();
        assert!(!bad_enums.is_error);
        let rows = lessons.list_for_director("operations").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].scope, "workspace:ws_1",
            "an entity-less lesson stays inside its workspace"
        );
        assert_eq!(rows[0].source, "observation", "unknown source falls back");
        assert_eq!(rows[0].severity, "soft", "unknown severity falls back");
    }
}
