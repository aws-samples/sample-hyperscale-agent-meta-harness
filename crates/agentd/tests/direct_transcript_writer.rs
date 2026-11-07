//! The transcript writer: records become numbered, write-once chunk objects;
//! an upload failure never fails the run and leaves a gap that the next object names; two writers, a restart, a pre-created key and a
//! lost answer all end in a truthful sequence; no buffer grows without a bound; and the final flush says what it holds.
//! The store is an in-memory stand-in that answers 412 the way S3 does for `If-None-Match: *`; time is paused.
#![allow(clippy::unwrap_used)]

mod direct_transcript_support;

use agentd::acp::Dir;
use agentd::direct::transcript::settings::Settings;
use agentd::direct::transcript::store::ChunkStore;
use agentd::direct::transcript::writer::{Writer, WriterConfig};
use direct_transcript_support::{
    MemStore, RUN, ULID_A, ULID_B, all_records, key, read_all, read_chunk,
};
use harness_protocol::transcript::{CompletenessKind, TranscriptSummary};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const SID: &str = "sess-1";

fn quick() -> Settings {
    Settings {
        upload_tries: 3,
        retry_base: Duration::from_millis(10),
        final_deadline: Duration::from_secs(3),
        ..Settings::default()
    }
}

fn start_with(store: &Arc<MemStore>, incarnation: &str, s: Settings) -> Writer {
    let dynstore: Arc<dyn ChunkStore> = store.clone();
    Writer::start(
        WriterConfig {
            run_id: RUN.to_string(),
            engine: "Engine".to_string(),
            protocol: "1.38.0".to_string(),
            incarnation: incarnation.to_string(),
            settings: s,
            secrets: Vec::new(),
        },
        dynstore,
    )
}

fn start(store: &Arc<MemStore>) -> Writer {
    start_with(store, ULID_A, quick())
}

/// Lets the tasks run: a few turns of the scheduler with no time passing.
async fn settle() {
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }
}

async fn pass(secs: u64) {
    settle().await;
    tokio::time::advance(Duration::from_secs(secs)).await;
    settle().await;
}

fn update(u: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": u}})
}

fn chunk(text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn tool_call(i: usize) -> Value {
    update(
        json!({"sessionUpdate": "tool_call", "toolCallId": format!("t{i}"), "title": "ls", "kind": "execute", "status": "pending", "rawInput": {"command": "ls"}}),
    )
}

fn tool_done(i: usize, out: &str) -> Value {
    update(
        json!({"sessionUpdate": "tool_call_update", "toolCallId": format!("t{i}"), "status": "completed", "rawOutput": out}),
    )
}

fn thought(text: &str) -> Value {
    update(
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn response(id: u64, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A short session: handshake, a prompt, some text, a tool call and the answer of the prompt (nine records).
fn session(w: &Writer) {
    w.push(
        Dir::ToEngine,
        &request(1, "initialize", json!({"protocolVersion": 1})),
        false,
    );
    w.push(
        Dir::FromEngine,
        &response(1, json!({"protocolVersion": 1})),
        false,
    );
    w.push(
        Dir::ToEngine,
        &request(2, "session/new", json!({"cwd": "/w"})),
        false,
    );
    w.push(
        Dir::FromEngine,
        &response(2, json!({"sessionId": SID})),
        false,
    );
    w.push(
        Dir::ToEngine,
        &request(
            3,
            "session/prompt",
            json!({"sessionId": SID, "prompt": [{"type": "text", "text": "go"}]}),
        ),
        false,
    );
    w.push(Dir::FromEngine, &chunk("I will start."), false);
    w.push(Dir::FromEngine, &tool_call(1), false);
    w.push(Dir::FromEngine, &tool_done(1, "ok"), false);
    w.push(
        Dir::FromEngine,
        &response(
            3,
            json!({"stopReason": "end_turn", "usage": {"totalTokens": 5}}),
        ),
        false,
    );
}

fn kinds(recs: &[(String, Value)]) -> Vec<String> {
    recs.iter()
        .map(|(_, r)| r["kind"].as_str().unwrap().to_string())
        .collect()
}

fn entry(
    s: &TranscriptSummary,
    k: CompletenessKind,
) -> Option<&harness_protocol::transcript::CompletenessEntry> {
    s.entries.iter().find(|e| e.kind == k)
}

// ---- a normal run -------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_session_is_stored_as_numbered_chunks_the_last_of_which_says_it_is_closed() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    session(&w);
    let summary = w.finish().await;
    let chunks = read_all(&store);
    assert_eq!(
        chunks.iter().map(|c| c.0).collect::<Vec<_>>(),
        [0, 1],
        "the answer of the prompt ended an object, the final flush wrote the end"
    );
    let recs = all_records(&store);
    assert_eq!(
        kinds(&recs),
        [
            "opaque",
            "opaque",
            "opaque",
            "session",
            "prompt",
            "agent_message_chunk",
            "tool_call",
            "tool_call_update",
            "prompt_result",
            "end"
        ]
    );
    let ns: Vec<u64> = recs.iter().map(|(_, r)| r["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, (0..10).collect::<Vec<u64>>());
    assert!(recs.iter().all(|(inc, _)| inc == ULID_A));
    assert!(chunks[0].1.get("closed").is_none());
    assert_eq!(chunks[1].1["closed"], true);
    assert_eq!(chunks[1].1["engine"], "Engine");
    assert_eq!(chunks[1].1["protocol"], "1.38.0");
    assert_eq!(
        (
            chunks[0].1["firstRec"].as_u64(),
            chunks[0].1["lastRec"].as_u64()
        ),
        (Some(0), Some(8))
    );
    assert_eq!(
        (
            chunks[1].1["firstRec"].as_u64(),
            chunks[1].1["lastRec"].as_u64()
        ),
        (Some(9), Some(9))
    );
    assert_eq!(summary.last_transcript_seq, Some(1));
    assert!(
        summary.is_lossless() || summary.entries.is_empty(),
        "{summary:?}"
    );
    assert!(summary.validate());
}

#[tokio::test(start_paused = true)]
async fn the_end_of_a_conversation_with_text_still_pending_is_one_final_object() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    w.push(
        Dir::FromEngine,
        &chunk("a last thought spoken aloud"),
        false,
    );
    let s = w.finish().await;
    let recs = all_records(&store);
    assert_eq!(kinds(&recs), ["agent_message_chunk", "end"]);
    assert_eq!(
        recs[0].1["data"]["content"]["text"],
        "a last thought spoken aloud"
    );
    assert_eq!(read_all(&store).len(), 1);
    assert_eq!(s.last_transcript_seq, Some(0));
}

#[tokio::test(start_paused = true)]
async fn an_empty_run_still_ends_with_an_end_record_so_a_restart_can_tell() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    let s = w.finish().await;
    let chunks = read_all(&store);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].1["closed"], true);
    assert_eq!(kinds(&all_records(&store)), ["end"]);
    assert_eq!(s.last_transcript_seq, Some(0));
}

#[tokio::test(start_paused = true)]
async fn an_object_is_written_when_its_records_pass_the_byte_limit_and_the_numbers_run_on() {
    let store = Arc::new(MemStore::new());
    let w = start_with(
        &store,
        ULID_A,
        Settings {
            flush_bytes: 1024,
            ..quick()
        },
    );
    for i in 0..60 {
        w.push(Dir::FromEngine, &tool_call(i), false);
    }
    settle().await;
    let before_end = read_all(&store);
    assert!(
        before_end.len() >= 5,
        "{} objects before the end",
        before_end.len()
    );
    w.finish().await;
    let chunks = read_all(&store);
    assert_eq!(
        chunks.iter().map(|c| c.0).collect::<Vec<_>>(),
        (0..chunks.len() as u64).collect::<Vec<_>>()
    );
    let mut next = 0u64;
    for (_, h, recs) in &chunks {
        assert_eq!(
            h["firstRec"].as_u64(),
            Some(next),
            "an object starts where the one before ended"
        );
        next = h["lastRec"].as_u64().unwrap() + 1;
        assert_eq!(recs.len() as u64, next - h["firstRec"].as_u64().unwrap());
    }
    assert_eq!(next, 61, "60 records and the end");
}

#[tokio::test(start_paused = true)]
async fn an_object_is_written_at_the_latest_thirty_seconds_after_its_first_record() {
    let store = Arc::new(MemStore::new());
    let w = start_with(
        &store,
        ULID_A,
        Settings {
            final_deadline: Duration::from_secs(3),
            ..Settings::default()
        },
    );
    w.push(Dir::FromEngine, &tool_call(1), false);
    pass(29).await;
    assert!(store.keys().is_empty(), "29 s: not yet");
    pass(2).await;
    assert_eq!(
        store.seqs(),
        [0],
        "31 s: written (the freshness bound is the flush interval plus 5 s)"
    );
    // the timer works without a new message: a second record, then silence
    w.push(Dir::FromEngine, &tool_call(2), false);
    pass(31).await;
    assert_eq!(store.seqs(), [0, 1]);
}

#[tokio::test(start_paused = true)]
async fn the_answer_to_a_prompt_and_a_cancel_write_an_object_at_once() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    w.push(
        Dir::ToEngine,
        &request(3, "session/prompt", json!({"sessionId": SID, "prompt": []})),
        false,
    );
    w.push(Dir::FromEngine, &tool_call(1), false);
    settle().await;
    assert!(store.keys().is_empty());
    w.push(
        Dir::ToEngine,
        &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": SID}}),
        false,
    );
    settle().await;
    assert_eq!(store.seqs(), [0], "the cancel ended an object");
    w.push(
        Dir::FromEngine,
        &response(3, json!({"stopReason": "cancelled"})),
        false,
    );
    settle().await;
    assert_eq!(store.seqs(), [0, 1], "and the answer of the prompt another");
    w.flush_now();
    w.push(Dir::FromEngine, &tool_call(2), false);
    w.flush_now();
    settle().await;
    assert_eq!(store.seqs(), [0, 1, 2], "flush_now writes what is pending");
}

// ---- a failing store never fails the run ---------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_store_that_always_fails_leaves_the_run_untouched_and_the_summary_says_what_was_lost() {
    let store = Arc::new(MemStore::new());
    store.fail_puts(u32::MAX);
    let w = start(&store);
    session(&w);
    let s = tokio::time::timeout(Duration::from_secs(30), w.finish())
        .await
        .expect("finish returns within its deadline");
    assert!(store.keys().is_empty());
    assert_eq!(s.last_transcript_seq, None);
    let failed = entry(&s, CompletenessKind::Failed).expect("a failed entry");
    assert_eq!(failed.count, Some(10), "the nine records and the end");
    assert!(s.validate());
    assert!(!s.is_lossless());
    // pushing after the end is harmless
    w.push(Dir::FromEngine, &tool_call(9), false);
}

#[tokio::test(start_paused = true)]
async fn after_a_failure_the_next_object_starts_with_a_gap_record_for_what_was_lost() {
    let store = Arc::new(MemStore::new());
    store.fail_puts(3); // the three tries of the first object
    let w = start(&store);
    session(&w);
    settle().await;
    pass(5).await;
    assert!(store.keys().is_empty(), "the first object was given up");
    w.push(Dir::FromEngine, &tool_call(2), false);
    let s = w.finish().await;
    let chunks = read_all(&store);
    assert_eq!(
        chunks.len(),
        1,
        "the sequence number was never used: the next object takes seq 0"
    );
    let recs = all_records(&store);
    assert_eq!(kinds(&recs), ["gap", "tool_call", "end"]);
    let gap = &recs[0].1;
    assert_eq!(gap["n"], 0, "the gap record has the first lost number");
    assert_eq!(gap["data"]["kind"], "failed");
    assert_eq!(gap["data"]["count"], 9);
    assert_eq!(recs[1].1["n"], 9);
    assert_eq!(
        (
            chunks[0].1["firstRec"].as_u64(),
            chunks[0].1["lastRec"].as_u64()
        ),
        (Some(0), Some(10))
    );
    assert_eq!(entry(&s, CompletenessKind::Failed).unwrap().count, Some(9));
    assert_eq!(s.last_transcript_seq, Some(0));
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_passes_is_retried_and_leaves_no_trace() {
    let store = Arc::new(MemStore::new());
    store.fail_puts(2);
    let w = start(&store);
    session(&w);
    let s = w.finish().await;
    assert_eq!(
        store.attempts_on(&key(0)),
        3,
        "two failures and the success"
    );
    assert_eq!(store.seqs(), [0, 1]);
    assert!(entry(&s, CompletenessKind::Failed).is_none());
    assert!(entry(&s, CompletenessKind::Conflict).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_lost_answer_is_not_a_conflict_the_writer_finds_its_own_object() {
    let store = Arc::new(MemStore::new());
    *store.ambiguous.lock().unwrap() = 1; // stored, but the answer was lost
    let w = start(&store);
    session(&w);
    let s = w.finish().await;
    assert_eq!(
        store.seqs(),
        [0, 1],
        "one object per chunk, no duplicate, no skipped number"
    );
    assert_eq!(store.attempts_on(&key(0)), 2);
    assert!(entry(&s, CompletenessKind::Conflict).is_none(), "{s:?}");
    assert!(entry(&s, CompletenessKind::Failed).is_none());
    read_all(&store);
}

// ---- keys the writer does not own ----------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_key_made_by_someone_else_is_skipped_and_the_summary_names_it() {
    let store = Arc::new(MemStore::new());
    store.put_raw(&key(0), b"another object".to_vec());
    let w = start(&store);
    session(&w);
    let s = w.finish().await;
    assert_eq!(store.seqs(), [0, 1, 2]);
    assert!(
        read_chunk(&store.objects.lock().unwrap()[&key(0)], 0).is_err(),
        "the object that was already there is still there and is not a chunk"
    );
    assert_eq!(
        store.objects.lock().unwrap()[&key(0)],
        b"another object".to_vec(),
        "never overwritten"
    );
    let c = entry(&s, CompletenessKind::Conflict).expect("a conflict entry");
    assert_eq!((c.from_seq, c.to_seq, c.count), (Some(0), Some(0), Some(1)));
    assert_eq!(s.last_transcript_seq, Some(2));
    // the records are whole
    let a = store.objects.lock().unwrap();
    let (h1, r1) = read_chunk(&a[&key(1)], 1).unwrap();
    let (h2, r2) = read_chunk(&a[&key(2)], 2).unwrap();
    assert_eq!(h1["firstRec"], 0);
    assert_eq!(r1.len() + r2.len(), 10);
    assert_eq!(h2["closed"], true);
}

#[tokio::test(start_paused = true)]
async fn a_run_of_taken_keys_costs_one_refused_put_not_one_per_key() {
    let store = Arc::new(MemStore::new());
    for s in 0..30 {
        store.put_raw(&key(s), b"junk".to_vec());
    }
    let w = start(&store);
    session(&w);
    let s = w.finish().await;
    assert_eq!(store.attempts_on(&key(0)), 1);
    for taken in 1..30 {
        assert_eq!(
            store.attempts_on(&key(taken)),
            0,
            "key {taken} was skipped by the listing"
        );
    }
    assert_eq!(store.attempts_on(&key(30)), 1);
    let c = entry(&s, CompletenessKind::Conflict).unwrap();
    assert_eq!((c.from_seq, c.to_seq), (Some(0), Some(29)));
    assert_eq!(c.count, Some(30));
}

#[tokio::test(start_paused = true)]
async fn the_writer_cannot_list_and_still_finds_a_free_key() {
    let store = Arc::new(MemStore::new());
    store
        .list_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for s in 0..3 {
        store.put_raw(&key(s), b"old".to_vec());
    }
    let w = start(&store);
    session(&w);
    let s = w.finish().await;
    assert_eq!(store.seqs(), [0, 1, 2, 3, 4]);
    let c = entry(&s, CompletenessKind::Conflict).unwrap();
    assert_eq!((c.from_seq, c.to_seq), (Some(0), Some(2)));
    for seq in [0, 1, 2] {
        assert_eq!(store.objects.lock().unwrap()[&key(seq)], b"old".to_vec());
    }
}

// ---- a restart: a new incarnation ---------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_writer_killed_mid_object_is_continued_by_a_new_incarnation_that_says_the_tail_is_unknown()
 {
    let store = Arc::new(MemStore::new());
    let a = start_with(&store, ULID_A, quick());
    session(&a); // the answer of the prompt ended object 0
    settle().await;
    a.push(Dir::FromEngine, &tool_call(2), false); // pending, never written
    a.push(Dir::FromEngine, &chunk("never stored"), false);
    settle().await;
    a.abort(); // the microVM is gone
    assert_eq!(store.seqs(), [0]);

    let b = start_with(&store, ULID_B, quick());
    b.push(Dir::FromEngine, &tool_call(3), false);
    let s = b.finish().await;
    assert_eq!(
        store.seqs(),
        [0, 1],
        "the new writer goes on after the highest valid object"
    );
    let chunks = read_all(&store);
    assert_eq!(chunks[0].1["incarnation"], ULID_A);
    assert_eq!(chunks[1].1["incarnation"], ULID_B);
    let recs: Vec<Value> = chunks[1].2.clone();
    assert_eq!(recs[0]["n"], 0, "numbers restart in a new incarnation");
    assert_eq!(recs[0]["kind"], "gap");
    assert_eq!(recs[0]["data"]["kind"], "tail-unknown");
    assert_eq!(recs[0]["data"]["afterSeq"], 0);
    assert_eq!(recs[1]["kind"], "tool_call");
    let t = entry(&s, CompletenessKind::TailUnknown).expect("tail-unknown");
    assert_eq!((t.from_seq, t.to_seq), (Some(0), Some(0)));
    assert!(
        !all_text(&store).contains("never stored"),
        "the tail of the killed writer is lost, and only that"
    );
}

fn all_text(store: &MemStore) -> String {
    all_records(store)
        .iter()
        .map(|(_, r)| r.to_string())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn after_a_clean_end_a_new_incarnation_starts_quietly() {
    let store = Arc::new(MemStore::new());
    let a = start_with(&store, ULID_A, quick());
    session(&a);
    a.finish().await;
    assert_eq!(store.seqs(), [0, 1]);
    let b = start_with(&store, ULID_B, quick());
    b.push(Dir::FromEngine, &tool_call(7), false);
    let s = b.finish().await;
    assert_eq!(store.seqs(), [0, 1, 2]);
    let (_, _, recs) = &read_all(&store)[2];
    assert_eq!(
        recs[0]["kind"], "tool_call",
        "no gap record: the first writer closed its last object"
    );
    assert_eq!(recs[0]["n"], 0);
    assert!(entry(&s, CompletenessKind::TailUnknown).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_taken_key_beyond_the_head_does_not_move_the_sequence() {
    let store = Arc::new(MemStore::new());
    let a = start_with(&store, ULID_A, quick());
    session(&a);
    settle().await;
    a.abort();
    // other objects already sit ahead of the writer: bytes that are not a chunk
    store.put_raw(&key(3), b"junk".to_vec());
    store.put_raw(&key(4), b"more junk".to_vec());
    let b = start_with(&store, ULID_B, quick());
    for i in 0..3 {
        b.push(Dir::FromEngine, &tool_call(i), false);
        b.flush_now();
        settle().await;
    }
    let s = b.finish().await;
    assert_eq!(
        store.attempts_on(&key(1)),
        1,
        "the writer resumed right after the highest valid object"
    );
    assert_eq!(store.attempts_on(&key(2)), 1);
    assert_eq!(store.attempts_on(&key(3)), 1, "and met the taken key once");
    assert_eq!(store.attempts_on(&key(4)), 0);
    let c = entry(&s, CompletenessKind::Conflict).unwrap();
    assert_eq!((c.from_seq, c.to_seq), (Some(3), Some(4)));
    assert_eq!(store.seqs(), [0, 1, 2, 3, 4, 5, 6]);
}

#[tokio::test(start_paused = true)]
async fn two_writers_that_start_from_the_same_head_both_complete_and_keep_every_record() {
    let store = Arc::new(MemStore::new());
    let a = start_with(&store, ULID_A, quick());
    let b = start_with(&store, ULID_B, quick());
    session(&a);
    session(&b);
    let (sa, sb) = tokio::join!(a.finish(), b.finish());
    let chunks = read_all(&store);
    assert_eq!(
        chunks.iter().map(|c| c.0).collect::<Vec<_>>(),
        (0..chunks.len() as u64).collect::<Vec<_>>(),
        "no hole in the sequence"
    );
    let recs = all_records(&store);
    for inc in [ULID_A, ULID_B] {
        let ns: Vec<u64> = recs
            .iter()
            .filter(|(i, _)| i == inc)
            .map(|(_, r)| r["n"].as_u64().unwrap())
            .collect();
        assert_eq!(ns, (0..10).collect::<Vec<u64>>(), "{inc} lost nothing");
    }
    let conflicts: u64 = [&sa, &sb]
        .iter()
        .filter_map(|s| entry(s, CompletenessKind::Conflict))
        .filter_map(|c| c.count)
        .sum();
    assert!(
        conflicts >= 1,
        "the loser met the winner's key: {sa:?} {sb:?}"
    );
    assert!(sa.validate() && sb.validate());
}

// ---- bounds ----------------------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_stalled_store_does_not_hold_up_capture_and_the_buffers_stay_bounded() {
    let store = Arc::new(MemStore::new());
    *store.delay.lock().unwrap() = Duration::from_secs(3600);
    let s = Settings {
        queue_bytes: 1024 * 1024,
        ..quick()
    };
    let w = start_with(&store, ULID_A, s.clone());
    let big = "x".repeat(400_000);
    let mut largest = 0usize;
    for i in 0..80 {
        let m = tool_done(i, &big);
        largest = largest.max(serde_json::to_vec(&m).unwrap().len());
        w.push(Dir::FromEngine, &m, false); // returns at once, whatever the store does
        if i % 10 == 0 {
            settle().await;
        }
    }
    settle().await;
    assert!(
        w.queued_peak() <= s.queue_bytes + 2 * largest,
        "peak {} against {}",
        w.queued_peak(),
        s.queue_bytes
    );
    assert!(
        w.stats().shed > 0 || w.stats().truncated > 0,
        "something was shed or cut: {:?}",
        w.stats()
    );
    let summary = tokio::time::timeout(Duration::from_secs(60), w.finish())
        .await
        .expect("the final flush ends at its deadline");
    assert!(store.keys().is_empty(), "nothing was ever stored");
    assert!(summary.validate());
    assert!(
        entry(&summary, CompletenessKind::Shed).is_some()
            || entry(&summary, CompletenessKind::Failed).is_some(),
        "{summary:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_final_flush_gives_up_at_its_deadline_and_the_run_goes_on() {
    let store = Arc::new(MemStore::new());
    *store.delay.lock().unwrap() = Duration::from_secs(3600);
    let w = start_with(
        &store,
        ULID_A,
        Settings {
            final_deadline: Duration::from_secs(2),
            ..quick()
        },
    );
    session(&w);
    let before = tokio::time::Instant::now();
    let s = w.finish().await;
    let took = tokio::time::Instant::now() - before;
    assert!(took <= Duration::from_secs(4), "{took:?}");
    assert!(entry(&s, CompletenessKind::Failed).is_some(), "{s:?}");
    assert_eq!(s.last_transcript_seq, None);
}

#[tokio::test(start_paused = true)]
async fn a_deeply_nested_tool_payload_never_makes_a_chunk_the_reader_refuses() {
    // The reader refuses a whole chunk when one record nests deeper than 32 levels, and a stored object can never be rewritten.
    let store = Arc::new(MemStore::new());
    let w = start_with(&store, ULID_A, quick());
    session(&w);
    let mut deep = json!("bottom");
    for _ in 0..100 {
        deep = json!({ "k": deep });
    }
    w.push(
        Dir::FromEngine,
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": {
            "sessionUpdate": "tool_call", "toolCallId": "deep", "title": "run", "kind": "execute", "status": "pending", "rawInput": deep}}}),
        false,
    );
    let s = w.finish().await;
    let chunks = read_all(&store); // panics when an object breaks the reader's rules, depth included
    assert!(
        chunks
            .iter()
            .any(|c| c.2.iter().any(|r| r["data"]["toolCallId"] == "deep")),
        "the record is stored, cut"
    );
    assert!(entry(&s, CompletenessKind::Truncated).is_some(), "{s:?}");
}

#[tokio::test(start_paused = true)]
async fn twenty_minutes_of_a_busy_session_is_stored_whole_in_a_few_dozen_objects() {
    let store = Arc::new(MemStore::new());
    let w = start_with(&store, ULID_A, Settings::default());
    w.push(
        Dir::ToEngine,
        &request(
            3,
            "session/prompt",
            json!({"sessionId": SID, "prompt": [{"type": "text", "text": "build it"}]}),
        ),
        false,
    );
    let mut want = String::new();
    let mut tools = 0usize;
    for minute_half in 0..40 {
        // thirty seconds: 200 chunks of text and a tool call that prints 20 KB
        for c in 0..200 {
            let t = format!("{minute_half:02}-{c:03} ");
            want.push_str(&t);
            w.push(Dir::FromEngine, &chunk(&t), false);
        }
        if minute_half % 2 == 0 {
            tools += 1;
            w.push(Dir::FromEngine, &tool_call(tools), false);
            w.push(
                Dir::FromEngine,
                &tool_done(tools, &"line of test output\n".repeat(1000)),
                false,
            );
        }
        pass(30).await;
    }
    w.push(
        Dir::FromEngine,
        &response(3, json!({"stopReason": "end_turn"})),
        false,
    );
    let s = w.finish().await;
    let chunks = read_all(&store);
    assert!(
        // text that is not yet a record waits in the held window until a tool call ends the message, so an object is not cut on every
        // thirty seconds of text alone
        (20..=90).contains(&chunks.len()),
        "{} objects",
        chunks.len()
    );
    assert_eq!(
        chunks.iter().map(|c| c.0).collect::<Vec<_>>(),
        (0..chunks.len() as u64).collect::<Vec<_>>()
    );
    let recs = all_records(&store);
    let ns: Vec<u64> = recs.iter().map(|(_, r)| r["n"].as_u64().unwrap()).collect();
    assert_eq!(
        ns,
        (0..ns.len() as u64).collect::<Vec<_>>(),
        "every record once, in order"
    );
    let text: String = recs
        .iter()
        .filter(|(_, r)| r["kind"] == "agent_message_chunk")
        .map(|(_, r)| r["data"]["content"]["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(text, want);
    assert_eq!(
        recs.iter()
            .filter(|(_, r)| r["kind"] == "tool_call_update")
            .count(),
        tools
    );
    let total: usize = store.objects.lock().unwrap().values().map(Vec::len).sum();
    assert!(total < 1_000_000, "{total} bytes stored");
    assert!(
        store
            .objects
            .lock()
            .unwrap()
            .values()
            .all(|b| b.len() < 400_000)
    );
    assert!(s.validate() && s.entries.is_empty(), "{s:?}");
    assert!(w.queued_peak() < 2 * 1024 * 1024);
}

// ---- what the summary counts ---------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_summary_counts_what_was_left_out_and_what_was_cut() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    w.push(Dir::FromEngine, &thought("a private thought"), false);
    w.push(Dir::FromEngine, &thought("another"), false);
    w.push(Dir::FromEngine, &tool_done(1, &"y".repeat(400_000)), false);
    w.push(Dir::FromEngine, &tool_done(2, "ok"), false);
    let s = w.finish().await;
    assert_eq!(
        entry(&s, CompletenessKind::Excluded).unwrap().count,
        Some(2)
    );
    assert_eq!(
        entry(&s, CompletenessKind::Truncated).unwrap().count,
        Some(1)
    );
    assert!(entry(&s, CompletenessKind::Failed).is_none());
    assert!(!all_text(&store).contains("private thought"));
    assert!(
        !s.is_lossless(),
        "a cut record is a loss; thoughts alone are not"
    );
}

#[tokio::test(start_paused = true)]
async fn thoughts_alone_leave_a_summary_that_is_lossless() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    w.push(Dir::FromEngine, &thought("only thoughts"), false);
    w.push(Dir::FromEngine, &tool_call(1), false);
    let s = w.finish().await;
    assert!(s.is_lossless(), "{s:?}");
    assert_eq!(
        entry(&s, CompletenessKind::Excluded).unwrap().count,
        Some(1)
    );
}

#[tokio::test(start_paused = true)]
async fn session_secrets_added_while_the_run_goes_on_are_redacted() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    let secret = "wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY0123456789";
    w.push(
        Dir::FromEngine,
        &tool_done(1, &format!("before {secret}")),
        false,
    );
    w.add_secrets(&[secret.to_string()]);
    w.push(
        Dir::FromEngine,
        &tool_done(2, &format!("after {secret}")),
        false,
    );
    w.finish().await;
    let text = all_text(&store);
    assert!(!text.contains("after wJalr"), "{text}");
    assert!(text.contains("after <redacted>"));
}

#[tokio::test(start_paused = true)]
async fn a_message_of_an_abandoned_session_is_not_stored_and_finish_twice_is_the_same() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    w.push(Dir::FromEngine, &chunk("LATE CHATTER"), true);
    w.push(Dir::FromEngine, &tool_call(1), false);
    let a = w.finish().await;
    let b = w.finish().await;
    assert_eq!(a, b);
    assert!(!all_text(&store).contains("LATE CHATTER"));
    assert_eq!(store.seqs(), [0]);
}

#[tokio::test(start_paused = true)]
async fn the_end_record_and_the_stats_count_the_updates_that_came_outside_a_prompt() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    session(&w); // the answer of prompt 3 is the last message
    w.push(
        Dir::FromEngine,
        &update(json!({"sessionUpdate": "usage_update"})),
        false,
    );
    w.push(Dir::FromEngine, &chunk("a late word"), false);
    let s = w.finish().await;
    assert_eq!(w.stats().unattributed, 2);
    assert!(
        s.is_lossless(),
        "unattributed updates are stored, not lost: {s:?}"
    );
    let recs: Vec<Value> = read_all(&store).into_iter().flat_map(|c| c.2).collect();
    let end = recs.last().unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["data"]["unattributed"], 2);
    let late = recs.iter().find(|r| r["kind"] == "usage_update").unwrap();
    assert_eq!(late["turn_id"], Value::Null);
    let first = recs.iter().find(|r| r["kind"] == "prompt").unwrap();
    assert_eq!(first["turn_id"], json!(3));
}
