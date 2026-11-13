//! Evidence record header.

use harness_protocol::evidence::{EvidenceError, EvidenceHeader, EvidenceResult, validate_header};

const K0: &str = include_str!("fixtures/evidence-k0.json");
const PAUSE: &str = include_str!("fixtures/evidence-pause.json");

#[test]
fn committed_records_parse_and_validate() {
    let k0: EvidenceHeader = serde_json::from_str(K0).unwrap();
    validate_header(&k0).unwrap();
    assert_eq!(k0.kind, "k0");
    assert_eq!(k0.result, EvidenceResult::Pass);
    assert_eq!(k0.account, "800000000022");
    assert_eq!(
        k0.code_sha.as_deref(),
        Some("86b16b9a90ba908ed62d8609150f3130ead20664")
    );
    assert!(k0.inputs.is_object() && k0.measurements.is_object());

    let pause: EvidenceHeader = serde_json::from_str(PAUSE).unwrap();
    validate_header(&pause).unwrap();
    assert_eq!(pause.source_tree, None);
}

#[test]
fn round_trip_keeps_every_field_name() {
    let k0: EvidenceHeader = serde_json::from_str(K0).unwrap();
    let v = serde_json::to_value(&k0).unwrap();
    let original: serde_json::Value = serde_json::from_str(K0).unwrap();
    assert_eq!(v, original);
}

#[test]
fn header_rules() {
    let good: EvidenceHeader = serde_json::from_str(PAUSE).unwrap();
    let mut h = good.clone();
    h.schema_version = 3;
    assert!(matches!(
        validate_header(&h).unwrap_err(),
        EvidenceError::SchemaVersion(3)
    ));
    let mut h = good.clone();
    h.account = "12345".into();
    assert!(matches!(
        validate_header(&h).unwrap_err(),
        EvidenceError::Account(_)
    ));
    let mut h = good.clone();
    h.created_at = "2026-09-25".into();
    assert!(matches!(
        validate_header(&h).unwrap_err(),
        EvidenceError::CreatedAt(_)
    ));
    let mut h = good.clone();
    h.kind.clear();
    assert!(matches!(
        validate_header(&h).unwrap_err(),
        EvidenceError::Kind
    ));
    let mut h = good;
    h.inputs = serde_json::json!([1, 2]);
    assert!(matches!(
        validate_header(&h).unwrap_err(),
        EvidenceError::NotAnObject("inputs")
    ));
    assert_eq!(serde_json::to_value(EvidenceResult::Fail).unwrap(), "fail");
}
