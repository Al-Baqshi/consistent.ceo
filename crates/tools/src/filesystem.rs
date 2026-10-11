//! The `filesystem` tool: read/write/list/search files under a workspace root, sandboxed so a
//! path can never resolve outside that root. Runs unattended (no approval prompt) as part of
//! the executor's "code/file work inside a directory jail" posture — the jail itself is this
//! module's job.

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::capabilities::CapabilityId;
use consistent_world_common::types::ToolResult;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const MAX_FILE_SIZE_SEARCH: u64 = 1024 * 1024; // 1MB per file for search

/// Executes `read`/`write`/`list`/`search` actions confined to `workspace_root`, optionally
/// further restricted by `policy`.
pub struct FileSystemTool {
    pub workspace_root: String,
    pub policy: Option<FileSystemPolicy>,
}

/// Per-call `_policy` override, deserialized from the `_policy` argument the task-execution path
/// injects. Any field left unset keeps the tool's own configured value.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileSystemPolicyOverride {
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    allowed_capabilities: Vec<CapabilityId>,
    #[serde(default)]
    read_roots: Vec<String>,
    #[serde(default)]
    write_roots: Vec<String>,
    #[serde(default)]
    list_roots: Vec<String>,
    #[serde(default)]
    search_roots: Vec<String>,
}

/// Access-control policy for the filesystem tool: which capabilities are granted and, per
/// action, which roots a resolved path must fall under.
#[derive(Debug, Clone)]
pub struct FileSystemPolicy {
    pub deny_by_default: bool,
    pub allowed_capabilities: Vec<CapabilityId>,
    pub read_roots: Vec<String>,
    pub write_roots: Vec<String>,
    pub list_roots: Vec<String>,
    pub search_roots: Vec<String>,
}

#[derive(Debug)]
struct PolicyError {
    code: &'static str,
    message: String,
}

impl FileSystemTool {
    // The workspace root a call resolves paths against: the `_policy.workspaceRoot` override
    // when present, else the tool's own configured root.
    fn effective_workspace_root(&self, arguments: &serde_json::Value) -> String {
        arguments
            .get("_policy")
            .cloned()
            .and_then(|v| serde_json::from_value::<FileSystemPolicyOverride>(v).ok())
            .and_then(|p| p.workspace_root)
            .unwrap_or_else(|| self.workspace_root.clone())
    }

    // Merge the `_policy` override onto the tool's configured policy: any list the override
    // sets replaces the corresponding list wholesale (empty means "not overridden", not "clear
    // it"), so a caller must pass every root it wants to keep, not just the ones it's adding.
    fn effective_policy(&self, arguments: &serde_json::Value) -> Option<FileSystemPolicy> {
        let mut policy = self.policy.clone()?;
        let Some(override_policy) = arguments
            .get("_policy")
            .cloned()
            .and_then(|v| serde_json::from_value::<FileSystemPolicyOverride>(v).ok())
        else {
            return Some(policy);
        };
        if !override_policy.allowed_capabilities.is_empty() {
            policy.allowed_capabilities = override_policy.allowed_capabilities;
        }
        if !override_policy.read_roots.is_empty() {
            policy.read_roots = override_policy.read_roots;
        }
        if !override_policy.write_roots.is_empty() {
            policy.write_roots = override_policy.write_roots;
        }
        if !override_policy.list_roots.is_empty() {
            policy.list_roots = override_policy.list_roots;
        }
        if !override_policy.search_roots.is_empty() {
            policy.search_roots = override_policy.search_roots;
        }
        Some(policy)
    }

    /// Resolve and validate path: must exist and be under workspace_root, no `../` escapes.
    fn resolve_path_with_root(&self, root_str: &str, path: &str) -> anyhow::Result<PathBuf> {
        let root = Path::new(root_str)
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("workspace_root invalid: {}", e))?;
        let p = root.join(path.trim_start_matches('/'));
        let canonical = p
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("path invalid or not found: {}", e))?;
        if !canonical.starts_with(&root) {
            anyhow::bail!("path escapes workspace (sandbox violation)");
        }
        Ok(canonical)
    }

    /// For paths that may not exist yet (e.g. write): normalize `..` and `.`, ensure under root.
    fn resolve_path_for_write_with_root(
        &self,
        root_str: &str,
        path: &str,
    ) -> anyhow::Result<PathBuf> {
        let root = Path::new(root_str)
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("workspace_root invalid: {}", e))?;
        let mut resolved = root.clone();
        for comp in Path::new(path.trim_start_matches('/')).components() {
            match comp {
                std::path::Component::ParentDir => {
                    resolved = resolved.parent().unwrap_or(&root).to_path_buf();
                    if !resolved.starts_with(&root) {
                        anyhow::bail!("path escapes workspace (sandbox violation)");
                    }
                }
                std::path::Component::CurDir => {}
                _ => resolved = resolved.join(comp.as_os_str()),
            }
        }
        if !resolved.starts_with(&root) {
            anyhow::bail!("path escapes workspace (sandbox violation)");
        }
        Ok(resolved)
    }

    fn policy_error(code: &'static str, message: impl Into<String>) -> ToolResult {
        let payload = serde_json::json!({
            "type": "policy_denied",
            "code": code,
            "message": message.into(),
        });
        ToolResult {
            call_id: String::new(),
            output: payload.to_string(),
            is_error: true,
        }
    }

    // Which configured root list governs a given action, if a policy is set at all.
    // Which root list governs an action, *for the policy actually in force*.
    //
    // This takes the policy as an argument rather than reading `self.policy`:
    // the per-call `_policy` override is merged in `effective_policy`, and
    // reading the constructed policy here silently discarded every overridden
    // root — the caller's roots were computed and then ignored, so the jail in
    // force was never the jail the caller asked for.
    fn roots_for_action<'a>(policy: &'a FileSystemPolicy, action: &str) -> &'a [String] {
        match action {
            "read" => policy.read_roots.as_slice(),
            "write" => policy.write_roots.as_slice(),
            "list" => policy.list_roots.as_slice(),
            "search" => policy.search_roots.as_slice(),
            _ => &[],
        }
    }

    // Enforce the policy for one already-resolved path: first the action's required capability
    // (read/write/list/search), then, if roots are configured for that action, that the path
    // actually falls under one of them.
    fn check_policy(
        &self,
        policy: Option<&FileSystemPolicy>,
        action: &str,
        resolved: &Path,
    ) -> Result<(), PolicyError> {
        let Some(policy) = policy else {
            return Ok(());
        };
        let required_capability = match action {
            "read" | "list" | "search" => Some(CapabilityId::FsRead),
            "write" => Some(CapabilityId::FsWrite),
            _ => None,
        };
        if let Some(required) = required_capability {
            // An empty capability list under deny_by_default means "nothing
            // allowed", not "everything allowed".
            let capability_allowed = if policy.allowed_capabilities.is_empty() {
                !policy.deny_by_default
            } else {
                policy.allowed_capabilities.iter().any(|c| c == &required)
            };
            if !capability_allowed {
                return Err(PolicyError {
                    code: "FS_CAPABILITY_DENIED",
                    message: format!("capability '{}' is not allowed", required),
                });
            }
        }
        let roots = Self::roots_for_action(policy, action);
        if roots.is_empty() && !policy.deny_by_default {
            return Ok(());
        }
        let mut canonical_roots: Vec<PathBuf> = Vec::new();
        for root in roots {
            let root_path = Path::new(root);
            let canonical = root_path.canonicalize().map_err(|_| PolicyError {
                code: "FS_POLICY_ROOT_INVALID",
                message: format!("configured root is invalid: {}", root),
            })?;
            canonical_roots.push(canonical);
        }

        if canonical_roots.iter().any(|r| resolved.starts_with(r)) {
            return Ok(());
        }

        if policy.deny_by_default {
            return Err(PolicyError {
                code: "FS_PATH_NOT_ALLOWED",
                message: format!(
                    "action '{}' denied for path '{}' (outside allowed roots)",
                    action,
                    resolved.display()
                ),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl Tool for FileSystemTool {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    fn description(&self) -> &'static str {
        "Read, write, search, and list files within the workspace"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["read", "write", "list", "search"] },
                "path": { "type": "string" },
                "content": { "type": "string" },
                "query": { "type": "string" }
            },
            "required": ["action", "path"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("filesystem tool missing `action`"))?;
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("filesystem tool missing `path`"))?;

        let workspace_root = self.effective_workspace_root(&arguments);
        let effective_policy = self.effective_policy(&arguments);

        let output = match action {
            "read" => {
                let resolved = match self.resolve_path_with_root(&workspace_root, path) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                };
                if let Err(e) = self.check_policy(effective_policy.as_ref(), "read", &resolved) {
                    return Ok(Self::policy_error(e.code, e.message));
                }
                if !resolved.is_file() {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: "Not a file".to_string(),
                        is_error: true,
                    });
                }
                match tokio::fs::read_to_string(&resolved).await {
                    Ok(c) => c,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                }
            }
            "write" => {
                let content = arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let resolved = match self.resolve_path_for_write_with_root(&workspace_root, path) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                };
                if let Err(e) = self.check_policy(effective_policy.as_ref(), "write", &resolved) {
                    return Ok(Self::policy_error(e.code, e.message));
                }
                if let Some(parent) = resolved.parent()
                    && let Err(e) = tokio::fs::create_dir_all(parent).await
                {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: e.to_string(),
                        is_error: true,
                    });
                }
                if let Err(e) = tokio::fs::write(&resolved, content).await {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: e.to_string(),
                        is_error: true,
                    });
                }
                format!("Wrote {} bytes to {}", content.len(), resolved.display())
            }
            "list" => {
                let resolved = match self.resolve_path_with_root(&workspace_root, path) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                };
                if let Err(e) = self.check_policy(effective_policy.as_ref(), "list", &resolved) {
                    return Ok(Self::policy_error(e.code, e.message));
                }
                if !resolved.is_dir() {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: "Not a directory".to_string(),
                        is_error: true,
                    });
                }
                let mut entries = Vec::new();
                let mut read_dir = match tokio::fs::read_dir(&resolved).await {
                    Ok(d) => d,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                };
                loop {
                    let entry = match read_dir.next_entry().await {
                        Ok(Some(e)) => e,
                        Ok(None) => break,
                        Err(e) => {
                            return Ok(ToolResult {
                                call_id: String::new(),
                                output: e.to_string(),
                                is_error: true,
                            });
                        }
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let kind = match entry.file_type().await {
                        Ok(ft) if ft.is_dir() => "dir",
                        _ => "file",
                    };
                    entries.push(format!("{} {}", kind, name));
                }
                entries.sort();
                entries.join("\n")
            }
            "search" => {
                let query = arguments
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if query.is_empty() {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: "search action requires `query`".to_string(),
                        is_error: true,
                    });
                }
                let resolved = match self.resolve_path_with_root(&workspace_root, path) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                };
                if let Err(e) = self.check_policy(effective_policy.as_ref(), "search", &resolved) {
                    return Ok(Self::policy_error(e.code, e.message));
                }
                if !resolved.is_dir() {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: "search path must be a directory".to_string(),
                        is_error: true,
                    });
                }
                // Results are reported relative to the tool's own configured root, not a
                // per-call `_policy` override, so paths stay stable across calls with different
                // overrides.
                let root = match Path::new(&self.workspace_root).canonicalize() {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: e.to_string(),
                            is_error: true,
                        });
                    }
                };
                let mut matches = Vec::new();
                for entry in WalkDir::new(&resolved)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| !e.path().starts_with(".git"))
                {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let rel = path
                        .strip_prefix(&root)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| path.display().to_string());
                    let meta = match std::fs::metadata(path) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if meta.len() > MAX_FILE_SIZE_SEARCH {
                        continue;
                    }
                    let content = match std::fs::read_to_string(path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    for (i, line) in content.lines().enumerate() {
                        if line.contains(query) {
                            matches.push(format!("{}:{}:{}", rel, i + 1, line.trim()));
                        }
                    }
                }
                matches.join("\n")
            }
            _ => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Unknown action: {}", action),
                    is_error: true,
                });
            }
        };

        Ok(ToolResult {
            call_id: String::new(),
            output,
            is_error: false,
        })
    }
}
