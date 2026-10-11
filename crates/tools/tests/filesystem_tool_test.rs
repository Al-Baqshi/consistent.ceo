//! Integration tests for `FileSystemTool`: read/write/list/search happy paths, the sandbox that
//! rejects paths escaping the workspace root, and `_policy` overrides (scoped roots, deny-by-
//! default enforcement).

use consistent_world_common::capabilities::CapabilityId;
use consistent_world_tools::Tool;
use consistent_world_tools::filesystem::{FileSystemPolicy, FileSystemTool};
use tempfile::TempDir;

#[tokio::test]
async fn test_filesystem_read_write() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tool = FileSystemTool {
        workspace_root: root.to_string_lossy().into_owned(),
        policy: None,
    };

    let write_res = tool
        .execute(serde_json::json!({
            "action": "write",
            "path": "foo/bar.txt",
            "content": "hello world"
        }))
        .await
        .unwrap();
    assert!(!write_res.is_error, "write failed: {}", write_res.output);

    let read_res = tool
        .execute(serde_json::json!({
            "action": "read",
            "path": "foo/bar.txt"
        }))
        .await
        .unwrap();
    assert!(!read_res.is_error, "read failed: {}", read_res.output);
    assert_eq!(read_res.output.trim(), "hello world");
}

#[tokio::test]
async fn test_filesystem_path_sandbox() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tool = FileSystemTool {
        workspace_root: root.to_string_lossy().into_owned(),
        policy: None,
    };

    // Attempt to escape with ../ into something like /etc/passwd
    let escape = "../../etc/passwd".to_string();
    let res = tool
        .execute(serde_json::json!({
            "action": "read",
            "path": escape
        }))
        .await
        .unwrap();
    assert!(
        res.is_error
            && (res.output.contains("sandbox")
                || res.output.contains("escapes")
                || res.output.contains("invalid")),
        "expected sandbox rejection, got: {}",
        res.output
    );

    // Write with .. should also be rejected
    let write_escape = tool
        .execute(serde_json::json!({
            "action": "write",
            "path": "../../tmp/outside",
            "content": "no"
        }))
        .await
        .unwrap();
    assert!(
        write_escape.is_error,
        "write escape should be rejected: {}",
        write_escape.output
    );
}

#[tokio::test]
async fn test_filesystem_list() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/f1.txt"), "x").unwrap();
    std::fs::write(root.join("a/f2.txt"), "y").unwrap();

    let tool = FileSystemTool {
        workspace_root: root.to_string_lossy().into_owned(),
        policy: None,
    };
    let res = tool
        .execute(serde_json::json!({ "action": "list", "path": "a" }))
        .await
        .unwrap();
    assert!(!res.is_error);
    let lines: Vec<&str> = res.output.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().any(|l| l.contains("f1.txt")));
    assert!(lines.iter().any(|l| l.contains("f2.txt")));
}

#[tokio::test]
async fn test_filesystem_search() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("b/one.txt"), "alpha\nneedle\nbeta").unwrap();
    std::fs::write(root.join("b/two.txt"), "no needle here").unwrap();

    let tool = FileSystemTool {
        workspace_root: root.to_string_lossy().into_owned(),
        policy: None,
    };
    let res = tool
        .execute(serde_json::json!({
            "action": "search",
            "path": "b",
            "query": "needle"
        }))
        .await
        .unwrap();
    assert!(!res.is_error);
    let lines: Vec<&str> = res.output.lines().collect();
    assert!(lines.len() >= 2);
    assert!(res.output.contains("needle"));
}

#[tokio::test]
async fn test_filesystem_policy_override_changes_workspace_and_roots() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("entity")).unwrap();
    std::fs::write(root.join("entity/in.txt"), "scoped").unwrap();

    let tool = FileSystemTool {
        workspace_root: root.to_string_lossy().into_owned(),
        policy: Some(FileSystemPolicy {
            deny_by_default: true,
            allowed_capabilities: vec![CapabilityId::FsRead],
            read_roots: vec![root.to_string_lossy().into_owned()],
            write_roots: vec![],
            list_roots: vec![root.to_string_lossy().into_owned()],
            search_roots: vec![root.to_string_lossy().into_owned()],
        }),
    };

    let scoped = tool
        .execute(serde_json::json!({
            "action": "read",
            "path": "in.txt",
            "_policy": {
                "workspaceRoot": root.join("entity").to_string_lossy().into_owned(),
                "readRoots": [root.join("entity").to_string_lossy().into_owned()]
            }
        }))
        .await
        .unwrap();
    assert!(!scoped.is_error, "scoped read failed: {}", scoped.output);
    assert_eq!(scoped.output.trim(), "scoped");
}

#[tokio::test]
async fn test_filesystem_policy_denies_outside_allowed_root() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("allowed")).unwrap();
    std::fs::write(root.join("allowed/in.txt"), "ok").unwrap();
    std::fs::create_dir_all(root.join("blocked")).unwrap();
    std::fs::write(root.join("blocked/no.txt"), "no").unwrap();

    let allowed_root = root.join("allowed");
    let tool = FileSystemTool {
        workspace_root: root.to_string_lossy().into_owned(),
        policy: Some(FileSystemPolicy {
            deny_by_default: true,
            allowed_capabilities: vec![CapabilityId::FsRead, CapabilityId::FsWrite],
            read_roots: vec![allowed_root.to_string_lossy().into_owned()],
            write_roots: vec![allowed_root.to_string_lossy().into_owned()],
            list_roots: vec![allowed_root.to_string_lossy().into_owned()],
            search_roots: vec![allowed_root.to_string_lossy().into_owned()],
        }),
    };

    let denied = tool
        .execute(serde_json::json!({
            "action": "read",
            "path": "blocked/no.txt"
        }))
        .await
        .unwrap();
    assert!(denied.is_error);
    assert!(denied.output.contains("FS_PATH_NOT_ALLOWED"));

    let allowed = tool
        .execute(serde_json::json!({
            "action": "read",
            "path": "allowed/in.txt"
        }))
        .await
        .unwrap();
    assert!(!allowed.is_error, "allowed read failed: {}", allowed.output);
    assert_eq!(allowed.output.trim(), "ok");
}

#[tokio::test]
async fn a_per_call_root_override_actually_governs_the_jail() {
    // Regression: `check_policy` merged the `_policy` override and then read
    // roots from the *constructed* policy, so an override's roots were computed
    // and discarded — the jail in force was never the one the caller asked for.
    let tmp = TempDir::new().unwrap();
    let allowed = tmp.path().join("allowed");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("secret.txt"), "nope").unwrap();

    // Constructed permissively over the whole temp dir...
    let tool = FileSystemTool {
        workspace_root: tmp.path().to_string_lossy().into_owned(),
        policy: Some(FileSystemPolicy {
            deny_by_default: true,
            allowed_capabilities: vec![CapabilityId::FsRead],
            read_roots: vec![tmp.path().to_string_lossy().into_owned()],
            write_roots: vec![],
            list_roots: vec![],
            search_roots: vec![],
        }),
    };

    // ...but this call narrows the jail to `allowed/` only.
    let res = tool
        .execute(serde_json::json!({
            "action": "read",
            "path": "other/secret.txt",
            "_policy": {
                "readRoots": [allowed.to_string_lossy().into_owned()],
                "allowedCapabilities": ["fs.read"]
            }
        }))
        .await
        .unwrap();
    assert!(
        res.is_error,
        "the override's narrower root must be enforced, got: {}",
        res.output
    );
    assert!(
        res.output.contains("FS_PATH_NOT_ALLOWED"),
        "got: {}",
        res.output
    );
}
