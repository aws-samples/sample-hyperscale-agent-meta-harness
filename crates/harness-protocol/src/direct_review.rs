//! The review round of a direct run.
//!
//! When Cline ends its turn with a message and the harness counted at least one changed file, agentd sends ONE more prompt in the same
//! session: the review prompt. It asks Cline to look over its own delivery as the maintainer who must merge it. The prompt is the user's
//! own file (`~/.config/ham/review.md`, written by `ham rules init` with the default below); `ham` sends its body with the run unread, as it
//! sends `continue.md`. The harness fills in `{{base}}` (the commit the run started from) and `{{facts}}` (lines it computed from the work
//! tree), sends the prompt once, accepts the turn that follows under the normal rules and records the round in the summary
//! ([`DirectReview`]). The label of the run does not change because of the round: it is still "Cline reported done" only when Cline ends its
//! turn with a message and the tree has a changed file, and a round that fails leaves the outcome the turn before it had.
//!
//! `StartDirectRunRequest.review` ([`ReviewOptions`]) is the user's switch and file. Absent means the stage default, which is on (`auto`);
//! `ham` sends the member only when the user switched the round off (`run.review_round = off`) or has a review file. [`check_review`] is the
//! ONE check: `ham` (before a run starts), control-api (400 `invalid-review`) and agentd (the run ends `stopped_setup_failed` before any model
//! call) call it, and its messages never repeat a value.
//!
//! Everything here is re-exported from [`crate::direct`].

use crate::direct_continue::is_commit_id;
use crate::direct_goal::GoalRange;
use crate::direct_skills::sha256_hex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// `guardS`: the least time, in seconds, a run must have left for the round to start (stage `AGENTD_DIRECT_REVIEW_GUARD_S`). The default is
/// 20 minutes. `ham` takes it from the `guard_minutes` key of `review.md`.
pub const REVIEW_GUARD_S: GoalRange = GoalRange {
    default: 1200,
    min: 60,
    max: 28_800,
};

/// The default review prompt: the six points of the review as a maintainer, for any repository and language. `{{base}}` and `{{facts}}` are
/// filled in by agentd. `ham rules init` writes it to `review.md`; agentd uses it when the request carries no text.
pub const DEFAULT_REVIEW_PROMPT: &str = "Review this delivery as the maintainer who must merge it, and assume it has defects. BASE is the commit you started from: {{base}}.
Facts the harness computed:
{{facts}}
1 Scope: run git status --porcelain and git diff --stat BASE. Remove what the task did not need: scratch files, new top-level folders and unrelated edits. Leave the lifecycle records under .ham/ as they are, because the harness reads them; remove them only when the repository's own checks reject them.
2 Other users: for each changed file, symbol and claim, git grep -n its name and its old wording, and fix stale copies. If a script gained an import or a dependency, find the other scripts, build files and fixtures that use it and run their tests.
3 Clean base: make a scratch worktree of BASE outside the repository (git worktree add --detach \"$(mktemp -d)/base\" BASE), run the repository's own checks there and here, and compare the NAMES of the failing tests and any build or setup error, never counts and never git stash. A failure here that is not there is yours.
4 Red first: in a second scratch worktree that holds your work (HEAD plus any change you have not committed) put the non-test files back to BASE and run the new tests. They must fail on an assertion about the reported problem, not on an import, a setup or an environment variable; try the relevant variables set and unset.
5 Every rewritten line needs a test that fails when it is reverted.
6 Claims: list each claim in your commit messages, docs and last message and prove it with a command you ran now, or remove it.
Time box: about 15 minutes. Run the checks only for the modules you touched and for the scripts, tests and callers that use them; do not build or test the whole workspace of a large project. What you could not run in that time goes into your final message as a NOT RUN line.
Scratch worktrees live outside the repository: remove them (git worktree remove, then git worktree prune) when you finish. Fix what you find, rerun the checks, commit in new commits (do not rewrite commits that exist), and give the final message stating only what you ran and saw.
";

/// The token the harness replaces with the commit the run started from.
pub const REVIEW_TOKEN_BASE: &str = "{{base}}";
/// The token the harness replaces with the lines it computed from the work tree.
pub const REVIEW_TOKEN_FACTS: &str = "{{facts}}";

/// `run.review_round`: whether the round runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    /// The round runs under the conditions above. The default.
    Auto,
    /// No round.
    Off,
}

impl ReviewMode {
    /// The word of the setting and of the wire.
    pub fn word(self) -> &'static str {
        match self {
            ReviewMode::Auto => "auto",
            ReviewMode::Off => "off",
        }
    }

    /// The mode a word names; `None` for anything else.
    pub fn parse(word: &str) -> Option<ReviewMode> {
        match word {
            "auto" => Some(ReviewMode::Auto),
            "off" => Some(ReviewMode::Off),
            _ => None,
        }
    }
}

/// `StartDirectRunRequest.review` (protocol 1.44.0): the user's switch for the review round and, when the user has a review file, its body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewOptions {
    pub mode: ReviewMode,
    /// The body of `~/.config/ham/review.md`, with `{{base}}` and `{{facts}}` left as typed. Absent: agentd uses [`DEFAULT_REVIEW_PROMPT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// SHA-256 of `text` (lowercase hex); present exactly when `text` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Seconds the run must have left for the round to start (60 to 28,800); absent: the stage's own, 20 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_s: Option<u32>,
}

impl ReviewOptions {
    /// The member that switches the round off.
    pub fn off() -> ReviewOptions {
        ReviewOptions {
            mode: ReviewMode::Off,
            text: None,
            sha256: None,
            guard_s: None,
        }
    }

    /// The member of a user who has a review file: the round on, the body and its hash, and the guard when the file names one.
    pub fn with_text(text: &str, guard_s: Option<u32>) -> ReviewOptions {
        ReviewOptions {
            mode: ReviewMode::Auto,
            text: Some(text.to_string()),
            sha256: Some(sha256_hex(text.as_bytes())),
            guard_s,
        }
    }
}

/// Why a `review` member was refused. The messages name the member and the rule and never a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewProblem {
    /// `text` is empty or only white space.
    TextEmpty,
    /// `text` holds a NUL byte.
    TextNul,
    /// `sha256` is missing, is there without `text`, or is not the hash of `text`.
    Sha,
    /// `guardS` is outside [`REVIEW_GUARD_S`].
    Guard,
}

impl ReviewProblem {
    /// The member's name on the wire.
    pub fn key(self) -> &'static str {
        match self {
            ReviewProblem::TextEmpty | ReviewProblem::TextNul => "text",
            ReviewProblem::Sha => "sha256",
            ReviewProblem::Guard => "guardS",
        }
    }

    /// A stable word for the kind of problem.
    pub fn code(self) -> &'static str {
        match self {
            ReviewProblem::TextEmpty => "empty",
            ReviewProblem::TextNul => "nul",
            ReviewProblem::Sha => "sha_mismatch",
            ReviewProblem::Guard => "out_of_range",
        }
    }
}

impl fmt::Display for ReviewProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewProblem::TextEmpty => write!(f, "review: text must not be empty"),
            ReviewProblem::TextNul => write!(f, "review: text must not hold a NUL byte"),
            ReviewProblem::Sha => write!(
                f,
                "review: sha256 must be the SHA-256 of text, and is sent only with text"
            ),
            ReviewProblem::Guard => write!(
                f,
                "review: guardS must be a whole number from {} to {}",
                REVIEW_GUARD_S.min, REVIEW_GUARD_S.max
            ),
        }
    }
}

impl std::error::Error for ReviewProblem {}

/// The ONE check of the `review` member. Pure; the first problem in the order `text`, `sha256`, `guardS` is returned. A member that switches
/// the round off is checked like any other: a hand-made request that could not run with the round on is no request to store.
pub fn check_review(r: &ReviewOptions) -> Result<(), ReviewProblem> {
    match (&r.text, &r.sha256) {
        (Some(text), sha) => {
            if text.trim().is_empty() {
                return Err(ReviewProblem::TextEmpty);
            }
            if text.contains('\0') {
                return Err(ReviewProblem::TextNul);
            }
            match sha {
                Some(s) if s.eq_ignore_ascii_case(&sha256_hex(text.as_bytes())) => {}
                _ => return Err(ReviewProblem::Sha),
            }
        }
        (None, Some(_)) => return Err(ReviewProblem::Sha),
        (None, None) => {}
    }
    if let Some(g) = r.guard_s
        && !REVIEW_GUARD_S.contains(g)
    {
        return Err(ReviewProblem::Guard);
    }
    Ok(())
}

// ---- summary: what the round did ------------------------------------------------------------------------------------------------

/// `DirectReview.status` of a round that was sent.
pub const REVIEW_RAN: &str = "ran";
/// `DirectReview.status` of a round that was not sent.
pub const REVIEW_SKIPPED: &str = "skipped";

/// The reasons a round is skipped, as `DirectReview.reason` words (a reader older than the writer must accept a new word, so the field is text).
pub const REVIEW_SKIP_OFF: &str = "off";
pub const REVIEW_SKIP_FORCED_STOP: &str = "forced_stop";
pub const REVIEW_SKIP_CANCELLED: &str = "cancelled";
pub const REVIEW_SKIP_NOT_DONE: &str = "not_done";
pub const REVIEW_SKIP_BLOCKED: &str = "blocked";
pub const REVIEW_SKIP_QUESTION: &str = "question";
pub const REVIEW_SKIP_NO_CHANGED_FILE: &str = "no_changed_file";
pub const REVIEW_SKIP_FILES_UNKNOWN: &str = "files_unknown";
pub const REVIEW_SKIP_NO_TIME: &str = "no_time";

/// `DirectReview.result` of a round whose turn ended with a message from Cline.
pub const REVIEW_COMPLETED: &str = "completed";
/// `DirectReview.result` of a round whose turn ended any other way: the run keeps the outcome the turn before the round had.
pub const REVIEW_FAILED: &str = "failed";
/// `DirectReview.result` of a round the user's cancel ended: the run ends as the user's stop.
pub const REVIEW_CANCELLED: &str = "cancelled";

/// What the review round did in a run (protocol 1.44.0), in `summary.json` and in the evidence. A fact the harness read, never a claim about the
/// work: the round asks Cline to look again, and the harness makes no judgment of the result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectReview {
    /// [`REVIEW_RAN`] or [`REVIEW_SKIPPED`].
    pub status: String,
    /// A skipped round: why (`REVIEW_SKIP_*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The commit `HEAD` pointed to when the round began (40 or 64 lowercase hex digits); left out when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_before: Option<String>,
    /// The commit `HEAD` pointed to when the round ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_after: Option<String>,
    /// Whether the round changed any file (the work tree after differs from the work tree before); left out when the harness could not tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<bool>,
    /// How long the round took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
    /// Input tokens the round used, when the engine reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens the round used, when the engine reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// A round that ran: [`REVIEW_COMPLETED`], [`REVIEW_FAILED`] or [`REVIEW_CANCELLED`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// A word of the summary: 1 to 32 lowercase letters, digits or underscores.
fn plain_word(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl DirectReview {
    /// A round that was not sent, with the reason.
    pub fn skipped(reason: &str) -> DirectReview {
        DirectReview {
            status: REVIEW_SKIPPED.to_string(),
            reason: Some(reason.to_string()),
            commit_before: None,
            commit_after: None,
            changed_files: None,
            seconds: None,
            input_tokens: None,
            output_tokens: None,
            result: None,
        }
    }

    /// The record holds no more than the rules allow: plain words, and commit ids in the form git writes them.
    pub fn is_valid(&self) -> bool {
        plain_word(&self.status)
            && self.reason.as_deref().is_none_or(plain_word)
            && self.result.as_deref().is_none_or(plain_word)
            && [&self.commit_before, &self.commit_after]
                .into_iter()
                .flatten()
                .all(|c| is_commit_id(c) || is_long_commit_id(c))
    }

    /// The one or two sentences EVIDENCE.md and the card carry. They say what the round was and what it did, and claim nothing about the work:
    /// no word of [`crate::direct::BANNED_CLAIM_WORDS`].
    pub fn sentence(&self) -> String {
        self.sentence_for("Cline")
    }

    /// [`sentence`](Self::sentence) for a run of the engine `name`. For `Cline` it is the same text, byte for byte.
    pub fn sentence_for(&self, name: &str) -> String {
        if self.status != REVIEW_RAN {
            let why = match self.reason.as_deref() {
                Some(REVIEW_SKIP_OFF) => "you switched the round off".to_string(),
                Some(REVIEW_SKIP_FORCED_STOP) => "the harness ended the run".to_string(),
                Some(REVIEW_SKIP_CANCELLED) => "you cancelled the run".to_string(),
                Some(REVIEW_SKIP_NOT_DONE) => {
                    format!("{name}'s last turn did not end with a message")
                }
                Some(REVIEW_SKIP_BLOCKED) => {
                    format!("{name} ended its message with a BLOCKED line")
                }
                Some(REVIEW_SKIP_QUESTION) => {
                    format!("{name} ended its message with a question for you")
                }
                Some(REVIEW_SKIP_NO_CHANGED_FILE) => "no file had changed".to_string(),
                Some(REVIEW_SKIP_FILES_UNKNOWN) => {
                    "the harness could not count the changed files".to_string()
                }
                Some(REVIEW_SKIP_NO_TIME) => "the run had too little time left".to_string(),
                _ => "the round did not apply".to_string(),
            };
            return format!("The harness did not ask {name} to review its delivery: {why}.");
        }
        let mut s = format!(
            "The harness asked {name} once to review its own delivery as the maintainer who must merge it."
        );
        let mut facts: Vec<String> = Vec::new();
        if let Some(sec) = self.seconds {
            facts.push(format!("It took {sec} s"));
        }
        match (self.input_tokens, self.output_tokens) {
            (Some(i), Some(o)) => facts.push(format!("it used {i} input and {o} output tokens")),
            (Some(i), None) => facts.push(format!("it used {i} input tokens")),
            (None, Some(o)) => facts.push(format!("it used {o} output tokens")),
            (None, None) => {}
        }
        if !facts.is_empty() {
            s.push(' ');
            s.push_str(&facts.join(", "));
            s.push('.');
        }
        match self.changed_files {
            Some(true) => s.push_str(" The round changed files."),
            Some(false) => s.push_str(" The round changed no file."),
            None => {}
        }
        if let (Some(b), Some(a)) = (&self.commit_before, &self.commit_after) {
            let short = |c: &str| c.chars().take(12).collect::<String>();
            if b == a {
                s.push_str(&format!(" The head commit stayed {}.", short(b)));
            } else {
                s.push_str(&format!(
                    " The head commit went from {} to {}.",
                    short(b),
                    short(a)
                ));
            }
        }
        match self.result.as_deref() {
            Some(REVIEW_FAILED) => {
                s.push_str(&format!(
                    " The round ended without a message from {name}, so the outcome of the run is the one the turn before it had."
                ));
                if self.changed_files == Some(true) {
                    s.push_str(" The last message of the run is from before the round, and the round had changed files.");
                }
            }
            Some(REVIEW_CANCELLED) => s.push_str(" You cancelled the run during the round."),
            _ => {}
        }
        s.push_str(" The harness made no judgment of the result.");
        s
    }
}

fn is_long_commit_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
