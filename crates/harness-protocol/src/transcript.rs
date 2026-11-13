//! The stored conversation and the engine capability record on the wire.
//!
//! agentd's transcript module writes the conversation as numbered, write-once gzip chunk objects under the run's agent prefix and,
//! once per session, one `session.capabilities` event. This module holds what both ends and the Lambdas share: the feature and control
//! words the worker declares, the object key helpers, the completeness summary that `turn-complete` carries, and the capability
//! payload with its size rule. The chunk body (header and records) is the reader's contract and lives in `ham_core::transcript`; its
//! writer is agentd, which does not depend on `ham-core`.

use crate::messages::{MESSAGE_NEXT_CONTROL, MESSAGE_NOW_CONTROL};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The feature word an Agent item carries when the worker's image writes the transcript.
pub const TRANSCRIPT_FEATURE: &str = "transcript.v1";
/// The most entries one completeness summary carries.
pub const MAX_COMPLETENESS_ENTRIES: usize = 256;
/// The most words one harness list keeps, and the longest word.
pub const MAX_HARNESS_WORDS: usize = 16;
pub const MAX_HARNESS_WORD_CHARS: usize = 64;

/// The sequence number a chunk file name carries: exactly twelve digits and `.ndjson.gz`.
pub fn seq_of_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(".ndjson.gz")?;
    if digits.len() == 12 && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

/// Why a range of the stored conversation is not shown in full. The words a writer may declare; the reader adds two of its own
/// (`unreadable`, `missing`) that are never written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CompletenessKind {
    /// Thoughts, binary output and replayed history: left out on purpose.
    Excluded,
    /// A size cap cut it.
    Truncated,
    /// The writer shed it under load.
    Shed,
    /// Redaction withheld it.
    Suppressed,
    /// Storage failed.
    Failed,
    /// Another writer held the key, or two writers overlapped.
    Conflict,
    /// An older image stores no transcript.
    Unsupported,
    /// A writer ended without a final flush.
    TailUnknown,
}

/// One declared range of loss or exclusion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompletenessEntry {
    pub kind: CompletenessKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

/// What the worker declares at `turn-complete` (protocol 1.38.0): the last chunk it stored and what it left out or lost. It is the
/// worker's report, written by a process the engine can reach; the state `complete` means "the worker reports no loss".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transcript_seq: Option<u64>,
    #[serde(default)]
    pub entries: Vec<CompletenessEntry>,
}

impl TranscriptSummary {
    /// Valid, and declaring nothing but what was left out on purpose (thoughts, binary output): the state `complete` means the worker
    /// reports no loss.
    pub fn is_lossless(&self) -> bool {
        self.validate()
            && self
                .entries
                .iter()
                .all(|e| e.kind == CompletenessKind::Excluded)
    }

    /// Whether the summary is well formed: a bounded number of entries and every range forward with both ends or neither.
    pub fn validate(&self) -> bool {
        self.entries.len() <= MAX_COMPLETENESS_ENTRIES
            && self.entries.iter().all(|e| match (e.from_seq, e.to_seq) {
                (Some(a), Some(b)) => a <= b,
                (None, None) => true,
                _ => false,
            })
    }
}

/// The word lists a worker registers on its Agent item: the harness's own controls and the features of its image.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HarnessCapabilities {
    #[serde(default)]
    pub controls: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
}

impl HarnessCapabilities {
    /// What this build of the worker image offers.
    pub fn this_build() -> Self {
        Self {
            controls: vec![MESSAGE_NEXT_CONTROL.into(), MESSAGE_NOW_CONTROL.into()],
            features: vec![TRANSCRIPT_FEATURE.into()],
        }
    }

    /// The same lists with every word checked ([`clean_words`]).
    pub fn cleaned(&self) -> Self {
        Self {
            controls: clean_words(&self.controls),
            features: clean_words(&self.features),
        }
    }
}

/// `words` without a word that is not a lower-case token (`[a-z][a-z0-9._-]*`, at most 64 characters), without repeats, at most 16.
pub fn clean_words(words: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words {
        if out.len() == MAX_HARNESS_WORDS {
            break;
        }
        let mut chars = w.chars();
        let plain = chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && w.chars().count() <= MAX_HARNESS_WORD_CHARS
            && chars.all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if plain && !out.contains(w) {
            out.push(w.clone());
        }
    }
    out
}

/// The engine, as ACP's `initialize` names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityAgent {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityMode {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityModes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default)]
    pub available: Vec<CapabilityMode>,
}

/// One session option (`session/new` `configOptions`): its name and, while the record has room, the value it holds now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Value>,
}

/// One command the engine advertises.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `session.capabilities`: what the engine says it can do in this session, in ACP's
/// own words, and the harness's own lists. One per session. The client reads it to decide what to show; control-api and worker-api
/// read `harness` to register the Agent item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilitiesPayload {
    /// The record's format version.
    pub v: u32,
    /// The engine's display label.
    pub engine: String,
    /// 1 for the run's first session, 2 after a fresh session, and so on.
    pub session: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// ACP `protocolVersion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<CapabilityAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<CapabilityModes>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<CapabilityConfigOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CapabilityCommand>,
    pub harness: HarnessCapabilities,
    /// Sections were dropped to fit the event's payload limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl SessionCapabilitiesPayload {
    /// The record cut to at most `max` bytes of JSON: first the commands, then the values of the options, then the session
    /// capabilities, and when that is still not enough the option list, the mode list and the agent's name. The harness lists, the
    /// engine label and the session number are never dropped. `truncated` is set when anything went.
    pub fn bounded(mut self, max: usize) -> Self {
        let fits = |p: &Self| serde_json::to_vec(p).map_or(0, |v| v.len()) <= max;
        if fits(&self) {
            return self;
        }
        self.truncated = true;
        self.commands.clear();
        if fits(&self) {
            return self;
        }
        for o in &mut self.config_options {
            o.current = None;
        }
        if fits(&self) {
            return self;
        }
        self.session_capabilities = None;
        if fits(&self) {
            return self;
        }
        self.config_options.clear();
        if fits(&self) {
            return self;
        }
        if let Some(m) = &mut self.modes {
            m.available.clear();
        }
        if fits(&self) {
            return self;
        }
        self.agent = None;
        if fits(&self) {
            return self;
        }
        self.prompt_capabilities = None;
        self
    }
}
