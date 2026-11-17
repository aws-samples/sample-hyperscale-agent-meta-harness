//! With: which copy a cancelled run delivers, in one wording. When the cancel is recorded the service has not chosen yet:
//! `ham cancel`, `ham watch` and the user guide say the rule in the same words (`ham_core::cancel_note::DELIVERS`). When the service has
//! decided, its word about the lost session names the copy, and the notice and the card's row say the same copy.
#![allow(clippy::unwrap_used)]

use ham_core::cancel_note::CancelNote;
use ham_core::lost_session::{self, SessionLost};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01M7B2R7K3C9XJ4D8E6FQ0WT1N";
const RULE: &str = "delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved \
                    copy, if the run saved one";

fn waiting(payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: "01M7B2R7K3C9XJ4D8E6FQ0WT09".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 9,
        ts: "2026-11-16T07:15:33Z".into(),
        event_type: EventType::RunWaiting,
        authority: Authority::Harness,
        payload: Payload::Inline { value: payload },
    }
}

fn note(payload: Value) -> CancelNote {
    CancelNote::parse(&waiting(payload)).expect("a cancel note")
}

fn repo_text(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// When the cancel is recorded: one rule, in the same words, in `ham cancel`, in `ham watch` and in both guides.
#[test]
fn b853_b854_the_lines_of_a_recorded_cancel_say_one_rule_about_the_copy() {
    let kind = ham_core::cancel_note::KIND;
    let live = note(json!({
        "kind": kind, "requestedAt": "2026-11-16T07:05:22Z", "session": 1, "sessionLive": true,
        "lastHeartbeatAt": "2026-11-16T07:05:00Z", "lostAfter": "2026-11-16T07:15:00Z"
    }));
    let none = note(
        json!({"kind": kind, "requestedAt": "2026-11-16T07:20:01Z", "session": 1, "sessionLive": false}),
    );
    for n in [&live, &none] {
        let answer = n.answer_lines(RUN).join(" ");
        assert!(answer.contains(&format!("{RULE}.")), "ham cancel: {answer}");
        assert!(
            n.watch_line().ends_with(&format!("{RULE}.")),
            "ham watch: {}",
            n.watch_line()
        );
        assert!(
            n.notice_line(RUN).contains(&format!("{RULE}.")),
            "the run screen: {}",
            n.notice_line(RUN)
        );
        // No line promises the saved copy alone.
        for said in [answer, n.watch_line()] {
            assert!(
                !said.contains("and delivers the last saved copy")
                    && !said.contains("The service delivers the last saved copy"),
                "{said}"
            );
        }
    }
    // A run that is already delivering has chosen: its line names no rule.
    let delivering = note(
        json!({"kind": kind, "requestedAt": "2026-11-16T07:20:01Z", "session": 1, "sessionLive": false, "delivering": true}),
    );
    assert!(!delivering.watch_line().contains("read back"));
    // The user guide says the rule in the same words. (The operating guide does too; the document test of the cancel holds it.)
    let guide = repo_text("docs/user-guide.md");
    assert!(
        guide.contains(RULE),
        "the user guide says the rule in these words"
    );
    assert!(
        !guide.contains("delivers the last saved copy, if the run saved one"),
        "the user guide no longer promises the saved copy alone"
    );
}

fn lost(delivers: &str, extra: Value) -> SessionLost {
    let mut v = json!({
        "kind": lost_session::KIND, "session": 1, "decidedAt": "2026-11-16T07:15:33Z", "lastHeartbeatAt": "2026-11-16T07:05:22Z",
        "cause": "no-heartbeat", "silenceS": 600, "lostInARow": 1, "limit": 3, "then": "end", "delivers": delivers,
        "cancelledAt": "2026-11-16T07:05:22Z"
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    SessionLost::parse(&waiting(v)).expect("the lost-session word")
}

/// When the service has decided: the notice (`ham watch`, the run screen) and the card's row name the same copy.
#[test]
fn b853_b854_the_notice_and_the_card_name_the_copy_the_service_chose_for_a_cancelled_run() {
    // The files read back from the lost session's storage.
    let read_back = lost(
        "copy",
        json!({"recovery": {"tried": true, "used": true, "kept": true}, "readBackSession": 1}),
    );
    assert!(
        read_back.sentence().ends_with(
            "You had stopped the run at 2026-11-16 07:05 UTC, so no new session starts: the run ends and the service delivers the files \
             it read back from session 1's storage."
        ),
        "{}",
        read_back.sentence()
    );
    assert_eq!(
        read_back.card_row(),
        "Lost      session 1: last heartbeat 2026-11-16 07:05 UTC, called lost 07:15 UTC; you had stopped the run, so no new session \
         started and the run ended with the files the service read back from session 1's storage"
    );
    // The last saved copy, with files that were read back and not used: the command that downloads them is named in both.
    let saved = lost(
        "copy",
        json!({"recovery": {"tried": true, "used": false, "reason": "storage-older", "kept": true}}),
    );
    assert!(
        saved
            .sentence()
            .contains("the run ends and the service delivers the last saved copy."),
        "{}",
        saved.sentence()
    );
    assert!(
        saved.sentence().ends_with(&format!(
            "Download the files read back with `ham pull {RUN} --read-back 1`."
        )),
        "{}",
        saved.sentence()
    );
    assert_eq!(
        saved.card_row(),
        format!(
            "Lost      session 1: last heartbeat 2026-11-16 07:05 UTC, called lost 07:15 UTC; you had stopped the run, so no new \
             session started and the run ended with the last saved copy; the storage was read back and not used; ham pull {RUN} \
             --read-back 1 downloads the files read back"
        )
    );
    // Nothing to deliver: neither names a copy.
    let nothing = lost("nothing", json!({}));
    assert!(
        nothing
            .sentence()
            .ends_with("The run saved no copy of its work, so there is nothing to deliver."),
        "{}",
        nothing.sentence()
    );
    assert!(
        nothing
            .card_row()
            .ends_with("so no new session started and the run ended"),
        "{}",
        nothing.card_row()
    );
    // The user guide quotes the row of the card.
    let guide = repo_text("docs/user-guide.md");
    for words in [
        "the run ended with the files the service read back from session 1's storage",
        "the run ended with the last saved copy",
    ] {
        assert!(guide.contains(words), "the user guide quotes: {words}");
    }
}
