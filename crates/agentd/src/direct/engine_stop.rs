//! The stops the engine chooses itself. A prompt the engine ends `cancelled` when agentd wrote no
//! `session/cancel` is the engine's own brake, not a provider failure: on the pinned engine (3.0.71) it is the loop guard (five identical
//! consecutive tool calls) or the mistake limit (six failed turns in a row), while a really cut model stream never ends `cancelled`.
//!
//! The first slices of provider failover read such a stop as a `Permanent` provider failure, so a run that the engine stopped itself
//! walked every route (many attempts and model switches, and it ended on the last model of the list). It gets its own class
//! ([`crate::direct::retry::RetryClass::EngineStop`]) and its own resume count; it never switches a route or a model, never excludes a
//! route and never counts toward the switch bound.
//!
//! This module holds the pure part: the facts one attempt left ([`AttemptFacts`], kept by the collector), the shape they show
//! ([`Shape`]) and the one case that is worth a probe ([`needs_probe`]). [`StopReport`] bundles them with the attempt's duration and
//! gives the verdict ([`Verdict`]); it is also the input of the layered recovery ladder:
//! a later failure
//! analysis reads the shape, the facts and the duration from here and builds its bounded digest of the chat from them. Nothing here
//! calls a model. The reversal triggers, should the pinned engine change: any run on
//! the pinned build where a provider fault ends a prompt `cancelled`, or a model-invocation log that shows an incomplete model call just
//! before a stop. A CI test pins the engine pin to this rule, so an engine upgrade cannot land
//! without the cut-stream measurements being made again.

use std::time::Duration;

/// The upstream tag of the engine build the rule above was measured on.
pub const MEASURED_UPSTREAM_TAG: &str = "cli-v3.0.71";
/// The tree of the engine's patch series on that tag (`[series] tree` of the same lock). Series 14 (patch 0014, blank keys of a replayed Bedrock
/// tool input) and series 13 as its control gave that result earlier.
pub const MEASURED_SERIES_TREE: &str = "f8990b6745402aec9bf0458611c763a1a7e710f8";

/// Identical consecutive tool calls at the end of the attempt that show the engine's loop guard (its hard threshold; the soft warning is 3).
pub const LOOP_GUARD_CALLS: u32 = 5;
/// Failed tool calls in a row at the end of the attempt that show the engine's consecutive-mistake limit.
pub const MISTAKE_LIMIT_TURNS: u32 = 6;
/// An unexplained stop with no completed call that came sooner than this after the prompt was sent might be a provider fault that
/// showed no output, so the route is asked once with a small test request.
pub const AMBIGUOUS_UNDER: Duration = Duration::from_secs(120);

/// What one attempt (one `session/prompt`) did, as the collector saw it. Counters over the tool calls of this attempt only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttemptFacts {
    /// Tool calls the engine started in this attempt.
    pub tool_calls: u64,
    /// Tool calls that ended `completed`.
    pub completed: u64,
    /// Tool calls that ended `failed` in a row at the end of the attempt (a completed call ends the row).
    pub failed_tail: u32,
    /// Tool calls with the same kind and title in a row at the end of the attempt.
    pub identical_tail: u32,
}

/// The shape of a stop the engine chose, from the facts. A hint for the explanation and for the probe; it never moves the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// [`LOOP_GUARD_CALLS`] or more identical calls at the end.
    LoopGuard,
    /// [`MISTAKE_LIMIT_TURNS`] or more failed calls at the end.
    MistakeLimit,
    /// Neither tail is long enough.
    Unexplained,
}

impl Shape {
    /// The word the progress event and the log carry.
    pub fn word(self) -> &'static str {
        match self {
            Self::LoopGuard => "loop-guard",
            Self::MistakeLimit => "mistake-limit",
            Self::Unexplained => "unexplained",
        }
    }
}

/// The shape the facts show; the loop guard is read first.
pub fn shape_of(f: &AttemptFacts) -> Shape {
    if f.identical_tail >= LOOP_GUARD_CALLS {
        Shape::LoopGuard
    } else if f.failed_tail >= MISTAKE_LIMIT_TURNS {
        Shape::MistakeLimit
    } else {
        Shape::Unexplained
    }
}

/// True for the one shape that could hide a provider fault: nothing explains the stop, the attempt completed no call and it ended
/// less than [`AMBIGUOUS_UNDER`] after the prompt was sent. The caller asks the route once; only a route that does not answer turns the
/// stop into a provider failure.
pub fn needs_probe(f: &AttemptFacts, ran: Duration) -> bool {
    shape_of(f) == Shape::Unexplained && f.completed == 0 && ran < AMBIGUOUS_UNDER
}

/// One short line saying what the attempt looked like at its end (no title, no text of the work).
pub fn describe(shape: Shape, f: &AttemptFacts) -> String {
    match shape {
        Shape::LoopGuard => format!("{} identical tool calls in a row", f.identical_tail),
        Shape::MistakeLimit => format!("{} failed tool calls in a row", f.failed_tail),
        Shape::Unexplained => format!(
            "no repeated or failed calls at the end; {} completed tool call{}",
            f.completed,
            if f.completed == 1 { "" } else { "s" }
        ),
    }
}

/// What the recovery loop learned in one stage (one call of `prompt_with_retry`: the user's prompt, an instruction, a fresh session) that
/// a reader above it takes as plain fields. The caller starts a stage with
/// `FailoverRecord::begin_stage`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageFacts {
    /// Engine stops seen in this stage (the first is resumed by the loop, a second is the human's to handle).
    pub stage_engine_stops: u32,
    /// The shape of the latest engine stop of the stage.
    pub last_shape: Option<Shape>,
    /// Identical tool calls in a row at the end of the latest engine-stopped attempt; `None` before any engine stop.
    pub identical_calls: Option<u32>,
    /// Failed tool calls in a row at the end of the latest engine-stopped attempt; `None` before any engine stop.
    pub failed_turns: Option<u32>,
    /// The ladder went on hold on its bound of prompts that achieve nothing ([`crate::direct::failover::FRUITLESS_PROMPTS_MAX`]).
    pub cap_hit: bool,
    /// The prompt failed with error -32603 while the session disk was short: the cause, with its numbers. The loop stopped at once
    /// (no resume, no switch of route, no further prompt), and the assessment, the classifier and the end sequence read the cause here.
    pub disk_full: Option<crate::direct::disk::DiskFull>,
}

/// What the harness knows about one stop the engine chose: the shape, the facts it was read from and how long the attempt ran. The
/// pure input of the decision below and of any later reader of the stop; it
/// holds no text of the work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopReport {
    pub shape: Shape,
    pub facts: AttemptFacts,
    /// How long after the prompt was sent the engine stopped.
    pub ran: Duration,
}

/// What a stop is: the engine's own brake, or a provider failure that showed no output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The engine's own brake: resume on the same route and model, never switch.
    EngineStop,
    /// A provider fault that showed no output: the provider path (resumes, then the ladder).
    ProviderFailure,
}

impl StopReport {
    pub fn new(facts: AttemptFacts, ran: Duration) -> Self {
        Self {
            shape: shape_of(&facts),
            facts,
            ran,
        }
    }

    /// The word the progress event and the log carry.
    pub fn hint(&self) -> &'static str {
        self.shape.word()
    }

    /// One short line (see [`describe`]).
    pub fn describe(&self) -> String {
        describe(self.shape, &self.facts)
    }

    /// True when the route is worth one small test request (see [`needs_probe`]).
    pub fn needs_probe(&self) -> bool {
        needs_probe(&self.facts, self.ran)
    }

    /// The verdict: an engine stop, unless the stop is worth a probe and the route did not answer it (`route_down` is `Some(true)`).
    /// `None` means no probe was possible (one route, probe off); a probe never moves a stop that is not worth one.
    pub fn verdict(&self, route_down: Option<bool>) -> Verdict {
        if self.needs_probe() && route_down == Some(true) {
            Verdict::ProviderFailure
        } else {
            Verdict::EngineStop
        }
    }
}
