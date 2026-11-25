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
    git_with_env_until, git_with_env_until_input,
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

    /// Whether the time to give up has come; never while there is none. The answer is for this instant and is given at once. The
    /// copy task asks the same of its look by waiting no time for [`Self::reached`] (`ScratchRepo::given_up`).
    pub fn has_come(&self) -> bool {
        let at = *self.at.borrow();
        at.is_some_and(|at| tokio::time::Instant::now() >= at)
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
///
/// A look of the timer or of a tool call whose git work makes this spacing hold the next look past its tick has left
/// the timer ([`Cadence::holds_past_tick`]). The spacing itself is unchanged: the looks after such a look come when it says. A keep of the
/// run's commits may follow that look at once ([`Snapshotter::keep_at_once`]), and a keep in flight runs on through the wait for
/// this spacing, until the next look starts.
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

    /// Whether the spacing holds the look after a look of the timer or of a tool call past its tick: the look started at
    /// `started`, its git work ended at `git_ended`, and at the tick, one period of the timer after `started` (every look arms
    /// the timer again when it starts), [`Cadence::delay`] still asks for a
    /// wait. No number of its own: the timer and the spacing decide. At the default timer of 15 s it is a look whose git work
    /// took more than 1.5 s, whatever made it slow. `false` when the copies are off.
    pub fn holds_past_tick(&self, started: Instant, git_ended: Instant) -> bool {
        let Some(every) = self.period(false) else {
            return false;
        };
        let mut clock = CopyClock::default();
        clock.looked(started, git_ended, None);
        !self.delay(&clock, false, started + every).is_zero()
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
    /// Reads what the engine's store holds as packs ([`Self::engine_packs`]): [`engine_packs_of`], or what a test set.
    packs_reader: PacksQuestion,
    /// A read of the engine's packs has not returned ([`Self::engine_packs`]); the thread that makes the read clears it.
    packs_busy: Arc<AtomicBool>,
    /// Answers whether `.git` of the engine's repository is the repository's own directory: [`git_dir_is_own`], or what a
    /// test set ([`Snapshotter::set_git_dir_reader`]).
    git_dir_reader: GitDirQuestion,
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
            packs_reader: PacksQuestion(Arc::new(engine_packs_of)),
            packs_busy: Arc::new(AtomicBool::new(false)),
            git_dir_reader: GitDirQuestion(Arc::new(git_dir_is_own)),
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
    /// What this repository knows for the keep of the run's commits ([`Self::keep_commits`]).
    keep: KeepState,
    /// How long the steps of the look in progress took, for the worker's log line of the look ([`Self::take_look_steps`]).
    steps: LookSteps,
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
        Self::new_primed_with_free(agent_data, repo, base, prime, None).await
    }

    /// [`Self::new_primed`] with the free bytes that the keep's rules read in place of those of the disk of the private
    /// repository, from the worker's start on: `None` reads the disk ([`Self::set_keep_free_bytes`]). Tests: a disk that is short
    /// when a worker starts cannot be had from a real one.
    pub async fn new_primed_with_free(
        agent_data: &Path,
        repo: &Path,
        base: &str,
        prime: Option<Duration>,
        free: Option<u64>,
    ) -> Result<Self, String> {
        let git_dir = agent_data.join("snapshot.git");
        // Root invariant 9 wins over the keep at a worker's start too, before the start writes anything into the private
        // repository. The pack an earlier worker kept is looked at first, and it does not stay on a disk that has less free than
        // the keep's rule asks ([`Self::keep_rule_short`], the one function that a look asks too; at a start the last bundle is 0
        // and the marks are those of a run): its bytes are free before `git init`, the config calls, the seed and the first
        // reading write anything, and no check is paid for a pack that goes. Its head stays marked as tried, as after
        // [`Self::keep_gives_way`]. One question to the worker's own disk, asked only while a pack stands; no git call.
        let mut kept = kept_at_open(&git_dir);
        let short = kept.is_some()
            && Self::keep_rule_short(
                &TidyMarks::RUN,
                0,
                0,
                free.unwrap_or_else(|| free_bytes(&git_dir)),
            )
            .is_some();
        let gave_way = kept.take_if(|_| short).map(|pack| {
            remove_pack(&git_dir.join(KEPT_DIR).join("pack"), &pack.name);
            pack.head
        });
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
            keep: KeepState::default(),
            steps: LookSteps::default(),
        };
        made.keep.kept = kept;
        made.keep.free = free.into_iter().collect();
        // A run whose check of a kept pack did not end once keeps no pack any more ([`KEPT_OFF`]).
        made.keep.run_off = std::fs::symlink_metadata(made.git_dir.join(KEPT_DIR).join(KEPT_OFF))
            .is_ok_and(|mark| mark.is_file());
        if made.keep.run_off {
            made.drop_kept();
        }
        made.check_kept(prime.unwrap_or(KEPT_CHECK_LIMIT)).await;
        made.keep.tried = made
            .keep
            .kept
            .as_ref()
            .map(|kept| kept.head.clone())
            .or(gave_way);
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
        let (repo, is_own) = (self.repo.clone(), self.scratch.git_dir_reader.0.clone());
        let own = self
            .scratch
            .on_tree("its git directory", move || is_own(&repo))
            .await
            .ok()?;
        if !own {
            return None;
        }
        let head = self.engine_head_id().await?;
        self.scratch.has_commit(&head).await.then_some(head)
    }

    /// What the engine's `HEAD` names: one `git rev-parse` in the engine's repository and no file call of agentd. The caller has
    /// asked whether `.git` is the repository's own directory: a look under the flag of its file calls ([`Self::engine_head`]),
    /// the keep at once under the flag of the packs' read ([`ScratchRepo::engine_packs_if_own`]). Whether this repository can
    /// read that commit is not asked.
    async fn engine_head_id(&self) -> Option<String> {
        self.scratch
            .git(&self.repo, &["rev-parse", "HEAD"], &[])
            .await
            .ok()
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
        // The two git calls of a look that walk the run's commits read the kept pack, when this look may
        // ([`Self::keep_check`]). It is an object folder beside this repository's own store, named for these calls alone.
        let kept_dir = self.git_dir.join(KEPT_DIR);
        let mut envs: Vec<(&str, &Path)> = vec![("GIT_DIR", gd)];
        if self.keep.use_kept {
            envs.push(("GIT_ALTERNATE_OBJECT_DIRECTORIES", &kept_dir));
        }
        let mut count_args = vec!["rev-list", "--count", "refs/heads/snapshot"];
        count_args.extend(below_floors.iter().map(String::as_str));
        let above: u64 = self
            .scratch
            .git(cwd, &count_args, &envs)
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
        self.scratch.git(cwd, &args, &envs).await.map_err(ws)?;
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
            keep: KeepState::default(),
            steps: LookSteps::default(),
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
    /// It starts with the clean-up a look left due ([`Self::tidy_if_due`]), after the lock files are cleared. Before
    /// both, a kept pack gives way on a short disk ([`Self::keep_gives_way`]).
    pub async fn final_copy(&mut self, head: Option<&str>) -> Result<SnapshotMade, String> {
        // The copy of the end times its steps as a look does, for the worker's save line ([`Self::take_look_steps`]); it reads no
        // head of the engine, so `head` stays 0.
        self.steps = LookSteps::default();
        let at = Instant::now();
        // The copy of the end has its caller's budget; a look that was aborted in the middle must not leave its own limit behind.
        self.scratch.give_up = None;
        // Root invariant 9 over the keep comes first: on a short disk the bytes of a kept pack are the copy's.
        self.keep_gives_way();
        self.clear_stale_locks();
        // The copy of the end reads no kept pack and keeps none: nothing is in front of it, and a delivery is planned from it.
        self.keep.use_kept = false;
        self.tidy_if_due().await;
        self.steps.tidy = at.elapsed();
        let made = self.final_copy_kept(head).await;
        self.tidy_due = true;
        made
    }

    async fn final_copy_kept(&mut self, head: Option<&str>) -> Result<SnapshotMade, String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let n = self.count + 1;
        let read_unix = now_unix();
        let at = Instant::now();
        let tree = match head {
            Some(head) => self.scratch.tree_of(head).await?,
            None => self.current_tree().await?,
        };
        self.steps.tree = at.elapsed();
        let at = Instant::now();
        let (commit, tree) = match head {
            Some(head) => (head.to_string(), tree),
            None => {
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
        self.steps.commit = at.elapsed();
        let at = Instant::now();
        let (bundle, bundle_bytes) = self.write_bundle(self.form).await?;
        self.steps.bundle = at.elapsed();
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
        self.steps = LookSteps::default();
        let at = Instant::now();
        // Root invariant 9 over the keep comes first, before anything is written into the private repository.
        self.keep_gives_way();
        // What the engine's packs are when a look of the timer starts, for the keep that may follow it at once.
        let packs_read = self.note_look_packs().await;
        self.tidy_if_due().await;
        self.steps.tidy = at.elapsed().saturating_sub(packs_read);
        self.steps.head = packs_read;
        let made = self.snapshot_kept().await;
        if !matches!(made, Ok(None)) {
            self.tidy_due = true;
        }
        made
    }

    async fn snapshot_kept(&mut self) -> Result<Option<SnapshotMade>, String> {
        let read_unix = now_unix();
        self.keep.rest_head = None;
        self.keep.due_at_once = false;
        let at = Instant::now();
        let tree = self.current_tree().await?;
        self.steps.tree = at.elapsed();
        if tree == self.last_tree {
            return Ok(None);
        }
        let gd: &Path = &self.git_dir;
        let at = Instant::now();
        let head = self.engine_head().await;
        self.steps.head += at.elapsed();
        if self.keep.look_packs.is_some() {
            self.keep.look_head.clone_from(&head);
        }
        let parent = head
            .clone()
            .or_else(|| self.last_parent.clone())
            .unwrap_or_else(|| self.base.clone());
        let n = self.count + 1;
        let at = Instant::now();
        let (commit, tree) = self.commit_copy(tree, Some(&parent), n).await?;
        self.scratch
            .git(
                self.git_dir.parent().unwrap_or(Path::new("/")),
                &["update-ref", "refs/heads/snapshot", &commit],
                &[("GIT_DIR", gd)],
            )
            .await
            .map_err(ws)?;
        self.steps.commit = at.elapsed();
        let at = Instant::now();
        self.keep_check(head.as_deref()).await;
        let (bundle, bundle_bytes) = self.write_bundle(self.form).await?;
        self.steps.bundle = at.elapsed();
        // A look that read the kept pack is followed by no keep at once.
        self.keep.look_read_kept = self.keep.use_kept;
        self.steps.engine = self.keep.note_head(head.as_deref(), bundle_bytes);
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

// ----: the kept pack of the run's commits -----------------------------------------------------------------------------------
//
// What costs in a look is loose objects: every copy above the base packs every commit the run made again, and git opens each loose
// object of those commits in the engine's store on the session's storage. So the objects of the run's commits that are in no pack
// are packed once into a folder of this repository on the worker's own disk (the keep), and the two git calls of a look that walk
// the run's commits read them there. Root invariant 9 binds every line: a keep never fails, delays, thins or loses a copy.
//
// - The pack is no part of this repository's own object store. It lies in `kept/pack/`, an object folder of its own that only
//   `rev-list --count` and `bundle create` of a look are given (`GIT_ALTERNATE_OBJECT_DIRECTORIES`). `add -A`, `write-tree`,
//   `commit-tree`, `prune`, `repack` and `count-objects` never see it, so the index, the refs and the loose objects of this
//   repository are what they are without a keep, and removing the kept pack at any moment leaves the repository of release 5.25.
//   In `objects/pack` the pack would make `git prune` remove the loose copy of a blob that it also holds, and when the pack is
//   replaced no store would hold a blob the index still names.
// - It holds what is in no pack (`pack-objects --revs --unpacked --incremental`): what the engine has packed is read from the
//   engine's pack, in the form the engine's pack has it, as in release 5.25. A pack of every commit of the run would hold whole
//   objects where the engine's pack holds deltas against objects below the floors, and the copy would grow.
// - A kept pack and a pack of the engine never hold the same object when a bundle is written. The names of the engine's packs
//   are read before a keep and after it (a keep during which the engine repacked is a miss) and at every look that would read
//   the kept pack ([`Snapshotter::keep_check`]): when they have changed, the kept pack is removed before the bundle is written and
//   the next look at rest keeps again. Once the engine packs its own commits (`git gc`), its pack holds them as deltas where it
//   can; a bundle that took the kept pack's whole objects instead would be many times the copy of release 5.25. Which of two
//   packs git reads first differs between git versions (by the files' times in one, by the order of the object folders in
//   another), so nothing here rests on it.
// - The keep runs between two looks, never inside one, and the start of the next look ends it.
// - A keep follows the second look in a row that found the same head, and it follows a look at once when that look left
//   the timer while the engine's head and packs stood still ([`Snapshotter::keep_at_once`]): after a burst of commits that
//   nothing packed, one look reads the loose objects, not two.

/// The folder of the private repository of the copies that holds the kept pack, an object folder of its own.
const KEPT_DIR: &str = "kept";
/// How the first line of the marker of a kept pack starts; the engine's head the pack was kept for follows.
const KEPT_LINE: &str = "agentd kept ";
/// How the second line of the marker starts; what the engine's own packs were when the pack was kept follows
/// ([`ScratchRepo::engine_packs`]).
const KEPT_PACKS_LINE: &str = "engine packs ";
/// The most packs of the engine's store whose names are read ([`ScratchRepo::engine_packs`]); a store with more keeps no pack.
const ENGINE_PACKS_MAX: usize = 4_096;
/// What a read of the engine's packs answers while an earlier read has not returned ([`ScratchRepo::off_the_looks_flag`]).
/// A keep that gets this answer to its first read has tried nothing ([`Snapshotter::keep_commits`]).
const PACKS_READ_OUTSTANDING: &str =
    "snapshot: an earlier read of the engine's packs has not returned; they were not read";
/// The longest the git of one keep may run before it is given up; whatever starts the next look ends it sooner. It is the
/// least the git work of a look has ([`COPY_GIT_FLOOR`]).
pub const KEEP_LIMIT: Duration = COPY_GIT_FLOOR;
/// The longest git's own check of a pack that an earlier worker kept may run ([`Snapshotter::check_kept`]) before it is
/// given up and the pack is removed. The check runs where the first reading of the tree runs, before the engine starts, and has
/// that reading's limit: [`PRIME_LIMIT`] in a session, and the limit a caller of [`Snapshotter::new_primed`] names.
pub const KEPT_CHECK_LIMIT: Duration = PRIME_LIMIT;
/// The mark, a file in the folder [`KEPT_DIR`], that switches the keep off for the rest of the run: git's check of a kept
/// pack did not end within its limit when a worker started ([`Snapshotter::check_kept`]). A pack that cannot be checked within the
/// limit is never kept twice, whatever its size.
const KEPT_OFF: &str = "off";

/// What came of a keep of the run's commits ([`Snapshotter::keep_commits`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    /// No keep was due.
    NotDue,
    /// The pack is kept: its size in bytes.
    Kept { bytes: u64 },
    /// The keep's git ran and the run's commits hold nothing to keep: the engine has packed every object of them.
    Nothing,
    /// The keep was missed: why. No pack and no marker are left, and no copy is affected.
    Missed(String),
}

/// What a look found of the engine's head, for the worker's log line of the look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EngineHead {
    /// The head could not be read: the engine removed or replaced `.git`.
    #[default]
    Gone,
    /// Another head than the look before found, or the first head a look of this worker found.
    Moved,
    /// The head the look before found.
    Same,
}

impl EngineHead {
    /// The word of the log line.
    pub fn word(self) -> &'static str {
        match self {
            EngineHead::Gone => "gone",
            EngineHead::Moved => "moved",
            EngineHead::Same => "same",
        }
    }
}

/// How long the steps of the last look took and what it found of the engine's head ([`Snapshotter::take_look_steps`]), for
/// the worker's log line of the look.
#[derive(Clone, Copy, Debug, Default)]
pub struct LookSteps {
    /// The clean-up of the private repository, when one was due.
    pub tidy: Duration,
    /// Reading the tree (`add -A`, `write-tree`).
    pub tree: Duration,
    /// Reading the engine's head.
    pub head: Duration,
    /// The copy's commit and its ref.
    pub commit: Duration,
    /// The bundle, with the read of the engine's packs while a pack is kept.
    pub bundle: Duration,
    /// What the look found of the engine's head.
    pub engine: EngineHead,
}

/// The pack this repository keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeptPack {
    /// The engine's head the pack was kept for.
    head: String,
    /// `pack-<hash>`, the name its files share.
    name: String,
    /// The size of the pack file.
    bytes: u64,
    /// What the engine's own packs were when the pack was kept ([`ScratchRepo::engine_packs`]).
    engine_packs: String,
}

/// What a [`Snapshotter`] knows for the keep of the run's commits.
#[derive(Debug, Default)]
struct KeepState {
    /// The keep is switched off (`AGENTD_DIRECT_KEEP_COMMITS=0`).
    off: bool,
    /// The engine's head as the last look that read it found it.
    seen_head: Option<String>,
    /// The engine's head when a keep is due after the look that just made a copy: that look found the head the look before it
    /// had found, or it is a look of the timer or of a tool call that left the timer and during which neither the
    /// engine's head nor its packs changed ([`Snapshotter::keep_at_once`]). Neither shows that the engine is at rest: the keep
    /// reads the commit it was given, and a read that fails is a miss.
    rest_head: Option<String>,
    /// The keep that is due is a keep at once ([`Snapshotter::keep_at_once`]), not the keep after a second look of the
    /// same head. It marks the head as tried only when it ends as kept.
    due_at_once: bool,
    /// A keep at once was started and no keep has ended as kept since. While the mark
    /// stands, no keep at once follows any look ([`Snapshotter::keep_at_once_may_follow`]): a keep at once that was missed,
    /// was cut or was refused for a short disk is tried once and not again until a keep is kept. The keep after a second
    /// look of the same head does not ask the mark, and when it ends as kept it clears it. So between two kept packs at most
    /// one keep at once is missed, cut or refused. Ruling 45 (c) of the lead: a keep that found nothing to pack leaves the
    /// mark as it was before that keep; it read no loose object of the run, and costs two git calls.
    at_once_spent: bool,
    /// The look that starts next is one that a keep at once may follow: the timer or a tool call started it, and
    /// agentd was not asked to end before ([`Snapshotter::look_is_spaced`]); taken when the look starts.
    spaced_look: bool,
    /// The look that just stored its copy left the timer: the spacing holds the look after it past its tick
    /// ([`Snapshotter::look_left_the_timer`]).
    left_timer: bool,
    /// What the engine's packs were when the look in progress started, read for a look of the timer or of a tool call
    /// while a keep is possible at all; unset for every other look and when the folder could not be read.
    look_packs: Option<String>,
    /// The engine's head as the look in progress read it, while [`Self::look_packs`] is set.
    look_head: Option<String>,
    /// The look that just made a copy read the kept pack at its bundle step. No keep at
    /// once follows such a look ([`Snapshotter::keep_at_once_may_follow`]): once a pack is kept, the looks keep the rule of
    /// the second look.
    look_read_kept: bool,
    /// The pack that is kept.
    kept: Option<KeptPack>,
    /// The head a keep was last tried for: this worker does not try it again until the head has moved. The mark is in
    /// memory: a worker that starts takes it from the pack that stands, or from the pack a short disk took at that start
    /// ([`Snapshotter::new_primed_with_free`]), and has none on a private repository without a pack.
    tried: Option<String>,
    /// The head of a keep that is in flight; still set when the keep was cut ([`Snapshotter::keep_cut`]).
    keeping: Option<String>,
    /// The size of the bundle of the last copy.
    last_bundle_bytes: u64,
    /// The look in progress may read the kept pack ([`Snapshotter::keep_check`]).
    use_kept: bool,
    /// Why the pack an earlier worker kept was not adopted ([`Snapshotter::check_kept`]), until the caller has taken it
    /// ([`Snapshotter::take_kept_note`]).
    note: Option<String>,
    /// The free bytes the keep's rules read in place of those of the disk of the private repository, one answer for each
    /// question in order, the last one for every question after it; without one the disk is read
    /// ([`Snapshotter::set_keep_free_bytes`], [`Snapshotter::set_keep_free_answers`]); tests.
    free: Vec<u64>,
    /// The size of the pack an earlier worker kept and how long git's check of it ran when this repository was opened
    /// ([`Snapshotter::check_kept`]), until the caller has taken them ([`Snapshotter::take_kept_check`]).
    checked: Option<(u64, Duration)>,
    /// This run keeps no pack any more: the private repository carries the mark [`KEPT_OFF`].
    run_off: bool,
}

impl KeepState {
    /// The look that just made a copy found the engine's head at `head`, and its bundle has `bundle_bytes` bytes.
    fn note_head(&mut self, head: Option<&str>, bundle_bytes: u64) -> EngineHead {
        self.last_bundle_bytes = bundle_bytes;
        let found = match head {
            Some(head) if self.seen_head.as_deref() == Some(head) => {
                self.rest_head = Some(head.to_string());
                EngineHead::Same
            }
            Some(_) => EngineHead::Moved,
            None => EngineHead::Gone,
        };
        self.seen_head = head.map(str::to_string);
        found
    }
}

/// Removes the files of the pack `name` in `dir`: the index first, so that git stops reading the pack, the marker last.
fn remove_pack(dir: &Path, name: &str) {
    for ext in ["idx", "pack", "rev", "keep"] {
        let _ = std::fs::remove_file(dir.join(format!("{name}.{ext}")));
    }
}

/// Removes every file in the folder `dir`.
fn clear_folder(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The folder a keep writes its pack into before the pack stands. It is emptied when a keep starts and when the keep ends in
/// any way, also when the keep's future is dropped: a keep that failed, met a full disk or was cut leaves no file behind.
struct Incoming(PathBuf);

impl Drop for Incoming {
    fn drop(&mut self) {
        clear_folder(&self.0);
    }
}

/// What the engine's store at `dir` (its `objects/pack`) holds as packs: their number and a hash over their names. A store
/// without that folder holds none.
fn engine_packs_of(dir: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".pack"))
            .take(ENGINE_PACKS_MAX + 1)
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(io(e)),
    };
    if names.len() > ENGINE_PACKS_MAX {
        return Err(format!(
            "snapshot: the engine's store holds more than {ENGINE_PACKS_MAX} packs"
        ));
    }
    names.sort();
    let mut hash = Sha256::new();
    for name in &names {
        hash.update(name.as_bytes());
        hash.update(b"\n");
    }
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{}:{digest}", names.len()))
}

/// What reads the packs of an engine's store, given the store's `objects/pack` ([`engine_packs_of`]; a test gives a storage
/// that answers slowly, [`Snapshotter::set_engine_packs_reader`]).
pub type PacksReader = Arc<dyn Fn(&Path) -> Result<String, String> + Send + Sync>;

/// A [`PacksReader`] as a member of [`ScratchRepo`].
struct PacksQuestion(PacksReader);

impl std::fmt::Debug for PacksQuestion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PacksQuestion")
    }
}

/// Whether `<repo>/.git` is the repository's own plain directory ([`git_dir_problem`] finds nothing): file calls on the work
/// tree's file system.
fn git_dir_is_own(repo: &Path) -> bool {
    git_dir_problem(repo).is_none()
}

/// What answers whether `.git` of an engine's repository is the repository's own directory, given the repository
/// ([`git_dir_is_own`]; a test gives a storage that answers slowly, [`Snapshotter::set_git_dir_reader`]).
pub type GitDirReader = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// A [`GitDirReader`] as a member of [`ScratchRepo`].
struct GitDirQuestion(GitDirReader);

impl std::fmt::Debug for GitDirQuestion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitDirQuestion")
    }
}

/// What a worker finds when it opens the private repository at `git_dir` that an earlier worker left (ruling 12 of the
/// lead's rulings on the build brief). Everything in the folder a keep writes into is removed: a keep that was cut before its marker
/// stood left it. Of the packs whose marker carries the worker's own line, the one with the newest marker that has its pack and its
/// index is the kept pack; one that lacks a file (a keep that was cut after its marker stood) and every older one (a keep that was
/// cut while the pack before was replaced) are removed. A pack with another marker, or with none, is not the worker's: it stays.
fn kept_at_open(git_dir: &Path) -> Option<KeptPack> {
    let kept = git_dir.join(KEPT_DIR);
    clear_folder(&kept.join("incoming"));
    let packs = kept.join("pack");
    let mut own: Vec<(std::time::SystemTime, KeptPack)> = Vec::new();
    for entry in std::fs::read_dir(&packs).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".keep") else {
            continue;
        };
        let Some(text) =
            read_regular_nofollow(&entry.path()).and_then(|b| String::from_utf8(b).ok())
        else {
            continue;
        };
        let mut lines = text.lines();
        let Some(head) = lines.next().and_then(|l| l.strip_prefix(KEPT_LINE)) else {
            continue;
        };
        let head = head.trim();
        let engine_packs = lines
            .next()
            .and_then(|l| l.strip_prefix(KEPT_PACKS_LINE))
            .unwrap_or_default()
            .trim()
            .to_string();
        let pack = packs.join(format!("{name}.pack"));
        let bytes = std::fs::metadata(&pack).map(|m| m.len()).ok();
        match bytes {
            Some(bytes)
                if object_id(head).is_ok() && packs.join(format!("{name}.idx")).is_file() =>
            {
                let written = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                own.push((
                    written,
                    KeptPack {
                        head: head.to_string(),
                        name: name.to_string(),
                        bytes,
                        engine_packs,
                    },
                ));
            }
            _ => remove_pack(&packs, name),
        }
    }
    own.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let newest = own.pop().map(|(_, kept)| kept);
    for (_, older) in own {
        remove_pack(&packs, &older.name);
    }
    newest
}

impl ScratchRepo {
    /// What the engine's store holds as packs now ([`engine_packs_of`]): one read of a folder on the work tree's file
    /// system, made off the runtime's threads and given up with the keep or the look in progress, like the file calls of a look
    /// ([`Self::on_tree`]). A keep makes it before and after it packs, and a look makes it once while a pack is kept
    /// ([`Snapshotter::keep_check`]). A look of the timer or of a tool call makes it once when it starts, and the keep at once makes it once
    /// more, with its question about the engine's `.git` in the same call ([`Self::engine_packs_if_own`]). It has a flag of its
    /// own. A keep is dropped when the next look starts, and its read may not
    /// have returned then: under the flag of the look's file calls that look would be refused its own first call and fail, and
    /// the copy of a stop would be lost to a keep. So one read of the packs is outstanding at most, a second one is refused at
    /// once, and a look whose read is refused reads no kept pack. No file call of a look waits for it or is refused because of it.
    async fn engine_packs(&self) -> Result<String, String> {
        let dir = self.repo.join(".git/objects/pack");
        let read = self.packs_reader.0.clone();
        self.off_the_looks_flag(move || read(&dir)).await
    }

    /// [`Self::engine_packs`] for the keep at once, which also asks whether the engine's `.git` is still the repository's
    /// own directory: both are answered by one call off the runtime's threads, under the flag of the packs' read. `None` when
    /// `.git` is not the repository's own directory: no git is then run in the engine's repository, and the packs are not read.
    ///
    /// The keep at once makes no call under the flag of a look's file calls ([`Self::on_tree`]). Its future is dropped when the
    /// next look starts, and a call of it that has not returned then would refuse that look its first file call: the look that
    /// a request to end asks for would fail, and the work written before the request would be in no stored copy.
    async fn engine_packs_if_own(&self) -> Result<Option<String>, String> {
        let (repo, dir) = (self.repo.clone(), self.repo.join(".git/objects/pack"));
        let (is_own, read) = (self.git_dir_reader.0.clone(), self.packs_reader.0.clone());
        self.off_the_looks_flag(move || {
            if is_own(&repo) {
                read(&dir).map(Some)
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// Runs `call`, file calls of a keep or of a look's question about the engine's packs, on a thread of its own
    /// under the flag of the packs' read, never under the flag of a look's file calls ([`Self::on_tree`]). It is given up with
    /// the keep or the look in progress. One such call is outstanding at most: while it has not returned, the next one is
    /// refused at once.
    async fn off_the_looks_flag<T: Send + 'static>(
        &self,
        call: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        if self.packs_busy.swap(true, Ordering::SeqCst) {
            return Err(PACKS_READ_OUTSTANDING.to_string());
        }
        let busy = self.packs_busy.clone();
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
            Some(Ok(out)) => out,
            Some(Err(e)) => {
                // The read ended in a panic: it is not outstanding.
                self.packs_busy.store(false, Ordering::SeqCst);
                Err(format!("snapshot: the engine's packs: {e}"))
            }
            None => Err(
                "snapshot: the work tree did not answer; the engine's packs were not read"
                    .to_string(),
            ),
        }
    }
}

impl Snapshotter {
    /// Whether this repository keeps the run's commits (`AGENTD_DIRECT_KEEP_COMMITS`, tests and the bench only). Switched
    /// off, the pack the worker kept is removed, no look reads a kept pack and no keep is made: the looks are those of release 5.25.
    pub fn set_keep_commits(&mut self, on: bool) {
        self.keep.off = !on;
        if !on {
            self.drop_kept();
        }
    }

    /// Another reader of the engine's packs than [`engine_packs_of`]; tests.
    pub fn set_engine_packs_reader(&mut self, reader: PacksReader) {
        self.scratch.packs_reader = PacksQuestion(reader);
    }

    /// Another answer to whether the engine's `.git` is the repository's own directory than [`git_dir_is_own`]; tests.
    pub fn set_git_dir_reader(&mut self, reader: GitDirReader) {
        self.scratch.git_dir_reader = GitDirQuestion(reader);
    }

    /// The bytes of the kept pack; 0 without one.
    pub fn kept_bytes(&self) -> u64 {
        self.keep.kept.as_ref().map_or(0, |kept| kept.bytes)
    }

    /// Git's own check of the pack an earlier worker kept ([`kept_at_open`]), before this worker adopts it: `git index-pack
    /// --verify`, the cheapest command of git that reads every object of a pack. It inflates each object, hashes it against the
    /// pack's index, and hashes the whole file against its trailer. git copies the bytes of a packed object into a bundle without
    /// reading them, so a look beside a pack with wrong bytes stored a copy that no reader could fetch. A pack that fails the
    /// check is removed: the looks are then those of release 5.25, the head is kept again at its next rest, and
    /// [`Self::take_kept_note`] says why, once.
    ///
    /// A check that has not ended after `limit` is given up, with its git ended. The pack is removed, and the keep is switched
    /// off for the rest of the run by a mark in the private repository ([`KEPT_OFF`]): a later worker would keep the same
    /// commits again and pay the same check at its start, so a pack that cannot be checked within the limit is not kept twice.
    /// [`Self::take_kept_note`] says so, once. How long the check ran is kept for the caller's log ([`Self::take_kept_check`]).
    ///
    /// The check is given an object folder that holds nothing (the folder a keep writes into, which the open has just cleared).
    /// With the objects of this repository at hand git would compare every object of the pack with the copy the repository
    /// holds, and the engine's store on the session's storage holds each of them. So the check reads the pack and its index on
    /// the worker's own disk and nothing else. It runs once, when the repository is opened, before the engine starts.
    async fn check_kept(&mut self, limit: Duration) {
        let Some(kept) = &self.keep.kept else {
            return;
        };
        let bytes = kept.bytes;
        let folder = self.git_dir.join(KEPT_DIR);
        let pack = folder.join("pack").join(format!("{}.pack", kept.name));
        let nothing = folder.join("incoming");
        let gd: &Path = &self.git_dir;
        let began = Instant::now();
        let give_up = GiveUp::at(Some(tokio::time::Instant::now() + limit));
        let checked = git_with_env_until(
            self.git_dir.parent().unwrap_or(Path::new("/")),
            &["index-pack", "--verify", pack.to_str().unwrap_or_default()],
            &[("GIT_DIR", gd), ("GIT_OBJECT_DIRECTORY", &nothing)],
            &give_up,
        )
        .await;
        let took = began.elapsed();
        self.keep.checked = Some((bytes, took));
        if let Err(e) = checked {
            self.keep.note = Some(if took >= limit {
                self.keep.run_off = true;
                let marked = std::fs::write(
                    folder.join(KEPT_OFF),
                    "agentd keeps no pack in this run: git's check of a kept pack did not end within its limit\n",
                );
                format!(
                    "git's check of the pack an earlier worker kept did not end in {} s; the pack was removed and no pack is \
                     kept for the rest of this run{}",
                    limit.as_secs(),
                    marked.err().map_or(String::new(), |e| format!(
                        " (the mark that says so could not be written: {})",
                        io(e)
                    ))
                )
            } else {
                format!(
                    "the pack an earlier worker kept did not pass git's check and was removed: {}",
                    ws(e)
                )
            });
            self.drop_kept();
        }
    }

    /// Why the pack an earlier worker kept was not adopted when this repository was opened ([`Self::check_kept`]), once;
    /// `None` when it was adopted or there was none. The caller logs it.
    pub fn take_kept_note(&mut self) -> Option<String> {
        self.keep.note.take()
    }

    /// The size of the pack an earlier worker kept and how long git's check of it ran when this repository was opened
    /// ([`Self::check_kept`]), once; `None` when the repository held no kept pack, so that nothing was checked. The caller logs
    /// it in the line of the first reading of the tree: it is what the start of a turn pays for a kept pack.
    pub fn take_kept_check(&mut self) -> Option<(u64, Duration)> {
        self.keep.checked.take()
    }

    /// The free bytes the keep's rules read in place of those of the disk of the private repository; `None` reads the
    /// disk. Tests: a disk whose free space lies between the two rules of the keep cannot be had from a real one.
    pub fn set_keep_free_bytes(&mut self, free: Option<u64>) {
        self.keep.free = free.into_iter().collect();
    }

    /// [`Self::set_keep_free_bytes`] with one answer for each question of the keep's rules from now on, in order; the
    /// last answer stands for every question after it. Tests: the free space of a disk that changes between the start of a
    /// look and its bundle.
    pub fn set_keep_free_answers(&mut self, answers: Vec<u64>) {
        self.keep.free = answers;
    }

    /// The keep's free-space rule, in one place: what the rule asks a disk to have free, when `free` is less. The rule
    /// asks for the mark below which the clean-up runs whatever the growth ([`TidyMarks::free_bytes`]), twice the last bundle,
    /// and `more`; a disk that has exactly that much free is not short. Every question of the keep comes here: that of a look,
    /// of the copy of the end and of a keep before it starts ([`Self::keep_short_disk`]), and that of a worker that starts,
    /// where no snapshotter exists yet ([`Self::new_primed_with_free`]: the marks of a run and no bundle yet). No file call
    /// and no git call is made.
    fn keep_rule_short(
        marks: &TidyMarks,
        last_bundle_bytes: u64,
        more: u64,
        free: u64,
    ) -> Option<u64> {
        let need = marks
            .free_bytes
            .saturating_add(last_bundle_bytes.saturating_mul(2))
            .saturating_add(more);
        (free < need).then_some(need)
    }

    /// The keep's free-space rule asked of the disk of the private repository: its free bytes and what the rule asks
    /// for, when the disk has less ([`Self::keep_rule_short`], with the marks of this repository and its last bundle). A pack
    /// that is kept does not stay on a disk that is short of the rule ([`Self::keep_gives_way`], `more` is 0). No keep is
    /// started on a disk that is short of the rule and of the pack the keep is about to write, which is about as large as the
    /// last bundle ([`Self::keep_commits`], `more` is the size of the last bundle): the pack that is then written leaves the
    /// disk above the rule, so no look removes it for the room it took.
    fn keep_short_disk(&mut self, more: u64) -> Option<(u64, u64)> {
        let free = match self.keep.free.len() {
            0 => free_bytes(&self.git_dir),
            1 => self.keep.free[0],
            _ => self.keep.free.remove(0),
        };
        Self::keep_rule_short(&self.scratch.marks, self.keep.last_bundle_bytes, more, free)
            .map(|need| (free, need))
    }

    /// How long the steps of the last look took and what it found of the engine's head.
    pub fn take_look_steps(&mut self) -> LookSteps {
        std::mem::take(&mut self.steps)
    }

    /// Whether a keep would start after the look that just made a copy ([`Self::keep_commits`]): that look found the head the
    /// look before it had found, or a keep at once is due after it ([`Self::keep_at_once`]); that head is no commit the
    /// run started from (an engine that made no commit has nothing above the base), the copies are above a floor, the keep is on
    /// (the switch, and no check of a kept pack was given up in this run), and no keep was tried for that head. No git call and
    /// no file call is made for the answer.
    pub fn keep_is_due(&self) -> bool {
        self.keep
            .rest_head
            .as_deref()
            .is_some_and(|head| self.keep_wanted(head))
    }

    /// Whether this repository keeps a pack at all: the keep is on, and the copies are above a floor.
    fn keep_possible(&self) -> bool {
        !self.keep.off
            && !self.keep.run_off
            && self.form == BundleForm::AboveFloor
            && !self.floors.is_empty()
    }

    fn keep_wanted(&self, head: &str) -> bool {
        self.keep_possible()
            && !self.floors.iter().any(|floor| floor == head)
            && self.keep.tried.as_deref() != Some(head)
    }

    /// The look that starts next is one that the spacing places (`yes`): the timer or a tool call started it, and
    /// agentd was not asked to end before. These are the looks a keep at once may follow (ruling 43 (d) of the lead: also the
    /// look after a tool call; when a burst of commits ends while the copy task waits for its tick, the first look after it
    /// is the tool call's). The copy task says so before each of its looks; a look that nobody announced is none, and
    /// neither is the look that a request to end asks for or a look that tries the copy of a stop again.
    pub fn look_is_spaced(&mut self, yes: bool) {
        self.keep.spaced_look = yes;
    }

    /// When the look that starts is a look of the timer or of a tool call ([`Self::look_is_spaced`]) and this
    /// repository keeps a pack at all, what the engine's packs are now ([`ScratchRepo::engine_packs`], one read of a folder
    /// on the session's storage): the keep at once asks whether they changed while the look ran ([`Self::keep_at_once`]).
    /// The answer is the time the read took, which is part of the look's git work; every other look makes no read.
    async fn note_look_packs(&mut self) -> Duration {
        self.keep.look_head = None;
        self.keep.look_packs = None;
        self.keep.left_timer = false;
        self.keep.look_read_kept = false;
        if !std::mem::take(&mut self.keep.spaced_look) || !self.keep_possible() {
            return Duration::ZERO;
        }
        let at = Instant::now();
        self.keep.look_packs = self.scratch.engine_packs().await.ok();
        at.elapsed()
    }

    /// The copy task says whether the look that just stored its copy left the timer: the timer or a tool call started
    /// it, agentd was not asked to end, and the spacing holds the look after it past its tick ([`Cadence::holds_past_tick`]).
    pub fn look_left_the_timer(&mut self, left: bool) {
        self.keep.left_timer = left;
    }

    /// Whether a keep at once may follow the look that just stored its copy ([`Self::keep_at_once`]): the look left the
    /// timer, it read the engine's head and its packs when it started, it read no kept pack (ruling 37 (c) of the lead), no
    /// keep at once was missed, cut or refused since the last kept pack (rulings 43 (b) and 45 (c), the mark
    /// `at_once_spent`; one that found nothing to pack does not count), no keep is due by the
    /// rule of the second look, and a keep is wanted for that head. No git call and no file call is made for the answer.
    ///
    /// A look that read the kept pack is followed by no keep at once, whatever made it slow: the rule adds one earlier keep
    /// for a burst of commits that no pack holds, and never a keep after every look of the timer. While commits keep coming
    /// under a kept pack, each such keep would pack every unpacked object of the run again
    /// (`b932_steady_commits_get_one_keep_for_their_first_burst`); a second burst under a kept pack keeps the rule of the
    /// second look (`b932_a_second_burst_after_a_kept_pack_keeps_the_rule_of_the_second_look`).
    ///
    /// The mark bounds what a keep at once that fails can cost. While commits keep coming and no pack gets kept, each look
    /// that left the timer on a new head would start one more `pack-objects` over every unpacked object of the run. One is
    /// tried, and none again until a keep has ended as kept
    /// (`b932_steady_commits_whose_keeps_at_once_fail_get_one_keep_at_once`).
    pub fn keep_at_once_may_follow(&self) -> bool {
        self.keep.left_timer
            && !self.keep.look_read_kept
            && !self.keep.at_once_spent
            && self.keep.rest_head.is_none()
            && self.keep.look_packs.is_some()
            && self
                .keep
                .look_head
                .as_deref()
                .is_some_and(|head| self.keep_wanted(head))
    }

    /// Rule B of the lead's ruling 32: the keep at once. A look of the timer or of a tool call (ruling 43 (d)) that
    /// left the timer ([`Self::look_left_the_timer`]) is followed by looks that the spacing holds back, and until a pack is kept every one of
    /// them reads each loose object of the run's commits again. The copy task calls this in the wait that follows such a look,
    /// right before [`Self::keep_commits`]: after the copy of the look is stored and the look is counted, never between a copy
    /// and its store, and it ends with the keep's future. A keep is then due at once when the engine's `.git` is still the
    /// repository's own directory and its packs, read once more now, are those the look found when it started (git's own `gc`
    /// in flight changes them), and the engine's head, read once more now (one `git rev-parse`), is the head the look read at
    /// its start. That shows that no commit landed and nothing was packed while the look ran; it does not show that the engine
    /// is at rest, and the keep does not need it ([`Self::keep_commits`]).
    ///
    /// Ruling 37 (a) of the lead: none of these questions is a call under the flag of a look's file calls
    /// ([`ScratchRepo::on_tree`]). What `.git` is and what the packs are is one call under the flag of the packs' read
    /// ([`ScratchRepo::engine_packs_if_own`]), and the head is a git call that ends with this future. So a question that has not
    /// returned when the next look starts, also the look that a request to end asks for, refuses that look no file call
    /// (`b932_a_question_of_the_keep_at_once_that_has_not_returned_fails_no_look`).
    ///
    /// The rule only adds an earlier keep. Nothing is read when the keep is already due by the rule of the second look, when
    /// the look was not announced as the timer's or a tool call's ([`Self::look_is_spaced`]), when it stayed on the timer, or when no keep
    /// is wanted for the head ([`Self::keep_at_once_may_follow`]). A keep at once that does not end as kept (missed, cut,
    /// refused for a short disk, nothing to pack) leaves the head unmarked, so the keep after a second look of the same
    /// head follows as it always did, once. One that was missed, was cut or was refused for a short disk leaves the mark
    /// of ruling 43 (b) behind ([`Self::keep_commits`]): no keep at once follows any look until a keep has ended as kept.
    /// One that found nothing to pack leaves the mark as it was (ruling 45 (c)).
    pub async fn keep_at_once(&mut self, give_up: &GiveUp) {
        let may = self.keep_at_once_may_follow();
        self.keep.left_timer = false;
        let (Some(head), Some(packs)) = (self.keep.look_head.take(), self.keep.look_packs.take())
        else {
            return;
        };
        if !may {
            return;
        }
        self.scratch.give_up = Some(give_up.clone());
        // Ruling 37 (a) of the lead: no question of the keep at once is asked under the flag of a look's file calls. What the
        // engine's `.git` is and what its packs are is one call under the flag of the packs' read; the head is one git call,
        // which ends with this future.
        let still = matches!(
            self.scratch.engine_packs_if_own().await,
            Ok(Some(now)) if now == packs
        ) && self.engine_head_id().await.as_deref() == Some(head.as_str());
        self.scratch.give_up = None;
        if still {
            self.keep.rest_head = Some(head);
            self.keep.due_at_once = true;
        }
    }

    /// A keep that was in flight was cut before the look that starts now. When the timer started that look (`by_timer`),
    /// the keep is not tried again until the head has moved: a keep that does not fit between two looks of the timer is not paid
    /// for at every look. When a tool call or a request to end cut it, it is tried again at the next look that qualifies.
    /// A keep at once that was cut never marked the head ([`Self::keep_commits`]), so nothing is cleared for it and the
    /// keep after a second look of the same head follows.
    pub fn keep_cut(&mut self, by_timer: bool) {
        if let Some(head) = self.keep.keeping.take()
            && !by_timer
            && self.keep.tried.as_deref() == Some(head.as_str())
        {
            self.keep.tried = None;
        }
    }

    /// Removes the kept pack. The repository is then what it is without a keep.
    fn drop_kept(&mut self) {
        self.keep.use_kept = false;
        if let Some(kept) = self.keep.kept.take() {
            remove_pack(&self.git_dir.join(KEPT_DIR).join("pack"), &kept.name);
        }
    }

    /// Root invariant 9 wins over the keep, in one place. It is the first thing a look does, before anything is written
    /// into the private repository ([`Self::look`]), the first thing the copy of the end does ([`Self::final_copy`]), and what the
    /// check before the bundle starts with ([`Self::keep_check`]). A worker that starts asks the same rule, the one function
    /// [`Self::keep_rule_short`], before its first write, where no snapshotter exists yet ([`Self::new_primed_with_free`]).
    ///
    /// A kept pack is removed when the disk of the private repository has less free than the keep's rule asks
    /// ([`Self::keep_short_disk`], one question to the worker's own disk while a pack is kept). The pack is about as large as a
    /// bundle, and on a disk that has run short since the keep its bytes are free before the tree is read: the copy that
    /// release 5.26 stores there is stored. The head stays marked as tried, so this worker does not keep it again until it has
    /// moved, and no pack is written and removed look after look.
    ///
    /// The folder a keep writes into is emptied too. The git of a keep that was cut is told to end and is not waited for, and a
    /// file it made after the sweep of [`Incoming`] would stay on a disk where no keep starts. No keep is in flight here: a keep
    /// holds the repository, and what starts a look or the copy of the end drops it first. No git call is made.
    fn keep_gives_way(&mut self) {
        clear_folder(&self.git_dir.join(KEPT_DIR).join("incoming"));
        if self.keep.kept.is_some() && self.keep_short_disk(0).is_some() {
            self.drop_kept();
        }
    }

    /// Whether the look in progress, which found the engine's head at `head`, reads the kept pack: while a pack is kept,
    /// the engine's `.git` is the repository's own, the head is above the floors, and the engine's store holds the packs it held
    /// when the pack was kept ([`ScratchRepo::engine_packs`], one read of a folder on the session's storage). A kept pack that the
    /// engine's packs overtook (a repack, a fetch, a pack a tool left) is removed here, before the bundle is written, and its head
    /// stays marked as tried: this worker packs the run's commits again only when the head has moved, so an engine whose packs
    /// change under a head that rests costs a worker one keep and one removal at the most, never a pack written and removed look
    /// after look. The mark holds for this worker: a worker that starts later on a private repository without a pack keeps the
    /// head once more. So is the pack of an engine whose `.git` is gone, and the pack of a head that is back at a commit the run
    /// started from: nothing of the run is above the floors then, and that mark is cleared. When the folder cannot be read, or
    /// an earlier read of it has not returned, this look reads no kept pack and is the look of release 5.25. No git call is
    /// made for it.
    ///
    /// Root invariant 9 wins over the keep: before anything else the short-disk question is asked once more
    /// ([`Self::keep_gives_way`]). The look asked it when it started, and has written the files that changed into the private
    /// repository since.
    async fn keep_check(&mut self, head: Option<&str>) {
        self.keep.use_kept = false;
        self.keep_gives_way();
        let Some(kept) = &self.keep.kept else {
            return;
        };
        let fresh = match head {
            Some(head) if !self.floors.iter().any(|floor| floor == head) => self
                .scratch
                .engine_packs()
                .await
                .ok()
                .map(|now| now == kept.engine_packs),
            _ => {
                self.drop_kept();
                self.keep.tried = None;
                return;
            }
        };
        match fresh {
            Some(true) => self.keep.use_kept = true,
            Some(false) => self.drop_kept(),
            None => {}
        }
    }

    /// The keep of the run's commits (decisions 1 to 4 of the build brief, rulings 8 to 13 of the lead). The caller calls it
    /// after a look that the timer started has stored its
    /// copy, never before the store and never inside a look, and drops it when
    /// the next look starts, which a request to end brings at
    /// once: the git of a dropped keep is ended with its
    /// group and what it wrote is removed ([`Incoming`]).
    ///
    /// A keep is due when that look found the head the look before it had found, or at once after a look that left the
    /// timer while the engine's head and packs stood still ([`Self::keep_is_due`], [`Self::keep_at_once`]). It reads the commit
    /// ids it was given, the head that look read and the floors, never a ref that can move. One `git pack-objects --revs --unpacked
    /// --incremental --non-empty` writes the objects of the run's commits that are in no pack into `kept/incoming/`; the marker
    /// (`agentd kept <head>`, and what the engine's packs were) is written beside it; then the marker, the pack, its reverse index
    /// and last its index are renamed into `kept/pack/`, where git sees a pack by its index; then the pack kept before is removed.
    /// The marker is the only state. What the keep holds on the worker's disk is one pack, two while it is replaced, and it is not
    /// started while the disk has less free than the mark of the clean-up and three times the last bundle: the keep's rule, and
    /// the pack that is about to be written ([`Self::keep_short_disk`]).
    ///
    /// A keep is tried once for a head: a short disk, a git that failed (an object the engine pruned under it, a
    /// disk that filled in the middle of the write), an engine that repacked while the pack was written, and nothing to pack are
    /// all the one try, and none of them changes a copy. [`Keep::Missed`] says why, for the caller's log. A keep at once
    /// ([`Self::keep_at_once`]) is not that one try unless it ends as kept: after one that was missed, was cut or found nothing
    /// to pack, the keep after a second look of the same head is still tried, once.
    ///
    /// Ruling 43 (b) of the lead: a keep at once that starts here sets the mark `at_once_spent`, before its first call, so
    /// that one whose future is dropped in the middle leaves it too. A keep that ends as kept, at once or after a second
    /// look, clears it. While it stands no keep at once follows any look ([`Self::keep_at_once_may_follow`]); the keep after
    /// a second look of the same head never asks it.
    ///
    /// Ruling 45 (c) of the lead: a keep that found nothing to pack leaves the mark as it was before that keep. The mark
    /// bounds the keeps at once that can read every loose object of the run before they fail; one that finds nothing has
    /// read none and costs two git calls. So a keep at once in a store that the engine has packed does not take the keep
    /// at once from a later burst that nothing packs
    /// (`b932_a_burst_after_a_keep_at_once_that_found_nothing_to_pack_gets_its_keep_at_once`), and nothing to pack never
    /// clears a mark that stands (`b932_a_keep_that_found_nothing_to_pack_leaves_a_mark_that_stands`).
    ///
    /// Ruling 43 (e) of the lead: a keep whose first read of the engine's packs is refused because an earlier read has not
    /// returned ([`ScratchRepo::off_the_looks_flag`]) is no try. No git ran and the storage was not asked again, so both
    /// marks are set back to what they were: the head stays unmarked, and the keep after the next look of the timer that
    /// finds that head is tried. Before, a read that one slow call left outstanding spent the head's one try, and a head
    /// that rested got no pack in that worker
    /// (`b932_a_keep_whose_first_read_of_the_packs_is_refused_is_no_try`).
    pub async fn keep_commits(&mut self, give_up: &GiveUp) -> Keep {
        let at_once = std::mem::take(&mut self.keep.due_at_once);
        let Some(head) = self.keep.rest_head.take() else {
            return Keep::NotDue;
        };
        if !self.keep_wanted(&head) {
            return Keep::NotDue;
        }
        // Ruling 43 (e): what the marks were, for a keep that turns out to have tried nothing.
        let marks = (self.keep.tried.clone(), self.keep.at_once_spent);
        if at_once {
            self.keep.at_once_spent = true;
        } else {
            self.keep.tried = Some(head.clone());
        }
        if let Some((free, need)) = self.keep_short_disk(self.keep.last_bundle_bytes) {
            return Keep::Missed(format!(
                "the disk of the private repository has {free} bytes free; a pack is kept only above {need}"
            ));
        }
        self.keep.keeping = Some(head.clone());
        self.scratch.give_up = Some(give_up.clone());
        let made = self.keep_pack(&head, give_up).await;
        self.scratch.give_up = None;
        self.keep.keeping = None;
        match made {
            Ok(Some(kept)) => {
                let bytes = kept.bytes;
                self.keep.kept = Some(kept);
                self.keep.tried = Some(head);
                self.keep.at_once_spent = false;
                Keep::Kept { bytes }
            }
            Ok(None) => {
                self.drop_kept();
                // Ruling 45 (c) of the lead: nothing to pack leaves the mark as it was.
                self.keep.at_once_spent = marks.1;
                Keep::Nothing
            }
            Err(e) => {
                if e == PACKS_READ_OUTSTANDING {
                    // The keep's first read of the engine's packs was refused: a read after it cannot be, because one read
                    // is outstanding at most and the first one had returned. Nothing was tried.
                    (self.keep.tried, self.keep.at_once_spent) = marks;
                }
                Keep::Missed(e)
            }
        }
    }

    async fn keep_pack(
        &mut self,
        head: &str,
        give_up: &GiveUp,
    ) -> Result<Option<KeptPack>, String> {
        let gd: &Path = &self.git_dir;
        let cwd = self.git_dir.parent().unwrap_or(Path::new("/"));
        let incoming = self.git_dir.join(KEPT_DIR).join("incoming");
        let packs = self.git_dir.join(KEPT_DIR).join("pack");
        std::fs::create_dir_all(&incoming).map_err(io)?;
        std::fs::create_dir_all(&packs).map_err(io)?;
        clear_folder(&incoming);
        // Declared before the git call: when this future is dropped, git's group is ended first and its files are removed after.
        let _left = Incoming(incoming.clone());
        let engine_packs = self.scratch.engine_packs().await?;
        let mut input = format!("{head}\n");
        for floor in &self.floors {
            input.push_str(&format!("^{floor}\n"));
        }
        let base = incoming.join("pack");
        let hash = git_with_env_until_input(
            cwd,
            &[
                "pack-objects",
                "--revs",
                "--unpacked",
                "--incremental",
                "--non-empty",
                "-q",
                base.to_str().unwrap_or_default(),
            ],
            &[("GIT_DIR", gd)],
            Some(input.as_bytes()),
            give_up,
        )
        .await
        .map_err(ws)?;
        if self.scratch.engine_packs().await? != engine_packs {
            return Err(
                "snapshot: the engine repacked its store while the pack was written".to_string(),
            );
        }
        if hash.is_empty() {
            return Ok(None);
        }
        object_id(&hash)?;
        let name = format!("pack-{hash}");
        let bytes = std::fs::metadata(incoming.join(format!("{name}.pack")))
            .map_err(io)?
            .len();
        if !incoming.join(format!("{name}.idx")).is_file() {
            return Err("snapshot: git pack-objects left no index of its pack".to_string());
        }
        std::fs::write(
            incoming.join(format!("{name}.keep")),
            format!("{KEPT_LINE}{head}\n{KEPT_PACKS_LINE}{engine_packs}\n"),
        )
        .map_err(io)?;
        for ext in ["keep", "pack", "rev", "idx"] {
            let from = incoming.join(format!("{name}.{ext}"));
            if !from.exists() {
                continue;
            }
            if let Err(e) = std::fs::rename(&from, packs.join(format!("{name}.{ext}"))) {
                remove_pack(&packs, &name);
                self.keep.kept = self.keep.kept.take().filter(|kept| kept.name != name);
                return Err(io(e));
            }
        }
        if let Some(old) = &self.keep.kept
            && old.name != name
        {
            remove_pack(&packs, &old.name);
        }
        Ok(Some(KeptPack {
            head: head.to_string(),
            name,
            bytes,
            engine_packs,
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
