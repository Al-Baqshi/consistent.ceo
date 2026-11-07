//! Reminders that fire, and keep following up until they're done.
//!
//! Anything can carry a reminder: a `task_kind == "reminder"` row (chat's
//! `set_reminder`), or a task/note whose metadata has `remind: true` (a note
//! like "send ABT the photo" with a follow-up time). The fire time lives in
//! metadata `due_at`.
//!
//! The [`ReminderTicker`] polls the in-memory task list. When an item comes
//! due it pushes a `ScheduledTaskFired` feed event carrying the task id (so
//! clients can offer Done / Snooze) and schedules the next nudge on a fixed
//! ladder ([`follow_up_after`]): an hour later, then the next morning, then
//! a "still relevant?" the morning after, then it parks and is surfaced by
//! the hourly open-loops beat instead. Only the owner ends a reminder (done,
//! cancel); nothing auto-completes it. The ladder is deterministic and
//! exposed through the API so the phone can schedule the same nudges as
//! local notifications and keep reminding while offline.
//!
//! Time resolution is deterministic ([`parse_when`]) with an LLM-provided
//! `in_minutes` override for phrasings the parser doesn't cover. Wall-clock
//! forms use the client's UTC offset (stored per reminder as `tz_offset_min`).

use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Timelike, Utc};
use serde::Serialize;
use std::sync::Arc;

use crate::state::{ControlState, Task, TaskOutcome, TaskStatus};

/// Task metadata key holding the RFC3339 time of the next nudge.
pub const REMINDER_DUE_AT_KEY: &str = "due_at";
/// Legacy flag: true once a one-shot reminder fired. Still honoured on read so
/// old rows don't re-fire; new rows track [`NUDGES_KEY`] instead.
pub const REMINDER_FIRED_KEY: &str = "reminder_fired";
/// How many times this item has nudged the owner since it was last (re)scheduled.
pub const NUDGES_KEY: &str = "reminder_nudges";
/// When the current reminder first fired (RFC3339).
pub const FIRST_FIRED_KEY: &str = "reminder_first_fired_at";
/// Set when the follow-up ladder ran out; the item waits for the open-loops beat.
pub const PARKED_KEY: &str = "reminder_parked";
/// `false` = fire once and stop (no follow-ups). Absent = follow up.
pub const FOLLOW_UP_KEY: &str = "follow_up";
/// Marks a non-reminder task/note as carrying a reminder.
pub const REMIND_KEY: &str = "remind";
/// Owner's UTC offset in minutes when the reminder was made; "next morning" uses it.
pub const TZ_KEY: &str = "tz_offset_min";
/// The fire time a phone confirmed it scheduled as a local notification;
/// matching fires skip the (duplicate) push.
pub const LOCAL_SCHEDULED_KEY: &str = "local_scheduled_due";
/// Client-side id for reminders created offline, for idempotent sync.
pub const CLIENT_ID_KEY: &str = "client_id";
/// "Do it for me": the computer task this reminder was handed to. While set,
/// the reminder stops nudging; the task's outcome closes or revives it.
pub const DELEGATED_TASK_KEY: &str = "delegated_task_id";
pub const DELEGATED_APPROVAL_KEY: &str = "delegated_approval_id";
/// Who closed it: absent = the owner, "computer" = a delegated task.
pub const DONE_BY_KEY: &str = "done_by";
/// Completions needed before the learned hour replaces [`MORNING_HOUR`].
const MIN_HABIT_SAMPLES: usize = 6;

/// Nudges after the first fire before the item parks.
pub const MAX_FOLLOW_UPS: u32 = 3;
/// Morning follow-ups land at this local hour.
pub const MORNING_HOUR: u32 = 9;
/// Automatic follow-ups that hit the owner's quiet hours wait this long and retry.
const QUIET_RETRY_MINUTES: i64 = 30;

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
    if let Some(due) = parse_weekday(s, now) {
        return Some(due);
    }
    if let Some(rest) = s.strip_prefix("tomorrow") {
        let rest = rest.trim().trim_start_matches("at").trim();
        let (h, m) = match rest {
            "" | "morning" | "in the morning" => (MORNING_HOUR, 0),
            "noon" | "midday" | "lunch" | "lunchtime" => (12, 0),
            "afternoon" => (15, 0),
            "evening" => (18, 0),
            "night" | "tonight" => (20, 0),
            other => parse_clock(other)?,
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

/// Find a time phrase inside free text ("send ABT the photo tomorrow at 9am")
/// by trying every short word window; the longest window that parses wins.
pub fn find_when_in_text(
    text: &str,
    now: DateTime<Utc>,
    offset_minutes: Option<i32>,
) -> Option<DateTime<Utc>> {
    split_when(text, now, offset_minutes).1
}

/// Split one captured sentence into the thing to do and its time:
/// "send ABT the photo tomorrow at 9am" → ("send ABT the photo", tomorrow 09:00).
/// Strips a leading "remind me to" and the matched time phrase (with a
/// dangling "at"/"on"/"by"). Without a time, the text comes back whole.
pub fn split_when(
    text: &str,
    now: DateTime<Utc>,
    offset_minutes: Option<i32>,
) -> (String, Option<DateTime<Utc>>) {
    let raw: Vec<&str> = text.split_whitespace().collect();
    let norm: Vec<String> = raw
        .iter()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != ':')
                .to_ascii_lowercase()
        })
        .collect();
    // (start, len, due)
    let mut best: Option<(usize, usize, DateTime<Utc>)> = None;
    for start in 0..norm.len() {
        // Bare numbers ("call 3 people") are not times.
        if norm[start].is_empty() || norm[start].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        for len in (1..=5).rev() {
            if start + len > norm.len() {
                continue;
            }
            let phrase = norm[start..start + len].join(" ");
            if let Some(due) = parse_when_in_timezone(&phrase, now, offset_minutes) {
                if best.is_none_or(|(_, l, _)| len > l) {
                    best = Some((start, len, due));
                }
                break;
            }
        }
    }
    let mut keep: Vec<&str> = raw.clone();
    let due = best.map(|(start, len, due)| {
        let mut from = start;
        // "... at 5pm", "... on friday", "... by tomorrow"
        if from > 0 && matches!(norm[from - 1].as_str(), "at" | "on" | "by" | "for") {
            from -= 1;
        }
        keep.drain(from..start + len);
        due
    });
    let mut what = keep.join(" ");
    for prefix in [
        "remind me to ",
        "remind me ",
        "reminder to ",
        "reminder: ",
        "don't let me forget to ",
        "dont let me forget to ",
        "i need to ",
        "need to ",
    ] {
        if what.to_ascii_lowercase().starts_with(prefix) {
            what = what[prefix.len()..].to_string();
            break;
        }
    }
    let what = what
        .trim()
        .trim_end_matches([',', ';', '-', '—'])
        .trim()
        .to_string();
    (
        if what.is_empty() {
            text.trim().to_string()
        } else {
            what
        },
        due,
    )
}

/// Tomorrow at [`MORNING_HOUR`], the default for anything with no time.
pub fn tomorrow_morning(now: DateTime<Utc>, offset_minutes: Option<i32>) -> DateTime<Utc> {
    next_morning(
        now + Duration::hours(12),
        offset_minutes.unwrap_or_else(local_offset_minutes),
        MORNING_HOUR,
    )
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

/// "friday", "on friday at 2pm", "next monday morning", "monday 9:30am".
/// A bare weekday means its next occurrence (never today) at 9am.
fn parse_weekday(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let s = s.strip_prefix("on ").unwrap_or(s).trim();
    // "next monday" = the coming Monday, same as "monday".
    let s = s.strip_prefix("next ").unwrap_or(s).trim();
    let (day_word, rest) = s.split_once(' ').unwrap_or((s, ""));
    let target = match day_word.trim_end_matches(',') {
        "monday" | "mon" => chrono::Weekday::Mon,
        "tuesday" | "tue" | "tues" => chrono::Weekday::Tue,
        "wednesday" | "wed" => chrono::Weekday::Wed,
        "thursday" | "thu" | "thurs" => chrono::Weekday::Thu,
        "friday" | "fri" => chrono::Weekday::Fri,
        "saturday" | "sat" => chrono::Weekday::Sat,
        "sunday" | "sun" => chrono::Weekday::Sun,
        _ => return None,
    };
    let rest = rest.trim().trim_start_matches("at").trim();
    let (h, m) = match rest {
        "" | "morning" => (MORNING_HOUR, 0),
        "afternoon" => (15, 0),
        "evening" => (18, 0),
        "night" => (20, 0),
        other => parse_clock(other)?,
    };
    let today = now.weekday().num_days_from_monday() as i64;
    let mut ahead = (target.num_days_from_monday() as i64 - today).rem_euclid(7);
    if ahead == 0 {
        ahead = 7;
    }
    let day = (now + Duration::days(ahead)).date_naive();
    Some(Utc.from_utc_datetime(&day.and_hms_opt(h, m, 0)?))
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

// Booleans have been written both as JSON bools and as "true"/"false"
// strings over time; accept either.
fn meta_bool(metadata: Option<&serde_json::Value>, key: &str) -> Option<bool> {
    match metadata?.get(key)? {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn metadata_fired(metadata: Option<&serde_json::Value>) -> bool {
    meta_bool(metadata, REMINDER_FIRED_KEY).unwrap_or(false)
}

fn meta_u32(task: &Task, key: &str) -> u32 {
    task.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0) as u32
}

fn meta_set(task: &mut Task, key: &str, value: serde_json::Value) {
    let meta = task.metadata.get_or_insert_with(|| serde_json::json!({}));
    if !meta.is_object() {
        *meta = serde_json::json!({});
    }
    if let Some(obj) = meta.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
}

fn meta_remove(task: &mut Task, key: &str) {
    if let Some(obj) = task.metadata.as_mut().and_then(|m| m.as_object_mut()) {
        obj.remove(key);
    }
}

/// True for anything the reminder engine owns: reminder rows, plus tasks and
/// notes that were given a reminder time.
pub fn is_reminder_like(task: &Task) -> bool {
    task.task_kind.as_deref() == Some("reminder")
        || meta_bool(task.metadata.as_ref(), REMIND_KEY).unwrap_or(false)
}

fn follows_up(task: &Task) -> bool {
    meta_bool(task.metadata.as_ref(), FOLLOW_UP_KEY).unwrap_or(true)
}

/// The owner's UTC offset for this reminder: stored at creation, else the
/// gateway machine's own offset (the gateway usually runs on their computer).
pub fn tz_offset(task: &Task) -> i32 {
    task.metadata
        .as_ref()
        .and_then(|m| m.get(TZ_KEY))
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .unwrap_or_else(local_offset_minutes)
}

pub fn local_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

/// The first `MORNING_HOUR` local time at or after `after`.
fn next_morning(after: DateTime<Utc>, offset_minutes: i32, hour: u32) -> DateTime<Utc> {
    let zone = FixedOffset::east_opt(offset_minutes.clamp(-14 * 60, 14 * 60) * 60)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("utc"));
    let local = after.with_timezone(&zone);
    let mut day = local.date_naive();
    loop {
        if let Some(candidate) = day
            .and_hms_opt(hour.min(23), 0, 0)
            .and_then(|n| zone.from_local_datetime(&n).single())
            .map(|d| d.with_timezone(&Utc))
            && candidate >= after
        {
            return candidate;
        }
        day = day.succ_opt().unwrap_or(day);
    }
}

/// When to nudge again after the `nudges`-th fire at `fired_at`.
/// `None` = the ladder is done; the item parks.
pub fn follow_up_after(
    nudges: u32,
    fired_at: DateTime<Utc>,
    offset_minutes: i32,
    hour: u32,
) -> Option<DateTime<Utc>> {
    match nudges {
        1 => Some(fired_at + Duration::hours(1)),
        // Next morning (at the owner's best hour), never sooner than a few
        // hours after the last nudge.
        2 => Some(next_morning(
            fired_at + Duration::hours(3),
            offset_minutes,
            hour,
        )),
        // The "still relevant?" ask, a day later.
        3 => Some(next_morning(
            fired_at + Duration::hours(20),
            offset_minutes,
            hour,
        )),
        _ => None,
    }
}

/// Every future nudge time for this item if the owner does nothing: the next
/// fire plus the ladder after it. Phones schedule exactly these.
pub fn planned_nudges(task: &Task) -> Vec<DateTime<Utc>> {
    planned_nudges_at(task, MORNING_HOUR)
}

/// [`planned_nudges`] with the owner's learned morning hour.
pub fn planned_nudges_at(task: &Task, hour: u32) -> Vec<DateTime<Utc>> {
    let Some(next) = metadata_due_at(task.metadata.as_ref()) else {
        return Vec::new();
    };
    if matches!(
        reminder_state(task),
        ReminderState::Done
            | ReminderState::Cancelled
            | ReminderState::Parked
            | ReminderState::Delegated
    ) {
        return Vec::new();
    }
    let mut out = vec![next];
    if follows_up(task) {
        let offset = tz_offset(task);
        let mut nudges = meta_u32(task, NUDGES_KEY);
        let mut at = next;
        while nudges < 1 + MAX_FOLLOW_UPS {
            nudges += 1;
            match follow_up_after(nudges, at, offset, hour) {
                Some(n) => {
                    out.push(n);
                    at = n;
                }
                None => break,
            }
        }
    }
    out
}

/// The local hour the owner actually gets things done, learned from when
/// they marked reminders done (their own taps, not the computer's). Morning
/// follow-ups land there instead of a fixed 9am. Needs a few completions in
/// waking hours; until then, [`MORNING_HOUR`].
pub fn preferred_hour(tasks: &[&Task]) -> u32 {
    let mut counts = [0usize; 24];
    let mut samples = 0usize;
    for t in tasks {
        if !is_reminder_like(t)
            || t.status != TaskStatus::Completed
            || !follows_up(t)
            || crate::state::task_meta_str(t, DONE_BY_KEY).is_some()
        {
            continue;
        }
        let Some(done) = t
            .completed_at
            .as_deref()
            .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
        else {
            continue;
        };
        let local = done.with_timezone(&Utc) + Duration::minutes(tz_offset(t) as i64);
        let h = local.hour() as usize;
        if (7..=21).contains(&h) {
            counts[h] += 1;
            samples += 1;
        }
    }
    if samples < MIN_HABIT_SAMPLES {
        return MORNING_HOUR;
    }
    // Light smoothing so one stray completion doesn't move the hour.
    (7..=21)
        .max_by_key(|&h| {
            (
                2 * counts[h] + counts[h - 1] + counts[h + 1],
                std::cmp::Reverse(h),
            )
        })
        .map(|h| h as u32)
        .unwrap_or(MORNING_HOUR)
}

/// [`preferred_hour`] for every entity that has tasks, keyed by entity id.
pub fn preferred_hours(tasks: &[Task]) -> std::collections::HashMap<String, u32> {
    let mut by_entity: std::collections::HashMap<&str, Vec<&Task>> =
        std::collections::HashMap::new();
    for t in tasks {
        if let Some(e) = t.entity_id.as_deref() {
            by_entity.entry(e).or_default().push(t);
        }
    }
    by_entity
        .into_iter()
        .map(|(e, ts)| (e.to_string(), preferred_hour(&ts)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderState {
    /// Waiting for its first fire.
    Scheduled,
    /// Fired at least once; waiting for the owner.
    Due,
    /// Follow-ups ran out; still open, surfaced by the open-loops beat.
    Parked,
    /// Handed to the owner's computer ("Do it for me"); waiting on that task.
    Delegated,
    Done,
    Cancelled,
}

pub fn reminder_state(task: &Task) -> ReminderState {
    match task.status {
        TaskStatus::Completed => return ReminderState::Done,
        TaskStatus::Cancelled | TaskStatus::Failed => return ReminderState::Cancelled,
        _ => {}
    }
    if crate::state::task_meta_str(task, DELEGATED_TASK_KEY).is_some() {
        return ReminderState::Delegated;
    }
    if meta_bool(task.metadata.as_ref(), PARKED_KEY).unwrap_or(false) {
        return ReminderState::Parked;
    }
    if meta_u32(task, NUDGES_KEY) > 0 || metadata_fired(task.metadata.as_ref()) {
        ReminderState::Due
    } else {
        ReminderState::Scheduled
    }
}

/// What a client sees for one reminder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReminderView {
    pub id: String,
    pub title: String,
    /// The thing to do, without the "Reminder (...):" prefix.
    pub what: String,
    pub kind: String,
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    pub state: ReminderState,
    /// Next time the gateway will nudge; `None` when nothing is scheduled.
    pub fire_at: Option<String>,
    /// The nudges after `fire_at` if the owner does nothing.
    pub follow_ups: Vec<String>,
    pub first_fired_at: Option<String>,
    pub nudges: u32,
    pub created_at: String,
    /// The computer task this was handed to, when delegated.
    pub delegated_task_id: Option<String>,
    pub delegated_approval_id: Option<String>,
}

/// The bare action text of a reminder ("call mum"), for nudge copy.
pub fn reminder_what(task: &Task) -> String {
    // internal_create_task stores the request content as "Capture:\n{text}".
    if task.task_kind.as_deref() == Some("reminder")
        && let Some(content) = task
            .result
            .as_deref()
            .map(|r| r.trim().strip_prefix("Capture:").unwrap_or(r).trim())
            .filter(|s| !s.is_empty())
    {
        return content.to_string();
    }
    let title = task.title.trim();
    // "Reminder (tomorrow 9am): call mum" / "Reminder: call mum"
    if let Some(rest) = title.strip_prefix("Reminder")
        && let Some((_, what)) = rest.split_once(": ")
    {
        return what.trim().to_string();
    }
    title.to_string()
}

/// Comparable form of a reminder's text: lowercase words, no punctuation,
/// no leading "to". "Call the dentist." == "call the dentist".
pub fn normalize_what(what: &str) -> String {
    let words: Vec<String> = what
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect();
    let start = usize::from(words.first().is_some_and(|w| w == "to"));
    words[start..].join(" ")
}

pub fn view(task: &Task, entity_name: Option<String>) -> ReminderView {
    view_at(task, entity_name, MORNING_HOUR)
}

/// [`view`] with the owner's learned morning hour.
pub fn view_at(task: &Task, entity_name: Option<String>, hour: u32) -> ReminderView {
    let mut planned = planned_nudges_at(task, hour)
        .into_iter()
        .map(|d| d.to_rfc3339());
    let fire_at = planned.next();
    ReminderView {
        id: task.id.clone(),
        title: task.title.clone(),
        what: reminder_what(task),
        kind: task.task_kind.clone().unwrap_or_else(|| "task".into()),
        entity_id: task.entity_id.clone(),
        entity_name,
        state: reminder_state(task),
        fire_at,
        follow_ups: planned.collect(),
        first_fired_at: crate::state::task_meta_str(task, FIRST_FIRED_KEY),
        nudges: meta_u32(task, NUDGES_KEY),
        created_at: task.created_at.clone(),
        delegated_task_id: crate::state::task_meta_str(task, DELEGATED_TASK_KEY),
        delegated_approval_id: crate::state::task_meta_str(task, DELEGATED_APPROVAL_KEY),
    }
}

/// Mark a reminder as handed to computer task `task_id`.
pub fn mark_delegated(task: &mut Task, task_id: &str, approval_id: &str) {
    meta_set(task, DELEGATED_TASK_KEY, serde_json::json!(task_id));
    meta_set(task, DELEGATED_APPROVAL_KEY, serde_json::json!(approval_id));
    meta_remove(task, PARKED_KEY);
}

fn clear_delegation(task: &mut Task) {
    meta_remove(task, DELEGATED_TASK_KEY);
    meta_remove(task, DELEGATED_APPROVAL_KEY);
}

/// The instruction a delegated reminder hands the computer agent.
pub fn delegation_instruction(task: &Task, extra: Option<&str>) -> String {
    let mut out = format!(
        "Do this for the owner: {}. It came from one of their reminders. If it needs \
         something only they have (a file you can't find, a login, a decision), stop and say \
         exactly what is missing instead of guessing.",
        reminder_what(task)
    );
    if let Some(extra) = extra.map(str::trim).filter(|e| !e.is_empty()) {
        out.push_str("\n\nDetails from the owner: ");
        out.push_str(extra);
    }
    out
}

/// Metadata for a new reminder firing at `due`.
pub fn schedule_metadata(due: DateTime<Utc>, offset_minutes: Option<i32>) -> serde_json::Value {
    serde_json::json!({
        REMINDER_DUE_AT_KEY: due.to_rfc3339(),
        REMINDER_FIRED_KEY: false,
        NUDGES_KEY: 0,
        TZ_KEY: offset_minutes.unwrap_or_else(local_offset_minutes),
    })
}

/// Point an item's next nudge at `due` and restart its follow-up ladder.
/// Reopens it if it was finished. Works on any task, turning it into a
/// reminder-carrying item if it wasn't one.
pub fn reschedule(task: &mut Task, due: DateTime<Utc>, offset_minutes: Option<i32>) {
    let due_s = due.to_rfc3339();
    clear_delegation(task);
    meta_set(task, REMINDER_DUE_AT_KEY, serde_json::json!(due_s));
    meta_set(task, REMINDER_FIRED_KEY, serde_json::json!(false));
    meta_set(task, NUDGES_KEY, serde_json::json!(0));
    meta_remove(task, PARKED_KEY);
    meta_remove(task, FIRST_FIRED_KEY);
    meta_remove(task, LOCAL_SCHEDULED_KEY);
    if let Some(off) = offset_minutes {
        meta_set(task, TZ_KEY, serde_json::json!(off));
    }
    if task.task_kind.as_deref() != Some("reminder") {
        meta_set(task, REMIND_KEY, serde_json::json!(true));
    }
    task.due_at = Some(due_s);
    if matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
    ) {
        crate::state::set_task_status_by_human(task, TaskStatus::Queued);
        task.progress = 0;
        task.outcome = None;
    }
}

/// An owner's answer to a reminder.
#[derive(Debug, Clone, PartialEq)]
pub enum ReminderAction {
    Done,
    Cancel,
    /// Push the next nudge to this time (snooze, reschedule, reopen).
    At(DateTime<Utc>),
}

pub fn apply_action(task: &mut Task, action: &ReminderAction, offset_minutes: Option<i32>) {
    clear_delegation(task);
    match action {
        ReminderAction::Done => {
            crate::state::set_task_status_by_human(task, TaskStatus::Completed);
            task.progress = 100;
            task.outcome = Some(TaskOutcome::Completed);
            meta_remove(task, PARKED_KEY);
        }
        ReminderAction::Cancel => {
            crate::state::set_task_status_by_human(task, TaskStatus::Cancelled);
            task.progress = 100;
            meta_remove(task, PARKED_KEY);
        }
        ReminderAction::At(due) => reschedule(task, *due, offset_minutes),
    }
}

/// Copy for one nudge. `nudge` is 1 for the first fire.
pub fn nudge_copy(task: &Task, nudge: u32) -> (String, Option<String>) {
    let what = reminder_what(task);
    match nudge {
        0 | 1 => (task.title.trim().to_string(), None),
        n if n > MAX_FOLLOW_UPS => (
            format!("Still relevant? {what}"),
            Some("Reminded a few times now. Done, snooze, or drop it?".to_string()),
        ),
        _ => (
            format!("Still open: {what}"),
            Some("Tap Done when it's handled, or snooze it.".to_string()),
        ),
    }
}

fn human_span(d: Duration) -> String {
    let mins = d.num_minutes().abs();
    if mins < 60 {
        format!("{} min", mins.max(1))
    } else if mins < 48 * 60 {
        format!("{}h", mins / 60)
    } else {
        format!("{}d", mins / (24 * 60))
    }
}

/// Chat context: the open reminders in scope, with ids, so "done", "push it
/// to tomorrow", or "drop the dentist one" map straight onto `update_task`
/// instead of a guess (or a claim nothing backs).
pub fn chat_block(tasks: &[&Task], now: DateTime<Utc>) -> Option<String> {
    let mut open: Vec<(&Task, ReminderState, Option<DateTime<Utc>>)> = tasks
        .iter()
        .filter(|t| is_reminder_like(t))
        .map(|t| (*t, reminder_state(t), metadata_due_at(t.metadata.as_ref())))
        .filter(|(_, s, _)| !matches!(s, ReminderState::Done | ReminderState::Cancelled))
        .collect();
    if open.is_empty() {
        return None;
    }
    // Waiting on the owner first, then soonest.
    open.sort_by_key(|(_, s, due)| {
        (
            matches!(s, ReminderState::Scheduled),
            due.map(|d| d.timestamp()).unwrap_or(i64::MAX),
        )
    });
    let mut out = String::from(
        "## Open reminders (pass the id as task_id to update_task when the user says done, later, or drop)",
    );
    for (t, state, due) in open.into_iter().take(8) {
        let status = match (state, due) {
            (ReminderState::Delegated, _) => "being done on their computer".to_string(),
            (ReminderState::Parked, _) => {
                format!("reminded {}x, no answer", meta_u32(t, NUDGES_KEY))
            }
            (ReminderState::Due, _) => {
                format!("waiting on you, reminded {}x", meta_u32(t, NUDGES_KEY))
            }
            (_, Some(d)) if d > now => format!("next in {}", human_span(d - now)),
            _ => "due now".to_string(),
        };
        out.push_str(&format!("\n- {} · {} · {}", t.id, reminder_what(t), status));
    }
    Some(out)
}

/// A short answer to a nudge ("done", "push it to tomorrow", "drop it"),
/// read without a model. `None` = not confidently an answer; let chat think.
pub fn parse_reply(
    message: &str,
    now: DateTime<Utc>,
    offset_minutes: Option<i32>,
) -> Option<ReminderAction> {
    let m = message
        .trim()
        .trim_end_matches(['.', '!', ' '])
        .to_lowercase()
        .replace('’', "'");
    // Long messages are conversations, not answers.
    if m.is_empty() || m.split_whitespace().count() > 7 || m.contains('?') {
        return None;
    }
    let starts = |words: &[&str]| {
        words
            .iter()
            .any(|w| m == *w || m.starts_with(&format!("{w} ")) || m.starts_with(&format!("{w},")))
    };
    if starts(&[
        "done",
        "did it",
        "all done",
        "finished",
        "handled",
        "sorted",
        "all sorted",
        "completed",
        "sent",
        "sent it",
        "paid",
        "called",
        "booked",
        "✅",
        "👍",
        "yes done",
    ]) {
        return Some(ReminderAction::Done);
    }
    if starts(&[
        "drop it",
        "drop that",
        "cancel",
        "cancel it",
        "cancel that",
        "forget it",
        "forget that",
        "never mind",
        "nevermind",
        "not needed",
        "no longer needed",
        "skip it",
    ]) {
        return Some(ReminderAction::Cancel);
    }
    // A bare time ("tomorrow", "in an hour", "friday 3pm") is an answer too.
    if let Some(due) = parse_when_in_timezone(&m, now, offset_minutes) {
        return Some(ReminderAction::At(due.max(now + Duration::minutes(5))));
    }
    let later = starts(&[
        "later",
        "snooze",
        "remind me later",
        "not now",
        "push it",
        "push",
        "move it",
        "remind me",
        "ask me",
        "in a bit",
    ]);
    if !later {
        return None;
    }
    let due =
        find_when_in_text(&m, now, offset_minutes).unwrap_or_else(|| now + Duration::hours(1));
    Some(ReminderAction::At(due.max(now + Duration::minutes(5))))
}

/// The one reminder a short reply is about: the open reminder whose text the
/// assistant's last message mentioned. `None` when no single match exists.
pub fn reply_target<'a>(tasks: &[&'a Task], last_assistant: &str) -> Option<&'a Task> {
    let said = last_assistant.to_lowercase();
    let mut hits = tasks.iter().copied().filter(|t| {
        is_reminder_like(t)
            && !matches!(
                reminder_state(t),
                ReminderState::Done | ReminderState::Cancelled | ReminderState::Delegated
            )
            && {
                let what = reminder_what(t).to_lowercase();
                what.len() >= 3 && said.contains(&what)
            }
    });
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// Words for the reflex's confirmation.
pub fn reply_confirmation(
    task: &Task,
    action: &ReminderAction,
    offset_minutes: Option<i32>,
) -> String {
    let what = reminder_what(task);
    match action {
        ReminderAction::Done => format!("Marked done: {what}."),
        ReminderAction::Cancel => format!("Dropped: {what}."),
        ReminderAction::At(due) => {
            let zone =
                FixedOffset::east_opt(offset_minutes.unwrap_or_else(local_offset_minutes) * 60)
                    .unwrap_or_else(|| FixedOffset::east_opt(0).expect("utc"));
            let local = due.with_timezone(&zone);
            let today = Utc::now().with_timezone(&zone).date_naive();
            let day = if local.date_naive() == today {
                "today".to_string()
            } else if Some(local.date_naive()) == today.succ_opt() {
                "tomorrow".to_string()
            } else {
                local.format("%A").to_string()
            };
            format!(
                "Okay, I'll remind you {day} at {}: {what}.",
                local.format("%-H:%M")
            )
        }
    }
}

/// What the hourly check-in sees for one owner.
#[derive(Debug, Clone, Default)]
pub struct OpenLoops {
    /// One line per item, most urgent first.
    pub lines: Vec<String>,
    /// Changes only when an item appears, resolves, or is nudged again, so an
    /// unchanged hour stays silent.
    pub fingerprint: String,
    /// Something needs the owner this hour (nudged, parked, overdue, or due
    /// within 60 minutes). Items further out are context only.
    pub actionable: bool,
}

/// Items due within this window count as "this hour".
const ACTIONABLE_SOON_MINUTES: i64 = 60;

/// What the hourly check-in looks at for `entity`'s owner: open reminders
/// (nudged, parked, or due within the next 90 minutes) and overdue tasks,
/// most urgent first.
pub async fn open_loop_lines(
    control: &ControlState,
    entity: &crate::state::Entity,
    now: DateTime<Utc>,
) -> OpenLoops {
    let names: std::collections::HashMap<String, String> = {
        let entities = control.entities.read().await;
        let workspace = entity.workspace_id.as_deref().unwrap_or("default");
        entities
            .iter()
            .filter(|e| {
                e.id == entity.id
                    || (!e.is_personal
                        && e.workspace_id.as_deref().unwrap_or("default") == workspace)
            })
            .map(|e| (e.id.clone(), e.name.clone()))
            .collect()
    };
    let tasks = control.tasks.read().await;
    // (rank, sort key, line, fingerprint part)
    let mut rows: Vec<(u8, i64, String, String)> = Vec::new();
    for t in tasks.iter() {
        let Some(eid) = t.entity_id.as_ref() else {
            continue;
        };
        let Some(ename) = names.get(eid) else {
            continue;
        };
        let suffix = if *eid == entity.id {
            String::new()
        } else {
            format!(" ({ename})")
        };
        if is_reminder_like(t) {
            let state = reminder_state(t);
            let nudges = meta_u32(t, NUDGES_KEY);
            let what = reminder_what(t);
            let first = crate::state::task_meta_str(t, FIRST_FIRED_KEY)
                .and_then(|f| DateTime::parse_from_rfc3339(&f).ok())
                .map(|f| f.with_timezone(&Utc));
            match state {
                ReminderState::Parked | ReminderState::Due => {
                    let since = first
                        .map(|f| format!(", first {} ago", human_span(now - f)))
                        .unwrap_or_default();
                    let tag = if state == ReminderState::Parked {
                        format!("reminded {nudges}x, no answer{since}")
                    } else {
                        format!("reminded {nudges}x{since}")
                    };
                    rows.push((
                        if state == ReminderState::Parked { 0 } else { 1 },
                        -(nudges as i64),
                        format!("[{tag}] {what}{suffix}"),
                        format!("{}:{:?}:{nudges}", t.id, state),
                    ));
                }
                ReminderState::Scheduled => {
                    if let Some(due) = metadata_due_at(t.metadata.as_ref())
                        && due > now
                        && due - now <= Duration::minutes(90)
                    {
                        rows.push((
                            3,
                            (due - now).num_minutes(),
                            format!("[due in {}] {what}{suffix}", human_span(due - now)),
                            format!("{}:soon", t.id),
                        ));
                    }
                }
                _ => {}
            }
        } else if crate::state::task_is_overdue(t, now)
            && let Some(due) = crate::state::task_due_at(t)
        {
            rows.push((
                2,
                -(now - due).num_minutes(),
                format!(
                    "[overdue {}] {}{suffix}",
                    human_span(now - due),
                    t.title.trim()
                ),
                format!("{}:overdue", t.id),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    rows.truncate(8);
    let actionable = rows
        .iter()
        .any(|r| r.0 < 3 || (r.0 == 3 && r.1 <= ACTIONABLE_SOON_MINUTES));
    let mut parts: Vec<&str> = rows.iter().map(|r| r.3.as_str()).collect();
    parts.sort_unstable();
    OpenLoops {
        fingerprint: parts.join("|"),
        lines: rows.into_iter().map(|r| r.2).collect(),
        actionable,
    }
}

/// Polls the task list, fires due reminders, and runs the follow-up ladder.
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

    /// Fire everything due at `now`. Returns the number of nudges sent.
    pub async fn run_one_tick(&self, now: DateTime<Utc>) -> usize {
        // Entities in quiet hours right now: automatic follow-ups wait for them
        // to end. The first fire still goes out — the owner chose that time.
        let quiet: std::collections::HashSet<String> = {
            let entities = self.control.entities.read().await;
            let local = chrono::Local::now().time();
            entities
                .iter()
                .filter(|e| {
                    e.entity_runtime
                        .as_ref()
                        .and_then(|rt| rt.quiet_hours_local.as_ref())
                        .is_some_and(|q| crate::notify::quiet_hours_active(q, local))
                })
                .map(|e| e.id.clone())
                .collect()
        };

        struct Fire {
            task: Task,
            nudge: u32,
            push: bool,
        }
        // Mutate under the write lock, emit feed events after releasing it.
        let mut fired: Vec<Fire> = Vec::new();
        let mut changed: Vec<Task> = Vec::new();
        // (reminder, "done for you" or "couldn't do it: …")
        let mut delegation_news: Vec<(Task, String, Option<String>)> = Vec::new();
        {
            let mut tasks = self.control.tasks.write().await;
            let hours = preferred_hours(&tasks);
            let outcomes: std::collections::HashMap<String, (TaskStatus, Option<String>)> = tasks
                .iter()
                .filter(|t| t.task_kind.as_deref() == Some("machine_action"))
                .map(|t| (t.id.clone(), (t.status, t.result.clone())))
                .collect();
            for t in tasks.iter_mut() {
                if !is_reminder_like(t) {
                    continue;
                }
                let state = reminder_state(t);
                if state == ReminderState::Delegated {
                    let linked =
                        crate::state::task_meta_str(t, DELEGATED_TASK_KEY).unwrap_or_default();
                    let failed = match outcomes.get(&linked) {
                        Some((TaskStatus::Completed, _)) => {
                            apply_action(t, &ReminderAction::Done, None);
                            meta_set(t, DONE_BY_KEY, serde_json::json!("computer"));
                            delegation_news.push((
                                t.clone(),
                                format!("Done for you: {}", reminder_what(t)),
                                None,
                            ));
                            changed.push(t.clone());
                            None
                        }
                        Some((TaskStatus::Failed | TaskStatus::Cancelled, result)) => {
                            Some(result.clone())
                        }
                        // The computer task is gone: treat it like a failure.
                        None => Some(None),
                        // Waiting for approval or still running.
                        Some(_) => None,
                    };
                    if let Some(result) = failed {
                        // The owner still has to deal with it: nudge again now.
                        let why = result
                            .map(|r| r.chars().take(200).collect::<String>())
                            .filter(|r| !r.trim().is_empty())
                            .unwrap_or_else(|| "It was declined or didn't finish.".to_string());
                        // This message is the nudge; the ladder resumes an hour on.
                        reschedule(t, now + Duration::hours(1), None);
                        delegation_news.push((
                            t.clone(),
                            format!("Couldn't do it for you: {}", reminder_what(t)),
                            Some(why),
                        ));
                        changed.push(t.clone());
                    }
                    continue;
                }
                if matches!(
                    state,
                    ReminderState::Done | ReminderState::Cancelled | ReminderState::Parked
                ) {
                    continue;
                }
                let Some(due) = metadata_due_at(t.metadata.as_ref()) else {
                    continue;
                };
                if due > now {
                    continue;
                }
                let nudges = meta_u32(t, NUDGES_KEY);
                // A legacy row already fired once under the one-shot rules.
                if nudges == 0 && metadata_fired(t.metadata.as_ref()) {
                    continue;
                }
                let is_follow_up = nudges > 0;
                if is_follow_up && t.entity_id.as_ref().is_some_and(|e| quiet.contains(e)) {
                    let later = now + Duration::minutes(QUIET_RETRY_MINUTES);
                    meta_set(
                        t,
                        REMINDER_DUE_AT_KEY,
                        serde_json::json!(later.to_rfc3339()),
                    );
                    continue;
                }

                let nudge = nudges + 1;
                let due_s = due.to_rfc3339();
                // A phone that confirmed scheduling this exact fire shows it
                // itself; pushing too would ring twice.
                let push = crate::state::task_meta_str(t, LOCAL_SCHEDULED_KEY).as_deref()
                    != Some(due_s.as_str());
                meta_set(t, NUDGES_KEY, serde_json::json!(nudge));
                meta_set(t, REMINDER_FIRED_KEY, serde_json::json!(true));
                if nudge == 1 {
                    meta_set(t, FIRST_FIRED_KEY, serde_json::json!(now.to_rfc3339()));
                }
                let next = if follows_up(t) {
                    follow_up_after(
                        nudge,
                        due.max(now - Duration::minutes(5)),
                        tz_offset(t),
                        t.entity_id
                            .as_ref()
                            .and_then(|e| hours.get(e).copied())
                            .unwrap_or(MORNING_HOUR),
                    )
                } else {
                    None
                };
                match next {
                    Some(n) => {
                        meta_set(t, REMINDER_DUE_AT_KEY, serde_json::json!(n.to_rfc3339()));
                    }
                    None if follows_up(t) => meta_set(t, PARKED_KEY, serde_json::json!(true)),
                    None => {
                        // One-shot reminder: firing is the whole job.
                        if crate::state::set_task_status_by_loop(t, TaskStatus::Completed) {
                            t.outcome = Some(TaskOutcome::Completed);
                            t.progress = 100;
                        }
                    }
                }
                fired.push(Fire {
                    task: t.clone(),
                    nudge,
                    push,
                });
                changed.push(t.clone());
            }
        }
        for t in &changed {
            self.control.persist_task_row(t).await;
        }
        for (t, title, detail) in delegation_news {
            self.control
                .push_reminder_feed(&t, title, detail, true)
                .await;
        }
        for f in &fired {
            let (title, detail) = nudge_copy(&f.task, f.nudge);
            self.control
                .push_reminder_feed(&f.task, title, detail, f.push)
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

    fn control() -> Arc<ControlState> {
        Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ))
    }

    fn reminder(id: &str, due: DateTime<Utc>) -> Task {
        Task {
            id: id.to_string(),
            title: format!("Reminder: {id}"),
            task_kind: Some("reminder".to_string()),
            status: TaskStatus::Backlog,
            metadata: Some(schedule_metadata(due, Some(0))),
            ..Task::default()
        }
    }

    async fn get(control: &ControlState, id: &str) -> Task {
        control
            .tasks
            .read()
            .await
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .unwrap()
    }

    #[tokio::test]
    async fn ticker_fires_once_then_follows_up_until_parked() {
        let control = control();
        let n = now(); // 10:00 UTC
        {
            let mut tasks = control.tasks.write().await;
            tasks.push(reminder("due", n - Duration::minutes(1)));
            tasks.push(reminder("future", n + Duration::hours(1)));
            // A reminder without due_at must never fire (legacy rows).
            tasks.push(Task {
                id: "legacy".into(),
                task_kind: Some("reminder".into()),
                ..Task::default()
            });
        }
        let ticker = ReminderTicker::new(control.clone(), 60);

        assert_eq!(ticker.run_one_tick(n).await, 1, "only the due one fires");
        assert_eq!(ticker.run_one_tick(n).await, 0, "idempotent second tick");
        assert_eq!(
            reminder_state(&get(&control, "future").await),
            ReminderState::Scheduled
        );
        control.tasks.write().await.retain(|t| t.id == "due");
        let t = get(&control, "due").await;
        assert_eq!(reminder_state(&t), ReminderState::Due);
        assert_ne!(t.status, TaskStatus::Completed, "firing never completes it");
        assert_eq!(meta_u32(&t, NUDGES_KEY), 1);
        // Next nudge an hour after the fire.
        let next = metadata_due_at(t.metadata.as_ref()).unwrap();
        assert_eq!(next, n - Duration::minutes(1) + Duration::hours(1));

        // Second nudge, then next morning 09:00, then the morning after, then parked.
        assert_eq!(ticker.run_one_tick(next).await, 1);
        let t = get(&control, "due").await;
        let morning = metadata_due_at(t.metadata.as_ref()).unwrap();
        assert_eq!(morning, Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap());
        assert_eq!(ticker.run_one_tick(morning).await, 1);
        let t = get(&control, "due").await;
        let last = metadata_due_at(t.metadata.as_ref()).unwrap();
        assert_eq!(last, Utc.with_ymd_and_hms(2026, 7, 19, 9, 0, 0).unwrap());
        assert_eq!(ticker.run_one_tick(last).await, 1);
        let t = get(&control, "due").await;
        assert_eq!(reminder_state(&t), ReminderState::Parked);
        assert_eq!(meta_u32(&t, NUDGES_KEY), 4);
        assert_eq!(
            ticker.run_one_tick(last + Duration::days(3)).await,
            0,
            "parked stays quiet"
        );

        let feed = control.operator_feed.read().await;
        let titles: Vec<&str> = feed.iter().map(|e| e.title.as_str()).collect();
        assert!(titles.contains(&"Reminder: due"), "{titles:?}");
        assert!(
            titles.iter().any(|t| t.starts_with("Still open: ")),
            "{titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.starts_with("Still relevant? ")),
            "{titles:?}"
        );
        assert!(
            feed.iter()
                .filter(|e| e.kind == crate::state::OperatorFeedKind::ScheduledTaskFired)
                .all(|e| e.task_id.as_deref() == Some("due")),
            "every nudge carries the reminder id"
        );
    }

    #[test]
    fn normalizes_reminder_text_for_duplicate_checks() {
        assert_eq!(
            normalize_what("Call the dentist."),
            normalize_what("to call the  dentist")
        );
        assert_ne!(
            normalize_what("call the dentist"),
            normalize_what("call the plumber")
        );
        assert_eq!(normalize_what("  "), "");
    }

    #[test]
    fn reads_short_replies_to_a_nudge() {
        let n = now(); // Friday 10:00 UTC
        let at = |m: &str| parse_reply(m, n, Some(0));
        assert_eq!(at("done"), Some(ReminderAction::Done));
        assert_eq!(at("Done, sent it!"), Some(ReminderAction::Done));
        assert_eq!(at("did it 👍"), Some(ReminderAction::Done));
        assert_eq!(
            at("drop it, not needed anymore"),
            Some(ReminderAction::Cancel)
        );
        assert_eq!(
            at("push it to tomorrow"),
            Some(ReminderAction::At(
                Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap()
            ))
        );
        assert_eq!(
            at("remind me in an hour"),
            Some(ReminderAction::At(n + Duration::hours(1)))
        );
        assert_eq!(
            at("later"),
            Some(ReminderAction::At(n + Duration::hours(1)))
        );
        assert_eq!(
            at("monday"),
            Some(ReminderAction::At(
                Utc.with_ymd_and_hms(2026, 7, 20, 9, 0, 0).unwrap()
            ))
        );
        // Not answers: questions, conversation, unrelated words.
        assert_eq!(at("done with what?"), None);
        assert_eq!(at("what should I focus on today"), None);
        assert_eq!(
            at("doner kebab tonight sounds great honestly, lets go now"),
            None
        );
        assert_eq!(at("thanks"), None);
    }

    #[test]
    fn a_reply_targets_the_one_reminder_just_mentioned() {
        let n = now();
        let mut photo = reminder("photo", n);
        photo.result = Some("Capture:\nsend ABT the photo".into());
        let mut bill = reminder("bill", n);
        bill.result = Some("Capture:\npay the electricity bill".into());
        let refs = [&photo, &bill];
        let target = reply_target(
            &refs,
            "Still open: send ABT the photo. Tap Done when it's handled.",
        );
        assert_eq!(target.map(|t| t.id.as_str()), Some("photo"));
        assert!(reply_target(&refs, "Morning! Anything I can help with?").is_none());
        assert!(
            reply_target(
                &refs,
                "Still open: send ABT the photo, and pay the electricity bill."
            )
            .is_none(),
            "two candidates: let the model ask"
        );
    }

    #[tokio::test]
    async fn delegated_reminders_wait_then_close_or_revive() {
        let control = control();
        let n = now();
        let mut done_one = reminder("photo", n - Duration::minutes(5));
        mark_delegated(&mut done_one, "job_ok", "appr_1");
        let mut failed_one = reminder("invoice", n - Duration::minutes(5));
        mark_delegated(&mut failed_one, "job_bad", "appr_2");
        let job = |id: &str, status: TaskStatus| Task {
            id: id.into(),
            task_kind: Some("machine_action".into()),
            status,
            result: Some("Couldn't find the invoice PDF on the Desktop.".into()),
            ..Task::default()
        };
        {
            let mut tasks = control.tasks.write().await;
            tasks.extend([done_one, failed_one]);
            tasks.push(job("job_ok", TaskStatus::Queued));
            tasks.push(job("job_bad", TaskStatus::Queued));
        }
        let ticker = ReminderTicker::new(control.clone(), 60);
        assert_eq!(
            ticker.run_one_tick(n).await,
            0,
            "delegated reminders don't nudge"
        );
        assert_eq!(
            reminder_state(&get(&control, "photo").await),
            ReminderState::Delegated
        );
        assert!(planned_nudges(&get(&control, "photo").await).is_empty());

        {
            let mut tasks = control.tasks.write().await;
            for t in tasks.iter_mut() {
                match t.id.as_str() {
                    "job_ok" => t.status = TaskStatus::Completed,
                    "job_bad" => t.status = TaskStatus::Failed,
                    _ => {}
                }
            }
        }
        ticker.run_one_tick(n).await;
        assert_eq!(
            reminder_state(&get(&control, "photo").await),
            ReminderState::Done
        );
        let revived = get(&control, "invoice").await;
        assert_eq!(reminder_state(&revived), ReminderState::Scheduled);
        assert_eq!(
            metadata_due_at(revived.metadata.as_ref()),
            Some(n + Duration::hours(1))
        );
        let feed = control.operator_feed.read().await;
        assert!(feed.iter().any(|e| e.title == "Done for you: photo"));
        assert!(feed.iter().any(|e| {
            e.title == "Couldn't do it for you: invoice"
                && e.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("invoice PDF"))
        }));
    }

    #[tokio::test]
    async fn one_shot_reminders_complete_on_fire() {
        let control = control();
        let n = now();
        let mut r = reminder("once", n - Duration::minutes(1));
        meta_set(&mut r, FOLLOW_UP_KEY, serde_json::json!(false));
        control.tasks.write().await.push(r);
        let ticker = ReminderTicker::new(control.clone(), 60);
        assert_eq!(ticker.run_one_tick(n).await, 1);
        assert_eq!(get(&control, "once").await.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn legacy_fired_rows_do_not_refire() {
        let control = control();
        let n = now();
        let mut r = reminder("old", n - Duration::minutes(1));
        meta_set(&mut r, REMINDER_FIRED_KEY, serde_json::json!("true"));
        meta_set(&mut r, NUDGES_KEY, serde_json::json!(0));
        control.tasks.write().await.push(r);
        let ticker = ReminderTicker::new(control.clone(), 60);
        assert_eq!(ticker.run_one_tick(n).await, 0);
    }

    #[test]
    fn actions_done_cancel_and_snooze() {
        let n = now();
        let mut t = reminder("a", n);
        meta_set(&mut t, NUDGES_KEY, serde_json::json!(3));
        meta_set(&mut t, PARKED_KEY, serde_json::json!(true));
        apply_action(&mut t, &ReminderAction::At(n + Duration::hours(2)), None);
        assert_eq!(
            reminder_state(&t),
            ReminderState::Scheduled,
            "snooze restarts the ladder"
        );
        assert_eq!(meta_u32(&t, NUDGES_KEY), 0);
        assert_eq!(
            metadata_due_at(t.metadata.as_ref()),
            Some(n + Duration::hours(2))
        );

        apply_action(&mut t, &ReminderAction::Done, None);
        assert_eq!(reminder_state(&t), ReminderState::Done);
        assert!(planned_nudges(&t).is_empty());
        // Snoozing a finished one brings it back.
        apply_action(&mut t, &ReminderAction::At(n + Duration::hours(1)), None);
        assert_eq!(reminder_state(&t), ReminderState::Scheduled);
        apply_action(&mut t, &ReminderAction::Cancel, None);
        assert_eq!(reminder_state(&t), ReminderState::Cancelled);
    }

    #[test]
    fn planned_nudges_match_what_the_ticker_will_do() {
        let n = now();
        let t = reminder("p", n + Duration::hours(1)); // 11:00 UTC
        let plan = planned_nudges(&t);
        assert_eq!(
            plan,
            vec![
                n + Duration::hours(1),
                n + Duration::hours(2),
                Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 7, 19, 9, 0, 0).unwrap(),
            ]
        );
        let v = view(&t, Some("Personal".into()));
        assert_eq!(v.fire_at.as_deref(), Some(plan[0].to_rfc3339().as_str()));
        assert_eq!(v.follow_ups.len(), 3);
        assert_eq!(v.what, "Reminder: p".strip_prefix("Reminder: ").unwrap());
    }

    #[test]
    fn learns_the_hour_the_owner_gets_things_done() {
        let done_at = |id: &str, hour: u32, by_computer: bool| {
            let mut t = reminder(id, now());
            apply_action(&mut t, &ReminderAction::Done, None);
            t.completed_at = Some(
                Utc.with_ymd_and_hms(2026, 7, 10, hour, 15, 0)
                    .unwrap()
                    .to_rfc3339(),
            );
            if by_computer {
                meta_set(&mut t, DONE_BY_KEY, serde_json::json!("computer"));
            }
            t
        };
        // Too little evidence: the default.
        let few: Vec<Task> = (0..3)
            .map(|i| done_at(&format!("f{i}"), 19, false))
            .collect();
        assert_eq!(
            preferred_hour(&few.iter().collect::<Vec<_>>()),
            MORNING_HOUR
        );
        // An evening person (tz 0), plus computer completions that must not count.
        let mut many: Vec<Task> = (0..6)
            .map(|i| done_at(&format!("e{i}"), 19, false))
            .collect();
        many.push(done_at("m1", 8, false));
        many.extend((0..9).map(|i| done_at(&format!("c{i}"), 3 + (i % 2), true)));
        assert_eq!(preferred_hour(&many.iter().collect::<Vec<_>>()), 19);
        assert_eq!(
            follow_up_after(2, now(), 0, 19),
            Some(Utc.with_ymd_and_hms(2026, 7, 17, 19, 0, 0).unwrap()),
            "the next-morning nudge now lands at their hour"
        );
    }

    #[test]
    fn morning_follow_up_respects_the_owner_timezone() {
        // Fired 10:00 UTC for someone at UTC+2: the next 09:00 local is 07:00 UTC tomorrow.
        let n = now();
        assert_eq!(
            follow_up_after(2, n, 120, MORNING_HOUR),
            Some(Utc.with_ymd_and_hms(2026, 7, 18, 7, 0, 0).unwrap())
        );
    }

    #[test]
    fn notes_and_tasks_with_remind_are_reminder_like() {
        let mut t = Task {
            task_kind: Some("note".into()),
            ..Task::default()
        };
        assert!(!is_reminder_like(&t));
        reschedule(&mut t, now(), None);
        assert!(is_reminder_like(&t));
        assert_eq!(t.task_kind.as_deref(), Some("note"), "stays a note");
    }

    #[test]
    fn splits_a_captured_sentence_into_what_and_when() {
        let n = now(); // Friday 10:00 UTC
        let cases = [
            (
                "send ABT the photo tomorrow at 9am",
                "send ABT the photo",
                Some(Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap()),
            ),
            (
                "Remind me to call mum in 2 hours",
                "call mum",
                Some(n + Duration::hours(2)),
            ),
            (
                "pay the electricity bill on monday",
                "pay the electricity bill",
                Some(Utc.with_ymd_and_hms(2026, 7, 20, 9, 0, 0).unwrap()),
            ),
            (
                "tonight, water the plants",
                "water the plants",
                Some(Utc.with_ymd_and_hms(2026, 7, 17, 20, 0, 0).unwrap()),
            ),
            ("buy 3 apples", "buy 3 apples", None),
            ("need to renew the domain", "renew the domain", None),
        ];
        for (text, what, due) in cases {
            assert_eq!(
                split_when(text, n, Some(0)),
                (what.to_string(), due),
                "{text}"
            );
        }
    }

    #[test]
    fn finds_times_inside_free_text() {
        let n = now(); // Friday 2026-07-17 10:00 UTC
        assert_eq!(
            find_when_in_text("send ABT the photo tomorrow at 9am please", n, Some(0)),
            Some(Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap())
        );
        assert_eq!(
            find_when_in_text("call mum in 2 hours", n, Some(0)),
            Some(n + Duration::hours(2))
        );
        assert_eq!(find_when_in_text("buy 3 apples", n, Some(0)), None);
        assert_eq!(
            tomorrow_morning(n, Some(0)),
            Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap()
        );
    }

    #[test]
    fn parses_tomorrow_parts_of_day() {
        let n = now();
        let at = |h| Some(Utc.with_ymd_and_hms(2026, 7, 18, h, 0, 0).unwrap());
        assert_eq!(parse_when("tomorrow evening", n), at(18));
        assert_eq!(parse_when("tomorrow morning", n), at(9));
        assert_eq!(parse_when("tomorrow afternoon", n), at(15));
        assert_eq!(parse_when("tomorrow night", n), at(20));
        assert_eq!(
            parse_reply("push it to tomorrow evening", n, Some(0)),
            Some(ReminderAction::At(
                Utc.with_ymd_and_hms(2026, 7, 18, 18, 0, 0).unwrap()
            ))
        );
    }

    #[test]
    fn parses_weekdays() {
        let n = now(); // Friday
        assert_eq!(
            parse_when("monday", n),
            Some(Utc.with_ymd_and_hms(2026, 7, 20, 9, 0, 0).unwrap())
        );
        assert_eq!(
            parse_when("on friday at 2pm", n),
            Some(Utc.with_ymd_and_hms(2026, 7, 24, 14, 0, 0).unwrap()),
            "today's weekday means next week"
        );
        assert_eq!(
            parse_when("next tuesday evening", n),
            Some(Utc.with_ymd_and_hms(2026, 7, 21, 18, 0, 0).unwrap())
        );
        assert_eq!(parse_when("fridays are fun", n), None);
    }

    #[tokio::test]
    async fn open_loops_list_and_fingerprint() {
        let control = control();
        let n = now();
        let personal = crate::state::Entity {
            id: "me".into(),
            name: "Personal".into(),
            is_personal: true,
            ..Default::default()
        };
        control.entities.write().await.push(personal.clone());
        let mut due = reminder("photo", n - Duration::hours(3));
        due.entity_id = Some("me".into());
        due.result = Some("Capture:\nsend ABT the photo".into());
        meta_set(&mut due, NUDGES_KEY, serde_json::json!(2));
        let mut soon = reminder("call", n + Duration::minutes(30));
        soon.entity_id = Some("me".into());
        let mut later = reminder("later", n + Duration::hours(5));
        later.entity_id = Some("me".into());
        control.tasks.write().await.extend([due, soon, later]);

        let loops = open_loop_lines(&control, &personal, n).await;
        let lines = &loops.lines;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("reminded 2x") && lines[0].contains("send ABT the photo"));
        assert!(lines[1].starts_with("[due in 30 min]"));
        assert!(loops.actionable);
        let again = open_loop_lines(&control, &personal, n + Duration::minutes(5)).await;
        assert_eq!(
            loops.fingerprint, again.fingerprint,
            "time passing alone doesn't count as news"
        );

        // Only something 75 minutes out: listed, but not worth interrupting for.
        control.tasks.write().await.retain(|t| t.id == "later");
        let quiet = open_loop_lines(
            &control,
            &personal,
            n + Duration::hours(3) + Duration::minutes(45),
        )
        .await;
        assert_eq!(quiet.lines.len(), 1, "{:?}", quiet.lines);
        assert!(!quiet.actionable);
    }
}
