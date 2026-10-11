//! Routes inbound [`Message`]s (chat turns, scheduled task fires) to the
//! matching runtime [`RuntimeAgent`], resolving scope first so the agent
//! knows which entity/mission it is acting on behalf of.

use std::collections::HashMap;
use std::sync::Arc;

use crate::session::resolve_scope_for_message;
use crate::state::ControlState;
use consistent_world_agents::agent::Agent as RuntimeAgent;
use consistent_world_agents::routing::{AgentRouter, RoutingRule};
use consistent_world_common::types::{ChannelKind, Message, Sender};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

// Truncates by character count, not byte offset, so multi-byte UTF-8 (emoji,
// macronised text) is never sliced mid-character.
fn truncate_chars_with_ellipsis(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn scheduled_feed_title(task_description: &str) -> String {
    format!(
        "Scheduled: {}",
        truncate_chars_with_ellipsis(task_description.trim(), 80)
    )
}

fn scheduled_feed_detail(reply: &str) -> Option<String> {
    let reply = reply.trim();
    (!reply.is_empty()).then(|| truncate_chars_with_ellipsis(reply, 400))
}

/// Holds the routing table and the live agent handles it routes to.
/// `Arc`-wrapped so cheap clones can be handed to every connection/task that
/// needs to dispatch a message.
#[derive(Clone)]
pub struct AgentRuntime {
    router: Arc<AgentRouter>,
    agents: Arc<HashMap<String, Arc<RuntimeAgent>>>,
    control_state: Option<Arc<ControlState>>,
}

impl AgentRuntime {
    /// `control_state` is optional so this can run in tests/tools without a
    /// full gateway; when absent, operator-feed side effects are skipped.
    pub fn new(
        router: AgentRouter,
        agents: HashMap<String, Arc<RuntimeAgent>>,
        control_state: Option<Arc<ControlState>>,
    ) -> Self {
        Self {
            router: Arc::new(router),
            agents: Arc::new(agents),
            control_state,
        }
    }

    /// Resolves scope, picks the matching agent via the routing table, and
    /// hands the message off (or resets the session on `/reset`).
    pub async fn handle_inbound_message(&self, mut message: Message) {
        let chat_bound_entity_id = if let Some(ref ctl) = self.control_state {
            let entities = ctl.entities.read().await;
            crate::entity_runtime::entity_id_for_bound_chat_session(&entities, &message.session_id)
        } else {
            None
        };
        let scope_resolution = resolve_scope_for_message(
            &message.channel,
            &message.sender.id,
            Some(message.content.as_str()),
            Some(&message.metadata),
            chat_bound_entity_id.as_deref(),
        );
        message.metadata["scope"] = json!({
            "scope_type": scope_resolution.scope_type,
            "scope_id": scope_resolution.scope_id,
            "resolution_reason": scope_resolution.resolution_reason,
            "channel_binding_metadata": scope_resolution.channel_binding_metadata
        });

        let agent_id = self.router.resolve(&message).to_string();
        let Some(agent) = self.agents.get(&agent_id).cloned() else {
            warn!(agent_id = %agent_id, session_id = %message.session_id, "message router: no matching agent");
            return;
        };

        if message.content.trim() == "/reset" {
            match agent.reset_session(&message.session_id).await {
                Ok(()) => {
                    info!(session_id = %message.session_id, "Session reset.");
                }
                Err(e) => {
                    warn!(session_id = %message.session_id, error = %e, "message router: reset failed");
                }
            }
            return;
        }

        match agent.handle_message(&message).await {
            Ok(reply) => {
                info!(session_id = %message.session_id, "agent reply: {}", reply);
            }
            Err(e) => {
                warn!(session_id = %message.session_id, agent_id = %agent_id, error = %e, "message router: agent failed");
            }
        }
    }

    /// Synthesizes a `Message` for a cron-fired scheduled task and runs it
    /// through the named agent directly (bypassing the router table), then
    /// records the outcome on the operator feed when `control_state` is set.
    pub async fn dispatch_scheduled_task(
        &self,
        agent_id: String,
        task_description: String,
        session_id: String,
        entity_id: Option<String>,
        connection_id: Option<String>,
    ) {
        let Some(agent) = self.agents.get(&agent_id).cloned() else {
            warn!(agent_id = %agent_id, session_id = %session_id, "scheduler: no such agent");
            return;
        };

        let scope = if let Some(ref eid) = entity_id {
            serde_json::json!({
                "scope_type": "entity",
                "scope_id": eid,
                "connection_id": connection_id,
            })
        } else {
            serde_json::json!({
                "scope_type": "principal",
                "scope_id": format!("schedule:{}", session_id),
            })
        };

        let feed_title = scheduled_feed_title(&task_description);

        let message = Message {
            id: Uuid::new_v4(),
            session_id: session_id.clone(),
            channel: ChannelKind::Web,
            sender: Sender {
                id: "scheduler".to_string(),
                display_name: Some("Scheduler".to_string()),
            },
            content: task_description,
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({
                "source": "scheduler",
                "agent_id": agent_id,
                "scope": scope,
            }),
        };
        match agent.handle_message(&message).await {
            Ok(reply) => {
                info!(session_id = %session_id, "scheduler task completed: {}", reply);
                if let Some(ref ctl) = self.control_state {
                    let detail = scheduled_feed_detail(&reply);
                    ctl.push_scheduled_fire_feed(feed_title, entity_id, connection_id, detail)
                        .await;
                }
            }
            Err(e) => {
                warn!(session_id = %session_id, error = %e, "scheduler task failed");
                if let Some(ref ctl) = self.control_state {
                    ctl.push_scheduled_fire_feed(
                        feed_title,
                        entity_id,
                        connection_id,
                        Some(format!("Failed: {e}")),
                    )
                    .await;
                }
            }
        }
    }
}

/// Drains an inbound-message channel and feeds each one through an
/// [`AgentRuntime`]. Intended to be spawned as a long-running task.
pub struct MessageRouter {
    rx: mpsc::Receiver<Message>,
    runtime: AgentRuntime,
}

impl MessageRouter {
    pub fn new(rx: mpsc::Receiver<Message>, runtime: AgentRuntime) -> Self {
        Self { rx, runtime }
    }

    /// Loops until the sender side of `rx` is dropped.
    pub async fn run(&mut self) {
        while let Some(message) = self.rx.recv().await {
            self.runtime.handle_inbound_message(message).await;
        }
    }
}

/// Builds the routing table from config entries, falling back to
/// `default_agent` when no rule matches a message's channel/peer.
pub fn build_agent_router(
    rules: &[crate::config::RoutingRuleEntry],
    default_agent: String,
) -> AgentRouter {
    let mut router = AgentRouter::new(default_agent);
    for rule in rules {
        let channel = rule
            .channel
            .as_deref()
            .and_then(|s| s.parse::<ChannelKind>().ok());
        router.add_rule(RoutingRule {
            channel,
            peer_pattern: rule.peer.clone(),
            agent_id: rule.agent_id.clone(),
        });
    }
    router
}

#[cfg(test)]
mod tests {
    use super::{scheduled_feed_detail, scheduled_feed_title};

    #[test]
    fn scheduled_title_truncation_is_char_safe_for_maori_text() {
        // Byte 80 falls inside one of these two-byte characters, which used to
        // panic when the scheduler sliced the UTF-8 string directly.
        let description = format!("{}🥝 anamata", "ā".repeat(79));
        let title = scheduled_feed_title(&description);

        assert!(title.starts_with("Scheduled: "));
        assert!(title.ends_with('…'));
        assert_eq!(title.trim_start_matches("Scheduled: ").chars().count(), 81);
        assert!(title.contains('🥝'));
    }

    #[test]
    fn scheduled_reply_truncation_is_char_safe_for_emoji() {
        // Byte 400 lands inside the compass emoji in the old implementation.
        let reply = format!("{}🧭 completed", "a".repeat(399));
        let detail = scheduled_feed_detail(&reply).expect("non-empty reply");

        assert!(detail.ends_with('…'));
        assert_eq!(detail.chars().count(), 401);
        assert!(detail.contains('🧭'));
    }
}
