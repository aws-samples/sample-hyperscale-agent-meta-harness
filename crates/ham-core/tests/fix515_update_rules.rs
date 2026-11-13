//! Rules of `ham update` that the earlier tests left out: an update of the backend alone, versions that are not of the form `N.NN`, and
//! a changelog asked about with such a version.
#![allow(clippy::unwrap_used)]

use ham_core::update::{Parts, Plan, Refusal, changes_between, plan};

const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n## [5.16] - 2026-11-20\n\n- The newest change.\n\n## [5.15] - 2026-11-12\n\n- An older change.\n";

/// The client runs the newest version and the backend an older one: the update changes the backend only, and the plan says so.
#[test]
fn fix515_a_backend_behind_a_current_client_is_an_update_of_the_backend_only() {
    let p = plan("5.16", Some("5.15"), "5.16");
    assert_eq!(p, Plan::Update(Parts::BackendOnly));
    assert_eq!(Parts::BackendOnly.words(), "backend only");
    assert_eq!(Parts::ClientOnly.words(), "client only");
    assert_eq!(Parts::BackendAndClient.words(), "backend and client");
}

/// A client or a backend whose version is not `N.NN` stops the update with that version named; none of them is compared as text.
#[test]
fn fix515_a_version_that_is_not_of_the_form_n_nn_stops_the_update_and_is_named() {
    assert_eq!(
        plan("dev", Some("5.15"), "5.16"),
        Plan::Refuse(Refusal::BadVersion("dev".into())),
        "the client"
    );
    assert_eq!(
        plan("5.15", Some("mixed"), "5.16"),
        Plan::Refuse(Refusal::BadVersion("mixed".into())),
        "the backend"
    );
    assert_eq!(
        plan("5.15", Some("5.15"), "5.160"),
        Plan::Refuse(Refusal::BadVersion("5.160".into())),
        "main"
    );
}

/// The changes between two versions are empty when either end is not a version: no section is guessed.
#[test]
fn fix515_the_changes_between_versions_are_empty_when_an_end_is_no_version() {
    assert_eq!(changes_between(CHANGELOG, "5.15", "5.16").len(), 1);
    assert!(changes_between(CHANGELOG, "dev", "5.16").is_empty());
    assert!(changes_between(CHANGELOG, "5.15", "newest").is_empty());
    assert!(changes_between(CHANGELOG, "", "").is_empty());
}
