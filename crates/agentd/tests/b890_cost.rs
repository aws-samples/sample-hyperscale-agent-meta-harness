//! What the checked write of the git settings costs the work in front of it.
//!
//! 1. **Nothing waits for it in front of the engine.** `git_settings::put_back` is called in one place: the end sequence of a
//!    session (`direct/mod.rs`, `finish`), after the modules were told that the engine is gone. No other source file of the worker
//!    calls it, and the checked write itself is called only for the git settings and for the marker of the copies, which was a synced
//!    write before. So no tool call and no prompt waits for the added sync and rename. An assertion over the sources.
//! 2. **The bound.** The end of a run takes less than [`BOUND`] more for both files at the median on the build host. An assertion,
//!    held with every build.
//! 3. **What the end of a run takes more.** A measurement on the build host, printed with its noise beside it: the write as it was
//!    before the change, run as two arms in turn with the write as it is now. Run it with
//!    `cargo nextest run -p agentd --test b890_cost --run-ignored only --no-capture`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{GitState, Worktree, check_git_dir, prepare_local};
use common::{python_calc, scratch};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The most the checked write of both files may add to the end of a run at the median on the build host.
const BOUND: Duration = Duration::from_millis(500);

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn b890_only_the_end_sequence_puts_the_git_settings_back_after_the_engine_is_gone() {
    let root = common::repo_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates/agentd/src"), &mut files);
    assert!(
        files.len() > 100,
        "the worker's sources were not found: {}",
        files.len()
    );
    let with = |needle: &str| -> Vec<String> {
        let mut v: Vec<String> = files
            .iter()
            .filter(|p| std::fs::read_to_string(p).unwrap().contains(needle))
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    };
    // The one caller of the write that repeats.
    assert_eq!(
        with("git_settings::put_back("),
        vec!["crates/agentd/src/direct/mod.rs"]
    );
    // One try of it: its own module and the one that repeats it.
    assert_eq!(
        with("restore_with("),
        vec![
            "crates/agentd/src/direct/git_settings.rs",
            "crates/agentd/src/workspace.rs"
        ]
    );
    // The checked write: the git settings and the marker of the copies.
    assert_eq!(
        with("checked_write::replace("),
        vec![
            "crates/agentd/src/direct/marker.rs",
            "crates/agentd/src/workspace.rs"
        ]
    );
    // The call is in the end sequence, after the modules were told that the engine is gone.
    let text = std::fs::read_to_string(root.join("crates/agentd/src/direct/mod.rs")).unwrap();
    assert_eq!(text.matches("git_settings::put_back(").count(), 1);
    let finish = text.find("\nasync fn finish(").expect("the end sequence");
    let end = finish + 1 + text[finish + 1..].find("\n}\n").expect("its end");
    let gone = finish
        + text[finish..]
            .find("hooks.shutdown(hook_env, &classified).await;")
            .expect("the modules are told that the engine is gone");
    let call = text.find("git_settings::put_back(").unwrap();
    assert!(
        finish < gone && gone < call && call < end,
        "finish {finish}, engine gone {gone}, call {call}, end {end}"
    );
    // The run's heartbeat goes on through the wait on the storage: the flusher that sends it every minute is stopped after the
    // call, at the end of the sequence. The state machine allows 600 s between two heartbeats, and the wait keeps none back.
    let stopped = finish
        + text[finish..]
            .find("flusher.stop().await")
            .expect("the flusher is stopped in the end sequence");
    assert!(
        call < stopped && stopped < end,
        "call {call}, flusher stopped {stopped}, end {end}"
    );
}

async fn checkout(label: &str) -> (Worktree, GitState) {
    let d = scratch(label);
    let src = python_calc(&d);
    let wt = prepare_local(&d.join("ws"), "run890", "direct", &src)
        .await
        .unwrap();
    // Both files are there at the start, so both are written at the end.
    std::fs::write(wt.repo.join(".git/info/attributes"), "*.bin -diff\n").unwrap();
    let state = GitState::snapshot(&wt).unwrap();
    (wt, state)
}

/// The write as it was before the change: the entry removed, a new file made with `O_EXCL` and `O_NOFOLLOW`, the bytes written, no
/// sync, the close not read; then the comparison with the start of the run.
fn before(wt: &Worktree, state: &GitState, files: &[(PathBuf, Vec<u8>)]) {
    check_git_dir(&wt.repo).unwrap();
    for (path, bytes) in files {
        std::fs::remove_file(path).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }
    state.verify(wt).unwrap();
}

/// The write as it is now: beside the file, synced, the close checked, renamed over it; then the comparison.
fn after(wt: &Worktree, state: &GitState) {
    state.restore(wt).unwrap();
}

fn timed(f: impl FnOnce()) -> Duration {
    let t = Instant::now();
    f();
    t.elapsed()
}

/// The median, with the tenth and the ninetieth of a hundred.
fn spread(v: &mut [Duration]) -> (Duration, Duration, Duration) {
    v.sort();
    (v[v.len() / 2], v[v.len() / 10], v[v.len() * 9 / 10])
}

fn micros(d: Duration) -> u128 {
    d.as_micros()
}

/// `rounds` rounds of the three arms in turn: before, after, before again.
async fn measure(label: &str, rounds: usize) -> [(Duration, Duration, Duration); 3] {
    let (wt, state) = checkout(label).await;
    let files: Vec<(PathBuf, Vec<u8>)> = [".git/config", ".git/info/attributes"]
        .iter()
        .map(|name| {
            let path = wt.repo.join(name);
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..rounds {
        a.push(timed(|| before(&wt, &state, &files)));
        b.push(timed(|| after(&wt, &state)));
        c.push(timed(|| before(&wt, &state, &files)));
    }
    [spread(&mut a), spread(&mut b), spread(&mut c)]
}

#[tokio::test]
async fn b890_the_checked_write_of_both_files_stays_under_its_bound() {
    let [first, now, again] = measure("b890-bound", 30).await;
    let was = (first.0 + again.0) / 2;
    let added = now.0.saturating_sub(was);
    println!(
        "B890 BOUND (30 rounds, both files): before {} us, now {} us, added {} us at the median; bound {} us",
        micros(was),
        micros(now.0),
        micros(added),
        micros(BOUND)
    );
    assert!(added < BOUND, "added {added:?} at the median");
}

#[tokio::test]
#[ignore = "a measurement: run with --run-ignored only --no-capture"]
async fn b890_what_the_checked_write_adds_to_the_end_of_a_run() {
    let rounds = 200;
    let [first, now, again] = measure("b890-cost", rounds).await;
    let was = (first.0 + again.0) / 2;
    println!(
        "B890 COST ({rounds} rounds, both files, the build host's own disk; median [tenth..ninetieth of a hundred] in microseconds)"
    );
    for (name, (median, low, high)) in [
        ("before the change      ", first),
        ("now                    ", now),
        ("before the change again", again),
    ] {
        println!(
            "B890 COST {name}: {} [{}..{}]",
            micros(median),
            micros(low),
            micros(high)
        );
    }
    println!(
        "B890 COST added at the median: {} us; noise between the two arms of before: {} us",
        micros(now.0.saturating_sub(was)),
        micros(first.0.abs_diff(again.0))
    );
    // What must hold on any host: the files read as at the start after every round.
    assert!(now.0 > Duration::ZERO);
}
