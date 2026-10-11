//! Keeps session history within a model's context window by collapsing
//! older messages into a single summary once a token threshold is crossed.

use anyhow::Result;
use std::future::Future;

/// How aggressively to compact history. `Safeguard` and `Aggressive` behave
/// the same today (both keep the most recent messages and summarize the
/// rest); the distinction is for future tuning. `Off` disables compaction.
pub enum CompactionMode {
    Safeguard,
    Aggressive,
    Off,
}

/// Decides when history needs compacting and performs the (synchronous,
/// non-LLM) placeholder summary; see [`compact_messages_to_summary`] for the
/// LLM-backed version callers should use to persist a real summary.
pub struct Compactor {
    pub mode: CompactionMode,
    pub max_context_tokens: usize,
}

impl Compactor {
    pub fn new(mode: CompactionMode, max_context_tokens: usize) -> Self {
        Self {
            mode,
            max_context_tokens,
        }
    }

    /// Whether `current_tokens` exceeds the configured budget (always false when `Off`).
    pub fn should_compact(&self, current_tokens: usize) -> bool {
        match self.mode {
            CompactionMode::Off => false,
            _ => current_tokens > self.max_context_tokens,
        }
    }

    /// Replace older messages with a single placeholder summary line, keeping
    /// the most recent `PRESERVE` messages verbatim. Callers that want a real
    /// LLM-generated summary should use [`compact_messages_to_summary`] instead.
    pub fn compact(&self, messages: &[String]) -> Result<Vec<String>> {
        match self.mode {
            CompactionMode::Off => Ok(messages.to_vec()),
            CompactionMode::Safeguard | CompactionMode::Aggressive => {
                // Number of most-recent messages kept verbatim; the rest collapse to one summary line.
                const PRESERVE: usize = 10;
                if messages.len() <= PRESERVE {
                    return Ok(messages.to_vec());
                }
                let split = messages.len() - PRESERVE;
                let mut compacted = Vec::with_capacity(PRESERVE + 1);
                let summary = format!(
                    "[summary] compacted {} earlier messages to fit context window",
                    split
                );
                compacted.push(summary);
                compacted.extend_from_slice(&messages[split..]);
                Ok(compacted)
            }
        }
    }
}

/// Produce a single summary string from older messages using an async summarizer (e.g. LLM).
/// Caller should then save_memory(agent_id, &summary, Some(&embedding)) to persist with embedding.
pub async fn compact_messages_to_summary<F, Fut>(
    messages: &[String],
    summarizer: F,
) -> Result<String>
where
    F: FnOnce(&[String]) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    if messages.is_empty() {
        return Ok(String::new());
    }
    summarizer(messages).await
}
