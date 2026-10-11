//! Per-model token pricing, used to estimate the USD cost of a completion
//! from its `TokenUsage` so directors/agents can be budget-aware.

use std::collections::HashMap;

/// USD price per 1M tokens for one model.
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

/// Looks up a model's price and turns token counts into a dollar cost.
pub struct CostCalculator {
    pricing: HashMap<String, ModelPricing>,
}

impl Default for CostCalculator {
    fn default() -> Self {
        Self::new()
    }
}

impl CostCalculator {
    pub fn new() -> Self {
        let mut pricing = HashMap::new();
        // Anthropic (USD per 1M tokens, public list price as of 2026-04).
        pricing.insert(
            "claude-opus-4".to_string(),
            ModelPricing {
                input_per_million: 15.0,
                output_per_million: 75.0,
            },
        );
        pricing.insert(
            "claude-sonnet-4".to_string(),
            ModelPricing {
                input_per_million: 3.0,
                output_per_million: 15.0,
            },
        );
        pricing.insert(
            "claude-sonnet-4-6".to_string(),
            ModelPricing {
                input_per_million: 3.0,
                output_per_million: 15.0,
            },
        );
        pricing.insert(
            "claude-haiku-4-5".to_string(),
            ModelPricing {
                input_per_million: 0.80,
                output_per_million: 4.0,
            },
        );
        // OpenAI
        pricing.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_million: 2.50,
                output_per_million: 10.0,
            },
        );
        pricing.insert(
            "gpt-4o-mini".to_string(),
            ModelPricing {
                input_per_million: 0.15,
                output_per_million: 0.60,
            },
        );
        // Google Gemini
        pricing.insert(
            "gemini-2.0-flash-exp".to_string(),
            ModelPricing {
                input_per_million: 0.10,
                output_per_million: 0.40,
            },
        );
        pricing.insert(
            "gemini-2.5-flash".to_string(),
            ModelPricing {
                input_per_million: 0.30,
                output_per_million: 2.50,
            },
        );
        pricing.insert(
            "gemini-2.5-pro".to_string(),
            ModelPricing {
                input_per_million: 1.25,
                output_per_million: 10.0,
            },
        );
        Self { pricing }
    }

    /// Look up model pricing. Matches exact, `provider/model` suffix, and
    /// prefix (so `claude-sonnet-4-20260101` resolves to `claude-sonnet-4`).
    fn lookup(&self, model: &str) -> Option<&ModelPricing> {
        let key = model.rsplit_once('/').map(|(_, m)| m).unwrap_or(model);
        if let Some(p) = self.pricing.get(key) {
            return Some(p);
        }
        self.pricing
            .iter()
            .filter(|(name, _)| key.starts_with(name.as_str()))
            .max_by_key(|(name, _)| name.len())
            .map(|(_, p)| p)
    }

    /// Estimate the USD cost of a completion; unknown models cost 0.0 rather than erroring.
    pub fn calculate(&self, model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
        match self.lookup(model) {
            Some(p) => {
                (input_tokens as f64 / 1_000_000.0) * p.input_per_million
                    + (output_tokens as f64 / 1_000_000.0) * p.output_per_million
            }
            None => 0.0,
        }
    }
}
