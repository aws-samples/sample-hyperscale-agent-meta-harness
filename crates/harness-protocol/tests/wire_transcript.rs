//! Protocol 1.38.0: the wire members of the stored conversation and of the engine
//! capability event. agentd writes them (the transcript module), worker-api stores what they declare (the Agent item's `features` and
//! `harnessControls`, the Turn item's `transcript`) and the read route (control-api) reads the stored form. The shapes here are the ones
//! `ham_core::transcript` reads: a change to any of them needs the reader's owner.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::{
    Authority, Envelope, EventType, Payload, SCHEMA_VERSION, validate,
};
use harness_protocol::store::AgentItem;

/// The size an engine's capability record is cut to (`agentd`'s `CAPABILITY_PAYLOAD_BYTES`).
const CAPABILITY_LIMIT: usize = 4096;
use harness_protocol::transcript::{
    CapabilityAgent, CapabilityCommand, CapabilityConfigOption, CapabilityMode, CapabilityModes,
    CompletenessEntry, CompletenessKind, HarnessCapabilities, MAX_COMPLETENESS_ENTRIES,
    SessionCapabilitiesPayload, TRANSCRIPT_FEATURE, TranscriptSummary, clean_words, seq_of_name,
};
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn envelope(authority: Authority, value: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01M4ZHFBV5SDX7J92E6YRH4QTX".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 3,
        ts: "2026-10-16T14:00:00Z".into(),
        event_type: EventType::SessionCapabilities,
        authority,
        payload: Payload::Inline { value },
    }
}

fn entry(
    kind: CompletenessKind,
    from: Option<u64>,
    to: Option<u64>,
    n: Option<u64>,
) -> CompletenessEntry {
    CompletenessEntry {
        kind,
        from_seq: from,
        to_seq: to,
        count: n,
    }
}

#[test]
fn the_protocol_is_1_38_0() {
    assert_eq!(PROTOCOL_VERSION, "1.59.0");
}

#[test]
fn session_capabilities_is_a_new_agent_event_type_last_in_the_table() {
    assert_eq!(
        EventType::SessionCapabilities.as_str(),
        "session.capabilities"
    );
    assert_eq!(
        EventType::SessionCapabilities.required_authority(),
        Authority::Agent
    );
    assert_eq!(EventType::ALL.last(), Some(&EventType::SessionCapabilities));
    assert!(!EventType::SessionCapabilities.changes_goal_status());
    assert_eq!(
        serde_json::to_value(EventType::SessionCapabilities).unwrap(),
        "session.capabilities"
    );
    validate(&envelope(Authority::Agent, json!({"v": 1}))).unwrap();
    // the harness's own authority is not the one this event carries: the engine's process writes it
    assert!(validate(&envelope(Authority::Harness, json!({"v": 1}))).is_err());
}

#[test]
fn the_chunk_keys_are_the_ones_the_reader_lists() {
    assert_eq!(
        keys::transcript_prefix(RUN),
        format!("runs/{RUN}/agents/direct/turns/1/transcript/")
    );
    assert_eq!(
        keys::transcript_chunk_key(RUN, 7),
        format!("runs/{RUN}/agents/direct/turns/1/transcript/000000000007.ndjson.gz")
    );
    assert_eq!(
        keys::transcript_chunk_key(RUN, 123_456_789_012),
        format!("runs/{RUN}/agents/direct/turns/1/transcript/123456789012.ndjson.gz")
    );
}

#[test]
fn a_chunk_name_is_twelve_digits_and_the_suffix() {
    assert_eq!(seq_of_name("000000000000.ndjson.gz"), Some(0));
    assert_eq!(seq_of_name("000000000042.ndjson.gz"), Some(42));
    for bad in [
        "42.ndjson.gz",
        "0000000000042.ndjson.gz",
        "00000000004a.ndjson.gz",
        "000000000042.ndjson",
        "000000000042.json.gz",
        "-00000000042.ndjson.gz",
        "+00000000042.ndjson.gz",
        "000000000042.ndjson.gz.tmp",
        "",
    ] {
        assert_eq!(seq_of_name(bad), None, "{bad}");
    }
}

#[test]
fn the_summary_writes_the_members_the_route_reads() {
    let s = TranscriptSummary {
        last_transcript_seq: Some(7),
        entries: vec![
            entry(CompletenessKind::Excluded, None, None, Some(12)),
            entry(CompletenessKind::Conflict, Some(4), Some(4), Some(1)),
            entry(CompletenessKind::TailUnknown, Some(2), Some(2), None),
        ],
    };
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        json!({
            "lastTranscriptSeq": 7,
            "entries": [
                {"kind": "excluded", "count": 12},
                {"kind": "conflict", "fromSeq": 4, "toSeq": 4, "count": 1},
                {"kind": "tail-unknown", "fromSeq": 2, "toSeq": 2}
            ]
        })
    );
    let back: TranscriptSummary =
        serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
    assert_eq!(back, s);
    // an empty summary has no members to speak of
    assert_eq!(
        serde_json::to_value(TranscriptSummary::default()).unwrap(),
        json!({"entries": []})
    );
    for kind in [
        "excluded",
        "truncated",
        "shed",
        "suppressed",
        "failed",
        "conflict",
        "unsupported",
        "tail-unknown",
    ] {
        let e: CompletenessEntry = serde_json::from_value(json!({"kind": kind})).unwrap();
        assert_eq!(serde_json::to_value(&e).unwrap()["kind"], kind);
    }
    // the reader's own words are not a writer's to declare
    assert!(serde_json::from_value::<CompletenessEntry>(json!({"kind": "unreadable"})).is_err());
    assert!(serde_json::from_value::<CompletenessEntry>(json!({"kind": "missing"})).is_err());
}

#[test]
fn a_summary_is_valid_when_bounded_and_ordered() {
    assert!(TranscriptSummary::default().validate());
    let ok = TranscriptSummary {
        last_transcript_seq: Some(0),
        entries: vec![entry(CompletenessKind::Shed, Some(3), Some(5), Some(9))],
    };
    assert!(ok.validate());
    let backwards = TranscriptSummary {
        last_transcript_seq: None,
        entries: vec![entry(CompletenessKind::Shed, Some(5), Some(3), None)],
    };
    assert!(!backwards.validate(), "a range runs forward");
    let half = TranscriptSummary {
        last_transcript_seq: None,
        entries: vec![entry(CompletenessKind::Shed, Some(5), None, None)],
    };
    assert!(!half.validate(), "a range has both ends or neither");
    let many = TranscriptSummary {
        last_transcript_seq: None,
        entries: (0..=MAX_COMPLETENESS_ENTRIES)
            .map(|_| entry(CompletenessKind::Truncated, None, None, Some(1)))
            .collect(),
    };
    assert!(
        !many.validate(),
        "{MAX_COMPLETENESS_ENTRIES} entries at most"
    );
    let exactly = TranscriptSummary {
        last_transcript_seq: None,
        entries: (0..MAX_COMPLETENESS_ENTRIES)
            .map(|_| entry(CompletenessKind::Truncated, None, None, Some(1)))
            .collect(),
    };
    assert!(exactly.validate());
}

#[test]
fn turn_complete_carries_the_summary_only_when_there_is_one() {
    let mut req: TurnCompleteRequest = serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "turn": 1, "finishReason": "end_turn"
    }))
    .unwrap();
    assert!(
        req.transcript.is_none(),
        "a request written before 1.38.0 reads"
    );
    assert!(
        serde_json::to_value(&req)
            .unwrap()
            .get("transcript")
            .is_none(),
        "and writes back without the member"
    );
    req.transcript = Some(TranscriptSummary {
        last_transcript_seq: Some(3),
        entries: vec![entry(CompletenessKind::Truncated, None, None, Some(2))],
    });
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(
        v["transcript"],
        json!({"lastTranscriptSeq": 3, "entries": [{"kind": "truncated", "count": 2}]})
    );
    let back: TurnCompleteRequest = serde_json::from_value(v).unwrap();
    assert_eq!(back, req);
}

#[test]
fn harness_lists_keep_only_plain_words_once_each_and_a_bounded_number() {
    let words = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert_eq!(
        clean_words(&words(&["message.next", "message.now", "message.next"])),
        words(&["message.next", "message.now"])
    );
    assert_eq!(
        clean_words(&words(&[
            "Bad Word",
            "ok-1",
            "",
            "has space",
            "UPPER",
            "a.b_c-d"
        ])),
        words(&["ok-1", "a.b_c-d"])
    );
    let long = "a".repeat(65);
    let fits = "a".repeat(64);
    assert_eq!(clean_words(&[long.clone(), fits.clone()]), vec![fits]);
    let many: Vec<String> = (0..40).map(|i| format!("feature.{i}")).collect();
    let kept = clean_words(&many);
    assert_eq!(kept.len(), 16);
    assert_eq!(kept[0], "feature.0");
    assert_eq!(kept[15], "feature.15");
}

#[test]
fn the_product_declares_its_features_and_controls() {
    let h = HarnessCapabilities::this_build();
    assert_eq!(TRANSCRIPT_FEATURE, "transcript.v1");
    assert_eq!(h.features, vec!["transcript.v1".to_string()]);
    assert_eq!(
        h.controls,
        vec!["message.next".to_string(), "message.now".to_string()]
    );
    assert_eq!(
        serde_json::to_value(&h).unwrap(),
        json!({"controls": ["message.next", "message.now"], "features": ["transcript.v1"]})
    );
}

fn payload(commands: usize, options: usize) -> SessionCapabilitiesPayload {
    SessionCapabilitiesPayload {
        v: 1,
        engine: "Engine".into(),
        session: 1,
        session_id: Some("s-1".into()),
        acp_version: Some(1),
        agent: Some(CapabilityAgent {
            name: "agent".into(),
            version: Some("3.0.71".into()),
        }),
        load_session: Some(true),
        prompt_capabilities: Some(json!({"image": true, "audio": false, "embeddedContext": true})),
        session_capabilities: Some(json!({"close": {}, "fork": {}, "list": {}, "resume": {}})),
        modes: Some(CapabilityModes {
            current: Some("act".into()),
            available: vec![
                CapabilityMode {
                    id: "plan".into(),
                    name: "Plan".into(),
                },
                CapabilityMode {
                    id: "act".into(),
                    name: "Act".into(),
                },
            ],
        }),
        config_options: (0..options)
            .map(|i| CapabilityConfigOption {
                id: format!("option-{i}"),
                name: format!("Option number {i}"),
                kind: Some("select".into()),
                current: Some(json!(format!("value-{i}-{}", "x".repeat(40)))),
            })
            .collect(),
        commands: (0..commands)
            .map(|i| CapabilityCommand {
                name: format!("command-{i}"),
                description: Some(format!(
                    "What command {i} does, in a sentence of some length"
                )),
            })
            .collect(),
        harness: HarnessCapabilities::this_build(),
        truncated: false,
    }
}

fn size(p: &SessionCapabilitiesPayload) -> usize {
    serde_json::to_vec(p).unwrap().len()
}

#[test]
fn a_small_capability_record_is_left_as_it_is() {
    let p = payload(3, 4);
    assert!(size(&p) < CAPABILITY_LIMIT);
    let b = p.clone().bounded(CAPABILITY_LIMIT);
    assert_eq!(b, p);
    assert!(!b.truncated);
    assert!(
        serde_json::to_value(&b).unwrap().get("truncated").is_none(),
        "no truncated member when nothing was dropped"
    );
}

#[test]
fn an_oversized_capability_record_drops_commands_first_then_option_values_then_session_capabilities()
 {
    // commands alone make it too big: only they go
    let p = payload(80, 4);
    assert!(size(&p) > CAPABILITY_LIMIT);
    let b = p.clone().bounded(CAPABILITY_LIMIT);
    assert!(b.commands.is_empty());
    assert!(b.truncated);
    assert_eq!(
        b.config_options, p.config_options,
        "the options stay while dropping commands is enough"
    );
    assert_eq!(b.session_capabilities, p.session_capabilities);
    assert!(size(&b) <= CAPABILITY_LIMIT);
    assert_eq!(b.harness, p.harness, "the harness lists are never dropped");
    assert_eq!(b.modes, p.modes);

    // option values next
    let p = payload(80, 40);
    let b = p.clone().bounded(CAPABILITY_LIMIT);
    assert!(b.commands.is_empty());
    assert!(b.truncated);
    assert_eq!(b.config_options.len(), 40, "the options keep their names");
    assert!(b.config_options.iter().all(|o| o.current.is_none()));
    assert_eq!(b.session_capabilities, p.session_capabilities);
    assert!(size(&b) <= CAPABILITY_LIMIT);

    // and the session capabilities when that is still not enough
    let mut p = payload(80, 40);
    p.session_capabilities = Some(json!({"big": "y".repeat(5000)}));
    let b = p.clone().bounded(CAPABILITY_LIMIT);
    assert!(b.session_capabilities.is_none());
    assert!(b.truncated);
    assert!(size(&b) <= CAPABILITY_LIMIT);
    assert_eq!(b.harness, p.harness);
}

#[test]
fn the_limit_is_exact() {
    let p = payload(3, 4);
    let n = size(&p);
    assert_eq!(p.clone().bounded(n), p, "exactly the limit passes");
    let b = p.clone().bounded(n - 1);
    assert!(b.truncated, "one byte less drops something");
}

#[test]
fn a_record_still_too_big_after_the_three_drops_loses_its_option_and_mode_lists_but_never_its_harness_lists()
 {
    let mut p = payload(0, 200);
    p.modes = Some(CapabilityModes {
        current: Some("act".into()),
        available: (0..300)
            .map(|i| CapabilityMode {
                id: format!("mode-{i}"),
                name: format!("A mode with a long name {i}"),
            })
            .collect(),
    });
    p.agent = Some(CapabilityAgent {
        name: "a".repeat(5000),
        version: None,
    });
    let b = p.clone().bounded(CAPABILITY_LIMIT);
    assert!(size(&b) <= CAPABILITY_LIMIT, "{} bytes", size(&b));
    assert!(b.truncated);
    assert!(b.config_options.is_empty());
    assert_eq!(b.harness, p.harness);
    assert_eq!(b.engine, p.engine);
    assert_eq!(b.session, p.session);
    // nothing but a record that cannot be shrunk gets past the limit: the lists in front of the harness's own are what goes
    let tiny = SessionCapabilitiesPayload {
        v: 1,
        engine: "E".into(),
        session: 1,
        harness: HarnessCapabilities::this_build(),
        ..SessionCapabilitiesPayload::default()
    };
    assert_eq!(tiny.clone().bounded(CAPABILITY_LIMIT), tiny);
}

fn agent_json() -> Value {
    json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1, "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:111111111111:runtime/r",
        "runtimeSessionId": "s", "generation": 1, "engine": "cline", "engineVersion": "3.0.71", "transport": "acp", "modelId": "m",
        "pool": "hamh-dispatch", "status": "running", "version": 1
    })
}

#[test]
fn an_agent_item_lists_features_only_when_it_has_some() {
    let a: AgentItem = serde_json::from_value(agent_json()).unwrap();
    assert!(
        a.features.is_empty(),
        "an item written before 1.38.0 reads with none"
    );
    assert!(
        serde_json::to_value(&a).unwrap().get("features").is_none(),
        "and writes back without the member"
    );
    let mut with = agent_json();
    with["features"] = json!(["transcript.v1"]);
    let a: AgentItem = serde_json::from_value(with.clone()).unwrap();
    assert_eq!(a.features, vec!["transcript.v1".to_string()]);
    assert_eq!(
        serde_json::to_value(&a).unwrap()["features"],
        json!(["transcript.v1"])
    );
    // beside the controls of the message path, which are a different list
    let mut both = with;
    both["harnessControls"] = json!(["message.next", "message.now"]);
    let a: AgentItem = serde_json::from_value(both).unwrap();
    assert_eq!(a.harness_controls.len(), 2);
    assert_eq!(a.features.len(), 1);
}

#[test]
fn a_summary_is_lossless_when_it_declares_only_what_was_left_out_on_purpose() {
    assert!(
        TranscriptSummary::default().is_lossless(),
        "nothing declared"
    );
    let only_excluded = TranscriptSummary {
        last_transcript_seq: Some(4),
        entries: vec![entry(CompletenessKind::Excluded, None, None, Some(12))],
    };
    assert!(only_excluded.is_lossless());
    for kind in [
        CompletenessKind::Truncated,
        CompletenessKind::Shed,
        CompletenessKind::Suppressed,
        CompletenessKind::Failed,
        CompletenessKind::Conflict,
        CompletenessKind::Unsupported,
        CompletenessKind::TailUnknown,
    ] {
        let s = TranscriptSummary {
            last_transcript_seq: Some(4),
            entries: vec![
                entry(CompletenessKind::Excluded, None, None, Some(1)),
                entry(kind, None, None, Some(1)),
            ],
        };
        assert!(!s.is_lossless(), "{kind:?}");
    }
    let invalid = TranscriptSummary {
        last_transcript_seq: None,
        entries: vec![entry(CompletenessKind::Excluded, Some(5), Some(3), None)],
    };
    assert!(
        !invalid.is_lossless(),
        "an invalid summary is not a claim of no loss"
    );
}
