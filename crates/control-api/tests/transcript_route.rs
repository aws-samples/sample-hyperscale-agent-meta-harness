//! `GET /v1/runs/{id}/transcript` against an in-memory bucket that refuses what S3 and the
//! control-api role's policy refuse (`tests/support`). The chunks are the Python-made files of `crates/ham-core/tests/fixtures/transcript`
//! and objects built here. Every response is read back as the client reads it: through `ham_core::transcript::parse_page`.
#![allow(clippy::unwrap_used)]

mod support;

use control_api::cursor::{self, Position};
use control_api::handler::{Deps, V1Request, handle_logged};
use control_api::transcript::{Limits, SummaryFact, TranscriptFacts, transcript_with};
use ham_core::transcript::{
    CHUNK_VERSION, Chunk, ChunkHeader, ChunkRecord, CompletenessEntry, Dir, FIRST_SEQ, GapCode,
    GapKind, MAX_CHUNKS_PER_REQUEST, Origin, TranscriptPage, TranscriptState, TranscriptSummary,
    chunk_gzip_stored, chunk_key, parse_page, seq_of_name, transcript_prefix,
};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::RunItem;
use harness_protocol::target::RunTarget;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use support::{Access, Bucket, NoExecution, Tables};

const ME: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const YOU: &str = "arn:aws:sts::800000000022:assumed-role/Admin/someone";
const RUN: &str = "01K83D1111111111111111111A";
const RUN_B: &str = "01K83D1111111111111111111B";
const INC_A: &str = "01K83DA0A0A0A0A0A0A0A0A0A0";

fn run_item(id: &str, owner: &str, status: GoalStatus) -> RunItem {
    RunItem {
        run_id: id.into(),
        owner_principal_arn: owner.into(),
        prompt_sha256: "cd".repeat(32),
        target: RunTarget::S3Only,
        base_ref: None,
        status,
        phase: GoalPhase::Planning,
        budget: None,
        deployment_digest: None,
        source_tree: None,
        engine_policy_version: 1,
        created_at: "2026-10-16T10:00:00.000Z".into(),
        version: 1,
        runtime_version: None,
        runtime_endpoint: None,
        aborted: None,
        continue_from: None,
        status_reason: None,
        resume_mode: None,
        slice: None,
        max_run_s: None,
        engine: None,
    }
}

fn supported() -> TranscriptFacts {
    TranscriptFacts {
        supported: true,
        purged: false,
        summary: SummaryFact::Absent,
    }
}

/// A table with one run of mine in `status` whose image writes a transcript.
fn tables(status: GoalStatus) -> Tables {
    let t = Tables::default();
    t.runs
        .lock()
        .unwrap()
        .insert(RUN.into(), run_item(RUN, ME, status));
    t.facts.lock().unwrap().insert(RUN.into(), supported());
    t
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ham-core/tests/fixtures/transcript")
}

/// Puts the chunk files of a scenario where the writer puts them.
fn load(b: &Bucket, run: &str, scenario: &str) {
    let dir = fixtures().join(scenario);
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_str().unwrap().to_string();
        let Some(name) = file.strip_suffix(".hex") else {
            continue;
        };
        let seq = seq_of_name(name).unwrap();
        let bytes = ham_core::gzip::unhex(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            b.put_if_absent(&chunk_key(run, seq), bytes),
            "{scenario}/{name} exists"
        );
    }
}

fn header(seq: u64, inc: &str, first: u64, last: u64) -> ChunkHeader {
    ChunkHeader {
        v: CHUNK_VERSION,
        seq,
        incarnation: inc.into(),
        first_rec: first,
        last_rec: last,
        created_at: "2026-10-16T10:00:00.000Z".into(),
        engine: "cline".into(),
        protocol: "1.38.0".into(),
    }
}

fn rec(n: u64, text: &str) -> ChunkRecord {
    ChunkRecord {
        n,
        t: None,
        dir: Dir::In,
        kind: "agent_message_chunk".into(),
        data: json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}, "messageId": format!("m{n}")}),
    }
}

/// A chunk of `count` records numbered from `first`, each with `text`.
fn chunk(seq: u64, inc: &str, first: u64, count: u64, text: &str) -> Vec<u8> {
    let records: Vec<ChunkRecord> = (first..first + count).map(|n| rec(n, text)).collect();
    let c = Chunk {
        header: header(seq, inc, first, first + count.max(1) - 1),
        records,
    };
    chunk_gzip_stored(&c)
}

fn deps<'a>(t: &'a Tables, b: &'a Bucket) -> Deps<&'a Tables, &'a Bucket, NoExecution> {
    Deps {
        store: t,
        blobs: b,
        execution: NoExecution,
        account: "800000000022".into(),
        operators: vec![],
        repo_names: Default::default(),
    }
}

fn request(caller: &str, run: &str, query: &[(&str, &str)]) -> V1Request {
    V1Request {
        method: "GET".into(),
        path: format!("/v1/runs/{run}/transcript"),
        caller_arn: Some(caller.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: query
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        body: String::new(),
    }
}

/// One call through the whole handler; the page as the client parses it.
async fn get(
    t: &Tables,
    b: &Bucket,
    caller: &str,
    run: &str,
    query: &[(&str, &str)],
) -> (u16, String, Vec<String>) {
    let (resp, diag) = handle_logged(request(caller, run, query), &deps(t, b)).await;
    (resp.status, resp.body, diag)
}

async fn page(t: &Tables, b: &Bucket, query: &[(&str, &str)]) -> TranscriptPage {
    let (status, body, _) = get(t, b, ME, RUN, query).await;
    assert_eq!(status, 200, "{body}");
    parse_page(body.as_bytes()).unwrap()
}

async fn page_with(
    t: &Tables,
    b: &Bucket,
    query: &[(&str, &str)],
    limits: &Limits,
) -> TranscriptPage {
    let (resp, _) = transcript_with(&request(ME, RUN, query), RUN, ME, &deps(t, b), limits).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    parse_page(resp.body.as_bytes()).unwrap()
}

fn ids(p: &TranscriptPage) -> Vec<(String, u64)> {
    p.records
        .iter()
        .map(|r| (r.incarnation.clone(), r.n))
        .collect()
}

fn pos(p: &TranscriptPage) -> Position {
    cursor::decode(RUN, ME, p.next.as_deref().unwrap()).unwrap()
}

// ---- who may read ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn another_principal_and_a_missing_run_are_both_not_found_before_anything_is_read() {
    let t = tables(GoalStatus::Active);
    t.runs
        .lock()
        .unwrap()
        .insert(RUN_B.into(), run_item(RUN_B, YOU, GoalStatus::Active));
    t.facts.lock().unwrap().insert(RUN_B.into(), supported());
    let b = Bucket::default();
    load(&b, RUN, "good");
    load(&b, RUN_B, "good");
    let before = b.calls();
    let cursor_of_mine = cursor::encode(RUN, ME, &Position::at(1, 0));
    let mut seen = Vec::new();
    for (caller, run, query) in [
        (YOU, RUN, vec![]),                                        // somebody else's run
        (ME, RUN_B, vec![]),                                       // I do not own it either
        (YOU, RUN, vec![("after", cursor_of_mine.as_str())]),      // even with my cursor
        (YOU, RUN, vec![("after", "garbage"), ("before", "end")]), // and a request that would be a 400 for the owner
        (ME, "01K83D1111111111111111111Z", vec![]),                // a run that does not exist
        (ME, "not-a-ulid", vec![]),                                // not an id at all
    ] {
        let (status, body, diag) = get(&t, &b, caller, run, &query).await;
        assert_eq!(status, 404, "{caller} {run} {query:?}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["code"], "not-found");
        assert!(diag.is_empty(), "{diag:?}");
        seen.push(v["code"].clone());
    }
    assert_eq!(
        b.calls(),
        before,
        "nothing in the bucket was touched for a run the caller does not own"
    );
    // The owner reads it.
    assert_eq!(get(&t, &b, ME, RUN, &[]).await.0, 200);
}

#[tokio::test]
async fn a_request_without_an_iam_caller_is_forbidden() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    let mut req = request(ME, RUN, &[]);
    req.caller_arn = None;
    let (resp, _) = handle_logged(req, &deps(&t, &b)).await;
    assert_eq!(resp.status, 403);
    assert_eq!(b.calls(), 0);
}

#[tokio::test]
async fn other_methods_on_the_route_are_405() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    for m in ["POST", "PUT", "DELETE"] {
        let mut req = request(ME, RUN, &[]);
        req.method = m.into();
        let (resp, _) = handle_logged(req, &deps(&t, &b)).await;
        assert_eq!(resp.status, 405, "{m}");
    }
}

// ---- the query -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_bad_query_is_a_400_and_reads_nothing() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let other = cursor::encode(RUN_B, ME, &Position::at(0, 0));
    let mine = cursor::encode(RUN, ME, &Position::at(0, 0));
    let foreign_caller = cursor::encode(RUN, YOU, &Position::at(0, 0));
    let before = b.calls();
    for query in [
        vec![("after", mine.as_str()), ("before", "end")],
        vec![("before", "abc")],
        vec![("before", "")],
        vec![("before", "1234567890123")],
        vec![("limit", "0")],
        vec![("limit", "65")],
        vec![("limit", "abc")],
        vec![("limit", "")],
        vec![("limit", "-1")],
        vec![("limit", "1000")],
        vec![("after", "garbage")],
        vec![("after", other.as_str())],
        vec![("after", foreign_caller.as_str())],
    ] {
        let (status, body, diag) = get(&t, &b, ME, RUN, &query).await;
        assert_eq!(status, 400, "{query:?}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["code"], "bad-request");
        // The message never repeats what was sent.
        for (_, value) in &query {
            assert!(value.len() < 4 || !body.contains(value), "{body}");
        }
        assert!(diag.is_empty());
    }
    assert_eq!(b.calls(), before);
    let long = "A".repeat(3000);
    assert_eq!(get(&t, &b, ME, RUN, &[("after", &long)]).await.0, 400);
    // The edges of limit are accepted.
    for l in ["1", "64"] {
        assert_eq!(
            get(&t, &b, ME, RUN, &[("limit", l)]).await.0,
            200,
            "limit {l}"
        );
    }
}

// ---- states ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_whose_image_writes_no_transcript_is_unsupported_and_costs_no_bucket_call() {
    let t = tables(GoalStatus::Complete);
    t.facts
        .lock()
        .unwrap()
        .insert(RUN.into(), TranscriptFacts::default());
    let b = Bucket::default();
    load(&b, RUN, "good");
    let p = page(&t, &b, &[]).await;
    assert_eq!(p.state, TranscriptState::Unsupported);
    assert!(p.records.is_empty() && p.gaps.is_empty() && p.next.is_none() && !p.more);
    assert_eq!(b.calls(), 0);
    // Purged is reserved and answers without reading too.
    t.facts.lock().unwrap().insert(
        RUN.into(),
        TranscriptFacts {
            supported: true,
            purged: true,
            summary: SummaryFact::Absent,
        },
    );
    let p = page(&t, &b, &[]).await;
    assert_eq!(p.state, TranscriptState::Purged);
    assert_eq!(b.calls(), 0);
}

#[tokio::test]
async fn nothing_committed_yet_is_empty_and_the_cursor_polls_from_the_start() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        (p.state, p.records.len(), p.gaps.len(), p.more),
        (TranscriptState::Empty, 0, 0, false)
    );
    assert_eq!(pos(&p), Position::at(0, 0));
    assert_eq!(
        (p.published_through, p.listed_through, p.prev),
        (None, None, None)
    );
    // The first chunk arrives; polling with the cursor finds it.
    load(&b, RUN, "good");
    let q = page(&t, &b, &[("after", p.next.as_deref().unwrap())]).await;
    assert_eq!(q.state, TranscriptState::Live);
    assert_eq!(q.records.len(), 13);
}

#[tokio::test]
async fn a_live_run_reads_in_order_as_engine_origin_with_a_stable_high_water_mark() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let p = page(&t, &b, &[]).await;
    assert_eq!(p.state, TranscriptState::Live);
    assert_eq!(p.records.len(), 13);
    assert_eq!(
        p.records.iter().map(|r| r.n).collect::<Vec<_>>(),
        (0..13).collect::<Vec<_>>()
    );
    assert_eq!(
        p.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 2, 2, 2]
    );
    assert!(
        p.records
            .iter()
            .all(|r| r.origin == Origin::Engine && r.incarnation == "01K83DA0A0A0A0A0A0A0A0A0A0")
    );
    assert_eq!(
        (
            p.published_through,
            p.listed_through,
            p.prev,
            p.more,
            p.skipped_keys
        ),
        (Some(2), Some(2), None, false, 0)
    );
    assert!(p.gaps.is_empty() && p.completeness.is_empty());
    assert_eq!(pos(&p), Position::at(3, 0));
    // The payload is the stored payload.
    assert_eq!(p.records[1].kind, "agent_message_chunk");
    assert_eq!(
        p.records[1].data["content"]["text"],
        "I will start with the tests."
    );
    assert_eq!(
        p.records[2].data["kind"], "edit",
        "ACP's own kind member sits in the payload"
    );
    // A chunk arrives while the client is reading: the same cursor finds it, the mark only grows.
    assert!(b.put_if_absent(&chunk_key(RUN, 3), chunk(3, INC_A, 13, 2, "later")));
    let q = page(&t, &b, &[("after", p.next.as_deref().unwrap())]).await;
    assert_eq!(
        q.records.iter().map(|r| r.n).collect::<Vec<_>>(),
        vec![13, 14]
    );
    assert_eq!((q.published_through, q.listed_through), (Some(3), Some(3)));
    assert!(q.listed_through >= p.listed_through);
    assert_eq!(pos(&q), Position::at(4, 0));
}

#[tokio::test]
async fn the_state_of_an_ended_run_comes_from_the_workers_summary_and_the_run_status() {
    let entry = |kind| CompletenessEntry {
        kind,
        from_seq: None,
        to_seq: None,
        count: Some(3),
    };
    let lossless = TranscriptSummary {
        last_transcript_seq: Some(2),
        entries: vec![CompletenessEntry {
            kind: GapKind::Excluded,
            from_seq: Some(0),
            to_seq: Some(2),
            count: None,
        }],
    };
    let shed = TranscriptSummary {
        last_transcript_seq: Some(2),
        entries: vec![entry(GapKind::Shed)],
    };
    let tail_unknown = vec![CompletenessEntry {
        kind: GapKind::TailUnknown,
        from_seq: None,
        to_seq: None,
        count: None,
    }];
    for (status, summary, want_state, want_completeness) in [
        (
            GoalStatus::Complete,
            SummaryFact::Valid(lossless.clone()),
            TranscriptState::Complete,
            lossless.entries.clone(),
        ),
        (
            GoalStatus::Cleared,
            SummaryFact::Valid(lossless.clone()),
            TranscriptState::Complete,
            lossless.entries.clone(),
        ),
        (
            GoalStatus::Blocked,
            SummaryFact::Valid(lossless.clone()),
            TranscriptState::Complete,
            lossless.entries.clone(),
        ),
        (
            GoalStatus::Complete,
            SummaryFact::Valid(shed.clone()),
            TranscriptState::Incomplete,
            shed.entries.clone(),
        ),
        (
            GoalStatus::Complete,
            SummaryFact::Absent,
            TranscriptState::Incomplete,
            tail_unknown.clone(),
        ),
        (
            GoalStatus::Complete,
            SummaryFact::Invalid,
            TranscriptState::Incomplete,
            tail_unknown.clone(),
        ),
        (
            GoalStatus::Active,
            SummaryFact::Valid(lossless.clone()),
            TranscriptState::Live,
            lossless.entries.clone(),
        ),
        (
            GoalStatus::Active,
            SummaryFact::Absent,
            TranscriptState::Live,
            vec![],
        ),
        // A paused or stalled run stopped delivered: its session is over, so its stored conversation is read like any ended run's.
        (
            GoalStatus::Paused,
            SummaryFact::Valid(lossless.clone()),
            TranscriptState::Complete,
            lossless.entries.clone(),
        ),
        (
            GoalStatus::Stalled,
            SummaryFact::Valid(lossless.clone()),
            TranscriptState::Complete,
            lossless.entries.clone(),
        ),
        (
            GoalStatus::Paused,
            SummaryFact::Absent,
            TranscriptState::Incomplete,
            tail_unknown.clone(),
        ),
    ] {
        let t = tables(status);
        t.facts.lock().unwrap().insert(
            RUN.into(),
            TranscriptFacts {
                supported: true,
                purged: false,
                summary,
            },
        );
        let b = Bucket::default();
        load(&b, RUN, "good");
        let p = page(&t, &b, &[]).await;
        assert_eq!(
            (p.state, &p.completeness),
            (want_state, &want_completeness),
            "{status:?}"
        );
        assert!(p.gaps.is_empty(), "{status:?}: {:?}", p.gaps);
    }
}

#[tokio::test]
async fn chunks_the_worker_declared_but_the_prefix_lacks_are_a_gap_once_the_whole_prefix_was_seen()
{
    let t = tables(GoalStatus::Complete);
    let summary = TranscriptSummary {
        last_transcript_seq: Some(5),
        entries: vec![],
    };
    t.facts.lock().unwrap().insert(
        RUN.into(),
        TranscriptFacts {
            supported: true,
            purged: false,
            summary: SummaryFact::Valid(summary),
        },
    );
    let b = Bucket::default();
    load(&b, RUN, "good");
    let p = page(&t, &b, &[]).await;
    assert_eq!(p.gaps.len(), 1);
    assert_eq!(
        (
            p.gaps[0].code,
            p.gaps[0].kind,
            p.gaps[0].from_seq,
            p.gaps[0].to_seq
        ),
        (GapCode::TailMissing, GapKind::Missing, 3, 5)
    );
    assert_eq!(
        p.state,
        TranscriptState::Complete,
        "the state is the worker's report; the gap is what the reader saw"
    );
    // A page that did not reach the end of the prefix does not claim a missing tail.
    let first = page(&t, &b, &[("limit", "1")]).await;
    assert!(first.more && first.gaps.is_empty(), "{:?}", first.gaps);
    // An active run's tail is not missing: it is not written yet.
    let t = tables(GoalStatus::Active);
    t.facts.lock().unwrap().insert(
        RUN.into(),
        TranscriptFacts {
            supported: true,
            purged: false,
            summary: SummaryFact::Valid(TranscriptSummary {
                last_transcript_seq: Some(5),
                entries: vec![],
            }),
        },
    );
    assert!(page(&t, &b, &[]).await.gaps.is_empty());
}

// ---- paging ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn pages_of_one_chunk_join_into_exactly_the_stored_record_without_loss_or_repeat() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let whole = page(&t, &b, &[]).await;
    let mut got = Vec::new();
    let mut counts = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let mut q = vec![("limit", "1")];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let p = page(&t, &b, &q).await;
        counts.push(p.records.len());
        got.extend(p.records.clone());
        after = p.next.clone();
        if !p.more {
            break;
        }
    }
    assert_eq!(counts, vec![5, 5, 3]);
    assert_eq!(
        got, whole.records,
        "the pages equal the stored record, record for record"
    );
}

#[tokio::test]
async fn limit_counts_chunks_and_more_says_whether_chunks_remain() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let p = page(&t, &b, &[("limit", "2")]).await;
    assert_eq!(
        (
            p.records.len(),
            p.more,
            p.published_through,
            p.listed_through
        ),
        (10, true, Some(1), Some(2))
    );
    assert_eq!(pos(&p), Position::at(2, 0));
    let q = page(
        &t,
        &b,
        &[("limit", "2"), ("after", p.next.as_deref().unwrap())],
    )
    .await;
    assert_eq!(
        (q.records.len(), q.more, q.published_through),
        (3, false, Some(2))
    );
    assert_eq!(q.prev, Some(2));
    // Three chunks with limit 3 read all and say there is no more.
    let all = page(&t, &b, &[("limit", "3")]).await;
    assert_eq!((all.records.len(), all.more), (13, false));
}

#[tokio::test]
async fn a_page_that_ends_inside_a_chunk_resumes_at_the_next_record() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let whole = page(&t, &b, &[]).await;
    let limits = Limits {
        page_bytes: 700,
        ..Limits::default()
    };
    let mut got = Vec::new();
    let mut pages = 0;
    let mut after: Option<String> = None;
    loop {
        let mut q = vec![];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let p = page_with(&t, &b, &q, &limits).await;
        pages += 1;
        let bytes: usize = p
            .records
            .iter()
            .map(|r| serde_json::to_vec(r).unwrap().len() + 1)
            .sum();
        assert!(
            p.records.len() == 1 || bytes <= 700,
            "page {pages}: {} records, {bytes} bytes",
            p.records.len()
        );
        assert!(!p.records.is_empty(), "every page makes progress");
        got.extend(p.records.clone());
        after = p.next.clone();
        if !p.more {
            break;
        }
        assert!(pages < 30, "no end");
    }
    assert!(pages >= 4, "{pages} pages");
    assert_eq!(got, whole.records);
}

#[tokio::test]
async fn before_end_opens_the_end_and_before_prev_walks_back_without_a_hole() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let end = page(&t, &b, &[("before", "end"), ("limit", "2")]).await;
    assert_eq!(
        end.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 1, 1, 1, 1, 2, 2, 2]
    );
    assert_eq!(
        (
            end.prev,
            end.more,
            end.published_through,
            end.listed_through
        ),
        (Some(1), false, Some(2), Some(2)),
        "more is about reading on with next; prev says older chunks exist"
    );
    assert_eq!(
        pos(&end),
        Position::at(3, 0),
        "the cursor polls forward from the end"
    );
    let older = page(
        &t,
        &b,
        &[("before", &end.prev.unwrap().to_string()), ("limit", "2")],
    )
    .await;
    assert_eq!(
        older.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![0, 0, 0, 0, 0]
    );
    assert_eq!(
        (older.prev, older.more),
        (None, true),
        "chunks 1 and 2 lie above the bound"
    );
    // Joined, the two pages equal the whole.
    let whole = page(&t, &b, &[]).await;
    let mut joined = older.records.clone();
    joined.extend(end.records.clone());
    assert_eq!(joined, whole.records);
    // A chunk number as the bound: chunk 2 lies above it, so reading on with next finds records and more says so.
    let below = page(&t, &b, &[("before", "2")]).await;
    assert_eq!(below.records.len(), 10);
    assert_eq!((below.prev, below.more), (None, true));
    assert_eq!(pos(&below), Position::at(2, 0));
    let none = page(&t, &b, &[("before", "0")]).await;
    assert!(none.records.is_empty());
    assert!(none.more, "every chunk lies above the bound");
    // A bound above every chunk leaves nothing above it.
    let above = page(&t, &b, &[("before", "3")]).await;
    assert_eq!((above.records.len(), above.more), (13, false));
}

#[tokio::test]
async fn a_page_that_starts_inside_a_chunk_says_which_before_returns_that_chunk_whole() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let open = cursor::encode(RUN, ME, &Position::at(1, 3));
    let p = page(&t, &b, &[("after", &open), ("limit", "1")]).await;
    assert_eq!(
        p.records.iter().map(|r| r.n).collect::<Vec<_>>(),
        vec![8, 9]
    );
    assert_eq!(
        p.prev,
        Some(2),
        "before=2 returns chunk 1 whole, with the records this page began after"
    );
    let back = page(&t, &b, &[("before", "2"), ("limit", "1")]).await;
    assert_eq!(
        back.records.iter().map(|r| r.n).collect::<Vec<_>>(),
        vec![5, 6, 7, 8, 9]
    );
}

// ---- gaps, writers and forged records --------------------------------------------------------------------------------

#[tokio::test]
async fn a_chunk_that_was_never_written_is_a_gap_in_the_sequence() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "gapped");
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        p.records.iter().map(|r| (r.seq, r.n)).collect::<Vec<_>>(),
        vec![(0, 0), (0, 1), (1, 2), (1, 3), (3, 6)]
    );
    assert_eq!(p.gaps.len(), 1);
    let g = &p.gaps[0];
    assert_eq!(
        (g.code, g.kind, g.from_seq, g.to_seq),
        (GapCode::MissingChunk, GapKind::Missing, 2, 2)
    );
    assert_eq!(p.published_through, Some(3));
}

#[tokio::test]
async fn a_second_writer_that_continues_is_no_gap_and_one_that_returns_is_a_conflict() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "handover");
    let p = page(&t, &b, &[]).await;
    assert!(p.gaps.is_empty(), "{:?}", p.gaps);
    assert_eq!(p.records.len(), 6);
    let incs: Vec<&str> = p.records.iter().map(|r| r.incarnation.as_str()).collect();
    assert_eq!(incs.iter().filter(|i| i.starts_with("01K83DA0")).count(), 3);
    assert_eq!(incs.iter().filter(|i| i.starts_with("01K83DB0")).count(), 3);
    // Record numbers restart in the second incarnation; the identity (incarnation, n) stays unique.
    let mut all = ids(&p);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 6);

    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "interleaved");
    let p = page(&t, &b, &[]).await;
    assert_eq!(p.records.len(), 3, "both writers' records stay");
    assert_eq!(p.gaps.len(), 1);
    assert_eq!(
        (p.gaps[0].code, p.gaps[0].kind, p.gaps[0].from_seq),
        (GapCode::Interleaved, GapKind::Conflict, 2)
    );
}

#[tokio::test]
async fn every_way_a_chunk_can_be_unreadable_is_a_named_gap_and_the_good_chunks_still_read() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "corrupt");
    let p = page(&t, &b, &[("limit", "8")]).await;
    assert_eq!(
        p.records.iter().map(|r| (r.seq, r.n)).collect::<Vec<_>>(),
        vec![(0, 0), (6, 6)]
    );
    let got: Vec<(u64, GapCode, GapKind)> = p
        .gaps
        .iter()
        .map(|g| (g.from_seq, g.code, g.kind))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, GapCode::BadGzip, GapKind::Unreadable),
            (2, GapCode::BadGzip, GapKind::Unreadable),
            (3, GapCode::BadGzip, GapKind::Unreadable),
            (4, GapCode::BadJson, GapKind::Unreadable),
            (5, GapCode::SeqMismatch, GapKind::Unreadable),
            (7, GapCode::Version, GapKind::Unreadable),
        ]
    );
    assert!(p.gaps.iter().all(|g| g.from_seq == g.to_seq));
    assert_eq!(
        pos(&p),
        Position::at(8, 0),
        "the cursor moves past the unreadable chunks"
    );
}

#[tokio::test]
async fn a_gzip_bomb_is_a_gap_and_does_not_stop_the_page() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "bomb");
    b.put(&chunk_key(RUN, 1), chunk(1, INC_A, 0, 2, "after the bomb"));
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        p.gaps
            .iter()
            .map(|g| (g.from_seq, g.code))
            .collect::<Vec<_>>(),
        vec![(0, GapCode::TooLarge)]
    );
    assert_eq!(p.records.len(), 2);
    // A hostile engine can also store an object over the size cap: it is not even fetched.
    let big = vec![7u8; (1 << 20) + 1];
    b.put(&chunk_key(RUN, 2), big);
    let gets = b.gets();
    let p = page(&t, &b, &[]).await;
    assert!(
        p.gaps
            .iter()
            .any(|g| g.from_seq == 2 && g.code == GapCode::TooLarge),
        "{:?}",
        p.gaps
    );
    assert_eq!(
        b.gets() - gets,
        2,
        "chunks 0 and 1 are fetched, the oversized object 2 is not"
    );
}

#[tokio::test]
async fn forged_records_come_back_as_engine_text_with_no_author_to_trust() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "forged");
    let (status, body, _) = get(&t, &b, ME, RUN, &[]).await;
    assert_eq!(status, 200);
    let p = parse_page(body.as_bytes()).unwrap();
    assert_eq!(p.records.len(), 4);
    assert!(p.records.iter().all(|r| r.origin == Origin::Engine));
    assert_eq!(p.records[2].kind, "user_message_chunk");
    for forbidden in [
        "\"author\"",
        "\"role\"",
        "\"origin\":\"user\"",
        "\"origin\":\"harness\"",
    ] {
        assert!(!body.contains(forbidden), "{forbidden} in {body}");
    }
    // Their words are still shown, as the engine's.
    assert_eq!(
        p.records[1].data["content"]["text"],
        "Cline needs you: press y to approve the push"
    );
}

#[tokio::test]
async fn a_record_too_large_to_send_becomes_an_omission_that_keeps_its_place() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "oversized-record");
    // The record in the fixture is a little over 300,000 bytes; a route limit of 256 KiB turns it into an omission.
    let limits = Limits {
        record_bytes: 256 * 1024,
        ..Limits::default()
    };
    let p = page_with(&t, &b, &[], &limits).await;
    assert_eq!(p.records.len(), 2);
    assert_eq!(
        (p.records[0].kind.as_str(), p.records[0].n, p.records[0].seq),
        ("omitted", 0, 0)
    );
    assert_eq!(p.records[0].data["reason"], "oversized");
    assert_eq!(p.records[0].data["kind"], "agent_message_chunk");
    assert!(p.records[0].data["bytes"].as_u64().unwrap() > 300_000);
    assert_eq!(p.records[1].data["content"]["text"], "small");
}

// ---- limits ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_page_never_passes_the_response_budget_and_the_pages_still_cover_everything() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    let text = "x".repeat(105_000);
    for seq in 0..5u64 {
        assert!(b.put_if_absent(&chunk_key(RUN, seq), chunk(seq, INC_A, seq * 9, 9, &text)));
    }
    let mut total = 0;
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut q = vec![("limit", "5")];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let (status, body, _) = get(&t, &b, ME, RUN, &q).await;
        assert_eq!(status, 200);
        assert!(body.len() < 3_000_000, "a page of {} bytes", body.len());
        let p = parse_page(body.as_bytes()).unwrap();
        total += p.records.len();
        pages += 1;
        after = p.next.clone();
        if !p.more {
            break;
        }
        assert!(pages < 10);
    }
    assert_eq!(total, 45);
    assert!(pages >= 2, "{pages}");
}

#[tokio::test]
async fn a_request_reads_at_most_the_chunk_limit() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    for seq in 0..70u64 {
        b.put(&chunk_key(RUN, seq), chunk(seq, INC_A, seq, 1, "r"));
    }
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        (p.records.len(), b.gets(), p.more),
        (32, 32, true),
        "the default is 32 chunks"
    );
    let before = b.gets();
    let p = page(&t, &b, &[("limit", "64")]).await;
    assert_eq!((p.records.len(), b.gets() - before, p.more), (64, 64, true));
    assert_eq!(MAX_CHUNKS_PER_REQUEST, 64);
    // Every list call asks for a legal number of keys (the bucket refuses the rest).
    assert!(b.log.lock().unwrap().iter().all(
        |a| !matches!(a, Access::List { max_keys, .. } if *max_keys == 0 || *max_keys > 1000)
    ));
}

#[tokio::test]
async fn thousands_of_junk_keys_and_a_pre_created_future_key_cost_a_few_lists_and_no_reads() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let prefix = transcript_prefix(RUN);
    for i in 0..5000 {
        b.put(&format!("{prefix}junk-{i:05}"), b"x".to_vec());
    }
    for name in [
        "seal",
        "manifest.json",
        "000000000000.ndjson.gz.tmp",
        "sub/000000000001.ndjson.gz",
        "00000000001.ndjson.gz",
    ] {
        b.put(
            &format!("{prefix}{name}"),
            b"{\"sealed\":true,\"lastSeq\":99}".to_vec(),
        );
    }
    // A chunk-shaped key far ahead, with nothing readable in it.
    b.put(&chunk_key(RUN, 999), b"garbage".to_vec());
    let started = std::time::Instant::now();
    let p = page(&t, &b, &[("limit", "10")]).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(p.records.len(), 13);
    assert_eq!(p.skipped_keys, 5005);
    let got: Vec<(u64, u64, GapCode)> = p
        .gaps
        .iter()
        .map(|g| (g.from_seq, g.to_seq, g.code))
        .collect();
    assert_eq!(
        got,
        vec![
            (3, 998, GapCode::MissingChunk),
            (999, 999, GapCode::BadGzip)
        ]
    );
    assert_eq!(
        b.gets(),
        4,
        "three real chunks and the garbage one; no junk key is read"
    );
    assert!(b.lists() <= 8, "{} list calls", b.lists());
    assert_eq!(p.state, TranscriptState::Live);
}

#[tokio::test]
async fn a_listing_that_meets_its_cap_says_so_and_the_cursor_carries_on_past_the_junk() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    let prefix = transcript_prefix(RUN);
    for i in 0..200 {
        b.put(&format!("{prefix}0000000-junk-{i:04}"), b"x".to_vec());
    }
    load(&b, RUN, "good");
    let limits = Limits {
        max_keys: 50,
        ..Limits::default()
    };
    let mut records = 0;
    let mut cap_gaps = 0;
    let mut after: Option<String> = None;
    let mut keys: Vec<String> = Vec::new();
    for round in 0..20 {
        let mut q = vec![];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let p = page_with(&t, &b, &q, &limits).await;
        records += p.records.len();
        cap_gaps += p
            .gaps
            .iter()
            .filter(|g| g.code == GapCode::ListLimit && g.kind == GapKind::Truncated)
            .count();
        after = p.next.clone();
        let k = cursor::decode(RUN, ME, p.next.as_deref().unwrap())
            .unwrap()
            .after_key;
        if let Some(k) = k {
            assert!(
                keys.last().is_none_or(|prev| *prev < k),
                "the continuation only moves forward: {keys:?} then {k}"
            );
            keys.push(k);
        }
        if !p.more {
            break;
        }
        assert!(round < 19, "no end");
    }
    assert_eq!(records, 13);
    assert!(cap_gaps >= 3, "{cap_gaps}");
}

#[tokio::test]
async fn a_slow_bucket_ends_the_page_at_the_time_budget_and_the_cursor_finishes_the_job() {
    let t = tables(GoalStatus::Active);
    let b = Bucket {
        latency: Duration::from_millis(40),
        ..Bucket::default()
    };
    for seq in 0..10u64 {
        b.put(&chunk_key(RUN, seq), chunk(seq, INC_A, seq * 2, 2, "slow"));
    }
    let limits = Limits {
        time: Duration::from_millis(100),
        ..Limits::default()
    };
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut q = vec![];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let p = page_with(&t, &b, &q, &limits).await;
        pages += 1;
        assert!(!p.records.is_empty(), "every page makes progress");
        seen.extend(ids(&p));
        after = p.next.clone();
        if !p.more {
            break;
        }
        assert!(pages < 12, "no end");
    }
    assert!(
        pages >= 2,
        "{pages}: the time budget should have ended the first page early"
    );
    assert_eq!(seen.len(), 20);
    let mut sorted = seen.clone();
    sorted.sort_by_key(|(_, n)| *n);
    assert_eq!(seen, sorted);
}

#[tokio::test]
async fn an_object_swapped_after_the_listing_is_a_conflict_not_an_error() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    *b.swap_after_list.lock().unwrap() =
        Some((chunk_key(RUN, 1), chunk(1, INC_A, 5, 1, "swapped")));
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        p.gaps
            .iter()
            .map(|g| (g.from_seq, g.code, g.kind))
            .collect::<Vec<_>>(),
        vec![(1, GapCode::ChangedWhileReading, GapKind::Conflict)]
    );
    assert_eq!(p.records.len(), 8, "chunks 0 and 2 are still read");
}

// ---- what reaches the bucket, the logs and the client ----------------------------------------------------------------

#[tokio::test]
async fn only_the_runs_own_transcript_prefix_is_listed_and_read() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    load(&b, RUN_B, "forged");
    b.put(
        &format!("runs/{RUN}/deliverable/secret.txt"),
        b"nope".to_vec(),
    );
    b.put(
        &format!("runs/{RUN}/agents/direct/turns/1/summary.json"),
        b"{}".to_vec(),
    );
    let _ = page(&t, &b, &[("before", "end")]).await;
    let _ = page(&t, &b, &[]).await;
    let prefix = transcript_prefix(RUN);
    let touched = b.touched();
    assert!(!touched.is_empty());
    assert!(
        touched.iter().all(|k| k.starts_with(&prefix)),
        "{touched:?}"
    );
}

#[tokio::test]
async fn the_bucket_double_refuses_what_s3_and_the_role_policy_refuse() {
    use control_api::handler::{Blobs, ObjectError};
    let b = Bucket::default();
    load(&b, RUN, "good");
    let prefix = transcript_prefix(RUN);
    let r = &b;
    assert!(Blobs::list_objects(&r, &prefix, None, 0).await.is_err());
    assert!(Blobs::list_objects(&r, &prefix, None, 1001).await.is_err());
    assert!(
        Blobs::list_objects(&r, &prefix, Some(&"k".repeat(1025)), 10)
            .await
            .is_err()
    );
    assert!(
        Blobs::list_objects(&r, &format!("runs/{RUN}/"), None, 10)
            .await
            .is_err(),
        "the role may not list the run's other prefixes"
    );
    assert!(Blobs::list_objects(&r, "", None, 10).await.is_err());
    assert!(
        Blobs::list_objects(&r, prefix.trim_end_matches('/'), None, 10)
            .await
            .is_err(),
        "no trailing slash, no match"
    );
    assert!(matches!(
        Blobs::get_object(&r, &format!("runs/{RUN}/deliverable/x"), 10, None).await,
        Err(ObjectError::Store(_))
    ));
    assert!(matches!(
        Blobs::get_object(&r, &"k".repeat(1025), 10, None).await,
        Err(ObjectError::Store(_))
    ));
    assert!(matches!(
        Blobs::get_object(&r, &chunk_key(RUN, 0), 5, None).await,
        Err(ObjectError::TooLarge)
    ));
    assert!(matches!(
        Blobs::get_object(&r, &chunk_key(RUN, 0), 1 << 20, Some("\"other\"")).await,
        Err(ObjectError::Changed)
    ));
    assert_eq!(
        Blobs::get_object(&r, &chunk_key(RUN, 77), 10, None)
            .await
            .unwrap(),
        None
    );
    // StartAfter is exclusive and keys come in binary order.
    let all = Blobs::list_objects(&r, &prefix, None, 10).await.unwrap();
    assert_eq!(all.objects.len(), 3);
    let rest = Blobs::list_objects(&r, &prefix, Some(&all.objects[0].key), 10)
        .await
        .unwrap();
    assert_eq!(rest.objects.len(), 2);
    let one = Blobs::list_objects(&r, &prefix, None, 2).await.unwrap();
    assert!(one.truncated && one.objects.len() == 2);
    // The writer's conditional put: a key that exists is refused.
    assert!(!b.put_if_absent(&chunk_key(RUN, 0), b"again".to_vec()));
}

#[tokio::test]
async fn storage_faults_are_503s_that_name_nothing_and_a_log_that_holds_no_cursor_or_chunk_text() {
    let t = tables(GoalStatus::Active);
    let b = Bucket {
        list_down: true,
        ..Bucket::default()
    };
    let (status, body, diag) = get(&t, &b, ME, RUN, &[]).await;
    assert_eq!(status, 503);
    assert!(!body.contains("SECRET-STORE-DETAIL"), "{body}");
    assert!(
        diag.iter().any(|d| d.contains("SECRET-STORE-DETAIL")),
        "the reason goes to the log: {diag:?}"
    );
    let b = Bucket {
        get_down: true,
        ..Bucket::default()
    };
    load(&b, RUN, "good");
    let (status, body, _) = get(&t, &b, ME, RUN, &[]).await;
    assert_eq!(status, 503);
    assert!(!body.contains("SECRET-STORE-DETAIL"), "{body}");
    let t2 = Tables {
        down: true,
        ..Tables::default()
    };
    let (status, body, _) = get(&t2, &b, ME, RUN, &[]).await;
    assert_eq!(status, 503);
    assert!(!body.contains("SECRET-STORE-DETAIL"), "{body}");
}

#[tokio::test]
async fn nothing_secret_comes_back_a_token_and_a_presigned_url_in_a_record_are_redacted() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    let sig = "0123456789abcdef".repeat(2);
    let cred = ["ASIA", "-test-", "0000"].concat();
    let url = format!(
        "https://bucket.s3.amazonaws.com/runs/x/y?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={cred}%2F20261016&X-Amz-Signature={sig}"
    );
    let text = format!("push with {token} then fetch {url}");
    b.put(&chunk_key(RUN, 0), chunk(0, INC_A, 0, 1, &text));
    let (status, body, diag) = get(&t, &b, ME, RUN, &[]).await;
    assert_eq!(status, 200);
    for forbidden in [token.as_str(), sig.as_str(), cred.as_str()] {
        assert!(!body.contains(forbidden), "{forbidden} in {body}");
    }
    assert!(
        diag.iter()
            .all(|d| !d.contains(&token) && !d.contains("X-Amz")),
        "{diag:?}"
    );
    let p = parse_page(body.as_bytes()).unwrap();
    assert!(
        p.records[0].data["content"]["text"]
            .as_str()
            .unwrap()
            .starts_with("push with ")
    );
}

#[tokio::test]
async fn the_access_line_and_the_diagnostics_hold_no_cursor() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "corrupt");
    let first = page(&t, &b, &[("limit", "2")]).await;
    let token = first.next.clone().unwrap();
    let req = request(ME, RUN, &[("after", &token), ("limit", "3")]);
    let (resp, diag) = handle_logged(req.clone(), &deps(&t, &b)).await;
    let lines = control_api::lambda::log_lines(&req, resp.status, &diag);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(!line.contains(&token), "{line}");
        assert!(!line.contains("after="), "{line}");
        assert!(!line.contains("this is not gzip"), "{line}");
    }
    assert!(
        diag.iter().any(|d| d.contains("bad-gzip")),
        "unreadable chunks are logged by code: {diag:?}"
    );
    let _ = resp;
}

// ---- findings of the implementation review (2026-10-16) --------------------------------------------------------------

#[tokio::test]
async fn an_ended_run_read_to_its_end_has_no_next_and_a_live_one_keeps_its_poll_cursor() {
    let b = Bucket::default();
    load(&b, RUN, "good");
    let ended = tables(GoalStatus::Complete);
    ended.facts.lock().unwrap().insert(
        RUN.into(),
        TranscriptFacts {
            supported: true,
            purged: false,
            summary: SummaryFact::Valid(TranscriptSummary {
                last_transcript_seq: Some(2),
                entries: vec![],
            }),
        },
    );
    let p = page(&ended, &b, &[]).await;
    assert_eq!(
        (p.records.len(), p.more, p.next.clone()),
        (13, false, None),
        "nothing is left to read on from"
    );
    // A client that follows next until it is absent stops.
    let first = page(&ended, &b, &[("limit", "1")]).await;
    assert!(first.more && first.next.is_some());
    // A live run keeps a cursor to poll from, even when it has nothing new.
    let live = tables(GoalStatus::Active);
    let p = page(&live, &b, &[]).await;
    assert!(!p.more && p.next.is_some());
    let again = page(&live, &b, &[("after", p.next.as_deref().unwrap())]).await;
    assert!(again.records.is_empty() && !again.more);
    assert_eq!(pos(&again), Position::at(3, 0));
}

#[tokio::test]
async fn the_first_chunk_is_number_zero_so_a_missing_start_is_a_gap() {
    assert_eq!(FIRST_SEQ, 0);
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    b.put(&chunk_key(RUN, 5), chunk(5, INC_A, 0, 2, "late start"));
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        p.gaps
            .iter()
            .map(|g| (g.from_seq, g.to_seq, g.code))
            .collect::<Vec<_>>(),
        vec![(0, 4, GapCode::MissingChunk)]
    );
    assert_eq!(p.records.len(), 2);
    // Read from a cursor, the start is not in question.
    let q = page(
        &t,
        &b,
        &[("after", &cursor::encode(RUN, ME, &Position::at(5, 0)))],
    )
    .await;
    assert!(q.gaps.is_empty());
}

#[tokio::test]
async fn before_end_over_a_listing_that_met_its_cap_does_not_present_the_start_as_the_end() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    let prefix = transcript_prefix(RUN);
    for i in 0..200 {
        b.put(&format!("{prefix}0000000-junk-{i:04}"), b"x".to_vec());
    }
    load(&b, RUN, "good");
    let limits = Limits {
        max_keys: 50,
        ..Limits::default()
    };
    let p = page_with(&t, &b, &[("before", "end")], &limits).await;
    assert!(
        p.records.is_empty(),
        "the tail was not located, so no window of the front is returned as if it were"
    );
    assert!(
        p.gaps.iter().any(|g| g.code == GapCode::ListLimit),
        "{:?}",
        p.gaps
    );
    assert!(p.more && p.next.is_some() && p.prev.is_none());
    // The cursor walks forward over the junk to the records.
    let mut after = p.next.clone();
    let mut records = 0;
    for _ in 0..10 {
        let q = page_with(&t, &b, &[("after", after.as_deref().unwrap())], &limits).await;
        records += q.records.len();
        if !q.more {
            break;
        }
        after = q.next;
    }
    assert_eq!(records, 13);
}

#[tokio::test]
async fn a_long_junk_key_is_carried_in_the_cursor_and_one_that_hides_a_token_ends_the_listing_openly()
 {
    let b = Bucket::default();
    let t = tables(GoalStatus::Active);
    let prefix = transcript_prefix(RUN);
    for i in 0..120 {
        b.put(
            &format!("{prefix}0000000-{}{i:04}", "j".repeat(300)),
            b"x".to_vec(),
        );
    }
    load(&b, RUN, "good");
    let limits = Limits {
        max_keys: 50,
        ..Limits::default()
    };
    let mut after: Option<String> = None;
    let mut records = 0;
    for round in 0..12 {
        let mut q = vec![];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let p = page_with(&t, &b, &q, &limits).await;
        records += p.records.len();
        if !p.more {
            break;
        }
        after = p.next.clone();
        assert!(round < 11, "no end");
    }
    assert_eq!(records, 13, "a 300-character key does not stop the walk");

    // A key that holds a token-shaped string is never copied into a token: the page says the listing stopped and offers no way on.
    let b = Bucket::default();
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    for i in 0..120 {
        b.put(&format!("{prefix}0000000-{token}-{i:04}"), b"x".to_vec());
    }
    load(&b, RUN, "good");
    let (resp, _) = transcript_with(&request(ME, RUN, &[]), RUN, ME, &deps(&t, &b), &limits).await;
    assert_eq!(resp.status, 200);
    assert!(
        !resp.body.contains(&token) && !resp.body.contains("a1B2c3D4"),
        "{}",
        resp.body
    );
    let p = parse_page(resp.body.as_bytes()).unwrap();
    assert!(p.gaps.iter().any(|g| g.code == GapCode::ListLimit));
    assert!(
        !p.more && p.next.is_none(),
        "an unresumable listing is not a normal-looking page: {:?}",
        p.next
    );
}

#[tokio::test]
async fn a_cursor_inside_a_chunk_is_bound_to_the_object_it_was_cut_from() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let limits = Limits {
        page_bytes: 700,
        ..Limits::default()
    };
    let first = page_with(&t, &b, &[], &limits).await;
    let cut = pos(&first);
    assert!(cut.idx > 0, "the first page ended inside a chunk: {cut:?}");
    assert!(
        cut.etag.is_some(),
        "and the cursor remembers which object it was cut from"
    );
    // The engine replaces that chunk with another valid one.
    b.put(
        &chunk_key(RUN, cut.seq),
        chunk(cut.seq, INC_A, 100, 6, "replaced"),
    );
    let next = page_with(
        &t,
        &b,
        &[("after", first.next.as_deref().unwrap())],
        &limits,
    )
    .await;
    assert!(
        next.gaps.iter().any(|g| g.from_seq == cut.seq
            && g.code == GapCode::ChangedWhileReading
            && g.kind == GapKind::Conflict),
        "{:?}",
        next.gaps
    );
    assert!(
        next.records.iter().all(|r| r.seq != cut.seq),
        "nothing is read from the replaced object"
    );
    // An unchanged object resumes exactly where it was cut.
    let t2 = tables(GoalStatus::Active);
    let b2 = Bucket::default();
    load(&b2, RUN, "good");
    let whole = page(&t2, &b2, &[]).await;
    let first = page_with(&t2, &b2, &[], &limits).await;
    let second = page_with(
        &t2,
        &b2,
        &[("after", first.next.as_deref().unwrap())],
        &limits,
    )
    .await;
    assert_eq!(
        first.records.last().map(|r| r.n).unwrap() + 1,
        second.records.first().map(|r| r.n).unwrap()
    );
    assert_eq!(whole.records[..first.records.len()], first.records[..]);
}

#[tokio::test]
async fn a_slow_listing_ends_the_page_at_the_soft_deadline_without_calling_it_a_gap() {
    let t = tables(GoalStatus::Active);
    let b = Bucket {
        latency: Duration::from_millis(30),
        ..Bucket::default()
    };
    let prefix = transcript_prefix(RUN);
    for i in 0..3000 {
        b.put(&format!("{prefix}0000000-junk-{i:05}"), b"x".to_vec());
    }
    load(&b, RUN, "good");
    let limits = Limits {
        time: Duration::from_millis(40),
        ..Limits::default()
    };
    let mut after: Option<String> = None;
    let mut records = 0;
    let mut stops = 0;
    let mut keys: Vec<String> = Vec::new();
    for round in 0..30 {
        let mut q = vec![];
        if let Some(a) = &after {
            q.push(("after", a.as_str()));
        }
        let p = page_with(&t, &b, &q, &limits).await;
        assert!(
            p.gaps.iter().all(|g| g.code != GapCode::ListLimit),
            "a time stop is not a cap: {:?}",
            p.gaps
        );
        records += p.records.len();
        if let Some(k) = cursor::decode(RUN, ME, p.next.as_deref().unwrap_or(""))
            .ok()
            .and_then(|c| c.after_key)
        {
            assert!(
                keys.last().is_none_or(|prev| *prev < k),
                "the listing only moves forward"
            );
            keys.push(k);
        }
        if p.records.is_empty() && p.more {
            stops += 1;
        }
        if !p.more {
            break;
        }
        after = p.next.clone();
        assert!(round < 29, "no end");
    }
    assert_eq!(records, 13);
    assert!(
        stops >= 1,
        "the first request should have run out of time inside the listing"
    );
}

#[tokio::test]
async fn a_record_number_used_twice_by_one_writer_is_a_conflict_and_the_second_is_dropped() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    b.put(
        &chunk_key(RUN, 0),
        chunk(0, INC_A, 0, 2, "first writer text"),
    );
    b.put(
        &chunk_key(RUN, 1),
        chunk(1, INC_A, 1, 2, "same numbers, other words"),
    );
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        p.records.iter().map(|r| (r.seq, r.n)).collect::<Vec<_>>(),
        vec![(0, 0), (0, 1), (1, 2)]
    );
    assert_eq!(
        p.gaps
            .iter()
            .map(|g| (g.from_seq, g.code, g.kind))
            .collect::<Vec<_>>(),
        vec![(1, GapCode::DuplicateRecord, GapKind::Conflict)]
    );
}

fn numbered(seq: u64, inc: &str, numbers: &[u64], gap_at: &[u64]) -> Vec<u8> {
    let records: Vec<ChunkRecord> = numbers
        .iter()
        .map(|n| ChunkRecord {
            n: *n,
            t: None,
            dir: Dir::In,
            kind: if gap_at.contains(n) {
                "gap".into()
            } else {
                "notice".into()
            },
            data: json!({"text": format!("record {n}")}),
        })
        .collect();
    let c = Chunk {
        header: header(seq, inc, numbers[0], *numbers.last().unwrap()),
        records,
    };
    chunk_gzip_stored(&c)
}

#[tokio::test]
async fn record_numbers_that_skip_with_no_gap_record_are_a_hole_in_the_conversation() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    b.put(&chunk_key(RUN, 0), numbered(0, INC_A, &[0, 1, 3], &[]));
    b.put(&chunk_key(RUN, 1), numbered(1, INC_A, &[4, 5], &[]));
    b.put(&chunk_key(RUN, 2), numbered(2, INC_A, &[9], &[]));
    b.put(
        &chunk_key(RUN, 3),
        numbered(3, "01K83DB0B0B0B0B0B0B0B0B0B0", &[0, 1], &[]),
    );
    b.put(
        &chunk_key(RUN, 4),
        numbered(4, "01K83DB0B0B0B0B0B0B0B0B0B0", &[2, 4, 5], &[4]),
    );
    let p = page(&t, &b, &[]).await;
    assert_eq!(
        p.gaps
            .iter()
            .map(|g| (g.from_seq, g.code, g.kind))
            .collect::<Vec<_>>(),
        vec![
            (0, GapCode::RecordHole, GapKind::Missing),
            (2, GapCode::RecordHole, GapKind::Missing)
        ],
        "a skip inside chunk 0 and a skip between chunks 1 and 2; the new writer restarting at 0 and a hole the writer recorded are not holes"
    );
    assert_eq!(p.records.len(), 11, "every record is still returned");
}

#[tokio::test]
async fn the_budget_leaves_room_for_the_worst_gaps_and_completeness() {
    let limits = Limits::default();
    assert!(control_api::transcript::RESPONSE_BUDGET - limits.page_bytes >= 64 * 1024);
    // 256 gaps and 256 completeness entries beside a full page stay under the budget.
    let t = tables(GoalStatus::Complete);
    let entries: Vec<CompletenessEntry> = (0..256)
        .map(|i| CompletenessEntry {
            kind: GapKind::Shed,
            from_seq: Some(i),
            to_seq: Some(i + 1),
            count: Some(u64::MAX / 2),
        })
        .collect();
    t.facts.lock().unwrap().insert(
        RUN.into(),
        TranscriptFacts {
            supported: true,
            purged: false,
            summary: SummaryFact::Valid(TranscriptSummary {
                last_transcript_seq: Some(9_999_999),
                entries,
            }),
        },
    );
    let b = Bucket::default();
    let text = "x".repeat(100_000);
    for seq in 0..64u64 {
        // Every other chunk is unreadable: 32 of the gaps; the rest carry big records.
        if seq % 2 == 0 {
            b.put(&chunk_key(RUN, seq), b"not gzip".to_vec());
        } else {
            b.put(&chunk_key(RUN, seq), chunk(seq, INC_A, seq * 10, 10, &text));
        }
    }
    let (status, body, _) = get(&t, &b, ME, RUN, &[("limit", "64")]).await;
    assert_eq!(status, 200);
    assert!(body.len() < 3_000_000, "{} bytes", body.len());
}

#[tokio::test]
async fn a_backward_cut_that_found_chunks_resumes_at_the_first_even_when_the_last_key_cannot_be_carried()
 {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "good");
    let prefix = transcript_prefix(RUN);
    // Junk after the chunks (it sorts after them) holds a token-shaped string, so the key the listing stopped on cannot go into a cursor.
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    for i in 0..120 {
        b.put(&format!("{prefix}zzz-{token}-{i:04}"), b"x".to_vec());
    }
    let limits = Limits {
        max_keys: 50,
        ..Limits::default()
    };
    let p = page_with(&t, &b, &[("before", "end")], &limits).await;
    assert!(p.records.is_empty(), "the front of the run is not the end");
    assert!(
        p.gaps.iter().any(|g| g.code == GapCode::ListLimit),
        "{:?}",
        p.gaps
    );
    assert!(
        p.more && p.next.is_some(),
        "the chunks that were found are reachable: {:?}",
        p.next
    );
    assert_eq!(pos(&p), Position::at(0, 0));
    let (resp, _) = transcript_with(
        &request(ME, RUN, &[("before", "end")]),
        RUN,
        ME,
        &deps(&t, &b),
        &limits,
    )
    .await;
    assert!(
        !resp.body.contains(&token) && !resp.body.contains("a1B2c3D4"),
        "{}",
        resp.body
    );
    // Following the cursor reads them.
    let q = page_with(&t, &b, &[("after", p.next.as_deref().unwrap())], &limits).await;
    assert_eq!(q.records.len(), 13);
}

#[tokio::test]
async fn a_cursor_into_a_chunk_that_has_gone_is_a_gap_and_the_read_moves_on() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    b.put(
        &chunk_key(RUN, 0),
        chunk(0, INC_A, 0, 6, "some words in a record"),
    );
    let limits = Limits {
        page_bytes: 300,
        ..Limits::default()
    };
    let first = page_with(&t, &b, &[], &limits).await;
    let cut = pos(&first);
    assert!(cut.idx > 0 && cut.etag.is_some(), "{cut:?}");
    // The object goes away before the client comes back.
    b.objects.lock().unwrap().remove(&chunk_key(RUN, 0));
    let next = page_with(
        &t,
        &b,
        &[("after", first.next.as_deref().unwrap())],
        &limits,
    )
    .await;
    assert_eq!(
        next.gaps
            .iter()
            .map(|g| (g.from_seq, g.to_seq, g.code))
            .collect::<Vec<_>>(),
        vec![(0, 0, GapCode::MissingChunk)]
    );
    assert!(next.records.is_empty());
    assert_eq!(
        pos(&next),
        Position::at(1, 0),
        "the cursor does not stay on the chunk that is gone"
    );
    // An ended run says so and has nothing to read on from.
    let ended = tables(GoalStatus::Complete);
    let end = page_with(
        &ended,
        &b,
        &[(
            "after",
            &cursor::encode(
                RUN,
                ME,
                &Position {
                    seq: 0,
                    idx: 2,
                    after_key: None,
                    etag: Some("\"abc\"".into()),
                },
            ),
        )],
        &limits,
    )
    .await;
    assert_eq!(
        end.gaps
            .iter()
            .map(|g| (g.from_seq, g.code))
            .collect::<Vec<_>>(),
        vec![(0, GapCode::MissingChunk)]
    );
    assert!(end.next.is_none() && !end.more);
}

#[tokio::test]
async fn a_gap_record_explains_the_numbers_it_shed_across_a_resume_point_and_a_chunk_boundary() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    // Chunk 0 ends with a gap record (numbers 3 and 4 were shed); chunk 1 goes on at 5.
    b.put(&chunk_key(RUN, 0), numbered(0, INC_A, &[0, 1, 2], &[2]));
    b.put(&chunk_key(RUN, 1), numbered(1, INC_A, &[5, 6], &[]));
    let p = page(&t, &b, &[]).await;
    assert!(
        p.gaps.is_empty(),
        "the gap record that ends chunk 0 explains the skip to 5: {:?}",
        p.gaps
    );
    assert_eq!(p.records.len(), 5);
    // A read that resumes just after a gap record in the middle of a chunk is not blamed for the numbers it shed.
    let b = Bucket::default();
    b.put(
        &chunk_key(RUN, 0),
        numbered(0, INC_A, &[0, 1, 2, 5, 6], &[2]),
    );
    let at = cursor::encode(
        RUN,
        ME,
        &Position {
            seq: 0,
            idx: 3,
            after_key: None,
            etag: None,
        },
    );
    let q = page(&t, &b, &[("after", &at)]).await;
    assert_eq!(
        q.records.iter().map(|r| r.n).collect::<Vec<_>>(),
        vec![5, 6]
    );
    assert!(q.gaps.is_empty(), "{:?}", q.gaps);
}

#[tokio::test]
async fn b776_a_record_of_300_kib_is_shown_whole_with_the_default_limits() {
    let t = tables(GoalStatus::Active);
    let b = Bucket::default();
    load(&b, RUN, "oversized-record");
    let p = page(&t, &b, &[]).await;
    assert_eq!(p.records.len(), 2);
    assert_eq!(
        p.records[0].kind, "agent_message_chunk",
        "the record is not replaced"
    );
    assert!(p.records[0].data["content"]["text"].as_str().unwrap().len() > 262_144);
    assert_eq!(p.records[1].data["content"]["text"], "small");
}

#[test]
fn b776_the_record_limit_is_the_page_budget_of_the_route() {
    use control_api::transcript::{METADATA_RESERVE, RESPONSE_BUDGET};
    let l = Limits::default();
    assert_eq!(l.record_bytes, RESPONSE_BUDGET - METADATA_RESERVE);
    assert_eq!(l.record_bytes, l.page_bytes);
}
