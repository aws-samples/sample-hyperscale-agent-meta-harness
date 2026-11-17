//! What the client shows of the worker's word about the command time limit of a run on the second engine. The worker tells the run
//! (`turn.progress`, kind `notify`, reason `command_limit`, its own sentence in `detail`) when the limit could not be placed, when the
//! engine did not confirm it, and when a command was stopped at another number. The client prints the sentence alone, whole.
#![allow(clippy::unwrap_used)]

use ham_core::notice::{NOTICE_MAX_CHARS, notice_of};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

fn event(payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: "01J9ZZZZZZ0000000000000001".into(),
        run_id: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 1,
        ts: "2026-11-16T07:00:00Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline { value: payload },
    }
}

fn notify(detail: Option<&str>) -> Envelope {
    let mut p =
        json!({"turn": 1, "kind": "notify", "reason": "command_limit", "outageMinutes": null});
    if let Some(d) = detail {
        p["detail"] = json!(d);
    }
    event(p)
}

/// The worker's three sentences (`agentd::engine::direct_words`), with the largest numbers the setting allows.
const SENTENCES: [&str; 3] = [
    "This run's command time limit of 1800 seconds could not be placed for OpenCode. Commands that set no time limit of their own are stopped after 120 seconds.",
    "OpenCode has not confirmed this run's command time limit of 1800 seconds. If it is not in force, commands that set no limit of their own stop after 120 seconds.",
    "OpenCode stopped a command after 120 seconds, not after this run's 1800: the command time limit was not applied to it.",
];

#[test]
fn b800_a_command_limit_notice_reads_as_its_sentence() {
    for sentence in SENTENCES {
        assert!(sentence.chars().count() <= NOTICE_MAX_CHARS, "{sentence}");
        let text = notice_of(&notify(Some(sentence))).unwrap().text;
        assert_eq!(text, sentence, "the sentence alone, whole");
    }
    // Without a detail the notice still says what it is about.
    assert_eq!(
        notice_of(&notify(None)).unwrap().text,
        "The run's command time limit is not confirmed for this session."
    );
    assert_eq!(
        notice_of(&notify(Some("  "))).unwrap().text,
        "The run's command time limit is not confirmed for this session."
    );
}
