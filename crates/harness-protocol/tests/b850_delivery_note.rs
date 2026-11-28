//! A run can switch off the harness's block on how the run delivers, an earlier worker image is sent nothing it
//! refuses, and the run's records say what the engine was told.
//!
//! Three things changed on the wire:
//!
//! * the start request takes `deliveryNote: false` (the user's `run.delivery_note`). Only `false` is ever sent or stored, so a request that
//!   keeps the default is byte for byte the request of protocol 1.61.0;
//! * the worker reads how the run delivers from the target of the stored request ([`DirectDelivery::of_target`], [`delivery_to_say`]), so
//!   the service writes no `delivery` member into the invocation and a worker image that predates the member is sent nothing it refuses;
//! * `summary.json` and the evidence carry the block the worker wrote before the first prompt of the session (`deliveryNote`).
//!
//! The sentence about a workflow file in a zip ([`zip_holds`]) is the one the run card shows; it lives here so that the card and EVIDENCE.md
//! say the same words.

#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    DirectDelivery, DirectEvidence, DirectSummary, HeadSource, MAX_DELIVERY_NOTE_CHARS,
    delivery_note_ok, delivery_to_say, zip_holds,
};
use harness_protocol::target::RunTarget;
use serde_json::{Value, json};

const REPO: &str = "cc:hamh-e2e-python-calc";

fn repo_target() -> RunTarget {
    RunTarget::Repo {
        repo: REPO.parse().unwrap(),
    }
}

fn start(extra: Value) -> Result<StartDirectRunRequest, serde_json::Error> {
    let mut v = json!({"prompt": "Add a workflow file.", "target": {"mode": "s3-only"}, "model": "us.anthropic.claude-sonnet-5-5"});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v)
}

#[test]
fn b850_the_protocol_is_1_65_0() {
    assert_eq!(PROTOCOL_VERSION, "1.65.0");
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    assert_eq!(docs["openapi-v1.json"]["info"]["version"], "1.65.0");
    assert_eq!(docs["openapi-internal.json"]["info"]["version"], "1.65.0");
}

#[test]
fn b850_a_start_request_takes_the_switch_and_a_default_request_has_no_member() {
    let off = start(json!({"deliveryNote": false})).unwrap();
    assert_eq!(off.delivery_note, Some(false));
    assert_eq!(serde_json::to_value(&off).unwrap()["deliveryNote"], false);
    // The default: no member, in and out, so the stored request of a default run is the one of protocol 1.61.0.
    let default = start(json!({})).unwrap();
    assert_eq!(default.delivery_note, None);
    assert!(
        serde_json::to_value(&default)
            .unwrap()
            .get("deliveryNote")
            .is_none()
    );
    // The member is a boolean and nothing else.
    for bad in [json!("off"), json!(0), json!(null_object())] {
        assert!(
            start(json!({"deliveryNote": bad})).is_err(),
            "a value that is no boolean is refused"
        );
    }
}

fn null_object() -> Value {
    json!({"enabled": false})
}

#[test]
fn b850_the_contract_file_names_the_key() {
    let docs: std::collections::BTreeMap<&str, Value> = harness_protocol::export::all_schemas()
        .into_iter()
        .collect();
    let text = docs["openapi-v1.json"].to_string();
    assert!(
        text.contains("run.delivery_note"),
        "the description of deliveryNote names the key"
    );
}

#[test]
fn b850_how_a_run_delivers_follows_from_its_target() {
    assert_eq!(
        DirectDelivery::of_target(&RunTarget::S3Only),
        DirectDelivery::zip()
    );
    assert_eq!(
        DirectDelivery::of_target(&repo_target()),
        DirectDelivery::pull_request(REPO)
    );
    assert_eq!(
        DirectDelivery::of_target(&RunTarget::NewRepo {
            repo: REPO.parse().unwrap()
        }),
        DirectDelivery::pull_request(REPO)
    );
}

#[test]
fn b850_the_worker_says_the_delivery_of_the_stored_target_unless_the_run_switched_it_off() {
    // No member in the invocation (a service of this version writes none): the stored target decides.
    assert_eq!(
        delivery_to_say(None, &RunTarget::S3Only, None),
        Some(DirectDelivery::zip())
    );
    assert_eq!(
        delivery_to_say(None, &repo_target(), Some(true)),
        Some(DirectDelivery::pull_request(REPO))
    );
    // The switch: nothing is said, whatever the invocation holds.
    assert_eq!(delivery_to_say(None, &RunTarget::S3Only, Some(false)), None);
    assert_eq!(
        delivery_to_say(
            Some(&DirectDelivery::zip()),
            &RunTarget::S3Only,
            Some(false)
        ),
        None
    );
}

#[test]
fn b850_a_member_of_the_invocation_must_agree_with_the_stored_target() {
    // A service of protocol 1.61.0 writes the member; it comes from the same target, so it agrees.
    assert_eq!(
        delivery_to_say(Some(&DirectDelivery::zip()), &RunTarget::S3Only, None),
        Some(DirectDelivery::zip())
    );
    // Two sources that differ: the worker cannot stand behind either, and says nothing.
    assert_eq!(
        delivery_to_say(Some(&DirectDelivery::zip()), &repo_target(), None),
        None
    );
    assert_eq!(
        delivery_to_say(
            Some(&DirectDelivery::pull_request("cc:another")),
            &repo_target(),
            None
        ),
        None
    );
}

fn summary_json(note: Option<&str>) -> Value {
    let mut v = json!({
        "schemaVersion": 1,
        "runId": "01M7H7GG5X7F8A8FKNTAECA34R", "agentId": "direct", "turn": 1,
        "outcome": "reported_done",
        "label": "Cline reported done",
        "detail": null,
        "stopReason": "end_turn",
        "turnOutcome": "completed",
        "clineMessage": "Added the workflow file .github/workflows/ci.yml.",
        "error": null,
        "model": {"id": "us.anthropic.claude-sonnet-5-5", "region": "us-east-1", "pinned": "us.anthropic.claude-sonnet-5-5"},
        "engine": {"name": "cline", "version": "3.0.71", "binarySha256": "cd".repeat(32)},
        "rules": [],
        "rulesPreflight": {"ran": true, "ms": 812},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 20},
        "nudge": {"requested": false, "honored": false},
        "usage": null,
        "toolCalls": 3,
        "timing": {"startedAt": "2026-11-16T03:46:50Z", "endedAt": "2026-11-16T03:50:00Z", "wallS": 190, "promptS": 180, "maxSilentGapS": 20},
        "git": {"baseSha": "1".repeat(40), "headSha": "2".repeat(40), "committedByHarness": true, "filesChanged": 2,
                "bundleUri": "s3://bucket/runs/01M7H7GG5X7F8A8FKNTAECA34R/agents/direct/turns/1/bundle", "bundleBytes": 1234},
        "snapshots": {"count": 1, "lastAt": "2026-11-16T03:49:00Z"},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.62.0"}
    });
    if let Some(n) = note {
        v["deliveryNote"] = json!(n);
    }
    v
}

const ZIP_BLOCK: &str = "How this run delivers (written by the harness): this run delivers only a zip. A zip takes files under `.github/workflows/` and a root `buildspec.yml`, so add such a file when the task asks for it, and name each one you add in your last message.";

#[test]
fn b850_the_summary_and_the_evidence_carry_what_the_engine_was_told() {
    let s: DirectSummary = serde_json::from_value(summary_json(Some(ZIP_BLOCK))).unwrap();
    assert_eq!(s.delivery_note.as_deref(), Some(ZIP_BLOCK));
    assert_eq!(serde_json::to_value(&s).unwrap()["deliveryNote"], ZIP_BLOCK);
    let e: DirectEvidence = s.to_evidence(HeadSource::Final);
    assert_eq!(e.delivery_note.as_deref(), Some(ZIP_BLOCK));
    assert_eq!(serde_json::to_value(&e).unwrap()["deliveryNote"], ZIP_BLOCK);
}

#[test]
fn b850_a_run_that_was_told_nothing_has_no_member() {
    let s: DirectSummary = serde_json::from_value(summary_json(None)).unwrap();
    assert_eq!(s.delivery_note, None);
    assert!(
        serde_json::to_value(&s)
            .unwrap()
            .get("deliveryNote")
            .is_none()
    );
    let e = s.to_evidence(HeadSource::Final);
    assert_eq!(e.delivery_note, None);
    assert!(
        serde_json::to_value(&e)
            .unwrap()
            .get("deliveryNote")
            .is_none()
    );
}

#[test]
fn b850_a_note_that_is_not_one_short_line_is_left_out_of_the_evidence() {
    assert!(delivery_note_ok(ZIP_BLOCK));
    let long = "x".repeat(MAX_DELIVERY_NOTE_CHARS + 1);
    for bad in ["", "two\nlines", "a\rb", "tab\there", long.as_str()] {
        assert!(!delivery_note_ok(bad), "{:?}", &bad[..bad.len().min(20)]);
        let s: DirectSummary = serde_json::from_value(summary_json(Some(bad))).unwrap();
        assert_eq!(s.to_evidence(HeadSource::Final).delivery_note, None);
    }
}

#[test]
fn b850_the_sentence_about_a_workflow_file_in_a_zip_is_the_one_the_card_shows() {
    let one = [".github/workflows/ci.yml".to_string()];
    assert_eq!(
        zip_holds(&one, &[]).as_deref(),
        Some(
            "The zip holds a workflow file (.github/workflows/ci.yml); it runs only if the code is pushed to a git host that runs such files."
        )
    );
    let three = [
        ".github/workflows/a.yml".to_string(),
        ".github/workflows/b.yml".to_string(),
        ".github/workflows/c.yml".to_string(),
    ];
    assert_eq!(
        zip_holds(&three, &["buildspec.yml".to_string()]).as_deref(),
        Some(
            "The zip holds 3 workflow files (.github/workflows/a.yml and 2 more); they run only if the code is pushed to a git host that runs such files. The zip holds a build specification (buildspec.yml); a build service runs it only if a build project builds this code."
        )
    );
    assert_eq!(zip_holds(&[], &[]), None);
    // The engine chose the file name: a control character becomes a space and a long path is cut.
    let odd = [format!(".github/workflows/{}\n.yml", "w".repeat(200))];
    let s = zip_holds(&odd, &[]).unwrap();
    assert!(!s.contains('\n'), "{s}");
    assert!(s.contains("...)"), "{s}");
}
