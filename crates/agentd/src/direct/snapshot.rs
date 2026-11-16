//! The copies of the work. While the prompt runs agentd saves the tree when it changed: every
//! `snapshot_every_s` seconds and after a tool call ([`Cadence`]), so that a microVM that dies still delivers the work so far. When the
//! engine has stopped it saves the end state once more ([`Snapshotter::final_copy`]), before anything can refuse a delivery.
//!
//! No look of the copy task is waited for without a limit. A session that was stopped from outside lived on for a minute and stored
//! no copy, because one look never returned and the task waited for it. Every git call of a look and every file call it makes on the work
//! tree ends when the look is given up ([`GiveUp`], [`ScratchRepo::on_tree`]); the copy task asks whether the file system of the work
//! tree answers before a look and while its git works ([`TreeAlive`]), and gives the look up when it does not; the git work of a look has a
//! limit that follows the last stored copy ([`git_limit`]); storing a copy has its own limit ([`store_limit`]); and the time a copy names
//! is from before its tree was read ([`cut_unix`]).
//!
//! The snapshot never touches the engine's repository: no index, no HEAD, no ref and no object file is written there (a
//! root-owned object directory would stop the engine's own `git commit`). It lives in a bare repository of its own,
//! `<agent_data>/snapshot.git` (root, mode 0700), whose `objects/info/alternates` names the repository's object store
//! ([`ScratchRepo`]). Each snapshot is `git add -A` into that repository's own index (seeded with the base tree, so a file
//! tracked at the base that an ignore rule now matches is not read as a deletion), `write-tree`, `commit-tree` on top of the
//! repository's HEAD, `update-ref refs/heads/snapshot` and a bundle of that ref. The tree is compared with the last
//! snapshot's,
//! so an idle session makes none. The copies are numbered within the run: a later session goes on counting ([`Snapshotter::count_from`]).
//! When the engine removed or replaced `.git`, its HEAD is not read and
//! neither its commits nor the base can be read through the alternate: the copy is then a commit with no parent of the tree hashed into a
//! new index ([`ScratchRepo::whole_tree`]), which holds the whole tree itself.
//!
//! The repository keeps what its last copy needs and no more ([`Snapshotter::drop_old_versions`]): before the look that follows one that
//! hashed a changed tree, and before the copy of the end, `git prune` removes the commits of earlier copies and the versions of files that
//! changed again, and no reflog keeps them. Without it a file rewritten before every copy left one version per copy on the worker's disk.
//!
//! agentd reads engine-controlled files here as root: the repository's `info/exclude` is opened without following a link and
//! without blocking, and only when it is a regular file of at most 1 MiB.

use crate::BoxFuture;
use crate::direct::classify::scrub_detail;
use crate::direct::input::{ChangedFiles, DigestSources, TreeSource};
use crate::time::now_unix;
use crate::workspace::{
    WsError, git_dir_problem, git_with_env, git_with_env_outside, git_with_env_until,
};
use rustix::fs::{FileType, Mode, OFlags, fstat, open};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The most bytes read from an engine-controlled file.
const MAX_CONTROLLED_FILE: u64 = 1 << 20;

/// The index of a copy that holds the whole tree ([`ScratchRepo::whole_tree`]), made new for each such copy.
const WHOLE_INDEX: &str = "index-whole";

/// One snapshot, ready to upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMade {
    pub commit: String,
    pub base: String,
    pub bundle: PathBuf,
    /// The number of the copy within the run: 1 for the first copy of the run, and a later session goes on counting.
    pub count: u32,
    /// The copy of the session's end ([`Snapshotter::final_copy`]); `snapshot.json` then carries `"final": true`.
    pub final_copy: bool,
    /// The size of the bundle in bytes.
    pub bundle_bytes: u64,
    /// The wall-clock second in which the look began to read the tree. The time the copy names is the second before it
    /// ([`cut_unix`]).
    pub read_unix: i64,
}

/// A look that has run this long without storing its copy is said to the run, once per streak of trouble ([`late_after`]).
pub const COPY_LATE_AFTER: Duration = Duration::from_secs(20);
/// A look is late only when it has also run this many times as long as the last look that stored a copy, so a repository whose
/// copies always take long is told once, not after every copy.
pub const COPY_LATE_FACTOR: u32 = 3;
/// The git work of one look is given up after this long at least, whatever the file system of the work tree answers ([`git_limit`]).
/// A storage can go on answering the question [`TreeAlive`] asks while a file read of git hangs: such a look ends here.
pub const COPY_GIT_FLOOR: Duration = Duration::from_secs(300);
/// The git work of a look has this many times what the git work of the last look that stored a copy took, when that is longer than
/// [`COPY_GIT_FLOOR`]: a repository whose copies take minutes keeps its copies.
pub const COPY_GIT_FACTOR: u32 = 10;
/// The git work of one look never has longer than this. A look that its own limit gave up has twice the time at the next try, up to
/// this ([`git_limit`]).
pub const COPY_GIT_LIMIT: Duration = Duration::from_secs(3_600);
/// While the git of a look works, the file system of the work tree is asked this often whether it still answers ([`TreeAlive`]).
pub const TREE_ASK_EVERY: Duration = Duration::from_secs(20);
/// Storing one copy (the credentials, both uploads and their tries) has this long, and one second more for each
/// [`COPY_STORE_BYTES_PER_S`] bytes of its bundle ([`store_limit`]).
pub const COPY_STORE_LIMIT: Duration = Duration::from_secs(30);
/// The slowest upload a copy is given time for (1 MiB a second).
pub const COPY_STORE_BYTES_PER_S: u64 = 1024 * 1024;
/// How long the file system of the work tree has to answer when the copy task asks it ([`TreeAlive::answers`]).
pub const TREE_ANSWER_LIMIT: Duration = Duration::from_secs(5);
/// When agentd is asked to end, who asked waits this long for the copy of the work as it is. In a process that ends after the copy,
/// a look in flight and the look that follows it end within it; the first process of its machine runs on, and its looks keep their own
/// limits.
pub const STOP_COPY_BUDGET: Duration = Duration::from_secs(30);
/// When the copy that a request to end asked for could not be saved and the process runs on, the copy task looks again this long
/// after the look that failed, until a copy is stored ([`CopyTimer::try_again`]).
pub const STOP_COPY_RETRY: Duration = Duration::from_secs(5);

/// How long a look may run before the run is told that its copy is late: `base` ([`COPY_LATE_AFTER`]), or [`COPY_LATE_FACTOR`]
/// times what the last look that stored a copy took (`last`), when that is longer.
pub fn late_after(base: Duration, last: Option<Duration>) -> Duration {
    last.map_or(base, |took| base.max(took * COPY_LATE_FACTOR))
}

/// How long the git work of a look has before it is given up: [`COPY_GIT_FLOOR`], or [`COPY_GIT_FACTOR`] times what the git work of
/// the last look that stored a copy took (`last`) when that is longer; twice that for each look in a row that this limit gave up
/// (`given_up`), so a copy that needs longer than its limit is made at a later try; never longer than [`COPY_GIT_LIMIT`].
pub fn git_limit(last: Option<Duration>, given_up: u32) -> Duration {
    let base = last.map_or(COPY_GIT_FLOOR, |took| {
        COPY_GIT_FLOOR.max(took.saturating_mul(COPY_GIT_FACTOR))
    });
    base.saturating_mul(1_u32.checked_shl(given_up).unwrap_or(u32::MAX))
        .min(COPY_GIT_LIMIT)
}

/// What the copy task keeps for [`git_limit`]: what the git work of the last look that stored a copy took, and how many looks in a
/// row their own git limit gave up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitBudget {
    last: Option<Duration>,
    given_up: u32,
}

impl GitBudget {
    /// How long the git work of the next look has.
    pub fn limit(&self) -> Duration {
        git_limit(self.last, self.given_up)
    }

    /// A look stored a copy whose git work took `git_took`.
    pub fn stored(&mut self, git_took: Duration) {
        self.last = Some(git_took);
        self.given_up = 0;
    }

    /// The git work of a look ended within its limit, and no copy was stored (the tree had not changed, or the copy could not be stored).
    pub fn ended(&mut self) {
        self.given_up = 0;
    }

    /// The git work of a look was given up at its own limit.
    pub fn gave_up(&mut self) {
        self.given_up = self.given_up.saturating_add(1);
    }
}

/// Whether a look that failed was given up by its own git limit: the limit has passed (`past_limit`), no silent tree ended the
/// git work first (`tree_silent`), and the git work had not ended (`git_ended`). A look whose git ended in time and whose copy could not
/// be stored afterwards failed for the store: its git limit gave nothing up, however long the store took, and the next look keeps the
/// limit ([`GitBudget::ended`]).
pub fn git_limit_gave_up(tree_silent: bool, git_ended: bool, past_limit: bool) -> bool {
    !tree_silent && !git_ended && past_limit
}

/// The timer of the copy task. It counts from the start of one look to the start of the next. A look that a request to end asked
/// for leaves the tick that was due where it is, so the copy of a stop never moves the next copy back; and after a copy of a stop that
/// could not be saved, a process that runs on looks again soon ([`STOP_COPY_RETRY`]). After any look that failed the tick of the next
/// one is due [`COPY_RETRY`] after that look started at the latest, whatever the timer.
#[derive(Clone, Copy, Debug)]
pub struct CopyTimer {
    every: Option<Duration>,
    next: Option<tokio::time::Instant>,
}

impl CopyTimer {
    /// A timer of `every` that starts at `now`; `None` when the invocation switched the copies off: no tick comes.
    pub fn new(every: Option<Duration>, now: tokio::time::Instant) -> Self {
        Self {
            every,
            next: every.map(|every| now + every),
        }
    }

    /// When the next look is due; `None`: none is.
    pub fn next(&self) -> Option<tokio::time::Instant> {
        self.next
    }

    /// A look starts at `now`: the timer is armed again. When a request to end asked for the look (`asked_to_end`), a tick that is still
    /// to come stays where it is.
    pub fn look_starts(&mut self, now: tokio::time::Instant, asked_to_end: bool) {
        if !asked_to_end || self.next.is_none_or(|next| next <= now) {
            self.next = self.every.map(|every| now + every);
        }
    }

    /// The next look is due `retry` after `from`, or at the tick of the timer when that comes sooner. `from` is the moment a copy of a
    /// stop failed ([`STOP_COPY_RETRY`]), or the start of a look that failed.
    pub fn try_again(&mut self, from: tokio::time::Instant, retry: Duration) {
        let at = from + retry;
        self.next = Some(self.next.map_or(at, |next| next.min(at)));
    }
}

/// What [`TreeAlive`] asks a work tree: `Ok` when the file system answered, else why not.
pub type TreeQuestion = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// The question [`TreeAlive`] asks. One `statvfs`, which asks the server of a network mount each time (file attributes can come from
/// a cache; this does not), and one read of a file every copy needs, the repository's `.git/HEAD`: a storage can answer the first while a
/// file read hangs. A file that is not there, or that is no regular file, is an answer too (the engine may have removed `.git`).
pub fn ask_tree(repo: &Path) -> Result<(), String> {
    rustix::fs::statvfs(repo)
        .map(|_| ())
        .map_err(|e| e.to_string())?;
    let _ = read_regular_nofollow(&repo.join(".git/HEAD"));
    Ok(())
}

/// Asks the file system of a work tree whether it answers ([`ask_tree`]). The question is asked on a thread of its own and waited for
/// at most the limit: on a mount whose server stopped answering it never returns and cannot be ended. While one such question has not
/// returned, the tree counts as silent and no further one is asked, so a tree that stays silent costs one thread.
///
/// A tree that answers can still stop answering in the next moment, and a server can answer this question while another call of it hangs:
/// the limits of the look hold then ([`git_limit`], [`store_limit`]).
#[derive(Clone)]
pub struct TreeAlive {
    repo: PathBuf,
    asking: Arc<AtomicBool>,
    question: TreeQuestion,
}

impl std::fmt::Debug for TreeAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeAlive")
            .field("repo", &self.repo)
            .field("asking", &self.asking)
            .finish_non_exhaustive()
    }
}

impl TreeAlive {
    pub fn new(repo: &Path) -> Self {
        Self::with_question(repo, Arc::new(ask_tree))
    }

    /// [`Self::new`] with another question (tests: a file system that does not answer).
    pub fn with_question(repo: &Path, question: TreeQuestion) -> Self {
        Self {
            repo: repo.to_path_buf(),
            asking: Arc::new(AtomicBool::new(false)),
            question,
        }
    }

    /// `Ok` when the file system of the tree answered within `limit`; else why not, in words for the log.
    pub async fn answers(&self, limit: Duration) -> Result<(), String> {
        if self.asking.swap(true, Ordering::SeqCst) {
            return Err("the work tree has not answered an earlier question".to_string());
        }
        let repo = self.repo.clone();
        let asking = self.asking.clone();
        let question = self.question.clone();
        let asked = tokio::task::spawn_blocking(move || {
            let answer = question(&repo);
            asking.store(false, Ordering::SeqCst);
            answer
        });
        match tokio::time::timeout(limit, asked).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(format!("the work tree: {e}")),
            Ok(Err(e)) => {
                // The question ended in a panic: it is not outstanding.
                self.asking.store(false, Ordering::SeqCst);
                Err(format!("the work tree: {e}"))
            }
            Err(_) => Err(format!(
                "the work tree did not answer within {} s",
                limit.as_secs_f32()
            )),
        }
    }
}

/// The longest storing a copy whose bundle has `bundle_bytes` bytes may take.
pub fn store_limit(bundle_bytes: u64) -> Duration {
    COPY_STORE_LIMIT + Duration::from_secs(bundle_bytes / COPY_STORE_BYTES_PER_S)
}

/// The time a copy names, for a look that began to read the tree in the second `read_unix`: the second before it. Every change made
/// up to and including the named second is in the copy, because the reading began after that second had ended. A change made in the second
/// of the reading itself may be in the copy or not, so the copy does not name that second: the client says "changes after <time> are not in
/// it", and a file written in the named second must never be missing. (Before, the copy named the second in which its upload had ended.)
pub fn cut_unix(read_unix: i64) -> i64 {
    read_unix - 1
}

/// When a look of the copy task gives up. The task sets the time before the look starts and may bring it forward while the look runs
/// (agentd was asked to end). Every git call of the look and every file call it makes on the work tree ends when that time comes, so a work
/// tree that stops answering cannot hold the copy task for good.
#[derive(Clone, Debug)]
pub struct GiveUp {
    at: Arc<tokio::sync::watch::Sender<Option<tokio::time::Instant>>>,
}

impl GiveUp {
    /// Gives up at `at`; `None` never does, until [`Self::no_later_than`] names a time.
    pub fn at(at: Option<tokio::time::Instant>) -> Self {
        Self {
            at: Arc::new(tokio::sync::watch::Sender::new(at)),
        }
    }

    /// Brings the time forward to `at`, when it is later than that or when there is none.
    pub fn no_later_than(&self, at: tokio::time::Instant) {
        self.at.send_if_modified(|now| match now {
            Some(t) if *t <= at => false,
            _ => {
                *now = Some(at);
                true
            }
        });
    }

    /// Returns when the time to give up has come; never while there is none.
    pub async fn reached(&self) {
        let mut seen = self.at.subscribe();
        loop {
            let at = *seen.borrow_and_update();
            match at {
                Some(at) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(at) => return,
                        changed = seen.changed() => {
                            if changed.is_err() {
                                tokio::time::sleep_until(at).await;
                                return;
                            }
                        }
                    }
                }
                None => {
                    if seen.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }

    /// The answer of `work`, or `None` when the time to give up came first.
    pub async fn run<F: std::future::Future>(&self, work: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            out = work => Some(out),
            () = self.reached() => None,
        }
    }
}

/// A copy that a tool call asked for starts no sooner than this after the last look at the tree started: the default timer, which
/// is also the shortest timer a run may name. Under the default timer the tick comes as soon, so a tool call brings a look forward only
/// under a timer the user lengthened.
pub const COPY_FLOOR: Duration = Duration::from_secs(15);
/// After a look whose git work took D the next one starts no sooner than this many times D after that work ended, so that the
/// looks take a tenth of the session's time at the most ([`Cadence::delay`] has the formula and its numbers). The uploads are not counted
/// ([`CopyClock::looked`]); a look that failed counts with the time its git work took ([`CopyClock::failed`]).
pub const COPY_SPACING: u32 = 9;
/// A timer that a test set (`AGENTD_DIRECT_SNAPSHOT_EVERY_MS`) caps the spacing at this many of its own periods
/// ([`Cadence::with_space_max`]), as every timer did while the default timer was one minute, so that the whole-turn tests keep their
/// clocks. No run has it: the cap of a run is [`COPY_SPACING_MAX`].
pub const COPY_SPACING_CAP: u32 = 5;
/// The most the spacing holds a look back, whatever the timer of the run: five minutes, so that one slow look cannot leave
/// the work without a look for long. A user lengthens the timer (`run.copy_every_seconds`) to store fewer copies, and the timer alone
/// does that. While the default timer was one minute the cap was also five timer periods. At a timer of 15 s that would be 75 s: a look
/// of 30 s would come back after 75 s and take 29% of the time, and one of 120 s 62%. Five minutes keep the tenth for every look of at
/// most 33 s (a look of 120 s takes 29%).
pub const COPY_SPACING_MAX: Duration = Duration::from_secs(300);
/// A look that failed is tried again this long after it started, whatever the timer of the run: the default timer.
/// The run was told "ham keeps trying", and a longer timer
/// saves nothing on a copy that was not stored.
pub const COPY_RETRY: Duration = Duration::from_secs(15);
/// A tool call wakes the copy task only while the last bundle was at most this many bytes. A repository with a long history (its
/// bundle holds the whole history each time) stays on the timer.
pub const WAKE_MAX_BUNDLE_BYTES: u64 = 8 * 1024 * 1024;

/// When the copy task looked at the tree last, what that look cost, and how big the last bundle was.
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyClock {
    /// When the last look started and when its git work ended.
    last: Option<(Instant, Instant)>,
    bundle_bytes: Option<u64>,
}

impl CopyClock {
    /// A look started at `started` and its git work ended at `git_ended`: the tree was read and, when it had changed, the bundle was
    /// written. The spacing counts this time alone. The uploads that follow are not counted, so a request to the bucket that hangs does
    /// not hold the next look back. `bundle_bytes` is the size of the bundle the look stored; `None` when the tree had not changed, which
    /// keeps the size of the bundle before.
    pub fn looked(&mut self, started: Instant, git_ended: Instant, bundle_bytes: Option<u64>) {
        self.last = Some((started, git_ended));
        if bundle_bytes.is_some() {
            self.bundle_bytes = bundle_bytes;
        }
    }

    /// A look could not make or store its copy, and what the spacing counts of it ran from `started` to `ended` ([`failed_look_counts`]):
    /// its git work, to the end of that work or to the moment the git was given up; a look that never started git counts as one of no
    /// length. It counts as a look that stored a copy does: a look whose git was given up after five minutes is not started
    /// again at once, and a look whose store failed after a short git work, or that ran no git at all, holds nothing back. The next look
    /// is the next tick of the timer, which comes [`COPY_RETRY`] after the start of this
    /// one at the latest ([`Cadence::period`]), or a tool call after the floor, and then waits for the spacing ([`Cadence::delay`]). The
    /// size of the bundle before is kept.
    pub fn failed(&mut self, started: Instant, ended: Instant) {
        self.last = Some((started, ended.max(started)));
    }
}

/// What the spacing counts of a look that failed ([`CopyClock::failed`]): its git work, from when that work began (`git_started`)
/// to when it ended (`git_ended`), or to `now` when it did not end (the git was given up). A look that never started git (`git_started` is
/// `None`: the file system of the work tree did not answer, or the copy before it had not ended) cost the engine nothing, however long it
/// waited: it counts as a look of no length at `started`, its own start, so the next try comes with the retry tick.
pub fn failed_look_counts(
    started: Instant,
    git_started: Option<Instant>,
    git_ended: Option<Instant>,
    now: Instant,
) -> (Instant, Instant) {
    match git_started {
        None => (started, started),
        Some(from) => (from, git_ended.unwrap_or(now).max(from)),
    }
}

/// When the copy task looks at the tree while the engine works. On the timer (`every`, counted from the start of one look to the
/// start of the next), and after a tool call ended ([`Cadence::wakes`]); a look that a tool call asked for waits for [`COPY_FLOOR`], and
/// every look waits for [`COPY_SPACING`] times the time git took for the look before it, at most [`COPY_SPACING_MAX`]
/// ([`Cadence::delay`]). A look that failed counts the same way ([`CopyClock::failed`]) and its timer is [`COPY_RETRY`] when that is
/// shorter ([`Cadence::period`]). A look copies only when the tree changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    /// The timer; `None` when the invocation switched the copies off (`snapshotEveryS` 0): no look while the engine works.
    pub every: Option<Duration>,
    /// [`COPY_FLOOR`], or what a test set.
    pub floor: Duration,
    /// [`COPY_RETRY`], or what a test set.
    pub retry: Duration,
    /// [`COPY_SPACING_MAX`], or [`COPY_SPACING_CAP`] periods of a timer that a test set.
    pub space_max: Duration,
}

impl Cadence {
    pub fn new(every: Option<Duration>) -> Self {
        Self {
            every,
            floor: COPY_FLOOR,
            retry: COPY_RETRY,
            space_max: COPY_SPACING_MAX,
        }
    }

    /// [`Cadence::new`] with another cap of the spacing (the copy task, for a timer that a test set; tests).
    pub fn with_space_max(mut self, space_max: Duration) -> Self {
        self.space_max = space_max;
        self
    }

    /// [`Cadence::new`] with another floor (tests).
    pub fn with_floor(mut self, floor: Duration) -> Self {
        self.floor = floor;
        self
    }

    /// [`Cadence::new`] with another retry time (tests).
    pub fn with_retry(mut self, retry: Duration) -> Self {
        self.retry = retry;
        self
    }

    /// The time from the start of a look to the next tick of the timer; `None` when the looks are off. It is the timer of the run after a
    /// look that stored a copy or found the tree unchanged. After a look that `failed` it is the retry time when that is shorter:
    /// the timer the user lengthened does not put off the next try of a copy that was not stored.
    pub fn period(&self, failed: bool) -> Option<Duration> {
        self.every
            .map(|every| if failed { every.min(self.retry) } else { every })
    }

    /// Whether a tool call that ended wakes the task: the copies are on and the last bundle was small ([`WAKE_MAX_BUNDLE_BYTES`]).
    pub fn wakes(&self, clock: &CopyClock) -> bool {
        self.every.is_some()
            && clock
                .bundle_bytes
                .is_none_or(|bytes| bytes <= WAKE_MAX_BUNDLE_BYTES)
    }

    /// [`Cadence::wakes`] as the copy task asks it. While the task tries the copy of a stop again (`retrying`: the first process of its
    /// machine was asked to end, the copy could not be saved, and the task looks every [`STOP_COPY_RETRY`]) no tool call wakes it. Those
    /// looks have no floor and no spacing, so a wake would start one after each tool call, each with its full git work, for as long as
    /// the copies fail. Nothing is lost: the retry tick comes sooner than [`COPY_FLOOR`] would let a tool call's look start.
    pub fn wakes_now(&self, clock: &CopyClock, retrying: bool) -> bool {
        !retrying && self.wakes(clock)
    }

    /// How long the task waits at `now` before it looks; `wake` says that a tool call asked for the look, not the timer.
    ///
    /// The formula of the looks. With S the start of the last look, E the end of its git work, D = E - S and I the timer of the
    /// run ([`Cadence::period`]), the tick comes at S + I and this wait puts the next look at
    ///
    /// ```text
    /// max(S + I, E + min(9 * D, 300 s))        the timer
    /// max(S + 15 s, E + min(9 * D, 300 s))     a tool call that ended, when that is sooner
    /// ```
    ///
    /// so the looks take D of every 10 * D at the most, a tenth of the time, while D is at most 33 s; a look that takes longer is
    /// followed by five minutes without one ([`COPY_SPACING_MAX`]). At the default timer of 15 s: D = 0.2 s, the next look 15 s after
    /// the start of this one (1.3% of the time); D = 3 s, after 30 s (10%); D = 30 s, after 300 s (10%); D = 120 s, after 420 s (29%).
    /// The share is of the wall clock, the time one git works: git is given no lower priority than the engine's commands.
    pub fn delay(&self, clock: &CopyClock, wake: bool, now: Instant) -> Duration {
        let Some((started, ended)) = clock.last else {
            return Duration::ZERO;
        };
        let space = (ended.saturating_duration_since(started) * COPY_SPACING).min(self.space_max);
        let spaced = ended + space;
        let floored = if wake { started + self.floor } else { started };
        spaced.max(floored).saturating_duration_since(now)
    }
}

fn io(e: std::io::Error) -> String {
    format!("snapshot: {e}")
}

fn ws(e: WsError) -> String {
    format!("snapshot: {e}")
}

/// The bytes of a regular file of at most [`MAX_CONTROLLED_FILE`] bytes, opened without following a link and without blocking;
/// `None` for a missing file, a link, a FIFO, a device, a directory or a file that is too big.
pub fn read_regular_nofollow(path: &Path) -> Option<Vec<u8>> {
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let st = fstat(&fd).ok()?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || u64::try_from(st.st_size).ok()? > MAX_CONTROLLED_FILE
    {
        return None;
    }
    let mut out = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_CONTROLLED_FILE)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// The most bytes of `git log` lines a stop's facts carry.
pub const MAX_LOG_BYTES: usize = 64 * 1024;
/// The most bytes of `git diff --stat` a stop's facts carry.
pub const MAX_STAT_BYTES: usize = 64 * 1024;
/// The most file lines of `git diff --stat` a stop's facts carry.
pub const MAX_STAT_FILES: usize = 500;
/// The most paths or log lines one call returns, whatever `limit` asks.
pub const MAX_FACT_ITEMS: usize = 2_000;
/// The file at the top of the work tree that an agent keeps its progress in, by convention.
pub const PROGRESS_FILE: &str = "PROGRESS.md";
/// The most bytes of the progress file a stop's facts carry.
pub const MAX_PROGRESS_BYTES: usize = 32 * 1024;

/// `Ok` for an object id (7 to 64 hexadecimal digits), so a value that reaches git's command line can never be an option or a ref.
fn object_id(s: &str) -> Result<(), String> {
    if (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("snapshot: not an object id: {}", s.escape_debug()))
    }
}

/// The head of `PROGRESS.md` at the top of the work tree at `repo` (at most [`MAX_PROGRESS_BYTES`] bytes, cut at a character
/// boundary), when it is a regular file: engine-controlled, so opened as `read_regular_nofollow` does. `None` when absent or not
/// a regular file. Not scrubbed here; the digest builder scrubs the whole text.
pub fn progress_file(repo: &Path) -> Option<String> {
    let bytes = read_regular_nofollow(&repo.join(PROGRESS_FILE))?;
    let text = String::from_utf8_lossy(&bytes).replace('\0', "");
    let kept = head_bytes(&text, MAX_PROGRESS_BYTES);
    (!kept.trim().is_empty()).then(|| kept.to_string())
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn head_bytes(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `lines` joined with newlines, whole lines only, at most `budget` bytes in all; `last` (a summary line) is always kept and counted.
fn bounded_lines(
    lines: impl Iterator<Item = String>,
    budget: usize,
    last: Option<String>,
) -> String {
    let reserve = last.as_ref().map_or(0, |l| l.len() + 1);
    let mut out = String::new();
    for line in lines {
        if out.len() + line.len() + 1 + reserve > budget {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    match last {
        Some(l) => out.push_str(&l),
        None => {
            out.pop();
        }
    }
    out
}

/// The most changed paths a stop's facts list (the digest lists them, the card shows the first twelve).
pub const MAX_FACT_PATHS: usize = 500;
/// The most `git log` lines a stop's facts carry.
pub const MAX_FACT_LOG_LINES: usize = 200;

/// What the work tree holds against the base commit when a stop is assessed: the git inputs of the digest of a fresh session and of
/// the card, gathered in one pass by [`ScratchRepo::git_facts`]. A part that could not be read is empty here and named in `errors`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitFacts {
    /// `<short id> <subject>` lines of the commits made since the base, newest first.
    pub git_log: Option<String>,
    /// `git diff --stat` of the work tree against the base.
    pub diff_stat: Option<String>,
    /// The first [`MAX_FACT_PATHS`] paths that differ from the base.
    pub changed_files: Vec<String>,
    /// How many paths differ from the base in all.
    pub files_total: u32,
    /// The head of `PROGRESS.md` at the top of the work tree.
    pub progress: Option<String>,
    /// One scrubbed line per part that failed.
    pub errors: Vec<String>,
}

impl GitFacts {
    /// Puts the git members of a digest's sources (`git_log`, `diff_stat`, `changed_files`, `files_total`, `progress_file`) into `into`;
    /// every other member is left as it is.
    pub fn fill(self, into: &mut DigestSources) {
        into.git_log = self.git_log;
        into.diff_stat = self.diff_stat;
        into.changed_files = self.changed_files;
        into.files_total = self.files_total;
        into.progress_file = self.progress;
    }
}

/// The real tree measurer and git half of the digest sources of a run: a [`ScratchRepo::facts`] repository, the base commit, and a lock
/// so that two callers never share the repository's index at once.
#[derive(Debug)]
pub struct GitSource {
    scratch: ScratchRepo,
    base: String,
    lock: tokio::sync::Mutex<()>,
}

impl GitSource {
    pub fn new(scratch: ScratchRepo, base: impl Into<String>) -> Self {
        Self {
            scratch,
            base: base.into(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Opens `<agent_data>/facts.git` for the work tree at `repo`, seeded with `base`.
    pub async fn open(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        Ok(Self::new(
            ScratchRepo::facts(agent_data, repo, base).await?,
            base,
        ))
    }

    /// Every git fact of the work tree now. The engine's `HEAD` is read with the one `git rev-parse HEAD` agentd already runs in the
    /// engine's repository for the snapshot and the nudge (hooks, fsmonitor and the global configuration off); nothing else is.
    pub async fn facts(&self) -> GitFacts {
        let _guard = self.lock.lock().await;
        match git_with_env(&self.scratch.repo, &["rev-parse", "HEAD"], &[]).await {
            Ok(head) => self.scratch.git_facts(&self.base, &head).await,
            Err(e) => {
                // Without the head there is no log; the rest of the facts do not need it.
                let mut facts = self.scratch.git_facts(&self.base, &self.base).await;
                facts.errors.push(scrub_detail(&format!("head: {}", ws(e))));
                facts
            }
        }
    }
}

impl TreeSource for GitSource {
    fn changed_files(&self, limit: usize) -> BoxFuture<'_, Option<ChangedFiles>> {
        Box::pin(async move {
            let _guard = self.lock.lock().await;
            let (paths, total) = self.scratch.changed_paths(&self.base, limit).await.ok()?;
            Some(ChangedFiles { paths, total })
        })
    }
}

/// A private bare repository that can hash the working tree of the engine's repository without writing to it: its own index
/// and object store, the engine's objects as an alternate, the engine's work tree as `GIT_WORK_TREE`.
#[derive(Debug)]
pub struct ScratchRepo {
    git_dir: PathBuf,
    repo: PathBuf,
    /// When the look that uses this repository now gives up; `None` outside a look of the copy task (the facts, the copy of the end,
    /// whose callers set their own limits).
    give_up: Option<GiveUp>,
    /// A file call on the work tree has not returned ([`Self::on_tree`]); the thread that makes the call clears it.
    busy: Arc<AtomicBool>,
}

impl ScratchRepo {
    /// `<parent>/probe.git`, for the nudge's change test.
    pub async fn new(parent: &Path, repo: &Path) -> Result<Self, String> {
        Self::open(parent.join("probe.git"), repo).await
    }

    /// Creates (or reuses) the repository at `git_dir` for the work tree at `repo`.
    pub async fn open(git_dir: PathBuf, repo: &Path) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&git_dir).map_err(io)?;
        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        let gd: &Path = &git_dir;
        let parent = git_dir.parent().unwrap_or(Path::new("/"));
        git_with_env(
            parent,
            &["init", "-q", "--bare", gd.to_str().unwrap_or_default()],
            &[],
        )
        .await
        .map_err(ws)?;
        // A work tree is set per call; the repository is not bare for the calls that add files.
        git_with_env(
            parent,
            &["config", "core.bare", "false"],
            &[("GIT_DIR", gd)],
        )
        .await
        .map_err(ws)?;
        let objects = repo.join(".git/objects");
        std::fs::create_dir_all(git_dir.join("objects/info")).map_err(io)?;
        std::fs::write(
            git_dir.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )
        .map_err(io)?;
        Ok(Self {
            git_dir,
            repo: repo.to_path_buf(),
            give_up: None,
            busy: Arc::new(AtomicBool::new(false)),
        })
    }

    /// A git call of this repository: one that ends when the look in progress is given up ([`git_with_env_until`]), or the plain call
    /// outside a look. Either is started outside `dir` and changes to it itself (`git -C`): starting a process in a directory waits until
    /// the new process is there, and in a work tree that stopped answering that wait cannot be ended.
    async fn git(
        &self,
        dir: &Path,
        args: &[&str],
        envs: &[(&str, &Path)],
    ) -> Result<String, WsError> {
        match &self.give_up {
            Some(give_up) => git_with_env_until(dir, args, envs, give_up).await,
            None => git_with_env_outside(dir, args, envs).await,
        }
    }

    /// Runs `call`, a file call on the work tree, on a thread of its own and waits for it until the look in progress is given up.
    /// On a network mount whose server stops answering such a call never returns and cannot be ended: made on a thread of the runtime it
    /// held that thread, and the task that made it, for good. One such call is outstanding at most: while it has not returned, the next
    /// one is refused at once, so a tree that stays silent costs one thread, whatever the number of looks.
    async fn on_tree<T: Send + 'static>(
        &self,
        what: &'static str,
        call: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, String> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(format!(
                "snapshot: the work tree has not answered an earlier call; {what} was not read"
            ));
        }
        let busy = self.busy.clone();
        let made = tokio::task::spawn_blocking(move || {
            let out = call();
            busy.store(false, Ordering::SeqCst);
            out
        });
        let waited = match &self.give_up {
            Some(give_up) => give_up.run(made).await,
            None => Some(made.await),
        };
        match waited {
            Some(Ok(out)) => Ok(out),
            Some(Err(e)) => {
                // The call ended in a panic: it is not outstanding.
                self.busy.store(false, Ordering::SeqCst);
                Err(format!("snapshot: {what}: {e}"))
            }
            None => Err(format!(
                "snapshot: the work tree did not answer; {what} was not read"
            )),
        }
    }

    fn envs<'a>(&'a self, index: &'a Path) -> [(&'static str, &'a Path); 3] {
        [
            ("GIT_DIR", &self.git_dir),
            ("GIT_WORK_TREE", &self.repo),
            ("GIT_INDEX_FILE", index),
        ]
    }

    /// Fills the private index with the tree of `commit`, so what is tracked there stays tracked whatever an ignore rule says.
    pub async fn seed(&self, commit: &str) -> Result<(), String> {
        let index = self.git_dir.join("index");
        git_with_env(&self.repo, &["read-tree", commit], &self.envs(&index))
            .await
            .map(|_| ())
            .map_err(ws)
    }

    /// `<agent_data>/facts.git` for the work tree at `repo`, seeded with the tree of `base`: the repository the git facts of a
    /// stop are read from ([`Self::changed_paths`], [`Self::log_since`], [`Self::diff_stat`]). It is its own repository (its own
    /// index and object store) so that reading the facts never moves the nudge's `probe.git` fingerprint, and the seed keeps a file
    /// that is tracked at `base` tracked whatever an ignore rule says now (without it such a file would read as deleted).
    pub async fn facts(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        let scratch = Self::open(agent_data.join("facts.git"), repo).await?;
        scratch.seed(base).await?;
        Ok(scratch)
    }

    /// A read-only git call on this repository alone: `GIT_DIR` set, run from agentd's own directory (never from the engine's).
    async fn read(&self, args: &[&str]) -> Result<String, String> {
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        self.git(cwd, args, &[("GIT_DIR", &self.git_dir)])
            .await
            .map_err(ws)
    }

    /// The tree id of the commit `commit` (an object id, 7 to 64 hexadecimal digits, never a ref name).
    pub async fn tree_of(&self, commit: &str) -> Result<String, String> {
        object_id(commit)?;
        self.read(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
            .await
    }

    /// The paths that differ between the commit `base` and the work tree as it is now (added, modified, deleted, type changed; no
    /// rename detection), in git's path order, at most `limit` of them (each scrubbed to one line), and how many differ in all.
    /// Read through this repository's own index: one caller at a time, and use a repository made by [`Self::facts`] with the same
    /// `base`, otherwise a tracked file an ignore rule matches would be listed as deleted. Nothing is run in the engine's repository.
    pub async fn changed_paths(
        &self,
        base: &str,
        limit: usize,
    ) -> Result<(Vec<String>, u32), String> {
        let base_tree = self.tree_of(base).await?;
        let work_tree = self.tree().await?;
        self.paths_between(&base_tree, &work_tree, limit).await
    }

    /// The paths that differ between two tree ids (see [`Self::changed_paths`]).
    async fn paths_between(
        &self,
        base_tree: &str,
        work_tree: &str,
        limit: usize,
    ) -> Result<(Vec<String>, u32), String> {
        if base_tree == work_tree {
            return Ok((Vec::new(), 0));
        }
        // `--name-status -z` prints `<status> NUL <path> NUL`; the status letter leads the output, so the trimming of the
        // output never eats the start of a path that begins with a space.
        let raw = self
            .read(&[
                "diff-tree",
                "-r",
                "-z",
                "--name-status",
                "--no-renames",
                base_tree,
                work_tree,
            ])
            .await?;
        let mut fields = raw.split('\0');
        let mut paths = Vec::new();
        let mut total: u32 = 0;
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            if status.is_empty() {
                break;
            }
            total = total.saturating_add(1);
            if paths.len() < limit.min(MAX_FACT_ITEMS) {
                paths.push(scrub_detail(path));
            }
        }
        Ok((paths, total))
    }

    /// The commits from `base` (excluded) to `head` (included), one `<short id> <subject>` line each, newest first, at most `limit`
    /// lines and [`MAX_LOG_BYTES`] bytes, every line scrubbed. `head` is an object id the caller read (the engine's `HEAD`); this
    /// repository has no refs of the engine's.
    pub async fn log_since(&self, base: &str, head: &str, limit: usize) -> Result<String, String> {
        object_id(base)?;
        object_id(head)?;
        let count = limit.min(MAX_FACT_ITEMS);
        if count == 0 {
            return Ok(String::new());
        }
        let raw = self
            .read(&[
                "log",
                "--no-color",
                "--no-decorate",
                "--no-show-signature",
                "--format=%h %s",
                &format!("--max-count={count}"),
                &format!("{base}..{head}"),
            ])
            .await?;
        Ok(bounded_lines(
            raw.lines().map(scrub_detail),
            MAX_LOG_BYTES,
            None,
        ))
    }

    /// `git diff --stat` between two tree ids, at most [`MAX_STAT_FILES`] file lines then ` ...` and the summary line, bounded to
    /// [`MAX_STAT_BYTES`] bytes with every line scrubbed; empty when the trees are equal.
    pub async fn diff_stat(&self, base_tree: &str, work_tree: &str) -> Result<String, String> {
        object_id(base_tree)?;
        object_id(work_tree)?;
        if base_tree == work_tree {
            return Ok(String::new());
        }
        let raw = self
            .read(&[
                "diff-tree",
                "-r",
                "--no-renames",
                "--no-color",
                &format!("--stat=120,60,{MAX_STAT_FILES}"),
                base_tree,
                work_tree,
            ])
            .await?;
        let mut lines: Vec<String> = raw.lines().map(scrub_detail).collect();
        // The last line is the summary (`N files changed, ...`): it stays when the file lines are cut to the byte budget.
        let summary = lines.pop();
        Ok(bounded_lines(lines.into_iter(), MAX_STAT_BYTES, summary))
    }

    /// Every git fact of a stop in one pass: the work tree is hashed once, and a part that fails leaves the others intact. `base` is
    /// the base commit and `head` the engine's `HEAD` (object ids the caller read). Use a repository made by [`Self::facts`].
    pub async fn git_facts(&self, base: &str, head: &str) -> GitFacts {
        let mut f = GitFacts {
            progress: progress_file(&self.repo),
            ..GitFacts::default()
        };
        let mut errors: Vec<String> = Vec::new();
        let mut fail = |part: &str, e: String| errors.push(scrub_detail(&format!("{part}: {e}")));
        let trees = match self.tree_of(base).await {
            Ok(base_tree) => self.tree().await.map(|work| (base_tree, work)),
            Err(e) => Err(e),
        };
        match trees {
            Ok((base_tree, work_tree)) if base_tree != work_tree => {
                match self
                    .paths_between(&base_tree, &work_tree, MAX_FACT_PATHS)
                    .await
                {
                    Ok((paths, total)) => {
                        f.changed_files = paths;
                        f.files_total = total;
                    }
                    Err(e) => fail("changed files", e),
                }
                match self.diff_stat(&base_tree, &work_tree).await {
                    Ok(stat) if !stat.is_empty() => f.diff_stat = Some(stat),
                    Ok(_) => {}
                    Err(e) => fail("diff stat", e),
                }
            }
            Ok(_) => {}
            Err(e) => fail("work tree", e),
        }
        match self.log_since(base, head, MAX_FACT_LOG_LINES).await {
            Ok(log) if !log.is_empty() => f.git_log = Some(log),
            Ok(_) => {}
            Err(e) => fail("log", e),
        }
        f.errors = errors;
        f
    }

    /// The repository's ignore rules, as they are now, read as a regular file or not at all, as this repository's own. The file is on the
    /// work tree's file system: it is read off the runtime's threads ([`Self::on_tree`]).
    async fn copy_exclude(&self) -> Result<(), String> {
        std::fs::create_dir_all(self.git_dir.join("info")).map_err(io)?;
        let path = self.repo.join(".git/info/exclude");
        let exclude = self
            .on_tree("its ignore rules", move || {
                read_regular_nofollow(&path).unwrap_or_default()
            })
            .await?;
        std::fs::write(self.git_dir.join("info/exclude"), exclude).map_err(io)
    }

    /// Whether this repository can read the commit `commit`, and so its history: from its own objects or from the engine's store
    /// through the alternate. Not after the engine removed or replaced `.git`.
    pub async fn has_commit(&self, commit: &str) -> bool {
        object_id(commit).is_ok()
            && self
                .read(&["cat-file", "-e", &format!("{commit}^{{commit}}")])
                .await
                .is_ok()
    }

    /// The tree hash of the work tree as it is now, hashed into a new index of its own. Every file is read again and its blob is
    /// written to this repository unless the alternate holds it, so the tree needs nothing of the base: a commit of it with no parent is a
    /// copy that holds the whole tree. A file the base tracked that an ignore rule matches now is left out, because the base cannot be read.
    pub async fn whole_tree(&self) -> Result<String, String> {
        let index = self.git_dir.join(WHOLE_INDEX);
        match std::fs::remove_file(&index) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        self.copy_exclude().await?;
        let envs = self.envs(&index);
        self.git(&self.repo, &["add", "-A"], &envs)
            .await
            .map_err(ws)?;
        self.git(&self.repo, &["write-tree"], &envs)
            .await
            .map_err(ws)
    }

    /// The tree hash of the work tree as it is now (`add -A`, then `write-tree`). It is content-aware: a modified file edited
    /// again hashes differently, which `git status` does not show.
    pub async fn tree(&self) -> Result<String, String> {
        let index = self.git_dir.join("index");
        self.copy_exclude().await?;
        let envs = self.envs(&index);
        self.git(&self.repo, &["add", "-A"], &envs)
            .await
            .map_err(ws)?;
        self.git(&self.repo, &["write-tree"], &envs)
            .await
            .map_err(ws)
    }
}

#[derive(Debug)]
pub struct Snapshotter {
    scratch: ScratchRepo,
    git_dir: PathBuf,
    repo: PathBuf,
    base: String,
    last_tree: String,
    /// The engine's `HEAD` as the last periodic copy read it: the parent of a final copy that may read nothing in the engine's repository.
    last_parent: Option<String>,
    count: u32,
    /// What the last [`Self::drop_old_versions`] could not do, for the caller's log ([`Self::take_tidy_error`]).
    tidy_error: Option<String>,
    /// A look hashed a changed tree since the last clean-up: the next look, or the copy of the end, starts with one.
    tidy_due: bool,
    /// The last look of the copy task ended in an error, so a git of it may have been ended in the middle: the next one clears the
    /// lock files first ([`Self::snapshot_until`]).
    gave_up: bool,
}

/// The lock files a git that was killed in the middle can leave in the private repository: the index's, the ref's, the bundle's
/// (`git bundle create` writes `<file>.lock` and renames it) and the repository's own.
const STALE_LOCKS: [&str; 6] = [
    "index.lock",
    "index-whole.lock",
    "HEAD.lock",
    "config.lock",
    "refs/heads/snapshot.lock",
    "snapshot.bundle.lock",
];

impl Snapshotter {
    /// Creates the snapshot repository for the worktree at `repo`, whose branch starts at `base`.
    pub async fn new(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        let git_dir = agent_data.join("snapshot.git");
        let scratch = ScratchRepo::open(git_dir.clone(), repo).await?;
        // No reflog keeps the commit of an earlier copy, and so its files, from `git prune` ([`Self::drop_old_versions`]); a
        // repository an earlier session made keeps none either.
        let gd: &Path = &git_dir;
        let cwd = agent_data;
        for args in [
            &["config", "core.logAllRefUpdates", "false"][..],
            &[
                "reflog",
                "expire",
                "--expire=now",
                "--expire-unreachable=now",
                "--all",
            ][..],
        ] {
            git_with_env(cwd, args, &[("GIT_DIR", gd)])
                .await
                .map_err(ws)?;
        }
        scratch.seed(base).await?;
        let base_tree = git_with_env(repo, &["rev-parse", &format!("{base}^{{tree}}")], &[])
            .await
            .map_err(ws)?;
        // The tree the session starts on is the tree of the commit the repository stands at: the base in a first session, the
        // commit an earlier session saved in a later one. That work is saved already, so the session makes no copy until a file changes.
        let start_tree = git_with_env(repo, &["rev-parse", "HEAD^{tree}"], &[])
            .await
            .unwrap_or(base_tree);
        Ok(Self {
            scratch,
            git_dir,
            repo: repo.to_path_buf(),
            base: base.to_string(),
            last_tree: start_tree,
            last_parent: None,
            count: 0,
            tidy_error: None,
            tidy_due: false,
            gave_up: false,
        })
    }

    /// Removes what no copy needs any more: the commits of earlier copies and the versions of files that changed again since.
    /// `git prune` keeps what `refs/heads/snapshot` and the private index name, and removes only this repository's own loose objects, never
    /// the engine's store behind the alternate. `git add` streams a file above git's big-file threshold (512 MiB) into a pack of its own,
    /// which prune does not shrink: while a pack is there, `git repack -a -d -l` puts what is still needed into one pack and removes the
    /// others (`-l`: none of the engine's objects; `--window=0`: no new deltas, the old pack's data is reused). It runs under the copy lock
    /// at the start of the look after one that hashed a changed tree, and of the copy of the end ([`Self::tidy_if_due`]), never between a
    /// copy and its upload: the time from reading the tree to storing its copy is what it was without it, and the first copy of a session
    /// waits for nothing. The time it takes counts as git time for the spacing. What it could not do does not fail the copy: the caller logs it ([`Self::take_tidy_error`]).
    async fn drop_old_versions(&mut self) {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let mut failed = Vec::new();
        if let Err(e) = self
            .scratch
            .git(cwd, &["prune", "--expire=now"], &[("GIT_DIR", gd)])
            .await
        {
            failed.push(ws(e));
        }
        if has_pack(&self.git_dir.join("objects/pack"))
            && let Err(e) = self
                .scratch
                .git(
                    cwd,
                    &["repack", "-a", "-d", "-l", "-q", "--window=0"],
                    &[("GIT_DIR", gd)],
                )
                .await
        {
            failed.push(ws(e));
        }
        self.tidy_error = (!failed.is_empty()).then(|| failed.join("; "));
    }

    /// [`Self::drop_old_versions`] when a look hashed a changed tree since the last one.
    async fn tidy_if_due(&mut self) {
        if std::mem::take(&mut self.tidy_due) {
            self.drop_old_versions().await;
        }
    }

    /// What the last clean-up of the repository could not do ([`Self::drop_old_versions`]), once.
    pub fn take_tidy_error(&mut self) -> Option<String> {
        self.tidy_error.take()
    }

    /// The copies of this session go on counting after `before`, the number of the newest copy the run saved before the session.
    /// Called once, before the first copy.
    pub fn count_from(&mut self, before: u32) {
        self.count = before;
    }

    /// The copy just made could not be stored: the next look copies the tree again, whether it changed or not, so a tree whose
    /// upload failed is not left without a copy until its next change.
    pub fn forget_last(&mut self) {
        self.last_tree.clear();
    }

    /// Removes the lock files a killed git left ([`STALE_LOCKS`]). The copy task is aborted when the engine stops and its git children are
    /// killed with it; only agentd uses this repository, one caller at a time, so a lock found here belongs to no living process.
    pub fn clear_stale_locks(&self) {
        for lock in STALE_LOCKS {
            let _ = std::fs::remove_file(self.git_dir.join(lock));
        }
    }

    /// [`ScratchRepo::tree`], or [`ScratchRepo::whole_tree`] when the index this repository keeps cannot be written as a tree any more (its
    /// entries name objects of the engine's store, and the engine removed it).
    async fn current_tree(&self) -> Result<String, String> {
        match self.scratch.tree().await {
            Ok(tree) => Ok(tree),
            Err(e) => self.scratch.whole_tree().await.map_err(|_| e),
        }
    }

    /// The engine's `HEAD`, while `.git` is the repository's own directory ([`git_dir_problem`]) and this repository can read that commit.
    /// `None` when the engine removed or replaced `.git`: agentd then runs no git in the engine's repository.
    async fn engine_head(&self) -> Option<String> {
        let repo = self.repo.clone();
        let own = self
            .scratch
            .on_tree("its git directory", move || {
                git_dir_problem(&repo).is_none()
            })
            .await
            .ok()?;
        if !own {
            return None;
        }
        let head = self
            .scratch
            .git(&self.repo, &["rev-parse", "HEAD"], &[])
            .await
            .ok()?;
        self.scratch.has_commit(&head).await.then_some(head)
    }

    /// The commit of copy `n` and its tree: `tree` on `parent` when this repository can read `parent`. Else
    /// a commit with no parent of the tree as
    /// [`ScratchRepo::whole_tree`] hashes it, whose bundle holds the whole tree itself. Run from agentd's own directory.
    async fn commit_copy(
        &self,
        tree: String,
        parent: Option<&str>,
        n: u32,
    ) -> Result<(String, String), String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let message = format!("harness: snapshot {n}");
        if let Some(parent) = parent
            && self.scratch.has_commit(parent).await
        {
            let commit = self
                .scratch
                .git(
                    cwd,
                    &["commit-tree", &tree, "-p", parent, "-m", &message],
                    &[("GIT_DIR", gd)],
                )
                .await
                .map_err(ws)?;
            return Ok((commit, tree));
        }
        let whole = self.scratch.whole_tree().await?;
        let commit = self
            .scratch
            .git(
                cwd,
                &["commit-tree", &whole, "-m", &message],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        Ok((commit, whole))
    }

    /// Writes the bundle of `refs/heads/snapshot` and returns where it is and its size. Run from agentd's own directory.
    async fn write_bundle(&self) -> Result<(PathBuf, u64), String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let bundle = self.git_dir.join("snapshot.bundle");
        let _ = std::fs::remove_file(&bundle);
        self.scratch
            .git(
                cwd,
                &[
                    "bundle",
                    "create",
                    "-q",
                    bundle.to_str().unwrap_or_default(),
                    "refs/heads/snapshot",
                ],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        let bytes = std::fs::metadata(&bundle).map_err(io)?.len();
        Ok((bundle, bytes))
    }

    /// The copy of the session's end, made when the engine and everything it started have stopped. With `head` (the session's
    /// final head, once the end commit is made) the `snapshot` ref names that commit itself: the copy is the head a delivery is planned
    /// from. Without one (the end commit failed, or agentd may run no git in the checkout) the copy is the tree as it is, on the last
    /// head a periodic copy read (the base when there was none): no git call reads the engine's repository for it. When that head cannot
    /// be read any more (the engine removed or replaced `.git`) the copy holds the whole tree and has no parent ([`Self::commit_copy`]).
    /// A copy is made whether or not the tree changed, so the newest record always says what the end state is.
    ///
    /// It starts with the clean-up a look left due ([`Self::drop_old_versions`]), after the lock files are cleared.
    pub async fn final_copy(&mut self, head: Option<&str>) -> Result<SnapshotMade, String> {
        // The copy of the end has its caller's budget; a look that was aborted in the middle must not leave its own limit behind.
        self.scratch.give_up = None;
        self.clear_stale_locks();
        self.tidy_if_due().await;
        let made = self.final_copy_kept(head).await;
        self.tidy_due = true;
        made
    }

    async fn final_copy_kept(&mut self, head: Option<&str>) -> Result<SnapshotMade, String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let n = self.count + 1;
        let read_unix = now_unix();
        let (commit, tree) = match head {
            Some(head) => {
                let tree = self.scratch.tree_of(head).await?;
                (head.to_string(), tree)
            }
            None => {
                let tree = self.current_tree().await?;
                let parent = self
                    .last_parent
                    .clone()
                    .unwrap_or_else(|| self.base.clone());
                self.commit_copy(tree, Some(&parent), n).await?
            }
        };
        self.scratch
            .git(
                cwd,
                &["update-ref", "refs/heads/snapshot", &commit],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        let (bundle, bundle_bytes) = self.write_bundle().await?;
        self.last_tree = tree;
        self.count = n;
        Ok(SnapshotMade {
            commit,
            base: self.base.clone(),
            bundle,
            count: n,
            final_copy: true,
            bundle_bytes,
            read_unix,
        })
    }

    /// Takes a snapshot of the tree as it is now; `None` when it equals the last one. Its parent is the engine's HEAD
    /// ([`Self::engine_head`]); without one, the last head a copy read (the base when there was none), as for the copy of the end; a parent
    /// this repository cannot read makes the copy hold the whole tree ([`Self::commit_copy`]).
    ///
    /// It starts with [`Self::drop_old_versions`] when the look before it hashed a changed tree, also when a later step of that look failed,
    /// so the objects of earlier copies never pile up; after a look at an unchanged tree, which wrote nothing, there is none.
    pub async fn snapshot(&mut self) -> Result<Option<SnapshotMade>, String> {
        self.scratch.give_up = None;
        self.look().await
    }

    /// [`Self::snapshot`] for a look of the copy task: every git call and every file call on the work tree ends when `give_up`
    /// says so, with an error that names the call. Lock files that a git which was ended in an earlier look left are removed first.
    pub async fn snapshot_until(
        &mut self,
        give_up: &GiveUp,
    ) -> Result<Option<SnapshotMade>, String> {
        self.scratch.give_up = Some(give_up.clone());
        if std::mem::take(&mut self.gave_up) {
            self.clear_stale_locks();
        }
        let made = self.look().await;
        self.scratch.give_up = None;
        if made.is_err() {
            // A git of this look may have been ended in the middle: the next look starts from a repository without its lock files.
            self.gave_up = true;
        }
        made
    }

    async fn look(&mut self) -> Result<Option<SnapshotMade>, String> {
        self.tidy_if_due().await;
        let made = self.snapshot_kept().await;
        if !matches!(made, Ok(None)) {
            self.tidy_due = true;
        }
        made
    }

    async fn snapshot_kept(&mut self) -> Result<Option<SnapshotMade>, String> {
        let read_unix = now_unix();
        let tree = self.current_tree().await?;
        if tree == self.last_tree {
            return Ok(None);
        }
        let gd: &Path = &self.git_dir;
        let head = self.engine_head().await;
        let parent = head
            .clone()
            .or_else(|| self.last_parent.clone())
            .unwrap_or_else(|| self.base.clone());
        let n = self.count + 1;
        let (commit, tree) = self.commit_copy(tree, Some(&parent), n).await?;
        self.scratch
            .git(
                self.git_dir.parent().unwrap_or(Path::new("/")),
                &["update-ref", "refs/heads/snapshot", &commit],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        let (bundle, bundle_bytes) = self.write_bundle().await?;
        self.last_tree = tree;
        if head.is_some() {
            self.last_parent = head;
        }
        self.count = n;
        Ok(Some(SnapshotMade {
            commit,
            base: self.base.clone(),
            bundle,
            count: n,
            final_copy: false,
            bundle_bytes,
            read_unix,
        }))
    }
}

/// Whether the folder `dir` holds a pack file.
fn has_pack(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".pack"))
    })
}

/// `snapshot.json`, written after the bundle so a reader that sees it finds the bundle.
///
/// The copy of the session's end adds `"final": true`; a periodic copy has no such member, so its record is what it always was.
pub fn snapshot_json(made: &SnapshotMade, at_rfc3339: &str) -> String {
    let mut record = serde_json::json!({
        "schemaVersion": 1,
        "headSha": made.commit,
        "baseSha": made.base,
        "at": at_rfc3339,
        "count": made.count,
    });
    if made.final_copy {
        record["final"] = serde_json::json!(true);
    }
    record.to_string()
}
