//! The git settings put back at the end of a session, repeated while the session's storage refuses the write.
//!
//! The end sequence puts `.git/config` and `.git/info/attributes` back as the session found them before agentd runs git in the
//! checkout (`crate::workspace::GitState`). The session's storage can refuse the data of a write with errno 28 while its files leave
//! room, for seconds to about a minute and a half. The write of `.git/config` checked
//! neither the sync nor the close. When it met such a refusal the file was left empty, the comparison with the start of the run
//! failed, and the run ended as a repository that had changed.
//!
//! * **The write is a checked one** (`crate::checked_write::replace`): a refusal is seen and leaves the file as it stood.
//! * **It is repeated while the answer is errno 28**, every [`REPEAT_EVERY`] for up to [`REPEAT_FOR`] ([`put_back`]). Any other
//!   error is not repeated and is what it was before ([`NotPutBack::Other`]). A try that took longer than the pause is followed by
//!   one try at once; the points of the grid it passed are left out, so a storage that refuses writes meets no burst of them.
//! * **Each answer is told to the watch of the session's storage** (`StorageWatch::own_write`, `Seen::GitSettings`): this is a
//!   write the worker makes anyway, like the marker of the copies. A refusal seen here brings the notice, and the first
//!   write the storage takes closes the stretch.
//! * **The copy of the end comes before the wait** (root invariant 9). The repeat can take minutes, and a session can end in it.
//!   So at the first refused write [`put_back`] calls what its caller gave it, before the first pause and before the watch is told
//!   (the watch's first notice of a session measures the files of the storage): the end sequence saves the copy of the end there,
//!   as the tree is.
//! * **The bound is the storage's time.** What the first refusal brought (the copy of the end, the watch's first notice) does not
//!   use it up: the grid of the tries is moved on by the time that took, so the tries after it still stand [`REPEAT_EVERY`] apart
//!   for [`REPEAT_FOR`], and the end's "for 270 s" is true.
//! * **When the bound runs out** agentd runs no git in the checkout, as for a repository it does not trust, and the end of the run
//!   says that the storage refused the write ([`NotPutBack::Refused`], `classify::settings_write_refused`). It never says that the
//!   repository changed.
//!
//! Only the end sequence calls [`put_back`], after the engine is gone: no tool call and no prompt waits for it
//! (`tests/b890_cost.rs`).

use crate::checked_write::FileCalls;
use crate::direct::snapshot::TREE_ANSWER_LIMIT;
use crate::direct::storage_watch::{Seen, StorageWatch};
use crate::logging::LogSink;
use crate::workspace::{GitState, Worktree, WsError};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// How long the write of the git settings is repeated while the storage answers errno 28: three times the longest refusal measured.
/// The longest refusal seen lasted 87 to 89 s, and after the restore of a large repository refusals came again on a grid of about
/// 30.5 s. Both numbers are measurements of 2026-11-21, filed under `evidence/b881-session-storage-round3-2026-11-21/` of the notes
/// repository.
pub const REPEAT_FOR: Duration = Duration::from_secs(270);
/// The pause between two tries: the time one test write of the worker's own is waited for (`crate::direct::storage_watch`,
/// [`TREE_ANSWER_LIMIT`]). That test write has no pace of its own: it is made when something asks for it.
pub const REPEAT_EVERY: Duration = TREE_ANSWER_LIMIT;

/// The worker's log line when the copy of the end was stored before the wait on the storage.
pub const COPIED_BEFORE_WAIT: &str =
    "direct turn git settings: the end state of the work was copied before the wait";
/// The start of the worker's log line when that copy could not be stored; the reason follows.
pub const NOT_COPIED_BEFORE_WAIT: &str =
    "direct turn git settings: the end state of the work was not copied before the wait";

/// How a refused write is repeated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repeat {
    /// The pause between two tries.
    pub every: Duration,
    /// No try starts later than this after the first one. The time of what is done at the first refusal is not counted.
    pub for_at_most: Duration,
}

impl Default for Repeat {
    fn default() -> Self {
        Self {
            every: REPEAT_EVERY,
            for_at_most: REPEAT_FOR,
        }
    }
}

/// Why the git settings were not put back. agentd then runs no git in the checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotPutBack {
    /// The session's storage answered errno 28 to every try until the bound ran out. Nothing says that the repository changed.
    Refused {
        /// The file the last try was for: `.git/config` or `.git/info/attributes`.
        file: String,
        /// How many tries the storage refused.
        tries: u32,
        /// The bound, in seconds.
        secs: u64,
        /// The step of the last try that was refused, with the system's words.
        last: String,
    },
    /// The checkout is not one agentd runs git in: `.git` moved or redirected, a settings file that does not read as it did, or a
    /// file call that failed in another way than errno 28.
    Other(WsError),
}

impl std::fmt::Display for NotPutBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused {
                file,
                tries,
                secs,
                last,
            } => write!(
                f,
                "the session's storage refused the write of {file} for {secs} s ({tries} tries; the last answer was {last})"
            ),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

/// The reason of a checkout agentd runs no git in: the words of [`WsError::Untrusted`] alone, any other error whole.
pub fn reason(e: &WsError) -> String {
    match e {
        WsError::Untrusted(why) => why.clone(),
        other => other.to_string(),
    }
}

fn tries_word(n: u32) -> &'static str {
    if n == 1 { "write" } else { "writes" }
}

/// Puts the git settings of the checkout back as `state` holds them, and repeats the write while the session's storage answers
/// errno 28 (`repeat`). `calls` are the file calls of the write, and `watch` is told each answer. The file calls run on a thread of
/// their own; between two tries this waits without holding anything. `at_first_refusal` is called once, after the first refused
/// write, before the watch is told of it and before the first pause, and never when no write is refused: what must not wait for
/// the storage is done there. The time it takes is not part of the bound.
pub async fn put_back<F>(
    state: &GitState,
    wt: &Worktree,
    calls: Arc<dyn FileCalls>,
    repeat: Repeat,
    log: &Arc<dyn LogSink>,
    watch: Option<&StorageWatch>,
    at_first_refusal: F,
) -> Result<(), NotPutBack>
where
    F: AsyncFnOnce(),
{
    let started = Instant::now();
    // Where the grid of the tries starts: at the first try, moved on by the time the first refusal's own work took.
    let mut grid = started;
    // The point of the grid the next try stands on, in pauses from `grid`.
    let mut point: u32 = 0;
    let mut refused: u32 = 0;
    let mut at_first_refusal = Some(at_first_refusal);
    loop {
        let (state, wt, calls) = (state.clone(), wt.clone(), calls.clone());
        let tried = match tokio::task::spawn_blocking(move || {
            state.restore_with(&wt, calls.as_ref())
        })
        .await
        {
            Ok(done) => done,
            Err(e) => Err(WsError::Io(format!(
                "the write of the git settings did not end: {e}"
            ))),
        };
        match tried {
            Ok(()) => {
                if let Some(watch) = watch {
                    watch.own_write(Seen::GitSettings, false).await;
                }
                // A write that met no refusal logs nothing: the log of such an end is as it was.
                if refused > 0 {
                    log.line(&format!(
                        "direct turn git settings put back after {refused} refused {} ({} s)",
                        tries_word(refused),
                        started.elapsed().as_secs()
                    ));
                }
                return Ok(());
            }
            Err(WsError::StorageRefused { file, step, text }) => {
                refused += 1;
                if refused == 1 {
                    log.line(&format!(
                        "direct turn git settings: the session's storage refused the write of {file} ({step}: {text}); the write is repeated every {} s for up to {} s",
                        repeat.every.as_secs(),
                        repeat.for_at_most.as_secs()
                    ));
                }
                // Root invariant 9 comes first: the copy of the end, and only then the watch, whose first notice of a session
                // measures the files of the storage.
                let before = Instant::now();
                if let Some(first) = at_first_refusal.take() {
                    first().await;
                }
                if let Some(watch) = watch {
                    watch.own_write(Seen::GitSettings, true).await;
                }
                if refused == 1 {
                    // Neither is the storage's time: the bound starts again where they end.
                    grid += before.elapsed();
                }
                // The tries stand on a grid from the first one, `every` apart. A try that took longer than that is followed by one
                // try at once, and the points of the grid it passed are left out: they are not made up for one after the other.
                let now = Instant::now();
                let passed =
                    now.saturating_duration_since(grid).as_nanos() / repeat.every.as_nanos().max(1);
                point = (point + 1).max(u32::try_from(passed).unwrap_or(u32::MAX));
                let next = (grid + repeat.every * point).max(now);
                if next > grid + repeat.for_at_most {
                    let secs = repeat.for_at_most.as_secs();
                    log.line(&format!(
                        "direct turn git settings not put back: the session's storage refused the write of {file} for {secs} s ({refused} tries); agentd runs no git in the checkout"
                    ));
                    return Err(NotPutBack::Refused {
                        file,
                        tries: refused,
                        secs,
                        last: format!("{step}: {text}"),
                    });
                }
                tokio::time::sleep_until(next).await;
            }
            Err(e) => {
                log.line(&format!("direct turn repository not trusted: {e}"));
                return Err(NotPutBack::Other(e));
            }
        }
    }
}
