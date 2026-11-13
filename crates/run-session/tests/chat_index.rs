//! The chat-content index a finished run leaves for search: one search item per run, built after the
//! session is stopped from the objective, the user's answers and the stored transcript, bounded and redacted, and never able to fail
//! the run. The transcript prefix is one the engine can write, so every read is bounded and every object is treated as hostile.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::gzip::gzip_stored;
use ham_core::runsearch::{Coverage, chat_sk, owner_pk};
use ham_core::transcript::{
    CHUNK_VERSION, Chunk, ChunkHeader, ChunkRecord, Dir, MAX_CHUNK_OBJECT_BYTES, chunk_gzip_stored,
    chunk_key, seq_of_name, transcript_prefix,
};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::index::{IndexLimits, Indexed, MAX_CHUNK_READS, best_effort, index_run};
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use support::fakes::*;

const INC: &str = "01K83DA0A0A0A0A0A0A0A0A0A0";
const CREATED: &str = "2026-10-13T18:00:00Z";
const PROMPT: &str = "Add percent_change(a, b) with tests";

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn started() -> Mem {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct."),
        true,
    ));
    call(
        &mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
    .unwrap();
    mem.clear_events();
    mem
}

async fn plan(mem: &Mem) -> Value {
    call(mem, json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": {"turn": turn_output(true), "failure": {}}}))
        .await
        .unwrap()
}

async fn finalize(mem: &Mem, plan: &Value) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "finalize", "runId": RUN, "plan": plan, "validated": {"pass": true},
            "packaged": {"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096},
            "prLink": {}, "prFailure": {}}),
    )
    .await
}

/// A run whose image writes a transcript, with the Python-made `good` chunks in the bucket.
async fn with_transcript(scenario: &str) -> Mem {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    load(&mem, scenario);
    mem
}

fn load(mem: &Mem, scenario: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../ham-core/tests/fixtures/transcript")
        .join(scenario);
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_str().unwrap().to_string();
        let Some(name) = file.strip_suffix(".hex") else {
            continue;
        };
        let bytes = ham_core::gzip::unhex(&std::fs::read_to_string(&path).unwrap()).unwrap();
        mem.put_blob(&chunk_key(RUN, seq_of_name(name).unwrap()), bytes);
    }
}

fn message(n: u64, text: &str) -> ChunkRecord {
    ChunkRecord {
        n,
        t: None,
        dir: Dir::In,
        kind: "agent_message_chunk".into(),
        data: json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}, "messageId": format!("m{n}")}),
    }
}

fn chunk_object(seq: u64, records: Vec<ChunkRecord>) -> Vec<u8> {
    let (first, last) = (
        records.first().map_or(0, |r| r.n),
        records.last().map_or(0, |r| r.n),
    );
    chunk_gzip_stored(&Chunk {
        header: ChunkHeader {
            v: CHUNK_VERSION,
            seq,
            incarnation: INC.into(),
            first_rec: first,
            last_rec: last,
            created_at: "2026-10-16T10:00:00.000Z".into(),
            engine: "cline".into(),
            protocol: "1.38.0".into(),
        },
        records,
    })
}

fn writes(mem: &Mem) -> Vec<SearchRecord> {
    mem.0.search_writes.lock().unwrap().clone()
}

// ---- what is written -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_finished_run_leaves_one_search_item_with_its_objective_answers_and_conversation() {
    let mem = with_transcript("good").await;
    mem.0
        .answers
        .lock()
        .unwrap()
        .push("use pytest, not unittest".into());
    let done = finalize(&mem, &plan(&mem).await).await.unwrap();
    assert_eq!(done["status"], "complete");
    assert_eq!(mem.run().status, GoalStatus::Complete);
    let w = writes(&mem);
    assert_eq!(w.len(), 1, "one item per run");
    let w = &w[0];
    assert_eq!(w.pk, owner_pk(OWNER));
    assert_eq!(w.sk, chat_sk(CREATED, RUN));
    // The body is not in the run index's item: the run list reads those whole, and a 32 KiB body in each would make it read megabytes.
    assert!(
        w.sk.starts_with("CHAT#") && !w.sk.starts_with("SRCH#"),
        "{}",
        w.sk
    );
    assert_eq!((w.run_id.as_str(), w.created_at.as_str()), (RUN, CREATED));
    assert_eq!(
        w.body.body.lines().collect::<Vec<_>>(),
        vec![
            PROMPT,
            "use pytest, not unittest",
            "I will start with the tests.",
            "Edit tests/test_calc.py",
            "The tests fail as expected. Now the code.",
            "Edit src/calc.py",
            "Run pytest tests",
        ]
    );
    assert_eq!((w.body.coverage, w.body.through), (Coverage::Full, Some(2)));
    assert_eq!(
        (w.body.segs[2].seq, w.body.segs[2].rec),
        (Some(0), 1),
        "the place of a message in the transcript"
    );
    assert!(w.indexed_at.starts_with("20"), "{}", w.indexed_at);
    // It listed the transcript prefix, nothing else, and read only the chunks it listed.
    let lists = mem.0.lists.lock().unwrap().clone();
    assert!(
        !lists.is_empty()
            && lists
                .iter()
                .all(|(p, _, n)| *p == transcript_prefix(RUN) && (1..=1000).contains(n)),
        "{lists:?}"
    );
    let gets = mem.0.bounded_gets.lock().unwrap().clone();
    assert_eq!(gets.len(), 3);
    assert!(
        gets.iter().all(
            |(k, max)| k.starts_with(&transcript_prefix(RUN)) && *max == MAX_CHUNK_OBJECT_BYTES
        )
    );
}

#[tokio::test]
async fn the_owner_partition_comes_from_the_run_item_and_a_repeat_writes_the_same_item() {
    let mem = with_transcript("good").await;
    let p = plan(&mem).await;
    finalize(&mem, &p).await.unwrap();
    finalize(&mem, &p).await.unwrap();
    let w = writes(&mem);
    assert_eq!(w.len(), 2, "a retried finalize indexes again");
    assert_eq!(
        (w[0].pk.as_str(), w[0].sk.as_str(), &w[0].body),
        (w[1].pk.as_str(), w[1].sk.as_str(), &w[1].body)
    );
    // Another owner's run lands in another partition.
    let other = with_transcript("good").await;
    other
        .0
        .runs
        .lock()
        .unwrap()
        .get_mut(RUN)
        .unwrap()
        .owner_principal_arn = "arn:aws:sts::700000000011:assumed-role/Admin/someone".into();
    finalize(&other, &plan(&other).await).await.unwrap();
    assert_ne!(writes(&other)[0].pk, w[0].pk);
    assert_eq!(
        writes(&other)[0].pk,
        owner_pk("arn:aws:sts::700000000011:assumed-role/Admin/someone")
    );
}

#[tokio::test]
async fn a_blocked_run_is_indexed_too_and_a_discarded_one_is_not() {
    let mem = with_transcript("good").await;
    call(&mem, json!({"op": "mark-blocked", "runId": RUN, "state": {"failure": {"Error": "States.TaskFailed", "Cause": "boom"}}})).await.unwrap();
    assert_eq!(mem.run().status, GoalStatus::Blocked);
    assert_eq!(writes(&mem).len(), 1);
    let cleared = with_transcript("good").await;
    let p = plan(&cleared).await;
    cleared.0.runs.lock().unwrap().get_mut(RUN).unwrap().status = GoalStatus::Cleared;
    finalize(&cleared, &p).await.unwrap();
    assert_eq!(
        writes(&cleared).len(),
        0,
        "the work of a discarded run is not published"
    );
    assert_eq!(
        index_run(&cleared.deps(), RUN, &IndexLimits::default()).await,
        Ok(Indexed::Skipped("cleared"))
    );
}

#[tokio::test]
async fn a_run_whose_image_writes_no_transcript_is_left_to_the_run_index_and_costs_no_bucket_call()
{
    let mem = started().await;
    load(&mem, "good");
    let p = plan(&mem).await;
    let with_flag_off = finalize(&mem, &p).await.unwrap();
    assert_eq!(writes(&mem).len(), 0);
    assert!(
        mem.0.lists.lock().unwrap().is_empty() && mem.0.bounded_gets.lock().unwrap().is_empty()
    );
    assert_eq!(
        index_run(&mem.deps(), RUN, &IndexLimits::default()).await,
        Ok(Indexed::Skipped("unsupported"))
    );
    // The answer of finalize is what it was before the index existed.
    let on = with_transcript("good").await;
    let with_flag_on = finalize(&on, &plan(&on).await).await.unwrap();
    assert_eq!(with_flag_off, with_flag_on);
    let missing = Mem::default();
    assert_eq!(
        index_run(&missing.deps(), RUN, &IndexLimits::default()).await,
        Ok(Indexed::Skipped("no-run"))
    );
}

// ---- a failure never reaches the run ---------------------------------------------------------------------------------

#[tokio::test]
async fn no_failure_of_the_index_changes_how_a_run_ends() {
    for call_name in ["transcript_supported", "list", "put_search_body"] {
        let mem = with_transcript("good").await;
        mem.fail(
            call_name,
            RunSessionError::Unavailable(format!("SECRET-DETAIL {call_name}")),
        );
        let done = finalize(&mem, &plan(&mem).await).await.unwrap();
        assert_eq!(done["status"], "complete", "{call_name}");
        assert_eq!(mem.run().status, GoalStatus::Complete);
        assert_eq!(writes(&mem).len(), 0, "{call_name}");
    }
    // The answers are a convenience, and a bucket that refuses every read still leaves the objective searchable, partly.
    let mem = with_transcript("good").await;
    mem.fail("user_answers", RunSessionError::Unavailable("x".into()));
    for _ in 0..3 {
        mem.fail("get_bounded", RunSessionError::Unavailable("y".into()));
    }
    finalize(&mem, &plan(&mem).await).await.unwrap();
    let w = writes(&mem);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].body.body, format!("{PROMPT}\n"));
    assert_eq!(w[0].body.coverage, Coverage::Partial);
}

#[tokio::test]
async fn the_report_names_the_outcome_and_never_the_conversation() {
    let mem = with_transcript("good").await;
    mem.0
        .answers
        .lock()
        .unwrap()
        .push("my private answer".into());
    let r = best_effort(&mem.deps(), RUN).await;
    assert_eq!(
        (r.outcome, r.reason.clone(), r.chunks_read),
        ("written", None, 3)
    );
    let line = r.line(RUN);
    for text in [PROMPT, "private", "percent_change", "tests"] {
        assert!(!line.contains(text), "{line}");
    }
    assert!(
        line.contains(RUN) && line.contains("\"outcome\":\"written\""),
        "{line}"
    );
    let bad = with_transcript("good").await;
    bad.fail("list", RunSessionError::Unavailable("SECRET-DETAIL".into()));
    let r = best_effort(&bad.deps(), RUN).await;
    assert_eq!(
        (r.outcome, r.reason.as_deref()),
        ("failed", Some("RunSessionUnavailable"))
    );
    assert!(!r.line(RUN).contains("SECRET-DETAIL"));
}

// ---- hostile transcripts ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_long_transcript_costs_a_bounded_number_of_reads_and_keeps_its_start_and_its_end() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    let filler = "word ".repeat(150);
    for seq in 0..600u64 {
        let text = match seq {
            0 => format!("FIRST-MARKER {filler}"),
            599 => format!("LAST-MARKER {filler}"),
            300 => format!("MIDDLE-MARKER {filler}"),
            _ => format!("message {seq} {filler}"),
        };
        mem.put_blob(
            &chunk_key(RUN, seq),
            chunk_object(seq, vec![message(seq, &text)]),
        );
    }
    let r = index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap();
    let Indexed::Written {
        coverage,
        bytes,
        chunks_read,
    } = r
    else {
        panic!("{r:?}")
    };
    assert_eq!(coverage, Coverage::Partial);
    assert!(bytes <= 32 * 1024 && bytes > 30 * 1024, "{bytes}");
    assert!(chunks_read < 80, "{chunks_read} reads for 600 chunks");
    assert!(chunks_read <= MAX_CHUNK_READS);
    assert_eq!(mem.0.bounded_gets.lock().unwrap().len(), chunks_read);
    let body = &writes(&mem)[0].body;
    assert!(
        body.body.contains("FIRST-MARKER")
            && body.body.contains("LAST-MARKER")
            && !body.body.contains("MIDDLE-MARKER")
    );
    assert_eq!(body.through, Some(599));
}

#[tokio::test]
async fn the_read_limit_and_the_time_limit_end_the_reads_and_the_body_is_still_written() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    for seq in 0..50u64 {
        mem.put_blob(
            &chunk_key(RUN, seq),
            chunk_object(seq, vec![message(seq, &format!("short message {seq}"))]),
        );
    }
    let few = IndexLimits {
        max_reads: 5,
        ..IndexLimits::default()
    };
    let Indexed::Written {
        coverage,
        chunks_read,
        ..
    } = index_run(&mem.deps(), RUN, &few).await.unwrap()
    else {
        panic!()
    };
    assert_eq!((coverage, chunks_read), (Coverage::Partial, 5));
    let none = IndexLimits {
        read_budget: Duration::ZERO,
        ..IndexLimits::default()
    };
    let before = mem.0.bounded_gets.lock().unwrap().len();
    let Indexed::Written { chunks_read, .. } = index_run(&mem.deps(), RUN, &none).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(chunks_read, 0);
    assert_eq!(
        mem.0.bounded_gets.lock().unwrap().len(),
        before,
        "no read once the budget is spent"
    );
    assert_eq!(
        writes(&mem).last().unwrap().body.body,
        format!("{PROMPT}\n")
    );
}

#[tokio::test]
async fn an_object_over_the_size_cap_is_never_fetched_and_a_junk_key_is_never_read() {
    let mem = with_transcript("good").await;
    let prefix = transcript_prefix(RUN);
    // A chunk-shaped key whose listing size is 2 GiB (the bytes are small: the listing is what a reader sees first).
    mem.put_blob(
        &chunk_key(RUN, 3),
        chunk_object(3, vec![message(13, "HOSTILE-HUGE")]),
    );
    mem.0
        .listed_sizes
        .lock()
        .unwrap()
        .insert(chunk_key(RUN, 3), 2 * 1024 * 1024 * 1024);
    for name in [
        "seal",
        "manifest.json",
        "junk-1",
        "sub/000000000004.ndjson.gz",
        "000000000005.ndjson.gz.tmp",
    ] {
        mem.put_blob(&format!("{prefix}{name}"), b"{\"sealed\":true}".to_vec());
    }
    // An object that is not a chunk at all.
    mem.put_blob(&chunk_key(RUN, 6), b"this is not gzip".to_vec());
    let Indexed::Written { coverage, .. } = index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        coverage,
        Coverage::Partial,
        "an unreadable chunk makes the body partial"
    );
    let gets: Vec<String> = mem
        .0
        .bounded_gets
        .lock()
        .unwrap()
        .iter()
        .map(|(k, _)| k.clone())
        .collect();
    assert!(
        !gets.contains(&chunk_key(RUN, 3)),
        "the 2 GiB object is not fetched"
    );
    assert!(
        gets.iter()
            .all(|k| seq_of_name(k.strip_prefix(&prefix).unwrap()).is_some()),
        "no junk key is read: {gets:?}"
    );
    assert!(!writes(&mem)[0].body.body.contains("HOSTILE-HUGE"));
    // An object one byte over the cap is not fetched either.
    let big = chunk_key(RUN, 7);
    mem.put_blob(&big, vec![0u8; MAX_CHUNK_OBJECT_BYTES + 1]);
    index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap();
    assert!(
        mem.0
            .bounded_gets
            .lock()
            .unwrap()
            .iter()
            .all(|(k, _)| *k != big)
    );
}

#[tokio::test]
async fn a_flood_of_junk_keys_is_listed_in_pages_up_to_the_cap_and_the_body_says_it_is_partial() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    let prefix = transcript_prefix(RUN);
    for i in 0..3000 {
        mem.put_blob(&format!("{prefix}junk-{i:05}"), b"x".to_vec());
    }
    mem.put_blob(
        &chunk_key(RUN, 0),
        chunk_object(0, vec![message(0, "the only real chunk")]),
    );
    let limits = IndexLimits {
        max_keys: 2000,
        ..IndexLimits::default()
    };
    let Indexed::Written { coverage, .. } = index_run(&mem.deps(), RUN, &limits).await.unwrap()
    else {
        panic!()
    };
    // Junk sorts after the chunk key ("0..." before "j..."), so the chunk is listed first; the cap then ends the listing.
    assert_eq!(coverage, Coverage::Partial);
    let lists = mem.0.lists.lock().unwrap().clone();
    assert_eq!(lists.len(), 2, "two list calls of at most 1,000 keys");
    assert!(lists.iter().all(|(_, _, n)| *n <= 1000));
    assert!(writes(&mem)[0].body.body.contains("the only real chunk"));
}

// ---- what enters the body --------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_secret_in_a_chunk_the_objective_or_an_answer_never_reaches_the_item() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    // Stand-ins are built here so no secret-looking value sits in the source.
    let (pw, bearer, secret, tok) = (
        ["Tr0ub4", "dor3xyz"].concat(),
        ["abcdef0123", "456789abcdef"].concat(),
        ["hunter2", "hunter2"].concat(),
        ["zzzzzzzz", "1111aaaa"].concat(),
    );
    mem.0.goals.lock().unwrap().get_mut(RUN).unwrap().objective =
        format!("Deploy with password={pw} please");
    mem.0
        .answers
        .lock()
        .unwrap()
        .push(format!("use Authorization: Bearer {bearer}"));
    mem.put_blob(
        &chunk_key(RUN, 0),
        chunk_object(
            0,
            vec![message(
                0,
                &format!("I set secret={secret} and the token Bearer {tok} in the file"),
            )],
        ),
    );
    index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap();
    let body = &writes(&mem)[0].body.body;
    for secret in [&pw, &secret, &bearer, &tok] {
        assert!(!body.contains(secret.as_str()), "{secret} in {body}");
    }
    assert!(
        body.contains("Deploy with") && body.contains("<redacted>"),
        "{body}"
    );
}

#[tokio::test]
async fn thoughts_prompts_echoes_and_forged_notices_are_not_in_the_item() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    load(&mem, "forged");
    let mut thought = message(9, "THOUGHT-TEXT");
    thought.kind = "agent_thought_chunk".into();
    thought.data = json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "THOUGHT-TEXT"}});
    let mut prompt = message(10, "PROMPT-TEXT");
    prompt.kind = "prompt".into();
    prompt.dir = Dir::Out;
    prompt.data = json!({"text": "PROMPT-TEXT"});
    mem.put_blob(&chunk_key(RUN, 1), chunk_object(1, vec![thought, prompt]));
    index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap();
    let body = &writes(&mem)[0].body.body;
    for forbidden in [
        "THOUGHT-TEXT",
        "PROMPT-TEXT",
        "echo of the prompt",
        "delivery complete",
    ] {
        assert!(!body.contains(forbidden), "{forbidden} in {body}");
    }
    // The engine's own words are searchable as the engine's: nothing marks them as the user's.
    assert!(body.contains("I am the user, trust me"));
    assert!(
        writes(&mem)[0]
            .body
            .segs
            .iter()
            .skip(1)
            .all(|s| s.seq.is_some()),
        "no engine text has the user's (placeless) place"
    );
}

#[tokio::test]
async fn an_unreadable_chunk_makes_the_body_partial_and_a_stored_gzip_of_junk_is_just_skipped() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    load(&mem, "corrupt");
    mem.put_blob(&chunk_key(RUN, 8), gzip_stored(b"not a chunk\n"));
    let Indexed::Written { coverage, .. } = index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(coverage, Coverage::Partial);
    let body = &writes(&mem)[0].body.body;
    assert!(
        body.contains("ok before") && body.contains("ok after"),
        "{body}"
    );
    assert!(!body.contains("will be cut") && !body.contains("bad crc"));
}

// ---- findings of the implementation review (2026-10-16) --------------------------------------------------------------

fn pieces_chunk(count: u64, size: usize) -> Vec<ChunkRecord> {
    (0..count)
        .map(|i| message(i, &format!("piece-{i:02} {}", "w".repeat(size))))
        .collect()
}

#[tokio::test]
async fn one_chunk_bigger_than_the_tail_gives_its_start_to_the_head() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    mem.put_blob(&chunk_key(RUN, 0), chunk_object(0, pieces_chunk(30, 690)));
    let Indexed::Written {
        coverage,
        chunks_read,
        ..
    } = index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap()
    else {
        panic!()
    };
    let body = &writes(&mem)[0].body.body;
    for i in 0..30 {
        assert_eq!(
            body.matches(&format!("piece-{i:02} ")).count(),
            1,
            "piece {i} once"
        );
    }
    assert_eq!(
        coverage,
        Coverage::Full,
        "head and tail together hold it all"
    );
    assert_eq!(
        chunks_read, 1,
        "the chunk the tail stopped inside is read once and its pieces serve the head too"
    );
    assert_eq!(mem.0.bounded_gets.lock().unwrap().len(), 1);
    // A chunk bigger than head and tail together loses its middle, says so, and repeats nothing.
    let big = started().await;
    *big.0.transcript_supported.lock().unwrap() = true;
    big.put_blob(&chunk_key(RUN, 0), chunk_object(0, pieces_chunk(70, 690)));
    let Indexed::Written { coverage, .. } = index_run(&big.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap()
    else {
        panic!()
    };
    let body = &writes(&big)[0].body.body;
    assert!(
        body.contains("piece-00 ") && body.contains("piece-69 ") && !body.contains("piece-35 ")
    );
    assert!(body.contains(ham_core::runsearch::GAP_MARKER));
    assert_eq!(coverage, Coverage::Partial);
    assert_eq!(body.matches("piece-69 ").count(), 1);
}

#[tokio::test]
async fn a_hole_in_the_chunk_numbers_or_a_missing_start_makes_the_body_partial() {
    for (name, seqs) in [
        ("a hole", vec![0u64, 1, 3]),
        ("a missing start", vec![1, 2]),
        ("a lone late chunk", vec![4]),
    ] {
        let mem = started().await;
        *mem.0.transcript_supported.lock().unwrap() = true;
        for (i, seq) in seqs.iter().enumerate() {
            mem.put_blob(
                &chunk_key(RUN, *seq),
                chunk_object(*seq, vec![message(i as u64, &format!("text {seq}"))]),
            );
        }
        let Indexed::Written { coverage, .. } =
            index_run(&mem.deps(), RUN, &IndexLimits::default())
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(coverage, Coverage::Partial, "{name}");
    }
}

#[tokio::test]
async fn a_secret_split_across_the_fragments_of_a_message_is_redacted_in_the_joined_text() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    let (front, back) = ("Tr0ub", "4dor3xyz");
    let mut a = message(0, &format!("please set password={front}"));
    let mut b = message(1, &format!("{back} in the file"));
    a.data["messageId"] = json!("m-split");
    b.data["messageId"] = json!("m-split");
    mem.put_blob(&chunk_key(RUN, 0), chunk_object(0, vec![a, b]));
    index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap();
    let body = &writes(&mem)[0].body.body;
    assert!(
        !body.contains(&[front, back].concat()) && !body.contains("Tr0ub4") && !body.contains(back),
        "{body}"
    );
    assert!(
        body.contains("please set password=") && body.contains("in the file"),
        "{body}"
    );
}

#[tokio::test]
async fn huge_message_ids_and_floods_of_distinct_tool_lines_do_not_pile_up() {
    let mem = started().await;
    *mem.0.transcript_supported.lock().unwrap() = true;
    // 20 chunks, each with a 900 KB message id: nothing retains it.
    for seq in 0..20u64 {
        let mut r = message(0, "x");
        r.data["messageId"] = json!("i".repeat(900_000));
        mem.put_blob(&chunk_key(RUN, seq), chunk_object(seq, vec![r]));
    }
    let Indexed::Written { bytes, .. } = index_run(&mem.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(bytes > 0 && bytes <= 32 * 1024);
    // 3 chunks of 5,000 distinct tool lines: the body is still bounded and still holds the ends.
    let flood = started().await;
    *flood.0.transcript_supported.lock().unwrap() = true;
    for seq in 0..3u64 {
        let records: Vec<ChunkRecord> = (0..5000u64)
            .map(|i| ChunkRecord {
                n: i,
                t: None,
                dir: Dir::In,
                kind: "tool_call".into(),
                data: json!({"sessionUpdate": "tool_call", "title": format!("Edit file-{seq}-{i:05}.rs"), "toolCallId": format!("c{seq}-{i}")}),
            })
            .collect();
        flood.put_blob(&chunk_key(RUN, seq), chunk_object(seq, records));
    }
    index_run(&flood.deps(), RUN, &IndexLimits::default())
        .await
        .unwrap();
    let body = &writes(&flood)[0].body;
    assert!(body.body.len() <= 32 * 1024);
    assert!(body.body.contains("file-0-00000.rs") && body.body.contains("file-2-04999.rs"));
}

fn chunk_object_of(seq: u64, incarnation: &str, records: Vec<ChunkRecord>) -> Vec<u8> {
    let (first, last) = (
        records.first().map_or(0, |r| r.n),
        records.last().map_or(0, |r| r.n),
    );
    chunk_gzip_stored(&Chunk {
        header: ChunkHeader {
            v: CHUNK_VERSION,
            seq,
            incarnation: incarnation.into(),
            first_rec: first,
            last_rec: last,
            created_at: "2026-10-16T10:00:00.000Z".into(),
            engine: "cline".into(),
            protocol: "1.38.0".into(),
        },
        records,
    })
}

fn gap_record(n: u64) -> ChunkRecord {
    ChunkRecord {
        n,
        t: None,
        dir: Dir::In,
        kind: "gap".into(),
        data: json!({}),
    }
}

#[tokio::test]
async fn record_numbers_that_skip_between_two_chunks_make_the_body_partial_unless_something_explains_them()
 {
    const OTHER: &str = "01K83DB0B0B0B0B0B0B0B0B0B0";
    type Case = (
        &'static str,
        Vec<ChunkRecord>,
        Vec<ChunkRecord>,
        &'static str,
        Coverage,
    );
    let cases: Vec<Case> = vec![
        (
            "the same writer goes on",
            vec![message(0, "a"), message(1, "b")],
            vec![message(2, "c")],
            INC,
            Coverage::Full,
        ),
        (
            "one number is missing",
            vec![message(0, "a"), message(1, "b")],
            vec![message(3, "c")],
            INC,
            Coverage::Partial,
        ),
        (
            "a gap record ends the first chunk",
            vec![message(0, "a"), gap_record(1)],
            vec![message(4, "c")],
            INC,
            Coverage::Full,
        ),
        (
            "a gap record starts the next",
            vec![message(0, "a"), message(1, "b")],
            vec![gap_record(3), message(4, "c")],
            INC,
            Coverage::Full,
        ),
        (
            "a new writer starts at 0",
            vec![message(0, "a"), message(1, "b")],
            vec![message(0, "c")],
            OTHER,
            Coverage::Full,
        ),
        (
            "a new writer starts at 4",
            vec![message(0, "a"), message(1, "b")],
            vec![message(4, "c")],
            OTHER,
            Coverage::Partial,
        ),
    ];
    for (name, first, second, second_inc, want) in cases {
        let mem = started().await;
        *mem.0.transcript_supported.lock().unwrap() = true;
        mem.put_blob(&chunk_key(RUN, 0), chunk_object_of(0, INC, first));
        mem.put_blob(&chunk_key(RUN, 1), chunk_object_of(1, second_inc, second));
        let Indexed::Written { coverage, .. } =
            index_run(&mem.deps(), RUN, &IndexLimits::default())
                .await
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(coverage, want, "{name}");
    }
}
