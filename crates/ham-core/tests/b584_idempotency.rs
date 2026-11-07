//! The backend alias is part of the idempotency key input when several backends exist, and a one-backend key is what it was.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, parse};
use ham_core::models::ModelPrefs;
use ham_core::request::{
    DirectInputs, direct_idempotency_key, direct_idempotency_key_on, direct_run_request,
};
use harness_protocol::api::v1::StartDirectRunRequest;

fn request(prompt: &str) -> StartDirectRunRequest {
    let Command::StartDirect(args) = parse(&[prompt, "--s3-only", "--direct"]).unwrap().command
    else {
        panic!("not a start");
    };
    direct_run_request(&args, DirectInputs::default(), &ModelPrefs::default()).unwrap()
}

#[test]
fn b584_without_an_alias_the_key_is_the_one_of_before() {
    let r = request("add a test");
    assert_eq!(
        direct_idempotency_key_on(&r, "n1", None),
        direct_idempotency_key(&r, "n1")
    );
}

#[test]
fn b584_the_alias_changes_the_key_and_the_same_alias_keeps_it() {
    let r = request("add a test");
    let work = direct_idempotency_key_on(&r, "n1", Some("work"));
    assert_eq!(
        work,
        direct_idempotency_key_on(&r, "n1", Some("work")),
        "a retry sends the same key"
    );
    assert_ne!(work, direct_idempotency_key_on(&r, "n1", Some("spare")));
    assert_ne!(work, direct_idempotency_key(&r, "n1"));
    assert_eq!(work.len(), 64);
}

#[test]
fn b584_an_alias_cannot_be_moved_into_the_nonce_to_collide() {
    let r = request("add a test");
    assert_ne!(
        direct_idempotency_key_on(&r, "n1", Some("work")),
        direct_idempotency_key_on(&r, "n1work", Some("")),
    );
}
