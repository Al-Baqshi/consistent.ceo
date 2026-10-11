//! Tests for `AgentRouter`: default fallback and glob-pattern peer routing.

use consistent_world_agents::routing::{AgentRouter, RoutingRule};
use consistent_world_common::types::{ChannelKind, Message, Sender};

// Builds a minimal Web-channel message from the given peer id.
fn msg(peer_id: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4(),
        session_id: format!("web-{}", peer_id),
        channel: ChannelKind::Web,
        sender: Sender {
            id: peer_id.to_string(),
            display_name: None,
        },
        content: "hi".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: serde_json::json!({}),
    }
}

#[test]
fn test_default_agent_routing() {
    let router = AgentRouter::new("default".to_string());
    assert_eq!(router.resolve(&msg("user1")), "default");
    assert_eq!(router.resolve(&msg("12345")), "default");
}

#[test]
fn test_peer_pattern_routing() {
    let mut router = AgentRouter::new("default".to_string());
    router.add_rule(RoutingRule {
        channel: Some(ChannelKind::Web),
        peer_pattern: Some("admin_*".to_string()),
        agent_id: "admin-agent".to_string(),
    });

    assert_eq!(router.resolve(&msg("admin_42")), "admin-agent");
    assert_eq!(router.resolve(&msg("user_1")), "default");
}
