//! The flow a run declared.
//!
//! The lifecycle contract asks a run to end the message of its final commit with a `Flow:` trailer. At the end of the run agentd reads that
//! message and records the word (`quick`, `full`, or `none` when the run declared neither) in `summary.json`, from where it reaches
//! `evidence/direct.json`, EVIDENCE.md and the delivery card. It is a fact recorded beside the label: nothing here changes an outcome, a
//! label, a check or a setting, and a commit that cannot be read leaves the last message in charge.
//!
//! The final task commit is the commit the engine left last, read before agentd's own end-of-run commit is made. The commit the run started
//! from is not one: a repository's earlier history never counts as the run's declaration.

use crate::workspace::{self, Worktree};
use harness_protocol::direct::{DirectFlow, tail_chars};

/// The most characters of a commit message that are read: the trailer block is at its end.
const MESSAGE_TAIL_CHARS: usize = 16_384;

/// The flow of a run: the trailer of the message of `task_head` (the engine's last commit; `None` when it could not be read), unless that
/// commit is `base` (the run made no commit), else the first line of `last_message`, else `none`.
pub async fn of_run(
    wt: &Worktree,
    task_head: Option<&str>,
    base: Option<&str>,
    last_message: &str,
) -> DirectFlow {
    let message = match task_head.filter(|h| Some(*h) != base) {
        Some(sha) => workspace::commit_message(wt, sha).await.ok(),
        None => None,
    };
    let tail = message
        .as_deref()
        .map(|m| tail_chars(m, MESSAGE_TAIL_CHARS));
    DirectFlow::read(tail, Some(last_message))
}
