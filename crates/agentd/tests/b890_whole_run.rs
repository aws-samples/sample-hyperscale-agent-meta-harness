//! The write of the git settings refused once, in a whole run: the real driver in process, the fake engine, a fake AWS. No AWS
//! call is made.
//!
//! A real refusal of the session's storage cannot be made on demand. The switch of the live test row answers the first write
//! of the git settings at the session's end as the storage was seen to answer: no data arrives and the sync says "No space left on
//! device". Before such a write left `.git/config` empty and the run ended "... .git/config is not what the run started with".
//! Now the end sequence sees the refusal, writes again after its pause, commits, bundles and delivers, and the run stays the engine's.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::checked_write::FileCalls;
use agentd::direct::git_settings::Repeat;
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const SWITCH: &str = "direct turn git settings: the stage's test switch answered this write with no space left on device; the storage refused no write";
const REFUSED: &str = "direct turn git settings: the session's storage refused the write of .git/config (sync: No space left on device (os error 28)); the write is repeated every 5 s for up to 270 s";
const COPIED: &str =
    "direct turn git settings: the end state of the work was copied before the wait";
/// The seconds follow: the pause of 5 s, and what the write that worked took.
const BACK: &str = "direct turn git settings put back after 1 refused write (";

fn settings_lines(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct turn git settings"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn b890_a_run_whose_write_of_the_git_settings_is_refused_once_stays_the_engines_and_is_delivered()
 {
    // The same run without the fault: what the engine's run reads like.
    let plain = Direct::start(Opts {
        scenario: "writes_files",
        ..Opts::default()
    })
    .await;
    let (_, plain_resp) = plain.invoke().await;
    let plain_summary = plain.summary();
    let plain_lines = settings_lines(&plain);

    let d = Direct::start(Opts {
        scenario: "writes_files",
        edit_cfg: Some(Box::new(|c| {
            c.direct.storage_test_refusal = true;
        })),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    let lines = settings_lines(&d);
    assert!(
        lines.iter().any(|l| l == SWITCH),
        "TRIGGER PRESENT is not shown: the first write of the git settings was not answered with errno 28; log: {:?}",
        d.log.lines()
    );
    println!(
        "B890 TRIGGER PRESENT: the first write of the git settings was answered with errno 28 at its sync"
    );
    // What the worker met, that it wrote again after its pause, and nothing else about the settings.
    // The copy of the end is stored before the wait (root invariant 9): a session that ended in the wait would have lost nothing.
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[..3], [SWITCH, REFUSED, COPIED]);
    assert!(lines[3].starts_with(BACK), "{lines:?}");
    let secs: u64 = lines[3][BACK.len()..]
        .trim_end_matches(" s)")
        .parse()
        .unwrap();
    assert!((5..60).contains(&secs), "{lines:?}");
    assert!(
        !d.log
            .lines()
            .iter()
            .any(|l| l.contains("repository not trusted")),
        "{:?}",
        d.log.lines()
    );

    // The run stays the engine's: its outcome, its label and its detail are those of the same run without the fault.
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(resp.outcome, plain_resp.outcome);
    let s = d.summary();
    assert_eq!(s.outcome, plain_summary.outcome);
    assert_eq!(s.label, plain_summary.label);
    assert_eq!(s.detail, plain_summary.detail);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(d.prompts_received(), plain.prompts_received());

    // The commit and the bundle were made and stored, as in the run without the fault.
    assert!(s.git.head_sha.is_some(), "{:?}", s.git);
    assert_eq!(
        s.git.committed_by_harness,
        plain_summary.git.committed_by_harness
    );
    assert_eq!(s.git.files_changed, plain_summary.git.files_changed);
    assert!(s.git.bundle_uri.is_some(), "{:?}", s.git);
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());

    // The switch's answer is no refusal of the storage: no storage notice says there was one.
    let notices: Vec<_> = d
        .events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "notify" && v["reason"] == "storage_refused")
        .collect();
    assert!(notices.is_empty(), "{notices:?}");
    assert!(
        !d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn storage refused a write")),
        "{:?}",
        d.log.lines()
    );

    // The run without the fault says nothing about the settings: its log is as it was.
    assert!(plain_lines.is_empty(), "{plain_lines:?}");
}

/// With both disk checks off a deployment has no watch of the storage, and so no switch: the write is the host's.
#[tokio::test(flavor = "multi_thread")]
async fn b890_without_a_watch_the_switch_answers_no_write() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        edit_cfg: Some(Box::new(|c| {
            c.direct.disk_low_mb = 0;
            c.direct.workspace_limit_mb = 0;
            c.direct.storage_test_refusal = true;
        })),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let lines = settings_lines(&d);
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(d.summary().label, "Cline reported done");
}

/// The session's storage as it was seen while it refuses data, for every file it is asked to write: the write call works, no byte
/// arrives and the sync answers errno 28.
#[derive(Debug, Default)]
struct RefusesEvery {
    files: AtomicU32,
}

impl FileCalls for RefusesEvery {
    fn write(&self, _file: &mut std::fs::File, _bytes: &[u8]) -> std::io::Result<()> {
        Ok(())
    }

    fn sync(&self, _file: &std::fs::File) -> std::io::Result<()> {
        self.files.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::from_raw_os_error(28))
    }
}

/// A pause of one second and a bound of two, so that a test waits two seconds where a session waits 270: tries at 0 s, 1 s and 2 s.
const SHORT: Repeat = Repeat {
    every: Duration::from_secs(1),
    for_at_most: Duration::from_secs(2),
};
const REFUSED_SHORT: &str = "direct turn git settings: the session's storage refused the write of .git/config (sync: No space left on device (os error 28)); the write is repeated every 1 s for up to 2 s";
const NOT_BACK_SHORT: &str = "direct turn git settings not put back: the session's storage refused the write of .git/config for 2 s (3 tries); agentd runs no git in the checkout";
const ERROR_SHORT: &str = "restore git settings: the session's storage refused the write of .git/config for 2 s (3 tries; the last answer was sync: No space left on device (os error 28))";
/// What no text of such an end may say: nothing changed the repository.
const NEVER: [&str; 3] = [
    "not trusted",
    "not what the run started with",
    "not the one the run started with",
];

/// The copies of the session's end the bucket holds: every version of `snapshot.json` whose record says `final`.
fn copies_of_the_end(d: &Direct) -> usize {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .filter(|(_, record)| {
            serde_json::from_slice::<serde_json::Value>(record)
                .unwrap()
                .get("final")
                .is_some()
        })
        .count()
}

/// A whole run of `scenario` in which the storage refuses every write of the git settings until the bound runs out. The file calls
/// and the short repeat are the test's own (`App::settings_write`); everything else is the end sequence as a session runs it.
async fn refused_to_the_bound(scenario: &'static str) -> (Direct, InvocationResponse) {
    let d = Direct::start(Opts {
        scenario,
        ..Opts::default()
    })
    .await;
    let storage = Arc::new(RefusesEvery::default());
    assert!(d.app.settings_write.set((storage.clone(), SHORT)).is_ok());
    let (_, resp) = d.invoke().await;
    assert_eq!(
        storage.files.load(Ordering::SeqCst),
        3,
        "TRIGGER PRESENT is not shown: the storage was not asked three times for the git settings; log: {:?}",
        d.log.lines()
    );
    println!(
        "B890 TRIGGER PRESENT: every write of the git settings was answered with errno 28 at its sync until the bound ran out"
    );
    let lines = settings_lines(&d);
    assert_eq!(lines, [REFUSED_SHORT, COPIED, NOT_BACK_SHORT], "{lines:?}");
    let all = d.log.lines().join("\n");
    for never in NEVER {
        assert!(!all.contains(never), "{never:?} in the log: {all}");
    }
    (d, resp)
}

/// The bound runs out in a whole run (done-when 4): the storage refuses the write of `.git/config` at every try. The run the engine
/// reported done ends with the storage's words and the command of the newest copy, agentd runs no git in the checkout, and the
/// copy of the end is the one stored before the wait: nothing has written the tree since, so it is not stored a second time.
#[tokio::test(flavor = "multi_thread")]
async fn b890_a_run_whose_write_of_the_git_settings_is_refused_to_the_bound_ends_with_the_storages_words_and_one_copy_of_the_end()
 {
    let (d, resp) = refused_to_the_bound("writes_files").await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    let s = d.summary();
    assert_eq!(
        s.label,
        "Cline stopped: the harness ended the run (the session's storage refused the write of .git/config for 2 s; agentd did not commit or bundle the checkout)"
    );
    let detail = format!(
        "The harness ended the run: the session's storage refused the write of .git/config for 2 s; agentd did not commit or bundle the checkout. Each try was answered with no space left on device. What the engine did is in the checkout and in the newest copy, if one was saved: ham pull {RUN} --snapshot downloads it."
    );
    assert_eq!(s.detail.as_deref(), Some(detail.as_str()));
    // The turn's error: the detail, and what the end sequence could not do. The command of the newest copy is said once.
    let error = resp.error.clone().unwrap();
    assert!(error.starts_with(&detail), "{error}");
    assert!(error.contains(ERROR_SHORT), "{error}");
    assert_eq!(error.matches("ham pull").count(), 1, "{error}");
    assert_eq!(s.error.as_deref(), Some(error.as_str()));
    for never in NEVER {
        assert!(!error.contains(never), "{never:?} in {error}");
    }

    // No git of agentd's ran in the checkout: no commit and no bundle.
    assert!(s.git.head_sha.is_none(), "{:?}", s.git);
    assert!(!s.git.committed_by_harness, "{:?}", s.git);
    assert!(s.git.bundle_uri.is_none(), "{:?}", s.git);
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_none());

    // Root invariant 9: the end state of the work has its copy, stored before the wait, and it is the one copy of the end.
    assert_eq!(copies_of_the_end(&d), 1);
}

/// A run the engine did not report done keeps the engine's label when the bound runs out: the harness claims nothing it replaces.
/// The turn's error says that the storage refused the write, and names the command of the newest copy, once.
#[tokio::test(flavor = "multi_thread")]
async fn b890_a_run_the_engine_did_not_report_done_keeps_its_label_and_its_error_names_the_storage_and_the_newest_copy()
 {
    // The same run without the fault: what the engine's stop reads like.
    let plain = Direct::start(Opts {
        scenario: "max_tokens",
        ..Opts::default()
    })
    .await;
    let (_, plain_resp) = plain.invoke().await;
    let plain_summary = plain.summary();
    assert_ne!(plain_summary.label, "Cline reported done");

    let (d, resp) = refused_to_the_bound("max_tokens").await;
    let s = d.summary();
    assert_eq!(s.label, plain_summary.label);
    assert_eq!(s.outcome, plain_summary.outcome);
    assert_eq!(s.detail, plain_summary.detail);
    assert_eq!(resp.outcome, plain_resp.outcome);
    let error = resp.error.clone().unwrap();
    assert!(
        error.contains(&format!(
            "{ERROR_SHORT}; ham pull {RUN} --snapshot downloads the newest copy, if one was saved"
        )),
        "{error}"
    );
    assert_eq!(error.matches("ham pull").count(), 1, "{error}");
    assert_eq!(s.error.as_deref(), Some(error.as_str()));
    for never in NEVER {
        assert!(!error.contains(never), "{never:?} in {error}");
    }
    assert!(s.git.head_sha.is_none(), "{:?}", s.git);
    assert!(s.git.bundle_uri.is_none(), "{:?}", s.git);
    assert_eq!(copies_of_the_end(&d), 1);
}
