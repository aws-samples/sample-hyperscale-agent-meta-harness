//! The backend version the tag shows survives answers the API gateway makes itself.
//!
//! A refusal by IAM (403), a throttle (429) and a gateway or Lambda failure (5xx) never reach the control-api handler, so they carry no
//! `x-hamh-backend-version` header. The tag must not turn to `backend v?.??` for one poll because of them. An answer from the backend
//! itself (2xx, other 4xx) without the header means the backend predates the tag, and that does read as unknown.

use ham::version::{self, BackendCell};
use std::collections::BTreeMap;

fn headers(value: Option<&str>) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    if let Some(v) = value {
        h.insert(version::BACKEND_HEADER.to_string(), v.to_string());
    }
    h
}

#[test]
fn a_gateway_answer_without_the_header_keeps_the_last_version() {
    let cell = BackendCell::new();
    for status in [403, 429, 500, 502, 503, 504] {
        cell.set_for_test(Some("4.02"));
        cell.note_answer(status, &headers(None));
        assert_eq!(cell.backend().as_deref(), Some("4.02"), "status {status}");
    }
}

#[test]
fn a_gateway_answer_without_the_header_leaves_an_unknown_version_unknown() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(504, &headers(None));
    assert_eq!(cell.backend(), None);
}

#[test]
fn an_answer_from_the_backend_without_the_header_reads_as_unknown() {
    let cell = BackendCell::new();
    for status in [200, 201, 204, 400, 404, 409] {
        cell.set_for_test(Some("4.02"));
        cell.note_answer(status, &headers(None));
        assert_eq!(cell.backend(), None, "status {status}");
    }
}

#[test]
fn a_valid_header_always_replaces_the_version() {
    let cell = BackendCell::new();
    for status in [200, 404, 429, 503] {
        cell.set_for_test(Some("4.01"));
        cell.note_answer(status, &headers(Some("4.02")));
        assert_eq!(cell.backend().as_deref(), Some("4.02"), "status {status}");
    }
}

#[test]
fn an_invalid_header_is_treated_like_a_missing_one() {
    let cell = BackendCell::new();
    cell.set_for_test(Some("4.02"));
    cell.note_answer(200, &headers(Some("v4.02")));
    assert_eq!(cell.backend(), None);
    cell.set_for_test(Some("4.02"));
    cell.note_answer(502, &headers(Some("4.2")));
    assert_eq!(cell.backend().as_deref(), Some("4.02"));
}
