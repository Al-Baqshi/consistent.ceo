//! Tests for `CodingAgentTool`: an `#[ignore]`d smoke test that actually shells out to the
//! `opencode` CLI (needs it installed and a real repo path), plus policy checks that don't
//! spawn anything (disallowed `kind`, cwd outside `allowed_cwd_roots`).

use consistent_world_tools::Tool;
use consistent_world_tools::coding_agent::CodingAgentTool;
use serde_json::json;

// Requires the `opencode` CLI on PATH and a real checkout at the hardcoded path below; run
// explicitly with `--ignored`.
#[tokio::test]
#[ignore]
async fn smoke_opencode_run_in_consistent_world_repo() {
    let tool = CodingAgentTool {
        working_dir: "/Users/mbaqshi/dev/consistent-world".to_string(),
        allowed_cwd_roots: vec!["/Users/mbaqshi/dev/consistent-world".to_string()],
        default_timeout_secs: 120,
        max_timeout_secs: 120,
        ..CodingAgentTool::default()
    };

    let result = tool
        .execute(json!({
            "kind": "opencode",
            "prompt": "In one short sentence, what kind of project is this? Do not edit any files.",
            "timeout_secs": 90
        }))
        .await
        .expect("tool execution should not error");

    println!("is_error = {}", result.is_error);
    println!("output:\n{}", result.output);
    assert!(!result.output.is_empty(), "expected some output");
}

#[tokio::test]
async fn rejects_disallowed_kind() {
    let tool = CodingAgentTool {
        allowed_kinds: vec!["opencode".to_string()],
        ..CodingAgentTool::default()
    };
    let result = tool
        .execute(json!({ "kind": "claude", "prompt": "hi" }))
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result.output.contains("CODING_AGENT_KIND_NOT_ALLOWED"));
}

#[tokio::test]
async fn rejects_cwd_outside_allowed_roots() {
    let tool = CodingAgentTool {
        working_dir: "/tmp".to_string(),
        allowed_cwd_roots: vec!["/tmp".to_string()],
        ..CodingAgentTool::default()
    };
    let result = tool
        .execute(json!({
            "kind": "opencode",
            "prompt": "hi",
            "cwd": "/etc"
        }))
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result.output.contains("CODING_AGENT_CWD_NOT_ALLOWED"));
}
