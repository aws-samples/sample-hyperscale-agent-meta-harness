//! The outcome sentence names the engine that ran. Cline keeps its fifteen frozen sentences byte for byte; OpenCode reads the same sentences
//! with its own name; the summary validator accepts the sentence of an engine only for that engine.

#![allow(clippy::unwrap_used)]

use harness_protocol::common::Engine;
use harness_protocol::direct::{
    DirectOutcome, DirectSummary, DirectSummaryError, HARNESS_STOP_PREFIX, banned_claim_word,
};
use serde_json::{Value, json};

/// The frozen table of the owner (fifteen sentences). A copy by hand: the test fails if the code table moves a letter.
const CLINE: [(DirectOutcome, &str); 15] = [
    (DirectOutcome::ReportedDone, "Cline reported done"),
    (DirectOutcome::StoppedSetupFailed, "Cline did not start"),
    (
        DirectOutcome::StoppedNoMessage,
        "Cline stopped: it ended its turn without a message",
    ),
    (
        DirectOutcome::StoppedMaxTokens,
        "Cline stopped: it reached the output limit",
    ),
    (
        DirectOutcome::StoppedMaxRequests,
        "Cline stopped: it reached its request limit",
    ),
    (
        DirectOutcome::StoppedRefusal,
        "Cline stopped: the model refused",
    ),
    (
        DirectOutcome::StoppedByUser,
        "Cline stopped: you stopped the run",
    ),
    (
        DirectOutcome::StoppedEngineCancelled,
        "Cline stopped: Cline ended the run itself",
    ),
    (
        DirectOutcome::StoppedProviderError,
        "Cline stopped: the model provider failed",
    ),
    (
        DirectOutcome::StoppedEngineDied,
        "Cline stopped: the Cline process failed",
    ),
    (
        DirectOutcome::StoppedSilence,
        "Cline stopped: no output for the silence limit",
    ),
    (
        DirectOutcome::StoppedTimeout,
        "Cline stopped: the run reached its time limit",
    ),
    (
        DirectOutcome::StoppedOther,
        "Cline stopped: the engine reported an unknown stop reason",
    ),
    (
        DirectOutcome::StoppedSessionLost,
        "Cline stopped: the session was lost",
    ),
    (
        DirectOutcome::StoppedNoChanges,
        "Cline stopped: it ended its turn without changing a file",
    ),
];

const OPENCODE: [(DirectOutcome, &str); 15] = [
    (DirectOutcome::ReportedDone, "OpenCode reported done"),
    (DirectOutcome::StoppedSetupFailed, "OpenCode did not start"),
    (
        DirectOutcome::StoppedNoMessage,
        "OpenCode stopped: it ended its turn without a message",
    ),
    (
        DirectOutcome::StoppedMaxTokens,
        "OpenCode stopped: it reached the output limit",
    ),
    (
        DirectOutcome::StoppedMaxRequests,
        "OpenCode stopped: it reached its request limit",
    ),
    (
        DirectOutcome::StoppedRefusal,
        "OpenCode stopped: the model refused",
    ),
    (
        DirectOutcome::StoppedByUser,
        "OpenCode stopped: you stopped the run",
    ),
    (
        DirectOutcome::StoppedEngineCancelled,
        "OpenCode stopped: OpenCode ended the run itself",
    ),
    (
        DirectOutcome::StoppedProviderError,
        "OpenCode stopped: the model provider failed",
    ),
    (
        DirectOutcome::StoppedEngineDied,
        "OpenCode stopped: the OpenCode process failed",
    ),
    (
        DirectOutcome::StoppedSilence,
        "OpenCode stopped: no output for the silence limit",
    ),
    (
        DirectOutcome::StoppedTimeout,
        "OpenCode stopped: the run reached its time limit",
    ),
    (
        DirectOutcome::StoppedOther,
        "OpenCode stopped: the engine reported an unknown stop reason",
    ),
    (
        DirectOutcome::StoppedSessionLost,
        "OpenCode stopped: the session was lost",
    ),
    (
        DirectOutcome::StoppedNoChanges,
        "OpenCode stopped: it ended its turn without changing a file",
    ),
];

#[test]
fn b788_the_fifteen_cline_sentences_are_byte_for_byte_the_frozen_table() {
    assert_eq!(CLINE.len(), DirectOutcome::ALL.len());
    for (outcome, sentence) in CLINE {
        assert_eq!(outcome.label(), sentence, "{outcome:?}");
        assert_eq!(outcome.label_for(None), sentence, "{outcome:?}");
        assert_eq!(
            outcome.label_for_engine(Engine::CLINE, None),
            sentence,
            "{outcome:?}"
        );
    }
    let all: Vec<DirectOutcome> = CLINE.iter().map(|(o, _)| *o).collect();
    assert_eq!(
        all,
        DirectOutcome::ALL.to_vec(),
        "the table is in declaration order"
    );
}

#[test]
fn b788_the_word_cline_is_in_the_frozen_table_only_as_the_name_of_the_engine() {
    // The sentence of another engine is the Cline sentence with the name replaced, so the replacement must touch only the name: every sentence starts with
    // it, two sentences say it a second time, and no other sentence holds the word.
    let mut total = 0;
    for outcome in DirectOutcome::ALL {
        let label = outcome.label();
        assert!(label.starts_with("Cline "), "{outcome:?}: {label}");
        let n = label.matches("Cline").count();
        let expected = match outcome {
            DirectOutcome::StoppedEngineCancelled | DirectOutcome::StoppedEngineDied => 2,
            _ => 1,
        };
        assert_eq!(n, expected, "{outcome:?}: {label}");
        total += n;
    }
    assert_eq!(total, 17);
    // The reason of a stop by the harness is not part of the replaced text: it follows the name unchanged.
    let d = format!("{HARNESS_STOP_PREFIX} Cline is slow. More.");
    assert_eq!(
        DirectOutcome::StoppedOther.label_for_engine(Engine::OPENCODE, Some(&d)),
        "OpenCode stopped: the harness ended the run (Cline is slow)"
    );
}

#[test]
fn b788_the_fifteen_opencode_sentences_are_pinned() {
    for (outcome, sentence) in OPENCODE {
        assert_eq!(
            outcome.label_for_engine(Engine::OPENCODE, None),
            sentence,
            "{outcome:?}"
        );
    }
}

#[test]
fn b788_every_opencode_sentence_names_opencode_and_none_says_cline() {
    for outcome in DirectOutcome::ALL {
        for detail in [None, Some("silence 900 s"), Some("")] {
            let label = outcome.label_for_engine(Engine::OPENCODE, detail);
            assert!(label.starts_with("OpenCode "), "{outcome:?}: {label}");
            assert!(!label.contains("Cline"), "{outcome:?}: {label}");
        }
    }
}

#[test]
fn b788_the_label_of_an_engine_with_no_table_entry_uses_the_name_of_the_engine() {
    let pi = Engine::new("pi").unwrap();
    assert_eq!(
        DirectOutcome::ReportedDone.label_for_engine(pi, None),
        "pi reported done"
    );
    assert_eq!(
        DirectOutcome::StoppedEngineDied.label_for_engine(pi, None),
        "pi stopped: the pi process failed"
    );
}

#[test]
fn b788_a_stop_by_the_harness_reads_the_same_for_each_engine_with_its_name() {
    let d = format!(
        "{HARNESS_STOP_PREFIX} the run made no progress for 3 rounds. The engine said more."
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for_engine(Engine::CLINE, Some(&d)),
        "Cline stopped: the harness ended the run (the run made no progress for 3 rounds)"
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for(Some(&d)),
        DirectOutcome::StoppedOther.label_for_engine(Engine::CLINE, Some(&d))
    );
    assert_eq!(
        DirectOutcome::StoppedOther.label_for_engine(Engine::OPENCODE, Some(&d)),
        "OpenCode stopped: the harness ended the run (the run made no progress for 3 rounds)"
    );
    let bare = HARNESS_STOP_PREFIX;
    assert_eq!(
        DirectOutcome::StoppedOther.label_for_engine(Engine::OPENCODE, Some(bare)),
        "OpenCode stopped: the harness ended the run"
    );
    // The harness form belongs to `stopped_other` only.
    assert_eq!(
        DirectOutcome::StoppedSilence.label_for_engine(Engine::OPENCODE, Some(&d)),
        "OpenCode stopped: no output for the silence limit"
    );
}

#[test]
fn b788_no_label_of_any_engine_says_verified_tested_gated_or_evaluated() {
    for engine in [Engine::CLINE, Engine::OPENCODE, Engine::new("pi").unwrap()] {
        for outcome in DirectOutcome::ALL {
            let label = outcome.label_for_engine(engine, None);
            assert_eq!(
                banned_claim_word(&label),
                None,
                "{engine} {outcome:?}: {label}"
            );
        }
    }
}

fn summary(engine: &str, outcome: DirectOutcome, label: &str) -> DirectSummary {
    let mut s: DirectSummary = serde_json::from_value(summary_json(engine)).unwrap();
    s.outcome = outcome;
    s.detail = None;
    s.label = label.to_string();
    s
}

/// The smallest summary the reader accepts (the shape of a run whose setup failed); the outcome and the label are set by the caller.
fn summary_json(engine: &str) -> Value {
    json!({
        "schemaVersion": 1, "runId": "01M3QA3YCXGFW428XNGR7R8VWD", "agentId": "direct", "turn": 1,
        "outcome": "stopped_setup_failed", "label": "Cline did not start",
        "detail": "request.json sha256 differs", "stopReason": null, "turnOutcome": "failed",
        "clineMessage": null, "error": "request.json sha256 differs",
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1"},
        "engine": {"name": engine},
        "rules": [], "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 0},
        "nudge": {"requested": false, "honored": false},
        "usage": null, "toolCalls": 0,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T18:00:01Z", "wallS": 1},
        "git": {"committedByHarness": false, "filesChanged": 0},
        "snapshots": {"count": 0},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.58.0"}
    })
}

fn template_ok() -> bool {
    serde_json::from_value::<DirectSummary>(summary_json("cline")).is_ok()
}

#[test]
fn b788_the_validator_takes_an_engines_label_only_for_that_engine() {
    assert!(
        template_ok(),
        "the summary template parses; adjust it to the current DirectSummary"
    );
    let cline = summary("cline", DirectOutcome::ReportedDone, "Cline reported done");
    assert_eq!(cline.validate(), Ok(()));
    let open = summary(
        "opencode",
        DirectOutcome::ReportedDone,
        "OpenCode reported done",
    );
    assert_eq!(open.validate(), Ok(()));
    // An OpenCode label on a Cline run, and the reverse.
    let wrong = summary(
        "cline",
        DirectOutcome::ReportedDone,
        "OpenCode reported done",
    );
    assert_eq!(wrong.validate(), Err(DirectSummaryError::LabelMismatch));
    let wrong = summary(
        "opencode",
        DirectOutcome::ReportedDone,
        "Cline reported done",
    );
    assert_eq!(wrong.validate(), Err(DirectSummaryError::LabelMismatch));
    // Every outcome, both engines.
    for (cl, oc) in CLINE.iter().zip(OPENCODE.iter()) {
        assert_eq!(
            summary("cline", cl.0, cl.1).validate(),
            Ok(()),
            "{:?}",
            cl.0
        );
        assert_eq!(
            summary("opencode", oc.0, oc.1).validate(),
            Ok(()),
            "{:?}",
            oc.0
        );
        assert_eq!(
            summary("cline", cl.0, oc.1).validate(),
            Err(DirectSummaryError::LabelMismatch),
            "{:?}",
            cl.0
        );
        assert_eq!(
            summary("opencode", oc.0, cl.1).validate(),
            Err(DirectSummaryError::LabelMismatch),
            "{:?}",
            oc.0
        );
    }
}

#[test]
fn b788_the_validator_checks_the_harness_form_for_the_engine_of_the_summary() {
    let d = format!("{HARNESS_STOP_PREFIX} the disk was full.");
    let mut s = summary(
        "opencode",
        DirectOutcome::StoppedOther,
        "OpenCode stopped: the harness ended the run (the disk was full)",
    );
    s.detail = Some(d.clone());
    assert_eq!(s.validate(), Ok(()));
    s.label = "Cline stopped: the harness ended the run (the disk was full)".into();
    assert_eq!(s.validate(), Err(DirectSummaryError::LabelMismatch));
}

#[test]
fn b788_a_summary_whose_engine_name_is_no_engine_name_has_no_valid_label() {
    // `Engine` is the shape of a profile name; "Cline" with a capital is none.
    let mut s = summary("cline", DirectOutcome::ReportedDone, "Cline reported done");
    s.engine.name = "Cline".into();
    assert_eq!(s.validate(), Err(DirectSummaryError::LabelMismatch));
}
