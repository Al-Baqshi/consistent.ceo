//! Short Memory Buffer: stateful, persona-aware conversation context.
//!
//! Provides a lightweight, high-speed cache for per-session context so the
//! Interpreter (and later the OS) can "stay in character" without reloading
//! the entire project history on every turn.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;

/// Static persona snapshot — stays constant during a session unless the OS updates it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntitySnapshot {
    pub entity_id: String,
    pub name: String,
    /// "You are Consistent, the decisive CEO…"
    pub persona_narrative: String,
    /// "Building the coffee subscription landing page."
    pub current_goal: String,
    /// ["Professional", "Witty", "Direct"]
    pub tone_constraints: Vec<String>,
}

impl Default for EntitySnapshot {
    fn default() -> Self {
        Self {
            entity_id: String::new(),
            name: "Default".to_string(),
            persona_narrative: "You are a helpful assistant.".to_string(),
            current_goal: "Assist the user.".to_string(),
            tone_constraints: vec!["Professional".to_string()],
        }
    }
}

/// A single compressed exchange in the sliding window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTurn {
    /// "user" or "assistant"
    pub role: String,
    /// Compressed / cleaned text
    pub content: String,
    pub timestamp: i64,
}

/// Per-session short-term memory.
pub struct ShortMemoryBuffer {
    pub snapshot: EntitySnapshot,
    pub window: VecDeque<ConversationTurn>,
    pub max_turns: usize,
}

impl ShortMemoryBuffer {
    /// Start an empty window for a session, bounded to `max_turns` turns.
    pub fn new(snapshot: EntitySnapshot, max_turns: usize) -> Self {
        Self {
            snapshot,
            window: VecDeque::new(),
            max_turns,
        }
    }

    /// Push a new turn, evicting the oldest if over capacity.
    pub fn push_turn(&mut self, role: &str, content: &str) {
        let turn = ConversationTurn {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now().timestamp(),
        };
        if self.window.len() >= self.max_turns {
            self.window.pop_front();
        }
        self.window.push_back(turn);
    }

    /// Build a system-prompt fragment the Interpreter can prepend.
    pub fn to_interpreter_prompt(&self) -> String {
        let history: Vec<String> = self
            .window
            .iter()
            .map(|t| format!("{}: {}", t.role, t.content))
            .collect();
        let history_block = if history.is_empty() {
            "(no recent history)".to_string()
        } else {
            history.join("\n")
        };
        format!(
            "Identity: {}\nGoal: {}\nTone: {}\nRecent History:\n{}",
            self.snapshot.persona_narrative,
            self.snapshot.current_goal,
            self.snapshot.tone_constraints.join(", "),
            history_block,
        )
    }

    /// Update the static snapshot (called by the OS on major completions).
    pub fn update_snapshot(&mut self, snapshot: EntitySnapshot) {
        self.snapshot = snapshot;
    }
}

/// Thread-safe session cache.
pub type SessionCache = Arc<DashMap<String, ShortMemoryBuffer>>;

/// Create a new empty session cache.
pub fn create_session_cache() -> SessionCache {
    Arc::new(DashMap::new())
}

/// Get or create a buffer for the given session.
pub fn ensure_session(
    cache: &SessionCache,
    session_id: &str,
    snapshot: EntitySnapshot,
    max_turns: usize,
) {
    if !cache.contains_key(session_id) {
        cache.insert(
            session_id.to_string(),
            ShortMemoryBuffer::new(snapshot, max_turns),
        );
    }
}

/// Detect intent flags in interpreter output.
#[derive(Debug, Clone)]
pub enum IntentFlag {
    CommitTask,
    UpdateGoal,
    AskClarification,
    None,
}

/// Parse intent flags from interpreter output.
pub fn detect_intent_flag(output: &str) -> IntentFlag {
    if output.contains("[COMMIT_TASK]") {
        IntentFlag::CommitTask
    } else if output.contains("[UPDATE_GOAL]") {
        IntentFlag::UpdateGoal
    } else if output.contains("[ASK_CLARIFICATION]") {
        IntentFlag::AskClarification
    } else {
        IntentFlag::None
    }
}

/// Extract clean text after stripping intent flags.
pub fn strip_intent_flags(output: &str) -> String {
    output
        .replace("[COMMIT_TASK]", "")
        .replace("[UPDATE_GOAL]", "")
        .replace("[ASK_CLARIFICATION]", "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> EntitySnapshot {
        EntitySnapshot {
            entity_id: "e1".into(),
            name: "Consistent".into(),
            persona_narrative: "You are Consistent.".into(),
            current_goal: "Ship the landing page.".into(),
            tone_constraints: vec!["Direct".into()],
        }
    }

    #[test]
    fn sliding_window_evicts_oldest() {
        let mut buf = ShortMemoryBuffer::new(snap(), 3);
        buf.push_turn("user", "one");
        buf.push_turn("assistant", "two");
        buf.push_turn("user", "three");
        buf.push_turn("assistant", "four");
        assert_eq!(buf.window.len(), 3);
        assert_eq!(buf.window.front().unwrap().content, "two");
        assert_eq!(buf.window.back().unwrap().content, "four");
    }

    #[test]
    fn interpreter_prompt_includes_identity_goal_and_history() {
        let mut buf = ShortMemoryBuffer::new(snap(), 5);
        buf.push_turn("user", "hello");
        let prompt = buf.to_interpreter_prompt();
        assert!(prompt.contains("Consistent"));
        assert!(prompt.contains("Ship the landing page."));
        assert!(prompt.contains("Direct"));
        assert!(prompt.contains("user: hello"));
    }

    #[test]
    fn empty_history_renders_placeholder() {
        let buf = ShortMemoryBuffer::new(snap(), 5);
        assert!(buf.to_interpreter_prompt().contains("(no recent history)"));
    }

    #[test]
    fn session_cache_is_per_session() {
        let cache = create_session_cache();
        ensure_session(&cache, "s1", snap(), 4);
        ensure_session(&cache, "s2", snap(), 4);
        cache.get_mut("s1").unwrap().push_turn("user", "s1-msg");
        assert_eq!(cache.get("s1").unwrap().window.len(), 1);
        assert_eq!(cache.get("s2").unwrap().window.len(), 0);
    }

    #[test]
    fn intent_flag_detection() {
        assert!(matches!(
            detect_intent_flag("[COMMIT_TASK] build it"),
            IntentFlag::CommitTask
        ));
        assert!(matches!(
            detect_intent_flag("[UPDATE_GOAL] new plan"),
            IntentFlag::UpdateGoal
        ));
        assert!(matches!(
            detect_intent_flag("[ASK_CLARIFICATION] which one?"),
            IntentFlag::AskClarification
        ));
        assert!(matches!(detect_intent_flag("plain text"), IntentFlag::None));
    }

    #[test]
    fn strip_intent_flags_leaves_clean_text() {
        assert_eq!(strip_intent_flags("  [COMMIT_TASK] build it  "), "build it");
    }
}
