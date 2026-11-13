//! The metrics port of the research route: what the Tavily client tells the operator, as numbers and nothing else.
//!
//! The Tavily client reports four things: an answer that refused the key (401 or 403), an answer that limited the rate (429), an answer
//! that reached the plan or pay-as-you-go limit (432 or 433) and the age of the stored key. The port is [`Metrics`], with a no-op default
//! ([`NoMetrics`]); [`EmfMetrics`] writes CloudWatch Embedded Metric Format lines to the Lambda's log, where CloudWatch turns them into
//! the metrics `Hamh/Research` / `Stage` that the stage's alarms watch (`infra/lib/stacks/research-alarms.ts`).
//!
//! An [`Event`] holds a status class or a time, never a key, a header, a response body or an address, and a line holds only the fixed
//! names below, the stage name and a number. The sink gets no secret and no response, so it has nothing to leak.

use crate::ports::{Clock, SystemClock};
use serde_json::{Map, Value, json};
use std::fmt;
use std::io::Write;
use std::sync::{Mutex, PoisonError};

/// Milliseconds in a day, for the age of the stored key.
const DAY_MS: f64 = 86_400_000.0;

/// The CloudWatch namespace of every metric. `infra/lib/stacks/research-alarms.ts` holds the same string; a test on each side pins both.
pub const NAMESPACE: &str = "Hamh/Research";
/// The one dimension of every metric. Its value is the stage name (`direct`).
pub const DIMENSION: &str = "Stage";

/// The metric names, shared with the alarms.
pub mod name {
    pub const KEY_REFUSED: &str = "TavilyKeyRefused";
    pub const RATE_LIMITED: &str = "TavilyRateLimited";
    pub const PLAN_LIMIT: &str = "TavilyPlanLimit";
    pub const KEY_AGE_DAYS: &str = "TavilyKeyAgeDays";
}

/// Something that happened to the stored Tavily key or to a call made with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Tavily answered 401 or 403.
    KeyRefused,
    /// Tavily answered 429.
    RateLimited,
    /// Tavily answered 432 or 433.
    PlanLimit,
    /// The key was read from Secrets Manager; `stored_at_ms` is the creation time of the version read, in epoch milliseconds.
    KeyLoaded { stored_at_ms: u64 },
}

/// Where the Tavily client reports. Implementations never block and never fail the call.
pub trait Metrics: Send + Sync {
    fn record(&self, event: Event);
}

/// The default: reports nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMetrics;

impl Metrics for NoMetrics {
    fn record(&self, _event: Event) {}
}

/// Whether `stage` is usable as the value of the `Stage` dimension: 1 to 32 characters, lower-case letters, digits and hyphens.
pub fn valid_stage(stage: &str) -> bool {
    !stage.is_empty()
        && stage.len() <= 32
        && stage
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Writes one Embedded Metric Format line per event to `out` (the Lambda's standard output).
pub struct EmfMetrics<W, C = SystemClock> {
    stage: String,
    clock: C,
    out: Mutex<W>,
}

impl<W, C> fmt::Debug for EmfMetrics<W, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmfMetrics")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Send, C: Clock> EmfMetrics<W, C> {
    /// `stage` is the value of the `Stage` dimension (see [`valid_stage`]); `clock` gives the time of a line and the age of a key.
    pub fn new(stage: &str, clock: C, out: W) -> Result<Self, String> {
        if !valid_stage(stage) {
            return Err(
                "the stage name for the metrics is 1 to 32 lower-case letters, digits or hyphens"
                    .into(),
            );
        }
        Ok(Self {
            stage: stage.to_string(),
            clock,
            out: Mutex::new(out),
        })
    }
}

/// The age in days, with a fraction, of a key version stored at `stored_at_ms`. A creation time after `now_ms` (clock skew) reads as zero.
fn age_days(now_ms: u64, stored_at_ms: u64) -> f64 {
    // The millisecond count of a key's age fits an f64 exactly for any age of this stack's life (below 2^53).
    now_ms.saturating_sub(stored_at_ms) as f64 / DAY_MS
}

impl<W: Write + Send, C: Clock> EmfMetrics<W, C> {
    /// The line of one event: `{"_aws": {...}, "Stage": <stage>, <metric>: <number>}`.
    fn line(&self, event: Event) -> String {
        let now = self.clock.now_ms();
        let (metric, unit, value) = match event {
            Event::KeyRefused => (name::KEY_REFUSED, "Count", json!(1)),
            Event::RateLimited => (name::RATE_LIMITED, "Count", json!(1)),
            Event::PlanLimit => (name::PLAN_LIMIT, "Count", json!(1)),
            Event::KeyLoaded { stored_at_ms } => (
                name::KEY_AGE_DAYS,
                "None",
                json!(age_days(now, stored_at_ms)),
            ),
        };
        let mut root = Map::new();
        root.insert(
            "_aws".to_string(),
            json!({
                "Timestamp": now,
                "CloudWatchMetrics": [{
                    "Namespace": NAMESPACE,
                    "Dimensions": [[DIMENSION]],
                    "Metrics": [{ "Name": metric, "Unit": unit }],
                }],
            }),
        );
        root.insert(DIMENSION.to_string(), Value::String(self.stage.clone()));
        root.insert(metric.to_string(), value);
        let mut text = Value::Object(root).to_string();
        text.push('\n');
        text
    }
}

impl<W: Write + Send, C: Clock> Metrics for EmfMetrics<W, C> {
    /// One write of one whole line, so lines of two calls never mix. A log that cannot be written is not the caller's failure.
    fn record(&self, event: Event) {
        let text = self.line(event);
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}
