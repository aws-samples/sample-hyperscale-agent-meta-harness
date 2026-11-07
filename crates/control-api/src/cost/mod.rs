//! Per-run token cost: tokens times the pinned price table (`prices.json`,
//! USD per 1M tokens for input, cache write, cache read and output). `GET /v1/runs/{runId}` shows
//! the run's cost from its turns' engine-reported usage by model and engine; `ham` prints it on the final card.

use harness_protocol::api::v1::{CostLine, RunCost};
use harness_protocol::common::{Engine, Usage};
use harness_protocol::store::{AgentItem, TurnItem};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The pinned price table (Pricing API, us-west-2, 2026-09-24).
pub const PINNED_PRICES: &str = include_str!("prices.json");

static PINNED: LazyLock<PriceTable> = LazyLock::new(|| {
    PriceTable::parse(PINNED_PRICES).unwrap_or_else(|e| panic!("cost/prices.json: {e}"))
});

/// One model's prices in USD per 1M tokens.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPrice {
    pub model_id: String,
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceTable {
    pub version: String,
    pub currency: String,
    pub unit: String,
    pub source: String,
    pub models: Vec<ModelPrice>,
}

impl PriceTable {
    /// A table in `prices.json` form: USD per 1M tokens, one row per model, no negative price.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t: PriceTable = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if t.currency != "USD" || t.unit != "1M tokens" {
            return Err(format!(
                "prices must be USD per 1M tokens, not {} per {}",
                t.currency, t.unit
            ));
        }
        for (i, m) in t.models.iter().enumerate() {
            if [m.input, m.cache_write, m.cache_read, m.output]
                .iter()
                .any(|p| !p.is_finite() || *p < 0.0)
            {
                return Err(format!("{} has a negative or non-finite price", m.model_id));
            }
            if t.models[..i].iter().any(|o| o.model_id == m.model_id) {
                return Err(format!("{} is listed twice", m.model_id));
            }
        }
        Ok(t)
    }

    pub fn price(&self, model_id: &str) -> Option<&ModelPrice> {
        let m = model_of(model_id);
        self.models.iter().find(|p| p.model_id == m)
    }

    /// The USD cost of `usage` on `model_id`; `None` for a model without a row.
    pub fn usd(&self, model_id: &str, usage: &Usage) -> Option<f64> {
        self.price(model_id).map(|p| {
            (usage.input_tokens as f64 * p.input
                + usage.cache_write_input_tokens as f64 * p.cache_write
                + usage.cache_read_input_tokens as f64 * p.cache_read
                + usage.output_tokens as f64 * p.output)
                / 1_000_000.0
        })
    }
}

/// The pinned table every run is priced with.
pub fn pinned() -> &'static PriceTable {
    &PINNED
}

/// The model an ID names: an invocation log may record an inference-profile or foundation-model
/// ARN, whose last path segment is the model ID.
pub fn model_of(id: &str) -> &str {
    if id.starts_with("arn:") {
        id.rsplit('/').next().unwrap_or(id)
    } else {
        id
    }
}

/// USD to the micro-dollar, so sums do not carry float noise.
pub fn round_usd(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

pub fn add_usage(total: &mut Usage, u: &Usage) {
    total.input_tokens += u.input_tokens;
    total.output_tokens += u.output_tokens;
    total.cache_read_input_tokens += u.cache_read_input_tokens;
    total.cache_write_input_tokens += u.cache_write_input_tokens;
}

/// The run's cost from its turns' engine-reported usage, one line per model and engine (the
/// turn's Agent item names both). `None` until a turn reports usage. A turn without usage or
/// without its Agent item counts in `turnsWithoutUsage`; a model without a price row counts its
/// tokens at zero USD and is named in `unpricedModels`.
pub fn run_cost(agents: &[AgentItem], turns: &[TurnItem], prices: &PriceTable) -> Option<RunCost> {
    let by_agent: BTreeMap<&str, &AgentItem> =
        agents.iter().map(|a| (a.agent_id.as_str(), a)).collect();
    let mut lines: BTreeMap<(String, Engine), Usage> = BTreeMap::new();
    let mut without = 0u32;
    for t in turns {
        match (t.usage.as_ref(), by_agent.get(t.agent_id.as_str())) {
            (Some(u), Some(a)) => add_usage(
                lines
                    .entry((model_of(&a.model_id).to_string(), a.engine))
                    .or_default(),
                u,
            ),
            _ => without += 1,
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut unpriced: Vec<String> = Vec::new();
    let lines = lines
        .into_iter()
        .map(|((model_id, engine), usage)| {
            let usd = match prices.usd(&model_id, &usage) {
                Some(x) => round_usd(x),
                None => {
                    if !unpriced.contains(&model_id) {
                        unpriced.push(model_id.clone());
                    }
                    0.0
                }
            };
            CostLine {
                model_id,
                engine,
                usage,
                usd,
            }
        })
        .collect();
    Some(RunCost {
        prices_version: prices.version.clone(),
        lines,
        turns_without_usage: without,
        unpriced_models: unpriced,
    })
}

/// The tokens a run's turns reported: the four kinds of token on every line of its cost (`RunDetail.tokensUsed`). The sum the delivery card
/// prints; 0 until a turn reports usage.
pub fn total_tokens(cost: Option<&RunCost>) -> u64 {
    cost.map_or(0, |c| {
        c.lines.iter().fold(0u64, |sum, l| {
            let u = &l.usage;
            sum.saturating_add(u.input_tokens)
                .saturating_add(u.output_tokens)
                .saturating_add(u.cache_read_input_tokens)
                .saturating_add(u.cache_write_input_tokens)
        })
    })
}

/// The sum of a cost's lines (`RunDetail.costUsd`).
pub fn total_usd(cost: &RunCost) -> f64 {
    round_usd(cost.lines.iter().map(|l| l.usd).sum())
}
