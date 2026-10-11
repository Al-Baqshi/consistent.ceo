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

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::agent::GatewayAuth;

const POLL_SECS: u64 = 5;
const FEED_LIMIT: usize = 25;
/// A backlog (gateway was down, laptop was asleep) shouldn't arrive as a wall
/// of banners.
const MAX_NOTIFICATIONS_PER_TICK: usize = 3;
const BODY_MAX_CHARS: usize = 180;

// One row from the gateway's operator feed, as returned by
// /api/operator-feed.
#[derive(Debug, Clone, Deserialize)]
struct FeedEvent {
    id: String,
    kind: String,
    title: String,
    #[serde(default)]
    detail: Option<String>,
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

// GET the operator feed with the shell's bearer token. Any failure — network,
// auth, bad JSON — collapses to None so the poller just retries next tick.
async fn fetch_feed(app: &AppHandle) -> Option<Vec<FeedEvent>> {
    let (base_url, token) = {
        let auth = app.state::<GatewayAuth>();
        (auth.base_url.clone(), auth.token())
    };
    let mut req = reqwest::Client::new()
        .get(format!("{base_url}/api/operator-feed?limit={FEED_LIMIT}"))
        .timeout(Duration::from_secs(4));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let page: FeedPage = res.json().await.ok()?;
    Some(page.items)
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
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
        loop {
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;

            let Some(feed) = fetch_feed(&app).await else {
                continue;
            };
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
                // app was opened.
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
        }
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
