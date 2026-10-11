//! Tests for `SubAgentManager`: spawn/list/kill bookkeeping and the
//! max_concurrent limit.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use consistent_world_agents::subagent::{SubAgentManager, SubAgentRunner};

// Completes immediately; used where we don't care what the sub-agent does.
struct NoOpRunner;

impl SubAgentRunner for NoOpRunner {
    fn run(
        &self,
        _parent_agent_id: String,
        _task_description: String,
        _session_id: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async {})
    }
}

/// Runner that never completes (so we can fill max_concurrent slots).
struct BlockingRunner;

impl SubAgentRunner for BlockingRunner {
    fn run(
        &self,
        _parent_agent_id: String,
        _task_description: String,
        _session_id: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        })
    }
}

#[tokio::test]
async fn test_subagent_spawn_and_kill() {
    let runner: Arc<dyn SubAgentRunner> = Arc::new(NoOpRunner);
    let manager = Arc::new(SubAgentManager::new(5, runner));

    let id = manager
        .spawn(
            "parent".to_string(),
            "do something".to_string(),
            "session-1".to_string(),
        )
        .unwrap();
    assert!(!id.is_empty());

    let list = manager.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].parent_id, "parent");
    assert_eq!(list[0].task_description, "do something");

    manager.kill(&id).unwrap();
    let list_after = manager.list();
    assert!(list_after.is_empty());
}

#[tokio::test]
async fn test_subagent_max_concurrent() {
    let runner: Arc<dyn SubAgentRunner> = Arc::new(BlockingRunner);
    let manager = Arc::new(SubAgentManager::new(2, runner));

    let id1 = manager.spawn("p".into(), "t1".into(), "s1".into()).unwrap();
    let id2 = manager.spawn("p".into(), "t2".into(), "s2".into()).unwrap();
    assert_eq!(manager.active_count(), 2);

    let err = manager.spawn("p".into(), "t3".into(), "s3".into());
    assert!(err.is_err());

    manager.kill(&id1).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let id3 = manager.spawn("p".into(), "t3".into(), "s3".into()).unwrap();
    assert_eq!(manager.active_count(), 2);

    manager.kill(&id2).unwrap();
    manager.kill(&id3).unwrap();
}
