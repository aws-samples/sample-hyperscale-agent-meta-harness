// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! What agentd reads at the end of a run and records as a fact: the last entry of the run log
//! (`.ham/lifecycle/LOG.md`, the lifecycle pack's record of what was run; `docs/lifecycle/LOG.md` when `.ham/records` says `docs`) when it is a red run. The harness does not judge the work: it
//! writes down what the log says, never the word verified, and says nothing when the last entry is green, is a `NOT RUN` line or cannot
//! be read. The file is the engine's, so it is read without following a link and only its tail.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::observe::{LOG_FILE, last_log_observation, note_says_pass, parse_last_entry};
use common::scratch;
use harness_protocol::direct::{DirectObservationKind, banned_claim_word};

fn red(text: &str) -> Option<(String, String, Option<String>)> {
    parse_last_entry(text).map(|o| {
        (
            o.command.unwrap_or_default(),
            o.exit.unwrap_or_default(),
            o.failed,
        )
    })
}

#[test]
fn a_last_entry_that_is_a_run_with_a_non_zero_exit_is_red() {
    let text = "RUN | pytest -q | exit 0 | pass 5 fail 0 | first\nRUN | pytest -q | exit 1 | pass 3 fail 2 | two fail\n";
    let o = parse_last_entry(text).unwrap();
    assert_eq!(o.kind, DirectObservationKind::LastLogEntryRed);
    assert_eq!(o.command.as_deref(), Some("pytest -q"));
    assert_eq!(
        (o.exit.as_deref(), o.failed.as_deref()),
        (Some("1"), Some("2"))
    );
    assert_eq!(
        o.text,
        "The last entry of the run log is a red run: pytest -q exited 1 and 2 failed."
    );
    assert_eq!(banned_claim_word(&o.text), None);
}

#[test]
fn a_zero_exit_with_failed_tests_is_red_and_a_zero_exit_without_is_not() {
    assert!(red("RUN | pytest | exit 0 | pass 3 fail 1 | x\n").is_some());
    assert!(red("RUN | pytest | exit 0 | pass 3 fail 0 | x\n").is_none());
    // a tool that prints no count: only the exit code speaks
    assert!(red("RUN | make test | exit 0 | pass unknown fail unknown | x\n").is_none());
    assert!(red("RUN | make test | exit 2 | pass unknown fail unknown | x\n").is_some());
    // a line the log format does not allow says nothing: it is neither read as red nor as green
    assert!(red("RUN | make test | exit unknown | pass unknown fail unknown | x\n").is_none());
}

#[test]
fn only_the_last_entry_counts_and_a_not_run_line_is_an_entry() {
    let green_after =
        "RUN | pytest | exit 1 | pass 1 fail 1 | x\nRUN | pytest | exit 0 | pass 2 fail 0 | y\n";
    assert!(red(green_after).is_none());
    let not_run_after = "RUN | pytest | exit 1 | pass 1 fail 1 | x\nNOT RUN | integration tests | no database here\n";
    assert!(red(not_run_after).is_none());
    // other lines (notes, a BLOCKED line, blanks) are not entries
    let noise = "RUN | pytest | exit 1 | pass 1 fail 1 | x\n\nnotes about the work\nBLOCKED: need a key\n  \n";
    assert!(red(noise).is_some());
}

#[test]
fn a_last_entry_that_cannot_be_read_says_nothing() {
    for text in [
        "",
        "\n\n",
        "RUN\n",
        "RUN | pytest\n",
        "RUN | pytest | passed\n",
        "NOT RUN | x | y\n",
        "garbage",
    ] {
        assert!(parse_last_entry(text).is_none(), "{text:?}");
    }
    // CRLF line ends and trailing spaces
    assert!(red("RUN | pytest | exit 1 | pass 1 fail 1 | x \r\n").is_some());
}

#[test]
fn a_command_with_a_claim_word_is_left_out_but_the_red_run_is_still_recorded() {
    let o = parse_last_entry("RUN | pytest tests/test_verified.py | exit 1 | pass 0 fail 1 | x\n")
        .unwrap();
    assert_eq!(o.command, None);
    assert_eq!(o.exit.as_deref(), Some("1"));
    assert_eq!(banned_claim_word(&o.text), None);
}

fn worktree(label: &str, log: &str) -> std::path::PathBuf {
    let d = scratch(label);
    std::fs::create_dir_all(d.join(".ham/lifecycle")).unwrap();
    std::fs::write(d.join(LOG_FILE), log).unwrap();
    d
}

#[test]
fn the_log_is_read_from_the_work_tree_and_a_missing_file_says_nothing() {
    let d = worktree(
        "observe-ok",
        "RUN | pytest -q | exit 1 | pass 3 fail 2 | x\n",
    );
    assert!(last_log_observation(&d).is_some());
    let empty = scratch("observe-none");
    assert!(last_log_observation(&empty).is_none());
}

#[test]
fn a_link_where_the_log_or_its_folders_belong_is_never_followed() {
    let outside = scratch("observe-outside");
    std::fs::write(
        outside.join("other.md"),
        "RUN | pytest -q | exit 1 | pass 0 fail 1 | x\n",
    )
    .unwrap();
    // the log itself is a link
    let d = worktree("observe-link", "");
    std::fs::remove_file(d.join(LOG_FILE)).unwrap();
    std::os::unix::fs::symlink(outside.join("other.md"), d.join(LOG_FILE)).unwrap();
    assert!(last_log_observation(&d).is_none());
    // the folder .ham/lifecycle is a link
    let d = scratch("observe-folder-link");
    std::fs::create_dir_all(d.join(".ham")).unwrap();
    std::fs::create_dir_all(outside.join("lifecycle")).unwrap();
    std::fs::write(
        outside.join("lifecycle/LOG.md"),
        "RUN | pytest -q | exit 1 | pass 0 fail 1 | x\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.join("lifecycle"), d.join(".ham/lifecycle")).unwrap();
    assert!(last_log_observation(&d).is_none());
    // .ham is a link
    let d = scratch("observe-ham-link");
    std::fs::create_dir_all(outside.join("ham2")).unwrap();
    std::os::unix::fs::symlink(outside.join("lifecycle"), outside.join("ham2/lifecycle")).unwrap();
    std::os::unix::fs::symlink(outside.join("ham2"), d.join(".ham")).unwrap();
    assert!(last_log_observation(&d).is_none());
    // a directory where the file belongs
    let d = scratch("observe-dir");
    std::fs::create_dir_all(d.join(LOG_FILE)).unwrap();
    assert!(last_log_observation(&d).is_none());
}

#[test]
fn only_the_tail_of_a_big_log_is_read() {
    let mut text = String::new();
    text.push_str("RUN | pytest | exit 1 | pass 0 fail 1 | the first line is red\n");
    text.push_str(&"NOT RUN | filler | a long note about nothing at all\n".repeat(20_000));
    text.push_str("RUN | pytest | exit 0 | pass 9 fail 0 | green at the end\n");
    assert!(text.len() > 512 * 1024);
    let d = worktree("observe-big", &text);
    assert!(
        last_log_observation(&d).is_none(),
        "the last entry is green"
    );
    // a red last entry is found in the tail
    let mut text = "x\n".repeat(400_000);
    text.push_str("RUN | pytest | exit 1 | pass 0 fail 1 | red at the end\n");
    let d = worktree("observe-big-red", &text);
    assert!(last_log_observation(&d).is_some());
}

// ---- where the log is: `.ham/records` moves it or turns it off --------------------------------------------------------------

const RED: &str = "RUN | pytest -q | exit 1 | pass 3 fail 2 | x\n";

fn tree_with(label: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let d = scratch(label);
    for (name, body) in files {
        let p = d.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    d
}

#[test]
fn a_repository_without_a_locator_is_read_in_the_hidden_default_and_not_in_docs() {
    // The log of a run sits in .ham/lifecycle unless the repository says otherwise
    assert_eq!(LOG_FILE, ".ham/lifecycle/LOG.md");
    let d = tree_with("observe-default", &[(".ham/lifecycle/LOG.md", RED)]);
    let o = last_log_observation(&d).unwrap();
    assert_eq!(o.command.as_deref(), Some("pytest -q"));
    // a red log left in docs/lifecycle (an older run, an older rules file) is not read, and the hidden log is green
    let d = tree_with(
        "observe-default-green",
        &[
            (
                ".ham/lifecycle/LOG.md",
                "RUN | pytest -q | exit 0 | pass 5 fail 0 | ok\n",
            ),
            ("docs/lifecycle/LOG.md", RED),
        ],
    );
    assert!(last_log_observation(&d).is_none());
    // with only a log in docs/lifecycle and no locator there is nothing to observe
    let d = tree_with("observe-docs-only", &[("docs/lifecycle/LOG.md", RED)]);
    assert!(last_log_observation(&d).is_none());
}

#[test]
fn a_moved_log_is_read_and_the_log_of_the_default_place_is_not() {
    let d = tree_with(
        "observe-moved",
        &[(".ham/records", "docs\n"), ("docs/lifecycle/LOG.md", RED)],
    );
    let o = last_log_observation(&d).unwrap();
    assert_eq!(o.command.as_deref(), Some("pytest -q"));
    // a red log left in .ham/lifecycle is stale when the records moved to docs, and the log of docs is green
    let d = tree_with(
        "observe-moved-green",
        &[
            (".ham/records", "docs\n"),
            (
                "docs/lifecycle/LOG.md",
                "RUN | pytest -q | exit 0 | pass 5 fail 0 | ok\n",
            ),
            (".ham/lifecycle/LOG.md", RED),
        ],
    );
    assert!(last_log_observation(&d).is_none());
    // the value .ham is the default place
    let d = tree_with(
        "observe-moved-named-default",
        &[(".ham/records", ".ham\n"), (".ham/lifecycle/LOG.md", RED)],
    );
    assert!(last_log_observation(&d).is_some());
}

#[test]
fn records_off_observes_nothing_whatever_a_log_says() {
    let d = tree_with(
        "observe-off",
        &[
            (".ham/records", "off\n"),
            ("docs/lifecycle/LOG.md", RED),
            (".ham/lifecycle/LOG.md", RED),
        ],
    );
    assert!(last_log_observation(&d).is_none());
}

#[test]
fn an_invalid_locator_leaves_the_log_of_the_default_place_observed() {
    let d = tree_with(
        "observe-invalid",
        &[(".ham/records", "../x\n"), (".ham/lifecycle/LOG.md", RED)],
    );
    assert!(last_log_observation(&d).is_some());
}

#[test]
fn a_link_in_the_path_of_a_moved_log_is_never_followed() {
    let outside = scratch("observe-moved-outside");
    std::fs::create_dir_all(outside.join("real")).unwrap();
    std::fs::write(outside.join("real/LOG.md"), RED).unwrap();
    // the folder that takes the place of .ham is a link
    let d = tree_with("observe-moved-link", &[(".ham/records", "a/b\n")]);
    std::fs::create_dir_all(d.join("a")).unwrap();
    std::fs::create_dir_all(outside.join("base")).unwrap();
    std::os::unix::fs::symlink(outside.join("base"), d.join("a/b")).unwrap();
    std::fs::create_dir_all(outside.join("base/lifecycle")).unwrap();
    std::fs::write(outside.join("base/lifecycle/LOG.md"), RED).unwrap();
    assert!(last_log_observation(&d).is_none());
    // a folder above it is a link
    let d = tree_with("observe-moved-link-up", &[(".ham/records", "a/b\n")]);
    std::fs::create_dir_all(outside.join("up/b/lifecycle")).unwrap();
    std::fs::write(outside.join("up/b/lifecycle/LOG.md"), RED).unwrap();
    std::os::unix::fs::symlink(outside.join("up"), d.join("a")).unwrap();
    assert!(last_log_observation(&d).is_none());
    // lifecycle is a link
    let d = tree_with("observe-moved-link-lifecycle", &[(".ham/records", "a/b\n")]);
    std::fs::create_dir_all(d.join("a/b")).unwrap();
    std::os::unix::fs::symlink(outside.join("real"), d.join("a/b/lifecycle")).unwrap();
    assert!(last_log_observation(&d).is_none());
    // the log is a link
    let d = tree_with("observe-moved-link-file", &[(".ham/records", "a/b\n")]);
    std::fs::create_dir_all(d.join("a/b/lifecycle")).unwrap();
    std::os::unix::fs::symlink(outside.join("real/LOG.md"), d.join("a/b/lifecycle/LOG.md"))
        .unwrap();
    assert!(last_log_observation(&d).is_none());
}

// ---- a row that carries its own verdict ------------------------------------------------------------------------
//
// The log is the engine's. A wrapper script that compares a failing suite with a list of failures the base commit already has logs
// `exit 0 | pass 6285 fail 161 | NEW-FAILURES PASS (every failure is also a failure of the base commit)`, and the card called it "a red run... and 161
// failed". The row says PASS in its own note, so the harness does not call it red. The exit code stays the first
// word: a non-zero code is red whatever the note says, and a row with failed tests and a note that does not say PASS stays red.

#[test]
fn a_row_with_failures_in_its_count_and_pass_in_its_note_is_not_red() {
    let row = "RUN | sh docs/lifecycle/verify-scripts-suite.sh | exit 0 | pass 6285 fail 161 | NEW-FAILURES PASS (every failure is also a failure of the base commit)\n";
    assert!(red(row).is_none(), "{:?}", red(row));
    assert!(red("RUN | make check | exit 0 | pass 10 fail 2 | PASS\n").is_none());
    assert!(red("RUN | make check | exit 0 | pass 10 fail 2 | NEW-FAILURES PASS\n").is_none());
}

#[test]
fn the_exit_code_is_red_whatever_the_note_says() {
    assert!(red("RUN | make check | exit 1 | pass 10 fail 2 | PASS\n").is_some());
    assert!(red("RUN | make check | exit 2 | pass unknown fail unknown | PASS\n").is_some());
}

#[test]
fn failures_with_a_note_that_does_not_say_pass_stay_red() {
    for note in [
        "x",
        "2 failed, 10 passed in 0.3s",
        "pass",
        "PASSED",
        "did not PASS",
        "NOT PASS",
        "no PASS yet",
        "never PASS",
        "didn't PASS",
        "PASS expected, 2 FAILED observed",
        "PASS or FAIL",
        "FAIL",
        "FAILED",
    ] {
        let row = format!("RUN | make check | exit 0 | pass 10 fail 2 | {note}\n");
        assert!(red(&row).is_some(), "{note:?}");
    }
}

/// The notes the self-check script (`scripts/tests/test_lifecycle_check.py`, `PassNoteCase`) and the scorer read too: one file,
/// so the three readers of a run row give the same answer and the self-check never calls red a row that the card does not.
#[test]
fn every_shared_note_case_is_read_as_the_script_and_the_scorer_read_it() {
    let path = common::repo_root().join("fixtures/lifecycle/pass-notes.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let cases = v["cases"].as_array().unwrap();
    assert!(cases.len() >= 40, "{}", cases.len());
    assert!(cases.iter().any(|c| c["says_pass"] == true));
    assert!(cases.iter().any(|c| c["says_pass"] == false));
    for c in cases {
        let note = c["note"].as_str().unwrap();
        assert_eq!(note_says_pass(note), c["says_pass"] == true, "{note:?}");
    }
}

// ---- the earlier folder (COMPAT-CAH-DATA): a repository that keeps its records in `.cah` is observed there ----------------------------------

#[test]
fn a_repository_that_keeps_its_records_in_the_earlier_folder_is_observed_there() {
    let d = tree_with("observe-earlier", &[(".cah/lifecycle/LOG.md", RED)]);
    let o = last_log_observation(&d).unwrap();
    assert_eq!(o.command.as_deref(), Some("pytest -q"));
    // The new folder holds no state, so it hides nothing; the log of a folder the run does not use is not read.
    let d = tree_with(
        "observe-earlier-empty-ham",
        &[(".ham/README", "x\n"), (".cah/lifecycle/LOG.md", RED)],
    );
    assert!(last_log_observation(&d).is_some());
    // A moved folder named in the earlier folder's own locator.
    let d = tree_with(
        "observe-earlier-moved",
        &[
            (".cah/records", "run-records\n"),
            ("run-records/lifecycle/LOG.md", RED),
        ],
    );
    assert!(last_log_observation(&d).is_some());
}

#[test]
fn state_in_both_folders_observes_nothing_in_the_earlier_one() {
    // The run is refused at its start; a tree that reaches the reader this way reads the default places only.
    let d = tree_with(
        "observe-both",
        &[
            (".ham/lifecycle/GOAL.md", "x\n"),
            (".cah/lifecycle/LOG.md", RED),
        ],
    );
    assert!(last_log_observation(&d).is_none());
}
