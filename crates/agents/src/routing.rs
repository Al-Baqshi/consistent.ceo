//! Routes an inbound [`Message`] to the agent id that should handle it,
//! based on ordered channel/peer-pattern rules with a default fallback.

use consistent_world_common::types::{ChannelKind, Message};

/// One routing rule: matches optionally on channel and/or a glob pattern
/// over the sender's peer id, and maps to a target agent id.
pub struct RoutingRule {
    pub channel: Option<ChannelKind>,
    pub peer_pattern: Option<String>,
    pub agent_id: String,
}

/// Ordered list of [`RoutingRule`]s plus a default agent for unmatched messages.
pub struct AgentRouter {
    rules: Vec<RoutingRule>,
    default_agent: String,
}

/// Simple glob match: `*` matches any sequence, `?` matches one character.
fn glob_match(pattern: &str, s: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), s.as_bytes())
}

// Standard iterative backtracking glob matcher: walk both strings in lockstep,
// and on a mismatch rewind to the most recent `*` and retry consuming one more
// character of `s` (star_pi/star_si remember that rewind point).
fn glob_match_bytes(pattern: &[u8], s: &[u8]) -> bool {
    let mut pi = 0;
    let mut si = 0;
    let mut star_pi = None;
    let mut star_si = None;
    while si <= s.len() {
        if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = Some(pi);
            star_si = Some(si);
            pi += 1;
            continue;
        }
        if pi == pattern.len() {
            if si == s.len() {
                return true;
            }
        } else if si < s.len() && (pattern[pi] == b'?' || pattern[pi] == s[si]) {
            pi += 1;
            si += 1;
            continue;
        }
        // Mismatch: if we've seen a `*`, backtrack to just after it and let
        // it absorb one more character; otherwise there's no match.
        if let (Some(pp), Some(ss)) = (star_pi, star_si) {
            pi = pp + 1;
            si = ss + 1;
            star_si = Some(si);
            continue;
        }
        return false;
    }
    false
}

impl AgentRouter {
    /// Create a router with no rules; every message resolves to `default_agent`.
    pub fn new(default_agent: String) -> Self {
        Self {
            rules: Vec::new(),
            default_agent,
        }
    }

    /// Append a rule. Rules are tried in insertion order; first match wins.
    pub fn add_rule(&mut self, rule: RoutingRule) {
        self.rules.push(rule);
    }

    /// Resolve message to agent_id: first matching rule, else default.
    pub fn resolve(&self, message: &Message) -> &str {
        let peer_id = &message.sender.id;
        for rule in &self.rules {
            let channel_ok = rule
                .channel
                .as_ref()
                .map(|c| c == &message.channel)
                .unwrap_or(true);
            let peer_ok = rule
                .peer_pattern
                .as_ref()
                .map(|p| glob_match(p, peer_id))
                .unwrap_or(true);
            if channel_ok && peer_ok {
                return &rule.agent_id;
            }
        }
        &self.default_agent
    }
}
