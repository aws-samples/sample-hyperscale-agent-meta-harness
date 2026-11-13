//! The version records of the three end-of-run objects. agentd writes `bundle`, `summary.json` and `test-report.json` and keeps what S3
//! answered to each of its own writes: the SHA-256 of the body and the version id. The records ride in the key-protected `turn-complete`
//! body, which the engine cannot send, so delivery can read exactly the objects agentd wrote.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::output_pins::{OutputPin, OutputPins};
use harness_protocol::store::CredsBinding;
use serde_json::json;

const SHA: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

fn pin(version: Option<&str>) -> OutputPin {
    OutputPin {
        sha256: SHA.to_string(),
        version_id: version.map(str::to_string),
    }
}

fn turn(extra: serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "runId": "01M4XTESTRUNDIRECT00000001", "agentId": "direct", "turn": 1, "finishReason": "end_turn"
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

#[test]
fn b759_a_pin_needs_a_lowercase_hex_digest() {
    assert!(pin(None).validate().is_ok());
    for bad in [
        "",
        "AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12AB12",
        &SHA[..63],
        &format!("{SHA}0"),
        &SHA.replace("ab", "zz"),
    ] {
        let p = OutputPin {
            sha256: bad.to_string(),
            version_id: None,
        };
        assert!(p.validate().is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn b759_a_version_id_is_one_printable_token_and_never_null() {
    assert!(
        pin(Some(
            "3sL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY+MTRCxf3vjVBH40Nr8X8gdRQBpUMLUo"
        ))
        .validate()
        .is_ok()
    );
    for bad in ["", "null", "a b", "a\nb", "a\tb", &"v".repeat(1025), "é"] {
        assert!(
            pin(Some(bad)).validate().is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn b759_the_pin_reads_as_the_base64_form_s3_reports() {
    // S3 reports `x-amz-checksum-sha256` as the base64 of the digest; the pin keeps hex.
    let p = OutputPin {
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        version_id: None,
    };
    assert_eq!(
        p.sha256_base64(),
        "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
    );
}

#[test]
fn b759_a_turn_complete_body_carries_the_pins_and_an_old_body_still_parses() {
    let with: TurnCompleteRequest = serde_json::from_value(turn(json!({
        "outputPins": {
            "bundle": {"sha256": SHA, "versionId": "v-bundle"},
            "summary": {"sha256": SHA, "versionId": "v-summary"},
            "testReport": {"sha256": SHA}
        }
    })))
    .unwrap();
    let pins = with.output_pins.as_ref().unwrap();
    assert_eq!(
        pins.bundle.as_ref().unwrap().version_id.as_deref(),
        Some("v-bundle")
    );
    assert_eq!(
        pins.summary.as_ref().unwrap().version_id.as_deref(),
        Some("v-summary")
    );
    assert_eq!(pins.test_report.as_ref().unwrap().version_id, None);
    assert!(pins.validate().is_ok());
    // The member round-trips under its camelCase names.
    let back = serde_json::to_value(&with).unwrap();
    assert_eq!(back["outputPins"]["testReport"]["sha256"], SHA);

    let old: TurnCompleteRequest = serde_json::from_value(turn(json!({}))).unwrap();
    assert!(old.output_pins.is_none());
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("outputPins")
            .is_none()
    );
}

#[test]
fn b759_a_pin_set_with_one_bad_member_is_refused_as_a_whole() {
    let pins = OutputPins {
        bundle: Some(pin(Some("v1"))),
        summary: Some(OutputPin {
            sha256: "nothex".into(),
            version_id: None,
        }),
        test_report: None,
    };
    let why = pins.validate().unwrap_err();
    assert!(why.contains("summary"), "{why}");
}

#[test]
fn b759_a_pin_set_with_no_member_is_empty() {
    assert!(OutputPins::default().is_empty());
    let pins = OutputPins {
        bundle: Some(pin(None)),
        ..OutputPins::default()
    };
    assert!(!pins.is_empty());
}

#[test]
fn b759_a_binding_says_whether_the_run_requires_pins_and_an_old_item_does_not() {
    let old: CredsBinding = serde_json::from_value(json!({
        "nonceSha256": "00".repeat(32), "revoked": false, "tenant": "t", "mode": "m", "sessionPolicy": "{}"
    }))
    .unwrap();
    assert!(!old.output_pins_required());
    let new: CredsBinding = serde_json::from_value(json!({
        "nonceSha256": "00".repeat(32), "revoked": false, "tenant": "t", "mode": "m", "sessionPolicy": "{}",
        "outputPinsRequired": true
    }))
    .unwrap();
    assert!(new.output_pins_required());
    assert_eq!(
        serde_json::to_value(&old)
            .unwrap()
            .get("outputPinsRequired"),
        None
    );
}
