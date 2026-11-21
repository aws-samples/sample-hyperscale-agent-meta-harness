//! The answer that asks for an update: the contract of a feature that an older `ham` cannot handle.
//!
//! The service answers such a client with HTTP 409, one sentence that names `ham update`, and a `code` that ends in `-needs-update`. This
//! file holds the two rules a feature's own test calls on its code and its sentence (`ham_core::update::asks_for_update`,
//! `ham_core::update::needs_update_sentence_ok`).

use ham_core::update::{NEEDS_UPDATE_SUFFIX, asks_for_update, needs_update_sentence_ok};

/// The sentence written for the first such answer, a copy of the work that an older `ham` cannot download: what cannot be done, that
/// the work is saved, the command, "ask again".
const SENTENCE: &str = "this copy of the work needs a newer ham to download; the work is saved; run ham update and ask again";

#[test]
fn b513_a_code_asks_for_update_by_its_ending() {
    assert_eq!(NEEDS_UPDATE_SUFFIX, "-needs-update");
    assert!(asks_for_update("snapshot-needs-update"));
    assert!(asks_for_update("x-needs-update"));
    // The ending alone names no feature, and the ending must be the end.
    for code in [
        "-needs-update",
        "needs-update",
        "stage-updating",
        "snapshot-updating",
        "snapshot-needs-update-x",
        "snapshot-needs-updates",
        "snapshot-needs-update ",
        "",
    ] {
        assert!(!asks_for_update(code), "{code:?}");
    }
}

#[test]
fn b513_the_sentence_of_such_an_answer_names_the_command_on_one_line() {
    assert!(needs_update_sentence_ok(SENTENCE), "{SENTENCE}");
    // Without the command the person does not know the remedy.
    assert!(!needs_update_sentence_ok(
        "this copy of the work needs a newer ham to download; the work is saved; update ham and ask again"
    ));
    // One line: a released client prints the sentence between `HTTP 409: ` and the code.
    for broken in [
        "this copy needs a newer ham;\nrun ham update and ask again",
        "this copy needs a newer ham;\r run ham update and ask again",
        "this copy needs a newer ham;\u{2028}run ham update and ask again",
        "this copy needs a newer ham;\u{1b}[2J run ham update and ask again",
        "this copy needs a newer ham;\trun ham update and ask again",
    ] {
        assert!(!needs_update_sentence_ok(broken), "{broken:?}");
    }
    // At most 240 characters, counted as characters.
    let tail = "; run ham update and ask again";
    let fits = format!("{}{tail}", "a".repeat(240 - tail.len()));
    assert_eq!(fits.chars().count(), 240);
    assert!(needs_update_sentence_ok(&fits));
    let long = format!("{}{tail}", "a".repeat(241 - tail.len()));
    assert_eq!(long.chars().count(), 241);
    assert!(!needs_update_sentence_ok(&long));
    let wide = format!("{}{tail}", "\u{e9}".repeat(240 - tail.len()));
    assert_eq!(wide.chars().count(), 240);
    assert!(
        needs_update_sentence_ok(&wide),
        "240 characters of two bytes each fit"
    );
}
