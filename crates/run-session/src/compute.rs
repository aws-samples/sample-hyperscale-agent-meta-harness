//! Which machine a run's session starts on.
//!
//! The user's request may carry `compute` (`microvm`, `instances` or `auto`; `ham` sends only the first two). `auto` and an absent member mean the
//! stage's own policy ([`ComputePolicy`], from `COMPUTE_POLICY`), which is [`DEFAULT_COMPUTE_POLICY`] when the stage sets nothing. The choice is made
//! once, when `start-session` runs for the first time, and is written on the Agent item before the first invoke: a retry reads it back
//! ([`recorded`]) and never decides again, so a deploy that changes the policy between two attempts cannot move a run.
//!
//! An `instances` session needs three facts of the stage: the Instances worker runtime (its ARN in the SSM parameter named by
//! `INSTANCES_RUNTIME_ARN_PARAMETER`), the capacity provider (its id in the SSM parameter named by `CAPACITY_PROVIDER_ID_PARAMETER`; the id exists only
//! after the stack that creates the provider is deployed, so the function holds the parameter's name, never the id, and `DeleteCapacityProviderSession`
//! at the end of the run uses the id that was read when the session started and is recorded on the Agent item) and, when the stage sets one, the most
//! sessions that may be active at once (`COMPUTE_MAX_ACTIVE`).

use harness_protocol::direct::{ComputeMode, ComputeOptions};
use harness_protocol::store::AgentItem;

/// The stage's policy for a run whose request says `auto` or nothing, when the stage sets no `COMPUTE_POLICY`. The ONE place the default is written:
/// sets it from the first-call time the live gate measures (about 93 s, inside "about one to two minutes", so `Instances`), by editing
/// this line and the assertion in `tests/compute_choice.rs`.
pub const DEFAULT_COMPUTE_POLICY: ComputePolicy = ComputePolicy::Microvm;

/// The endpoint an Instances runtime is invoked at when it has no `live` endpoint to pin.
pub const QUALIFIER_DEFAULT: &str = "DEFAULT";

/// The most `COMPUTE_MAX_ACTIVE` may be (a typing slip guard; the EC2 quota is the real ceiling).
pub const MAX_ACTIVE_LIMIT: u32 = 1000;

/// The kind of machine a session runs on, as decided for one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compute {
    Microvm,
    Instances,
}

impl Compute {
    /// The word on the Agent item (`AgentItem.compute`).
    pub fn word(self) -> &'static str {
        match self {
            Compute::Microvm => "microvm",
            Compute::Instances => "instances",
        }
    }

    /// The kind a recorded word names; `None` for anything else (including `auto`, which is never recorded).
    pub fn from_word(word: &str) -> Option<Compute> {
        match word {
            "microvm" => Some(Compute::Microvm),
            "instances" => Some(Compute::Instances),
            _ => None,
        }
    }
}

/// What the stage does for a request that leaves the machine to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputePolicy {
    Microvm,
    Instances,
}

impl ComputePolicy {
    pub fn word(self) -> &'static str {
        match self {
            ComputePolicy::Microvm => "microvm",
            ComputePolicy::Instances => "instances",
        }
    }

    /// The policy a `COMPUTE_POLICY` word names; `None` for anything else (`auto` is a user's word, not a stage policy).
    pub fn parse(word: &str) -> Option<ComputePolicy> {
        match word {
            "microvm" => Some(ComputePolicy::Microvm),
            "instances" => Some(ComputePolicy::Instances),
            _ => None,
        }
    }
}

impl From<ComputePolicy> for Compute {
    fn from(p: ComputePolicy) -> Compute {
        match p {
            ComputePolicy::Microvm => Compute::Microvm,
            ComputePolicy::Instances => Compute::Instances,
        }
    }
}

/// The compute settings of the function (`Config.compute`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeStage {
    /// `COMPUTE_POLICY`; [`DEFAULT_COMPUTE_POLICY`] when unset.
    pub policy: ComputePolicy,
    /// `COMPUTE_MAX_ACTIVE`: the most Instances sessions active at once; `None` sets no cap (the account's EC2 quota still applies).
    pub max_active: Option<u32>,
}

impl Default for ComputeStage {
    fn default() -> Self {
        ComputeStage {
            policy: DEFAULT_COMPUTE_POLICY,
            max_active: None,
        }
    }
}

/// The machine for a run that has none recorded yet: the user's explicit word, else the stage's policy.
pub fn choose_compute(asked: Option<ComputeOptions>, policy: ComputePolicy) -> Compute {
    match asked.map(|c| c.mode) {
        Some(ComputeMode::Microvm) => Compute::Microvm,
        Some(ComputeMode::Instances) => Compute::Instances,
        Some(ComputeMode::Auto) | None => policy.into(),
    }
}

/// The machine a run's Agent item records: `Some(Microvm)` for an item written before the member existed, `None` for a word this build does not
/// know (the caller treats it as an invalid record, not as the microVM).
pub fn recorded(agent: &AgentItem) -> Option<Compute> {
    match agent.compute.as_deref() {
        None => Some(Compute::Microvm),
        Some(w) => Compute::from_word(w),
    }
}

/// The names of the two SSM parameters an Instances session reads when it starts (the stack publishes the values and names the parameters on the
/// function). `None` is a stage that does not name that parameter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstancesParameters {
    /// `INSTANCES_RUNTIME_ARN_PARAMETER`: holds the Instances worker runtime's ARN.
    pub runtime_arn: Option<String>,
    /// `CAPACITY_PROVIDER_ID_PARAMETER`: holds the AgentCore capacity provider's id.
    pub capacity_provider: Option<String>,
}

/// The compute settings from the function environment, and the names of the SSM parameters that hold the Instances runtime's ARN and the capacity
/// provider's id. Every variable is optional; a value that is set and wrong is an error that names the variable. A stage whose policy is `instances`
/// must name both parameters (nothing could start otherwise); a stage that keeps the microVM as its policy may name them or leave them out.
pub fn from_lookup(
    get: impl Fn(&str) -> Option<String>,
) -> Result<(ComputeStage, InstancesParameters), String> {
    let text = |k: &str| get(k).filter(|v| !v.is_empty());
    let policy = match text("COMPUTE_POLICY") {
        None => DEFAULT_COMPUTE_POLICY,
        Some(w) => ComputePolicy::parse(&w)
            .ok_or_else(|| format!("COMPUTE_POLICY must be microvm or instances, got {w:?}"))?,
    };
    let max_active = match text("COMPUTE_MAX_ACTIVE") {
        None => None,
        Some(v) => match v.parse::<u32>() {
            Ok(n) if (1..=MAX_ACTIVE_LIMIT).contains(&n) => Some(n),
            _ => {
                return Err(format!(
                    "COMPUTE_MAX_ACTIVE must be a whole number from 1 to {MAX_ACTIVE_LIMIT}, got {v:?}"
                ));
            }
        },
    };
    let parameters = InstancesParameters {
        runtime_arn: text("INSTANCES_RUNTIME_ARN_PARAMETER"),
        capacity_provider: text("CAPACITY_PROVIDER_ID_PARAMETER"),
    };
    if policy == ComputePolicy::Instances {
        if parameters.runtime_arn.is_none() {
            return Err("missing environment variable INSTANCES_RUNTIME_ARN_PARAMETER (COMPUTE_POLICY is instances)".into());
        }
        if parameters.capacity_provider.is_none() {
            return Err("missing environment variable CAPACITY_PROVIDER_ID_PARAMETER (COMPUTE_POLICY is instances)".into());
        }
    }
    Ok((ComputeStage { policy, max_active }, parameters))
}
