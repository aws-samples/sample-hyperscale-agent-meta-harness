//! A run blocked because its repository is not on the stage's allowlist says what to do: the refusal read `refused: cc:<name> is not on the
//! allowlist (REPOALLOW#cc:<Region>:<name>)`, an internal key and no remedy. It now names the operator command that allows the repository.

#![allow(clippy::unwrap_used)]

use delivery::checks::check_allowlisted;
use delivery::error::DeliveryError;
use harness_protocol::target::RepoTarget;

fn target(s: &str) -> RepoTarget {
    s.parse().unwrap()
}

fn refused(r: Result<(), DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn b719_the_allowlist_refusal_names_the_command_that_allows_the_repository() {
    let reason = refused(check_allowlisted(&target("cc:hamh-e2e-python-calc"), None));
    assert_eq!(
        reason,
        "refused: cc:hamh-e2e-python-calc is not on the allowlist of this stage; an operator allows it with `ham-admin repos allow cc:hamh-e2e-python-calc`"
    );
}

#[test]
fn b719_the_allowlist_refusal_shows_no_internal_key() {
    let reason = refused(check_allowlisted(&target("cc:hamh-e2e-python-calc"), None));
    assert!(!reason.contains("REPOALLOW#"), "{reason}");
    assert!(reason.contains("is not on the allowlist"), "{reason}");
}
