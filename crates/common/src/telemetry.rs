//! Tracing setup, and LLM cost/latency tracking used to build the
//! GET /api/metrics snapshot.

use std::collections::HashMap;
use tracing_subscriber::{EnvFilter, fmt};

/// Initialize the global `tracing` subscriber. Honors `RUST_LOG` if set,
/// otherwise falls back to `log_level`.
pub fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .init();
}

// Cap on stored per-model latency samples; older samples are overwritten
// round-robin once the cap is hit (see `record` below).
const MAX_LATENCY_SAMPLES: usize = 10_000;

/// Running token/cost/latency totals for a single model.
#[derive(Debug, Default, Clone)]
pub struct ModelBreakdown {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_count: u64,
    /// Latency samples in ms for this model (capped).
    pub latency_ms_samples: Vec<u64>,
}

/// Running token/cost/request totals for a single agent.
#[derive(Debug, Default, Clone)]
pub struct AgentBreakdown {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_count: u64,
}

/// Accumulates usage/cost across all LLM calls, broken down by model and by agent.
#[derive(Debug, Default)]
pub struct CostTracker {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub request_count: u64,
    pub per_model: HashMap<String, ModelBreakdown>,
    pub per_agent: HashMap<String, AgentBreakdown>,
}

impl CostTracker {
    /// Record one completed LLM call: updates the running totals plus the
    /// per-model and per-agent breakdowns.
    // TODO: collapse the token/cost params into a usage struct.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        agent_id: &str,
        _session_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
        latency_ms: u64,
    ) {
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cost_usd += cost_usd;
        self.request_count += 1;

        let model_ent = self.per_model.entry(model.to_string()).or_default();
        model_ent.input_tokens += input_tokens;
        model_ent.output_tokens += output_tokens;
        model_ent.cost_usd += cost_usd;
        model_ent.request_count += 1;
        // Below the cap, grow the sample list; once capped, overwrite samples
        // round-robin so we keep bounded memory while staying roughly representative.
        if model_ent.latency_ms_samples.len() < MAX_LATENCY_SAMPLES {
            model_ent.latency_ms_samples.push(latency_ms);
        } else {
            let i = self.request_count as usize % MAX_LATENCY_SAMPLES;
            model_ent.latency_ms_samples[i] = latency_ms;
        }

        let agent_ent = self.per_agent.entry(agent_id.to_string()).or_default();
        agent_ent.input_tokens += input_tokens;
        agent_ent.output_tokens += output_tokens;
        agent_ent.cost_usd += cost_usd;
        agent_ent.request_count += 1;
    }

    /// p50 and p95 latency in ms for a sorted slice. Returns (p50, p95).
    fn percentiles(samples: &[u64]) -> (u64, u64) {
        if samples.is_empty() {
            return (0, 0);
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        let p50 = sorted[(n * 50) / 100];
        let p95 = sorted[(n * 95) / 100];
        (p50, p95)
    }
}

/// JSON-serializable metrics for GET /api/metrics.
#[derive(serde::Serialize)]
pub struct MetricsSnapshot {
    pub total_requests: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    pub per_model: HashMap<String, ModelMetricsJson>,
    pub per_agent: HashMap<String, AgentMetricsJson>,
}

/// Per-model slice of [`MetricsSnapshot`], including computed latency percentiles.
#[derive(serde::Serialize)]
pub struct ModelMetricsJson {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_count: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
}

/// Per-agent slice of [`MetricsSnapshot`].
#[derive(serde::Serialize)]
pub struct AgentMetricsJson {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub request_count: u64,
}

impl CostTracker {
    /// Build a JSON-serializable snapshot of all totals, computing latency
    /// percentiles per model on the fly.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let per_model = self
            .per_model
            .iter()
            .map(|(k, v)| {
                let (p50, p95) = Self::percentiles(&v.latency_ms_samples);
                (
                    k.clone(),
                    ModelMetricsJson {
                        input_tokens: v.input_tokens,
                        output_tokens: v.output_tokens,
                        cost_usd: v.cost_usd,
                        request_count: v.request_count,
                        latency_p50_ms: p50,
                        latency_p95_ms: p95,
                    },
                )
            })
            .collect();
        let per_agent = self
            .per_agent
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    AgentMetricsJson {
                        input_tokens: v.input_tokens,
                        output_tokens: v.output_tokens,
                        cost_usd: v.cost_usd,
                        request_count: v.request_count,
                    },
                )
            })
            .collect();
        MetricsSnapshot {
            total_requests: self.request_count,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_cost_usd: self.total_cost_usd,
            per_model,
            per_agent,
        }
    }
}
