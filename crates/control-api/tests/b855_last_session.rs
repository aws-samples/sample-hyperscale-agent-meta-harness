//! The state of a stored conversation is read from every invoked session, and the newest one decides its end.
//!
//! A run is a list of sessions, and each stores the summary of its conversation on its own Turn item. The route read the first
//! session's summary only. A run whose first session was lost therefore always read `incomplete` with the notice that its end may be
//! missing, although its last session closed the conversation; and a run whose first session handed off and whose second lost its end
//! read `complete`. Here the route gets the facts from the test's store (`TranscriptFacts`): the newest invoked session's summary, the
//! entries the earlier sessions declared, and whether an earlier session left none. `tests/transcript_aws.rs` holds the adapter that
//! makes these facts from the Turn items.
#![allow(clippy::unwrap_used)]

mod support;

use control_api::handler::{Deps, V1Request, handle_logged};
use control_api::transcript::{SummaryFact, TranscriptFacts};
use ham_core::transcript::{
    CompletenessEntry, GapCode, GapKind, MAX_COMPLETENESS_ENTRIES, TranscriptPage, TranscriptState,
    TranscriptSummary, chunk_key, parse_page, seq_of_name,
};
use harness_protocol::store::RunItem;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use support::{Bucket, NoExecution, Tables};

const ME: &str = "arn:aws:sts::123456789012:assumed-role/Admin/tester";
const RUN: &str = "01K83D1111111111111111111A";
const INVOKED_2: &str = "2026-11-16T07:15:34Z";

fn run_item(status: &str) -> RunItem {
    serde_json::from_value(json!({
        "runId": RUN, "ownerPrincipalArn": ME, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": status, "phase": "green",
        "enginePolicyVersion": 1, "createdAt": "2026-11-16T07:00:00Z", "version": 1
    }))
    .unwrap()
}

/// A table with one run of mine in `status` and these facts about its conversation.
fn tables(status: &str, facts: TranscriptFacts) -> Tables {
    let t = Tables::default();
    t.runs.lock().unwrap().insert(RUN.into(), run_item(status));
    t.facts.lock().unwrap().insert(RUN.into(), facts);
    t
}

/// The chunks 0 to 2 of the fixture run, where the writer puts them (every session of a run writes into the one prefix).
fn bucket() -> Bucket {
    let b = Bucket::default();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../ham-core/tests/fixtures/transcript/good");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_str().unwrap().to_string();
        let Some(name) = file.strip_suffix(".hex") else {
            continue;
        };
        let seq = seq_of_name(name).unwrap();
        let bytes = ham_core::gzip::unhex(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(b.put_if_absent(&chunk_key(RUN, seq), bytes));
    }
    b
}

async fn page(t: &Tables, b: &Bucket) -> TranscriptPage {
    let deps: Deps<&Tables, &Bucket, NoExecution> = Deps {
        store: t,
        blobs: b,
        execution: NoExecution,
        account: "123456789012".into(),
        operators: vec![],
        repo_names: Default::default(),
    };
    let req = V1Request {
        method: "GET".into(),
        path: format!("/v1/runs/{RUN}/transcript"),
        caller_arn: Some(ME.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: BTreeMap::new(),
        body: String::new(),
    };
    let (resp, _) = handle_logged(req, &deps).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    parse_page(resp.body.as_bytes()).unwrap()
}

fn excluded(count: u64) -> CompletenessEntry {
    CompletenessEntry {
        kind: GapKind::Excluded,
        from_seq: None,
        to_seq: None,
        count: Some(count),
    }
}

/// An open tail: of an earlier session when it names the chunk it follows, of the run's last session when it names none.
fn tail(after: Option<u64>) -> CompletenessEntry {
    CompletenessEntry {
        kind: GapKind::TailUnknown,
        from_seq: after,
        to_seq: after,
        count: None,
    }
}

fn summary(last: u64, entries: Vec<CompletenessEntry>) -> SummaryFact {
    SummaryFact::Valid(TranscriptSummary {
        last_transcript_seq: Some(last),
        entries,
        answered_tokens: None,
    })
}

fn facts(
    newest: SummaryFact,
    earlier: Vec<CompletenessEntry>,
    earlier_unreported: bool,
) -> TranscriptFacts {
    TranscriptFacts {
        supported: true,
        purged: false,
        summary: newest,
        earlier,
        earlier_unreported,
        last_session_invoked_at: Some(INVOKED_2.into()),
    }
}

#[tokio::test]
async fn b855_the_last_session_without_a_summary_reads_incomplete() {
    // Session 1 handed off with a lossless summary; session 2 lost its end. The run has ended.
    let lost_end = || facts(SummaryFact::Absent, vec![excluded(4)], false);
    for status in ["complete", "blocked", "cleared", "stalled"] {
        let p = page(&tables(status, lost_end()), &bucket()).await;
        assert_eq!(p.state, TranscriptState::Incomplete, "{status}");
        assert_eq!(
            p.completeness,
            vec![excluded(4), tail(None)],
            "{status}: session 1's entries, then the open end of the last session"
        );
        assert_eq!(p.last_session_invoked_at.as_deref(), Some(INVOKED_2));
    }
    // The same facts for a run that has not ended: its second session is still writing, so nothing says that its end is open.
    let p = page(&tables("active", lost_end()), &bucket()).await;
    assert_eq!(p.state, TranscriptState::Live);
    assert_eq!(p.completeness, vec![excluded(4)]);
    // A newest summary that is not well formed counts as none.
    let p = page(
        &tables(
            "complete",
            facts(SummaryFact::Invalid, vec![excluded(4)], false),
        ),
        &bucket(),
    )
    .await;
    assert_eq!(p.state, TranscriptState::Incomplete);
    assert_eq!(p.completeness, vec![excluded(4), tail(None)]);
}

#[tokio::test]
async fn b855_a_lost_first_session_leaves_no_open_end() {
    // Session 1 was lost and left no summary. Session 2 found its last chunk and declared the open tail in place, then closed.
    let t = tables(
        "complete",
        facts(summary(2, vec![tail(Some(1)), excluded(3)]), vec![], true),
    );
    let p = page(&t, &bucket()).await;
    assert_eq!(
        p.state,
        TranscriptState::Incomplete,
        "a located hole is still a hole"
    );
    assert_eq!(p.completeness, vec![tail(Some(1)), excluded(3)]);
    assert!(
        p.completeness
            .iter()
            .all(|e| !(e.kind == GapKind::TailUnknown && e.from_seq.is_none())),
        "no open tail without a chunk: the last session closed its conversation"
    );
    assert_eq!(p.last_session_invoked_at.as_deref(), Some(INVOKED_2));
    // Session 2 could not list the stored chunks at its start, so it declared no open tail: the page still reads incomplete (an
    // earlier session left no summary), and it guesses no place.
    let t = tables(
        "complete",
        facts(summary(2, vec![excluded(3)]), vec![], true),
    );
    let p = page(&t, &bucket()).await;
    assert_eq!(p.state, TranscriptState::Incomplete);
    assert_eq!(p.completeness, vec![excluded(3)]);
}

#[tokio::test]
async fn b855_two_closed_sessions_read_complete_and_keep_declared_losses() {
    // Both sessions closed and declared no loss.
    let t = tables(
        "complete",
        facts(summary(2, vec![excluded(3)]), vec![excluded(4)], false),
    );
    let p = page(&t, &bucket()).await;
    assert_eq!(p.state, TranscriptState::Complete);
    assert_eq!(p.completeness, vec![excluded(4), excluded(3)]);
    assert!(p.gaps.is_empty(), "{:?}", p.gaps);
    // Session 1 declared a loss: it stays declared, and the run reads incomplete although the last session lost nothing.
    let failed = CompletenessEntry {
        kind: GapKind::Failed,
        from_seq: Some(1),
        to_seq: Some(1),
        count: None,
    };
    let t = tables(
        "complete",
        facts(
            summary(2, vec![excluded(3)]),
            vec![failed.clone(), excluded(4)],
            false,
        ),
    );
    let p = page(&t, &bucket()).await;
    assert_eq!(p.state, TranscriptState::Incomplete);
    assert_eq!(p.completeness, vec![failed, excluded(4), excluded(3)]);
    // The tail the worker declared and the prefix lacks is measured against the last session's last chunk: the prefix holds the
    // chunks 0 to 2, and the last session says it wrote up to chunk 4.
    let t = tables(
        "complete",
        facts(summary(4, vec![excluded(3)]), vec![excluded(4)], false),
    );
    let p = page(&t, &bucket()).await;
    assert_eq!(p.gaps.len(), 1, "{:?}", p.gaps);
    assert_eq!(
        (p.gaps[0].code, p.gaps[0].from_seq, p.gaps[0].to_seq),
        (GapCode::TailMissing, 3, 4)
    );
}

/// A page carries a bounded list of entries. When the sessions declared more, the oldest give way, and the page says less than was
/// declared: it does not read complete.
#[tokio::test]
async fn b855_too_many_entries_read_incomplete() {
    let earlier: Vec<CompletenessEntry> =
        (0..MAX_COMPLETENESS_ENTRIES as u64).map(excluded).collect();
    let t = tables(
        "complete",
        facts(
            summary(2, vec![excluded(9_001), excluded(9_002)]),
            earlier,
            false,
        ),
    );
    let p = page(&t, &bucket()).await;
    assert_eq!(p.completeness.len(), MAX_COMPLETENESS_ENTRIES);
    assert_eq!(
        p.completeness.first(),
        Some(&excluded(2)),
        "the two oldest entries gave way"
    );
    assert_eq!(p.completeness.last(), Some(&excluded(9_002)));
    assert_eq!(
        p.state,
        TranscriptState::Incomplete,
        "every entry that is left is an exclusion, and the list is cut"
    );
    // TRIGGER PRESENT: the same entries within the bound read complete.
    let earlier: Vec<CompletenessEntry> = (0..(MAX_COMPLETENESS_ENTRIES as u64 - 2))
        .map(excluded)
        .collect();
    let t = tables(
        "complete",
        facts(
            summary(2, vec![excluded(9_001), excluded(9_002)]),
            earlier,
            false,
        ),
    );
    assert_eq!(page(&t, &bucket()).await.state, TranscriptState::Complete);
}
