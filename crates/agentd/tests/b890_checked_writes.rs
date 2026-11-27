//! The worker's own writes on the session's storage at the end of a session.
//!
//! The session's storage was seen to refuse the data of a write with errno 28 while its files left room. The write call itself
//! works; the sync and the close answer "No space left on device", and the file is left empty. In 3 of 70 measured runs on a restored
//! repository the run then ended with "... .git/config is not what the run started with", with the storage service's refusal line in
//! the same millisecond. The stand-in of these tests ([`Refusing`]) is that storage at the file calls of the write.
//!
//! The first test reproduced that label before the fix (its red output is kept with the change's records). The others hold the
//! rule: the write is a checked one, a refused write leaves the file as it stood, it is repeated while the answer is errno 28 and
//! only then, the bound and the pause are the measured ones, the end of the run names the storage when the bound runs out, a real
//! change of the file keeps its label, no link is followed, and the watch of the session's storage is told.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::checked_write::{self, FileCalls, Host};
use agentd::direct::classify;
use agentd::direct::disk::{DiskProbe, Limits, Reading};
use agentd::direct::git_settings::{self, NotPutBack, REPEAT_EVERY, REPEAT_FOR, Repeat};
use agentd::direct::marker;
use agentd::direct::snapshot::TREE_ANSWER_LIMIT;
use agentd::direct::storage_watch::{HostWrites, StorageWatch, WatchSetup, WriteError};
use agentd::events::EventRecorder;
use agentd::logging::{LogSink, MemoryLog};
use agentd::workspace::{
    GitState, NEW_SUFFIX, TurnAuthor, Worktree, WsError, commit_turn_verified, prepare_local,
};
use common::{python_calc, scratch};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::envelope::{EventType, Payload};
use harness_protocol::runtime::TurnOutcome;
use serde_json::Value;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

const RUN: &str = "01RUNB890CHECKEDWRITES0001";

/// The label of the measured runs, word for word.
const CHANGED: &str = "Cline stopped: the harness ended the run (agentd did not commit or bundle the checkout: .git/config is not what the run started with)";

/// A short pause and a short bound, so that a test waits milliseconds where a session waits seconds.
const QUICK: Repeat = Repeat {
    every: Duration::from_millis(5),
    for_at_most: Duration::from_secs(2),
};

fn no_space() -> std::io::Error {
    std::io::Error::from_raw_os_error(28)
}

/// Which call of a refused write answers errno 28.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    Sync,
    Close,
    SyncAndClose,
}

impl At {
    /// The step a checked write names for it: what the sync met comes before what the close met.
    fn step(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Sync | Self::SyncAndClose => "sync",
        }
    }
}

/// The session's storage while it refuses data, as it was seen: the write call works and no byte arrives, and the sync, the close or
/// both answer errno 28. It takes the first `skip` files that are written, refuses the `files` files after them and takes every file
/// after those.
#[derive(Debug)]
struct Refusing {
    at: At,
    skip: AtomicU32,
    files_left: AtomicU32,
    /// The file being written is one the storage refuses.
    current: AtomicBool,
    /// Files the storage refused, and files it took.
    refused: AtomicU32,
    taken: AtomicU32,
}

impl Refusing {
    fn files(at: At, files: u32) -> Arc<Self> {
        Self::after(0, at, files)
    }

    fn after(skip: u32, at: At, files: u32) -> Arc<Self> {
        Arc::new(Self {
            at,
            skip: AtomicU32::new(skip),
            files_left: AtomicU32::new(files),
            current: AtomicBool::new(false),
            refused: AtomicU32::new(0),
            taken: AtomicU32::new(0),
        })
    }

    fn refusing(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }

    fn refused(&self) -> u32 {
        self.refused.load(Ordering::SeqCst)
    }

    fn taken(&self) -> u32 {
        self.taken.load(Ordering::SeqCst)
    }
}

fn take_one(n: &AtomicU32) -> bool {
    n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

impl FileCalls for Refusing {
    fn create(&self, path: &Path, mode: u32) -> std::io::Result<File> {
        let refuse = !take_one(&self.skip) && take_one(&self.files_left);
        self.current.store(refuse, Ordering::SeqCst);
        if refuse {
            self.refused.fetch_add(1, Ordering::SeqCst);
        } else {
            self.taken.fetch_add(1, Ordering::SeqCst);
        }
        Host.create(path, mode)
    }

    fn write(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
        if self.refusing() {
            // The call works; the data never arrives.
            return Ok(());
        }
        file.write_all(bytes)
    }

    fn sync(&self, file: &File) -> std::io::Result<()> {
        if self.refusing() && matches!(self.at, At::Sync | At::SyncAndClose) {
            return Err(no_space());
        }
        file.sync_all()
    }

    fn close(&self, file: File) -> std::io::Result<()> {
        drop(file);
        if self.refusing() && matches!(self.at, At::Close | At::SyncAndClose) {
            return Err(no_space());
        }
        Ok(())
    }
}

/// A storage that answers one step of every write with one errno, and counts the files it was asked to make.
#[derive(Debug)]
struct Failing {
    step: &'static str,
    errno: i32,
    created: AtomicU32,
}

impl Failing {
    fn at(step: &'static str, errno: i32) -> Arc<Self> {
        Arc::new(Self {
            step,
            errno,
            created: AtomicU32::new(0),
        })
    }

    fn answer(&self, step: &str) -> std::io::Result<()> {
        if self.step == step {
            return Err(std::io::Error::from_raw_os_error(self.errno));
        }
        Ok(())
    }
}

impl FileCalls for Failing {
    fn create(&self, path: &Path, mode: u32) -> std::io::Result<File> {
        self.created.fetch_add(1, Ordering::SeqCst);
        self.answer("create")?;
        Host.create(path, mode)
    }

    fn write(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
        self.answer("write")?;
        file.write_all(bytes)
    }

    fn sync(&self, file: &File) -> std::io::Result<()> {
        self.answer("sync")?;
        file.sync_all()
    }

    fn close(&self, file: File) -> std::io::Result<()> {
        drop(file);
        self.answer("close")
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.answer("rename")?;
        std::fs::rename(from, to)
    }
}

/// A storage that takes every write, and somebody who changes the file right after agentd put it in place.
#[derive(Debug)]
struct Meddled;

impl FileCalls for Meddled {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)?;
        let mut f = std::fs::OpenOptions::new().append(true).open(to)?;
        f.write_all(b"[filter \"x\"]\n\tclean = cat\n")
    }
}

async fn checkout(label: &str) -> Worktree {
    let d = scratch(label);
    let src = python_calc(&d);
    prepare_local(&d.join("ws"), "run890", "direct", &src)
        .await
        .unwrap()
}

fn config_of(wt: &Worktree) -> std::path::PathBuf {
    wt.repo.join(".git/config")
}

fn beside_config(wt: &Worktree) -> std::path::PathBuf {
    wt.repo.join(format!(".git/config{NEW_SUFFIX}"))
}

fn gone(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_err()
}

fn sink(log: &Arc<MemoryLog>) -> Arc<dyn LogSink> {
    log.clone()
}

/// [`git_settings::put_back`] with nothing to do at the first refusal.
async fn put_back(
    state: &GitState,
    wt: &Worktree,
    calls: Arc<dyn FileCalls>,
    repeat: Repeat,
    log: &Arc<dyn LogSink>,
    watch: Option<&StorageWatch>,
) -> Result<(), NotPutBack> {
    git_settings::put_back(state, wt, calls, repeat, log, watch, async || {}).await
}

/// The label of a run the engine ended with a message, as the end sequence makes it from what putting the git settings back came to.
fn label_after(put: &Result<(), NotPutBack>) -> String {
    match put {
        Ok(()) => DirectOutcome::ReportedDone.label_for(None),
        Err(why) => {
            let c = classify::no_git(why, RUN);
            c.outcome.label_for(c.detail.as_deref())
        }
    }
}

/// The measured case: the storage refuses the data of the worker's write of `.git/config` once, at the sync, at the close or at both.
/// The run does not end as a repository that changed: the worker sees the refusal, writes again and finds the file as the run started.
#[tokio::test]
async fn b890_a_write_of_the_git_settings_that_the_storage_refuses_once_does_not_end_the_run_as_a_changed_repository()
 {
    for at in [At::Sync, At::Close, At::SyncAndClose] {
        let wt = checkout("b890-once").await;
        let config = config_of(&wt);
        let at_start = std::fs::read(&config).unwrap();
        assert!(!at_start.is_empty());
        let state = GitState::snapshot(&wt).unwrap();
        let storage = Refusing::files(at, 1);
        let log = MemoryLog::new();
        // The session's clock, stopped: the bound and the "0 s" of the last line count the pause, not this machine's time for
        // a write.
        tokio::time::pause();
        let put = put_back(&state, &wt, storage.clone(), QUICK, &sink(&log), None).await;
        tokio::time::resume();
        let label = label_after(&put);
        assert_eq!(
            label,
            "Cline reported done",
            "{at:?}: {put:?}; the worker's log: {:?}",
            log.lines()
        );
        assert_eq!(std::fs::read(&config).unwrap(), at_start, "{at:?}");
        state.verify(&wt).unwrap();
        assert_eq!((storage.refused(), storage.taken()), (1, 1), "{at:?}");
        // The worker's log says what it met and that the write worked in the end; it never says the repository is not trusted.
        let lines = log.lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0],
            format!(
                "direct turn git settings: the session's storage refused the write of .git/config ({}: No space left on device (os error 28)); the write is repeated every 0 s for up to 2 s",
                at.step()
            )
        );
        assert_eq!(
            lines[1],
            "direct turn git settings put back after 1 refused write (0 s)"
        );
    }
}

/// One try: a refused write is an error of its own, and the file stands as the engine left it, neither empty nor half written.
#[tokio::test]
async fn b890_a_refused_write_is_seen_and_leaves_the_file_as_it_stood() {
    for at in [At::Sync, At::Close, At::SyncAndClose] {
        let wt = checkout("b890-seen").await;
        let config = config_of(&wt);
        let state = GitState::snapshot(&wt).unwrap();
        // The engine rewrote the file during the run.
        let engines = "[core]\n\tfilemode = false\n";
        std::fs::write(&config, engines).unwrap();
        let storage = Refusing::files(at, 1);
        match state.restore_with(&wt, storage.as_ref()) {
            Err(WsError::StorageRefused { file, step, text }) => {
                assert_eq!(file, ".git/config");
                assert_eq!(step, at.step());
                assert_eq!(text, "No space left on device (os error 28)");
            }
            other => panic!("{at:?}: {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&config).unwrap(), engines);
        assert!(gone(&beside_config(&wt)), "{at:?}");
        // The same call made again, once the storage takes writes, puts the file back.
        state.restore_with(&wt, storage.as_ref()).unwrap();
        state.verify(&wt).unwrap();
        assert!(gone(&beside_config(&wt)), "{at:?}");
    }
}

/// The second file is written the same way: a refusal of `.git/info/attributes` is named with its file and waited out.
#[tokio::test]
async fn b890_the_attributes_file_is_a_checked_write_too() {
    let wt = checkout("b890-attributes").await;
    let attributes = wt.repo.join(".git/info/attributes");
    std::fs::write(&attributes, "*.bin -diff\n").unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    std::fs::write(&attributes, "*.py filter=x\n").unwrap();
    // The storage takes the write of the config and refuses the one of the attributes.
    let storage = Refusing::after(1, At::Close, 1);
    match state.restore_with(&wt, storage.as_ref()) {
        Err(WsError::StorageRefused { file, step, .. }) => {
            assert_eq!((file.as_str(), step), (".git/info/attributes", "close"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&attributes).unwrap(),
        "*.py filter=x\n"
    );
    let log = MemoryLog::new();
    let storage = Refusing::after(1, At::Sync, 2);
    // The session's clock, stopped: the bound counts the two pauses, not this machine's time for three writes.
    tokio::time::pause();
    let put = put_back(&state, &wt, storage.clone(), QUICK, &sink(&log), None).await;
    tokio::time::resume();
    put.unwrap();
    assert_eq!(
        std::fs::read_to_string(&attributes).unwrap(),
        "*.bin -diff\n"
    );
    assert_eq!(storage.refused(), 2);
    assert!(
        log.lines()[0].contains("refused the write of .git/info/attributes (sync:"),
        "{:?}",
        log.lines()
    );
    assert!(gone(
        &wt.repo.join(format!(".git/info/attributes{NEW_SUFFIX}"))
    ));
}

/// The bound and the pause are the measured ones, and when the bound runs out the end of the run says that the storage refused the
/// write and names the command of the newest copy. It never says that the repository changed or is not trusted.
#[tokio::test]
async fn b890_when_the_bound_runs_out_the_end_names_the_storage_and_the_newest_copy() {
    // Three times the longest refusal measured (87 to 89 s), at the pause one test write of the worker's own is waited for.
    assert_eq!(REPEAT_FOR, Duration::from_secs(270));
    assert_eq!(REPEAT_EVERY, TREE_ANSWER_LIMIT);
    assert_eq!(REPEAT_EVERY, Duration::from_secs(5));
    assert_eq!(
        Repeat::default(),
        Repeat {
            every: REPEAT_EVERY,
            for_at_most: REPEAT_FOR
        }
    );
    let wt = checkout("b890-bound").await;
    let config = config_of(&wt);
    let state = GitState::snapshot(&wt).unwrap();
    let engines = "[core]\n\tfilemode = false\n";
    std::fs::write(&config, engines).unwrap();
    let storage = Refusing::files(At::Sync, u32::MAX);
    let log = MemoryLog::new();
    // The session's clock, stopped: the pauses pass at once and are counted.
    tokio::time::pause();
    let began = tokio::time::Instant::now();
    let put = put_back(
        &state,
        &wt,
        storage.clone(),
        Repeat::default(),
        &sink(&log),
        None,
    )
    .await;
    let waited = began.elapsed();
    tokio::time::resume();
    assert!(
        waited >= Duration::from_secs(270) && waited < Duration::from_secs(271),
        "{waited:?}"
    );
    // A try at 0 s, 5 s and so on up to 270 s.
    assert_eq!(storage.refused(), 55);
    let why = put.clone().unwrap_err();
    assert_eq!(
        why,
        NotPutBack::Refused {
            file: ".git/config".to_string(),
            tries: 55,
            secs: 270,
            last: "sync: No space left on device (os error 28)".to_string(),
        }
    );
    // The file is as the engine left it, and nothing stands beside it.
    assert_eq!(std::fs::read_to_string(&config).unwrap(), engines);
    assert!(gone(&beside_config(&wt)));

    let c = classify::no_git(&why, RUN);
    assert_eq!(c.outcome, DirectOutcome::StoppedOther);
    assert_eq!(c.turn_outcome, TurnOutcome::Failed);
    assert!(c.fault_hint.is_none());
    let detail = c.detail.clone().unwrap();
    assert_eq!(
        label_after(&put),
        "Cline stopped: the harness ended the run (the session's storage refused the write of .git/config for 270 s; agentd did not commit or bundle the checkout)"
    );
    assert_eq!(
        detail,
        format!(
            "The harness ended the run: the session's storage refused the write of .git/config for 270 s; agentd did not commit or bundle the checkout. Each try was answered with no space left on device. What the engine did is in the checkout and in the newest copy, if one was saved: ham pull {RUN} --snapshot downloads it."
        )
    );
    // The longer of the two file names still fits the label whole.
    let longer = classify::settings_write_refused(".git/info/attributes", 270, RUN);
    assert_eq!(
        longer.outcome.label_for(longer.detail.as_deref()),
        "Cline stopped: the harness ended the run (the session's storage refused the write of .git/info/attributes for 270 s; agentd did not commit or bundle the checkout)"
    );
    let error_text = format!("restore git settings: {why}");
    assert_eq!(
        error_text,
        "restore git settings: the session's storage refused the write of .git/config for 270 s (55 tries; the last answer was sync: No space left on device (os error 28))"
    );
    let lines = log.lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(
        lines[0],
        "direct turn git settings: the session's storage refused the write of .git/config (sync: No space left on device (os error 28)); the write is repeated every 5 s for up to 270 s"
    );
    assert_eq!(
        lines[1],
        "direct turn git settings not put back: the session's storage refused the write of .git/config for 270 s (55 tries); agentd runs no git in the checkout"
    );
    // Never, for this cause: that the repository is not trusted or not the one the run started with.
    for text in [detail.as_str(), error_text.as_str(), &lines.join("\n")] {
        for never in [
            "not trusted",
            "not what the run started with",
            "not the one the run started with",
            "repository",
        ] {
            assert!(!text.contains(never), "{never:?} in {text}");
        }
    }
    assert!(harness_protocol::direct::banned_claim_word(&detail).is_none());
}

/// A refusal as long as the longest one measured (87 to 89 s) is waited out: the write that follows it works, and the run ends as
/// the engine's.
#[tokio::test]
async fn b890_a_refusal_as_long_as_the_longest_measured_is_waited_out() {
    let wt = checkout("b890-longest").await;
    let state = GitState::snapshot(&wt).unwrap();
    // Refused at 0 s, 5 s and so on up to 85 s; taken at 90 s.
    let storage = Refusing::files(At::SyncAndClose, 18);
    let log = MemoryLog::new();
    tokio::time::pause();
    let began = tokio::time::Instant::now();
    let put = put_back(
        &state,
        &wt,
        storage.clone(),
        Repeat::default(),
        &sink(&log),
        None,
    )
    .await;
    let waited = began.elapsed();
    tokio::time::resume();
    assert!(
        waited >= Duration::from_secs(90) && waited < Duration::from_secs(91),
        "{waited:?}"
    );
    assert_eq!(label_after(&put), "Cline reported done");
    assert_eq!((storage.refused(), storage.taken()), (18, 1));
    state.verify(&wt).unwrap();
    assert_eq!(
        log.lines().last().unwrap(),
        "direct turn git settings put back after 18 refused writes (90 s)"
    );
}

const BY: TurnAuthor<'static> = TurnAuthor {
    role: "direct",
    agent: "direct",
    task: None,
};

/// A real difference keeps its label: a file that does not read as it did after a write the storage took, and a file somebody
/// changed between the write and agentd's commit.
#[tokio::test]
async fn b890_a_real_change_of_the_git_settings_still_ends_with_its_label() {
    // The storage takes the write; the file then reads otherwise.
    let wt = checkout("b890-real-1").await;
    let state = GitState::snapshot(&wt).unwrap();
    let log = MemoryLog::new();
    let put = put_back(&state, &wt, Arc::new(Meddled), QUICK, &sink(&log), None).await;
    assert_eq!(
        put,
        Err(NotPutBack::Other(WsError::Untrusted(
            ".git/config is not what the run started with".to_string()
        )))
    );
    assert_eq!(label_after(&put), CHANGED);
    assert_eq!(
        log.lines(),
        vec![
            "direct turn repository not trusted: the repository is not the one the run started with: .git/config is not what the run started with"
        ]
    );

    // The settings were put back; somebody changes the file before agentd's commit.
    let wt = checkout("b890-real-2").await;
    let state = GitState::snapshot(&wt).unwrap();
    let log = MemoryLog::new();
    put_back(&state, &wt, Arc::new(Host), QUICK, &sink(&log), None)
        .await
        .unwrap();
    assert!(log.lines().is_empty(), "{:?}", log.lines());
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(config_of(&wt))
        .unwrap();
    f.write_all(b"[core]\n\tfilemode = false\n").unwrap();
    drop(f);
    std::fs::write(
        wt.repo.join("tests/test_calc.py"),
        "def test_z():\n    pass\n",
    )
    .unwrap();
    let e = commit_turn_verified(&wt, 1, &BY, &state).await.unwrap_err();
    assert_eq!(label_after(&Err(NotPutBack::Other(e))), CHANGED);
}

/// Only errno 28 is repeated. Any other error of a file call ends the try at once, with the words it had before.
#[tokio::test]
async fn b890_an_error_that_is_not_errno_28_is_not_repeated() {
    // EIO, EROFS, EACCES and EDQUOT at each step of the write.
    for (step, errno) in [
        ("sync", 5),
        ("close", 5),
        ("create", 30),
        ("write", 13),
        ("rename", 122),
    ] {
        let wt = checkout("b890-other").await;
        let state = GitState::snapshot(&wt).unwrap();
        let storage = Failing::at(step, errno);
        let log = MemoryLog::new();
        let put = put_back(&state, &wt, storage.clone(), QUICK, &sink(&log), None).await;
        let Err(NotPutBack::Other(WsError::Io(text))) = &put else {
            panic!("{step} {errno}: {put:?}")
        };
        assert!(
            text.starts_with(&format!("{step}: "))
                && text.ends_with(&format!("(os error {errno})")),
            "{text}"
        );
        assert_eq!(storage.created.load(Ordering::SeqCst), 1, "{step} {errno}");
        assert!(
            label_after(&put).starts_with(&format!(
                "Cline stopped: the harness ended the run (agentd did not commit or bundle the checkout: workspace I/O: {step}: "
            )),
            "{}",
            label_after(&put)
        );
        assert_eq!(log.lines().len(), 1, "{:?}", log.lines());
        assert!(log.lines()[0].starts_with("direct turn repository not trusted: workspace I/O: "));
        assert!(gone(&beside_config(&wt)));
    }
    // Errno 28 at the steps that are neither the sync nor the close is the same refusal, and is repeated.
    for step in ["create", "write", "rename"] {
        let wt = checkout("b890-other-28").await;
        let state = GitState::snapshot(&wt).unwrap();
        let storage = Failing::at(step, 28);
        let log = MemoryLog::new();
        let quick = Repeat {
            every: Duration::from_millis(5),
            for_at_most: Duration::from_millis(20),
        };
        // The session's clock, stopped: the pauses pass at once and are counted, and the time a try takes on this machine is
        // not part of them. The bound counts from the first try, so on a running clock a first try that outlasts it is the only
        // one (a write that reaches the rename has synced a real file, which took longer than 20 ms on a busy disk).
        tokio::time::pause();
        let began = tokio::time::Instant::now();
        let put = put_back(&state, &wt, storage.clone(), quick, &sink(&log), None).await;
        let waited = began.elapsed();
        tokio::time::resume();
        let Err(NotPutBack::Refused { tries, last, .. }) = &put else {
            panic!("{step}: {put:?}")
        };
        // A try at 0, 5, 10, 15 and 20 ms: every point of the grid up to the bound, and none after it. The timer wakes on whole
        // milliseconds of its own, so the pauses can end a millisecond late on that clock.
        assert_eq!(*tries, 5, "{step}: {tries} tries in {waited:?}");
        assert!(
            waited >= Duration::from_millis(20) && waited < Duration::from_millis(22),
            "{step}: {waited:?}"
        );
        assert_eq!(
            last,
            &format!("{step}: No space left on device (os error 28)")
        );
        assert_eq!(storage.created.load(Ordering::SeqCst), *tries);
    }
}

/// The link and ownership rules of the write are the ones it had: no link is followed, at the file's name or at the name beside it,
/// a directory in the file's place goes, and the file agentd leaves is a new plain file of its own user, mode 0644.
#[tokio::test]
async fn b890_no_link_is_followed_at_the_file_or_beside_it() {
    let outside = scratch("b890-links-outside").join("victim");
    let keep = "this file is not agentd's to write\n";

    // A link in the place of the file.
    let wt = checkout("b890-links-1").await;
    let config = config_of(&wt);
    let at_start = std::fs::read(&config).unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    std::fs::write(&outside, keep).unwrap();
    std::fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(&outside, &config).unwrap();
    state.restore(&wt).unwrap();
    assert!(
        std::fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(std::fs::read(&config).unwrap(), at_start);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), keep);

    // A link at the name beside the file, where the new bytes are written first.
    let beside = beside_config(&wt);
    std::os::unix::fs::symlink(&outside, &beside).unwrap();
    std::fs::write(&config, "[core]\n\tfilemode = false\n").unwrap();
    state.restore(&wt).unwrap();
    assert_eq!(std::fs::read(&config).unwrap(), at_start);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), keep);
    assert!(gone(&beside));

    // A directory at the name beside the file, and a directory in the place of the file.
    std::fs::create_dir(&beside).unwrap();
    std::fs::write(beside.join("x"), "x").unwrap();
    std::fs::remove_file(&config).unwrap();
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("y"), "y").unwrap();
    state.restore(&wt).unwrap();
    assert_eq!(std::fs::read(&config).unwrap(), at_start);
    assert!(gone(&beside));

    // A second name for a file that is somebody else's: the file agentd leaves is a new one, and the other one keeps its bytes.
    std::fs::remove_file(&config).unwrap();
    std::fs::hard_link(&outside, &config).unwrap();
    state.restore(&wt).unwrap();
    assert_eq!(std::fs::read(&config).unwrap(), at_start);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), keep);
    let (mine, theirs) = (
        std::fs::metadata(&config).unwrap(),
        std::fs::metadata(&outside).unwrap(),
    );
    assert_ne!(mine.ino(), theirs.ino());
    assert_eq!(mine.nlink(), 1);
    // Mode 0644 less what the process's mask takes: the owner reads and writes, nobody else writes.
    assert_eq!(mine.permissions().mode() & 0o622, 0o600);
    assert_eq!(mine.uid(), rustix::process::geteuid().as_raw());
}

/// The checked write itself: each step that fails is the answer and leaves the target as it stood; what the write or the sync met
/// comes before what the close met; a write the storage takes leaves the bytes and nothing beside them.
#[test]
fn b890_the_checked_write_names_the_step_and_keeps_the_target() {
    let d = scratch("b890-replace");
    let (target, beside) = (d.join("file"), d.join("file.new"));
    std::fs::write(&target, "earlier").unwrap();
    for step in ["create", "write", "sync", "close", "rename"] {
        let failed = checked_write::replace(
            Failing::at(step, 28).as_ref(),
            &beside,
            &target,
            0o644,
            b"new",
        )
        .unwrap_err();
        assert_eq!(failed.0, step);
        assert_eq!(failed.1.raw_os_error(), Some(28));
        assert!(checked_write::refused(&failed));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "earlier");
        assert!(gone(&beside), "{step}");
    }
    // Another errno is not a refusal.
    let failed = checked_write::replace(
        Failing::at("sync", 5).as_ref(),
        &beside,
        &target,
        0o644,
        b"new",
    )
    .unwrap_err();
    assert!(!checked_write::refused(&failed));
    // Both the sync and the close refused: the sync is the answer.
    let failed = checked_write::replace(
        Refusing::files(At::SyncAndClose, 1).as_ref(),
        &beside,
        &target,
        0o644,
        b"new",
    )
    .unwrap_err();
    assert_eq!(failed.0, "sync");
    // A name that exists beside the target is an error, not a write into it.
    std::fs::write(&beside, "somebody's").unwrap();
    let failed = checked_write::replace(&Host, &beside, &target, 0o644, b"new").unwrap_err();
    assert_eq!(failed.0, "create");
    assert_eq!(failed.1.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "earlier");
    std::fs::remove_file(&beside).unwrap();
    // The host's calls: the bytes are in place, with the mode, and nothing stands beside them.
    checked_write::replace(&Host, &beside, &target, 0o644, b"new").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o622,
        0o600
    );
    assert!(gone(&beside));
}

/// The sweep: the marker of the copies checked its sync and not its close. It is the same checked write now: a refusal at the close
/// is an error with its step, the earlier marker stands whole, and the watch of the session's storage reads it as a refusal.
#[test]
fn b890_the_marker_of_the_copies_checks_its_close_and_keeps_the_earlier_marker() {
    let dir = scratch("b890-marker").join("run");
    std::fs::create_dir(&dir).unwrap();
    let (file, tmp) = (dir.join(marker::MARKER_FILE), dir.join(".copies.json.tmp"));
    marker::write_with(&Host, &dir, b"{\"count\":1}").unwrap();
    for at in [At::Close, At::Sync, At::SyncAndClose] {
        let (step, e) = marker::write_with(Refusing::files(at, 1).as_ref(), &dir, b"{\"count\":2}")
            .unwrap_err();
        assert_eq!(step, at.step());
        assert_eq!(e.raw_os_error(), Some(28));
        assert_eq!(std::fs::read(&file).unwrap(), b"{\"count\":1}");
        assert!(gone(&tmp), "{at:?}");
        let seen = WriteError {
            step,
            errno: e.raw_os_error(),
            text: e.to_string(),
            test_switch: false,
        };
        assert!(seen.refused(), "{at:?}");
    }
    // Errno 28 at the close is a refusal; another errno there is not.
    let close = |errno: i32| WriteError {
        step: "close",
        errno: Some(errno),
        text: String::new(),
        test_switch: false,
    };
    assert!(close(28).refused());
    assert!(!close(5).refused());
    marker::write_with(&Host, &dir, b"{\"count\":2}").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"{\"count\":2}");
    assert!(gone(&tmp));
}

/// A size the test gives.
#[derive(Debug)]
struct Sized(u64);

impl DiskProbe for Sized {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async { Reading::default() })
    }

    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async move { (Some(self.0), false) })
    }
}

fn watch(log: &Arc<MemoryLog>) -> (StorageWatch, Arc<EventRecorder>) {
    let rec = EventRecorder::new(RUN, "direct");
    let watch = StorageWatch::new(WatchSetup {
        writes: Arc::new(HostWrites),
        probe: Arc::new(Sized(165)),
        rec: rec.clone(),
        log: log.clone(),
        run_id: RUN.to_string(),
        session: 1,
        run_dir: scratch("b890-watch-run"),
        limits: Limits::default(),
        test_refusal: false,
    });
    (watch, rec)
}

fn storage_notices(rec: &EventRecorder) -> Vec<Value> {
    rec.drain()
        .into_iter()
        .filter_map(|e| match e.payload {
            Payload::Inline { value }
                if e.event_type == EventType::TurnProgress
                    && value["kind"] == "notify"
                    && value["reason"] == "storage_refused" =>
            {
                Some(value)
            }
            _ => None,
        })
        .collect()
}

/// The write of the git settings is a write the worker makes anyway, like the marker of the copies: a refusal it meets brings the
/// one notice of its stretch, and the write that works closes the stretch. A write the storage takes at once brings nothing.
#[tokio::test]
async fn b890_the_watch_of_the_storage_is_told_what_the_write_of_the_git_settings_met() {
    let wt = checkout("b890-watch").await;
    let state = GitState::snapshot(&wt).unwrap();
    let log = MemoryLog::new();
    let (watch, rec) = watch(&log);

    // Taken at once: no notice and no line.
    put_back(
        &state,
        &wt,
        Arc::new(Host),
        QUICK,
        &sink(&log),
        Some(&watch),
    )
    .await
    .unwrap();
    assert!(storage_notices(&rec).is_empty());
    assert!(log.lines().is_empty(), "{:?}", log.lines());

    // Refused three times, then taken: one notice, and the stretch is closed.
    let storage = Refusing::files(At::Sync, 3);
    // The session's clock, stopped: the bound and the "0 s" of the last line count the three pauses, not this machine's time.
    tokio::time::pause();
    let put = put_back(&state, &wt, storage, QUICK, &sink(&log), Some(&watch)).await;
    tokio::time::resume();
    put.unwrap();
    let notices = storage_notices(&rec);
    assert_eq!(notices.len(), 1, "{notices:?}");
    let n = &notices[0];
    assert_eq!(n["storage"]["seen"], "end");
    assert_eq!(n["storage"]["stretch"], 1);
    assert_eq!(n["storage"]["usedMb"], 165);
    assert!(n["storage"].get("testSwitch").is_none());
    let detail = n["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(
            "The session's storage refused a write while its files add up to 165 MiB of 1024 MiB (no space left on device)."
        ),
        "{detail}"
    );
    assert!(
        detail.ends_with(&format!(
            "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
        )),
        "{detail}"
    );
    let lines = log.lines();
    assert!(
        lines.iter().any(|l| l.starts_with(
            "direct turn storage refused a write (seen at the git settings at the session's end): no space left on device; stretch 1"
        )),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(
            |l| l.starts_with("direct turn storage takes writes again after ")
                && l.ends_with("stretch 1 closed")
        ),
        "{lines:?}"
    );
    assert_eq!(
        lines.last().unwrap(),
        "direct turn git settings put back after 3 refused writes (0 s)"
    );

    // A later refusal of the same session is the short notice of its second stretch.
    let storage = Refusing::files(At::Close, 1);
    tokio::time::pause();
    let put = put_back(&state, &wt, storage, QUICK, &sink(&log), Some(&watch)).await;
    tokio::time::resume();
    put.unwrap();
    let notices = storage_notices(&rec);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["storage"]["stretch"], 2);
    assert_eq!(notices[0]["storage"]["seen"], "end");
}

/// Root invariant 9: the repeat can take minutes and a session can end in it. What the caller gives is done once, after the first
/// refused write and before the first pause (the end sequence saves the copy of the end there), and never when no write is refused.
#[tokio::test]
async fn b890_what_must_not_wait_is_done_at_the_first_refusal_and_before_the_first_pause() {
    let wt = checkout("b890-first").await;
    let state = GitState::snapshot(&wt).unwrap();
    for (refusals, calls_expected) in [(0u32, 0u32), (1, 1), (4, 1), (u32::MAX, 1)] {
        let storage = Refusing::files(At::Sync, refusals);
        let log = MemoryLog::new();
        let called = AtomicU32::new(0);
        // What the storage had refused, and the worker's log, at the moment of the call.
        let seen_at_call = std::sync::Mutex::new(Vec::new());
        tokio::time::pause();
        let began = tokio::time::Instant::now();
        let _ = git_settings::put_back(
            &state,
            &wt,
            storage.clone(),
            Repeat::default(),
            &sink(&log),
            None,
            async || {
                called.fetch_add(1, Ordering::SeqCst);
                seen_at_call.lock().unwrap().push((
                    storage.refused(),
                    began.elapsed(),
                    log.lines().len(),
                ));
            },
        )
        .await;
        tokio::time::resume();
        assert_eq!(called.load(Ordering::SeqCst), calls_expected, "{refusals}");
        if calls_expected == 1 {
            // One write refused, no pause yet, and the log already says what the worker met.
            assert_eq!(
                *seen_at_call.lock().unwrap(),
                vec![(1, Duration::ZERO, 1)],
                "{refusals}"
            );
        }
    }
    // An error that is not errno 28 is not a refusal: nothing is called.
    let called = AtomicU32::new(0);
    let log = MemoryLog::new();
    let put = git_settings::put_back(
        &state,
        &wt,
        Failing::at("sync", 5),
        QUICK,
        &sink(&log),
        None,
        async || {
            called.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert!(matches!(put, Err(NotPutBack::Other(_))), "{put:?}");
    assert_eq!(called.load(Ordering::SeqCst), 0);
}

/// Root invariant 9 comes first: at the first refused write the copy of the end is saved before the watch of the storage is told.
/// On the first notice of a session the watch measures the files of the storage, which takes time; the copy does not wait for it.
#[tokio::test]
async fn b890_the_copy_of_the_end_is_saved_before_the_watch_of_the_storage_is_told() {
    let wt = checkout("b890-copy-first").await;
    let state = GitState::snapshot(&wt).unwrap();
    let log = MemoryLog::new();
    let (watch, rec) = watch(&log);
    let storage = Refusing::files(At::Sync, 2);
    // What the watch had said when the caller's work was called: its notices, and its lines in the log.
    let said_at_call = std::sync::Mutex::new(Vec::new());
    // The session's clock, stopped: the bound counts the two pauses, not this machine's time.
    tokio::time::pause();
    let put = git_settings::put_back(
        &state,
        &wt,
        storage,
        QUICK,
        &sink(&log),
        Some(&watch),
        async || {
            let lines = log
                .lines()
                .iter()
                .filter(|l| l.starts_with("direct turn storage refused a write"))
                .count();
            said_at_call
                .lock()
                .unwrap()
                .push((storage_notices(&rec).len(), lines));
        },
    )
    .await;
    tokio::time::resume();
    put.unwrap();
    assert_eq!(*said_at_call.lock().unwrap(), vec![(0, 0)]);
    // The watch is told right after: the one notice of the stretch is there, and its line.
    let notices = storage_notices(&rec);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["storage"]["seen"], "end");
    let lines = log.lines();
    assert!(
        lines[0].starts_with(
            "direct turn git settings: the session's storage refused the write of .git/config"
        ),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with(
            "direct turn storage refused a write (seen at the git settings at the session's end)"
        ),
        "{lines:?}"
    );
}

/// The bound is the time the storage is given to take the write, so what is done at the first refusal does not use it up. After a
/// copy of the end that took a minute the tries still stand 5 s apart for 270 s, none follows another without a pause, and the
/// end's "for 270 s" is true.
#[tokio::test]
async fn b890_the_time_of_the_copy_before_the_wait_does_not_shorten_the_bound() {
    let wt = checkout("b890-copy-time").await;
    let state = GitState::snapshot(&wt).unwrap();
    let storage = Refusing::files(At::Sync, u32::MAX);
    let log = MemoryLog::new();
    tokio::time::pause();
    let began = tokio::time::Instant::now();
    let put = git_settings::put_back(
        &state,
        &wt,
        storage.clone(),
        Repeat::default(),
        &sink(&log),
        None,
        async || {
            tokio::time::sleep(Duration::from_secs(60)).await;
        },
    )
    .await;
    let waited = began.elapsed();
    tokio::time::resume();
    // A try at 0 s, the copy until 60 s, then a try every 5 s from 65 s to 330 s: 55 tries, as without the copy.
    assert!(
        waited >= Duration::from_secs(330) && waited < Duration::from_secs(331),
        "{waited:?}"
    );
    assert_eq!(storage.refused(), 55);
    assert_eq!(
        put,
        Err(NotPutBack::Refused {
            file: ".git/config".to_string(),
            tries: 55,
            secs: 270,
            last: "sync: No space left on device (os error 28)".to_string(),
        })
    );
}

/// One rule says which answer is a refusal of the data: errno 28 at the create, the write, the sync, the close or the rename of the
/// worker's own file. The checked write and the storage watch's test write both read it, so the two cannot come apart.
#[test]
fn b890_one_rule_says_which_answer_of_the_storage_is_a_refusal() {
    assert_eq!(
        checked_write::DATA_STEPS,
        ["create", "write", "sync", "close", "rename"]
    );
    for step in ["create", "write", "sync", "close", "rename"] {
        assert!(checked_write::refused_at(step, Some(28)), "{step}");
        assert!(checked_write::refused(&(step, no_space())), "{step}");
        // Any other errno at the same step is an error, not a refusal.
        for errno in [None, Some(5), Some(13), Some(122)] {
            assert!(!checked_write::refused_at(step, errno), "{step} {errno:?}");
        }
        assert!(
            !checked_write::refused(&(step, std::io::Error::from_raw_os_error(5))),
            "{step}"
        );
    }
    // Errno 28 at a step that carries no data of the file (the watch's folder check, a leftover it removes) is no refusal.
    for step in ["folder", "leftover", "remove", "lock", "open"] {
        assert!(!checked_write::refused_at(step, Some(28)), "{step}");
    }
}

/// A storage that refuses every write at the sync, takes `slow` to begin the second one, and keeps when each write began.
#[derive(Debug)]
struct SlowSecond {
    slow: Duration,
    began: std::sync::Mutex<Vec<std::time::Instant>>,
}

impl FileCalls for SlowSecond {
    fn create(&self, path: &Path, mode: u32) -> std::io::Result<File> {
        let nth = {
            let mut began = self.began.lock().unwrap();
            began.push(std::time::Instant::now());
            began.len()
        };
        if nth == 2 {
            std::thread::sleep(self.slow);
        }
        Host.create(path, mode)
    }

    fn write(&self, _file: &mut File, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }

    fn sync(&self, _file: &File) -> std::io::Result<()> {
        Err(no_space())
    }
}

/// The pace holds after a write that took long. One try follows it at once; the points of the grid it passed are left out, and
/// the tries after it stand a pause apart again. They are not made up for one after the other, as a burst against a storage that
/// refuses writes.
#[tokio::test]
async fn b890_a_write_that_took_long_is_not_followed_by_a_burst_of_tries() {
    let wt = checkout("b890-no-burst").await;
    let state = GitState::snapshot(&wt).unwrap();
    let every = Duration::from_millis(500);
    let storage = Arc::new(SlowSecond {
        slow: Duration::from_millis(2200),
        began: std::sync::Mutex::new(Vec::new()),
    });
    let log = MemoryLog::new();
    let put = put_back(
        &state,
        &wt,
        storage.clone(),
        Repeat {
            every,
            for_at_most: Duration::from_secs(4),
        },
        &sink(&log),
        None,
    )
    .await;
    assert!(matches!(put, Err(NotPutBack::Refused { .. })), "{put:?}");
    let began = storage.began.lock().unwrap().clone();
    let gaps: Vec<Duration> = began.windows(2).map(|w| w[1] - w[0]).collect();
    // Tries at 0 s and 0.5 s. The second takes 2.2 s and passes the points at 1, 1.5, 2 and 2.5 s. One try follows it at once, at
    // 2.7 s, then the grid again: 3, 3.5 and 4 s. Six tries; making up for the four passed points gives nine. A loaded host can be
    // late for a point, which leaves a try out and never adds one.
    assert!(
        (4..=6).contains(&began.len()),
        "{} tries, {gaps:?}",
        began.len()
    );
    // No try begins less than half a pause after the one before it, but for the first point of the grid after the long write.
    let close = gaps.iter().filter(|g| **g < every / 2).count();
    assert!(close <= 1, "{gaps:?}");
}
