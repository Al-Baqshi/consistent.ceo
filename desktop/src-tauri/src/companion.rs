//! Desktop companion: watches the gateway's operator feed and raises native
//! notifications for the things a CEO should be interrupted for.
//!
//! The desktop shell is a WebView over the locally-served dashboard, so until
//! now the only alerting it had was the web layer's browser `Notification`
//! calls. Those are tied to a live window: minimise the app and the operator
//! stops hearing from their own system. Polling here, in the Rust shell, means
//! reminders and approvals land in the OS notification centre whether or not
//! the window is open — and the shell already owns the gateway process, so it
//! has the auth token and a guaranteed-local endpoint.
//!
//! The notifiable-kind list mirrors `is_notifiable` in
//! `crates/gateway/src/notify.rs`. It is duplicated on purpose: this shell can
//! be pointed at an older gateway, and an unknown kind should be ignored
//! rather than mis-notified.
//!
//! Reminders that fired while the app was closed are caught up on launch from
//! `/api/reminders` (the feed only says what happened since the cursor).
//!
//! Clicking a notification: tauri-plugin-notification 2.3 on desktop is
//! fire-and-forget (notify-rust `show()`, no click callback or actions), so a
//! click can't route anywhere. The tray menu carries "Reminders (N due)"
//! instead, which brings the window forward on the Reminders panel.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_notification::NotificationExt;

use crate::agent::GatewayAuth;

const POLL_SECS: u64 = 5;
const FEED_LIMIT: usize = 25;
/// A backlog (gateway was down, laptop was asleep) shouldn't arrive as a wall
/// of banners.
const MAX_NOTIFICATIONS_PER_TICK: usize = 3;
const BODY_MAX_CHARS: usize = 180;
/// Re-read `/api/reminders` for the tray count every this many feed polls.
const REMINDER_REFRESH_TICKS: u32 = 12;
/// Launch catch-up only covers nudges this recent.
const CATCH_UP_HOURS: i64 = 12;
const SHOWN_FILE: &str = "companion_reminders_shown.json";
const SHOWN_KEEP_DAYS: i64 = 7;

// One row from the gateway's operator feed, as returned by
// /api/operator-feed.
#[derive(Debug, Clone, Deserialize)]
struct FeedEvent {
    id: String,
    kind: String,
    title: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default, rename = "taskId")]
    task_id: Option<String>,
}

// One row from /api/reminders (gateway `reminders::ReminderView`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReminderRow {
    id: String,
    #[serde(default)]
    what: String,
    #[serde(default)]
    title: String,
    state: String,
    #[serde(default)]
    first_fired_at: Option<String>,
    #[serde(default)]
    nudges: u32,
}

#[derive(Debug, Deserialize)]
struct RemindersPage {
    #[serde(default)]
    items: Vec<ReminderRow>,
}

#[derive(Debug, Deserialize)]
struct FeedPage {
    #[serde(default)]
    items: Vec<FeedEvent>,
}

/// Kinds arrive over the wire exactly as `OperatorFeedKind` serialises, which is
/// `#[serde(rename_all = "snake_case")]` — `"agent_run_completed"`, not
/// `"AgentRunCompleted"`. Matching the Rust variant spelling here meant nothing
/// ever matched and the shell raised **zero** OS notifications, silently, since
/// the feature shipped. Keep these strings in the wire spelling.
fn is_notifiable(kind: &str) -> bool {
    matches!(
        kind,
        "scheduled_task_fired"
            | "approval_needed"
            | "approval_request_created"
            | "mission_user_input_needed"
            | "heartbeat_result"
            | "agent_run_completed"
            | "agent_run_failed"
            | "mission_stalled"
            | "task_stuck"
            | "task_blocked"
            | "mission_completed"
            // A commitment slipped, the morning brief is ready, or an entity is
            // waiting to be told where its work should live.
            | "task_overdue"
            | "daily_digest"
            | "entity_workspace_needed"
            // A "done" the verifier didn't believe, a scheduled mission review,
            // a dead integration, a final report waiting to be read.
            | "verification_needs_review"
            | "mission_review_due"
            | "connection_check_failed"
            | "mission_report_ready"
    )
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "scheduled_task_fired" => "Reminder",
        "approval_needed" | "approval_request_created" => "Needs your approval",
        "mission_user_input_needed" => "Needs your input",
        "heartbeat_result" => "Heartbeat",
        "agent_run_completed" => "Run finished",
        "agent_run_failed" => "Run failed",
        "mission_stalled" => "Mission stalled",
        "task_stuck" => "Task stuck",
        "task_blocked" => "Task blocked",
        "mission_completed" => "Mission completed",
        "task_overdue" => "Overdue",
        "daily_digest" => "Your day",
        "entity_workspace_needed" => "Where should this live?",
        "verification_needs_review" => "Needs review",
        "mission_review_due" => "Review due",
        "connection_check_failed" => "Connection failed",
        "mission_report_ready" => "Report ready",
        _ => "Consistent CEO",
    }
}

// Char-boundary-safe truncation (never splits a multi-byte character), with
// a trailing ellipsis when text is actually cut.
fn truncate(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// Events newer than `cursor`, oldest-first. The feed arrives newest-first, so
/// everything ahead of the cursor is new. An unknown cursor (the row aged out
/// of the 200-event ring) yields the whole window, which the caller then caps.
fn new_events_since(feed: &[FeedEvent], cursor: &str) -> Vec<FeedEvent> {
    let idx = feed.iter().position(|e| e.id == cursor);
    let mut fresh: Vec<FeedEvent> = match idx {
        Some(i) => feed[..i].to_vec(),
        None => feed.to_vec(),
    };
    fresh.reverse();
    fresh
}

/// Cursor state: the newest feed event this shell has already shown.
#[derive(Default)]
pub struct FeedCursor(Mutex<Option<String>>);

/// The tray's "Reminders" entry, relabelled with the due count.
#[derive(Default)]
pub struct TrayReminders(pub Mutex<Option<MenuItem<Wry>>>);

// GET a gateway endpoint with the shell's bearer token. Any failure — network,
// auth, bad JSON — collapses to None so the poller just retries next tick.
async fn get_json<T: serde::de::DeserializeOwned>(app: &AppHandle, path: &str) -> Option<T> {
    let (base_url, token) = {
        let auth = app.state::<GatewayAuth>();
        (auth.base_url.clone(), auth.token())
    };
    let mut req = reqwest::Client::new()
        .get(format!("{base_url}{path}"))
        .timeout(Duration::from_secs(4));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

async fn fetch_feed(app: &AppHandle) -> Option<Vec<FeedEvent>> {
    let page: FeedPage = get_json(app, &format!("/api/operator-feed?limit={FEED_LIMIT}")).await?;
    Some(page.items)
}

async fn fetch_reminders(app: &AppHandle) -> Option<Vec<ReminderRow>> {
    let page: RemindersPage = get_json(app, "/api/reminders?include=open").await?;
    Some(page.items)
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Show the main window on the dashboard's Reminders panel.
pub fn open_reminders(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_focus();
    // Leave sign-in and the boot loader alone; anywhere else, go home to the panel.
    let _ = w.eval(
        "(function(){var h='#reminders',p=location.pathname;\
         if(p.indexOf('/login')===0||p.indexOf('/loader')===0)return;\
         if(p!=='/'){location.assign('/'+h);return;}\
         if(location.hash!==h){location.hash=h;}\
         else{window.dispatchEvent(new CustomEvent('consistent-world:open-reminders'));}})();",
    );
}

fn tray_label(due: usize) -> String {
    if due == 0 {
        "Reminders".to_string()
    } else {
        format!("Reminders ({due} due)")
    }
}

fn update_tray(app: &AppHandle, reminders: &[ReminderRow]) {
    let due = reminders.iter().filter(|r| r.state == "due").count();
    if let Some(state) = app.try_state::<TrayReminders>() {
        if let Ok(guard) = state.0.lock() {
            if let Some(item) = guard.as_ref() {
                let _ = item.set_text(tray_label(due));
            }
        }
    }
}

// ── Which reminders this machine has already announced ─────────────────────
//
// Reminder id → when the shell last raised a notification for it, persisted so
// a relaunch doesn't re-announce what the operator already saw.

fn shown_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join(SHOWN_FILE))
}

fn load_shown(app: &AppHandle) -> HashMap<String, String> {
    shown_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_shown(app: &AppHandle, shown: &mut HashMap<String, String>, now: DateTime<Utc>) {
    let cutoff = now - chrono::Duration::days(SHOWN_KEEP_DAYS);
    shown.retain(|_, at| parse_time(at).map(|t| t >= cutoff).unwrap_or(false));
    let Some(path) = shown_path(app) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string(shown) {
        let _ = std::fs::write(path, raw);
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Roughly when the latest nudge went out. The view carries only the first
/// fire and a count; the gateway's ladder is +1h, then next morning, then the
/// morning after (`reminders::follow_up_after`), so this is an estimate.
fn last_nudge_estimate(first: DateTime<Utc>, nudges: u32, now: DateTime<Utc>) -> DateTime<Utc> {
    let est = match nudges {
        0 | 1 => first,
        2 => first + chrono::Duration::hours(1),
        3 => first + chrono::Duration::hours(16),
        _ => first + chrono::Duration::hours(40),
    };
    est.min(now)
}

/// Due reminders whose latest nudge was recent and hasn't been announced here.
fn catch_up_candidates<'a>(
    reminders: &'a [ReminderRow],
    shown: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> Vec<&'a ReminderRow> {
    reminders
        .iter()
        .filter(|r| r.state == "due")
        .filter(|r| {
            let Some(first) = r.first_fired_at.as_deref().and_then(parse_time) else {
                return false;
            };
            let last = last_nudge_estimate(first, r.nudges, now);
            if now - last > chrono::Duration::hours(CATCH_UP_HOURS) {
                return false;
            }
            match shown.get(&r.id).and_then(|s| parse_time(s)) {
                Some(seen) => seen + chrono::Duration::minutes(5) < last,
                None => true,
            }
        })
        .collect()
}

fn reminder_text(r: &ReminderRow) -> &str {
    if r.what.trim().is_empty() {
        &r.title
    } else {
        &r.what
    }
}

/// On launch: announce reminders that fired while the app was closed.
fn catch_up(app: &AppHandle, reminders: &[ReminderRow]) {
    let now = Utc::now();
    let mut shown = load_shown(app);
    let pending = catch_up_candidates(reminders, &shown, now);
    if pending.is_empty() {
        return;
    }
    if pending.len() > MAX_NOTIFICATIONS_PER_TICK {
        let list: Vec<&str> = pending.iter().map(|r| reminder_text(r)).collect();
        notify(
            app,
            &format!("{} reminders waiting", pending.len()),
            &truncate(&list.join(" · "), BODY_MAX_CHARS),
        );
    } else {
        for r in &pending {
            notify(
                app,
                &format!("Reminder · {}", truncate(reminder_text(r), 60)),
                "Still open. Open Consistent to mark it done or snooze it.",
            );
        }
    }
    let stamp = now.to_rfc3339();
    for r in pending {
        shown.insert(r.id.clone(), stamp.clone());
    }
    save_shown(app, &mut shown, now);
}

/// Poll the feed forever, notifying on anything new and notifiable.
///
/// Every failure path is a silent skip-and-retry: the gateway may still be
/// booting, may be mid-restart, or the operator may have quit it. A companion
/// that spams errors about its own transport is worse than one that waits.
pub fn spawn_feed_watcher(app: AppHandle) {
    app.manage(FeedCursor::default());
    tauri::async_runtime::spawn(async move {
        let mut seen: HashSet<String> = HashSet::new();
        let mut caught_up = false;
        let mut ticks: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;

            let Some(feed) = fetch_feed(&app).await else {
                continue;
            };

            if !caught_up || ticks.is_multiple_of(REMINDER_REFRESH_TICKS) {
                if let Some(reminders) = fetch_reminders(&app).await {
                    if !caught_up {
                        catch_up(&app, &reminders);
                        caught_up = true;
                    }
                    update_tray(&app, &reminders);
                }
            }
            ticks = ticks.wrapping_add(1);

            let Some(newest) = feed.first().map(|e| e.id.clone()) else {
                continue;
            };

            let cursor = {
                let state = app.state::<FeedCursor>();
                let guard = state.0.lock().ok();
                guard.and_then(|g| g.clone())
            };

            match cursor {
                // First successful poll only anchors the cursor — the operator
                // doesn't want banners for everything that happened before the
                // app was opened. Reminders among them are the catch-up's job.
                None => {}
                Some(c) => {
                    let fresh: Vec<FeedEvent> = new_events_since(&feed, &c)
                        .into_iter()
                        .filter(|e| is_notifiable(&e.kind))
                        .filter(|e| !seen.contains(&e.id))
                        .collect();
                    let start = fresh.len().saturating_sub(MAX_NOTIFICATIONS_PER_TICK);
                    for event in &fresh[start..] {
                        let title = format!(
                            "{} · {}",
                            kind_label(&event.kind),
                            truncate(&event.title, 60)
                        );
                        let body = truncate(
                            event.detail.as_deref().unwrap_or(&event.title),
                            BODY_MAX_CHARS,
                        );
                        notify(&app, &title, &body);
                    }
                    let fired: Vec<&str> = fresh
                        .iter()
                        .filter(|e| e.kind == "scheduled_task_fired")
                        .filter_map(|e| e.task_id.as_deref())
                        .collect();
                    if !fired.is_empty() {
                        let now = Utc::now();
                        let mut shown = load_shown(&app);
                        for id in fired {
                            shown.insert(id.to_string(), now.to_rfc3339());
                        }
                        save_shown(&app, &mut shown, now);
                        if let Some(reminders) = fetch_reminders(&app).await {
                            update_tray(&app, &reminders);
                        }
                    }
                    for event in fresh {
                        seen.insert(event.id);
                    }
                    // The ring is 200 events; keeping a little more than that
                    // is enough to never re-notify, and bounds memory.
                    if seen.len() > 512 {
                        seen.clear();
                    }
                }
            }

            if let Some(state) = app.try_state::<FeedCursor>() {
                if let Ok(mut guard) = state.0.lock() {
                    *guard = Some(newest);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, kind: &str) -> FeedEvent {
        FeedEvent {
            id: id.to_string(),
            kind: kind.to_string(),
            title: "t".to_string(),
            detail: None,
            task_id: None,
        }
    }

    fn rem(id: &str, state: &str, first: Option<DateTime<Utc>>, nudges: u32) -> ReminderRow {
        ReminderRow {
            id: id.to_string(),
            what: format!("do {id}"),
            title: String::new(),
            state: state.to_string(),
            first_fired_at: first.map(|t| t.to_rfc3339()),
            nudges,
        }
    }

    #[test]
    fn catch_up_takes_recent_unannounced_due_reminders() {
        let now = Utc::now();
        let h = chrono::Duration::hours;
        let rows = vec![
            rem("recent", "due", Some(now - h(2)), 1),
            rem("old", "due", Some(now - h(30)), 1),
            rem("scheduled", "scheduled", None, 0),
            // First fired long ago, but the 2nd follow-up was this morning.
            rem("followed_up", "due", Some(now - h(20)), 3),
            rem("seen", "due", Some(now - h(1)), 1),
        ];
        let mut shown = HashMap::new();
        shown.insert(
            "seen".to_string(),
            (now - chrono::Duration::minutes(30)).to_rfc3339(),
        );
        let ids: Vec<&str> = catch_up_candidates(&rows, &shown, now)
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, ["recent", "followed_up"]);
    }

    #[test]
    fn a_newer_nudge_than_the_last_announcement_is_shown_again() {
        let now = Utc::now();
        let rows = vec![rem("r", "due", Some(now - chrono::Duration::hours(3)), 2)];
        let mut shown = HashMap::new();
        shown.insert(
            "r".to_string(),
            (now - chrono::Duration::hours(3)).to_rfc3339(),
        );
        assert_eq!(catch_up_candidates(&rows, &shown, now).len(), 1);
    }

    #[test]
    fn feed_events_carry_the_reminder_id() {
        let page: FeedPage = serde_json::from_str(
            r#"{"items":[{"id":"1","kind":"scheduled_task_fired","title":"Reminder: call mum","taskId":"t-1"}]}"#,
        )
        .unwrap();
        assert_eq!(page.items[0].task_id.as_deref(), Some("t-1"));
    }

    #[test]
    fn tray_label_counts_due() {
        assert_eq!(tray_label(0), "Reminders");
        assert_eq!(tray_label(2), "Reminders (2 due)");
    }

    #[test]
    fn new_events_are_returned_oldest_first() {
        // Feed arrives newest-first: c, b, a. Cursor at `a`.
        let feed = vec![
            ev("c", "scheduled_task_fired"),
            ev("b", "task_created"),
            ev("a", "task_created"),
        ];
        let fresh = new_events_since(&feed, "a");
        assert_eq!(
            fresh.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn cursor_at_newest_yields_nothing() {
        let feed = vec![ev("c", "scheduled_task_fired"), ev("b", "task_created")];
        assert!(new_events_since(&feed, "c").is_empty());
    }

    #[test]
    fn unknown_cursor_yields_the_whole_window() {
        let feed = vec![ev("c", "scheduled_task_fired"), ev("b", "task_created")];
        assert_eq!(new_events_since(&feed, "gone").len(), 2);
    }

    #[test]
    fn only_interruption_worthy_kinds_notify() {
        assert!(is_notifiable("scheduled_task_fired"));
        assert!(is_notifiable("heartbeat_result"));
        assert!(is_notifiable("task_overdue"));
        assert!(is_notifiable("daily_digest"));
        assert!(is_notifiable("verification_needs_review"));
        assert!(is_notifiable("mission_review_due"));
        assert!(is_notifiable("connection_check_failed"));
        assert!(is_notifiable("mission_report_ready"));
        assert!(!is_notifiable("task_created"));
        assert!(!is_notifiable("goal_loop_decision"));
    }

    /// The bug this guards: kinds were listed in Rust variant spelling while the
    /// gateway serialises snake_case, so nothing matched and no notification
    /// ever fired. Assert against a real captured payload rather than a
    /// hand-written kind string, so the wire format is what's being tested.
    #[test]
    fn kinds_are_matched_in_the_wire_spelling_the_gateway_sends() {
        let page: FeedPage = serde_json::from_str(
            r#"{"items":[{"id":"1","kind":"agent_run_completed","title":"Executed: build the page"}],
                "total":1,"limit":25,"offset":0,"hasMore":false}"#,
        )
        .expect("the gateway's feed page shape must deserialize");
        let event = &page.items[0];
        assert!(
            is_notifiable(&event.kind),
            "`{}` came straight off the wire and must notify",
            event.kind
        );
        assert_eq!(kind_label(&event.kind), "Run finished");
        assert!(
            !is_notifiable("AgentRunCompleted"),
            "variant spelling is not what the gateway sends; matching it hid the outage"
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        let s = "é".repeat(50);
        assert_eq!(truncate(&s, 10).chars().count(), 10);
    }
}
