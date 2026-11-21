//! No tool call waits for a copy of the work, and neither does a request for the session's credentials.
//!
//! The maintainers' sentence is "ensure our save every 15sec thing doesn't impact tool calling or long running commands". By the code a
//! tool call's end only wakes the copy task, and the copy's lock is taken by the copy task and by the copy of the end alone. This test
//! holds that for every later build with a whole turn: a look of the copy task is held, first in its git and then in its upload, and
//! while it is held the engine ends tool calls and the worker's credential socket is asked. All of them are answered before the look
//! goes on.
//!
//! The test asserts the order of these events and three durations it can fail on. While a look is held, two tool calls end within
//! [`ANSWERED_WITHIN`] of the moment the engine may say them, and the credentials are answered within the same time: a tool call or a
//! credential request that waited for the copy would wait for as long as the test holds the look, which is until they are answered.
//! And the turn ends within [`TURN_ENDS_WITHIN`] of the moment the engine may say the rest of its reply: a copy that holds up the end
//! of the session fails there. The times of the steps are printed, so a run shows where its time went.
//!
//! What the test does not hold: how much a copy slows a long command. Release 5.24, whose copies cost a long command more than the
//! bound allows, passes this test in the same time as the code after it (6.3 s against 4.7 s on a quiet host), because no
//! tool call of that release waited for a copy either. That cost is counted by `b860_a_look_stays_inside_its_budget`
//! (`tests/b860_look_cost.rs`) and measured on the bench and in the live proof of a release.
#![allow(clippy::unwrap_used)]

mod b860_support;
mod common;
mod direct_support;

use b860_support::{MIB, big_file};
use common::scratch;
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::TurnOutcome;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The file the test puts into the work tree: larger than what both ends of a connection buffer, so the bundle that holds it cannot have
/// left agentd whole when the bucket stops reading.
const BIG: u64 = 24 * MIB;
/// The bucket reads this much of the bundle and then nothing more.
const BUCKET_READS: usize = 1024 * 1024;
/// While a look is held, the tool calls the engine may say end within this long and a request for the credentials is answered within
/// this long. On a quiet host both take under a tenth of a second; a request path that waited for the copy would never get there,
/// because the test opens no gate before both are answered.
const ANSWERED_WITHIN: Duration = Duration::from_secs(10);
/// The turn ends within this long of the moment the engine may say the rest of its reply. On a quiet host it takes under a second;
/// the copy of the end has a budget of 120 s, so a turn whose end waits that budget out, or a large part of it, fails here.
const TURN_ENDS_WITHIN: Duration = Duration::from_secs(45);

/// The steps of the test and when each was reached, for the test's output and the words of a failed assertion.
struct Steps {
    began: Instant,
    last: Instant,
    lines: Vec<String>,
}

impl Steps {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            began: now,
            last: now,
            lines: Vec::new(),
        }
    }

    /// Notes that `what` has happened; the answer is how long it took since the step before it.
    fn reached(&mut self, what: &str) -> Duration {
        let now = Instant::now();
        let took = now.duration_since(self.last);
        self.lines.push(format!(
            "{:>8.2} s  (+{:.2} s)  {what}",
            now.duration_since(self.began).as_secs_f64(),
            took.as_secs_f64()
        ));
        self.last = now;
        took
    }

    fn report(&self) -> String {
        self.lines.join("\n")
    }
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
async fn wait_for(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The two gates of the test and the steps of the engine. Whatever way the test ends, both gates are opened and the engine may say
/// everything, so no run of the test leaves a process that waits.
struct Gates {
    /// While this file is there and `open` is not, `git bundle create` in the private repository of the copies waits.
    armed: PathBuf,
    /// Written by the stand-in for git when a `git bundle create` waits.
    held: PathBuf,
    open: PathBuf,
    /// The file of `FAKE_ACP_STEP_FILE`: how many messages of its reply the engine may have written.
    steps: PathBuf,
}

impl Gates {
    fn new(dir: &Path) -> Self {
        let gates = Self {
            armed: dir.join("gate-armed"),
            held: dir.join("gate-held"),
            open: dir.join("gate-open"),
            steps: dir.join("engine-steps"),
        };
        gates.step(0);
        gates
    }

    /// The engine may write the first `n` messages of its reply.
    fn step(&self, n: u64) {
        std::fs::write(&self.steps, format!("{n}\n")).unwrap();
    }
}

impl Drop for Gates {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.open, "open\n");
        let _ = std::fs::write(&self.steps, "1000000\n");
    }
}

/// A stand-in for git, in a folder that leads the search path of `agentd serve`. While the gate is armed and not open, a
/// `bundle create` in the private repository of the copies writes the file `held` and waits until the gate is open. Every call is the
/// real git in the end.
fn git_with_a_gate(dir: &Path, gates: &Gates) -> PathBuf {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("gate-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
bundle=0
for a in "$@"; do if [ "$a" = bundle ]; then bundle=1; fi; done
if [ "$bundle" = 1 ] && [ -e '{armed}' ] && [ ! -e '{open}' ]; then
  case "$GIT_DIR" in
    */snapshot.git)
      : > '{held}'
      while [ ! -e '{open}' ]; do sleep 0.05; done ;;
  esac
fi
exec '{real}' "$@"
"#,
        armed = gates.armed.display(),
        open = gates.open.display(),
        held = gates.held.display(),
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// How many tool calls agentd has seen end: the `turn.progress` events of a tool call's update that say `completed`, as the bucket's
/// neighbour, the fake control plane, got them from agentd's event flusher.
fn calls_ended(d: &Direct) -> usize {
    d.events_of(EventType::TurnProgress)
        .iter()
        .filter(|v| {
            v.get("toolCallId").is_some_and(|id| !id.is_null()) && v["status"] == "completed"
        })
        .count()
}

/// The answer of the worker's credential socket, asked as an engine's `credential_process` asks it; a socket that does not answer within
/// [`ANSWERED_WITHIN`] fails the test.
async fn credentials(d: &Direct) -> String {
    let socket = d.app.cfg.runtime_dir.join("creds.sock");
    tokio::time::timeout(
        ANSWERED_WITHIN,
        agentd::creds_socket::fetch_credentials(&socket),
    )
    .await
    .expect("TRIGGER PRESENT: the credential socket did not answer within its limit while a look was held")
    .expect("the credential socket gave the credentials")
}

/// The sizes of the copies the bucket holds and what the worker has logged so far, for the words of a failed assertion.
fn log(d: &Direct) -> String {
    let sizes: Vec<usize> = d
        .aws
        .versions_of(&keys::snapshot_bundle_key(RUN))
        .iter()
        .map(|(_, b)| b.len())
        .collect();
    format!(
        "the bundles stored have {sizes:?} bytes\n{}",
        d.process
            .as_ref()
            .map(direct_support::Process::log_text)
            .unwrap_or_default()
    )
}

/// How many copies of the run's work the bucket has stored.
fn copies_stored(d: &Direct) -> usize {
    d.aws.versions_of(&keys::snapshot_bundle_key(RUN)).len()
}

#[tokio::test(flavor = "multi_thread")]
async fn b860_tool_calls_and_credentials_do_not_wait_for_a_copy() {
    let scratch = scratch("b860-no-wait");
    let mut steps = Steps::new();
    let gates = Gates::new(&scratch);
    let bin = git_with_a_gate(&scratch, &gates);
    let d = Direct::start_process(Opts {
        scenario: "four_edits",
        engine_env: vec![("FAKE_ACP_STEP_FILE", gates.steps.display().to_string())],
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "300".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(60), || !d.prompts_received().is_empty()).await,
        "the engine got its prompt"
    );
    steps.reached("the engine has its prompt");
    assert_eq!(
        calls_ended(&d),
        0,
        "TRIGGER PRESENT is not shown: the engine said something before its first step"
    );

    // The first look of the session stores a copy at once: agentd moved the hook folder of the repository aside, so the tree differs
    // from the base. The gates are set once that copy is stored, so the next copy is the one with the large file.
    assert!(
        wait_for(Duration::from_secs(60), || copies_stored(&d) == 1).await,
        "the copy of the session's start is stored; log:\n{}",
        log(&d)
    );
    steps.reached("the copy of the session's start is stored");

    // Gate 1: the look that finds the new file is held in `git bundle create`.
    d.aws.hold_put_body("snapshot.bundle", BUCKET_READS);
    std::fs::write(&gates.armed, "armed\n").unwrap();
    // The file comes into the tree whole, by a rename: a look that read a part of it would make a small bundle, and the bucket holds
    // only the first upload of a bundle.
    let made = big_file(&scratch, "big.bin.new", BIG);
    let dest = d.worktree().join("big.bin");
    std::fs::rename(made.path(), &dest).unwrap();
    drop(made);
    let big = b860_support::TestFile(dest);
    assert!(
        wait_for(Duration::from_secs(120), || gates.held.exists()).await,
        "TRIGGER PRESENT is not shown: no look of the copy task reached git bundle create; log:\n{}",
        log(&d)
    );
    steps.reached("a look is held in git bundle create");
    // The engine ends two tool calls, and the credential socket is asked, while that look is held.
    gates.step(4);
    assert!(
        wait_for(ANSWERED_WITHIN, || calls_ended(&d) >= 2).await,
        "TRIGGER PRESENT: two tool calls did not end within {ANSWERED_WITHIN:?} while a look was held in git: {} ended\n{}",
        calls_ended(&d),
        steps.report()
    );
    steps.reached("two tool calls ended while the look was held in git");
    let answer = credentials(&d).await;
    assert!(answer.contains("AccessKeyId"), "credentials in the answer");
    steps.reached("the credentials were answered while the look was held in git");
    assert!(
        !gates.open.exists() && copies_stored(&d) == 1,
        "the look was still held in git when the tool calls had ended and the credentials were answered; log:\n{}",
        log(&d)
    );

    // Gate 2: the look goes on, and the bucket stops reading the bundle after its first megabyte.
    std::fs::write(&gates.open, "open\n").unwrap();
    assert!(
        wait_for(Duration::from_secs(120), || d.aws.put_body_held()).await,
        "TRIGGER PRESENT is not shown: the upload of the bundle did not reach the bucket ({} PUTs of it); log:\n{}",
        d.aws.put_attempts(&keys::snapshot_bundle_key(RUN)),
        log(&d)
    );
    steps.reached("the upload of the copy is held by the bucket");
    gates.step(8);
    assert!(
        wait_for(ANSWERED_WITHIN, || calls_ended(&d) >= 4).await,
        "TRIGGER PRESENT: two more tool calls did not end within {ANSWERED_WITHIN:?} while the upload of a copy was held: {} \
         ended\n{}",
        calls_ended(&d),
        steps.report()
    );
    steps.reached("two more tool calls ended while the upload was held");
    let answer = credentials(&d).await;
    assert!(answer.contains("AccessKeyId"), "credentials in the answer");
    steps.reached("the credentials were answered while the upload was held");
    assert!(
        d.aws.put_body_held() && copies_stored(&d) == 1,
        "the upload was still held when the tool calls had ended and the credentials were answered; log:\n{}",
        log(&d)
    );

    // The large file leaves the tree before the look ends: no later copy and no bundle of the session carries it, so they fit what the
    // fake store takes. No look runs meanwhile: the one in flight holds the copy task.
    drop(big);
    d.aws.release_put_body();
    assert!(
        wait_for(Duration::from_secs(60), || copies_stored(&d) >= 2).await,
        "the copy that was held is stored once the bucket reads again; log:\n{}",
        log(&d)
    );
    steps.reached("the copy that was held is stored");
    // The second version of the key: a later look may have stored its own copy since.
    let versions = d.aws.versions_of(&keys::snapshot_bundle_key(RUN));
    assert!(
        versions[1].1.len() as u64 > BIG,
        "the copy stored then is the one whose upload was held: {} bytes",
        versions[1].1.len()
    );
    gates.step(1_000_000);
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let end = steps.reached("the turn ended after the engine could say the rest of its reply");
    println!("{}", steps.report());
    assert!(
        end <= TURN_ENDS_WITHIN,
        "TRIGGER PRESENT: the turn needed {end:?} to end once the engine could say the rest of its reply, more than \
         {TURN_ENDS_WITHIN:?}:\n{}\nlog:\n{}",
        steps.report(),
        log(&d)
    );
}
