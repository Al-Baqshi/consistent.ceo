//! Reminders that actually fire.
//!
//! Chat's `set_reminder` tool stores a task with `task_kind == "reminder"`
//! and a resolved `due_at` in the task metadata. The [`ReminderTicker`] polls
//! the in-memory task list, and when a reminder comes due it marks the task
//! completed and pushes a `ScheduledTaskFired` operator-feed event — the same
//! surface the cron scheduler uses, so every client that renders the feed
//! shows reminders with zero changes.
//!
//! Time resolution is deterministic ([`parse_when`]) with an LLM-provided
//! `in_minutes` override for phrasings the parser doesn't cover. All
//! wall-clock forms ("tomorrow 9am") are interpreted in UTC until per-user
//! timezones exist; relative forms ("in 20 minutes") are exact.

use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Timelike, Utc};
use std::sync::Arc;

use crate::state::{ControlState, TaskOutcome, TaskStatus};

/// Task metadata key holding the resolved RFC3339 fire time.
pub const REMINDER_DUE_AT_KEY: &str = "due_at";
/// Task metadata key set once a reminder has fired, so a tick never fires it twice.
pub const REMINDER_FIRED_KEY: &str = "reminder_fired";

/// Resolve a natural-language "when" into an absolute UTC time.
/// Returns `None` when the phrase isn't confidently parseable.
pub fn parse_when(when: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let raw = when.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    let s = raw.to_ascii_lowercase();
    let s = s.trim();

    if let Some(due) = parse_relative(s, now) {
        return Some(due);
    }
    if let Some(due) = parse_named_moment(s, now) {
        return Some(due);
    }
    if let Some(rest) = s.strip_prefix("tomorrow") {
        let rest = rest.trim().trim_start_matches("at").trim();
        let (h, m) = if rest.is_empty() {
            (9, 0)
        } else {
            parse_clock(rest)?
        };
        let d = (now + Duration::days(1)).date_naive();
        return Utc
            .with_ymd_and_hms(
                d.format("%Y").to_string().parse().ok()?,
                d.format("%m").to_string().parse().ok()?,
                d.format("%d").to_string().parse().ok()?,
                h,
                m,
                0,
            )
            .single();
    }
    // "today at 5pm" / "at 17:30" / bare "5pm"
    let rest = s
        .strip_prefix("today")
        .unwrap_or(s)
        .trim()
        .trim_start_matches("at")
        .trim();
    if let Some((h, m)) = parse_clock(rest) {
        let today = now
            .with_hour(h)?
            .with_minute(m)?
            .with_second(0)?
            .with_nanosecond(0)?;
        return Some(if today > now {
            today
        } else {
            today + Duration::days(1)
        });
    }
    None
}

/// Resolve a reminder using the operator's current UTC offset for wall-clock
/// phrases. Relative and RFC3339 timestamps remain absolute and are therefore
/// interpreted exactly as supplied. Invalid zones or nonexistent DST times
/// interpreted exactly as supplied.
pub fn parse_when_in_timezone(
    when: &str,
    now: DateTime<Utc>,
    offset_minutes: Option<i32>,
) -> Option<DateTime<Utc>> {
    let raw = when.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if DateTime::parse_from_rfc3339(raw).is_ok() || parse_relative(&lower, now).is_some() {
        return parse_when(raw, now);
    }

    let Some(offset_minutes) = offset_minutes else {
        return parse_when(raw, now);
    };
    let zone = FixedOffset::east_opt(offset_minutes.clamp(-24 * 60, 24 * 60) * 60)?;
    let local_now = now.with_timezone(&zone);
    // Parse the wall-clock phrase against a UTC value carrying the local
    // calendar fields, then reinterpret the resulting naive timestamp in the
    // requested zone. This keeps the existing phrase grammar in one place.
    let fake_now = Utc
        .with_ymd_and_hms(
            local_now.year(),
            local_now.month(),
            local_now.day(),
            local_now.hour(),
            local_now.minute(),
            local_now.second(),
        )
        .single()?;
    let fake_due = parse_when(raw, fake_now)?;
    zone.from_local_datetime(&fake_due.naive_utc())
        .single()
        .or_else(|| zone.from_local_datetime(&fake_due.naive_utc()).earliest())
        .map(|dt| dt.with_timezone(&Utc))
}

/// "in 20 minutes", "in an hour", "after 2 days", "30 min from now"
fn parse_relative(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let body = if let Some(b) = s.strip_prefix("in ") {
        b.to_string()
    } else if let Some(b) = s.strip_prefix("after ") {
        b.to_string()
    } else {
        let b = s.strip_suffix(" from now")?;
        b.to_string()
    };
    let body = body.trim();
    if body == "half an hour" || body == "half hour" {
        return Some(now + Duration::minutes(30));
    }
    let mut parts = body.split_whitespace();
    let qty_raw = parts.next()?;
    let qty: f64 = match qty_raw {
        "a" | "an" | "one" => 1.0,
        "two" => 2.0,
        "three" => 3.0,
        other => other.parse().ok()?,
    };
    let unit = parts.next().unwrap_or("minutes");
    let seconds = match unit.trim_end_matches('.') {
        "s" | "sec" | "secs" | "second" | "seconds" => qty,
        "m" | "min" | "mins" | "minute" | "minutes" => qty * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => qty * 3600.0,
        "d" | "day" | "days" => qty * 86400.0,
        "w" | "week" | "weeks" => qty * 604800.0,
        _ => return None,
    };
    if !seconds.is_finite() || !(0.0..=366.0 * 86400.0).contains(&seconds) {
        return None;
    }
    Some(now + Duration::seconds(seconds.round() as i64))
}

/// "tonight", "this evening", "noon", "eod", etc — fixed times of day.
fn parse_named_moment(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let at = |h: u32, m: u32, now: DateTime<Utc>| -> Option<DateTime<Utc>> {
        let t = now
            .with_hour(h)?
            .with_minute(m)?
            .with_second(0)?
            .with_nanosecond(0)?;
        Some(if t > now { t } else { t + Duration::days(1) })
    };
    match s {
        "tonight" => at(20, 0, now),
        "this evening" => at(18, 0, now),
        "this afternoon" => at(15, 0, now),
        "this morning" | "in the morning" => at(9, 0, now),
        "noon" | "midday" | "at noon" => at(12, 0, now),
        "midnight" | "at midnight" => at(0, 0, now),
        "end of day" | "eod" => at(17, 0, now),
        _ => None,
    }
}

/// "5pm", "5:30 pm", "17:30", "9am"
fn parse_clock(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let (body, pm, explicit_meridiem) = if let Some(b) = s.strip_suffix("pm") {
        (b.trim(), true, true)
    } else if let Some(b) = s.strip_suffix("am") {
        (b.trim(), false, true)
    } else {
        (s, false, false)
    };
    let (h_str, m_str) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, "0"),
    };
    let mut h: u32 = h_str.trim().parse().ok()?;
    let m: u32 = m_str.trim().parse().ok()?;
    if m > 59 {
        return None;
    }
    if explicit_meridiem {
        if h == 0 || h > 12 {
            return None;
        }
        if pm && h != 12 {
            h += 12;
        }
        if !pm && h == 12 {
            h = 0;
        }
    } else if h > 23 {
        return None;
    }
    Some((h, m))
}

fn metadata_due_at(metadata: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let due = metadata?.get(REMINDER_DUE_AT_KEY)?.as_str()?;
    DateTime::parse_from_rfc3339(due)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn metadata_fired(metadata: Option<&serde_json::Value>) -> bool {
    metadata
        .and_then(|m| m.get(REMINDER_FIRED_KEY))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Polls the task list and fires due reminders exactly once each.
pub struct ReminderTicker {
    control: Arc<ControlState>,
    poll_secs: u64,
}

impl ReminderTicker {
    /// Builds a ticker; `poll_secs` is clamped to at least 1 second.
    pub fn new(control: Arc<ControlState>, poll_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            control,
            poll_secs: poll_secs.max(1),
        })
    }

    /// Runs forever, ticking on `poll_secs` and logging when reminders fire.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(self.poll_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let fired = self.run_one_tick(Utc::now()).await;
            if fired > 0 {
                tracing::info!(fired, "reminder ticker: fired due reminders");
            }
        }
    }

    /// Fire everything due at `now`. Returns the number fired.
    pub async fn run_one_tick(&self, now: DateTime<Utc>) -> usize {
        // Mutate under the write lock, emit feed events after releasing it.
        let mut fired: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        {
            let mut tasks = self.control.tasks.write().await;
            for t in tasks.iter_mut() {
                if t.task_kind.as_deref() != Some("reminder") {
                    continue;
                }
                if matches!(
                    t.status,
                    TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
                ) {
                    continue;
                }
                if metadata_fired(t.metadata.as_ref()) {
                    continue;
                }
                let Some(due) = metadata_due_at(t.metadata.as_ref()) else {
                    continue;
                };
                if due > now {
                    continue;
                }
                // Status first, and bail if the loop-side setter refuses: marking
                // `reminder_fired` on a task we then leave alone would retire the
                // reminder without ever firing it. The status filter above already
                // skips cancelled reminders, so this is belt and braces — but the
                // order is what makes it free.
                if !crate::state::set_task_status_by_loop(t, TaskStatus::Completed) {
                    continue;
                }
                let meta = t.metadata.get_or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert(REMINDER_FIRED_KEY.to_string(), serde_json::json!(true));
                }
                t.outcome = Some(TaskOutcome::Completed);
                t.progress = 100;
                fired.push((t.title.clone(), t.entity_id.clone(), t.result.clone()));
            }
        }
        for (title, entity_id, detail) in &fired {
            self.control
                .push_scheduled_fire_feed(title.clone(), entity_id.clone(), None, detail.clone())
                .await;
        }
        fired.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Task;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 17, 10, 0, 0).unwrap()
    }

    #[test]
    fn parses_relative_forms() {
        let n = now();
        assert_eq!(
            parse_when("in 20 minutes", n),
            Some(n + Duration::minutes(20))
        );
        assert_eq!(parse_when("In 2 hours", n), Some(n + Duration::hours(2)));
        assert_eq!(parse_when("in an hour", n), Some(n + Duration::hours(1)));
        assert_eq!(parse_when("after 3 days", n), Some(n + Duration::days(3)));
        assert_eq!(
            parse_when("30 min from now", n),
            Some(n + Duration::minutes(30))
        );
        assert_eq!(
            parse_when("in half an hour", n),
            Some(n + Duration::minutes(30))
        );
    }

    #[test]
    fn parses_clock_and_named_forms() {
        let n = now(); // 10:00 UTC
        assert_eq!(
            parse_when("at 5pm", n),
            Some(n.with_hour(17).unwrap().with_minute(0).unwrap())
        );
        // 9am already passed → tomorrow 9am
        assert_eq!(
            parse_when("9am", n),
            Some(
                (n + Duration::days(1))
                    .with_hour(9)
                    .unwrap()
                    .with_minute(0)
                    .unwrap()
            )
        );
        assert_eq!(
            parse_when("tomorrow at 7:30am", n),
            Some(
                (n + Duration::days(1))
                    .with_hour(7)
                    .unwrap()
                    .with_minute(30)
                    .unwrap()
            )
        );
        assert_eq!(
            parse_when("tomorrow", n),
            Some(
                (n + Duration::days(1))
                    .with_hour(9)
                    .unwrap()
                    .with_minute(0)
                    .unwrap()
            )
        );
        assert_eq!(
            parse_when("tonight", n),
            Some(n.with_hour(20).unwrap().with_minute(0).unwrap())
        );
        assert_eq!(parse_when("gibberish someday", n), None);
        assert_eq!(parse_when("", n), None);
    }

    #[test]
    fn wall_clock_forms_use_the_operator_timezone() {
        let n = "2026-07-17T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        // Auckland is UTC+12 in July: tomorrow at 09:00 local is 21:00 UTC.
        assert_eq!(
            parse_when_in_timezone("tomorrow at 9am", n, Some(12 * 60)),
            Some("2026-07-17T21:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn extreme_timezone_offsets_are_clamped() {
        let n = now();
        assert!(parse_when_in_timezone("tomorrow at 9am", n, Some(14 * 60)).is_some());
    }

    #[tokio::test]
    async fn ticker_fires_due_reminder_once_and_pushes_feed() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        let n = now();
        {
            let mut tasks = control.tasks.write().await;
            tasks.push(Task {
                id: "rem_due".to_string(),
                title: "Reminder: check the oven".to_string(),
                task_kind: Some("reminder".to_string()),
                status: TaskStatus::Queued,
                metadata: Some(serde_json::json!({
                    REMINDER_DUE_AT_KEY: (n - Duration::minutes(1)).to_rfc3339(),
                    REMINDER_FIRED_KEY: false,
                })),
                ..Task::default()
            });
            tasks.push(Task {
                id: "rem_future".to_string(),
                title: "Reminder: future".to_string(),
                task_kind: Some("reminder".to_string()),
                status: TaskStatus::Queued,
                metadata: Some(serde_json::json!({
                    REMINDER_DUE_AT_KEY: (n + Duration::hours(1)).to_rfc3339(),
                })),
                ..Task::default()
            });
            // A reminder without due_at must never fire (legacy rows).
            tasks.push(Task {
                id: "rem_legacy".to_string(),
                title: "Reminder: legacy".to_string(),
                task_kind: Some("reminder".to_string()),
                status: TaskStatus::Queued,
                ..Task::default()
            });
        }

        let ticker = ReminderTicker::new(control.clone(), 60);
        assert_eq!(ticker.run_one_tick(n).await, 1, "only the due one fires");
        assert_eq!(ticker.run_one_tick(n).await, 0, "idempotent second tick");

        let tasks = control.tasks.read().await;
        let due = tasks.iter().find(|t| t.id == "rem_due").unwrap();
        assert_eq!(due.status, TaskStatus::Completed);
        assert!(metadata_fired(due.metadata.as_ref()));
        let future = tasks.iter().find(|t| t.id == "rem_future").unwrap();
        assert_eq!(future.status, TaskStatus::Queued);
        drop(tasks);

        let feed = control.operator_feed.read().await;
        assert!(
            feed.iter().any(|e| e.title.contains("check the oven")),
            "feed event pushed for fired reminder"
        );
    }
}
