//! What the watch of the session's storage costs the work in front of it: two measurements on the build host.
//!
//! They are measurements, not assertions about milliseconds: each prints its numbers with the noise beside them (the same arm run
//! twice, in turn with the other), and asserts only what must hold on any host. Run them with
//! `cargo nextest run -p agentd --test b881_cost --run-ignored only --no-capture`.
//!
//! 1. The write of the copies' marker after each stored copy: as before the watch (`marker::write`) against through the watch
//!    (`StorageWatch::marker_write_for`, which takes the watch's lock, makes the same write and applies its result). The copy task
//!    makes this write after the upload of a copy, outside the time the spacing of the copies counts, so the difference is what a
//!    stored copy costs more, and nothing of it is in front of the engine.
//! 2. From a tool result to the next event: the collector takes a final tool result that names no space left on device and then the
//!    next tool call, with the real watch (its task and its real test writes run beside) against without a watch.
//! 3. The spacing of the copies in whole runs (the real driver in process, the fake engine, a fake bucket): while the engine is held, a
//!    writer changes one file of the work tree every 50 ms for ten seconds, and the copies the bucket stored in that time are counted,
//!    with the watch (every marker write goes through it) against without one (both disk checks off: the marker is written as before).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::acp::AcpHandler;
use agentd::direct::collector::DirectCollector;
use agentd::direct::disk::{DiskProbe, Limits, Reading};
use agentd::direct::marker;
use agentd::direct::storage_watch::{CopyRef, HostWrites, Marked, StorageWatch, WatchSetup};
use agentd::events::EventRecorder;
use agentd::logging::MemoryLog;
use direct_support::{Direct, Opts, RUN as WHOLE_RUN};
use harness_protocol::direct::CopyMarker;
use harness_protocol::direct::s3 as keys;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const RUN: &str = "01JRUN0000000000000000000A";

/// A size that is never asked for in these measurements: no write is refused.
#[derive(Debug)]
struct Small;

impl DiskProbe for Small {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async { Reading::default() })
    }

    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async { (Some(12), false) })
    }
}

fn watch_in(root: &Path, rec: &Arc<EventRecorder>) -> StorageWatch {
    StorageWatch::new(WatchSetup {
        writes: Arc::new(HostWrites),
        probe: Arc::new(Small),
        rec: rec.clone(),
        log: MemoryLog::new(),
        run_id: RUN.to_string(),
        session: 1,
        run_dir: root.join(RUN),
        limits: Limits::default(),
        test_refusal: false,
    })
}

fn stats(mut v: Vec<Duration>) -> (Duration, Duration, Duration) {
    v.sort();
    let n = v.len();
    (v[n / 2], v[n * 95 / 100], v[n - 1])
}

fn show(label: &str, v: &[Duration]) -> Duration {
    let (median, p95, max) = stats(v.to_vec());
    println!(
        "  {label}: median {median:?}, p95 {p95:?}, max {max:?} ({} samples)",
        v.len()
    );
    median
}

fn marker_of(count: u32) -> CopyMarker {
    CopyMarker {
        schema_version: marker::SCHEMA_VERSION,
        session: 1,
        count,
        head_sha: format!("{count:040x}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement: run with --run-ignored only --no-capture"]
async fn b881_cost_of_the_marker_write_through_the_watch() {
    let root = common::scratch("b881-cost-marker");
    std::fs::create_dir_all(root.join(RUN)).unwrap();
    let rec = EventRecorder::new(RUN, "direct");
    let watch = watch_in(&root, &rec);
    let task = watch.start();
    let (mut a1, mut a2, mut b) = (Vec::new(), Vec::new(), Vec::new());
    let mut n = 0u32;
    for _round in 0..10 {
        for arm in ["a1", "b", "a2"] {
            for _ in 0..30 {
                n += 1;
                let m = marker_of(n);
                let t = Instant::now();
                if arm == "b" {
                    let copy = CopyRef {
                        session: 1,
                        count: n,
                        at: "2026-11-21T00:37:12Z".to_string(),
                        head_sha: m.head_sha.clone(),
                        version_id: format!("version-{n}"),
                    };
                    watch
                        .marker_write_for(serde_json::to_vec(&m).unwrap(), Marked::Copy(copy))
                        .await
                        .unwrap();
                } else {
                    marker::write(&root, RUN, &m).await.unwrap();
                }
                let took = t.elapsed();
                match arm {
                    "a1" => a1.push(took),
                    "a2" => a2.push(took),
                    _ => b.push(took),
                }
            }
        }
    }
    task.stop().await;
    println!(
        "b881 cost, the marker write after a stored copy on the build host, 10 rounds of 30 writes for each arm in turn (a1, b, a2):"
    );
    let m1 = show("arm A (as before the watch), first ", &a1);
    let m2 = show("arm A (as before the watch), second", &a2);
    let mb = show("arm B (through the watch)          ", &b);
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    println!(
        "  B against A (median of both A arms): {:+.1} us; noise, A second against A first: {:+.1} us",
        us(mb) - (us(m1) + us(m2)) / 2.0,
        us(m2) - us(m1)
    );
    // The marker on the storage is the last one written, whichever way it went, and no notice was made.
    let on_disk = marker::read(&root, RUN, rustix::process::geteuid().as_raw()).unwrap();
    assert_eq!(on_disk.count, n);
    assert!(rec.drain().is_empty(), "a write that worked made a notice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a measurement: run with --run-ignored only --no-capture"]
async fn b881_cost_from_a_tool_result_to_the_next_event() {
    let root = common::scratch("b881-cost-result");
    std::fs::create_dir_all(root.join(RUN)).unwrap();
    let rec = EventRecorder::new(RUN, "direct");
    let watch = watch_in(&root, &rec);
    let task = watch.start();
    let named = "cp: error writing 'out.bin': No space left on device";
    let (mut a1, mut a2, mut b) = (Vec::new(), Vec::new(), Vec::new());
    for round in 0..10 {
        for arm in ["a1", "b", "a2"] {
            let mut c = DirectCollector::new(rec.clone()).with_engine("cline");
            if arm == "b" {
                c = c.with_storage_watch(watch.clone());
            }
            for i in 0..30 {
                let id = format!("r{round}-{arm}-{i}");
                let next = format!("{id}-next");
                let call = |id: &str| {
                    json!({"sessionId": "s", "update": {"sessionUpdate": "tool_call", "toolCallId": id, "title": "run_commands: make",
                           "kind": "execute", "status": "pending", "rawInput": {"commands": ["make"]}}})
                };
                let done = json!({"sessionId": "s", "update": {"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed",
                                  "rawOutput": json!([{"query": "make", "result": named, "success": false}]).to_string()}});
                c.on_notification("session/update", &call(&id));
                // From the tool result that names no space until the collector has taken the next event of the engine.
                let t = Instant::now();
                c.on_notification("session/update", &done);
                c.on_notification("session/update", &call(&next));
                let took = t.elapsed();
                match arm {
                    "a1" => a1.push(took),
                    "a2" => a2.push(took),
                    _ => b.push(took),
                }
                // The engine's next result is not instant either: a pause, so that the watch's task makes its write beside the
                // collector as it does in a session, and not all at the end.
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
    tokio::time::timeout(Duration::from_secs(30), watch.idle())
        .await
        .unwrap();
    let writes = watch.test_writes_started();
    task.stop().await;
    println!(
        "b881 cost, from a tool result that names no space left to the next event of the engine taken, 10 rounds of 30 results for each arm in turn (a1, b, a2):"
    );
    let m1 = show("arm A (no watch), first        ", &a1);
    let m2 = show("arm A (no watch), second       ", &a2);
    let mb = show("arm B (the watch, real writes) ", &b);
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    println!(
        "  B against A (median of both A arms): {:+.1} us; noise, A second against A first: {:+.1} us; test writes the watch made beside: {writes} for 300 results",
        us(mb) - (us(m1) + us(m2)) / 2.0,
        us(m2) - us(m1)
    );
    // One test write for each result at the most, and none without the watch; no write was refused, so no notice was made.
    assert!((1..=300).contains(&writes), "{writes} test writes");
    assert!(
        !rec.drain().iter().any(|e| matches!(&e.payload,
            harness_protocol::envelope::Payload::Inline { value } if value["reason"] == "storage_refused")),
        "a write that worked made a notice"
    );
}

/// How long the writer changes the work tree in one whole run.
const WINDOW: Duration = Duration::from_secs(10);

/// One whole run of `no_space_result` whose engine is held after its two edits. Once the first copy of the session is stored, a
/// writer changes one file of the work tree every 50 ms for [`WINDOW`]; the answer is how many copies the bucket stored in that
/// time. `with_watch` false switches both disk checks off, so the session has no watch and writes its marker as before.
async fn copies_in_a_window(with_watch: bool, label: &str) -> usize {
    let dir = common::scratch(label);
    let steps = dir.join("engine-steps");
    std::fs::write(&steps, "4\n").unwrap();
    let d = Direct::start(Opts {
        scenario: "no_space_result",
        engine_env: vec![("FAKE_ACP_STEP_FILE", steps.display().to_string())],
        timeout_s: 300,
        edit_cfg: Some(Box::new(move |c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(300));
            c.direct.snapshot_floor = Some(Duration::from_millis(50));
            if !with_watch {
                c.direct.disk_low_mb = 0;
                c.direct.workspace_limit_mb = 0;
            }
        })),
        ..Opts::default()
    })
    .await;
    let stored = || {
        d.aws
            .versions_of(&keys::snapshot_bundle_key(WHOLE_RUN))
            .len()
    };
    let writer = async {
        let wait_until = Instant::now() + Duration::from_secs(120);
        while stored() == 0 && Instant::now() < wait_until {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let before = stored();
        let file = d.worktree().join("ticker.txt");
        let end = Instant::now() + WINDOW;
        let mut n = 0u64;
        while Instant::now() < end {
            n += 1;
            let _ = std::fs::write(&file, format!("{n}\n"));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let made = stored() - before;
        std::fs::write(&steps, "1000000\n").unwrap();
        (before, made)
    };
    let ((_, resp), (before, made)) = tokio::join!(d.invoke(), writer);
    assert!(
        before >= 1,
        "no copy was stored before the window: {resp:?}"
    );
    made
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run with --run-ignored only --no-capture"]
async fn b881_cost_of_the_spacing_of_the_copies_in_whole_runs() {
    let (mut a1, mut a2, mut b) = (Vec::new(), Vec::new(), Vec::new());
    for round in 0..3 {
        for arm in ["a1", "b", "a2"] {
            let made =
                copies_in_a_window(arm == "b", &format!("b881-cost-spacing-{round}-{arm}")).await;
            match arm {
                "a1" => a1.push(made),
                "a2" => a2.push(made),
                _ => b.push(made),
            }
        }
    }
    let spacing = |v: &[usize]| {
        let total: usize = v.iter().sum();
        WINDOW.as_secs_f64() * 1000.0 * v.len() as f64 / total.max(1) as f64
    };
    println!(
        "b881 cost, the copies a session stored in {} s while one file changed every 50 ms (copy timer 300 ms), 3 whole runs of each arm in turn (a1, b, a2):",
        WINDOW.as_secs()
    );
    println!(
        "  arm A (no watch), first : copies {a1:?}, mean spacing {:.0} ms",
        spacing(&a1)
    );
    println!(
        "  arm A (no watch), second: copies {a2:?}, mean spacing {:.0} ms",
        spacing(&a2)
    );
    println!(
        "  arm B (the watch)       : copies {b:?}, mean spacing {:.0} ms",
        spacing(&b)
    );
    println!(
        "  B against A (mean of both A arms): {:+.0} ms a copy; noise, A second against A first: {:+.0} ms a copy",
        spacing(&b) - (spacing(&a1) + spacing(&a2)) / 2.0,
        spacing(&a2) - spacing(&a1)
    );
    assert!(
        a1.iter().chain(&a2).chain(&b).all(|n| *n >= 2),
        "a run stored fewer than two copies in its window: {a1:?} {b:?} {a2:?}"
    );
}
