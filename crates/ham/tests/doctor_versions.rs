// COMPAT-CAH: this file names the prefix `cah-e2e-new-` of the earlier backend on purpose.
//! The `versions` line of `ham doctor` against a stage that answers with the header of the earlier names only: it still works with this
//! `ham`, and the line says which `--new-repo` names that stage takes and what the delivery card of a run on it shows.
#![allow(clippy::unwrap_used)]

use ham::doctor::{versions_check, versions_check_for};
use ham::version::Generation;

#[test]
fn a_current_stage_reads_as_it_did() {
    let was = versions_check("5.00", Some("5.00"));
    let now = versions_check_for("5.00", Some("5.00"), Generation::Current);
    assert_eq!(
        (was.pass, was.warn, &was.detail),
        (now.pass, now.warn, &now.detail)
    );
    assert!(now.pass && !now.warn);
    assert_eq!(now.detail, "ham v5.00 | backend v5.00");
}

#[test]
fn a_stage_of_the_earlier_names_is_advice_and_names_the_new_repository_prefix() {
    let c = versions_check_for("5.00", Some("4.42"), Generation::Earlier);
    assert!(c.pass && c.warn, "advice, exit code 0: {c:?}");
    assert!(
        c.detail.starts_with("ham v5.00 | backend v4.42"),
        "{}",
        c.detail
    );
    assert!(c.detail.contains("old names"), "{}", c.detail);
    assert!(c.detail.contains("cah-e2e-new-"), "{}", c.detail);
}

#[test]
fn an_unknown_backend_says_what_it_said_before() {
    let c = versions_check_for("5.00", None, Generation::Current);
    assert!(c.warn);
    assert!(c.detail.contains("backend v?.??"), "{}", c.detail);
    assert!(c.detail.contains("reports no version"), "{}", c.detail);
}

#[test]
fn a_stage_of_the_earlier_names_says_what_the_delivery_card_of_its_runs_shows() {
    // The default rules put the records under .ham/, which a backend of the earlier names does not read: its runs end with "No test command was
    // found" on the delivery card, though the code and the tests of the run are fine. The line says so, and keeps what it said before.
    let c = versions_check_for("5.00", Some("4.42"), Generation::Earlier);
    assert!(c.pass && c.warn, "advice, exit code 0: {c:?}");
    for part in [
        "No test command was found",
        "delivery card",
        "torn down and redeployed under the new names",
        "unaffected",
        "cah-e2e-new-",
        "It still works with this ham",
    ] {
        assert!(
            c.detail.contains(part),
            "{part:?} is missing from: {}",
            c.detail
        );
    }
    assert!(!c.detail.contains('\n'), "one line: {}", c.detail);
}

#[test]
fn only_a_stage_of_the_earlier_names_carries_that_sentence() {
    for (backend, generation) in [
        (Some("5.00"), Generation::Current),
        (Some("4.99"), Generation::Current),
        (None, Generation::Current),
        (None, Generation::Earlier),
    ] {
        let c = versions_check_for("5.00", backend, generation);
        assert!(
            !c.detail.contains("No test command"),
            "{backend:?} {generation:?}: {}",
            c.detail
        );
    }
}
