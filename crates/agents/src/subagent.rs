//! Sub-agents: spawn background tasks that run the agent loop, with max_concurrent and kill support.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use async_trait::async_trait;
use consistent_world_common::types::ToolResult;
use consistent_world_tools::Tool;

/// Called when a sub-agent task is started. Typically dispatches a synthetic message to the agent.
pub trait SubAgentRunner: Send + Sync {
    fn run(
        &self,
        parent_agent_id: String,
        task_description: String,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Manages spawned sub-agent tasks: spawn (with max_concurrent), kill, list.
pub struct SubAgentManager {
    max_concurrent: usize,
    runner: Arc<dyn SubAgentRunner>,
    active: Mutex<Vec<SubAgentHandle>>,
}

struct SubAgentHandle {
    id: String,
    parent_id: String,
    task_description: String,
    session_id: String,
    join: JoinHandle<()>,
}

/// Public view of a running sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentStatus {
    pub id: String,
    pub parent_id: String,
    pub task_description: String,
    pub session_id: String,
}

impl SubAgentManager {
    pub fn new(max_concurrent: usize, runner: Arc<dyn SubAgentRunner>) -> Self {
        Self {
            max_concurrent,
            runner,
            active: Mutex::new(Vec::new()),
        }
    }

    /// Remove completed handles from active.
    fn prune_completed(&self) {
        let mut guard = self.active.lock().expect("subagent mutex poisoned");
        guard.retain(|h| !h.join.is_finished());
    }

    /// Spawn a background task. Returns sub-agent id. Errors if at max_concurrent.
    pub fn spawn(
        &self,
        parent_agent_id: String,
        task_description: String,
        session_id: String,
    ) -> anyhow::Result<String> {
        self.prune_completed();
        let guard = self.active.lock().expect("subagent mutex poisoned");
        if guard.len() >= self.max_concurrent {
            anyhow::bail!("max concurrent subagents ({}) reached", self.max_concurrent);
        }
        drop(guard);

        let id = Uuid::new_v4().to_string();
        let runner = self.runner.clone();
        let pa = parent_agent_id.clone();
        let td = task_description.clone();
        let si = session_id.clone();
        let join = tokio::spawn(async move {
            runner.run(pa, td, si).await;
        });

        let handle = SubAgentHandle {
            id: id.clone(),
            parent_id: parent_agent_id,
            task_description,
            session_id,
            join,
        };
        self.active
            .lock()
            .expect("subagent mutex poisoned")
            .push(handle);

        Ok(id)
    }

    /// Abort a sub-agent by id. Returns Ok(()) if found and aborted.
    pub fn kill(&self, id: &str) -> anyhow::Result<()> {
        let mut guard = self.active.lock().expect("subagent mutex poisoned");
        if let Some(pos) = guard.iter().position(|h| h.id == id) {
            let handle = guard.remove(pos);
            handle.join.abort();
            return Ok(());
        }
        anyhow::bail!("subagent not found: {}", id)
    }

    /// List currently running sub-agents (does not prune; callers may want to spawn after list).
    pub fn list(&self) -> Vec<SubAgentStatus> {
        let guard = self.active.lock().expect("subagent mutex poisoned");
        guard
            .iter()
            .map(|h| SubAgentStatus {
                id: h.id.clone(),
                parent_id: h.parent_id.clone(),
                task_description: h.task_description.clone(),
                session_id: h.session_id.clone(),
            })
            .collect()
    }

    /// Count of tracked sub-agent handles (does not prune completed ones first).
    pub fn active_count(&self) -> usize {
        self.active.lock().expect("subagent mutex poisoned").len()
    }
}

// --- Built-in tools for LLM (subagent.spawn, subagent.kill, subagent.list) ---

/// Tool: subagent.spawn — spawn a background task run by the same agent.
pub struct SubAgentSpawnTool {
    manager: Arc<SubAgentManager>,
    parent_agent_id: String,
}

impl SubAgentSpawnTool {
    /// Bind this tool instance to a manager and the agent that owns it
    /// (spawned sub-agents are attributed to `parent_agent_id`).
    pub fn new(manager: Arc<SubAgentManager>, parent_agent_id: String) -> Self {
        Self {
            manager,
            parent_agent_id,
        }
    }
}

#[async_trait]
impl Tool for SubAgentSpawnTool {
    fn name(&self) -> &'static str {
        "subagent.spawn"
    }

    fn description(&self) -> &'static str {
        "Spawn a background sub-agent to run a task. Returns the sub-agent id. Use subagent.kill to stop it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_description": { "type": "string", "description": "What the sub-agent should do" },
                "session_id": { "type": "string", "description": "Session id for the sub-agent run" }
            },
            "required": ["task_description", "session_id"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let task_description = arguments
            .get("task_description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = arguments
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let id = self
            .manager
            .spawn(self.parent_agent_id.clone(), task_description, session_id)?;
        Ok(ToolResult {
            call_id: String::new(),
            output: format!("Spawned sub-agent id: {}", id),
            is_error: false,
        })
    }
}

/// Tool: subagent.kill — stop a running sub-agent by id.
pub struct SubAgentKillTool {
    manager: Arc<SubAgentManager>,
}

impl SubAgentKillTool {
    /// Bind this tool instance to the manager it should kill sub-agents on.
    pub fn new(manager: Arc<SubAgentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SubAgentKillTool {
    fn name(&self) -> &'static str {
        "subagent.kill"
    }

    fn description(&self) -> &'static str {
        "Kill a running sub-agent by id. Use subagent.list to see active ids."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Sub-agent id to kill" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let id = arguments
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("subagent.kill requires \"id\" string"))?;
        match self.manager.kill(id) {
            Ok(()) => Ok(ToolResult {
                call_id: String::new(),
                output: format!("Killed sub-agent {}", id),
                is_error: false,
            }),
            Err(e) => Ok(ToolResult {
                call_id: String::new(),
                output: e.to_string(),
                is_error: true,
            }),
        }
    }
}

/// Tool: subagent.list — list running sub-agents.
pub struct SubAgentListTool {
    manager: Arc<SubAgentManager>,
}

impl SubAgentListTool {
    /// Bind this tool instance to the manager it should list sub-agents from.
    pub fn new(manager: Arc<SubAgentManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for SubAgentListTool {
    fn name(&self) -> &'static str {
        "subagent.list"
    }

    fn description(&self) -> &'static str {
        "List currently running sub-agents (id, parent_id, task_description, session_id)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let list = self.manager.list();
        let lines: Vec<String> = list
            .iter()
            .map(|s| {
                format!(
                    "id={} parent={} session={} task=\"{}\"",
                    s.id,
                    s.parent_id,
                    s.session_id,
                    s.task_description.replace('\n', " ")
                )
            })
            .collect();
        let output = if lines.is_empty() {
            "No active sub-agents.".to_string()
        } else {
            lines.join("\n")
        };
        Ok(ToolResult {
            call_id: String::new(),
            output,
            is_error: false,
        })
    }
}

/// Register the three subagent tools (subagent.spawn, subagent.kill, subagent.list) into an
/// existing tool registry. Call this when building an agent that should support spawning
/// background sub-agents.
pub fn register_subagent_tools(
    registry: &mut consistent_world_tools::ToolRegistry,
    manager: Arc<SubAgentManager>,
    parent_agent_id: String,
) {
    registry.register(Box::new(SubAgentSpawnTool::new(
        manager.clone(),
        parent_agent_id,
    )));
    registry.register(Box::new(SubAgentKillTool::new(manager.clone())));
    registry.register(Box::new(SubAgentListTool::new(manager)));
}
