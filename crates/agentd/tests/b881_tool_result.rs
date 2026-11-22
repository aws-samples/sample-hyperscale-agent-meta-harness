//! A tool result that names no space left on device makes the worker test the storage itself: the recorder reads the result,
//! the collector hands it on, and neither waits for the test write.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::acp::AcpHandler;
use agentd::direct::collector::DirectCollector;
use agentd::direct::disk::{self, DiskProbe, Limits, Reading};
use agentd::direct::recorder::Recorder;
use agentd::direct::storage_watch::{Seen, StorageWatch, StorageWrites, WatchSetup, WriteError};
use agentd::events::EventRecorder;
use agentd::logging::MemoryLog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn call(id: &str, title: &str) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute",
           "status": "pending", "rawInput": {"commands": ["make"]}})
}

/// The first engine's final update: the result is in `rawOutput`.
fn done(id: &str, status: &str, output: &str) -> Value {
    json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status,
           "rawOutput": Value::String(json!([{"query": "make", "result": output, "success": status == "completed"}]).to_string())})
}

/// The second engine's update: the result is in the content blocks, and `rawOutput` holds only its own metadata.
fn blocks(id: &str, status: &str, output: &str) -> Value {
    json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status,
           "rawOutput": {"metadata": {"exit": 1}},
           "content": [{"type": "content", "content": {"type": "text", "text": output}}]})
}

const NO_SPACE_TEXTS: [&str; 4] = [
    "cp: error writing 'big.bin': No space left on device",
    "Error: ENOSPC: no space left on device, write",
    "failed to write: Os { code: 28 } (os error 28)",
    "fatal: unable to write new index file: no space left on device",
];

#[test]
fn b881_a_final_tool_result_that_names_no_space_is_handed_on_once() {
    for (i, text) in NO_SPACE_TEXTS.iter().enumerate() {
        let mut r = Recorder::new(vec![]).with_engine("cline");
        let id = format!("t{i}");
        r.on_tool_call(&call(&id, "run_commands: make"));
        assert!(
            !r.take_space_named(),
            "a call that has no result yet names nothing"
        );
        // An update that is not final says nothing yet, whatever its text.
        r.on_tool_call_update(&done(&id, "in_progress", text));
        assert!(!r.take_space_named(), "{text}");
        r.on_tool_call_update(&done(&id, "failed", text));
        assert!(r.take_space_named(), "{text}");
        assert!(!r.take_space_named(), "handed on once: {text}");
    }
    // A completed call whose output holds the words reads the same (a command that printed them and exited 0).
    let mut r = Recorder::new(vec![]).with_engine("cline");
    r.on_tool_call(&call("t1", "run_commands: make"));
    r.on_tool_call_update(&done("t1", "completed", NO_SPACE_TEXTS[0]));
    assert!(r.take_space_named());
    // A result that names something else hands nothing on: a read-only file system, a full quota, an exit code 28.
    for text in [
        "touch: cannot touch 'x': Read-only file system (os error 30)",
        "Disk quota exceeded",
        "[Command exited with code 28]",
        "all 280 tests passed; os error 280 is not a thing",
        "ok",
    ] {
        let mut r = Recorder::new(vec![]).with_engine("cline");
        r.on_tool_call(&call("t1", "run_commands: make"));
        r.on_tool_call_update(&done("t1", "failed", text));
        assert!(!r.take_space_named(), "{text}");
    }
    // The next result decides anew: a result without the words after one with them hands nothing on.
    let mut r = Recorder::new(vec![]).with_engine("cline");
    r.on_tool_call(&call("t1", "run_commands: make"));
    r.on_tool_call_update(&done("t1", "failed", NO_SPACE_TEXTS[0]));
    r.on_tool_call(&call("t2", "run_commands: make"));
    r.on_tool_call_update(&done("t2", "completed", "ok"));
    assert!(
        !r.take_space_named(),
        "the newest result holds no such words"
    );
    // Only the last 256 KiB of a result are read.
    let mut r = Recorder::new(vec![]).with_engine("cline");
    r.on_tool_call(&call("t1", "run_commands: make"));
    let far = format!("No space left on device\n{}", "x".repeat(300 * 1024));
    r.on_tool_call_update(&done("t1", "completed", &far));
    assert!(
        !r.take_space_named(),
        "words before the last 256 KiB are not read"
    );
    let near = format!("{}\nNo space left on device\n", "x".repeat(300 * 1024));
    r.on_tool_call_update(&done("t1", "completed", &near));
    assert!(r.take_space_named());
    // The engine sends the final status with the call itself.
    let mut r = Recorder::new(vec![]).with_engine("cline");
    let mut whole = call("t9", "run_commands: make");
    whole["status"] = json!("failed");
    whole["rawOutput"] = json!("write error: No space left on device");
    r.on_tool_call(&whole);
    assert!(r.take_space_named());
}

#[test]
fn b881_the_second_engines_content_blocks_are_read_the_same_way() {
    for text in NO_SPACE_TEXTS {
        let mut r = Recorder::new(vec![]).with_engine("opencode");
        r.on_tool_call(&call("t1", "bash"));
        r.on_tool_call_update(&blocks("t1", "in_progress", text));
        assert!(!r.take_space_named(), "{text}");
        r.on_tool_call_update(&blocks("t1", "completed", text));
        assert!(r.take_space_named(), "{text}");
        assert!(!r.take_space_named());
    }
    let mut r = Recorder::new(vec![]).with_engine("opencode");
    r.on_tool_call(&call("t1", "bash"));
    r.on_tool_call_update(&blocks("t1", "completed", "ok"));
    assert!(!r.take_space_named());
    // Its own metadata in `rawOutput` is not the result: words there hand nothing on.
    let mut odd = blocks("t1", "completed", "ok");
    odd["rawOutput"] = json!({"metadata": {"note": "No space left on device"}});
    r.on_tool_call_update(&odd);
    assert!(!r.take_space_named());
}

// ---- the collector and the prompt boundary never wait -----------------------------------------------------------------------------

/// A storage whose test write does not return until the gate opens.
#[derive(Debug)]
struct Held {
    gate: Arc<(Mutex<bool>, Condvar)>,
    tests: AtomicU32,
}

impl StorageWrites for Held {
    fn test(&self, _run_dir: &Path) -> Result<(), WriteError> {
        self.tests.fetch_add(1, Ordering::SeqCst);
        let (open, cv) = &*self.gate;
        let mut open = open.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
        Ok(())
    }

    fn marker(&self, _run_dir: &Path, _bytes: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }
}

#[derive(Debug)]
struct NoSize;

impl DiskProbe for NoSize {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async { Reading::default() })
    }
}

fn update(u: Value) -> Value {
    json!({"sessionId": "s1", "update": u})
}

#[tokio::test]
async fn b881_the_collector_and_the_prompt_boundary_never_wait_for_the_test_write() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let held = Arc::new(Held {
        gate: gate.clone(),
        tests: AtomicU32::new(0),
    });
    let rec = EventRecorder::new("run-1", "direct");
    let watch = StorageWatch::new(WatchSetup {
        writes: held.clone(),
        probe: Arc::new(NoSize),
        rec: rec.clone(),
        log: MemoryLog::new(),
        run_id: "run-1".to_string(),
        session: 1,
        run_dir: common::scratch("b881-held"),
        limits: Limits::default(),
        test_refusal: false,
    });
    let task = watch.start();
    let mut c = DirectCollector::new(rec.clone())
        .with_engine("cline")
        .with_storage_watch(watch.clone());
    // 200 tool results that name no space, while the storage answers nothing: each is taken at once.
    let began = Instant::now();
    for i in 0..200 {
        let id = format!("t{i}");
        c.on_notification("session/update", &update(call(&id, "run_commands: make")));
        c.on_notification(
            "session/update",
            &update(done(
                &id,
                "failed",
                "cp: write error: No space left on device",
            )),
        );
    }
    // The prompt boundary's wake returns at once too.
    for _ in 0..200 {
        watch.wake(Seen::PromptBoundary);
    }
    let took = began.elapsed();
    assert_eq!(c.tool_calls(), 200);
    // The write did start, and it is still out: nobody waited for it.
    for _ in 0..2000 {
        if held.tests.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        held.tests.load(Ordering::SeqCst),
        1,
        "one test write, and it is out"
    );
    assert!(
        took < Duration::from_secs(2),
        "the collector waited: {took:?}"
    );
    {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }
    watch.settled().await;
    task.stop().await;
}

#[tokio::test]
async fn b881_a_result_without_the_words_wakes_nothing() {
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let held = Arc::new(Held {
        gate,
        tests: AtomicU32::new(0),
    });
    let rec = EventRecorder::new("run-1", "direct");
    let watch = StorageWatch::new(WatchSetup {
        writes: held.clone(),
        probe: Arc::new(NoSize),
        rec: rec.clone(),
        log: MemoryLog::new(),
        run_id: "run-1".to_string(),
        session: 1,
        run_dir: common::scratch("b881-quiet"),
        limits: Limits::default(),
        test_refusal: false,
    });
    let task = watch.start();
    let mut c = DirectCollector::new(rec)
        .with_engine("cline")
        .with_storage_watch(watch.clone());
    for i in 0..50 {
        let id = format!("t{i}");
        c.on_notification("session/update", &update(call(&id, "run_commands: make")));
        c.on_notification("session/update", &update(done(&id, "completed", "ok")));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    watch.settled().await;
    assert_eq!(
        held.tests.load(Ordering::SeqCst),
        0,
        "a result that names nothing makes no test write"
    );
    assert_eq!(watch.test_writes_started(), 0);
    task.stop().await;
}

// ---- what reading a result costs (section 7 of the brief, measurement 2) ------------------------------------------------------------

/// The search of a result for the words, on the largest text that is read (256 KiB) with no match: measured on the build host
/// (36 processors, load 30 to 60) on 2026-11-21 at 3.5 ms a result in a test build, which is not optimized (median of 200 runs,
/// the largest 9.1 ms), and at 0.3 ms in an optimized build as the worker's is (a copy of the function compiled alone). The bound is twenty
/// times the test build's number: a search that gets slower by an order of magnitude fails here, and the noise of the host does not.
#[test]
fn b881_reading_a_result_for_the_words_is_cheap() {
    let text = "a line of build output that names nothing of interest\n".repeat(256 * 1024 / 54);
    assert!(text.len() <= 256 * 1024 && text.len() > 250 * 1024);
    let mut times: Vec<Duration> = (0..200)
        .map(|_| {
            let t = Instant::now();
            assert!(!disk::names_no_space(&text));
            t.elapsed()
        })
        .collect();
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "b881 names_no_space on {} bytes with no match: median {median:?}, max {:?}",
        text.len(),
        times[times.len() - 1]
    );
    assert!(
        median < Duration::from_micros(3_500 * 20),
        "median {median:?}"
    );
}
