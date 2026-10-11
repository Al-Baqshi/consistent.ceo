//! Companion notifications: one pipeline from the operator feed to every screen.
//!
//! The operator feed is already the spine — reminders fire into it
//! ([`crate::reminders`]), approvals and mission events push into it
//! ([`crate::state::ControlState::push_approval_feed`] and friends), and the
//! web shell polls it. This module makes it *reach* the CEO instead of waiting
//! to be looked at.
//!
//! Shape: [`crate::state::ControlState::append_operator_feed_event`] drops a
//! [`NotificationSignal`] into an unbounded channel; [`NotificationDispatcher`]
//! owns the receiving half and runs as one background task. Feed writes never
//! block on delivery, and a dead dispatcher degrades to "feed still works,
//! nothing gets pushed".
//!
//! Two deliberate restraints:
//!
//! - **Kind allowlist** ([`is_notifiable`]). The feed carries ~30 event kinds,
//!   most of which are timeline detail, not interruptions. Only the ones a CEO
//!   would want to be interrupted for leave the machine.
//! - **Push is opt-in.** `[notifications] mobile_push` defaults to `false`, so
//!   out of the box nothing transits a third party; devices foreground-poll
//!   `/api/operator-feed` instead. Flipping it on routes titles/bodies through
//!   Expo's push service, which is stated plainly in the settings copy.

use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::device_store::SurrealDeviceStore;
use crate::state::{OperatorFeedEvent, OperatorFeedKind, QuietHoursLocal};
use crate::webhooks::SurrealWebhookStore;

/// Expo's push endpoint. Batched, max 100 messages per request.
const EXPO_PUSH_URL: &str = "https://exp.host/--/api/v2/push/send";
const EXPO_BATCH_MAX: usize = 100;
const PUSH_TIMEOUT_SECS: u64 = 15;
/// Per-target budget for an outbound webhook POST.
const WEBHOOK_TIMEOUT_SECS: u64 = 10;
/// Notification bodies are a glance, not a document.
const BODY_MAX_CHARS: usize = 180;

/// A feed event paired with the workspace it belongs to. The feed event itself
/// carries only an optional `entity_id`, so the workspace is resolved once at
/// append time rather than re-derived per device.
#[derive(Debug, Clone)]
pub struct NotificationSignal {
    pub event: OperatorFeedEvent,
    pub workspace_id: String,
}

pub type NotificationSender = mpsc::UnboundedSender<NotificationSignal>;

/// TOML `[notifications]`.
#[derive(Debug, Clone, Default)]
pub struct NotificationConfig {
    /// Route pushes through Expo. Default `false` — poll-only, nothing leaves
    /// the operator's machines.
    pub mobile_push: bool,
}

/// Whether `now_local` falls inside an entity's quiet window.
///
/// [`QuietHoursLocal`] stores no timezone, so both ends are read as *server*
/// local wall-clock — the gateway runs on the operator's own machine, and
/// inventing a timezone here would silently disagree with whatever the settings
/// UI showed when the window was set.
///
/// Overnight windows (22:00–07:00) are the common case, so a window whose end
/// is before its start wraps midnight rather than being empty. Unparseable
/// values mean "no quiet hours": a malformed string must never mute the
/// companion permanently.
pub fn quiet_hours_active(q: &QuietHoursLocal, now_local: chrono::NaiveTime) -> bool {
    let (Some(start), Some(end)) = (parse_hhmm(&q.start), parse_hhmm(&q.end)) else {
        return false;
    };
    if start == end {
        // Zero-width window. Treated as "off" rather than "always on" — the
        // failure mode of a permanently silent companion is the worse one.
        return false;
    }
    if start < end {
        now_local >= start && now_local < end
    } else {
        now_local >= start || now_local < end
    }
}

fn parse_hhmm(raw: &str) -> Option<chrono::NaiveTime> {
    let s = raw.trim();
    chrono::NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| chrono::NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

#[cfg(test)]
mod quiet_hours_tests {
    use super::*;

    fn at(h: u32, m: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn window(start: &str, end: &str) -> QuietHoursLocal {
        QuietHoursLocal {
            start: start.to_string(),
            end: end.to_string(),
        }
    }

    #[test]
    fn overnight_window_wraps_midnight() {
        let q = window("22:00", "07:00");
        assert!(quiet_hours_active(&q, at(23, 30)), "before midnight");
        assert!(quiet_hours_active(&q, at(2, 0)), "after midnight");
        assert!(quiet_hours_active(&q, at(22, 0)), "start is inclusive");
        assert!(!quiet_hours_active(&q, at(7, 0)), "end is exclusive");
        assert!(!quiet_hours_active(&q, at(12, 0)), "midday is not quiet");
    }

    #[test]
    fn same_day_window_is_a_plain_range() {
        let q = window("13:00", "14:30");
        assert!(quiet_hours_active(&q, at(13, 45)));
        assert!(!quiet_hours_active(&q, at(12, 59)));
        assert!(!quiet_hours_active(&q, at(14, 30)));
    }

    #[test]
    fn malformed_or_empty_window_never_mutes() {
        assert!(!quiet_hours_active(
            &window("not a time", "07:00"),
            at(2, 0)
        ));
        assert!(!quiet_hours_active(&window("22:00", ""), at(23, 0)));
        assert!(!quiet_hours_active(&window("09:00", "09:00"), at(9, 0)));
    }
}

/// Which feed kinds are worth interrupting a human for.
///
/// Everything else stays feed-only: still visible in the activity timeline and
/// the web shell, just not pushed. Keep this list short on purpose — a
/// companion that pings for every task transition gets muted, and a muted
/// companion is worse than none.
pub fn is_notifiable(kind: &OperatorFeedKind) -> bool {
    matches!(
        kind,
        // A reminder the CEO explicitly asked for came due.
        OperatorFeedKind::ScheduledTaskFired
            // Something is waiting on a human decision.
            | OperatorFeedKind::ApprovalNeeded
            | OperatorFeedKind::ApprovalRequestCreated
            | OperatorFeedKind::MissionUserInputNeeded
            // A director's beat produced something worth reading.
            | OperatorFeedKind::HeartbeatResult
            // Long-running work on the operator's machine finished or died.
            | OperatorFeedKind::AgentRunCompleted
            | OperatorFeedKind::AgentRunFailed
            // Autonomous work stopped making progress. A blocked task is the
            // state that most needs a human — silence here means the failure
            // sits invisible until someone happens to open the feed.
            | OperatorFeedKind::MissionStalled
            | OperatorFeedKind::TaskStuck
            | OperatorFeedKind::TaskBlocked
            // The goal the operator stated is done.
            | OperatorFeedKind::MissionCompleted
            // A commitment slipped, or the morning brief is ready. These are the
            // only two kinds that exist to push the operator rather than report
            // on the system.
            | OperatorFeedKind::TaskOverdue
            | OperatorFeedKind::DailyDigest
            // Autonomous work is holding because nobody said where it should
            // happen. Silent here means a task queue that never moves.
            | OperatorFeedKind::EntityWorkspaceNeeded
            // A "done" claim the verifier didn't believe. The task is parked
            // on a human verdict; without a ping it parks forever.
            | OperatorFeedKind::VerificationNeedsReview
            // A mission is due its scheduled human review.
            | OperatorFeedKind::MissionReviewDue
            // An integration stopped working. Everything downstream of it is
            // quietly failing until someone reconnects it.
            | OperatorFeedKind::ConnectionCheckFailed
            // A mission's final report exists and is waiting to be read.
            | OperatorFeedKind::MissionReportReady
    )
}

/// Human-facing title for a pushed event.
fn notification_title(event: &OperatorFeedEvent) -> String {
    let prefix = match event.kind {
        OperatorFeedKind::ScheduledTaskFired => "Reminder",
        OperatorFeedKind::ApprovalNeeded | OperatorFeedKind::ApprovalRequestCreated => {
            "Needs your approval"
        }
        OperatorFeedKind::MissionUserInputNeeded => "Needs your input",
        OperatorFeedKind::HeartbeatResult => "Heartbeat",
        OperatorFeedKind::AgentRunCompleted => "Run finished",
        OperatorFeedKind::AgentRunFailed => "Run failed",
        OperatorFeedKind::MissionStalled => "Mission stalled",
        OperatorFeedKind::TaskStuck => "Task stuck",
        OperatorFeedKind::TaskBlocked => "Task blocked",
        OperatorFeedKind::MissionCompleted => "Mission completed",
        OperatorFeedKind::TaskOverdue => "Overdue",
        OperatorFeedKind::DailyDigest => "Your day",
        OperatorFeedKind::EntityWorkspaceNeeded => "Where should this live?",
        OperatorFeedKind::VerificationNeedsReview => "Needs review",
        OperatorFeedKind::MissionReviewDue => "Review due",
        OperatorFeedKind::ConnectionCheckFailed => "Connection failed",
        OperatorFeedKind::MissionReportReady => "Report ready",
        _ => "Consistent CEO",
    };
    format!("{prefix} · {}", truncate(&event.title, 60))
}

// Human-facing body: the detail line when present, else the title itself.
fn notification_body(event: &OperatorFeedEvent) -> String {
    event
        .detail
        .as_deref()
        .map(|d| truncate(d, BODY_MAX_CHARS))
        .unwrap_or_else(|| truncate(&event.title, BODY_MAX_CHARS))
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// One entry in Expo's `data` response array.
#[derive(Debug, Deserialize)]
struct ExpoTicket {
    status: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    details: Option<ExpoTicketDetails>,
}

#[derive(Debug, Deserialize)]
struct ExpoTicketDetails {
    #[serde(default)]
    error: Option<String>,
}

// Top-level shape of Expo's push batch response.
#[derive(Debug, Deserialize)]
struct ExpoResponse {
    #[serde(default)]
    data: Vec<ExpoTicket>,
}

/// Consumes [`NotificationSignal`]s off the channel and fans each one out to
/// push devices and outbound webhooks.
pub struct NotificationDispatcher {
    devices: Arc<SurrealDeviceStore>,
    config: NotificationConfig,
    http: reqwest::Client,
    /// Outbound webhook subscriptions. `None` = the store failed to init (or
    /// this is a test); fan-out silently does nothing, pushes are unaffected.
    webhooks: Option<Arc<SurrealWebhookStore>>,
}

impl NotificationDispatcher {
    /// Builds a dispatcher; call [`Self::spawn`] to start consuming signals.
    pub fn new(
        devices: Arc<SurrealDeviceStore>,
        config: NotificationConfig,
        webhooks: Option<Arc<SurrealWebhookStore>>,
    ) -> Self {
        Self {
            devices,
            config,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(PUSH_TIMEOUT_SECS))
                .build()
                .unwrap_or_default(),
            webhooks,
        }
    }

    /// Create the channel and spawn the consumer. Returns the sender to hand to
    /// [`crate::state::ControlState::attach_notification_sender`].
    pub fn spawn(self) -> NotificationSender {
        let (tx, mut rx) = mpsc::unbounded_channel::<NotificationSignal>();
        tokio::spawn(async move {
            while let Some(signal) = rx.recv().await {
                self.handle(signal).await;
            }
            tracing::warn!("notification dispatcher channel closed; pushes disabled");
        });
        tx
    }

    /// One signal, two deliveries: the operator's own devices (Expo push) and
    /// any registered outbound webhooks. Both are best-effort and log their own
    /// failures. Push runs first on purpose — the operator's own notification
    /// never waits on a third party's endpoint (a slow subscriber can only delay
    /// the *next* event, bounded by [`WEBHOOK_TIMEOUT_SECS`] per target).
    async fn handle(&self, signal: NotificationSignal) {
        self.deliver_push(&signal).await;
        self.deliver_webhooks(&signal).await;
    }

    /// Fan out to this workspace's webhook subscriptions. Best-effort, 10s per
    /// target, failures logged and swallowed — same posture as
    /// [`crate::alerts::notify_failure`]. A subscriber that is down must never
    /// turn into a gateway error.
    async fn deliver_webhooks(&self, signal: &NotificationSignal) {
        let Some(store) = self.webhooks.as_ref() else {
            return;
        };
        let subs = match store.list_by_workspace(&signal.workspace_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "webhook fan-out: subscription lookup failed");
                return;
            }
        };
        let targets: Vec<_> = subs
            .into_iter()
            .filter(|s| crate::webhooks::subscription_matches(&s.kinds, &signal.event.kind))
            .collect();
        if targets.is_empty() {
            return;
        }
        let payload = serde_json::json!({
            "event": signal.event,
            "workspaceId": signal.workspace_id,
        });
        for sub in targets {
            let result = self
                .http
                .post(&sub.url)
                .timeout(std::time::Duration::from_secs(WEBHOOK_TIMEOUT_SECS))
                .json(&payload)
                .send()
                .await;
            match result {
                Ok(resp) if !resp.status().is_success() => {
                    tracing::warn!(
                        webhook = %sub.id,
                        status = %resp.status(),
                        "webhook delivery returned non-2xx"
                    );
                }
                Err(e) => {
                    tracing::warn!(webhook = %sub.id, error = %e, "webhook delivery failed");
                }
                Ok(_) => {
                    tracing::debug!(webhook = %sub.id, kind = ?signal.event.kind, "webhook delivered");
                }
            }
        }
    }

    async fn deliver_push(&self, signal: &NotificationSignal) {
        if !is_notifiable(&signal.event.kind) {
            return;
        }
        if !self.config.mobile_push {
            // Poll-only posture: devices pick this up from /api/operator-feed.
            tracing::debug!(
                kind = ?signal.event.kind,
                "notifiable event; mobile push disabled, leaving to feed poll"
            );
            return;
        }

        let devices = match self.devices.list_pushable(&signal.workspace_id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "notification dispatch: device lookup failed");
                return;
            }
        };
        if devices.is_empty() {
            return;
        }

        // Don't re-push an event a device has already been told about. The
        // cursor is the event id we last delivered; equality means this exact
        // event already went out (a retry after a partial batch, say).
        let fresh: Vec<_> = devices
            .into_iter()
            .filter(|d| d.last_seen_feed_event_id.as_deref() != Some(signal.event.id.as_str()))
            .collect();
        if fresh.is_empty() {
            return;
        }

        let title = notification_title(&signal.event);
        let body = notification_body(&signal.event);
        // `feedEventId` is the de-dupe key: the phone's foreground feed poll
        // raises its own banner for the same event, and drops it when a push
        // with this id has already been shown (`mobile/src/lib/companion.ts`).
        let data = serde_json::json!({
            "feedEventId": signal.event.id,
            "kind": signal.event.kind,
            "entityId": signal.event.entity_id,
            "taskId": signal.event.task_id,
            "missionId": signal.event.mission_id,
            "approvalId": signal.event.approval_id,
        });

        for chunk in fresh.chunks(EXPO_BATCH_MAX) {
            let messages: Vec<serde_json::Value> = chunk
                .iter()
                .filter_map(|d| d.push_token.as_deref())
                .map(|token| {
                    serde_json::json!({
                        "to": token,
                        "title": title,
                        "body": body,
                        "sound": "default",
                        "data": data,
                    })
                })
                .collect();
            if messages.is_empty() {
                continue;
            }

            let tickets = match self.send_batch(&messages).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "expo push failed");
                    continue;
                }
            };

            // Expo returns tickets positionally, so zip against the same slice
            // the messages were built from.
            for (device, ticket) in chunk.iter().zip(tickets) {
                if ticket.status == "ok" {
                    if let Err(e) = self.devices.set_cursor(&device.id, &signal.event.id).await {
                        tracing::warn!(error = %e, device = %device.id, "cursor update failed");
                    }
                    continue;
                }
                let reason = ticket
                    .details
                    .as_ref()
                    .and_then(|d| d.error.as_deref())
                    .or(ticket.message.as_deref())
                    .unwrap_or("unknown");
                if reason == "DeviceNotRegistered" {
                    tracing::info!(device = %device.id, "expo token dead; disabling device");
                    if let Err(e) = self.devices.disable_token(&device.id).await {
                        tracing::warn!(error = %e, device = %device.id, "disable token failed");
                    }
                } else {
                    tracing::warn!(device = %device.id, reason = %reason, "expo push rejected");
                }
            }
        }
    }

    // POSTs one batch (<= EXPO_BATCH_MAX) to Expo and returns its per-message tickets.
    async fn send_batch(&self, messages: &[serde_json::Value]) -> anyhow::Result<Vec<ExpoTicket>> {
        let res = self
            .http
            .post(EXPO_PUSH_URL)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .json(messages)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("expo push HTTP {}: {}", status, truncate(&body, 300));
        }
        let parsed: ExpoResponse = res.json().await?;
        Ok(parsed.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: OperatorFeedKind, title: &str, detail: Option<&str>) -> OperatorFeedEvent {
        OperatorFeedEvent {
            id: "evt-1".to_string(),
            timestamp: "2026-07-28T09:00:00Z".to_string(),
            kind,
            task_id: None,
            mission_id: None,
            entity_id: None,
            connection_id: None,
            title: title.to_string(),
            detail: detail.map(str::to_string),
            approval_id: None,
        }
    }

    #[test]
    fn reminders_and_approvals_are_notifiable() {
        assert!(is_notifiable(&OperatorFeedKind::ScheduledTaskFired));
        assert!(is_notifiable(&OperatorFeedKind::ApprovalRequestCreated));
        assert!(is_notifiable(&OperatorFeedKind::HeartbeatResult));
        assert!(is_notifiable(&OperatorFeedKind::AgentRunCompleted));
    }

    #[test]
    fn human_needed_kinds_are_notifiable() {
        // Each of these parks work on a person. They used to stay feed-only,
        // so nothing ever told the person.
        assert!(is_notifiable(&OperatorFeedKind::VerificationNeedsReview));
        assert!(is_notifiable(&OperatorFeedKind::MissionReviewDue));
        assert!(is_notifiable(&OperatorFeedKind::ConnectionCheckFailed));
        assert!(is_notifiable(&OperatorFeedKind::MissionReportReady));
    }

    #[test]
    fn human_needed_kinds_get_their_own_title() {
        let t = notification_title(&event(
            OperatorFeedKind::VerificationNeedsReview,
            "Ship the pricing page",
            None,
        ));
        assert_eq!(t, "Needs review · Ship the pricing page");
    }

    #[test]
    fn timeline_detail_is_not_notifiable() {
        assert!(!is_notifiable(&OperatorFeedKind::TaskCreated));
        assert!(!is_notifiable(&OperatorFeedKind::GoalLoopDecision));
        assert!(!is_notifiable(&OperatorFeedKind::MissionFollowupSuppressed));
    }

    #[test]
    fn title_carries_the_kind_then_the_subject() {
        let t = notification_title(&event(
            OperatorFeedKind::ScheduledTaskFired,
            "Call the investor",
            None,
        ));
        assert_eq!(t, "Reminder · Call the investor");
    }

    #[test]
    fn body_prefers_detail_and_truncates() {
        let long = "x".repeat(400);
        let b = notification_body(&event(
            OperatorFeedKind::HeartbeatResult,
            "Daily pulse",
            Some(&long),
        ));
        assert_eq!(b.chars().count(), BODY_MAX_CHARS);
        assert!(b.ends_with('…'));
    }

    #[test]
    fn body_falls_back_to_title_when_no_detail() {
        let b = notification_body(&event(
            OperatorFeedKind::AgentRunFailed,
            "Refactor auth",
            None,
        ));
        assert_eq!(b, "Refactor auth");
    }

    #[test]
    fn truncate_is_char_safe_on_multibyte() {
        // Byte-slicing here would panic; chars() keeps it safe.
        let s = "é".repeat(50);
        let out = truncate(&s, 10);
        assert_eq!(out.chars().count(), 10);
    }
}
