//! Integration tests for `ShellTool`: basic command execution, static deny-list rejection, and
//! `ShellPolicy` enforcement (working-dir override, cwd roots, hard-deny/approval-required
//! patterns, and empty-capability-list-under-deny-by-default).

use consistent_world_common::capabilities::CapabilityId;
use consistent_world_tools::Tool;
use consistent_world_tools::shell::{ShellPolicy, ShellTool};
use tempfile::TempDir;

#[tokio::test]
async fn test_shell_tool_basic() {
    let tool = ShellTool {
        allowed_commands: None,
        denied_commands: vec!["rm -rf".to_string()],
        working_dir: ".".to_string(),
        policy: None,
    };

    let res = tool
        .execute(serde_json::json!({ "command": "echo test", "timeout_secs": 5 }))
        .await
        .unwrap();
    assert!(!res.is_error);
    assert_eq!(res.output.trim(), "test");
}

#[tokio::test]
async fn test_shell_tool_denied() {
    let tool = ShellTool {
        allowed_commands: None,
        denied_commands: vec!["rm -rf".to_string()],
        working_dir: ".".to_string(),
        policy: None,
    };

    let res = tool
        .execute(serde_json::json!({ "command": "rm -rf /", "timeout_secs": 5 }))
        .await
        .unwrap();
    assert!(res.is_error);
}

#[tokio::test]
async fn test_shell_tool_policy_override_changes_working_dir() {
    let tmp = TempDir::new().unwrap();
    let entity = tmp.path().join("entity");
    std::fs::create_dir_all(&entity).unwrap();
    std::fs::write(entity.join("marker.txt"), "ok").unwrap();

    let tool = ShellTool {
        allowed_commands: None,
        denied_commands: vec![],
        working_dir: tmp.path().to_string_lossy().into_owned(),
        policy: Some(ShellPolicy {
            deny_by_default: true,
            allowed_capabilities: vec![CapabilityId::ShellExecSafe],
            approval_required_patterns: vec![],
            hard_deny_patterns: vec![],
            allowed_cwd_roots: vec![tmp.path().to_string_lossy().into_owned()],
        }),
    };

    let pwd = if cfg!(windows) {
        "(Get-Location).Path"
    } else {
        "pwd"
    };
    let res = tool
        .execute(serde_json::json!({
            "command": pwd,
            "_policy": {
                "workingDir": entity.to_string_lossy().into_owned(),
                "allowedCwdRoots": [entity.to_string_lossy().into_owned()]
            }
        }))
        .await
        .unwrap();
    assert!(!res.is_error, "shell override failed: {}", res.output);
    let expected = consistent_world_common::platform::process_path(&entity.canonicalize().unwrap());
    assert_eq!(res.output.trim(), expected.to_string_lossy());
}

#[tokio::test]
async fn test_shell_tool_policy_cwd_and_risk() {
    let tmp = TempDir::new().unwrap();
    let allowed = tmp.path().join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let blocked = tmp.path().join("blocked");
    std::fs::create_dir_all(&blocked).unwrap();

    let tool = ShellTool {
        allowed_commands: None,
        denied_commands: vec![],
        working_dir: allowed.to_string_lossy().into_owned(),
        policy: Some(ShellPolicy {
            deny_by_default: true,
            allowed_capabilities: vec![CapabilityId::ShellExecSafe],
            approval_required_patterns: vec!["sudo".to_string()],
            hard_deny_patterns: vec!["rm -rf /".to_string()],
            allowed_cwd_roots: vec![allowed.to_string_lossy().into_owned()],
        }),
    };

    let denied_cwd = tool
        .execute(serde_json::json!({
            "command": "echo hi",
            "cwd": blocked.to_string_lossy().into_owned()
        }))
        .await
        .unwrap();
    assert!(denied_cwd.is_error);
    assert!(denied_cwd.output.contains("SHELL_CWD_NOT_ALLOWED"));

    let approval = tool
        .execute(serde_json::json!({
            "command": "sudo echo hi",
            "cwd": allowed.to_string_lossy().into_owned()
        }))
        .await
        .unwrap();
    assert!(approval.is_error);
    assert!(approval.output.contains("SHELL_NEEDS_HUMAN_APPROVAL"));

    let hard_deny = tool
        .execute(serde_json::json!({
            "command": "rm -rf /",
            "cwd": allowed.to_string_lossy().into_owned()
        }))
        .await
        .unwrap();
    assert!(hard_deny.is_error);
    assert!(hard_deny.output.contains("SHELL_RISK_HARD_DENY"));
}

#[tokio::test]
async fn test_shell_tool_empty_capabilities_denies_under_deny_by_default() {
    let tmp = TempDir::new().unwrap();
    let tool = ShellTool {
        allowed_commands: None,
        denied_commands: vec![],
        working_dir: tmp.path().to_string_lossy().into_owned(),
        policy: Some(ShellPolicy {
            deny_by_default: true,
            allowed_capabilities: vec![],
            approval_required_patterns: vec![],
            hard_deny_patterns: vec![],
            allowed_cwd_roots: vec![tmp.path().to_string_lossy().into_owned()],
        }),
    };

    let res = tool
        .execute(serde_json::json!({ "command": "echo hi", "timeout_secs": 5 }))
        .await
        .unwrap();
    assert!(res.is_error, "empty capability list must deny, got: {}", res.output);
    assert!(res.output.contains("SHELL_CAPABILITY_DENIED"));
}
