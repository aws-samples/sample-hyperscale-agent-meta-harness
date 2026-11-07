//! The flow a run declared.
//!
//! The lifecycle contract asks a run to end the message of its final commit with a trailer line, `Flow: quick` or `Flow: full`, naming the path
//! it took (the short path for a bounded change, or the eight steps). Until this version nothing in the product read that line. The harness now
//! reads it at the end of the run and records it as a fact beside the label: `DirectSummary.flow` in `summary.json`, `DirectEvidence.flow` in
//! EVIDENCE.md and `evidence/direct.json`, and one `Flow` row on the delivery card. No label, outcome, check or setting depends on it.
//!
//! Three values: `quick`, `full`, and `none` for a run whose final commit message and last message declare neither. The member is absent when
//! the harness could not read the run's commits at all (the session ended before any work was saved, or the repository could not be trusted).
//!
//! Where the word comes from, first match wins ([`DirectFlow::read`]):
//!
//! 1. the trailer block of the final task commit's message: the last paragraph of the message, when it is not the only paragraph and every line
//!    of it has the shape of a Git trailer (`Key: value`, or a continuation line that starts with white space). The last `Flow` line of that
//!    block counts, its key without regard to case, its value exactly `quick` or `full` without regard to case. This is the block that
//!    `git interpret-trailers --parse` reads, so a line under a sentence of the same paragraph, or followed by prose, is not a declaration;
//! 2. the first line of the engine's last message, when it is `Flow: quick` or `Flow: full` (the form the first version of the contract asked
//!    for), with backticks or asterisks around it ignored.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The flow a run declared.
///
/// A reader is lenient, as for every record the harness writes: a word this build does not know reads as [`DirectFlow::Undeclared`], so a
/// later writer that adds a word cannot make an older reader drop the whole summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DirectFlow {
    /// `Flow: quick`: the short path for a bounded change.
    #[serde(rename = "quick")]
    Quick,
    /// `Flow: full`: the eight steps.
    #[serde(rename = "full")]
    Full,
    /// Neither the final commit message nor the last message declared a flow. The wire word is `none`.
    #[serde(rename = "none", other)]
    Undeclared,
}

impl DirectFlow {
    /// The word on the wire, in EVIDENCE.md and on the card.
    pub const fn word(self) -> &'static str {
        match self {
            DirectFlow::Quick => "quick",
            DirectFlow::Full => "full",
            DirectFlow::Undeclared => "none",
        }
    }

    /// The flow a wire word names; `None` for anything else (the words are lower case and exact).
    pub fn parse(word: &str) -> Option<DirectFlow> {
        match word {
            "quick" => Some(DirectFlow::Quick),
            "full" => Some(DirectFlow::Full),
            "none" => Some(DirectFlow::Undeclared),
            _ => None,
        }
    }

    /// The value of a declaration: exactly `quick` or `full`, without regard to case. `none` is no declaration.
    fn declared(value: &str) -> Option<DirectFlow> {
        if value.eq_ignore_ascii_case("quick") {
            Some(DirectFlow::Quick)
        } else if value.eq_ignore_ascii_case("full") {
            Some(DirectFlow::Full)
        } else {
            None
        }
    }

    /// The flow the trailer block of a commit message declares, or `None` when it declares none (see the module text for the block).
    pub fn from_commit_message(message: &str) -> Option<DirectFlow> {
        let text = message.replace("\r\n", "\n");
        // Paragraphs are separated by one or more blank lines; the message must have a paragraph before the trailer block.
        let mut paragraphs: Vec<Vec<&str>> = vec![Vec::new()];
        for line in text.lines() {
            if line.trim().is_empty() {
                if paragraphs.last().is_some_and(|p| !p.is_empty()) {
                    paragraphs.push(Vec::new());
                }
            } else if let Some(p) = paragraphs.last_mut() {
                p.push(line);
            }
        }
        if paragraphs.last().is_some_and(Vec::is_empty) {
            paragraphs.pop();
        }
        if paragraphs.len() < 2 {
            return None;
        }
        let block = paragraphs.last()?;
        let mut flow = None;
        for (i, line) in block.iter().enumerate() {
            if line.starts_with([' ', '\t']) {
                // A continuation line belongs to the trailer above it; the first line of the block cannot be one.
                if i == 0 {
                    return None;
                }
                continue;
            }
            let (key, value) = line.split_once(':')?;
            let key = key.trim_end();
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return None;
            }
            if key.eq_ignore_ascii_case("flow") {
                flow = DirectFlow::declared(value.trim());
            }
        }
        flow
    }

    /// The flow the first line of an engine message declares, or `None`: the first non-blank line, with backticks and asterisks around it
    /// ignored, must be `Flow: quick` or `Flow: full` (key and word without regard to case).
    pub fn from_last_message(message: &str) -> Option<DirectFlow> {
        let first = message.lines().find(|l| !l.trim().is_empty())?;
        let line = first.trim().trim_matches(['`', '*']).trim();
        let (key, value) = line.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("flow") {
            return None;
        }
        DirectFlow::declared(value.trim())
    }

    /// The flow of a run: the final task commit's trailer, else the first line of the last message, else [`DirectFlow::Undeclared`].
    pub fn read(commit_message: Option<&str>, last_message: Option<&str>) -> DirectFlow {
        commit_message
            .and_then(DirectFlow::from_commit_message)
            .or_else(|| last_message.and_then(DirectFlow::from_last_message))
            .unwrap_or(DirectFlow::Undeclared)
    }
}
