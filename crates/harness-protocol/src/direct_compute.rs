//! The compute choice of a direct run.
//!
//! A run's Cline works on one machine for the whole run. The AgentCore runtime of the stage has two kinds: the microVM (2 vCPU, 8 GiB, a 1 GB
//! checkout mount, the machine every run used before this version) and Instances (one EC2 machine per session in the stage's account, with
//! many CPUs and a large disk). `StartDirectRunRequest.compute` ([`ComputeOptions`]) is the user's choice, and the stage decides the rest:
//! `auto` and an absent member both mean "the stage's own policy". `ham` sends the member only for `microvm` or `instances`, so a user who
//! never touches the setting sends the request of an older client byte for byte.
//!
//! The choice is made once, when the session starts, and is recorded on the Agent item (`AgentItem.compute`, `AgentItem.capacity_provider_id`):
//! the compute type of a runtime cannot change and a session lives on one machine, so no retry, no policy change and no later request moves a run
//! to the other kind. The member names a mode and nothing else; the machine's size and the stage's policy are the deployment's.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The words of `run.compute` and of the flag, in the order the setting lists them.
pub const COMPUTE_WORDS: [&str; 3] = ["auto", "microvm", "instances"];

/// `run.compute`: which kind of machine the session runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputeMode {
    /// The stage decides (its policy word). The default; the member is not sent.
    Auto,
    /// The microVM: 2 vCPU, 8 GiB, a 1 GB checkout mount.
    Microvm,
    /// One EC2 machine per session in the stage's account (AgentCore Instances), with the stage's CPUs and disk.
    Instances,
}

impl ComputeMode {
    /// The word of the setting, of the flag and of the wire.
    pub fn word(self) -> &'static str {
        match self {
            ComputeMode::Auto => "auto",
            ComputeMode::Microvm => "microvm",
            ComputeMode::Instances => "instances",
        }
    }

    /// The mode a word names; `None` for anything else (the words are lower case and exact).
    pub fn parse(word: &str) -> Option<ComputeMode> {
        match word {
            "auto" => Some(ComputeMode::Auto),
            "microvm" => Some(ComputeMode::Microvm),
            "instances" => Some(ComputeMode::Instances),
            _ => None,
        }
    }
}

/// `StartDirectRunRequest.compute` (protocol 1.45.0): the user's choice of machine. The request type refuses an unknown word and an unknown
/// member before anything is stored (a 400 `bad-request` that never repeats the value), so there is no separate check.
///
/// Protocol 1.57.0 adds `containerSignals`. The Instances machine cannot run containers, so the repo-gateway looks for container use in
/// the staged repository, and these lines say what counts: each is a signal (file names, optionally `contains` and a text) or the word `default`,
/// which stands for the stage's built-in list. Absent means the built-in list, an empty list means no signals. The wire carries the lines and
/// nothing else; `ham-core` owns the grammar and the built-in list, and control-api checks the lines at the start of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComputeOptions {
    pub mode: ComputeMode,
    /// The user's container signals; see the type. Left out when the user set none, so a request that keeps the defaults is the request of an older client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_signals: Option<Vec<String>>,
}

impl ComputeOptions {
    /// The member for `mode`: sent for `microvm` and `instances`, left out (`None`) for `auto`, which is what an absent member means.
    pub fn for_mode(mode: ComputeMode) -> Option<ComputeOptions> {
        ComputeOptions::new(mode, None)
    }

    /// The member for `mode` and the user's container signals: left out only for `auto` with no list, because a list must reach the stage whatever
    /// the mode is.
    pub fn new(
        mode: ComputeMode,
        container_signals: Option<Vec<String>>,
    ) -> Option<ComputeOptions> {
        (mode != ComputeMode::Auto || container_signals.is_some()).then_some(ComputeOptions {
            mode,
            container_signals,
        })
    }
}
