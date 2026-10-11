//! Cron-based scheduler: persist tasks to store (SurrealDB or SQLite), poll at interval, dispatch to agent.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

/// Dispatcher called when a scheduled task fires. Typically creates a synthetic message and runs the agent.
pub trait ScheduleDispatcher: Send + Sync {
    fn dispatch(&self, params: ScheduledDispatchParams)
    -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Everything the dispatcher needs to turn one firing into a synthetic chat
/// message and agent run.
#[derive(Clone, Debug)]
pub struct ScheduledDispatchParams {
    pub agent_id: String,
    pub task_description: String,
    pub session_id: String,
    pub entity_id: Option<String>,
    pub connection_id: Option<String>,
}

/// A persisted cron schedule: what to run next, and when.
#[derive(Clone, Debug)]
pub struct ScheduledTask {
    pub id: String,
    pub cron_expression: String,
    pub next_run: DateTime<Utc>,
    pub agent_id: String,
    pub task_description: String,
    pub entity_id: Option<String>,
    pub connection_id: Option<String>,
    /// Workspace that created the schedule. `None` on rows written before
    /// schedules were tenant-scoped; those read as the default workspace's.
    pub workspace_id: Option<String>,
    /// Principal that created the schedule, for audit. `None` on legacy rows.
    pub created_by: Option<String>,
}

/// Backing store for scheduled tasks (SurrealDB or SQLite).
#[async_trait]
pub trait ScheduleStore: Send + Sync {
    async fn load_all(&self) -> anyhow::Result<Vec<ScheduledTask>>;
    async fn save(&self, task: &ScheduledTask) -> anyhow::Result<()>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
}

/// SurrealDB-backed store for scheduled tasks. Shares the same Surreal connection as MemoryStore.
pub struct SurrealScheduleStore {
    db: Arc<Surreal<Any>>,
}

impl SurrealScheduleStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ScheduleStore for SurrealScheduleStore {
    async fn load_all(&self) -> anyhow::Result<Vec<ScheduledTask>> {
        let rows: Vec<ScheduledTaskRow> = self
            .db
            .select("scheduled_tasks")
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB select scheduled_tasks: {}", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(ScheduledTask {
                id: record_id_to_key_string(&r.id),
                cron_expression: r.cron_expression,
                next_run: DateTime::parse_from_rfc3339(&r.next_run)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                agent_id: r.agent_id,
                task_description: r.task_description,
                entity_id: r.entity_id,
                connection_id: r.connection_id,
                workspace_id: r.workspace_id,
                created_by: r.created_by,
            });
        }
        Ok(out)
    }

    async fn save(&self, task: &ScheduledTask) -> anyhow::Result<()> {
        let row = ScheduledTaskRowContent {
            id: task.id.clone(),
            cron_expression: task.cron_expression.clone(),
            next_run: task.next_run.to_rfc3339(),
            agent_id: task.agent_id.clone(),
            task_description: task.task_description.clone(),
            entity_id: task.entity_id.clone(),
            connection_id: task.connection_id.clone(),
            workspace_id: task.workspace_id.clone(),
            created_by: task.created_by.clone(),
        };
        let _: Option<serde::de::IgnoredAny> = self
            .db
            .upsert(("scheduled_tasks", task.id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB upsert scheduled_task: {}", e))?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let _: Option<serde::de::IgnoredAny> = self
            .db
            .delete(("scheduled_tasks", id))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB delete scheduled_task: {}", e))?;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
struct ScheduledTaskRow {
    id: surrealdb::RecordId,
    cron_expression: String,
    next_run: String,
    agent_id: String,
    task_description: String,
    #[serde(default)]
    entity_id: Option<String>,
    #[serde(default)]
    connection_id: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
}

// SurrealDB record ids render as `table:key` (sometimes backtick-quoted);
// this recovers the bare key.
fn record_id_to_key_string(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    if let Some(rest) = s.split_once(':').map(|x| x.1) {
        rest.trim_matches('`').to_string()
    } else {
        s
    }
}

#[derive(serde::Serialize)]
struct ScheduledTaskRowContent {
    id: String,
    cron_expression: String,
    next_run: String,
    agent_id: String,
    task_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
}

/// Scheduler: poll store at interval, dispatch due tasks, update next_run from cron.
/// Polls the schedule store at a fixed interval and dispatches any task whose
/// `next_run` has passed.
#[derive(Clone)]
pub struct Scheduler {
    store: Arc<dyn ScheduleStore>,
    dispatcher: Arc<dyn ScheduleDispatcher>,
    poll_interval_secs: u64,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("poll_interval_secs", &self.poll_interval_secs)
            .finish_non_exhaustive()
    }
}

impl Scheduler {
    pub fn new(
        store: Arc<dyn ScheduleStore>,
        dispatcher: Arc<dyn ScheduleDispatcher>,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            store,
            dispatcher,
            poll_interval_secs: poll_interval_secs.max(1),
        }
    }

    /// Add a task (persisted). next_run is set from cron or now.
    /// Persists a new scheduled task, computing its first `next_run` from the
    /// cron expression.
    pub async fn add_task(&self, task: ScheduledTask) -> anyhow::Result<()> {
        let next = next_run_from_cron(&task.cron_expression)?;
        let task = ScheduledTask {
            next_run: next,
            ..task
        };
        self.store.save(&task).await
    }

    /// List all scheduled tasks.
    pub async fn list_tasks(&self) -> anyhow::Result<Vec<ScheduledTask>> {
        self.store.load_all().await
    }

    /// Remove a task by id.
    pub async fn remove_task(&self, id: &str) -> anyhow::Result<()> {
        self.store.delete(id).await
    }

    /// Run one poll tick (for tests). Returns after processing due tasks.
    pub async fn run_one_tick(&self) -> anyhow::Result<()> {
        self.tick().await
    }

    /// Run the poll loop: every poll_interval_secs, check due tasks, dispatch, update next_run.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.poll_interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                tracing::warn!("scheduler tick error: {:?}", e);
            }
        }
    }

    // Dispatches every task whose next_run has passed, then reschedules it
    // from its cron expression.
    //
    // The next run is worked out *before* dispatching. The old order fired
    // first and then `?`-propagated a cron with no further occurrence (a
    // one-shot date in the past, an unparseable row): the task never got a new
    // `next_run`, so it fired again every tick forever, and the early return
    // starved every task after it in the list. Now such a task is retired and
    // the loop moves on; one bad row can no longer stall the others.
    async fn tick(&self) -> anyhow::Result<()> {
        let now = Utc::now();
        let tasks = self.store.load_all().await?;
        for mut task in tasks {
            if task.next_run > now {
                continue;
            }
            let next_run = match next_run_from_cron(&task.cron_expression) {
                Ok(next) => Some(next),
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.id,
                        error = %e,
                        "scheduler: cron has no further occurrence; this is its last run"
                    );
                    None
                }
            };
            let session_id = format!("schedule-{}", task.id);
            let params = ScheduledDispatchParams {
                agent_id: task.agent_id.clone(),
                task_description: task.task_description.clone(),
                session_id,
                entity_id: task.entity_id.clone(),
                connection_id: task.connection_id.clone(),
            };
            // Retire or reschedule before dispatch, so a slow or panicking run
            // cannot leave the row due and have the next tick fire it again.
            let persisted = match next_run {
                Some(next) => {
                    task.next_run = next;
                    self.store.save(&task).await
                }
                None => self.store.delete(&task.id).await,
            };
            if let Err(e) = persisted {
                // Dispatching a task whose reschedule did not stick would fire
                // it again next tick; skip it this round instead.
                tracing::warn!(
                    task_id = %task.id,
                    error = %e,
                    "scheduler: could not persist the next run; skipping this firing"
                );
                continue;
            }
            self.dispatcher.dispatch(params).await;
        }
        Ok(())
    }
}

fn next_run_from_cron(expr: &str) -> anyhow::Result<DateTime<Utc>> {
    let schedule = Schedule::from_str(expr)
        .map_err(|e| anyhow::anyhow!("invalid cron expression {:?}: {}", expr, e))?;
    schedule
        .upcoming(Utc)
        .next()
        .ok_or_else(|| anyhow::anyhow!("no upcoming run for cron {:?}", expr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct MemStore {
        rows: Mutex<HashMap<String, ScheduledTask>>,
    }

    #[async_trait]
    impl ScheduleStore for MemStore {
        async fn load_all(&self) -> anyhow::Result<Vec<ScheduledTask>> {
            let mut rows: Vec<_> = self.rows.lock().unwrap().values().cloned().collect();
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(rows)
        }
        async fn save(&self, task: &ScheduledTask) -> anyhow::Result<()> {
            self.rows
                .lock()
                .unwrap()
                .insert(task.id.clone(), task.clone());
            Ok(())
        }
        async fn delete(&self, id: &str) -> anyhow::Result<()> {
            self.rows.lock().unwrap().remove(id);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Counting {
        fired: Mutex<Vec<String>>,
        count: AtomicUsize,
    }

    impl ScheduleDispatcher for Counting {
        fn dispatch(
            &self,
            params: ScheduledDispatchParams,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.fired.lock().unwrap().push(params.session_id);
            Box::pin(async {})
        }
    }

    fn task(id: &str, cron: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            cron_expression: cron.to_string(),
            next_run: Utc::now() - chrono::Duration::seconds(5),
            agent_id: "agent".to_string(),
            task_description: "do it".to_string(),
            entity_id: None,
            connection_id: None,
            workspace_id: None,
            created_by: None,
        }
    }

    #[tokio::test]
    async fn a_cron_with_no_next_run_fires_once_and_does_not_starve_later_tasks() {
        let store = Arc::new(MemStore::default());
        // Sorted first, and its only occurrence (year 2000) is long gone.
        store.save(&task("a-expired", "0 0 0 1 1 * 2000")).await.unwrap();
        store.save(&task("b-hourly", "0 0 * * * *")).await.unwrap();
        let dispatcher = Arc::new(Counting::default());
        let scheduler = Scheduler::new(store.clone(), dispatcher.clone(), 60);

        scheduler.run_one_tick().await.unwrap();
        assert_eq!(dispatcher.count.load(Ordering::SeqCst), 2);
        assert!(
            dispatcher
                .fired
                .lock()
                .unwrap()
                .contains(&"schedule-b-hourly".to_string()),
            "the task after the expired one still ran"
        );
        let rows = store.load_all().await.unwrap();
        assert_eq!(rows.len(), 1, "the exhausted schedule was retired");
        assert_eq!(rows[0].id, "b-hourly");
        assert!(rows[0].next_run > Utc::now());

        // Next tick: nothing is due, so nothing fires again.
        scheduler.run_one_tick().await.unwrap();
        assert_eq!(dispatcher.count.load(Ordering::SeqCst), 2);
    }
}
