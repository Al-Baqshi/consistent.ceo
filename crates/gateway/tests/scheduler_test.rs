//! Tests the cron scheduler's tick-and-dispatch loop against a real
//! Surreal-backed `ScheduleStore`.
//!
//! `run_one_tick` is driven directly rather than through a timer, so the
//! test is deterministic.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Utc;
use consistent_world_gateway::scheduler::{
    ScheduleDispatcher, ScheduleStore, ScheduledDispatchParams, ScheduledTask, Scheduler,
    SurrealScheduleStore,
};
use consistent_world_memory::store::SurrealMemoryStore;
use uuid::Uuid;

/// A `ScheduleDispatcher` test double that just counts how many times it fired.
struct CountingDispatcher {
    count: AtomicUsize,
}

impl ScheduleDispatcher for CountingDispatcher {
    fn dispatch(
        &self,
        _params: ScheduledDispatchParams,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

#[tokio::test]
async fn test_scheduler_cron_fires() {
    let memory_store = SurrealMemoryStore::connect_memory().await.unwrap();
    let db = memory_store.db();
    let store: Arc<dyn ScheduleStore> = Arc::new(SurrealScheduleStore::new(db));
    let count = Arc::new(CountingDispatcher {
        count: AtomicUsize::new(0),
    });
    let dispatcher: Arc<dyn ScheduleDispatcher> = count.clone();
    let scheduler = Arc::new(Scheduler::new(store.clone(), dispatcher, 1));

    let id = Uuid::new_v4().to_string();
    let task = ScheduledTask {
        id: id.clone(),
        cron_expression: "0 * * * * *".to_string(),
        next_run: Utc::now(),
        agent_id: "agent1".to_string(),
        task_description: "test task".to_string(),
        entity_id: None,
        connection_id: None,
        workspace_id: None,
        created_by: None,
    };
    store.save(&task).await.unwrap();

    scheduler.run_one_tick().await.unwrap();

    assert_eq!(count.count.load(Ordering::SeqCst), 1);
}
