// COMPAT-CAH: this file names the header of the earlier backend, `x-cah-backend-version`, on purpose.
//! The two headers that carry the backend's version while a stage may still run the code of the earlier names: the client reads
//! `x-hamh-backend-version` first and `x-cah-backend-version` second; with neither (or with neither valid) the backend reads as unknown, which
//! the tag writes `v?.??`. The client also remembers which header the last valid answer carried, because a stage that answers with the earlier
//! header only still expects the earlier `--new-repo` names.
#![allow(clippy::unwrap_used)]

use ham::doctor::versions_check_for;
use ham::version::{self, BackendCell, Generation};
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

const NEW: &str = "x-hamh-backend-version";
const OLD: &str = "x-cah-backend-version"; // COMPAT-CAH

#[test]
fn the_names_of_the_headers_are_the_two_the_backend_sends() {
    assert_eq!(version::BACKEND_HEADER, NEW);
    assert_eq!(version::EARLIER_BACKEND_HEADER, OLD);
}

#[test]
fn the_new_header_is_read() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(NEW, "5.00")]));
    assert_eq!(cell.backend().as_deref(), Some("5.00"));
    assert_eq!(cell.generation(), Generation::Current);
}

#[test]
fn the_earlier_header_is_read_when_it_is_the_only_one() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(OLD, "4.42")]));
    assert_eq!(cell.backend().as_deref(), Some("4.42"));
    assert_eq!(cell.generation(), Generation::Earlier);
}

#[test]
fn the_new_header_wins_when_both_are_sent() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(NEW, "5.00"), (OLD, "5.00")]));
    assert_eq!(cell.backend().as_deref(), Some("5.00"));
    assert_eq!(cell.generation(), Generation::Current);
    cell.note_answer(200, &headers(&[(NEW, "5.01"), (OLD, "4.99")]));
    assert_eq!(cell.backend().as_deref(), Some("5.01"));
}

#[test]
fn an_invalid_new_header_falls_to_a_valid_earlier_one_and_the_stage_is_still_of_the_current_generation()
 {
    let cell = BackendCell::new();
    // The backend sends both headers: a malformed new one does not turn the stage into one of the earlier names, whose prefix it would not take.
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(NEW, "v5.00"), (OLD, "4.42")]));
    assert_eq!(cell.backend().as_deref(), Some("4.42"));
    assert_eq!(cell.generation(), Generation::Current);
}

#[test]
fn a_new_header_that_is_no_version_still_names_the_generation() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(OLD, "4.42")]));
    assert_eq!(cell.generation(), Generation::Earlier);
    cell.note_answer(200, &headers(&[(NEW, "5.0"), (OLD, "x")]));
    assert_eq!(cell.backend(), None);
    assert_eq!(cell.generation(), Generation::Current);
}

#[test]
fn an_earlier_header_that_is_no_version_still_names_the_generation() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(OLD, "4.4")]));
    assert_eq!(cell.backend(), None);
    assert_eq!(cell.generation(), Generation::Earlier);
}

#[test]
fn neither_header_reads_as_unknown_and_the_exit_code_is_not_involved() {
    let cell = BackendCell::new();
    cell.set_for_test(Some("5.00"));
    cell.note_answer(200, &headers(&[]));
    assert_eq!(cell.backend(), None);
    cell.set_for_test(Some("5.00"));
    cell.note_answer(200, &headers(&[(NEW, "5.0"), (OLD, "x")]));
    assert_eq!(cell.backend(), None);
}

#[test]
fn a_gateway_answer_with_no_header_keeps_the_version_and_the_generation() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(OLD, "4.42")]));
    cell.note_answer(503, &headers(&[]));
    assert_eq!(cell.backend().as_deref(), Some("4.42"));
    assert_eq!(cell.generation(), Generation::Earlier);
}

#[test]
fn the_generation_is_the_one_last_seen_and_survives_an_answer_with_no_version() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(OLD, "4.42")]));
    cell.note_answer(404, &headers(&[]));
    assert_eq!(cell.backend(), None);
    assert_eq!(cell.generation(), Generation::Earlier);
    cell.note_answer(200, &headers(&[(NEW, "5.00")]));
    assert_eq!(cell.generation(), Generation::Current);
}

#[test]
fn before_any_answer_the_generation_is_the_current_one() {
    let cell = BackendCell::new();
    cell.set_for_test(None);
    assert_eq!(cell.generation(), Generation::Current);
}

#[test]
fn new_repositories_get_the_prefix_of_the_generation_last_seen() {
    assert_eq!(
        version::new_repo_prefix(Generation::Current),
        harness_protocol::target::NEW_REPO_PREFIXES[0]
    );
    assert_eq!(
        version::new_repo_prefix(Generation::Current),
        "hamh-e2e-new-"
    );
    assert_eq!(
        version::new_repo_prefix(Generation::Earlier),
        "cah-e2e-new-"
    );
}

#[test]
fn doctor_warns_about_the_delivery_card_only_after_an_answer_with_the_earlier_header_alone() {
    let cell = BackendCell::new();
    let line = || versions_check_for("5.00", cell.backend().as_deref(), cell.generation());
    cell.set_for_test(None);
    cell.note_answer(200, &headers(&[(OLD, "4.42")]));
    let earlier = line();
    assert!(earlier.warn, "{earlier:?}");
    assert!(
        earlier.detail.contains("No test command was found"),
        "{}",
        earlier.detail
    );
    assert!(
        earlier
            .detail
            .contains("torn down and redeployed under the new names"),
        "{}",
        earlier.detail
    );
    // The next answer carries the current header as well: the stage is of the current generation and the sentence is gone.
    cell.note_answer(200, &headers(&[(NEW, "5.00"), (OLD, "5.00")]));
    let current = line();
    assert!(!current.warn, "{current:?}");
    assert_eq!(current.detail, "ham v5.00 | backend v5.00");
}

// ---- the tests above own their cell, so they run side by side under plain `cargo test` (threads of one process); the tests below say so ----

/// Held by the one test of this binary that reads and writes the process-wide cell.
static GLOBAL: Mutex<()> = Mutex::new(());

#[test]
fn b731_two_cells_do_not_see_each_other() {
    let (a, b) = (BackendCell::new(), BackendCell::new());
    a.note_answer(200, &headers(&[(NEW, "5.00")]));
    b.note_answer(200, &headers(&[(OLD, "4.42")]));
    assert_eq!(a.backend().as_deref(), Some("5.00"));
    assert_eq!(a.generation(), Generation::Current);
    assert_eq!(b.backend().as_deref(), Some("4.42"));
    assert_eq!(b.generation(), Generation::Earlier);
}

#[test]
fn b731_threads_with_a_cell_each_keep_their_own_version() {
    let threads: Vec<_> = (0..16)
        .map(|i| {
            std::thread::spawn(move || {
                let cell = BackendCell::new();
                let value = format!("5.{i:02}");
                for _ in 0..200 {
                    cell.note_answer(200, &headers(&[(NEW, &value)]));
                    assert_eq!(cell.backend().as_deref(), Some(value.as_str()));
                    std::thread::yield_now();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn b731_a_new_cell_is_unknown_and_of_the_current_generation() {
    let cell = BackendCell::new();
    assert_eq!(cell.backend(), None);
    assert_eq!(cell.generation(), Generation::Current);
}

#[test]
fn b731_a_default_cell_is_a_new_cell() {
    let cell = BackendCell::default();
    cell.note_answer(200, &headers(&[(NEW, "5.00")]));
    assert_eq!(cell.backend().as_deref(), Some("5.00"));
    assert_eq!(BackendCell::default().backend(), None);
}

#[test]
fn b731_the_free_functions_are_the_one_process_wide_cell() {
    let _only_user = GLOBAL.lock().unwrap_or_else(PoisonError::into_inner);
    version::set_backend_for_test(None);
    version::note_answer(200, &headers(&[(NEW, "5.07")]));
    assert_eq!(version::backend().as_deref(), Some("5.07"));
    assert_eq!(version::generation(), Generation::Current);
    version::note_answer(200, &headers(&[(OLD, "4.42")]));
    assert_eq!(version::backend().as_deref(), Some("4.42"));
    assert_eq!(version::generation(), Generation::Earlier);
    version::set_backend_for_test(None);
    assert_eq!(version::backend(), None);
    assert_eq!(version::generation(), Generation::Current);
}
