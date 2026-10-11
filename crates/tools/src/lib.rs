//! The tools crate: every capability an agent can call as a function — browser automation,
//! filesystem access, shell execution, macOS system integrations, delegating to an external
//! coding-agent CLI, web search, raw HTTP, and WASM plugins. Each submodule implements the
//! [`Tool`] trait; [`ToolRegistry`] collects them so a model can list and invoke them by name.

pub mod browser;
pub mod coding_agent;
pub mod filesystem;
pub mod http;
pub mod shell;
pub mod system;
pub mod wasm_plugin;
pub mod web_search;

use async_trait::async_trait;
use consistent_world_common::types::ToolResult;

/// One callable capability: a stable name, a model-facing description, a JSON Schema for its
/// arguments, and the async entry point that actually runs it.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult>;
}

/// The set of tools available to an agent, looked up by name at call time.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Scan `local_plugins_dir` for installed plugins (each subdir with manifest.json + plugin.wasm),
    /// load each into `wasm_host`, and register a tool per plugin.
    pub fn register_plugins_from_dir(
        &mut self,
        wasm_host: std::sync::Arc<consistent_world_plugins::host::WasmHost>,
        local_plugins_dir: &str,
    ) -> anyhow::Result<()> {
        let registry = consistent_world_plugins::registry::PluginRegistry::new(
            String::new(),
            local_plugins_dir.to_string(),
        );
        let manifests = registry.list_installed()?;
        for m in manifests {
            let wasm_path = std::path::Path::new(local_plugins_dir)
                .join(&m.name)
                .join("plugin.wasm");
            if wasm_path.is_file() {
                if let Err(e) = wasm_host.load_plugin(&m.name, wasm_path.to_str().unwrap()) {
                    tracing::warn!("skip plugin {}: {}", m.name, e);
                    continue;
                }
                let tool = crate::wasm_plugin::WasmPluginTool::new(m.name, wasm_host.clone());
                self.register(Box::new(tool));
            }
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// The registered tools' name/description/schema, in the shape a model's tool-list prompt
    /// expects.
    pub fn list_definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters_schema(),
                })
            })
            .collect()
    }
}
