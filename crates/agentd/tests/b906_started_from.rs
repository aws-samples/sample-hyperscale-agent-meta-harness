//! The notice of a storage refusal names the copy a later session started from.
//!
//! A session after the first starts from a copy an earlier session saved. Until it has stored a copy of its own, that copy is the
//! last one known good: a refusal of the session's storage in that time left the notice without a copy to name. The restore now hands
//! the watch the copy it read, with the version the bucket answered for its bundle (`Marked::StartFrom`), and the notice names it as
//! the copy this session started from, with the command `ham pull <run> --before-refusal <id>`. What this file pins, with scripted
//! writes and a scripted size:
//!
//! * a refused marker of the start of a later session names the copy the session started from: its session, number, time, commit
//!   and version, and the command with the notice's own id;
//! * a refusal after the marker of the start, before the session stored a copy, names it too, and so does every later notice of the
//!   session, with the id of the session's first notice;
//! * a session that stored a copy of its own before the refusal names that copy, in the words it had;
//! * a first session, and a later session whose restore read no version, keep the sentence they had;
//! * an own copy the bucket gave no version leaves no copy to name.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::direct::disk::{DiskProbe, Limits, Reading};
use agentd::direct::storage_watch::{
    CopyRef, Marked, Seen, StorageWatch, StorageWrites, WatchSetup, WatchTask, WriteError,
};
use agentd::events::EventRecorder;
use agentd::logging::MemoryLog;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01JRUN0000000000000000000B";

/// Writes that answer from a list (`true`: the storage takes the write; `false`: errno 28), then take every write.
#[derive(Debug, Default)]
struct Script {
    answers: Mutex<VecDeque<bool>>,
    tests: AtomicU32,
}

impl Script {
    fn answer(&self) -> Result<(), WriteError> {
        if self.answers.lock().unwrap().pop_front().unwrap_or(true) {
            return Ok(());
        }
        Err(WriteError {
            step: "write",
            errno: Some(28),
            text: std::io::Error::from_raw_os_error(28).to_string(),
            test_switch: false,
        })
    }
}

impl StorageWrites for Script {
    fn test(&self, _run_dir: &Path) -> Result<(), WriteError> {
        let done = self.answer();
        self.tests.fetch_add(1, Ordering::SeqCst);
        done
    }

    fn marker(&self, _run_dir: &Path, _bytes: &[u8]) -> Result<(), WriteError> {
        self.answer()
    }
}

/// Files of 165 MiB: far below the warning mark, so a refusal brings a notice.
#[derive(Debug)]
struct Sized;

impl DiskProbe for Sized {
    fn measure(&self) -> BoxFuture<'_, Reading> {
        Box::pin(async { Reading::default() })
    }

    fn storage_used(&self) -> BoxFuture<'_, (Option<u64>, bool)> {
        Box::pin(async { (Some(165), false) })
    }
}

struct Rig {
    watch: StorageWatch,
    task: Option<WatchTask>,
    script: Arc<Script>,
    rec: Arc<EventRecorder>,
    log: Arc<MemoryLog>,
    seen: Vec<Envelope>,
}

/// The watch of session `session` of the run, whose writes answer `answers` in order.
fn rig(session: u32, answers: &[bool]) -> Rig {
    let script = Arc::new(Script {
        answers: Mutex::new(answers.iter().copied().collect()),
        tests: AtomicU32::new(0),
    });
    let rec = EventRecorder::new(RUN, "direct");
    let log = MemoryLog::new();
    let watch = StorageWatch::new(WatchSetup {
        writes: script.clone(),
        probe: Arc::new(Sized),
        rec: rec.clone(),
        log: log.clone(),
        run_id: RUN.to_string(),
        session,
        run_dir: common::scratch("b906-watch"),
        limits: Limits::default(),
        test_refusal: false,
    });
    let task = Some(watch.start());
    Rig {
        watch,
        task,
        script,
        rec,
        log,
        seen: Vec::new(),
    }
}

impl Rig {
    /// One wake, and the wait until the task made its test write and applied the result.
    async fn wake(&mut self, seen: Seen) {
        let before = self.script.tests.load(Ordering::SeqCst);
        self.watch.wake(seen);
        for _ in 0..4000 {
            if self.script.tests.load(Ordering::SeqCst) > before {
                self.watch.settled().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("the test write did not happen; log: {:?}", self.log.lines());
    }

    /// The storage notices recorded so far, with their event ids.
    fn notices(&mut self) -> Vec<(String, Value)> {
        self.seen.extend(self.rec.drain());
        self.seen
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::Inline { value }
                    if e.event_type == EventType::TurnProgress
                        && value["kind"] == "notify"
                        && value["reason"] == "storage_refused" =>
                {
                    Some((e.event_id.clone(), value.clone()))
                }
                _ => None,
            })
            .collect()
    }

    async fn stop(mut self) {
        if let Some(t) = self.task.take() {
            t.stop().await;
        }
    }
}

/// Copy 7 of the run, which session 1 saved: the copy session 2 started from, with the version the bucket answered for the read.
fn from() -> CopyRef {
    CopyRef {
        session: 1,
        count: 7,
        at: "2026-11-21T00:37:12Z".to_string(),
        head_sha: format!("{:040x}", 7),
        version_id: "version-the-restore-read".to_string(),
    }
}

/// Copy `count` of the run, which session 2 saved itself.
fn own(count: u32) -> CopyRef {
    CopyRef {
        session: 2,
        count,
        at: format!("2026-11-21T00:4{}:12Z", count % 10),
        head_sha: format!("{count:040x}"),
        version_id: format!("version-of-copy-{count}"),
    }
}

/// The member a notice carries for the copy the session started from.
fn from_member() -> Value {
    json!({
        "session": 1, "count": 7, "at": "2026-11-21T00:37:12Z",
        "headSha": format!("{:040x}", 7), "versionId": "version-the-restore-read",
    })
}

/// The sentence of the copy a session started from, with the id its command names.
fn from_sentence(id: &str) -> String {
    format!(
        "Copy 7 of session 1 (2026-11-21 00:37 UTC) is the copy this session started from: ham pull {RUN} --before-refusal {id} downloads it."
    )
}

/// The sentence every form of the notice ends with.
fn newest() -> String {
    format!(
        "ham pull {RUN} --snapshot downloads the newest copy, which holds the files as they are."
    )
}

#[tokio::test]
async fn b906_a_refused_start_marker_of_a_later_session_names_the_copy_it_started_from() {
    // The first write of session 2 through the watch is the marker of its start, and the storage refuses it. The session has stored
    // no copy of its own: the copy it started from is the last one known good, and the notice names it.
    let mut r = rig(2, &[false]);
    let refused = r
        .watch
        .marker_write_for(b"{}".to_vec(), Marked::StartFrom(from()))
        .await
        .unwrap_err();
    assert_eq!(refused.errno, Some(28));
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    let (id, notice) = &n[0];
    let s = &notice["storage"];
    assert_eq!(s["session"], 2, "{notice}");
    assert_eq!(s["stretch"], 1, "{notice}");
    assert_eq!(s["seen"], "start", "{notice}");
    assert_eq!(
        s["copyBefore"],
        from_member(),
        "the member names the copy of the earlier session, with the version the restore read: {notice}"
    );
    let detail = notice["detail"].as_str().unwrap();
    assert!(
        detail.contains(&format!(
            "This session had saved no copy before ham first saw this. {} {}",
            from_sentence(id),
            newest()
        )),
        "{detail}"
    );
    assert!(detail.ends_with(&newest()), "{detail}");
    r.stop().await;
}

#[tokio::test]
async fn b906_a_refusal_before_the_first_stored_copy_names_the_started_from_copy_and_later_notices_keep_it()
 {
    // The marker of the start is taken; the first refusal comes at a tool result, before session 2 stored a copy. The storage then
    // takes a write, the session stores copy 8, and a second stretch opens: the copy a notice names was fixed at the first refusal.
    let mut r = rig(2, &[true, false, true, true, false]);
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::StartFrom(from()))
        .await
        .unwrap();
    assert!(
        r.notices().is_empty(),
        "a start that was taken says nothing"
    );
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    let (first_id, first) = n[0].clone();
    assert_eq!(first["storage"]["seen"], "tool", "{first}");
    assert_eq!(first["storage"]["copyBefore"], from_member(), "{first}");
    assert!(
        first["detail"].as_str().unwrap().contains(&format!(
            "This session had saved no copy before ham first saw this. {}",
            from_sentence(&first_id)
        )),
        "{first}"
    );
    // The stretch closes, the session's own copy 8 is marked, and the storage refuses again.
    r.wake(Seen::PromptBoundary).await;
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Copy(own(8)))
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 2, "{:?}", r.log.lines());
    let later = &n[1].1;
    assert_eq!(later["storage"]["stretch"], 2, "{later}");
    assert_eq!(later["storage"]["first"], json!(first_id), "{later}");
    assert_eq!(
        later["storage"]["copyBefore"],
        from_member(),
        "a copy stored after the first refusal does not replace the copy the notices name: {later}"
    );
    assert_eq!(
        later["detail"],
        json!(format!(
            "The session's storage refused a write again (no space left on device); ham has seen this 2 times in this session. This session had saved no copy before ham first saw it. {} {}",
            from_sentence(&first_id),
            newest()
        )),
        "{later}"
    );
    r.stop().await;
}

#[tokio::test]
async fn b906_a_session_that_stored_a_copy_of_its_own_names_that_copy_as_before() {
    // Session 2 started from copy 7 and stored copy 8 itself before the storage refused a write: copy 8 is the last copy saved
    // before, and the notice has the words it always had. The copy the session started from is not named.
    let mut r = rig(2, &[true, true, false]);
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::StartFrom(from()))
        .await
        .unwrap();
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Copy(own(8)))
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    let (id, notice) = &n[0];
    let copy = &notice["storage"]["copyBefore"];
    assert_eq!(copy["session"], 2, "{notice}");
    assert_eq!(copy["count"], 8, "{notice}");
    assert_eq!(copy["versionId"], "version-of-copy-8", "{notice}");
    let detail = notice["detail"].as_str().unwrap();
    assert!(
        detail.contains(&format!(
            "Copy 8 (2026-11-21 00:48 UTC) is the last copy saved before ham first saw this: ham pull {RUN} --before-refusal {id} downloads it. {}",
            newest()
        )),
        "{detail}"
    );
    assert!(!detail.contains("started from"), "{detail}");
    assert!(!detail.contains("had saved no copy"), "{detail}");
    r.stop().await;
}

#[tokio::test]
async fn b906_a_first_session_and_a_restore_that_read_no_version_keep_the_sentence_they_had() {
    // Session 1 starts from no copy. Session 2 here restored an object for which the bucket answered no version, so its start
    // names no copy either. Both say what a session without a copy always said, and name the command for the newest copy.
    for session in [1, 2] {
        let mut r = rig(session, &[false]);
        r.watch
            .marker_write_for(b"{}".to_vec(), Marked::Start)
            .await
            .unwrap_err();
        let n = r.notices();
        assert_eq!(n.len(), 1, "session {session}: {:?}", r.log.lines());
        let notice = &n[0].1;
        assert_eq!(notice["storage"]["session"], session, "{notice}");
        assert!(
            notice["storage"].get("copyBefore").is_none(),
            "session {session}: {notice}"
        );
        let detail = notice["detail"].as_str().unwrap();
        assert!(
            detail.ends_with(&format!(
                "This session had saved no copy before ham first saw this. {}",
                newest()
            )),
            "session {session}: {detail}"
        );
        assert!(!detail.contains("--before-refusal"), "{detail}");
        assert!(!detail.contains("started from"), "{detail}");
        r.stop().await;
    }
}

#[tokio::test]
async fn b906_an_own_copy_without_a_version_leaves_no_copy_to_name() {
    // Session 2 started from copy 7 and stored copy 8, for which the bucket answered no version. Copy 7 is then no longer the last
    // copy saved and copy 8 cannot be asked for: the notice names none, as for a first session in the same case.
    let mut r = rig(2, &[true, true, false]);
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::StartFrom(from()))
        .await
        .unwrap();
    r.watch
        .marker_write_for(b"{}".to_vec(), Marked::Unnamed)
        .await
        .unwrap();
    r.wake(Seen::ToolResult).await;
    let n = r.notices();
    assert_eq!(n.len(), 1, "{:?}", r.log.lines());
    let notice = &n[0].1;
    assert!(notice["storage"].get("copyBefore").is_none(), "{notice}");
    let detail = notice["detail"].as_str().unwrap();
    assert!(!detail.contains("--before-refusal"), "{detail}");
    assert!(!detail.contains("started from"), "{detail}");
    assert!(detail.ends_with(&newest()), "{detail}");
    r.stop().await;
}
