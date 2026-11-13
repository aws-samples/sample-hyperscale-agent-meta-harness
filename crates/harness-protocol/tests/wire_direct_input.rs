//! Protocol 1.37.0: the wire shapes of `needs-input`, its acknowledgements, the answer, the control, the run
//! list member and the summary members. The golden files in `fixtures/direct-input/` hold the bytes the writers (agentd, control-api, ham)
//! must produce and the readers must accept: each parses into its type and writes back the same JSON.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::{InputAnswer, ListRunsResponse};
use harness_protocol::direct::{
    DirectAttemptKind, DirectFreshSession, DirectIntervention, InputClosedPayload,
    InputIgnoredPayload, InputReceivedPayload, InstructAction, InstructOrigin, NeedsInputPayload,
    NeedsInputReason, NewSessionPayload,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, validate};
use harness_protocol::store::ControlItem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/direct-input")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn envelope(name: &str) -> Envelope {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn value_of(e: &Envelope) -> Value {
    match &e.payload {
        Payload::Inline { value } => value.clone(),
        Payload::ClaimCheck { .. } => panic!("a golden event is inline"),
    }
}

/// The file's bytes equal the pretty JSON of its type (the order of the members is the order of the struct).
fn assert_bytes_round_trip<T: Serialize + for<'de> Deserialize<'de>>(name: &str) {
    let text = fixture(name);
    let typed: T = serde_json::from_str(&text).unwrap();
    let mut again = serde_json::to_string_pretty(&typed).unwrap();
    again.push('\n');
    assert_eq!(again, text, "{name} does not write back byte for byte");
}

fn assert_event_round_trip<T: Serialize + for<'de> Deserialize<'de>>(name: &str) -> T {
    let e = envelope(name);
    validate(&e).unwrap_or_else(|err| panic!("{name}: {err}"));
    assert_eq!(e.event_type, EventType::TurnProgress, "{name}");
    assert_eq!(e.authority, Authority::Agent, "{name}");
    assert_eq!(e.agent_id.as_deref(), Some("direct"), "{name}");
    assert_eq!(e.run_id, RUN, "{name}");
    let v = value_of(&e);
    let typed: T = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), v, "{name}");
    // the envelope itself writes back to the same JSON
    let whole: Value = serde_json::from_str(&fixture(name)).unwrap();
    assert_eq!(serde_json::to_value(&e).unwrap(), whole, "{name}");
    typed
}

#[test]
fn the_protocol_is_1_38_0() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
}

#[test]
fn the_needs_input_golden_files_round_trip() {
    let p: NeedsInputPayload = assert_event_round_trip("needs-input-engine-stop.json");
    assert_eq!(p.request_id, REQ);
    assert_eq!(p.reason, "engine_stop");
    assert!(p.blocking && p.can_continue);
    assert_eq!(p.options, InstructAction::ALL.to_vec());
    assert_eq!(p.models.len(), 2);
    assert_eq!(p.evidence.identical_calls, Some(5));
    assert_eq!(p.evidence.failed_turns, None);
    assert_eq!(p.recommended, None);
    assert!(p.question.is_none(), "no question member in v1");
    assert_eq!(p.wait_s, 1800);
    assert_eq!(p.expires_at.as_deref(), Some("2026-10-15T14:11:07Z"));
    // the event id is the request id
    assert_eq!(
        envelope("needs-input-engine-stop.json").event_id,
        p.request_id
    );

    let r: NeedsInputPayload = assert_event_round_trip("needs-input-replay-rejected.json");
    assert_eq!(r.reason, "replay_rejected");
    assert_eq!(r.n, 2);
    assert!(r.last_message.is_none());
}

#[test]
fn the_outage_notice_is_not_blocking_and_has_no_window() {
    let n: NeedsInputPayload = assert_event_round_trip("needs-input-outage-notice.json");
    assert_eq!(n.reason, "provider_outage");
    assert!(!n.blocking);
    assert!(n.options.is_empty() && n.models.is_empty());
    assert_eq!((n.wait_s, n.can_continue), (0, false));
    assert_eq!(n.expires_at, None);
    assert!(!NeedsInputReason::ProviderOutage.is_blocking());
}

#[test]
fn the_acknowledgement_and_closing_golden_files_round_trip() {
    let r: InputReceivedPayload = assert_event_round_trip("input-received.json");
    assert_eq!(
        (r.action, r.origin, r.text_bytes, r.waited_s),
        (
            InstructAction::ContinueWithPrompt,
            InstructOrigin::User,
            88,
            93
        )
    );
    assert!(r.model.is_none());
    let t: InputReceivedPayload = assert_event_round_trip("input-received-try-model.json");
    assert_eq!(t.action, InstructAction::TryModel);
    assert_eq!(t.model.as_deref(), Some("us.anthropic.claude-opus-5-5"));

    let i: InputIgnoredPayload = assert_event_round_trip("input-ignored.json");
    assert_eq!(i.why, "window-closed");

    let c: InputClosedPayload = assert_event_round_trip("input-closed.json");
    assert_eq!((c.how.as_str(), c.waited_s), ("expired", 1800));
}

#[test]
fn the_new_session_golden_files_round_trip() {
    let u: NewSessionPayload = assert_event_round_trip("new-session-user.json");
    assert_eq!(
        (u.session, u.trigger.as_str(), u.restarted_process),
        (2, "user", false)
    );
    assert_eq!(u.request_id.as_deref(), Some(REQ));
    let h: NewSessionPayload = assert_event_round_trip("new-session-harness.json");
    assert_eq!((h.trigger.as_str(), h.request_id), ("harness", None));
}

#[test]
fn the_control_and_the_answer_write_back_byte_for_byte() {
    assert_bytes_round_trip::<ControlItem>("control-instruct.json");
    assert_bytes_round_trip::<InputAnswer>("answer-request.json");
    assert_bytes_round_trip::<ListRunsResponse>("list-needs-input.json");

    let item: ControlItem = serde_json::from_str(&fixture("control-instruct.json")).unwrap();
    assert_eq!(item.control_id, REQ);
    match item.control {
        PendingControl::Instruct {
            request_id,
            action,
            text,
            model,
            origin,
        } => {
            assert_eq!(request_id, REQ, "the control id is the request id");
            assert_eq!(action, InstructAction::NewSessionWithPrompt);
            assert!(text.unwrap().starts_with("Run the sleeps"));
            assert_eq!(model, None);
            assert_eq!(origin, InstructOrigin::User);
        }
        other => panic!("not an instruct: {other:?}"),
    }
    let rows: ListRunsResponse = serde_json::from_str(&fixture("list-needs-input.json")).unwrap();
    let brief = rows.runs[0].needs_input.as_ref().unwrap();
    assert_eq!(
        (brief.reason.as_str(), brief.request_id.as_str()),
        ("engine_stop", REQ)
    );
}

#[test]
fn an_answer_refuses_unknown_members_and_an_origin() {
    let mut v: Value = serde_json::from_str(&fixture("answer-request.json")).unwrap();
    v["origin"] = json!("analyzer");
    assert!(
        serde_json::from_value::<InputAnswer>(v).is_err(),
        "a client cannot set the origin"
    );
    let finish = json!({"requestId": REQ, "action": "finish"});
    let a: InputAnswer = serde_json::from_value(finish.clone()).unwrap();
    assert_eq!(serde_json::to_value(&a).unwrap(), finish);
    assert!(
        serde_json::from_value::<InputAnswer>(json!({"requestId": REQ, "action": "dance"}))
            .is_err()
    );
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryParts {
    interventions: Vec<DirectIntervention>,
    fresh_sessions: Vec<DirectFreshSession>,
    usage: harness_protocol::common::Usage,
}

#[test]
fn the_summary_members_write_back_byte_for_byte() {
    assert_bytes_round_trip::<SummaryParts>("summary-interventions.json");
    let p: SummaryParts = serde_json::from_str(&fixture("summary-interventions.json")).unwrap();
    assert_eq!(
        p.interventions[0].action,
        Some(InstructAction::NewSessionWithPrompt)
    );
    assert_eq!(p.fresh_sessions[0].n, 2);
    assert_eq!(p.usage.input_tokens, 1_204_331);
}

#[test]
fn the_words_on_the_wire_are_the_ones_the_design_names() {
    assert_eq!(
        InstructAction::ALL.map(InstructAction::as_str),
        [
            "continue_with_prompt",
            "new_session_with_prompt",
            "try_model",
            "finish"
        ]
    );
    for a in InstructAction::ALL {
        assert_eq!(serde_json::to_value(a).unwrap(), json!(a.as_str()));
        assert_eq!(InstructAction::from_wire(a.as_str()), Some(a));
    }
    for r in NeedsInputReason::ALL {
        assert_eq!(serde_json::to_value(r).unwrap(), json!(r.as_str()));
        assert_eq!(NeedsInputReason::from_wire(r.as_str()), Some(r));
    }
    assert_eq!(NeedsInputReason::ALL.len(), 14);
    assert_eq!(
        serde_json::to_value(DirectAttemptKind::FreshSession).unwrap(),
        json!("fresh_session")
    );
    assert_eq!(
        serde_json::to_value(DirectAttemptKind::Instruction).unwrap(),
        json!("instruction")
    );
    assert_eq!(harness_protocol::direct::MAX_INPUT_REQUESTS, 3);
    assert_eq!(harness_protocol::direct::MAX_USER_SESSIONS, 3);
    assert_eq!(harness_protocol::direct::MAX_AUTO_SESSIONS, 1);
    assert_eq!(harness_protocol::direct::DEFAULT_CLIENT_INPUT_WAIT_S, 1800);
}

#[test]
fn the_a15_error_text_is_pinned() {
    let t = fixture("a15-error-text.txt");
    assert_eq!(
        t.trim_end(),
        "engine error -32603: Internal error: The format of the value at messages.7.content.0.toolUse.input is invalid. Provide a json object for the field and try again."
    );
}

#[test]
fn the_answer_route_is_listed_and_in_the_schema() {
    let route = harness_protocol::api::V1_ROUTES
        .iter()
        .find(|r| r.operation_id == "answerInput")
        .unwrap();
    assert_eq!(
        (route.method, route.path),
        ("POST", "/v1/runs/{runId}/input")
    );
}

// ---- behavior (red first): parse_utc_unix, lenient_pending, bounded, fold_input -----------------------------------------------------

mod behavior {
    #![allow(clippy::unwrap_used)]

    use super::{REQ, RUN, envelope, fixture};
    use harness_protocol::api::internal::{
        EventsBatchResponse, PendingControl, lenient_pending, split_pending,
    };
    use harness_protocol::common::parse_utc_unix;
    use harness_protocol::direct::{
        InputFold, InstructAction, InstructOrigin, MAX_FOLD_CLOSED, NEEDS_INPUT_PAYLOAD_BUDGET,
        NeedsInputModel, NeedsInputPayload, fold_input,
    };
    use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
    use serde_json::{Value, json};

    // ---- parse_utc_unix ----

    #[test]
    fn parse_utc_unix_reads_known_instants() {
        for (ts, want) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1999-12-31T23:59:59Z", 946_684_799),
            ("2000-03-01T00:00:00Z", 951_868_800),
            ("2024-02-29T23:59:59Z", 1_709_251_199),
            ("2026-10-15T13:41:07Z", 1_792_071_667),
            ("2026-11-01T08:00:00Z", 1_793_520_000),
        ] {
            assert_eq!(parse_utc_unix(ts), Some(want), "{ts}");
        }
    }

    #[test]
    fn parse_utc_unix_ignores_a_fraction_and_gives_window_arithmetic() {
        assert_eq!(
            parse_utc_unix("2026-10-15T13:41:07.250Z"),
            parse_utc_unix("2026-10-15T13:41:07Z")
        );
        let opened = parse_utc_unix("2026-10-15T13:41:07Z").unwrap();
        let expires = parse_utc_unix("2026-10-15T14:11:07Z").unwrap();
        assert_eq!(expires - opened, 1800);
    }

    #[test]
    fn parse_utc_unix_refuses_what_is_not_an_rfc3339_utc_timestamp() {
        for bad in [
            "",
            "not a time",
            "2026-13-01T00:00:00Z",
            "2026-10-15T13:41:07",
            "2026-10-15T13:41:07+00:00",
            "2026-10-15 13:41:07Z",
        ] {
            assert_eq!(parse_utc_unix(bad), None, "{bad:?}");
        }
    }

    // ---- lenient_pending ----

    fn batch(items: Value) -> Value {
        json!({"accepted": 5, "pending": items})
    }

    #[test]
    fn lenient_pending_keeps_a_cancel_beside_items_it_cannot_read() {
        let body = batch(json!([
            {"kind": "future-thing", "x": 1},
            {"kind": "cancel"},
            {"kind": "instruct", "requestId": REQ, "action": "dance"},
            {"kind": "steer", "text": "hello", "now": true},
            {"kind": "instruct", "requestId": REQ, "action": "finish"}
        ]));
        let r: EventsBatchResponse = serde_json::from_value(body).unwrap();
        assert_eq!(r.accepted, 5);
        assert_eq!(
            r.pending,
            vec![
                PendingControl::Cancel,
                PendingControl::Steer {
                    text: "hello".into(),
                    now: true
                },
                PendingControl::Instruct {
                    request_id: REQ.into(),
                    action: InstructAction::Finish,
                    text: None,
                    model: None,
                    origin: InstructOrigin::User,
                },
            ]
        );
    }

    #[test]
    fn split_pending_returns_the_items_it_skipped_in_order() {
        let items = vec![
            json!({"kind": "future-thing"}),
            json!({"kind": "cancel"}),
            json!({"kind": "instruct", "requestId": REQ, "action": "dance"}),
        ];
        let (ok, skipped) = split_pending(items.clone());
        assert_eq!(ok, vec![PendingControl::Cancel]);
        assert_eq!(skipped, vec![items[0].clone(), items[2].clone()]);
    }

    #[test]
    fn lenient_pending_still_refuses_a_list_that_is_not_an_array() {
        let e = serde_json::from_value::<EventsBatchResponse>(batch(json!({"kind": "cancel"})));
        assert!(e.is_err());
        let mut d = serde_json::Deserializer::from_str("42");
        assert!(lenient_pending(&mut d).is_err());
    }

    #[test]
    fn a_pre_1_37_body_reads_as_before_and_an_instruct_without_an_origin_is_the_users() {
        let old: EventsBatchResponse = serde_json::from_value(json!({"accepted": 1, "pending": [
            {"kind": "cancel"}, {"kind": "approvalAnswer", "approvalId": "a1", "decision": "approve"}
        ]}))
        .unwrap_or_else(|_| {
            serde_json::from_value(json!({"accepted": 1, "pending": [{"kind": "cancel"}]})).unwrap()
        });
        assert_eq!(old.pending[0], PendingControl::Cancel);
        let none: EventsBatchResponse = serde_json::from_value(json!({"accepted": 0})).unwrap();
        assert!(none.pending.is_empty());
        let i: PendingControl = serde_json::from_value(
            json!({"kind": "instruct", "requestId": REQ, "action": "continue_with_prompt", "text": "go on"}),
        )
        .unwrap();
        assert!(
            matches!(i, PendingControl::Instruct { origin: InstructOrigin::User, ref text, .. } if text.as_deref() == Some("go on"))
        );
    }

    // ---- NeedsInputPayload::bounded ----

    fn golden_payload() -> NeedsInputPayload {
        let e = envelope("needs-input-engine-stop.json");
        match e.payload {
            Payload::Inline { value } => serde_json::from_value(value).unwrap(),
            Payload::ClaimCheck { .. } => unreachable!(),
        }
    }

    #[test]
    fn a_payload_inside_its_limits_and_budget_is_returned_unchanged() {
        let p = golden_payload();
        assert!(p.json_len() <= NEEDS_INPUT_PAYLOAD_BUDGET);
        assert_eq!(p.clone().bounded(NEEDS_INPUT_PAYLOAD_BUDGET), p);
    }

    #[test]
    fn bounded_shrinks_in_the_documented_order_and_keeps_the_fixed_members() {
        let p = golden_payload();
        let no_msg = NeedsInputPayload {
            last_message: None,
            ..p.clone()
        };
        let no_msg_no_files = NeedsInputPayload {
            files: vec![],
            ..no_msg.clone()
        };
        let no_msg_no_files_no_err = NeedsInputPayload {
            evidence: harness_protocol::direct::NeedsInputEvidence {
                last_error: None,
                ..no_msg_no_files.evidence.clone()
            },
            ..no_msg_no_files.clone()
        };

        // 1. only the last message gives way
        let a = p.clone().bounded(no_msg.json_len() + 12);
        assert_eq!(a.files, p.files);
        assert_eq!(a.evidence.last_error, p.evidence.last_error);
        let tail = a.last_message.clone().unwrap_or_default();
        assert!(tail.len() <= 12, "the tail is cut: {tail:?}");
        assert!(
            p.last_message.as_deref().unwrap().ends_with(&tail),
            "the TAIL is kept"
        );

        // 2. then the files (filesTotal stays)
        let b = p.clone().bounded(no_msg_no_files.json_len());
        assert_eq!(b.last_message, None);
        assert!(b.files.is_empty());
        assert_eq!(b.files_total, p.files_total);
        assert_eq!(b.evidence.last_error, p.evidence.last_error);

        // 3. then the last error
        let c = p.clone().bounded(no_msg_no_files_no_err.json_len());
        assert_eq!(c.evidence.last_error, None);
        assert_eq!(c.title, p.title);

        // the fixed members never change
        for shrunk in [&a, &b, &c] {
            assert_eq!(
                (
                    &shrunk.kind,
                    &shrunk.request_id,
                    shrunk.n,
                    &shrunk.reason,
                    shrunk.blocking,
                    &shrunk.options,
                    shrunk.can_continue,
                    shrunk.wait_s,
                    &shrunk.expires_at,
                    &shrunk.text
                ),
                (
                    &p.kind,
                    &p.request_id,
                    p.n,
                    &p.reason,
                    p.blocking,
                    &p.options,
                    p.can_continue,
                    p.wait_s,
                    &p.expires_at,
                    &p.text
                )
            );
            assert_eq!(
                shrunk.models.iter().map(|m| &m.id).collect::<Vec<_>>(),
                p.models.iter().map(|m| &m.id).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn four_byte_code_points_everywhere_still_fit_one_inline_event() {
        let wide = |n: usize| "\u{1F600}".repeat(n);
        let mut p = golden_payload();
        p.text = wide(300);
        p.title = wide(300);
        p.last_message = Some(wide(900));
        p.files = (0..40).map(|_| wide(200)).collect();
        p.files_total = 40;
        p.evidence.last_error = Some(wide(400));
        p.question = Some(wide(500));
        p.models = (0..12)
            .map(|i| NeedsInputModel {
                id: format!("{}{i}", wide(30)),
                name: wide(80),
                context_tokens: 1_000_000,
            })
            .collect();
        let b = p.clone().bounded(NEEDS_INPUT_PAYLOAD_BUDGET);
        assert!(
            b.json_len() <= harness_protocol::envelope::MAX_INLINE_PAYLOAD_BYTES,
            "{} bytes",
            b.json_len()
        );
        assert!(
            b.json_len() <= NEEDS_INPUT_PAYLOAD_BUDGET,
            "{} bytes",
            b.json_len()
        );
        // limits applied
        assert_eq!(b.text.chars().count(), 160);
        assert_eq!(b.models.len(), 8);
        // fixed members intact
        assert_eq!(
            (
                &b.kind,
                &b.request_id,
                b.n,
                &b.reason,
                b.blocking,
                &b.options
            ),
            (
                &p.kind,
                &p.request_id,
                p.n,
                &p.reason,
                p.blocking,
                &p.options
            )
        );
        assert_eq!(
            (b.can_continue, b.wait_s, &b.expires_at),
            (p.can_continue, p.wait_s, &p.expires_at)
        );
        assert_eq!(
            b.models.iter().map(|m| &m.id).collect::<Vec<_>>(),
            p.models.iter().take(8).map(|m| &m.id).collect::<Vec<_>>()
        );
        // and it is a valid inline event
        let ev = Envelope {
            schema_version: 1,
            event_id: REQ.into(),
            run_id: RUN.into(),
            task_id: None,
            agent_id: Some("direct".into()),
            seq: 1,
            ts: "2026-10-15T13:41:07Z".into(),
            event_type: EventType::TurnProgress,
            authority: Authority::Agent,
            payload: Payload::Inline {
                value: b.to_value(),
            },
        };
        harness_protocol::envelope::validate(&ev).unwrap();
    }

    // ---- fold_input ----

    fn eid(n: u64) -> String {
        format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
    }

    fn at(n: u64) -> String {
        format!("2026-10-15T13:{:02}:{:02}Z", n / 60, n % 60)
    }

    fn progress(n: u64, agent: &str, value: Value) -> Envelope {
        Envelope {
            schema_version: 1,
            event_id: eid(n),
            run_id: RUN.into(),
            task_id: None,
            agent_id: Some(agent.into()),
            seq: n,
            ts: at(n),
            event_type: EventType::TurnProgress,
            authority: Authority::Agent,
            payload: Payload::Inline { value },
        }
    }

    fn harness_event(n: u64, event_type: EventType, value: Value) -> Envelope {
        Envelope {
            schema_version: 1,
            event_id: eid(n),
            run_id: RUN.into(),
            task_id: None,
            agent_id: None,
            seq: n,
            ts: at(n),
            event_type,
            authority: Authority::Harness,
            payload: Payload::Inline { value },
        }
    }

    /// A `needs-input` event whose request id is its own event id, as agentd writes it.
    fn needs(n: u64, reason: &str, blocking: bool) -> Envelope {
        let mut p = golden_payload();
        p.request_id = eid(n);
        p.n = 1;
        p.reason = reason.into();
        p.blocking = blocking;
        progress(n, "direct", p.to_value())
    }

    fn received(n: u64, request: u64, action: &str) -> Envelope {
        progress(
            n,
            "direct",
            json!({"turn": 1, "kind": "input-received", "text": "ok", "requestId": eid(request),
                   "action": action, "origin": "user", "textBytes": 3, "waitedS": 5}),
        )
    }

    fn closed(n: u64, request: u64, how: &str) -> Envelope {
        progress(
            n,
            "direct",
            json!({"turn": 1, "kind": "input-closed", "text": "closed", "requestId": eid(request),
                   "how": how, "waitedS": 60}),
        )
    }

    fn session(n: u64, number: u32, trigger: &str) -> Envelope {
        progress(
            n,
            "direct",
            json!({"turn": 1, "kind": "new-session", "text": "restarted", "requestId": null,
                   "session": number, "trigger": trigger, "reason": "engine_stop",
                   "restartedProcess": false, "digestBytes": 10, "promptBytes": 20}),
        )
    }

    fn fold(events: &[Envelope]) -> InputFold {
        fold_input(events.iter())
    }

    fn how(f: &InputFold) -> Vec<(&str, &str)> {
        f.closed
            .iter()
            .map(|c| (c.request_id.as_str(), c.how.as_str()))
            .collect()
    }

    #[test]
    fn an_empty_log_has_no_question_and_one_session() {
        let f = fold(&[]);
        assert_eq!(f, InputFold::default());
        assert!(f.open.is_none() && f.closed.is_empty());
        assert_eq!((f.sessions, f.last_session), (1, None));
    }

    #[test]
    fn a_needs_input_event_opens_a_request() {
        let e = needs(10, "engine_stop", true);
        let f = fold(std::slice::from_ref(&e));
        let open = f.open.as_ref().expect("open");
        assert_eq!(open.request_id, eid(10));
        assert_eq!(open.opened_ts, at(10));
        assert_eq!(open.payload.reason, "engine_stop");
        assert!(f.closed.is_empty());
    }

    #[test]
    fn an_answer_closes_the_request_with_the_action() {
        for action in [
            "continue_with_prompt",
            "new_session_with_prompt",
            "try_model",
        ] {
            let f = fold(&[needs(10, "no_changes", true), received(11, 10, action)]);
            assert!(f.open.is_none(), "{action}");
            assert_eq!(f.closed.len(), 1);
            assert_eq!(f.closed[0].how, format!("answered:{action}"));
            assert_eq!(
                (f.closed[0].n, f.closed[0].reason.as_str()),
                (1, "no_changes")
            );
            assert_eq!(f.closed[0].ts, at(11));
        }
    }

    #[test]
    fn finish_stays_open_until_the_closing_event_arrives() {
        let f = fold(&[needs(10, "no_changes", true), received(11, 10, "finish")]);
        assert_eq!(f.open.as_ref().map(|o| o.request_id.clone()), Some(eid(10)));
        let f = fold(&[
            needs(10, "no_changes", true),
            received(11, 10, "finish"),
            closed(12, 10, "finish"),
        ]);
        assert!(f.open.is_none());
        assert_eq!(how(&f), vec![(eid(10).as_str(), "finish")]);
    }

    #[test]
    fn expired_cancelled_deadline_and_recovered_close_with_their_word() {
        for word in [
            "expired",
            "cancelled",
            "deadline",
            "superseded",
            "recovered",
        ] {
            let f = fold(&[needs(10, "engine_stop", true), closed(11, 10, word)]);
            assert!(f.open.is_none(), "{word}");
            assert_eq!(how(&f), vec![(eid(10).as_str(), word)]);
            assert_eq!(f.closed[0].ts, at(11));
        }
    }

    #[test]
    fn a_new_question_supersedes_the_open_one() {
        let f = fold(&[
            needs(10, "engine_stop", true),
            needs(20, "no_changes", true),
        ]);
        assert_eq!(f.open.as_ref().unwrap().request_id, eid(20));
        assert_eq!(how(&f), vec![(eid(10).as_str(), "superseded")]);
        assert_eq!(f.closed[0].ts, at(20));
    }

    #[test]
    fn an_outage_notice_opens_unblocking_and_a_recovery_closes_it() {
        let f = fold(&[needs(10, "provider_outage", false)]);
        let open = f.open.as_ref().unwrap();
        assert!(!open.payload.blocking);
        let f = fold(&[
            needs(10, "provider_outage", false),
            closed(11, 10, "recovered"),
        ]);
        assert!(f.open.is_none());
        assert_eq!(how(&f), vec![(eid(10).as_str(), "recovered")]);
    }

    #[test]
    fn the_end_of_the_run_closes_an_open_request_as_ended() {
        for ender in [
            harness_event(30, EventType::RunCompleted, json!({"status": "complete"})),
            harness_event(30, EventType::RunAborted, json!({"cause": "x"})),
            harness_event(30, EventType::GoalStatus, json!({"status": "complete"})),
            harness_event(30, EventType::GoalStatus, json!({"status": "cleared"})),
        ] {
            let f = fold(&[needs(10, "engine_stop", true), ender.clone()]);
            assert!(f.open.is_none(), "{:?}", ender.event_type);
            assert_eq!(how(&f), vec![(eid(10).as_str(), "ended")]);
            assert_eq!(f.closed[0].ts, at(30));
        }
    }

    #[test]
    fn a_goal_status_that_is_not_terminal_or_belongs_to_a_task_does_not_close() {
        let mut task_level =
            harness_event(30, EventType::GoalStatus, json!({"status": "complete"}));
        task_level.task_id = Some("t1".into());
        for e in [
            harness_event(30, EventType::GoalStatus, json!({"status": "active"})),
            harness_event(31, EventType::GoalStatus, json!({"status": "paused"})),
            task_level,
        ] {
            let f = fold(&[needs(10, "engine_stop", true), e.clone()]);
            assert!(f.open.is_some(), "{:?}", e.payload);
        }
    }

    #[test]
    fn input_ignored_changes_nothing() {
        let ignored = progress(
            11,
            "direct",
            json!({"turn": 1, "kind": "input-ignored", "text": "no", "requestId": eid(10), "why": "window-closed"}),
        );
        let f = fold(&[needs(10, "engine_stop", true), ignored]);
        assert_eq!(f.open.as_ref().unwrap().request_id, eid(10));
        assert!(f.closed.is_empty());
    }

    #[test]
    fn a_repeated_event_counts_once() {
        let n = needs(10, "engine_stop", true);
        let f = fold(&[
            n.clone(),
            n.clone(),
            received(11, 10, "continue_with_prompt"),
        ]);
        assert_eq!(f.closed.len(), 1);
        assert!(f.open.is_none());
        // the same request id under a second event id is a duplicate too
        let mut again = n.clone();
        again.event_id = eid(12);
        let f = fold(&[n, received(11, 10, "continue_with_prompt"), again]);
        assert!(f.open.is_none(), "an answered request does not reopen");
        assert_eq!(f.closed.len(), 1);
    }

    #[test]
    fn events_in_any_order_fold_as_if_in_ulid_order() {
        let ordered = vec![
            needs(10, "engine_stop", true),
            received(11, 10, "new_session_with_prompt"),
            session(12, 2, "user"),
            needs(20, "no_changes", true),
        ];
        let mut shuffled = ordered.clone();
        shuffled.reverse();
        assert_eq!(fold(&shuffled), fold(&ordered));
        assert_eq!(fold(&ordered).sessions, 2);
        assert_eq!(fold(&ordered).open.as_ref().unwrap().request_id, eid(20));
    }

    /// agentd records a finish's `input-received` and then its `input-closed how=finish` in the same millisecond with the same `ts`.
    /// The ids of two events of one millisecond are in random order,
    /// so the fold must be the same in both orders.
    #[test]
    fn a_finish_folds_the_same_whichever_of_its_receipt_and_close_sorts_first() {
        let same_second = |mut e: Envelope| {
            e.ts = at(11);
            e
        };
        let natural = fold(&[
            needs(10, "no_changes", true),
            same_second(received(11, 10, "finish")),
            same_second(closed(12, 10, "finish")),
        ]);
        let inverted = fold(&[
            needs(10, "no_changes", true),
            same_second(closed(11, 10, "finish")),
            same_second(received(12, 10, "finish")),
        ]);
        assert_eq!(inverted, natural);
        let q = eid(10);
        assert!(natural.open.is_none());
        assert_eq!(how(&natural), vec![(q.as_str(), "finish")]);
    }

    /// Every ordering of `items`.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut all = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.to_vec();
            let first = rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, first.clone());
                all.push(tail);
            }
        }
        all
    }

    /// Whichever events of one question share a millisecond (all with one `ts`), the ids put them in any order. The card state is the
    /// same for every order: a finish (receipt and close), an answer with a new session, and the close of an expired question.
    #[test]
    fn the_events_of_one_millisecond_fold_to_one_state_in_every_order() {
        type Maker = fn(u64) -> Envelope;
        let one_second = |mut e: Envelope| {
            e.ts = at(30);
            e
        };
        let cases: Vec<(&str, Vec<Maker>)> = vec![
            (
                "finish",
                vec![|n| received(n, 10, "finish"), |n| closed(n, 10, "finish")],
            ),
            (
                "new session",
                vec![|n| received(n, 10, "new_session_with_prompt"), |n| {
                    session(n, 2, "user")
                }],
            ),
            (
                "answer and next question",
                vec![
                    |n| received(n, 10, "continue_with_prompt"),
                    |n| session(n, 2, "user"),
                    |n| closed(n, 99, "expired"),
                ],
            ),
        ];
        for (name, makers) in cases {
            let order: Vec<usize> = (0..makers.len()).collect();
            let fold_of = |perm: &[usize]| {
                let mut events = vec![needs(10, "no_changes", true)];
                for (slot, which) in perm.iter().enumerate() {
                    events.push(one_second(makers[*which](11 + slot as u64)));
                }
                fold(&events)
            };
            let first = fold_of(&order);
            for perm in permutations(&order) {
                assert_eq!(fold_of(&perm), first, "{name}: order {perm:?}");
            }
        }
    }

    #[test]
    fn corrupt_events_and_other_agents_are_skipped() {
        let bad_needs = progress(10, "direct", json!({"kind": "needs-input"}));
        let bad_received = progress(
            12,
            "direct",
            json!({"kind": "input-received", "requestId": eid(11), "action": "dance"}),
        );
        let f = fold(&[bad_needs, needs(11, "engine_stop", true), bad_received]);
        assert_eq!(
            f.open.as_ref().unwrap().request_id,
            eid(11),
            "the corrupt answer did not close it"
        );
        let other = progress(13, "other", needs(13, "engine_stop", true).payload_value());
        let f = fold(&[other]);
        assert!(f.open.is_none(), "another agent's event is not read");
        let unrelated = progress(
            14,
            "direct",
            json!({"turn": 1, "kind": "provider-retry", "text": "x"}),
        );
        assert_eq!(fold(&[unrelated]), InputFold::default());
    }

    #[test]
    fn an_answer_for_another_request_changes_nothing() {
        let f = fold(&[
            needs(10, "engine_stop", true),
            received(11, 99, "continue_with_prompt"),
        ]);
        assert_eq!(f.open.as_ref().unwrap().request_id, eid(10));
        let f = fold(&[needs(10, "engine_stop", true), closed(11, 99, "expired")]);
        assert_eq!(f.open.as_ref().unwrap().request_id, eid(10));
    }

    #[test]
    fn new_session_events_count_the_sessions() {
        let f = fold(&[session(10, 2, "harness"), session(11, 3, "user")]);
        assert_eq!(f.sessions, 3);
        assert_eq!(
            f.last_session
                .as_ref()
                .map(|s| (s.session, s.trigger.as_str())),
            Some((3, "user"))
        );
        // a tail read that missed the first event still reports the session the last one names
        let f = fold(&[session(11, 4, "user")]);
        assert_eq!(f.sessions, 4);
    }

    #[test]
    fn only_the_last_eight_closed_requests_are_kept_oldest_first() {
        let mut events = Vec::new();
        for i in 0..10u64 {
            events.push(needs(100 + i * 2, "engine_stop", true));
            events.push(closed(101 + i * 2, 100 + i * 2, "expired"));
        }
        let f = fold(&events);
        assert_eq!(f.closed.len(), MAX_FOLD_CLOSED);
        assert_eq!(f.closed[0].request_id, eid(104));
        assert_eq!(f.closed[7].request_id, eid(118));
    }

    #[test]
    fn the_golden_events_fold() {
        let events = [
            envelope("needs-input-engine-stop.json"),
            envelope("input-received.json"),
            envelope("new-session-user.json"),
        ];
        let f = fold_input(events.iter());
        assert!(f.open.is_none());
        assert_eq!(how(&f), vec![(REQ, "answered:continue_with_prompt")]);
        assert_eq!(f.sessions, 2);
        let _ = fixture("a15-error-text.txt");
    }

    // ---- coverage of the small readers: the wire words, the serde defaults and the fold's skip paths ----

    #[test]
    fn the_event_words_have_one_spelling() {
        use harness_protocol::direct::{
            InputClosedHow, InputIgnoreWhy, NewSessionTrigger, PROGRESS_INPUT_CLOSED,
            PROGRESS_INPUT_IGNORED, PROGRESS_INPUT_RECEIVED, PROGRESS_NEEDS_INPUT,
            PROGRESS_NEW_SESSION,
        };
        assert_eq!(
            InputIgnoreWhy::ALL.map(InputIgnoreWhy::as_str),
            [
                "window-closed",
                "unknown-request",
                "already-answered",
                "session-dead",
                "bad-action",
                "bad-model",
                "limit"
            ]
        );
        assert_eq!(
            InputClosedHow::ALL.map(InputClosedHow::as_str),
            [
                "expired",
                "cancelled",
                "deadline",
                "finish",
                "superseded",
                "recovered"
            ]
        );
        assert_eq!(
            [
                NewSessionTrigger::Harness,
                NewSessionTrigger::User,
                NewSessionTrigger::Analyzer
            ]
            .map(NewSessionTrigger::as_str),
            ["harness", "user", "analyzer"]
        );
        assert_eq!(
            (
                InstructOrigin::User.as_str(),
                InstructOrigin::Analyzer.as_str()
            ),
            ("user", "analyzer")
        );
        assert_eq!(InstructOrigin::user(), InstructOrigin::User);
        assert_eq!(
            [
                PROGRESS_NEEDS_INPUT,
                PROGRESS_INPUT_RECEIVED,
                PROGRESS_INPUT_IGNORED,
                PROGRESS_INPUT_CLOSED,
                PROGRESS_NEW_SESSION
            ],
            [
                "needs-input",
                "input-received",
                "input-ignored",
                "input-closed",
                "new-session"
            ]
        );
    }

    #[test]
    fn a_needs_input_payload_missing_its_optional_members_reads_with_the_defaults() {
        let p: NeedsInputPayload = serde_json::from_value(
            json!({"kind": "needs-input", "requestId": REQ, "reason": "future_reason", "title": "t"}),
        )
        .unwrap();
        assert!(p.blocking, "a question blocks unless it says it does not");
        assert_eq!((p.turn, p.n, p.wait_s, p.files_total), (1, 1, 0, 0));
        assert!(p.options.is_empty() && p.models.is_empty() && !p.can_continue);
        assert_eq!(p.recommended, None);
        assert_eq!(p.expires_at, None);
    }

    #[test]
    fn the_fold_skips_a_claim_check_and_a_corrupt_closing_or_session_event() {
        let claim = Envelope {
            payload: Payload::ClaimCheck {
                s3_uri: "s3://bucket/key".into(),
                sha256: "a".repeat(64),
                size_bytes: 10,
            },
            ..needs(10, "engine_stop", true)
        };
        assert_eq!(fold(&[claim]), InputFold::default());

        let bad_closed = progress(
            12,
            "direct",
            json!({"kind": "input-closed", "requestId": eid(11)}),
        );
        let bad_session = progress(
            13,
            "direct",
            json!({"kind": "new-session", "trigger": "user"}),
        );
        let f = fold(&[needs(11, "engine_stop", true), bad_closed, bad_session]);
        assert_eq!(f.open.as_ref().map(|o| o.request_id.clone()), Some(eid(11)));
        assert_eq!((f.sessions, f.last_session), (1, None));
    }

    trait PayloadValue {
        fn payload_value(&self) -> Value;
    }
    impl PayloadValue for Envelope {
        fn payload_value(&self) -> Value {
            match &self.payload {
                Payload::Inline { value } => value.clone(),
                Payload::ClaimCheck { .. } => Value::Null,
            }
        }
    }
}
