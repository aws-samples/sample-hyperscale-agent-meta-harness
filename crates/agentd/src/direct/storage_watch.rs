//! The watch of the session's storage.
//!
//! The session's storage (1 GiB, the checkout is on it) was seen to refuse every write of data with "No space left on device" while its
//! files added up to 16 percent of its size. A command then leaves files empty. Nothing a session can measure shows the cause; the
//! storage service's own log line in such a session is "Write failed: waiting to be backed up." This module says so when it sees a
//! refusal, and does nothing else: it stops no run, tells the engine nothing and deletes nothing.
//!
//! * **What makes it look.** A tool result that names no space left ([`Seen::ToolResult`]), each prompt boundary
//!   ([`Seen::PromptBoundary`]) and the engine error that may end the run ([`Seen::EngineError`]) each make the worker write a test file
//!   of its own. The write of the copies' marker ([`StorageWatch::marker_write`], [`Seen::CopyMarker`]) is a write the worker makes
//!   anyway, and its result counts the same way. The marker of a session's start is written the same way and has a word of its own.
//!   The first two only set a flag and wake the task ([`StorageWatch::wake`]): the collector
//!   and the prompt loop never wait for the storage.
//! * **The git settings at a session's end.** The end sequence writes `.git/config` and `.git/info/attributes` anew.
//!   That is a write the worker makes anyway too, and each of its answers counts the same
//!   way ([`StorageWatch::own_write`], [`Seen::GitSettings`]).
//! * **The write that decides.** [`HostWrites::test`] writes in the run's own folder, `<workspace root>/<run>`: it is on the session's
//!   storage, the worker makes it, and the engine cannot write there (its user owns only `direct/` inside it; in the `landlock` mode no
//!   grant covers it). The folder must be a real folder of the worker's own user that neither group nor others may write: it is
//!   opened once without following a link, checked through that handle, and the file is made and removed through it,
//!   so the write stays in the folder that was checked. The folders above it are the platform's
//!   mount and are not checked. The file is created new without following a link, mode 0600, 4 KiB, synced and removed. Only errno 28 from the create, the write or the sync is
//!   a refusal ([`StorageWrite::Refused`]). Every other error, an error of the remove, a folder that fails the rule and no answer within
//!   [`TREE_ANSWER_LIMIT`] is one log line and nothing else.
//! * **A notice needs room.** A refusal while the files are at or over the early-warning mark of the disk rule
//!   ([`Limits::storage_warn_mb`]) brings one log line: the disk rule's own notices and its stop cover a storage that is full.
//! * **One notice per stretch.** A stretch opens at the first refused write and closes at the first write the storage takes. The first
//!   notice of a session measures the files once ([`DiskProbe::storage_used`]) and is the full one; each later stretch brings a short
//!   notice with the count. One lock is held from before a write until its result is applied, so results are applied in the order the
//!   writes ran. The walk of the first notice is the one step that runs with the lock free: it is given up after
//!   [`TREE_ANSWER_LIMIT`], and a marker write or a test write made meanwhile does not wait for it. The write that is itself the
//!   session's first refusal waits for the walk, for at most that limit, whoever made it. What those writes meet while
//!   the files are measured belongs to that first notice: a refusal brings no notice of its own and starts no second walk, and
//!   after a write the storage took the notice is recorded for a stretch that is closed. A caller that is dropped while it waits
//!   for the walk (the watch's task at a session's end) leaves the next refusal its walk and its notice.
//! * **The copy named.** The copy task writes the marker of each stored copy through the watch ([`StorageWatch::marker_write_for`])
//!   and hands it the copy with the version the bucket gave its bundle ([`CopyRef`]). The newest copy of this session whose marker
//!   write the storage took before the session's first refusal is frozen at that refusal, and every notice of the session names it. A
//!   copy marked later is never named, and neither is a copy the bucket gave no version: a later copy replaces it at its key.
//! * **The event.** A `turn.progress` event of kind `notify` with the reason [`REASON`] and a `storage` member, recorded with an id made
//!   at the append so that its text names its own id. It goes to the event log only: no module of the seam is told (no ladder event), so
//!   nothing can turn it into a prompt for the engine.
//!
//! The switch of the live test row ([`WatchSetup::test_refusal`]) answers one test write with errno 28 without touching the storage:
//! the first one a tool result asks for after a copy of the session was marked, so that the notice of the row names a copy. The log
//! line and the event say that the switch made the answer. The switch also answers the first write of the git settings at the
//! session's end with errno 28 at its sync, once, without writing its data: the worker
//! then repeats the write as it does after a refusal of the storage. The log says that the switch made the answer, and no notice
//! follows it, because the storage refused no write.

use crate::checked_write::{FileCalls, Host};
use crate::direct::disk::{DiskProbe, Limits, MOUNT_QUOTA_MB};
use crate::direct::snapshot::TREE_ANSWER_LIMIT;
use crate::events::EventRecorder;
use crate::logging::LogSink;
use harness_protocol::direct::DIRECT_TURN;
use harness_protocol::envelope::EventType;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// The reason word of the notice.
pub const REASON: &str = "storage_refused";
/// The name of the test file in the run's folder.
pub const TEST_FILE: &str = ".ham-storage-test";
/// The size of the test write.
pub const TEST_BYTES: usize = 4096;
pub use crate::checked_write::ENOSPC;
/// The words of errno 28, as the notices and the log say them.
const NO_SPACE: &str = "no space left on device";
/// What is known about a refused write with room left on this storage. It is a statement about the storage, from measurements, and
/// not about the one refusal a notice reports, whose cause a session cannot read.
pub const KNOWN: &str = "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up. In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.";
/// What the notice of the test switch says after its other sentences.
pub const TEST_SWITCH_SENTENCE: &str =
    "This notice was made by the stage's test switch; the storage refused no write.";

/// A write to the session's storage that failed: at which step, with which errno when the system gave one, and the error's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError {
    /// `folder`, `leftover`, `create`, `write`, `sync`, `close`, `remove` or `rename`.
    pub step: &'static str,
    pub errno: Option<i32>,
    pub text: String,
    /// The answer was made by the switch of the live test row, not by the storage.
    pub test_switch: bool,
}

impl WriteError {
    fn io(step: &'static str, e: &std::io::Error) -> Self {
        Self {
            step,
            errno: e.raw_os_error(),
            text: e.to_string(),
            test_switch: false,
        }
    }

    fn plain(step: &'static str, text: String) -> Self {
        Self {
            step,
            errno: None,
            text,
            test_switch: false,
        }
    }

    /// Errno 28 from the create, the write, the sync, the close or the rename of the worker's own file: the storage refused the
    /// data. The close is a step of the marker's write.
    pub fn refused(&self) -> bool {
        crate::checked_write::refused_at(self.step, self.errno)
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.text)
    }
}

/// The two writes the worker makes in the run's folder on the session's storage. One trait, so a test fails both with one stand-in.
pub trait StorageWrites: Send + Sync + std::fmt::Debug {
    /// The test write: see [`HostWrites`].
    fn test(&self, run_dir: &Path) -> Result<(), WriteError>;
    /// The marker of the copies (`crate::direct::marker`): `bytes` written beside it, synced and renamed over it.
    fn marker(&self, run_dir: &Path, bytes: &[u8]) -> Result<(), WriteError>;
}

/// The real writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostWrites;

/// The test write of [`HostWrites`]. The run's folder is opened once without following a link. It is checked through that handle, and
/// the test file is made and removed through it, so the write stays in the folder that was checked whatever its path comes to name
/// afterwards. The folders above it are the platform's mount and are not checked. `checked` runs once, when the folder has
/// passed the rule and before the file is touched: the product gives nothing to run, and a test changes there what the path names.
pub fn test_write_with(run_dir: &Path, checked: &dyn Fn()) -> Result<(), WriteError> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    use rustix::io::Errno;
    use std::io::Write as _;
    let from = |step: &'static str, e: Errno| WriteError::io(step, &std::io::Error::from(e));
    // The folder is the run's own: a real folder (no link) of this process's user that neither group nor others may write. A
    // folder that fails the rule is not written to, because somebody else could then decide what the write meets.
    let dir = match rustix::fs::open(
        run_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(dir) => dir,
        // A link or a file in the folder's place.
        Err(e) if e == Errno::LOOP || e == Errno::NOTDIR => {
            return Err(WriteError::plain(
                "folder",
                "the run's folder is not a folder".to_string(),
            ));
        }
        Err(e) => return Err(from("folder", e)),
    };
    let folder = rustix::fs::fstat(&dir).map_err(|e| from("folder", e))?;
    let me = rustix::process::geteuid().as_raw();
    if folder.st_uid != me
        || Mode::from_raw_mode(folder.st_mode).intersects(Mode::WGRP | Mode::WOTH)
    {
        return Err(WriteError::plain(
            "folder",
            "the run's folder is not the worker's own".to_string(),
        ));
    }
    checked();
    // A file that a test which never ended left behind is removed as a name: a link there is not followed.
    match rustix::fs::unlinkat(&dir, TEST_FILE, AtFlags::empty()) {
        Ok(()) => {}
        Err(e) if e == Errno::NOENT => {}
        Err(e) => return Err(from("leftover", e)),
    }
    // The file is made new in the folder that was checked, and no link is followed.
    let fd = rustix::fs::openat(
        &dir,
        TEST_FILE,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|e| from("create", e))?;
    let mut f = std::fs::File::from(fd);
    let wrote = f
        .write_all(&[0u8; TEST_BYTES])
        .map_err(|e| WriteError::io("write", &e))
        .and_then(|()| f.sync_all().map_err(|e| WriteError::io("sync", &e)));
    drop(f);
    let removed =
        rustix::fs::unlinkat(&dir, TEST_FILE, AtFlags::empty()).map_err(|e| from("remove", e));
    // What the write met comes first: a refused write whose file could not be removed either is a refused write.
    wrote.and(removed)
}

impl StorageWrites for HostWrites {
    fn test(&self, run_dir: &Path) -> Result<(), WriteError> {
        test_write_with(run_dir, &|| {})
    }

    fn marker(&self, run_dir: &Path, bytes: &[u8]) -> Result<(), WriteError> {
        crate::direct::marker::write_locked(run_dir, bytes)
            .map_err(|(step, e)| WriteError::io(step, &e))
    }
}

/// What one write to the session's storage came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageWrite {
    /// The storage took the write.
    Took,
    /// Errno 28: the storage refused the data.
    Refused,
    /// Any other error, with its words: nothing is concluded from it.
    Inconclusive(String),
    /// No answer within [`TREE_ANSWER_LIMIT`], or an earlier test write is still out: nothing is concluded.
    NoAnswer,
}

/// A stored copy of the work, as the notice names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyRef {
    /// The session that saved it.
    pub session: u32,
    /// Its number within the run.
    pub count: u32,
    /// When it was saved (RFC 3339, UTC).
    pub at: String,
    /// The commit it holds.
    pub head_sha: String,
    /// The version the bucket gave its bundle.
    pub version_id: String,
}

impl CopyRef {
    fn to_value(&self) -> Value {
        json!({
            "session": self.session,
            "count": self.count,
            "at": self.at,
            "headSha": self.head_sha,
            "versionId": self.version_id,
        })
    }

    /// `2026-11-21 00:37 UTC` for `2026-11-21T00:37:12Z`: the time to the minute.
    fn minute(&self) -> String {
        let head: String = self.at.chars().take(16).collect();
        if head.chars().count() == 16 && self.at.as_bytes().get(10) == Some(&b'T') {
            format!("{} UTC", head.replacen('T', " ", 1))
        } else {
            self.at.clone()
        }
    }
}

/// What a write of the copies' marker is for ([`StorageWatch::marker_write_for`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marked {
    /// The marker of a session's start: it names no copy of this session.
    Start,
    /// The marker of a stored copy whose bundle the bucket gave a version.
    Copy(CopyRef),
    /// The marker of a stored copy whose bundle the bucket gave no version: it cannot be named once a newer copy replaced it, and the
    /// copy before it is no longer the last one saved.
    Unnamed,
}

/// What made the worker look at the storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// A tool result named no space left on device.
    ToolResult,
    /// A prompt is about to be sent.
    PromptBoundary,
    /// The marker of a stored copy was written.
    CopyMarker,
    /// The marker of a session's start was written. It is no copy's marker and names no copy of this session.
    StartMarker,
    /// The engine answered error -32603 and the disks are measured afresh.
    EngineError,
    /// The git settings were written anew at the session's end (`crate::direct::git_settings`).
    GitSettings,
}

impl Seen {
    /// The word of the event's `storage.seen` member.
    pub fn word(self) -> &'static str {
        match self {
            Self::ToolResult => "tool",
            Self::PromptBoundary => "prompt",
            Self::CopyMarker => "copy",
            Self::StartMarker => "start",
            Self::EngineError => "error",
            Self::GitSettings => "end",
        }
    }

    fn log_words(self) -> &'static str {
        match self {
            Self::ToolResult => "a tool result",
            Self::PromptBoundary => "a prompt boundary",
            Self::CopyMarker => "a copy's marker",
            Self::StartMarker => "the marker of the session's start",
            Self::EngineError => "an engine error",
            Self::GitSettings => "the git settings at the session's end",
        }
    }
}

/// What a watch is built from.
#[derive(Clone, Debug)]
pub struct WatchSetup {
    pub writes: Arc<dyn StorageWrites>,
    /// Measures the files for the first notice of the session ([`DiskProbe::storage_used`]).
    pub probe: Arc<dyn DiskProbe>,
    pub rec: Arc<EventRecorder>,
    pub log: Arc<dyn LogSink>,
    pub run_id: String,
    /// The session of the run (1 for the first).
    pub session: u32,
    /// The run's own folder on the session's storage, `<workspace root>/<run>`.
    pub run_dir: PathBuf,
    /// The disk rule's two numbers: a notice needs room below [`Limits::storage_warn_mb`].
    pub limits: Limits,
    /// The switch of the live test row (`crate::config::STORAGE_TEST_REFUSAL_VAR`, read only by a worker built for an account entry with
    /// the role `test`): the first test write that a tool result asks for after a copy of the session was marked is answered with
    /// errno 28, once in the session, without touching the storage. The log line and the event say that the switch made the answer.
    /// The first write of the git settings at the session's end is answered the same way, once
    /// ([`StorageWatch::settings_calls`]).
    pub test_refusal: bool,
}

/// The file calls of the write of the git settings under the switch of the live test row. The first file a session's end
/// writes is answered as the storage was seen to answer, once in the session: the file beside the target is made, no data is
/// written to it, and its sync answers errno 28. Every other call is the host's.
#[derive(Debug)]
struct SwitchCalls {
    log: Arc<dyn LogSink>,
    /// The one answer was made, or the file that gets it is being written.
    spent: AtomicBool,
    /// The file being written is the one the switch answers.
    current: AtomicBool,
    /// The answer was made and the watch was not told of a refused write since ([`StorageWatch::own_write`]).
    untold: AtomicBool,
}

impl FileCalls for SwitchCalls {
    fn create(&self, path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
        let answer = !self.spent.swap(true, Ordering::SeqCst);
        self.current.store(answer, Ordering::SeqCst);
        Host.create(path, mode)
    }

    fn write(&self, file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
        if self.current.load(Ordering::SeqCst) {
            return Ok(());
        }
        Host.write(file, bytes)
    }

    fn sync(&self, file: &std::fs::File) -> std::io::Result<()> {
        if self.current.swap(false, Ordering::SeqCst) {
            self.untold.store(true, Ordering::SeqCst);
            self.log.line(
                "direct turn git settings: the stage's test switch answered this write with no space left on device; the storage refused no write",
            );
            return Err(std::io::Error::from_raw_os_error(ENOSPC));
        }
        Host.sync(file)
    }
}

/// The stretch in progress.
#[derive(Clone, Copy, Debug)]
struct Open {
    /// The number of the notice that announced it; 0 for a stretch that brought none (the files were at the warning mark).
    n: u32,
    since: Instant,
}

#[derive(Debug, Default)]
struct State {
    open: Option<Open>,
    /// Notices sent in this session.
    notices: u32,
    /// The id of the session's first notice.
    first_id: Option<String>,
    /// The newest copy whose marker write the storage took, until the session's first refusal.
    candidate: Option<CopyRef>,
    /// The session saw a refusal: the candidate is frozen and no later copy replaces it.
    refusal_seen: bool,
    /// The test switch has made its one answer.
    switch_spent: bool,
    /// The newest result applied was a write the storage took. It says whether a stretch is open when the walk of the
    /// session's first notice answers, because other writes are applied while the files are measured.
    took_last: bool,
}

/// What asked for a test write that has not started yet.
#[derive(Clone, Copy, Debug)]
struct Asked {
    /// What asked first: a refusal's notice carries its word.
    seen: Seen,
    /// A tool result named no space left on device while this write, which something else had asked for, had not
    /// started. The write is made after the result and answers it too.
    tool_named: bool,
}

#[derive(Debug)]
struct Inner {
    setup: WatchSetup,
    /// Held from before a write until its result is applied.
    state: tokio::sync::Mutex<State>,
    /// What asked for a test write that has not started yet.
    pending: std::sync::Mutex<Option<Asked>>,
    notify: tokio::sync::Notify,
    /// A test write that got no answer in time is still out on its thread: no second one starts.
    out: Arc<AtomicBool>,
    /// The walk of the session's first notice is out, with the lock free. A refusal seen meanwhile starts no second walk.
    /// [`WalkOut`] gives the flag back when the walk's step is over, and when the caller that waited for the walk is dropped.
    measuring: AtomicBool,
    /// Test writes started (the real ones), for the cost tests.
    started: AtomicU64,
    /// Test writes [`StorageWatch::wake`] asked for, and how many of them the task has answered ([`StorageWatch::idle`]).
    asked: AtomicU64,
    answered: AtomicU64,
    /// The file calls of the write of the git settings when the switch of the live test row is on.
    settings_switch: Option<Arc<SwitchCalls>>,
}

/// The watch of one session. Cheap to clone.
#[derive(Clone, Debug)]
pub struct StorageWatch {
    inner: Arc<Inner>,
}

/// The task of a watch; [`WatchTask::stop`] ends it.
#[derive(Debug)]
pub struct WatchTask(tokio::task::JoinHandle<()>);

impl WatchTask {
    /// Ends the task. A test write that is out ends on its own thread.
    pub async fn stop(self) {
        self.0.abort();
        let _ = self.0.await;
    }
}

impl StorageWatch {
    pub fn new(setup: WatchSetup) -> Self {
        let settings_switch = setup.test_refusal.then(|| {
            Arc::new(SwitchCalls {
                log: setup.log.clone(),
                spent: AtomicBool::new(false),
                current: AtomicBool::new(false),
                untold: AtomicBool::new(false),
            })
        });
        Self {
            inner: Arc::new(Inner {
                setup,
                state: tokio::sync::Mutex::new(State::default()),
                pending: std::sync::Mutex::new(None),
                notify: tokio::sync::Notify::new(),
                out: Arc::new(AtomicBool::new(false)),
                measuring: AtomicBool::new(false),
                started: AtomicU64::new(0),
                asked: AtomicU64::new(0),
                answered: AtomicU64::new(0),
                settings_switch,
            }),
        }
    }

    /// Starts the task that makes the test writes [`Self::wake`] asks for.
    pub fn start(&self) -> WatchTask {
        let inner = self.inner.clone();
        WatchTask(tokio::spawn(async move {
            loop {
                inner.notify.notified().await;
                loop {
                    let next = inner
                        .pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take();
                    let Some(asked) = next else { break };
                    inner.test(asked.seen, asked.tool_named).await;
                    inner.answered.fetch_add(1, Ordering::SeqCst);
                }
            }
        }))
    }

    /// Asks for one test write and returns at once: it sets a flag and wakes the task. Wakes that arrive before the task ran are one
    /// test write, which keeps the word of the first. A tool result that finds the write of something else pending is
    /// remembered with it, so that the log says that a test write worked after that result too.
    pub fn wake(&self, seen: Seen) {
        let mut pending = self.inner.pending.lock().unwrap_or_else(|p| p.into_inner());
        match pending.as_mut() {
            None => {
                *pending = Some(Asked {
                    seen,
                    tool_named: false,
                });
                self.inner.asked.fetch_add(1, Ordering::SeqCst);
            }
            Some(first) if seen == Seen::ToolResult && first.seen != Seen::ToolResult => {
                first.tool_named = true;
            }
            Some(_) => {}
        }
        drop(pending);
        self.inner.notify.notify_one();
    }

    /// Makes one test write now and returns what it came to, after its result was applied. The prompt loop calls it beside each
    /// fresh measurement of the disks after an engine error, whether the run then stops or goes on, and once more at the stop after
    /// every route refused the switch; the words of a stop depend on the answer.
    pub async fn test_now(&self, seen: Seen) -> StorageWrite {
        self.inner.test(seen, false).await
    }

    /// [`Self::marker_write_for`] for a stored copy the bucket gave a version (`Some`) or for the marker of a session's start (`None`).
    pub async fn marker_write(
        &self,
        bytes: Vec<u8>,
        copy: Option<CopyRef>,
    ) -> Result<(), WriteError> {
        self.marker_write_for(bytes, copy.map_or(Marked::Start, Marked::Copy))
            .await
    }

    /// Writes the marker of the copies through the watch: the write's result counts like a test write's. `marked` says what the
    /// marker is for. When the storage took the write and the session has seen no refusal yet, a stored copy becomes the copy a
    /// notice would name, and a stored copy the bucket gave no version leaves none to name. The error is returned for the caller's
    /// log line.
    pub async fn marker_write_for(&self, bytes: Vec<u8>, marked: Marked) -> Result<(), WriteError> {
        let inner = &self.inner;
        // The marker of a session's start is no copy's marker, and a notice says so with a word of its own.
        let seen = if marked == Marked::Start {
            Seen::StartMarker
        } else {
            Seen::CopyMarker
        };
        let mut st = inner.state.lock().await;
        let (writes, dir) = (inner.setup.writes.clone(), inner.setup.run_dir.clone());
        let work = tokio::task::spawn_blocking(move || writes.marker(&dir, &bytes));
        let answer = match tokio::time::timeout(TREE_ANSWER_LIMIT, work).await {
            Ok(Ok(done)) => done,
            Ok(Err(e)) => Err(WriteError::plain("write", e.to_string())),
            Err(_) => Err(WriteError::plain(
                "write",
                format!(
                    "the storage did not answer within {} s",
                    TREE_ANSWER_LIMIT.as_secs()
                ),
            )),
        };
        let result = match &answer {
            Ok(()) => {
                if !st.refusal_seen {
                    match marked {
                        Marked::Start => {}
                        Marked::Copy(copy) => st.candidate = Some(copy),
                        Marked::Unnamed => st.candidate = None,
                    }
                }
                StorageWrite::Took
            }
            Err(e) if e.refused() => StorageWrite::Refused,
            Err(e) => StorageWrite::Inconclusive(e.to_string()),
        };
        // A marker write that failed in another way is logged by its caller, as before the watch.
        if matches!(result, StorageWrite::Took | StorageWrite::Refused) {
            inner.step(st, seen, &result, false).await;
        }
        answer
    }

    /// Counts what a write the worker made anyway met, as a test write's result is counted: `refused` is errno 28 from the
    /// write, `false` a write the storage took. For the git settings the end sequence writes anew; the copies' marker has
    /// [`Self::marker_write_for`].
    pub async fn own_write(&self, seen: Seen, refused: bool) {
        // An answer the switch of the live test row made is no refusal of the storage: it opens no stretch and brings no notice.
        if refused
            && self
                .inner
                .settings_switch
                .as_ref()
                .is_some_and(|s| s.untold.swap(false, Ordering::SeqCst))
        {
            return;
        }
        let st = self.inner.state.lock().await;
        let result = if refused {
            StorageWrite::Refused
        } else {
            StorageWrite::Took
        };
        self.inner.step(st, seen, &result, false).await;
    }

    /// The file calls for the write of the git settings at this session's end (`crate::direct::git_settings`): the host's,
    /// or, with the switch of the live test row ([`WatchSetup::test_refusal`]), calls that answer the first file written with errno 28
    /// at its sync, once in the session, without writing its data. The log says that the switch made the answer.
    pub fn settings_calls(&self) -> Arc<dyn FileCalls> {
        match &self.inner.settings_switch {
            Some(switch) => switch.clone(),
            None => Arc::new(Host),
        }
    }

    /// Returns once no write is in progress (the lock is free). The walk of a session's first notice runs with the lock free
    /// and is not waited for here: the caller whose write is that first refusal waits for it, and [`Self::idle`] waits
    /// for a walk of the watch's own task through its count of the wakes the task answered.
    pub async fn settled(&self) {
        drop(self.inner.state.lock().await);
    }

    /// Returns once every test write that [`Self::wake`] asked for was made and applied, with the notice of a first refusal
    /// recorded. The end of a session waits for it (with a bound of its own) before it stops the task, so that a refusal seen at
    /// the last tool result still brings its notice. A walk of the files that another caller started is that caller's wait and
    /// is not waited for: at a session's end the copy task is stopped and the prompt loop is over, so no other caller is left.
    pub async fn idle(&self) {
        while self.inner.answered.load(Ordering::SeqCst) < self.inner.asked.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        self.settled().await;
    }

    /// How many real test writes the watch has started.
    pub fn test_writes_started(&self) -> u64 {
        self.inner.started.load(Ordering::SeqCst)
    }
}

/// Gives the flag of the walk back when it is dropped: at the end of the walk's step, and when the caller that waited for
/// the walk is dropped first (the watch's task, which a session's end stops). The next refusal then measures the files again and
/// brings its notice.
struct WalkOut<'a>(&'a AtomicBool);

impl Drop for WalkOut<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Inner {
    /// One test write under the lock, and its result applied. `tool_named`: a tool result named no space left on device while
    /// this write, which `seen` had asked for, had not started.
    async fn test(&self, seen: Seen, tool_named: bool) -> StorageWrite {
        let mut st = self.state.lock().await;
        // The switch of the live test row: one answer, to a test write that a tool result asked for once a copy of the session is
        // marked, so that the notice names a copy and the row can download it.
        if self.setup.test_refusal
            && !st.switch_spent
            && seen == Seen::ToolResult
            && st.candidate.is_some()
        {
            st.switch_spent = true;
            self.setup.log.line(
                "direct turn storage: the stage's test switch answered this test write with no space left on device; the storage refused no write",
            );
            let result = StorageWrite::Refused;
            self.step(st, seen, &result, true).await;
            return result;
        }
        let result = self.write().await;
        // The storage took a write that was made after a tool result named the words, with no stretch open: the log says
        // so, as `step` does for a write that the tool result itself asked for.
        let answered_a_tool_result =
            tool_named && result == StorageWrite::Took && st.open.is_none();
        self.step(st, seen, &result, false).await;
        if answered_a_tool_result {
            self.setup.log.line(&format!(
                "direct turn storage: a tool result named no space left on device while the test write of {} had not started; that test write to the session's storage worked",
                seen.log_words()
            ));
        }
        result
    }

    /// The test write on a thread of its own, waited for [`TREE_ANSWER_LIMIT`] at the most. While one is out, no second one starts.
    async fn write(&self) -> StorageWrite {
        if self.out.swap(true, Ordering::SeqCst) {
            return StorageWrite::NoAnswer;
        }
        self.started.fetch_add(1, Ordering::SeqCst);
        let (writes, dir, out) = (
            self.setup.writes.clone(),
            self.setup.run_dir.clone(),
            self.out.clone(),
        );
        let work = tokio::task::spawn_blocking(move || {
            let done = writes.test(&dir);
            out.store(false, Ordering::SeqCst);
            done
        });
        match tokio::time::timeout(TREE_ANSWER_LIMIT, work).await {
            Ok(Ok(Ok(()))) => StorageWrite::Took,
            Ok(Ok(Err(e))) if e.refused() => StorageWrite::Refused,
            Ok(Ok(Err(e))) => StorageWrite::Inconclusive(e.to_string()),
            Ok(Err(e)) => {
                self.out.store(false, Ordering::SeqCst);
                StorageWrite::Inconclusive(format!("write: {e}"))
            }
            Err(_) => StorageWrite::NoAnswer,
        }
    }

    /// Applies the result of one write. It takes the lock its caller has held from before the write and gives it up when the
    /// result is applied. The walk of the session's first notice runs with the lock free and is given up after
    /// [`TREE_ANSWER_LIMIT`] (around the probe's own budget), so a walk that hangs holds up no other write; the lock is taken
    /// again only to record the notice.
    async fn step(
        &self,
        mut st: tokio::sync::MutexGuard<'_, State>,
        seen: Seen,
        result: &StorageWrite,
        by_switch: bool,
    ) {
        let log = &self.setup.log;
        match result {
            StorageWrite::Took => {
                st.took_last = true;
                match st.open.take() {
                Some(open) if open.n == 0 => log.line(&format!(
                    "direct turn storage takes writes again after {} s",
                    open.since.elapsed().as_secs()
                )),
                Some(open) => log.line(&format!(
                    "direct turn storage takes writes again after {} s; stretch {} closed",
                    open.since.elapsed().as_secs(),
                    open.n
                )),
                None if seen == Seen::ToolResult => log.line(
                    "direct turn storage: a tool result named no space left on device; a test write to the session's storage worked",
                ),
                None => {}
            }
            }
            StorageWrite::Inconclusive(why) => {
                log.line(&format!(
                    "direct turn storage: test write inconclusive ({why})"
                ));
            }
            StorageWrite::NoAnswer => log.line(&format!(
                "direct turn storage: test write inconclusive (no answer within {} s)",
                TREE_ANSWER_LIMIT.as_secs()
            )),
            StorageWrite::Refused => {
                st.took_last = false;
                if st.open.is_some() {
                    return;
                }
                // The copy a notice names is the one that stood at the session's first refusal.
                st.refusal_seen = true;
                let since = Instant::now();
                if st.first_id.is_some() {
                    // A later stretch: the short notice, with no walk.
                    st.notices += 1;
                    let n = st.notices;
                    st.open = Some(Open { n, since });
                    log.line(&format!(
                        "direct turn storage refused a write (seen at {}): {NO_SPACE}; stretch {n}",
                        seen.log_words()
                    ));
                    self.record(&st, seen, n, None, false, by_switch);
                    return;
                }
                // The session's first notice: the files are measured once, with the lock free. A refusal that finds a walk out
                // starts no second one: the notice of that walk tells of it.
                if self.measuring.swap(true, Ordering::SeqCst) {
                    return;
                }
                let _walk_out = WalkOut(&self.measuring);
                drop(st);
                let walked =
                    tokio::time::timeout(TREE_ANSWER_LIMIT, self.setup.probe.storage_used()).await;
                let mut st = self.state.lock().await;
                let gave_up = walked.is_err();
                let (used, partial) = walked.unwrap_or((None, false));
                // What the newest write met while the files were measured says whether the stretch is still open.
                let still_open = !st.took_last;
                if used.is_some_and(|u| u >= self.setup.limits.storage_warn_mb()) {
                    if still_open {
                        st.open = Some(Open { n: 0, since });
                    }
                    log.line(&format!(
                        "direct turn storage refused a write while its files add up to {} MiB: the disk notices cover it",
                        used.unwrap_or_default()
                    ));
                    return;
                }
                st.notices += 1;
                let n = st.notices;
                // The stretch this notice tells of is open, unless the storage took a write while the files were measured.
                if still_open {
                    st.open = Some(Open { n, since });
                }
                log.line(&format!(
                    "direct turn storage refused a write (seen at {}): {NO_SPACE}; stretch {n}, files {}{}",
                    seen.log_words(),
                    match used {
                        Some(u) => format!("{}{u} MiB", if partial { "at least " } else { "" }),
                        None if gave_up => format!(
                            "not measured (no answer within {} s)",
                            TREE_ANSWER_LIMIT.as_secs()
                        ),
                        None => "not measured".to_string(),
                    },
                    if still_open {
                        ""
                    } else {
                        "; the storage took a write while the files were measured"
                    }
                ));
                let id = self.record(&st, seen, n, Some((used, partial)), true, by_switch);
                st.first_id = Some(id);
            }
        }
    }

    /// Records the notice and returns its id. `size` is the measurement of the first notice; `full` chooses its words.
    fn record(
        &self,
        st: &State,
        seen: Seen,
        n: u32,
        size: Option<(Option<u64>, bool)>,
        full: bool,
        by_switch: bool,
    ) -> String {
        let setup = &self.setup;
        let copy = st.candidate.clone();
        let first = st.first_id.clone();
        self.setup
            .rec
            .record_urgent_minted(EventType::TurnProgress, |id| {
                let mut storage = json!({
                    "session": setup.session,
                    "stretch": n,
                    "seen": seen.word(),
                    "partial": size.is_some_and(|(_, partial)| partial),
                    "quotaMb": MOUNT_QUOTA_MB,
                });
                if let Some((Some(used), _)) = size {
                    storage["usedMb"] = json!(used);
                }
                if let Some(first) = &first {
                    storage["first"] = json!(first);
                }
                if let Some(copy) = &copy {
                    storage["copyBefore"] = copy.to_value();
                }
                if by_switch {
                    storage["testSwitch"] = json!(true);
                }
                // The command names the session's first notice: its own id for the first one.
                let named = first.as_deref().unwrap_or(id);
                let detail = if full {
                    full_words(
                        &setup.run_id,
                        named,
                        size.unwrap_or((None, false)),
                        copy.as_ref(),
                    )
                } else {
                    short_words(&setup.run_id, named, n, copy.as_ref())
                };
                let detail = if by_switch {
                    format!("{detail} {TEST_SWITCH_SENTENCE}")
                } else {
                    detail
                };
                json!({
                    "turn": DIRECT_TURN,
                    "kind": "notify",
                    "reason": REASON,
                    "outageMinutes": null,
                    "detail": detail,
                    "storage": storage,
                })
            })
    }
}

/// The sentence of the newest copy, which every form of the notice ends with.
fn newest_words(run: &str) -> String {
    format!(
        "ham pull {run} --snapshot downloads the newest copy, which holds the files as they are."
    )
}

/// The words of the first notice of a session.
pub fn full_words(
    run: &str,
    id: &str,
    (used, partial): (Option<u64>, bool),
    copy: Option<&CopyRef>,
) -> String {
    let first = match used {
        Some(used) => format!(
            "The session's storage refused a write while its files add up to {}{used} MiB of {MOUNT_QUOTA_MB} MiB ({NO_SPACE}).",
            if partial { "at least " } else { "" }
        ),
        None => format!(
            "The session's storage refused a write ({NO_SPACE}); the size of its files could not be measured."
        ),
    };
    let copy = match copy {
        Some(c) => format!(
            "Copy {} ({}) is the last copy saved before ham first saw this: ham pull {run} --before-refusal {id} downloads it.",
            c.count,
            c.minute()
        ),
        None => "This session had saved no copy before ham first saw this.".to_string(),
    };
    format!(
        "{first} {KNOWN} A file that a command wrote meanwhile can be empty; ham does not know which. {copy} {}",
        newest_words(run)
    )
}

/// The words of a later notice of a session: the count and the same commands.
pub fn short_words(run: &str, first_id: &str, n: u32, copy: Option<&CopyRef>) -> String {
    let copy = match copy {
        Some(c) => format!(
            "Copy {} ({}) is still the last copy saved before ham first saw it: ham pull {run} --before-refusal {first_id} downloads it.",
            c.count,
            c.minute()
        ),
        None => "This session had saved no copy before ham first saw it.".to_string(),
    };
    format!(
        "The session's storage refused a write again ({NO_SPACE}); ham has seen this {n} times in this session. {copy} {}",
        newest_words(run)
    )
}
