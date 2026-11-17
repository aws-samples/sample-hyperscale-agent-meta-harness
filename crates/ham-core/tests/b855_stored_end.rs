//! The end of a stored conversation is read by what the run's last session did.
//!
//! The read route says when the service invoked the run's last session (`lastSessionInvokedAt`). An `end` record that is no older was
//! written by that session: the conversation is closed, and the notice that its end may be missing is not shown, also for a run whose
//! events were not read and for a last session that wrote no start event. An `end` record that is older belongs to an earlier
//! session: the notice stays. A declared open tail, wherever it lies, keeps the conversation from being summed as whole.
#![allow(clippy::unwrap_used)]

use ham_core::chat::GapCause;
use ham_core::stored::{Read, StoredChat};
use ham_core::transcript::{
    CompletenessEntry, Dir, GapKind, TranscriptPage, TranscriptRecord, TranscriptState,
};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
/// 2026-11-14T07:00:00Z in Unix seconds.
const T0: i64 = 1_794_639_600;
const INC1: &str = "01M5A2R7K3C9XJ4D8E6F000001";
const INC2: &str = "01M5A2R7K3C9XJ4D8E6F000002";
const EARLIER: &str =
    "[an earlier session stopped without closing its conversation: its last lines may be missing]";
const CLOSED: &str = "the conversation was closed cleanly";

/// A record of chunk `seq`, written at 07:(03 + seq):00.
fn rec(seq: u64, inc: &str, n: u64, kind: &str, data: Value) -> TranscriptRecord {
    TranscriptRecord {
        seq,
        n,
        incarnation: inc.to_string(),
        origin: ham_core::transcript::Origin::Engine,
        t: Some(format!("2026-11-14T07:{:02}:00Z", 3 + seq)),
        dir: Dir::In,
        kind: kind.to_string(),
        data,
    }
}

fn msg(seq: u64, inc: &str, n: u64, text: &str) -> TranscriptRecord {
    rec(
        seq,
        inc,
        n,
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": format!("m{seq}"), "content": {"type": "text", "text": text}}),
    )
}

fn answered(seq: u64, inc: &str, n: u64, tokens: u64) -> TranscriptRecord {
    rec(
        seq,
        inc,
        n,
        "prompt_result",
        json!({"result": {"stopReason": "end_turn", "usage": {"totalTokens": tokens}}}),
    )
}

fn page(
    state: TranscriptState,
    records: Vec<TranscriptRecord>,
    completeness: Vec<CompletenessEntry>,
    last_session_invoked_at: Option<&str>,
) -> TranscriptPage {
    TranscriptPage {
        run_id: RUN.into(),
        state,
        records,
        gaps: vec![],
        completeness,
        published_through: None,
        listed_through: None,
        next: None,
        prev: None,
        more: false,
        skipped_keys: 0,
        last_session_invoked_at: last_session_invoked_at.map(str::to_string),
    }
}

fn excluded() -> CompletenessEntry {
    CompletenessEntry {
        kind: GapKind::Excluded,
        from_seq: None,
        to_seq: None,
        count: Some(3),
    }
}

fn tail(after: Option<u64>) -> CompletenessEntry {
    CompletenessEntry {
        kind: GapKind::TailUnknown,
        from_seq: after,
        to_seq: after,
        count: None,
    }
}

fn stored(p: TranscriptPage) -> StoredChat {
    let mut c = StoredChat::new();
    c.absorb(&p, Read::First);
    c
}

/// The bodies of the conversation of a run whose events say that its last session began at `began` (`None`: they do not say).
fn bodies(p: TranscriptPage, began: Option<i64>) -> Vec<String> {
    stored(p)
        .blocks_for(began)
        .into_iter()
        .map(|b| b.body)
        .collect()
}

fn open_end(shown: &[String]) -> bool {
    shown.iter().any(|b| b == GapCause::TailUnknown.words())
}

/// Session 1 (chunk 0, 07:03) was lost with its conversation open. Session 2 said so in its first record (chunk 1, 07:04) and closed
/// its own conversation (the `end` record of chunk 2, 07:05).
fn lost_then_closed() -> Vec<TranscriptRecord> {
    vec![
        msg(0, INC1, 0, "session one works"),
        rec(
            1,
            INC2,
            0,
            "gap",
            json!({"kind": "tail-unknown", "afterSeq": 0, "reason": "a previous writer ended without a final flush"}),
        ),
        msg(1, INC2, 1, "session two goes on"),
        rec(2, INC2, 2, "end", json!({})),
    ]
}

/// Session 1 closed its conversation (the `end` record of chunk 1, 07:04) and handed off; nothing of a later session is stored.
fn closed_then_nothing() -> Vec<TranscriptRecord> {
    vec![
        msg(0, INC1, 0, "session one works"),
        rec(1, INC1, 1, "end", json!({})),
    ]
}

#[test]
fn b855_no_end_notice_for_a_closed_last_session() {
    // The page of a run whose first session was lost and whose second closed: the route names the located open tail and no open end,
    // and says that the last session was invoked at 07:04. No event of the run is known here.
    let shown = bodies(
        page(
            TranscriptState::Incomplete,
            lost_then_closed(),
            vec![tail(Some(0)), excluded()],
            Some("2026-11-14T07:04:00Z"),
        ),
        None,
    );
    assert_eq!(
        shown.iter().filter(|b| b.starts_with(EARLIER)).count(),
        1,
        "the open tail of session 1 is said once, where it happened: {shown:#?}"
    );
    assert_eq!(shown.last().map(String::as_str), Some(CLOSED), "{shown:#?}");
    assert!(!open_end(&shown), "{shown:#?}");

    // The last session lost its end: the route names an open end, and the stored `end` (07:04) is older than the last session
    // (invoked at 07:10, it stored nothing). The notice stays, although no event names session 2.
    let lost_end = || {
        page(
            TranscriptState::Incomplete,
            closed_then_nothing(),
            vec![excluded(), tail(None)],
            Some("2026-11-14T07:10:00Z"),
        )
    };
    let shown = bodies(lost_end(), None);
    assert!(open_end(&shown), "{shown:#?}");
    // The run's events know only of session 1 (begun at 07:03): the service's later time counts.
    let shown = bodies(lost_end(), Some(T0 + 180));
    assert!(
        open_end(&shown),
        "the later of the two times decides: {shown:#?}"
    );

    // A run of one session that closed its conversation, and whose summary was not stored, so the route names an open end. The `end`
    // record (07:04) is no older than the session (invoked at 07:03): the conversation is closed, with no event read.
    let one = |invoked: Option<&str>| {
        page(
            TranscriptState::Incomplete,
            closed_then_nothing(),
            vec![tail(None)],
            invoked,
        )
    };
    let shown = bodies(one(Some("2026-11-14T07:03:00Z")), None);
    assert_eq!(shown.last().map(String::as_str), Some(CLOSED), "{shown:#?}");
    assert!(!open_end(&shown), "{shown:#?}");
    // The events' time counts when it is the later one: they say a session began at 07:09, after the `end`.
    let shown = bodies(one(Some("2026-11-14T07:03:00Z")), Some(T0 + 540));
    assert!(open_end(&shown), "{shown:#?}");

    // Without the member (a service that predates it) the rule is the one of the events alone: no time, the notice stays; a session
    // that began before the `end`, the notice goes.
    assert!(open_end(&bodies(one(None), None)));
    assert!(!open_end(&bodies(one(None), Some(T0 + 180))));
    // A time that is no UTC time is no time.
    assert!(open_end(&bodies(one(Some("soon")), None)));
}

/// The tokens of the prompts that answered are summed only for a conversation that is whole. A declared open tail is a hole, whatever
/// the state of the page says.
#[test]
fn b855_a_declared_open_tail_keeps_the_answered_sum_away() {
    let records = || {
        vec![
            answered(0, INC1, 0, 1_000),
            answered(1, INC2, 1, 2_500),
            rec(2, INC2, 2, "end", json!({})),
        ]
    };
    // TRIGGER PRESENT: two closed sessions, read whole.
    let whole = stored(page(
        TranscriptState::Complete,
        records(),
        vec![excluded(), excluded()],
        Some("2026-11-14T07:04:00Z"),
    ));
    assert_eq!(whole.answered_tokens(), Some(3_500));
    // A located open tail of an earlier session, and an open end, each keep the sum away, also under the state `complete`.
    for entry in [tail(Some(0)), tail(None)] {
        let holed = stored(page(
            TranscriptState::Complete,
            records(),
            vec![excluded(), entry],
            Some("2026-11-14T07:04:00Z"),
        ));
        assert_eq!(holed.answered_tokens(), None);
    }
}
