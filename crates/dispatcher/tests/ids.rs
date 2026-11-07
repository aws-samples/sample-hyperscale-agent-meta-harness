//! Run, agent and session identifiers.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use dispatcher::ids::{
    IdError, MIN_RUNTIME_SESSION_ID, is_id, role_session_name, runtime_session_id,
};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

#[test]
fn ids_are_letters_digits_and_hyphens() {
    assert!(is_id(RUN));
    assert!(is_id("a1"));
    assert!(is_id("k-suite-1"));
    for bad in ["", "a.b", "a b", "a/b", "a_b", "ä"] {
        assert!(!is_id(bad), "{bad:?}");
    }
}

#[test]
fn the_role_session_name_is_run_dot_agent_within_64_characters() {
    assert_eq!(role_session_name(RUN, "a1").unwrap(), format!("{RUN}.a1"));
    assert_eq!(
        role_session_name(&"R".repeat(59), "abcd").unwrap().len(),
        64
    );
    assert_eq!(
        role_session_name(&"R".repeat(60), "abcd"),
        Err(IdError::TooLong)
    );
    assert_eq!(role_session_name("r.1", "a"), Err(IdError::Bad("run")));
    assert_eq!(role_session_name("r", ""), Err(IdError::Bad("agent")));
    assert!(IdError::TooLong.to_string().contains("64"));
    assert!(IdError::Bad("run").to_string().contains("run"));
}

#[test]
fn runtime_session_ids_are_deterministic_long_enough_and_distinct_per_generation() {
    let a = runtime_session_id(RUN, "a1", 1);
    assert!(a.len() >= MIN_RUNTIME_SESSION_ID, "{a}");
    assert_eq!(MIN_RUNTIME_SESSION_ID, 33);
    assert_eq!(a, runtime_session_id(RUN, "a1", 1));
    assert_ne!(a, runtime_session_id(RUN, "a1", 2));
    assert!(a.starts_with(&format!("{RUN}-a1-")));
    assert!(is_id(&a));
    // Short ids are padded up to the minimum; long ones keep their natural length.
    let short = runtime_session_id("R1", "a1", 7);
    assert_eq!(short.len(), MIN_RUNTIME_SESSION_ID);
    assert!(short.ends_with('7'));
    let long = runtime_session_id(&"R".repeat(59), "abcd", 12);
    assert_eq!(long, format!("{}-abcd-g12", "R".repeat(59)));
    assert!(long.len() <= 256);
}

#[test]
fn control_ids_are_ulids_that_sort_by_time() {
    let a = dispatcher::ids::new_ulid();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let b = dispatcher::ids::new_ulid();
    assert!(harness_protocol::common::is_ulid(&a) && harness_protocol::common::is_ulid(&b));
    assert!(a < b, "{a} {b}");
    assert_ne!(dispatcher::ids::new_ulid(), dispatcher::ids::new_ulid());
}
