//! The client's words on when a run saves a copy. The worker stores a
//! copy when a file changed or the engine's head moved to another commit, so no sentence of the client says that a copy needs a file
//! change: the note of a run that has no copy (which the guide quotes), the help of the two copy-interval keys and the README's row
//! of the key. The look after a tool call stores a moved head too, so the two help texts and the README's row name
//! the commit where they name the copy after a tool call.

#![allow(clippy::unwrap_used)]

use ham_core::saved_copy::before_end_request;
use ham_core::settings::Key;

const LOOK: &str = "A look saves a copy of the work to the stage's bucket when a file changed or the engine made a commit, and";

fn readme() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn guide() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/user-guide.md");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn b904_a_run_without_a_copy_is_explained_by_no_file_change_and_no_commit() {
    assert_eq!(
        before_end_request(None, "01RUN", false),
        "No copy of the work had been saved before that request: a run saves a copy only after a file changed or the engine made a \
         commit."
    );
}

#[test]
fn b904_both_copy_interval_keys_say_that_a_look_saves_after_a_file_change_or_a_commit() {
    for key in [Key::RunCopyEverySeconds, Key::RunCopyEveryMinutes] {
        let help = key.row().help;
        assert!(help.contains(LOOK), "{help}");
        assert!(!help.contains("when files changed"), "{help}");
    }
}

#[test]
fn b904_the_readme_row_of_the_copy_interval_says_what_the_key_says() {
    let text = readme();
    let row = text
        .lines()
        .find(|l| l.starts_with("| `run.copy_every_seconds` |"))
        .unwrap_or_else(|| panic!("README.md has no row of run.copy_every_seconds"));
    assert!(
        row.contains("A look saves a copy of the work to the stage's bucket when a file changed or the engine made a commit, and `ham pull"),
        "{row}"
    );
    assert!(!row.contains("when files changed"), "{row}");
}

#[test]
fn b904_the_guide_quotes_the_note_of_a_run_without_a_copy_as_the_client_says_it() {
    let said = before_end_request(None, "01RUN", false);
    let text = guide();
    assert!(
        text.contains(&format!("With no saved copy, it says \"{said}\"")),
        "docs/user-guide.md does not quote the client's sentence: {said}"
    );
    assert!(!text.contains("a run saves a copy only after its files change"));
}

/// What both help texts say of the copy after a tool call.
const AFTER_A_TOOL_CALL: &str = "the copy after a tool call that changed a file or made a commit, made while the copies are 8 MiB or smaller";

#[test]
fn b983_both_copy_interval_keys_name_the_commit_in_the_copy_after_a_tool_call() {
    for key in [Key::RunCopyEverySeconds, Key::RunCopyEveryMinutes] {
        let help = key.row().help;
        assert!(help.contains(AFTER_A_TOOL_CALL), "{help}");
        assert!(!help.contains("a tool call that changed a file,"), "{help}");
    }
}

#[test]
fn b983_the_readme_row_of_the_copy_interval_names_the_commit_in_the_copy_after_a_tool_call() {
    let text = readme();
    let row = text
        .lines()
        .find(|l| l.starts_with("| `run.copy_every_seconds` |"))
        .unwrap_or_else(|| panic!("README.md has no row of run.copy_every_seconds"));
    assert!(
        row.contains("The copy after a tool call that changed a file or made a commit, made while the copies are small, and the copy when the engine stops do not move with this key."),
        "{row}"
    );
    assert!(!row.contains("a tool call that changed a file,"), "{row}");
}
