//! `StartDirectRunRequest.limits`: six optional numbers that move limits of the worker which a user could not change
//! before. Each is left off the wire when the user keeps the default, and the stage's `AGENTD_*` settings stay the defaults a member replaces.
//! [`check_limits`] is the ONE check: control-api (400 `invalid-limits`) and agentd (the run ends `stopped_setup_failed` before any model call)
//! call it, so they accept the same members. `ham` takes the bounds of its keys from the constants here. A member beats the stage's own clamp:
//! the bounds written here are the only ones that apply to a value the user sent.
//!
//! A `0` means "none of it" and never "without limit": no interrupt, no question, no fresh session. The automatic brakes of the run (one
//! automatic fresh session, twelve prompts without progress, three lost or empty sessions in a row, the stall brake of the goal loop and the run's
//! own deadline) do not move with these members.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// One whole-number limit: its default and the bounds a member is checked against (both ends included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitRange {
    pub default: u64,
    pub min: u64,
    pub max: u64,
}

impl LimitRange {
    /// Whether `n` is inside the bounds.
    pub const fn contains(&self, n: u64) -> bool {
        n >= self.min && n <= self.max
    }
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// The stage defaults of the three research byte budgets, and the least and most a member may ask for, as plain constants.
pub const DEFAULT_RESEARCH_FILE_BYTES: u64 = MIB;
pub const DEFAULT_RESEARCH_UNPACKED_BYTES: u64 = 200 * MIB;
pub const DEFAULT_RESEARCH_RUN_BYTES: u64 = 300 * MIB;
pub const MAX_RESEARCH_FILE_BYTES: u64 = 64 * MIB;
pub const MAX_RESEARCH_UNPACKED_BYTES: u64 = 1024 * MIB;
pub const MAX_RESEARCH_RUN_BYTES: u64 = 2048 * MIB;

/// `interrupts`: immediate (`now`) messages one run may make. Each cuts the engine's turn and ends the tool processes of the engine's user, so the
/// count is a guard against an interrupt storm. Stage default `AGENTD_MESSAGES_INTERRUPTS`.
pub const LIMIT_INTERRUPTS: LimitRange = LimitRange {
    default: 5,
    min: 0,
    max: 1000,
};
/// `questions`: questions a stopped run may put to the user. Stage default `AGENTD_DIRECT_INPUT_MAX_REQUESTS`. 0: the run never asks.
pub const LIMIT_QUESTIONS: LimitRange = LimitRange {
    default: 3,
    min: 0,
    max: 1000,
};
/// `freshSessions`: fresh sessions the user may ask for in one run (the one automatic session is counted apart and does not move). Stage default
/// `AGENTD_DIRECT_USER_SESSIONS_MAX`. 0: the user's fresh-session answer is refused.
pub const LIMIT_FRESH_SESSIONS: LimitRange = LimitRange {
    default: 3,
    min: 0,
    max: 1000,
};
/// `researchFileBytes`: one unpacked file of the research material is at most this many bytes. Stage default `AGENTD_RESEARCH_MAX_FILE_BYTES`.
pub const LIMIT_RESEARCH_FILE_BYTES: LimitRange = LimitRange {
    default: DEFAULT_RESEARCH_FILE_BYTES,
    min: 64 * KIB,
    max: MAX_RESEARCH_FILE_BYTES,
};
/// `researchUnpackedBytes`: one repository unpacks to at most this many bytes. Stage default `AGENTD_RESEARCH_MAX_UNPACKED_BYTES`. The upper
/// bound keeps clear of the free space of the worker's root volume; the research module also refuses a placement that would leave less than
/// 256 MiB free.
pub const LIMIT_RESEARCH_UNPACKED_BYTES: LimitRange = LimitRange {
    default: DEFAULT_RESEARCH_UNPACKED_BYTES,
    min: MIB,
    max: MAX_RESEARCH_UNPACKED_BYTES,
};
/// `researchRunBytes`: all the research material of a run is at most this many bytes. Stage default `AGENTD_RESEARCH_MAX_RUN_BYTES`.
pub const LIMIT_RESEARCH_RUN_BYTES: LimitRange = LimitRange {
    default: DEFAULT_RESEARCH_RUN_BYTES,
    min: MIB,
    max: MAX_RESEARCH_RUN_BYTES,
};

/// `StartDirectRunRequest.limits`. Every member is optional; a member left out keeps the stage's value for the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunLimits {
    /// Immediate (`now`) messages one run may make (0 to 1,000; the stage default is 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupts: Option<u32>,
    /// Questions a stopped run may put to the user (0 to 1,000; the stage default is 3; 0 never asks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<u32>,
    /// Fresh sessions the user may ask for in one run (0 to 1,000; the stage default is 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_sessions: Option<u32>,
    /// Bytes of one unpacked file of research material (64 KiB to 64 MiB; the stage default is 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_file_bytes: Option<u64>,
    /// Bytes of one unpacked repository of research material (1 MiB to 1 GiB; the stage default is 200 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_unpacked_bytes: Option<u64>,
    /// Bytes of all the research material of a run (1 MiB to 2 GiB; the stage default is 300 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_run_bytes: Option<u64>,
}

impl RunLimits {
    /// Whether the member says nothing: `ham` leaves it off the wire then.
    pub fn is_empty(&self) -> bool {
        *self == RunLimits::default()
    }
}

/// Why a `limits` member was refused: the member that is out of its range. The first in the order of the members is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitsProblem {
    Interrupts,
    Questions,
    FreshSessions,
    ResearchFileBytes,
    ResearchUnpackedBytes,
    ResearchRunBytes,
}

impl LimitsProblem {
    /// The member's name on the wire.
    pub fn key(self) -> &'static str {
        match self {
            LimitsProblem::Interrupts => "interrupts",
            LimitsProblem::Questions => "questions",
            LimitsProblem::FreshSessions => "freshSessions",
            LimitsProblem::ResearchFileBytes => "researchFileBytes",
            LimitsProblem::ResearchUnpackedBytes => "researchUnpackedBytes",
            LimitsProblem::ResearchRunBytes => "researchRunBytes",
        }
    }

    /// A stable word for the kind of problem (one today).
    pub fn code(self) -> &'static str {
        "out_of_range"
    }

    /// The bounds of the member.
    pub fn range(self) -> LimitRange {
        match self {
            LimitsProblem::Interrupts => LIMIT_INTERRUPTS,
            LimitsProblem::Questions => LIMIT_QUESTIONS,
            LimitsProblem::FreshSessions => LIMIT_FRESH_SESSIONS,
            LimitsProblem::ResearchFileBytes => LIMIT_RESEARCH_FILE_BYTES,
            LimitsProblem::ResearchUnpackedBytes => LIMIT_RESEARCH_UNPACKED_BYTES,
            LimitsProblem::ResearchRunBytes => LIMIT_RESEARCH_RUN_BYTES,
        }
    }

    /// The rule in words, without the value that broke it.
    pub fn rule(self) -> String {
        let r = self.range();
        format!("a whole number from {} to {}", r.min, r.max)
    }
}

impl fmt::Display for LimitsProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "limits: {} must be {}", self.key(), self.rule())
    }
}

impl std::error::Error for LimitsProblem {}

/// The ONE check of the `limits` member (protocol 1.57.0). Pure; the first problem in the order of the members is returned. The message never
/// repeats the value. The research budgets are independent counters (one file, one repository, the whole run): no order between them is required.
pub fn check_limits(l: &RunLimits) -> Result<(), LimitsProblem> {
    let rows = [
        (l.interrupts.map(u64::from), LimitsProblem::Interrupts),
        (l.questions.map(u64::from), LimitsProblem::Questions),
        (
            l.fresh_sessions.map(u64::from),
            LimitsProblem::FreshSessions,
        ),
        (l.research_file_bytes, LimitsProblem::ResearchFileBytes),
        (
            l.research_unpacked_bytes,
            LimitsProblem::ResearchUnpackedBytes,
        ),
        (l.research_run_bytes, LimitsProblem::ResearchRunBytes),
    ];
    for (value, problem) in rows {
        if let Some(n) = value
            && !problem.range().contains(n)
        {
            return Err(problem);
        }
    }
    Ok(())
}
