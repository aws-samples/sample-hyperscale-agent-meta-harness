//! The pull request of a follow-up run: where it points, why, and the words that link it to the pull request of the
//! run it continues.
//!
//! A follow-up run starts from the commit an earlier run delivered (`stage` with `fromRun`), so its branch `harness/<newRunId>` holds that
//! commit and the new work on top of it. Its pull request must show only the new work. Where it points depends on what became of the old run's
//! branch `harness/<oldRunId>`, which the user may have merged, deleted or pushed to since:
//!
//! - the branch still points at the commit the old run delivered: the pull request targets that branch (a stacked pull request);
//! - the old work was merged, or the branch was deleted: it targets the destination branch of the old run's pull request (`main` when the old
//!   run opened none);
//! - the branch is alive and points somewhere else: a delivery conflict. No pull request is opened, because any list of changes against a
//!   moved branch would be wrong. The run still ends with its zip.

use serde::{Deserialize, Serialize};

/// The earlier run a follow-up run continues, as `push-pr` takes it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FollowUp {
    /// The earlier run (a ULID, not the run itself).
    pub run_id: String,
    /// The commit that run delivered, the one the follow-up run started from (40 lowercase hex characters).
    pub head_sha: String,
    /// The console link of the earlier run's pull request, as its `delivery.pr` event kept it; absent when it opened none.
    #[serde(default)]
    pub pull_request_url: Option<String>,
}

/// Why the pull request points where it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Aim {
    /// The old run's branch still holds the commit it delivered.
    OldBranch,
    /// The old run's work is merged.
    Merged,
    /// The old run's branch no longer exists.
    Deleted,
}

/// What the gateway knows about the old run's branch and pull request when the follow-up run delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts<'a> {
    pub old_run: &'a str,
    pub recorded_head: &'a str,
    /// The tip of `refs/heads/harness/<old run>` in the repository; `None` when the branch does not exist.
    pub old_branch_tip: Option<&'a str>,
    /// The destination ref of the old run's pull request, or `refs/heads/main` when it opened none.
    pub destination_ref: &'a str,
    /// The tip of that destination branch; `None` when the branch does not exist.
    pub destination_tip: Option<&'a str>,
    /// The old pull request reports a merge, or the recorded head is in the history of the destination branch.
    pub merged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Open the pull request against `destination_ref`.
    Open { destination_ref: String, aim: Aim },
    /// Open none; the message is the reason the run reports.
    Conflict(String),
}

/// `refs/heads/main` as `main`.
fn branch_of(r: &str) -> &str {
    r.strip_prefix("refs/heads/").unwrap_or(r)
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The decision of the three cases.
///
/// Merged work goes first: once the old run's pull request is merged, a pull request stacked on its branch could only merge into that
/// branch, never into the destination the user chose. A branch that is gone (deleted, or never pushed because no pull request was
/// opened) and merged work both aim at the destination of the old run's pull request, which must still exist. A branch that is alive and
/// holds exactly the delivered commit is stacked on. Any other tip is a conflict.
pub fn decide(f: &Facts<'_>) -> Plan {
    let retarget = |aim: Aim| match f.destination_tip {
        Some(_) => Plan::Open {
            destination_ref: f.destination_ref.to_string(),
            aim,
        },
        None => Plan::Conflict(format!(
            "delivery conflict: the destination branch {} of the pull request of run {} no longer exists in the repository. No pull request was opened. The zip holds this run's code.",
            branch_of(f.destination_ref),
            f.old_run
        )),
    };
    if f.merged {
        return retarget(Aim::Merged);
    }
    match f.old_branch_tip {
        None => retarget(Aim::Deleted),
        Some(tip) if tip == f.recorded_head => Plan::Open {
            destination_ref: format!("refs/heads/harness/{}", f.old_run),
            aim: Aim::OldBranch,
        },
        Some(tip) => Plan::Conflict(format!(
            "delivery conflict: branch harness/{old} was moved after run {old} delivered {head}; it now points at {tip}. No pull request was opened, because its list of changes would be wrong. The zip holds this run's code.",
            old = f.old_run,
            head = short(f.recorded_head),
            tip = short(tip)
        )),
    }
}

/// How the old run's work reached the destination branch, as far as the gateway could tell: the old pull request reports a merge, and/or the
/// recorded commit is in the history of the destination branch. A squash merge reports the first and not the second: the old commits are not in
/// the destination's history, so the new pull request, which is built on them, lists them again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Landed {
    pub pr_merged: bool,
    pub in_history: bool,
}

/// The comment on the new pull request: which run and pull request it continues, where it points and why.
pub fn target_note(
    old_run: &str,
    old_url: Option<&str>,
    destination_ref: &str,
    aim: Aim,
    recorded_head: &str,
    landed: Landed,
) -> String {
    let branch = branch_of(destination_ref);
    let continues = match old_url {
        Some(u) => format!("Follow-up of run {old_run}, which continues its pull request {u}."),
        None => format!("Follow-up of run {old_run}, which opened no pull request."),
    };
    let why = match aim {
        Aim::OldBranch => format!(
            "This pull request targets {branch} because that branch still holds {}, the commit run {old_run} delivered, so the changes listed start at that commit.",
            short(recorded_head)
        ),
        Aim::Merged => match (landed.pr_merged, landed.in_history) {
            (true, false) => format!(
                "The pull request of run {old_run} is merged by a squash: its commits are not in the history of {branch}, so this pull request, which is built on them, targets {branch}, and run {old_run}'s changes are listed again in it. They are already in {branch}."
            ),
            (true, true) => format!(
                "The pull request of run {old_run} is merged and its commits are in the history of {branch}, so this pull request targets {branch}, the branch it was aimed at."
            ),
            _ => format!(
                "The work of run {old_run} is already in {branch}: its commits are in the history of that branch. This pull request targets {branch}, the branch the pull request of run {old_run} was aimed at."
            ),
        },
        Aim::Deleted => format!(
            "Branch harness/{old_run} no longer exists, so this pull request targets {branch}, the branch the pull request of run {old_run} was aimed at. Work of run {old_run} that was never merged is part of this pull request."
        ),
    };
    format!("{continues}\n{why}")
}

/// The comment on the old pull request: the run that continued its work and the link to that run's pull request.
pub fn link_note(new_run: &str, new_url: &str) -> String {
    format!("Run {new_run} continues the work of this pull request. Its pull request: {new_url}")
}

/// What `push-pr` answers for a follow-up run, beside the usual members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowUpOpened {
    /// The ref the new pull request points at.
    pub destination_ref: String,
    pub aim: Aim,
    /// The old run's pull request, when it opened one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_pull_request_id: Option<String>,
    /// The old pull request carries a comment that links to the new one.
    pub old_pull_request_linked: bool,
}
