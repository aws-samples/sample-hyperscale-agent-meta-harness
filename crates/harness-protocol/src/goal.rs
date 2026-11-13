//! Goal status, phase and evaluator verdicts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Goal status of a run: `active`, `paused`, `stalled`, `blocked`, `usage_limited`, `budget_limited`, `complete` or `cleared`. `complete` and
/// `cleared` are terminal.
///
/// `paused` is a stop on purpose: the run reached the total time it was given, or the person or the harness stopped it, and the work
/// is saved and delivered. `stalled` is a stop the service had to make because the run lost its progress: the session stopped answering, a
/// restore failed, several sessions in a row produced nothing, or Cline went silent. Both name a reason and a way to resume
/// ([`ResumeMode`]) on the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
    Cleared,
    /// See the type's text. Appended last so the declaration order of the older words does not move.
    Stalled,
}

/// How a `paused` or `stalled` run can go on. The service stores it beside the status and the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
    /// The same run id goes on in a new session. Reserved: no run is given this word yet (a run that stopped has been delivered, and a
    /// delivery is made once per run), so clients treat it as a follow-up request to the same route.
    SameRun,
    /// A new run starts from the code this run delivered and carries the same request (`POST /v1/runs/{runId}/resume`).
    FollowUp,
    /// Nothing to resume: no code was delivered, or the cause still holds.
    None,
}

impl GoalStatus {
    /// `complete` and `cleared` admit no further transition.
    pub fn is_terminal(self) -> bool {
        matches!(self, GoalStatus::Complete | GoalStatus::Cleared)
    }

    /// A direct run that ended and delivered its work: `complete`, or `paused` and `stalled` (a stop the run made delivered). Its session is
    /// over: it takes no answer, no message and no cancel, and a follow-up continues it.
    pub fn is_delivered(self) -> bool {
        matches!(
            self,
            GoalStatus::Complete | GoalStatus::Paused | GoalStatus::Stalled
        )
    }
}

/// Sub-state of `active`: planning, the TDD work phases, then VERIFY and delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    Planning,
    Red,
    Green,
    Refactor,
    Gating,
    Evaluating,
    Integrating,
    Delivering,
}

/// Evaluator verdict on the evidence bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Met,
    NotMet,
    Impossible,
}

/// A recorded verdict; the body lives in S3.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VerdictRef {
    pub evaluator_model_id: String,
    pub verdict: Verdict,
    pub s3_uri: String,
}
