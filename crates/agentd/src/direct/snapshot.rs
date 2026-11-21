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
//! The repository keeps what its last copy needs and a bounded rest ([`ScratchRepo::tidy`]): before the look that follows one that
//! hashed a changed tree, and before the copy of the end, `git prune` removes the commits of earlier copies and the versions of files that
//! changed again, once they amount to a mark,
//! and no reflog keeps them. Without it a file rewritten before
//! every copy left one version per copy on the worker's disk.
//!
//! agentd reads engine-controlled files here as root: the repository's `info/exclude` is opened without following a link and
//! without blocking, and only when it is a regular file of at most 1 MiB.

use crate::BoxFuture;
use crate::direct::classify::scrub_detail;
use crate::direct::input::{ChangedFiles, DigestSources, TreeSource};
use crate::logging::LogSink;
use crate::s3::FileDigest;
use crate::time::now_unix;
use crate::workspace::{
    WsError, git_bytes_with_env, git_dir_problem, git_with_env, git_with_env_outside,
    git_with_env_until,
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

/// The line that leads the settings of a private repository in its own `config` ([`ScratchRepo::tune`]): a repository that
/// holds it has its settings.
const TUNE_MARK: &str = "# agentd: the settings of a private repository of a working session";
/// The settings of a private repository of a working session ([`ScratchRepo::tune`]).
const TUNE_BLOCK: &str =
    "[pack]\n\tthreads = 1\n\twindow = 0\n[gc]\n\tauto = 0\n[core]\n\tuntrackedCache = false\n";

/// The first reading of the tree in a session, made before the engine starts ([`Snapshotter::new_primed`]), is given up after
/// this long. The session starts later by the time the reading takes, so a tree that cannot be read in a minute is read by the first
/// look instead, as before.
pub const PRIME_LIMIT: Duration = Duration::from_secs(60);

/// The clean-up of a private repository runs once the repository has grown by this many KiB of loose objects, or of packs,
/// since the last clean-up: 64 MiB ([`tidy_needed`]).
pub const TIDY_AFTER_KIB: u64 = 65_536;
/// The clean-up of a private repository runs once the repository has grown by this many loose objects since the last clean-up
/// (git's own mark for a `gc` is 6,700 loose objects).
pub const TIDY_AFTER_OBJECTS: u64 = 6_700;
/// The clean-up of a private repository runs whatever the repository holds while its disk has fewer free bytes than this: 1 GiB.
pub const TIDY_FREE_BYTES: u64 = 1 << 30;
/// The clean-up of `goal.git`, `probe.git` and `facts.git` after a reading is given up after this long, with its git ended
/// ([`ScratchRepo::tidy_logged`]): the reading it follows is made already, and what waits for that reading must not wait for a
/// clean-up without a limit. It is the least the git work of a look of the copy task has ([`COPY_GIT_FLOOR`]), which is what bounds
/// the clean-up of `snapshot.git`. A clean-up that was given up is tried again after the next reading.
pub const TIDY_LIMIT: Duration = COPY_GIT_FLOOR;

/// What a private repository holds of its own, as `git count-objects -v` says it ([`ScratchRepo::loose`]): the engine's store
/// behind the alternate is not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Loose {
    /// The loose objects (`count`).
    pub count: u64,
    /// The KiB the loose objects take (`size`).
    pub size_kib: u64,
    /// The KiB the packs take (`size-pack`).
    pub pack_kib: u64,
}

impl Loose {
    /// The three numbers out of the lines of `git count-objects -v`; a number that is not there is 0.
    pub fn parse(text: &str) -> Self {
        let number = |name: &str| {
            text.lines()
                .find_map(|l| {
                    l.strip_prefix(name)?
                        .strip_prefix(": ")?
                        .trim()
                        .parse()
                        .ok()
                })
                .unwrap_or(0)
        };
        Self {
            count: number("count"),
            size_kib: number("size"),
            pack_kib: number("size-pack"),
        }
    }
}

/// What the clean-up of a private repository does when it is asked ([`tidy_needed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tidy {
    /// Nothing: the repository has not grown by a mark since the last clean-up.
    No,
    /// `git prune --expire=now`.
    Prune,
    /// `git prune --expire=now`, and `git repack -a -d -l -q --window=0` while the repository holds a pack.
    PruneRepack,
}

/// The marks of the clean-up of a private repository: how much the repository grows before the next clean-up runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TidyMarks {
    /// KiB of loose objects, and KiB of packs.
    pub kib: u64,
    /// Loose objects.
    pub objects: u64,
    /// Free bytes of the repository's disk below which the clean-up runs whatever the growth.
    pub free_bytes: u64,
}

impl TidyMarks {
    /// The marks of a run: [`TIDY_AFTER_KIB`], [`TIDY_AFTER_OBJECTS`], [`TIDY_FREE_BYTES`].
    pub const RUN: Self = Self {
        kib: TIDY_AFTER_KIB,
        objects: TIDY_AFTER_OBJECTS,
        free_bytes: TIDY_FREE_BYTES,
    };
    /// Every clean-up that is asked for runs, as each did (tests of what a clean-up keeps).
    pub const AT_ONCE: Self = Self {
        kib: 0,
        objects: 0,
        free_bytes: 0,
    };

    /// What the clean-up does for a repository that holds `now`, held `after_last` after its last clean-up (nothing before the
    /// first), and whose disk has `free_bytes` free. Packs that grew by the mark ask for the prune and the repack; loose objects that
    /// grew by a mark, in bytes or in number, or a disk that is nearly full, ask for the prune.
    pub fn needed(&self, now: Loose, after_last: Loose, free_bytes: u64) -> Tidy {
        if now.pack_kib > 0 && now.pack_kib >= after_last.pack_kib.saturating_add(self.kib) {
            Tidy::PruneRepack
        } else if now.size_kib >= after_last.size_kib.saturating_add(self.kib)
            || now.count >= after_last.count.saturating_add(self.objects)
            || free_bytes < self.free_bytes
        {
            Tidy::Prune
        } else {
            Tidy::No
        }
    }
}

/// Whether the clean-up of a private repository runs now, by the marks of a run ([`TidyMarks::RUN`]). Before, `git prune`
/// ran at the start of every look after one that hashed a changed tree and walked what the repository can reach each time: on a
/// history of 20,000 small commits that was 0.32 s of processor time where the rest of the look cost 0.03 s. `git prune` stays git's
/// own; it runs when the repository has grown by 64 MiB of loose objects, by 6,700 loose objects or by 64 MiB of packs since the
/// last clean-up, or when its disk has less than 1 GiB free.
pub fn tidy_needed(now: Loose, after_last: Loose, free_bytes: u64) -> Tidy {
    TidyMarks::RUN.needed(now, after_last, free_bytes)
}

/// The free bytes of the disk that holds `dir`; `u64::MAX` when it cannot be asked (a disk that cannot be asked forces no clean-up).
fn free_bytes(dir: &Path) -> u64 {
    rustix::fs::statvfs(dir).map_or(u64::MAX, |s| s.f_bavail.saturating_mul(s.f_frsize))
}

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
    /// The hash and the size of the bundle, once the look has made them ([`hash_bundle`]); the upload then sends the bundle under
    /// this hash and does not hash it again. `None` as git leaves the copy, and for the copy of the end, whose store hashes it.
    pub digest: Option<FileDigest>,
}

/// Hashes the bundle of the copy `made` as the last part of the look's work, within the look's limit `give_up`, and keeps the
/// hash in `made.digest`. The hash is processor time of the copy (1.96 s for 14 stores of 74.6 MB in the measurement), so the look makes
/// it before its work ends and the spacing of the looks counts it ([`CopyClock::looked`]); before, the upload hashed the bundle after
/// that work had ended. The file is read off the runtime's threads (`s3::digest_file`). A hash that the limit gives up is an error: the
/// look has failed, like a look whose git call the limit gives up.
pub async fn hash_bundle(made: &mut SnapshotMade, give_up: &GiveUp) -> Result<(), String> {
    match give_up.run(crate::s3::digest_file(&made.bundle)).await {
        Some(Ok(digest)) => {
            made.digest = Some(digest);
            Ok(())
        }
        Some(Err(e)) => Err(format!("snapshot: the bundle was not hashed: {e}")),
        None => Err("snapshot: hashing the bundle was given up before it had ended".to_string()),
    }
}

/// The one copy of a read of a stopped session's storage ([`Snapshotter::read_back`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBack {
    pub commit: String,
    pub tree: String,
    /// The commit below it. `None` when the history could not be read: the copy then holds the whole tree itself.
    pub parent: Option<String>,
    /// Whether two passes over the tree gave the same tree.
    pub stable: bool,
    pub bundle: PathBuf,
    pub bundle_bytes: u64,
}

/// What the comparison of a copy read back with a saved copy found ([`Snapshotter::compare`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compared {
    /// The two hold the same tree.
    pub same: bool,
    /// The files of which the saved copy holds more than the storage did: a file of the copy read back that is a strict prefix of the
    /// same file in the saved copy (a shorter file too large to read counts unread), and a file that the saved copy holds, the commit
    /// below it does not, and the copy read back lacks.
    pub regressed: u32,
    /// At most `MAX_COMPARE_PATHS_SHOWN` of those paths, each scrubbed to one line.
    pub regressed_paths: Vec<String>,
    /// The files of the copy read back that equal the file in the commit below the saved copy while the saved copy holds another version.
    pub rewritten: u32,
    pub rewritten_paths: Vec<String>,
}

/// What the bundle of a copy of the work holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleForm {
    /// Everything the copy's commit reaches: the whole history. Such a bundle clones alone.
    Whole,
    /// What the copy's commit reaches and the bundle the run started from does not ([`Snapshotter::set_floors`]): the run's own commits
    /// and its uncommitted changes. git names the commits it left out in the bundle's header, and a reader takes the base first.
    /// Without a floor, and whenever git fails on this path, the bundle is whole.
    AboveFloor,
}

impl BundleForm {
    /// The form of the copies of this release: a copy leaves out what the bundle the run started from holds. It is a constant of
    /// the release, not a setting of a stage: no path leads from an operator to the worker's environment, and every reader of this
    /// release takes such a copy (the restore of a session, the delivery, the download route and the client).
    pub const RELEASE: BundleForm = BundleForm::AboveFloor;

    /// The form the word of `AGENTD_DIRECT_COPY_FORM` names (tests only): `whole`, `base`; unset and any other word is
    /// [`Self::RELEASE`].
    pub fn from_word(word: Option<&str>) -> Self {
        match word.map(str::trim) {
            Some("whole") => BundleForm::Whole,
            Some("base") => BundleForm::AboveFloor,
            _ => Self::RELEASE,
        }
    }
}

/// The longest header a copy above the floor may have, in bytes. The download route reads the first
/// `harness_protocol::bundle_header::HEADER_BYTES` (8,192) bytes of a copy and refuses a header that does not end inside them; a copy
/// whose header would be longer than this (a boundary commit with a very long subject) is written whole.
pub const THIN_HEADER_MAX: usize = 4_096;

/// The files of a tree by path: the blob and its size in bytes.
type TreeFiles = std::collections::BTreeMap<Vec<u8>, (String, u64)>;

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
        let facts = match git_with_env(&self.scratch.repo, &["rev-parse", "HEAD"], &[]).await {
            Ok(head) => self.scratch.git_facts(&self.base, &head).await,
            Err(e) => {
                // Without the head there is no log; the rest of the facts do not need it.
                let mut facts = self.scratch.git_facts(&self.base, &self.base).await;
                facts.errors.push(scrub_detail(&format!("head: {}", ws(e))));
                facts
            }
        };
        // Each reading of a changed tree leaves the new versions of the changed files in `facts.git`, and nothing removed them.
        // The clean-up runs when the repository has grown by a mark, under the lock of the readings; it fails no reading.
        self.scratch.tidy_logged().await;
        facts
    }

    /// Other marks for the clean-up of the private repository than those of a run ([`TidyMarks::RUN`]); tests.
    #[must_use]
    pub fn with_tidy_marks(mut self, marks: TidyMarks) -> Self {
        self.scratch.set_tidy_marks(marks);
        self
    }

    /// Another limit for the clean-up of the private repository than [`TIDY_LIMIT`]; tests.
    #[must_use]
    pub fn with_tidy_limit(mut self, limit: Duration) -> Self {
        self.scratch.set_tidy_limit(limit);
        self
    }
}

impl TreeSource for GitSource {
    fn changed_files(&self, limit: usize) -> BoxFuture<'_, Option<ChangedFiles>> {
        Box::pin(async move {
            let _guard = self.lock.lock().await;
            let found = self.scratch.changed_paths(&self.base, limit).await.ok();
            // The clean-up of `facts.git`, as after [`GitSource::facts`].
            self.scratch.tidy_logged().await;
            let (paths, total) = found?;
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
    /// The marks of this repository's clean-up ([`Self::tidy`]): [`TidyMarks::RUN`], or what a test set.
    marks: TidyMarks,
    /// What the repository held after its last clean-up; nothing before the first.
    after_last: std::sync::Mutex<Loose>,
    /// A clean-up of this repository failed and that was logged ([`Self::tidy_logged`]); it is logged again only after one
    /// that went through.
    tidy_said: AtomicBool,
    /// How long a clean-up of this repository after a reading may take ([`Self::tidy_logged`]): [`TIDY_LIMIT`], or what a
    /// test set.
    tidy_limit: Duration,
    /// The last clean-up found the disk of this repository below its free-space mark, and the log said so ([`Self::say_disk`]).
    disk_low: AtomicBool,
}

impl ScratchRepo {
    /// `<parent>/probe.git`, for the nudge's change test.
    pub async fn new(parent: &Path, repo: &Path) -> Result<Self, String> {
        Ok(Self::open(parent.join("probe.git"), repo)
            .await?
            .tuned()
            .await)
    }

    /// Writes the settings of a private repository of a working session into the repository's own `config`, once: one pack
    /// thread and no delta search (the pack of a copy is uploaded at once, and git otherwise packs on both processors of a session
    /// and searches deltas for it), no automatic `gc` and no untracked cache. Nothing else is set: the index preload, the index
    /// threads and the compression levels stay git's own. agentd's git calls read no system and no global configuration, so this
    /// file is all a private repository has. The four repositories of a working session call it (`snapshot.git`, `goal.git`,
    /// `probe.git`, `facts.git`); the repository of a read of a stopped session's storage does not
    /// ([`Snapshotter::for_read_back`]).
    pub async fn tune(&self) -> Result<(), String> {
        let path = self.git_dir.join("config");
        let config = tokio::fs::read_to_string(&path).await.map_err(io)?;
        if config.contains(TUNE_MARK) {
            return Ok(());
        }
        let mut tuned = config;
        if !tuned.is_empty() && !tuned.ends_with('\n') {
            tuned.push('\n');
        }
        tuned.push_str(TUNE_MARK);
        tuned.push('\n');
        tuned.push_str(TUNE_BLOCK);
        // Written beside the file and renamed: a worker that dies here leaves the file as it was or with its settings.
        let part = self.git_dir.join("config.tuned");
        tokio::fs::write(&part, tuned).await.map_err(io)?;
        tokio::fs::rename(&part, &path).await.map_err(io)
    }

    /// [`Self::tune`] for the makers of a private repository: a repository whose settings cannot be written is used as it is, and
    /// the failure is logged.
    pub(crate) async fn tuned(self) -> Self {
        if let Err(e) = self.tune().await {
            crate::logging::StderrLog.line(&format!(
                "direct turn private repository {}: its settings were not written: {}",
                self.git_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                scrub_detail(&e)
            ));
        }
        self
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
            marks: TidyMarks::RUN,
            after_last: std::sync::Mutex::new(Loose::default()),
            tidy_said: AtomicBool::new(false),
            tidy_limit: TIDY_LIMIT,
            disk_low: AtomicBool::new(false),
        })
    }

    /// [`Self::tidy`] for the repositories whose readings must never fail for a clean-up (`goal.git`, `probe.git`,
    /// `facts.git`): what a clean-up could not do is logged once, until one goes through again, and the reading goes on. Called
    /// after a reading that is made, under the lock that serializes the owner's readings, never inside the time limit of the
    /// reading itself: the clean-up has a limit of its own ([`TIDY_LIMIT`]). One that takes longer is given up, which ends its git
    /// and what git started (`workspace::KillGroup`), is logged like one that failed, and is tried again after the next reading.
    pub async fn tidy_logged(&self) {
        let e = match tokio::time::timeout(self.tidy_limit, self.tidy()).await {
            Ok(Ok(())) => {
                self.tidy_said.store(false, Ordering::SeqCst);
                return;
            }
            Ok(Err(e)) => e,
            Err(_) => format!(
                "given up after {} s and its git ended; it is tried again after the next reading",
                self.tidy_limit.as_secs_f32()
            ),
        };
        if !self.tidy_said.swap(true, Ordering::SeqCst) {
            crate::logging::StderrLog.line(&format!(
                "direct turn private repository {}: clean-up failed: {}",
                self.git_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                scrub_detail(&e)
            ));
        }
    }

    /// Other marks for this repository's clean-up than those of a run ([`TidyMarks::RUN`]); tests.
    pub fn set_tidy_marks(&mut self, marks: TidyMarks) {
        self.marks = marks;
    }

    /// Another limit for a clean-up of this repository after a reading than [`TIDY_LIMIT`]; tests.
    pub fn set_tidy_limit(&mut self, limit: Duration) {
        self.tidy_limit = limit;
    }

    /// What this repository holds of its own (`git count-objects -v`): its loose objects and its packs, never the engine's store.
    pub async fn loose(&self) -> Result<Loose, String> {
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        self.git(cwd, &["count-objects", "-v"], &[("GIT_DIR", &self.git_dir)])
            .await
            .map(|text| Loose::parse(&text))
            .map_err(ws)
    }

    /// The clean-up of this repository, when its marks ask for one ([`TidyMarks::needed`]): `git prune --expire=now` removes
    /// what nothing in the repository names any more (for the copies: the commits of earlier copies and the versions of files that
    /// changed again), and only the repository's own loose objects, never the engine's store behind the alternate. `git add` streams a
    /// file above git's big-file threshold (512 MiB) into a pack of its own, which prune does not shrink: when the packs grew by the
    /// mark, `git repack -a -d -l` puts what is still needed into one pack and removes the others (`-l`: none of the engine's objects;
    /// `--window=0`: no new deltas, the old pack's data is reused). What the repository holds afterwards is what the next clean-up
    /// counts from. A clean-up that only a nearly full disk asks for is left out while the repository holds what it held after the
    /// last one, and the log says when the free-space mark starts and stops forcing clean-ups ([`Self::say_disk`]).
    /// One caller at a time: a prune beside a `git add` in flight can remove an object that add has just written, so each
    /// owner calls it under the lock that serializes its readings of the tree. The error names what could not be done; the owner
    /// logs it and goes on.
    pub async fn tidy(&self) -> Result<(), String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let now = self.loose().await?;
        let after_last = *self.after_last.lock().unwrap_or_else(|p| p.into_inner());
        let free = free_bytes(gd);
        self.say_disk(free);
        let need = self.marks.needed(now, after_last, free);
        // A nearly full disk asks for the clean-up whatever the repository has grown by. When the repository holds exactly what it
        // held after its last clean-up, nothing was added since, and one more clean-up would walk what the repository can reach to
        // free nothing: it is left out. So a session on a nearly full disk pays for a clean-up only after a look or a reading that
        // added to the repository.
        let only_the_disk = self.marks.needed(now, after_last, u64::MAX) == Tidy::No;
        if need == Tidy::No || (only_the_disk && now == after_last) {
            return Ok(());
        }
        let mut failed = Vec::new();
        if let Err(e) = self
            .git(cwd, &["prune", "--expire=now"], &[("GIT_DIR", gd)])
            .await
        {
            failed.push(ws(e));
        }
        if need == Tidy::PruneRepack
            && has_pack(&self.git_dir.join("objects/pack"))
            && let Err(e) = self
                .git(
                    cwd,
                    &["repack", "-a", "-d", "-l", "-q", "--window=0"],
                    &[("GIT_DIR", gd)],
                )
                .await
        {
            failed.push(ws(e));
        }
        match self.loose().await {
            Ok(left) => *self.after_last.lock().unwrap_or_else(|p| p.into_inner()) = left,
            Err(e) => failed.push(e),
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// Says in the worker's log, once, that the disk of this repository has fewer free bytes than its mark, so that the
    /// clean-up runs whatever the repository has grown by, and once more when the disk has room again. Before, nothing in the log
    /// said why a session on a nearly full disk started `git prune` again and again.
    fn say_disk(&self, free: u64) {
        let low = free < self.marks.free_bytes;
        if self.disk_low.swap(low, Ordering::SeqCst) == low {
            return;
        }
        let name = self
            .git_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        crate::logging::StderrLog.line(&if low {
            format!(
                "direct turn private repository {name}: its disk has less than {} bytes free ({free} free): the clean-up runs \
                 after every reading that added to it",
                self.marks.free_bytes
            )
        } else {
            format!(
                "direct turn private repository {name}: its disk has room again ({free} bytes free): the clean-up waits for its \
                 marks again"
            )
        });
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
        let scratch = Self::open(agent_data.join("facts.git"), repo)
            .await?
            .tuned()
            .await;
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

    /// [`Self::read`] whose output comes back as bytes, untrimmed.
    async fn read_bytes(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        git_bytes_with_env(cwd, args, &[("GIT_DIR", &self.git_dir)])
            .await
            .map_err(ws)
    }

    /// The files of the tree `tree` by path, each with its blob and its size (`git ls-tree -r -l -z`). A submodule has no size
    /// and is left out.
    async fn files_of(&self, tree: &str) -> Result<TreeFiles, String> {
        object_id(tree)?;
        let raw = self
            .read_bytes(&["ls-tree", "-r", "-l", "-z", tree])
            .await?;
        let mut files = TreeFiles::new();
        for entry in raw.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            // `<mode> <type> <object> <size>` TAB `<path>`
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&entry[..tab]);
            let mut fields = meta.split_whitespace();
            let (Some(_mode), Some(kind), Some(blob), Some(size)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if kind != "blob" {
                continue;
            }
            let Ok(bytes) = size.parse::<u64>() else {
                continue;
            };
            files.insert(entry[tab + 1..].to_vec(), (blob.to_string(), bytes));
        }
        Ok(files)
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
        self.stage().await?;
        self.write_kept_tree().await
    }

    /// The first half of [`Self::tree`]: the work tree as it is now, staged in the index this repository keeps (`add -A`).
    async fn stage(&self) -> Result<(), String> {
        let index = self.git_dir.join("index");
        self.copy_exclude().await?;
        self.git(&self.repo, &["add", "-A"], &self.envs(&index))
            .await
            .map(|_| ())
            .map_err(ws)
    }

    /// The second half of [`Self::tree`]: the tree of the index this repository keeps (`write-tree`).
    async fn write_kept_tree(&self) -> Result<String, String> {
        let index = self.git_dir.join("index");
        self.git(&self.repo, &["write-tree"], &self.envs(&index))
            .await
            .map_err(ws)
    }

    /// Whether the look that uses this repository now has been given up (its time has come); never outside a look of the copy task.
    async fn given_up(&self) -> bool {
        match &self.give_up {
            Some(give_up) => tokio::time::timeout(Duration::ZERO, give_up.reached())
                .await
                .is_ok(),
            None => false,
        }
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
    /// What the last clean-up ([`ScratchRepo::tidy`]) could not do, for the caller's log ([`Self::take_tidy_error`]).
    tidy_error: Option<String>,
    /// A look hashed a changed tree since the last clean-up: the next look, or the copy of the end, starts with one.
    tidy_due: bool,
    /// The last look of the copy task ended in an error, so a git of it may have been ended in the middle: the next one clears the
    /// lock files first ([`Self::snapshot_until`]).
    gave_up: bool,
    /// What came of the first reading of the tree ([`Self::new_primed`]), for the caller's log ([`Self::take_prime_note`]).
    prime_note: Option<Result<Duration, String>>,
    /// The copy just made holds the whole tree because the tree could not be staged in the index this repository keeps,
    /// twice ([`Self::current_tree`]): the reason, for the caller's log ([`Self::take_whole_note`]).
    whole_note: std::sync::Mutex<Option<String>>,
    /// What the bundles of this session's copies hold ([`Self::set_form`]); [`BundleForm::Whole`] until it is set.
    form: BundleForm,
    /// The commits a copy above the floor leaves out with everything they reach ([`Self::set_floors`]): heads of the bundle the
    /// run started from that this repository can read. Empty: every bundle is whole.
    floors: Vec<String>,
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
    /// Creates the snapshot repository for the worktree at `repo`, whose branch starts at `base`, and reads the tree once
    /// ([`Self::new_primed`] with [`PRIME_LIMIT`]).
    pub async fn new(agent_data: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        Self::new_primed(agent_data, repo, base, Some(PRIME_LIMIT)).await
    }

    /// [`Self::new`] with the limit of the first reading of the tree given: `prime`. `None` makes no first reading, as
    /// (tests: the reading changes no decision of the first look).
    ///
    /// The index of the repository is seeded with `git read-tree`, so it names every file of the base and knows the state of
    /// none. The first look of the copy task therefore read and hashed every file (3.65 s of git on the large repository of the
    /// measurement), and with no look made yet the first tool call that ended started it at once, in front of the engine's next tool
    /// call. So the tree is read once here (`git add -A` and `git write-tree` in the private repository), before the engine starts,
    /// where it competes with nothing: the index then knows the state of the files and the first look costs what a look at an
    /// unchanged tree costs. The result is dropped: the tree the first look compares with stays the tree of the commit the session
    /// starts on, so that look decides as it did. A reading that fails or runs longer than `prime` is given up, with its git ended,
    /// and is no failure: the first look then reads every file, as before ([`Self::take_prime_note`] says what came of it).
    pub async fn new_primed(
        agent_data: &Path,
        repo: &Path,
        base: &str,
        prime: Option<Duration>,
    ) -> Result<Self, String> {
        let git_dir = agent_data.join("snapshot.git");
        let scratch = ScratchRepo::open(git_dir.clone(), repo)
            .await?
            .tuned()
            .await;
        // No reflog keeps the commit of an earlier copy, and so its files, from `git prune` ([`ScratchRepo::tidy`]); a
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
        let mut made = Self {
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
            prime_note: None,
            whole_note: std::sync::Mutex::new(None),
            form: BundleForm::Whole,
            floors: Vec::new(),
        };
        if let Some(limit) = prime {
            made.prime(limit).await;
        }
        Ok(made)
    }

    /// The first reading of the tree ([`Self::new_primed`]), given up after `limit`. Its git calls and its file calls on the
    /// work tree end when it is given up, as those of a look do, and the lock files an ended git left are removed, so the first look
    /// finds the repository as a look finds it.
    async fn prime(&mut self, limit: Duration) {
        let started = Instant::now();
        self.scratch.give_up = Some(GiveUp::at(Some(tokio::time::Instant::now() + limit)));
        let read = self.scratch.tree().await;
        self.scratch.give_up = None;
        self.prime_note = Some(match read {
            Ok(_) => Ok(started.elapsed()),
            Err(e) => {
                self.clear_stale_locks();
                Err(e)
            }
        });
    }

    /// What came of the first reading of the tree, once: how long it took, or why it was given up. `None` when none was made.
    pub fn take_prime_note(&mut self) -> Option<Result<Duration, String>> {
        self.prime_note.take()
    }

    /// Why the copy just made holds the whole tree, once, when it does because the tree could not be staged in the index
    /// this repository keeps ([`Self::current_tree`]). Such a copy has no parent and lacks the files that only the base tracks and
    /// an ignore rule now matches; the run is told that a copy was stored, so the worker's log is where the difference is said.
    pub fn take_whole_note(&mut self) -> Option<String> {
        self.whole_note
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Removes what no copy needs any more (the commits of earlier copies and the versions of files that changed again
    /// since), when a look hashed a changed tree since the last time and the marks of the repository ask for a clean-up
    /// ([`ScratchRepo::tidy`]: 64 MiB of loose objects, 6,700 loose objects or 64 MiB of packs more than after the last clean-up, or
    /// less than 1 GiB free on the disk). `git prune` keeps what `refs/heads/snapshot` and the private index name. It runs under the
    /// copy lock at the start of the look after one that hashed a changed tree, and of the copy of the end, never between a copy and
    /// its upload: the time from reading the tree to storing its copy is what it was without it, and the first copy of a session waits
    /// for nothing. The time it takes counts as git time for the spacing. What it could not do does not fail the copy: the caller logs
    /// it ([`Self::take_tidy_error`]). The repository of a read of a stopped session's storage never runs it.
    async fn tidy_if_due(&mut self) {
        if std::mem::take(&mut self.tidy_due) {
            self.tidy_error = self.scratch.tidy().await.err();
        }
    }

    /// What the last clean-up of the repository could not do ([`ScratchRepo::tidy`]), once.
    pub fn take_tidy_error(&mut self) -> Option<String> {
        self.tidy_error.take()
    }

    /// Other marks for the clean-up of this repository than those of a run ([`TidyMarks::RUN`]); tests.
    pub fn set_tidy_marks(&mut self, marks: TidyMarks) {
        self.scratch.set_tidy_marks(marks);
    }

    /// The copies of this session go on counting after `before`, the number of the newest copy the run saved before the session.
    /// Called once, before the first copy.
    pub fn count_from(&mut self, before: u32) {
        self.count = before;
    }

    /// What the bundles of this session's copies hold. Called once, before the first copy.
    pub fn set_form(&mut self, form: BundleForm) {
        self.form = form;
    }

    /// The commits a copy above the floor leaves out with everything they reach: heads of the bundle the run started from,
    /// read from that bundle's own header, each of which this repository can read ([`Self::has_commit`]). Every reader of a copy has
    /// everything those heads reach once it has taken the base, so any of them is a sound floor and no ancestor check is needed. Called
    /// once, before the first copy; an id that is no full object id is left out.
    pub fn set_floors(&mut self, floors: Vec<String>) {
        self.floors = floors
            .into_iter()
            .filter(|f| object_id(f).is_ok())
            .collect();
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
    ///
    /// A `git add` that failed is no such index. A git that died in the middle of it (a bus error on a file that was rewritten in
    /// place while git read it) leaves `index.lock`, and so does a git that was ended. The copy of the whole tree was then made in its
    /// place: it left out the files that only the kept index tracks, the look said that its copy was stored, the lock stayed, and the
    /// next look could not stage at all. So the lock files are removed as soon as a staging has failed (the git that held them has
    /// ended: only agentd uses this repository, one caller at a time) and the tree is staged once more in the kept index. Only when
    /// that fails too is the copy the whole tree, as before: a look that can make a copy never makes none. A look that was given up
    /// stages nothing a second time. No lock file is left behind by a staging that failed, whatever the look then says.
    ///
    /// A copy of the whole tree that is made because the staging failed twice lacks the files that only the base tracks and an
    /// ignore rule now matches, and the run is told that a copy was stored. The reason is kept for the worker's log
    /// ([`Self::take_whole_note`]), so that whoever reads why such a copy has no parent finds it.
    async fn current_tree(&self) -> Result<String, String> {
        let staged = match self.scratch.stage().await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.clear_stale_locks();
                if self.scratch.given_up().await {
                    Err(e)
                } else {
                    self.scratch.stage().await.map_err(|_| {
                        self.clear_stale_locks();
                        e
                    })
                }
            }
        };
        let (e, staging_failed) = match staged {
            Ok(()) => match self.scratch.write_kept_tree().await {
                Ok(tree) => return Ok(tree),
                Err(e) => (e, false),
            },
            Err(e) => (e, true),
        };
        match self.scratch.whole_tree().await {
            Ok(whole) => {
                if staging_failed {
                    *self.whole_note.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                        "the tree could not be staged in the index the worker keeps, at two tries ({e}); the copy holds the whole \
                         tree as it is now, without a parent and without the files that only the base tracks"
                    ));
                }
                Ok(whole)
            }
            Err(_) => {
                self.clear_stale_locks();
                Err(e)
            }
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
        self.commit_with(tree, parent, &format!("harness: snapshot {n}"))
            .await
    }

    /// [`Self::commit_copy`] with the subject of the commit given.
    async fn commit_with(
        &self,
        tree: String,
        parent: Option<&str>,
        message: &str,
    ) -> Result<(String, String), String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        if let Some(parent) = parent
            && self.scratch.has_commit(parent).await
        {
            let commit = self
                .scratch
                .git(
                    cwd,
                    &["commit-tree", &tree, "-p", parent, "-m", message],
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
                &["commit-tree", &whole, "-m", message],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        Ok((commit, whole))
    }

    /// Writes the bundle of `refs/heads/snapshot` and returns where it is and its size. Run from agentd's own directory.
    ///
    /// With [`BundleForm::AboveFloor`] and a floor the bundle leaves out what the floors reach ([`Self::write_bundle_above`]).
    /// In every other case, and whenever that path fails or says that this copy must be whole, the bundle holds everything the copy's
    /// commit reaches, as it always did: a copy is never lost to the form it was to have.
    async fn write_bundle(&self, form: BundleForm) -> Result<(PathBuf, u64), String> {
        let bundle = self.git_dir.join("snapshot.bundle");
        if form == BundleForm::AboveFloor
            && !self.floors.is_empty()
            && let Ok(Some(bytes)) = self.write_bundle_above(&bundle).await
        {
            return Ok((bundle, bytes));
        }
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
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

    /// Writes the bundle of `refs/heads/snapshot` without what the floors reach, and returns its size; `None` when this copy
    /// must be whole. An error is a git call that failed, and the caller then writes the bundle whole too.
    ///
    /// What is left out: when the copy's commit is above the floors (`git rev-list --count refs/heads/snapshot ^<floor>...` answers 1
    /// or more), the floors. When it answers 0 the ref names a commit the base holds, which only the copy of the end does (a session
    /// that changed no file and made no commit, a continued run, a reset to an earlier commit): git refuses a bundle with nothing in
    /// it, so the head's own parents are left out instead (`refs/heads/snapshot^@`). The ref still names the session's final head, the
    /// bundle holds that one commit, and what it needs are the head's parents, which the base holds. A root commit has no parent: its
    /// copy is whole. A count that fails is no zero.
    ///
    /// git writes the commits it left out as prerequisite lines of the bundle's header (bundle version 2). The header is read back:
    /// one that cannot be read, or that is longer than [`THIN_HEADER_MAX`], makes the copy whole.
    async fn write_bundle_above(&self, bundle: &Path) -> Result<Option<u64>, String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let below_floors: Vec<String> = self.floors.iter().map(|f| format!("^{f}")).collect();
        let mut count_args = vec!["rev-list", "--count", "refs/heads/snapshot"];
        count_args.extend(below_floors.iter().map(String::as_str));
        let above: u64 = self
            .scratch
            .git(cwd, &count_args, &[("GIT_DIR", gd)])
            .await
            .map_err(ws)?
            .trim()
            .parse()
            .map_err(|_| "snapshot: git rev-list --count answered no number".to_string())?;
        let left_out: Vec<String> = if above >= 1 {
            below_floors
        } else {
            let parents = self
                .scratch
                .git(
                    cwd,
                    &["rev-parse", "refs/heads/snapshot^@"],
                    &[("GIT_DIR", gd)],
                )
                .await
                .map_err(ws)?;
            let parents: Vec<String> = parents
                .split_whitespace()
                .filter(|p| object_id(p).is_ok())
                .map(|p| format!("^{p}"))
                .collect();
            if parents.is_empty() {
                return Ok(None);
            }
            parents
        };
        let _ = std::fs::remove_file(bundle);
        let mut args = vec![
            "bundle",
            "create",
            "-q",
            "--version=2",
            bundle.to_str().unwrap_or_default(),
            "refs/heads/snapshot",
        ];
        args.extend(left_out.iter().map(String::as_str));
        self.scratch
            .git(cwd, &args, &[("GIT_DIR", gd)])
            .await
            .map_err(ws)?;
        // The header as a reader will see it: it must end inside the bytes every reader takes, and say what the bundle needs.
        let mut prefix = Vec::new();
        std::fs::File::open(bundle)
            .map_err(io)?
            .take(harness_protocol::bundle_header::HEADER_BYTES_MAX)
            .read_to_end(&mut prefix)
            .map_err(io)?;
        let header_len = prefix
            .windows(2)
            .position(|w| w == b"\n\n")
            .map(|at| at + 2);
        match (harness_protocol::bundle_header::needs(&prefix), header_len) {
            (Ok(_), Some(len)) if len <= THIN_HEADER_MAX => {}
            _ => return Ok(None),
        }
        Ok(Some(std::fs::metadata(bundle).map_err(io)?.len()))
    }

    /// The private repository of a read of a stopped session's storage, `<dir>/snapshot.git`, for the work tree at `repo`. Unlike
    /// [`Self::new`] it is made whether or not the history can be read: the engine may have removed or replaced `.git` before its session
    /// was stopped, and the tree is still the work. Nothing is run in the engine's repository here and nothing is written to it, ever.
    pub async fn for_read_back(dir: &Path, repo: &Path, base: &str) -> Result<Self, String> {
        let git_dir = dir.join("snapshot.git");
        let scratch = ScratchRepo::open(git_dir.clone(), repo).await?;
        Ok(Self {
            scratch,
            git_dir,
            repo: repo.to_path_buf(),
            base: base.to_string(),
            last_tree: String::new(),
            last_parent: None,
            count: 0,
            tidy_error: None,
            tidy_due: false,
            gave_up: false,
            prime_note: None,
            whole_note: std::sync::Mutex::new(None),
            form: BundleForm::Whole,
            floors: Vec::new(),
        })
    }

    /// The copy of a read of a stopped session's storage: the tree as the storage holds it, as one commit on `refs/heads/snapshot`
    /// and the bundle of that ref, made like a copy of the copy task. Its parent is the engine's `HEAD` when the run's base is behind
    /// it, else `start_head` (the commit the session started on) when this repository can read it. With neither, the history cannot be
    /// read and the copy holds the whole tree with no parent ([`ScratchRepo::whole_tree`]): the tree is never thrown away because
    /// `.git` is gone. The tree is hashed twice; equal trees are a stable copy, and when they differ the pair is hashed once more. The
    /// commit's subject names the session and carries no copy number. Every git call and every file call on the work tree ends when
    /// `give_up` says so.
    pub async fn read_back(
        &mut self,
        start_head: &str,
        session: u32,
        give_up: &GiveUp,
    ) -> Result<ReadBack, String> {
        self.scratch.give_up = Some(give_up.clone());
        let made = self.read_back_kept(start_head, session).await;
        self.scratch.give_up = None;
        made
    }

    async fn read_back_kept(&mut self, start_head: &str, session: u32) -> Result<ReadBack, String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let base = self.base.clone();
        let parent = match self.engine_head().await {
            Some(head) if self.behind(&base, &head).await => Some(head),
            _ if self.scratch.has_commit(start_head).await => Some(start_head.to_string()),
            _ => None,
        };
        // What the base tracks stays tracked whatever an ignore rule says, while the base can be read.
        if parent.is_some() && self.scratch.has_commit(&base).await {
            let index = self.git_dir.join("index");
            let _ = self
                .scratch
                .git(cwd, &["read-tree", &base], &self.scratch.envs(&index))
                .await;
        }
        let first = self.current_tree().await?;
        let second = self.current_tree().await?;
        let (seen, mut stable) = if first == second {
            (second, true)
        } else {
            let third = self.current_tree().await?;
            let fourth = self.current_tree().await?;
            let same = third == fourth;
            (fourth, same)
        };
        let (commit, tree) = self
            .commit_with(
                seen.clone(),
                parent.as_deref(),
                &format!("harness: read back {session}"),
            )
            .await?;
        // A copy without a parent hashes the tree once more, into an index of its own: a tree that moved since is not a stable one.
        stable = stable && tree == seen;
        let parent = self
            .scratch
            .read(&["rev-parse", "--verify", "-q", &format!("{commit}^")])
            .await
            .ok()
            .filter(|p| !p.is_empty());
        self.scratch
            .git(
                cwd,
                &["update-ref", "refs/heads/snapshot", &commit],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        // The read-back bundle holds the whole history, whatever the form of a session's copies is.
        let (bundle, bundle_bytes) = self.write_bundle(BundleForm::Whole).await?;
        Ok(ReadBack {
            commit,
            tree,
            parent,
            stable,
            bundle,
            bundle_bytes,
        })
    }

    /// Whether the commit `ancestor` is behind the commit `head` in what this repository can read.
    async fn behind(&self, ancestor: &str, head: &str) -> bool {
        object_id(ancestor).is_ok()
            && object_id(head).is_ok()
            && self
                .scratch
                .read(&["merge-base", "--is-ancestor", ancestor, head])
                .await
                .is_ok()
    }

    /// How many files the tree `tree` holds and how many bytes they have.
    pub async fn totals(&self, tree: &str) -> Result<(u64, u64), String> {
        let files = self.scratch.files_of(tree).await?;
        let bytes = files.values().map(|(_, n)| *n).sum();
        Ok((files.len() as u64, bytes))
    }

    /// Whether this repository can read the commit `commit`, from its own objects or the engine's store.
    pub async fn has_commit(&self, commit: &str) -> bool {
        self.scratch.has_commit(commit).await
    }

    /// Brings the copy in the bundle `bundle` (its `refs/heads/snapshot`) into this repository and returns its commit: the saved
    /// copy a read compares with.
    pub async fn fetch_copy(&self, bundle: &Path) -> Result<String, String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        self.scratch
            .git(
                cwd,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    bundle.to_str().unwrap_or_default(),
                    "+refs/heads/snapshot:refs/baseline/snapshot",
                ],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        self.scratch
            .read(&["rev-parse", "--verify", "refs/baseline/snapshot^{commit}"])
            .await
    }

    /// Brings what the bundle the run started from holds into this repository (every ref of `bundle` under `refs/base/`),
    /// so that a saved copy that needs it can be fetched ([`Self::fetch_copy`]) when the stopped session's own store no longer
    /// holds it. Nothing reads those refs afterwards.
    pub async fn fetch_base(&self, bundle: &Path) -> Result<(), String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        self.scratch
            .git(
                cwd,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    bundle.to_str().unwrap_or_default(),
                    "+refs/*:refs/base/*",
                ],
                &[("GIT_DIR", gd)],
            )
            .await
            .map(|_| ())
            .map_err(ws)
    }

    /// Compares the tree `copy_tree` of a copy read back with the saved copy `baseline` (a commit this repository can read).
    ///
    /// A file of the copy read back that is a strict prefix of the same file in the saved copy is counted (`regressed`): the saved copy
    /// holds more of it than the storage did, which is what an open, unsynced file leaves. The bytes are compared only when the file on
    /// the storage is the shorter one; a pair whose longer side is above `max_prefix_bytes` is counted unread. A shorter file that is no
    /// prefix and a longer file are not counted.
    ///
    /// A file that the saved copy holds and the copy read back lacks is counted with them when the commit below the saved copy does
    /// not hold it either: the saved copy added the file, and the storage does not have it. A working session's copy reads the tree
    /// through the machine's file cache, so it can hold a file that never reached the storage. The same tree is left by an engine that
    /// removed a file it had just made, so this is doubt and no proof, and on doubt the run goes on from the saved copy. A file that
    /// the commit below already held and the storage lacks was removed since: a deletion, which is not counted. A saved copy without
    /// a commit below it has no earlier state to tell the two apart, and nothing is counted for it.
    ///
    /// A file of the copy read back that equals the file in the commit below the saved copy, while the saved copy holds another version,
    /// is counted too (`rewritten`): the storage holds the earlier version of a file that was rewritten in place. Only ids are compared.
    pub async fn compare(
        &self,
        copy_tree: &str,
        baseline: &str,
        max_prefix_bytes: u64,
        paths_shown: usize,
    ) -> Result<Compared, String> {
        let base_tree = self.scratch.tree_of(baseline).await?;
        if base_tree == copy_tree {
            return Ok(Compared {
                same: true,
                ..Compared::default()
            });
        }
        let base_files = self.scratch.files_of(&base_tree).await?;
        let copy_files = self.scratch.files_of(copy_tree).await?;
        // The commit below the saved copy, when it has one. A saved copy without a parent has no earlier version to compare with; a
        // parent that cannot be read is an error, so the comparison is unknown and never "done" with a check left out.
        let parents = self
            .scratch
            .read(&["rev-list", "--parents", "-n", "1", baseline])
            .await?;
        let below = match parents.split_whitespace().nth(1) {
            Some(parent) => {
                let tree = self.scratch.tree_of(parent).await?;
                Some(self.scratch.files_of(&tree).await?)
            }
            None => None,
        };
        let mut found = Compared::default();
        let show = |paths: &mut Vec<String>, path: &[u8]| {
            if paths.len() < paths_shown {
                paths.push(scrub_detail(&String::from_utf8_lossy(path)));
            }
        };
        for (path, (copy_blob, copy_bytes)) in &copy_files {
            let Some((base_blob, base_bytes)) = base_files.get(path) else {
                continue;
            };
            if base_blob == copy_blob {
                continue;
            }
            if below
                .as_ref()
                .and_then(|files| files.get(path))
                .is_some_and(|(blob, _)| blob == copy_blob)
            {
                found.rewritten += 1;
                show(&mut found.rewritten_paths, path);
            }
            if copy_bytes < base_bytes {
                let prefix = if *base_bytes > max_prefix_bytes || *copy_bytes == 0 {
                    true
                } else {
                    let long = self
                        .scratch
                        .read_bytes(&["cat-file", "blob", base_blob])
                        .await?;
                    let short = self
                        .scratch
                        .read_bytes(&["cat-file", "blob", copy_blob])
                        .await?;
                    long.starts_with(&short)
                };
                if prefix {
                    found.regressed += 1;
                    show(&mut found.regressed_paths, path);
                }
            }
        }
        // The files the saved copy added that the storage lacks.
        if let Some(below) = &below {
            for path in base_files.keys() {
                if !copy_files.contains_key(path) && !below.contains_key(path) {
                    found.regressed += 1;
                    show(&mut found.regressed_paths, path);
                }
            }
        }
        Ok(found)
    }

    /// The copy of the session's end, made when the engine and everything it started have stopped. With `head` (the session's
    /// final head, once the end commit is made) the `snapshot` ref names that commit itself: the copy is the head a delivery is planned
    /// from. Without one (the end commit failed, or agentd may run no git in the checkout) the copy is the tree as it is, on the last
    /// head a periodic copy read (the base when there was none): no git call reads the engine's repository for it. When that head cannot
    /// be read any more (the engine removed or replaced `.git`) the copy holds the whole tree and has no parent ([`Self::commit_copy`]).
    /// A copy is made whether or not the tree changed, so the newest record always says what the end state is.
    ///
    /// It starts with the clean-up a look left due ([`Self::tidy_if_due`]), after the lock files are cleared.
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
        let (bundle, bundle_bytes) = self.write_bundle(self.form).await?;
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
            digest: None,
        })
    }

    /// Takes a snapshot of the tree as it is now; `None` when it equals the last one. Its parent is the engine's HEAD
    /// ([`Self::engine_head`]); without one, the last head a copy read (the base when there was none), as for the copy of the end; a parent
    /// this repository cannot read makes the copy hold the whole tree ([`Self::commit_copy`]).
    ///
    /// It starts with [`Self::tidy_if_due`] when the look before it hashed a changed tree, also when a later step of that look failed,
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
        let (bundle, bundle_bytes) = self.write_bundle(self.form).await?;
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
            digest: None,
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
