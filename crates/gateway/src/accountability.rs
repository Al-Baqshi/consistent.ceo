//! Accountability: the loop that pushes back when a commitment slips.
//!
//! Everything else in the gateway reports on work that happened. This ticker
//! exists to interrupt the operator about work that *didn't*. It makes three
//! passes on a 300s cadence:
//!
//! 1. **Overdue** — a task past its `due_at` gets exactly one `TaskOverdue`
//!    push, recorded in metadata so restarts and re-ticks stay idempotent.
//! 2. **Escalation** — 24h later, one final `TaskOverdue`. Exactly one, ever;
//!    a companion that nags on a timer gets muted, and a muted companion is
//!    worse than none (same restraint as [`crate::notify::is_notifiable`]).
//! 3. **Morning digest** — after 08:00 local, one cross-entity `DailyDigest`.
//!    Heartbeats are per-entity by design, so with N entities there is no
//!    single "here is your day" surface; this is it.
//!
//! Quiet hours are honoured by *skipping the mark*, not by dropping the event:
//! the next tick after the window closes notifies normally, so nothing that
//! came due at 3am is lost.
//!
//! The counting and selection logic is free functions over `&[Task]` so the
//! interesting behaviour is testable without a `ControlState`.

use chrono::{DateTime, Duration, Local, NaiveDate, Timelike, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

use crate::state::{
    ApprovalStatus, ControlState, OperatorFeedEvent, OperatorFeedKind, Task, TaskStatus,
    task_due_at, task_is_overdue, task_kind_is_internal, task_meta_set_str, task_meta_str,
};

/// Metadata key stamped when the first overdue notification went out.
pub const OVERDUE_NOTIFIED_KEY: &str = "overdueNotifiedAt";
/// Metadata key stamped when the one-and-only escalation went out.
pub const OVERDUE_ESCALATED_KEY: &str = "overdueEscalatedAt";
/// How long an overdue task may sit unacknowledged before it escalates once.
const ESCALATE_AFTER_HOURS: i64 = 24;
/// Local hour from which the day is considered briefable.
const DIGEST_HOUR_LOCAL: u32 = 8;
/// The daily brief and the Today screen both lead with the same short list.
pub const FOCUS_CAP: usize = 3;

/// Streak and completion counts, computed from `completed_at` alone.
///
/// Task rows are never pruned, so this is derivable at any time without an
/// activity log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreakSummary {
    pub current_streak_days: u32,
    pub completed_today: u32,
    pub completed_last7: u32,
    pub completed_last30: u32,
}

// Best-effort RFC3339 parse, normalized to UTC; malformed strings become `None`.
fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn task_completed_at(task: &Task) -> Option<DateTime<Utc>> {
    task.completed_at.as_deref().and_then(parse_rfc3339)
}

/// Server-local calendar date of an instant. Days are what the operator counts
/// in, and the gateway runs on their machine.
fn local_date(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Local).date_naive()
}

fn is_terminal(task: &Task) -> bool {
    matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
    )
}

/// Due inside today's local calendar day and not already past. Overdue work is
/// counted separately so a task never appears in both lists.
pub fn task_due_today(task: &Task, now: DateTime<Utc>) -> bool {
    if task_kind_is_internal(task) || is_terminal(task) {
        return false;
    }
    let Some(due) = task_due_at(task) else {
        return false;
    };
    due >= now && local_date(due) == local_date(now)
}

/// Whole hours a task is past its deadline; `None` when it isn't overdue.
pub fn overdue_hours(task: &Task, now: DateTime<Utc>) -> Option<i64> {
    if !task_is_overdue(task, now) {
        return None;
    }
    task_due_at(task).map(|due| (now - due).num_hours().max(0))
}

// Lower rank sorts first: high-priority work floats to the top of the focus list.
fn priority_rank(priority: Option<&str>) -> u8 {
    match priority
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "high" | "critical" | "urgent" => 0,
        "medium" | "normal" => 1,
        "low" => 2,
        // Unset sorts with medium: an unprioritised task is ordinary work, not
        // deprioritised work.
        _ => 1,
    }
}

/// The short list the operator should actually do next.
///
/// Deterministic on purpose — the daily-review endpoint renders the operator's
/// morning and must not wait on a model. Overdue leads (oldest deadline first,
/// because the oldest slip is the one rotting), then today's work by priority.
pub fn select_focus(tasks: &[Task], now: DateTime<Utc>, cap: usize) -> Vec<Task> {
    let mut overdue: Vec<&Task> = tasks.iter().filter(|t| task_is_overdue(t, now)).collect();
    overdue.sort_by_key(|t| task_due_at(t));

    let mut today: Vec<&Task> = tasks.iter().filter(|t| task_due_today(t, now)).collect();
    today.sort_by(|a, b| {
        priority_rank(a.priority.as_deref())
            .cmp(&priority_rank(b.priority.as_deref()))
            .then_with(|| task_due_at(a).cmp(&task_due_at(b)))
    });

    overdue
        .into_iter()
        .chain(today)
        .take(cap)
        .cloned()
        .collect()
}

/// What the morning brief counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigestCounts {
    pub overdue: usize,
    pub due_today: usize,
    pub waiting: usize,
}

impl DigestCounts {
    fn is_empty(&self) -> bool {
        self.overdue == 0 && self.due_today == 0 && self.waiting == 0
    }
}

/// Tallies overdue, due-today, and pending-approval counts for the digest.
pub fn digest_counts(tasks: &[Task], waiting: usize, now: DateTime<Utc>) -> DigestCounts {
    DigestCounts {
        overdue: tasks.iter().filter(|t| task_is_overdue(t, now)).count(),
        due_today: tasks.iter().filter(|t| task_due_today(t, now)).count(),
        waiting,
    }
}

/// Elapsed time the way a person says it. `24h overdue` is a readout; "two
/// days" is what the operator actually thinks in, and the difference decides
/// whether a nudge reads as a colleague or a monitoring alert.
pub fn humanize_elapsed(hours: i64) -> String {
    match hours {
        h if h < 1 => "under an hour".to_string(),
        1 => "an hour".to_string(),
        h if h < 24 => format!("{h} hours"),
        h if h < 48 => "a day".to_string(),
        h if h < 14 * 24 => format!("{} days", h / 24),
        h => format!("{} weeks", h / (7 * 24)),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The one aggregated push of the day.
///
/// The old line read "Your day: 0 overdue, 0 due today, 1 waiting on you" —
/// three metrics, two of them zero, and the one thing that mattered buried
/// last. Zeros are dropped, the slipped work leads, and a streak is mentioned
/// only when there is a real one to mention: a "1 day streak" is not a streak,
/// and congratulating someone for nothing is how a companion loses credibility.
pub fn digest_title(counts: DigestCounts, streak_days: u32) -> String {
    let mut parts = Vec::new();
    if counts.overdue > 0 {
        parts.push(format!(
            "{} slipped",
            plural(counts.overdue, "thing", "things")
        ));
    }
    if counts.due_today > 0 {
        parts.push(format!("{} due today", counts.due_today));
    }
    if counts.waiting > 0 {
        parts.push(format!("{} waiting on you", counts.waiting));
    }
    let body = match parts.split_last() {
        None => "Nothing outstanding".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}, and {last}", rest.join(", ")),
    };
    if streak_days >= 2 {
        format!("Day {streak_days} — {body}")
    } else {
        body
    }
}

/// Consecutive local days with at least one completion, ending today or
/// yesterday.
///
/// Yesterday still counts: the day isn't over, so a streak must not read as
/// broken at 09:00 just because nothing is finished yet. Internal task kinds
/// (reminders, notes, beats) are excluded — they complete themselves, and a
/// streak that a scheduler can earn on the operator's behalf measures nothing.
pub fn compute_streak(tasks: &[Task], now: DateTime<Utc>) -> StreakSummary {
    let today = local_date(now);
    let mut days: HashSet<NaiveDate> = HashSet::new();
    let mut summary = StreakSummary::default();

    for task in tasks {
        if task_kind_is_internal(task) {
            continue;
        }
        let Some(done) = task_completed_at(task) else {
            continue;
        };
        let day = local_date(done);
        days.insert(day);
        let age = (today - day).num_days();
        if age == 0 {
            summary.completed_today += 1;
        }
        if (0..7).contains(&age) {
            summary.completed_last7 += 1;
        }
        if (0..30).contains(&age) {
            summary.completed_last30 += 1;
        }
    }

    let mut cursor = if days.contains(&today) {
        Some(today)
    } else {
        today.pred_opt()
    };
    while let Some(day) = cursor {
        if !days.contains(&day) {
            break;
        }
        summary.current_streak_days += 1;
        cursor = day.pred_opt();
    }
    summary
}

/// Detail line for the first overdue push.
///
/// Framed as the operator's own commitment ("you wanted this done") rather than
/// a record's due date, because that is what it is — the deadline came from
/// them, and a system that says so is holding them to their word instead of
/// reporting a field.
fn overdue_detail(due: DateTime<Utc>, hours: i64) -> String {
    format!(
        "You wanted this done {}. That was {} ago.",
        due.with_timezone(&Local).format("%A"),
        humanize_elapsed(hours)
    )
}

/// Detail line for the single escalation.
///
/// This is the only place the system says "I already mentioned this", and it is
/// the whole difference between a reminder and a follow-up. It is honest: the
/// escalation fires precisely because a first push went out and nothing
/// changed.
fn escalation_detail(hours: i64) -> String {
    format!(
        "I flagged this yesterday and it's still open — {} past when you wanted it.",
        humanize_elapsed(hours)
    )
}

/// Outcome of one accountability sweep, used for logging and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    pub notified: usize,
    pub escalated: usize,
    pub digested: bool,
}

impl TickReport {
    fn is_quiet(&self) -> bool {
        self.notified == 0 && self.escalated == 0 && !self.digested
    }
}

/// Polls for slipped commitments and briefs the operator once a morning.
pub struct AccountabilityTicker {
    control: Arc<ControlState>,
    poll_secs: u64,
}

impl AccountabilityTicker {
    /// Builds a ticker; `poll_secs` is clamped to at least 1 second.
    pub fn new(control: Arc<ControlState>, poll_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            control,
            poll_secs: poll_secs.max(1),
        })
    }

    /// Runs forever, ticking on `poll_secs` and logging any non-quiet sweep.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(self.poll_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let report = self.run_one_tick(Utc::now()).await;
            if !report.is_quiet() {
                tracing::info!(
                    notified = report.notified,
                    escalated = report.escalated,
                    digested = report.digested,
                    "accountability ticker: pushed"
                );
            }
        }
    }

    /// One full sweep at `now`. Injectable time keeps the behaviour testable.
    pub async fn run_one_tick(&self, now: DateTime<Utc>) -> TickReport {
        let muted = self.muted_entities().await;
        let mut report = self.sweep_overdue(now, &muted).await;
        report.digested = self.maybe_push_digest(now).await;
        report
    }

    /// Entities whose quiet window is open right now. Marking is skipped for
    /// their tasks so the notification fires after the window instead of being
    /// consumed by a silent tick.
    async fn muted_entities(&self) -> HashSet<String> {
        let now_local = Local::now().time();
        let entities = self.control.entities.read().await;
        entities
            .iter()
            .filter(|e| {
                e.entity_runtime
                    .as_ref()
                    .and_then(|rt| rt.quiet_hours_local.as_ref())
                    .is_some_and(|q| crate::notify::quiet_hours_active(q, now_local))
            })
            .map(|e| e.id.clone())
            .collect()
    }

    // First pass: mark newly-overdue tasks and any ripe for escalation.
    async fn sweep_overdue(&self, now: DateTime<Utc>, muted: &HashSet<String>) -> TickReport {
        let mut report = TickReport::default();
        // Mutate under the write lock, push feed events after releasing it.
        let mut pushes: Vec<(Task, String)> = Vec::new();
        {
            let mut tasks = self.control.tasks.write().await;
            for task in tasks.iter_mut() {
                if !task_is_overdue(task, now) {
                    continue;
                }
                if task
                    .entity_id
                    .as_deref()
                    .is_some_and(|id| muted.contains(id))
                {
                    continue;
                }
                let Some(due) = task_due_at(task) else {
                    continue;
                };
                let hours = (now - due).num_hours().max(0);

                match task_meta_str(task, OVERDUE_NOTIFIED_KEY).and_then(|v| parse_rfc3339(&v)) {
                    None => {
                        task_meta_set_str(task, OVERDUE_NOTIFIED_KEY, &now.to_rfc3339());
                        report.notified += 1;
                        pushes.push((task.clone(), overdue_detail(due, hours)));
                    }
                    Some(notified_at) => {
                        let ripe = now - notified_at >= Duration::hours(ESCALATE_AFTER_HOURS);
                        if ripe && task_meta_str(task, OVERDUE_ESCALATED_KEY).is_none() {
                            task_meta_set_str(task, OVERDUE_ESCALATED_KEY, &now.to_rfc3339());
                            report.escalated += 1;
                            pushes.push((task.clone(), escalation_detail(hours)));
                        }
                    }
                }
            }
        }
        for (task, detail) in &pushes {
            self.control
                .push_operator_feed(OperatorFeedKind::TaskOverdue, task, Some(detail.clone()))
                .await;
        }
        report
    }

    /// Exactly one brief per local day, and never a brief with nothing in it.
    async fn maybe_push_digest(&self, now: DateTime<Utc>) -> bool {
        let local = now.with_timezone(&Local);
        if local.hour() < DIGEST_HOUR_LOCAL {
            return false;
        }
        let today = local.date_naive();
        if self.digest_sent_on(today).await {
            return false;
        }

        let tasks: Vec<Task> = self.control.tasks.read().await.clone();
        let waiting = {
            let approvals = self.control.approvals.read().await;
            approvals
                .iter()
                .filter(|a| a.status == ApprovalStatus::Pending)
                .count()
        };
        let counts = digest_counts(&tasks, waiting, now);
        if counts.is_empty() {
            // A brief that says "nothing" trains the operator to ignore briefs.
            return false;
        }
        let focus = select_focus(&tasks, now, FOCUS_CAP);
        let detail = focus
            .iter()
            .map(|t| t.title.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");

        self.control
            .append_operator_feed_event(OperatorFeedEvent {
                id: Uuid::new_v4().to_string(),
                timestamp: now.to_rfc3339(),
                kind: OperatorFeedKind::DailyDigest,
                task_id: None,
                mission_id: None,
                // Cross-entity by design: the digest is the one aggregated push.
                entity_id: None,
                connection_id: None,
                title: digest_title(counts, compute_streak(&tasks, now).current_streak_days),
                detail: (!detail.is_empty()).then_some(detail),
                approval_id: None,
            })
            .await;
        true
    }

    async fn digest_sent_on(&self, day: NaiveDate) -> bool {
        let feed = self.control.operator_feed.read().await;
        feed.iter().any(|e| {
            e.kind == OperatorFeedKind::DailyDigest
                && parse_rfc3339(&e.timestamp).is_some_and(|t| local_date(t) == day)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ApprovalRequest, TaskStatus};

    /// The brief is the product's voice once a day. These pin the qualities
    /// that make it read as a colleague rather than a monitoring alert, because
    /// those are exactly the things a later "simplification" quietly undoes.
    mod voice {
        use super::*;

        #[test]
        fn the_brief_never_reports_a_zero() {
            let title = digest_title(
                DigestCounts {
                    overdue: 0,
                    due_today: 0,
                    waiting: 1,
                },
                0,
            );
            assert_eq!(title, "1 waiting on you");
            assert!(
                !title.contains('0'),
                "a zero is noise the reader has to discard: {title}"
            );
        }

        #[test]
        fn slipped_work_leads_and_reads_as_a_commitment() {
            let title = digest_title(
                DigestCounts {
                    overdue: 2,
                    due_today: 1,
                    waiting: 0,
                },
                0,
            );
            assert!(title.starts_with("2 things slipped"), "got {title}");
            assert!(title.contains("1 due today"));
        }

        #[test]
        fn a_streak_is_only_claimed_once_it_is_real() {
            let counts = DigestCounts {
                overdue: 1,
                due_today: 0,
                waiting: 0,
            };
            // One day is not a streak; saying so cheapens the word.
            assert!(!digest_title(counts, 1).contains("Day"));
            assert!(digest_title(counts, 4).starts_with("Day 4 — "));
        }

        #[test]
        fn elapsed_time_is_said_the_way_a_person_says_it() {
            assert_eq!(humanize_elapsed(3), "3 hours");
            assert_eq!(humanize_elapsed(30), "a day");
            assert_eq!(humanize_elapsed(72), "3 days");
            assert_eq!(humanize_elapsed(24 * 21), "3 weeks");
        }

        #[test]
        fn the_escalation_admits_it_already_asked() {
            // This sentence is the entire difference between a reminder that
            // repeats and a system that follows up.
            let detail = escalation_detail(50);
            assert!(detail.contains("flagged this"), "got {detail}");
            assert!(detail.contains("still open"), "got {detail}");
            assert!(
                !detail.contains("50h"),
                "raw hours read as telemetry: {detail}"
            );
        }

        #[test]
        fn the_first_nudge_frames_the_deadline_as_the_operators_own() {
            let due = now() - chrono::Duration::hours(26);
            let detail = overdue_detail(due, 26);
            assert!(detail.contains("You wanted this done"), "got {detail}");
            assert!(detail.contains("a day"), "got {detail}");
        }
    }

    /// Anchored to *local* 14:00, so both the local-day arithmetic and the
    /// 08:00 digest threshold behave identically on any machine's timezone.
    fn now() -> DateTime<Utc> {
        Local::now()
            .with_hour(14)
            .unwrap()
            .with_minute(0)
            .unwrap()
            .with_second(0)
            .unwrap()
            .with_nanosecond(0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn task(id: &str, status: TaskStatus) -> Task {
        Task {
            id: id.to_string(),
            title: format!("Task {id}"),
            status,
            ..Task::default()
        }
    }

    fn completed_on(id: &str, at: DateTime<Utc>) -> Task {
        Task {
            completed_at: Some(at.to_rfc3339()),
            ..task(id, TaskStatus::Completed)
        }
    }

    fn due(mut t: Task, at: DateTime<Utc>) -> Task {
        t.due_at = Some(at.to_rfc3339());
        t
    }

    async fn control() -> Arc<ControlState> {
        Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ))
    }

    #[test]
    fn streak_counts_consecutive_days_and_windows() {
        let n = now();
        let tasks = vec![
            completed_on("a", n),
            completed_on("b", n - Duration::days(1)),
            completed_on("c", n - Duration::days(2)),
            completed_on("d", n - Duration::days(10)),
        ];
        let s = compute_streak(&tasks, n);
        assert_eq!(s.current_streak_days, 3);
        assert_eq!(s.completed_today, 1);
        assert_eq!(s.completed_last7, 3);
        assert_eq!(s.completed_last30, 4);
    }

    #[test]
    fn a_gap_breaks_the_streak() {
        let n = now();
        let tasks = vec![
            completed_on("a", n),
            // no completion on day 1
            completed_on("c", n - Duration::days(2)),
            completed_on("d", n - Duration::days(3)),
        ];
        assert_eq!(compute_streak(&tasks, n).current_streak_days, 1);
    }

    #[test]
    fn streak_ending_yesterday_survives_an_empty_today() {
        let n = now();
        let tasks = vec![
            completed_on("b", n - Duration::days(1)),
            completed_on("c", n - Duration::days(2)),
        ];
        let s = compute_streak(&tasks, n);
        assert_eq!(s.current_streak_days, 2, "today isn't over yet");
        assert_eq!(s.completed_today, 0);

        // Finishing something today extends rather than restarts it.
        let mut extended = tasks.clone();
        extended.push(completed_on("a", n));
        assert_eq!(compute_streak(&extended, n).current_streak_days, 3);
    }

    #[test]
    fn streak_ignores_self_completing_internal_kinds() {
        let n = now();
        let mut reminder = completed_on("rem", n);
        reminder.task_kind = Some("reminder".to_string());
        assert_eq!(compute_streak(&[reminder], n).current_streak_days, 0);
    }

    #[test]
    fn focus_leads_with_the_oldest_slip_then_priority() {
        let n = now();
        let mut low = due(
            task("today_low", TaskStatus::Queued),
            n + Duration::hours(1),
        );
        low.priority = Some("low".to_string());
        let mut high = due(
            task("today_high", TaskStatus::Queued),
            n + Duration::hours(2),
        );
        high.priority = Some("high".to_string());
        let tasks = vec![
            due(task("late_new", TaskStatus::Queued), n - Duration::hours(2)),
            low,
            high,
            due(task("late_old", TaskStatus::Queued), n - Duration::days(3)),
        ];
        let ids: Vec<String> = select_focus(&tasks, n, FOCUS_CAP)
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec!["late_old", "late_new", "today_high"]);
    }

    #[tokio::test]
    async fn overdue_marking_is_idempotent() {
        let control = control().await;
        let n = now();
        control
            .tasks
            .write()
            .await
            .push(due(task("t1", TaskStatus::Queued), n - Duration::hours(3)));

        let ticker = AccountabilityTicker::new(control.clone(), 300);
        assert_eq!(ticker.run_one_tick(n).await.notified, 1);
        assert_eq!(
            ticker.run_one_tick(n).await.notified,
            0,
            "already notified; no second push"
        );

        let feed = control.operator_feed.read().await;
        let overdue: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == OperatorFeedKind::TaskOverdue)
            .collect();
        assert_eq!(overdue.len(), 1);
        assert!(
            overdue[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("You wanted this done") && d.contains("3 hours")),
            "detail names the operator's own deadline and how long it has slipped: {:?}",
            overdue[0].detail
        );
    }

    #[tokio::test]
    async fn escalation_fires_exactly_once_after_24h() {
        let control = control().await;
        let n = now();
        control
            .tasks
            .write()
            .await
            .push(due(task("t1", TaskStatus::Queued), n - Duration::hours(2)));

        let ticker = AccountabilityTicker::new(control.clone(), 300);
        assert_eq!(ticker.run_one_tick(n).await.notified, 1);
        // 23h later: not yet ripe.
        assert_eq!(
            ticker.run_one_tick(n + Duration::hours(23)).await.escalated,
            0
        );
        let later = n + Duration::hours(25);
        assert_eq!(ticker.run_one_tick(later).await.escalated, 1);
        assert_eq!(
            ticker
                .run_one_tick(later + Duration::hours(48))
                .await
                .escalated,
            0,
            "escalation never loops"
        );

        let tasks = control.tasks.read().await;
        let t = &tasks[0];
        assert!(task_meta_str(t, OVERDUE_NOTIFIED_KEY).is_some());
        assert!(task_meta_str(t, OVERDUE_ESCALATED_KEY).is_some());
    }

    #[tokio::test]
    async fn completed_tasks_are_never_marked_overdue() {
        let control = control().await;
        let n = now();
        control.tasks.write().await.push(due(
            task("done", TaskStatus::Completed),
            n - Duration::hours(5),
        ));
        let ticker = AccountabilityTicker::new(control.clone(), 300);
        assert_eq!(ticker.run_one_tick(n).await.notified, 0);
    }

    #[tokio::test]
    async fn digest_pushes_at_most_once_per_day() {
        let control = control().await;
        let n = now(); // 14:00 UTC — past the 08:00 local threshold in most zones
        {
            let mut tasks = control.tasks.write().await;
            tasks.push(due(task("t1", TaskStatus::Queued), n - Duration::hours(3)));
        }
        control.approvals.write().await.push(ApprovalRequest {
            id: "ap1".to_string(),
            created_at: n.to_rfc3339(),
            updated_at: n.to_rfc3339(),
            entity_id: "ent".to_string(),
            connection_id: None,
            task_id: None,
            action: "send_email".to_string(),
            status: ApprovalStatus::Pending,
            detail: None,
            created_by: None,
        });

        let ticker = AccountabilityTicker::new(control.clone(), 300);
        assert!(ticker.run_one_tick(n).await.digested);
        assert!(!ticker.run_one_tick(n).await.digested, "one brief per day");
        assert!(
            !ticker.run_one_tick(n + Duration::hours(2)).await.digested,
            "still the same day"
        );

        let feed = control.operator_feed.read().await;
        let digests: Vec<_> = feed
            .iter()
            .filter(|e| e.kind == OperatorFeedKind::DailyDigest)
            .collect();
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].title, "1 thing slipped, and 1 waiting on you");
        assert_eq!(digests[0].detail.as_deref(), Some("Task t1"));
    }

    #[tokio::test]
    async fn empty_day_gets_no_digest() {
        let control = control().await;
        let ticker = AccountabilityTicker::new(control.clone(), 300);
        assert!(!ticker.run_one_tick(now()).await.digested);
    }
}
