//! Tool wrapper for WASM plugins: each loaded plugin is exposed as a tool that calls WasmHost::call_plugin.

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::types::ToolResult;
use consistent_world_plugins::host::WasmHost;
use std::sync::Arc;

/// Adapts one loaded WASM plugin to the [`Tool`] interface, so it shows up in the registry
/// alongside the built-in tools.
pub struct WasmPluginTool {
    /// Display name (must be 'static for trait; set at construction via leak).
    name: &'static str,
    /// Plugin name passed to WasmHost::call_plugin.
    plugin_name: String,
    host: Arc<WasmHost>,
}

impl WasmPluginTool {
    /// Creates a tool for the given plugin. The name is used as the tool name and must be unique.
    pub fn new(plugin_name: String, host: Arc<WasmHost>) -> Self {
        let name: &'static str = Box::leak(plugin_name.clone().into_boxed_str());
        Self {
            name,
            plugin_name,
            host,
        }
    }
}

#[async_trait]
impl Tool for WasmPluginTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "WASM plugin (run plugin_execute with JSON arguments)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "input": { "type": "object", "description": "JSON object passed to the plugin" }
            }
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let args_bytes = serde_json::to_vec(&arguments).unwrap_or_else(|_| b"{}".to_vec());
        match self.host.call_plugin(&self.plugin_name, &args_bytes) {
            Ok(out) => {
                let output = String::from_utf8_lossy(&out).into_owned();
                Ok(ToolResult {
                    call_id: String::new(),
                    output,
                    is_error: false,
                })
            }
            Err(e) => Ok(ToolResult {
                call_id: String::new(),
                output: e.to_string(),
                is_error: true,
            }),
        }
    }
}
