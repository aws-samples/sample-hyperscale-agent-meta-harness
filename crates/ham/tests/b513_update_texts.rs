//! The texts of the client that tell a person that this `ham` or the stage is too old: each names the command `ham update`.
//!
//! A text about a stage that is too old keeps its direction ("update the stage") and adds, in brackets, the command and who can run it:
//! `ham update` updates a stage with the caller's own AWS identity, so a person who uses a stage that someone else runs cannot update it.
//! The sentence for a model the stage's list lacks (`tests/b862_older_backend.rs`) has the same form, `Update the stage (run ham update)`.

#![allow(clippy::unwrap_used)]

use ham::api::{
    ApiError, TAGS_UNSUPPORTED, predates_copy_interval_words, predates_delivery_note_words,
    predates_engine_choice_words, predates_recover_after_stop_words,
    predates_remove_earlier_records_words, predates_short_copy_interval_words, predates_words,
    tags_failure,
};
use ham_core::engine_file;
use ham_core::settings::Key;
use harness_protocol::api::ApiError as WireError;

/// How a text about a stage that is too old names the command and the person who can run it.
const STAGE: &str = "(run ham update; ask the person who runs the stage if that is not you)";

#[test]
fn b513_every_text_of_a_stage_that_predates_a_setting_ends_with_ham_update_and_who_can_run_it() {
    assert_eq!(
        predates_delivery_note_words(),
        format!(
            "this stage predates the switch of the delivery note, so it cannot start a run with run.delivery_note = false; run `ham config unset run.delivery_note` or update the stage {STAGE}"
        )
    );
    assert_eq!(
        predates_recover_after_stop_words(),
        format!(
            "this stage predates the switch of reading a stopped session's storage back, so it cannot start a run with run.recover_after_stop = false; run `ham config unset run.recover_after_stop` or update the stage {STAGE}"
        )
    );
    // The switch for the earlier run's records came with the same release as this rule and ends the same way.
    assert_eq!(
        predates_remove_earlier_records_words(),
        format!(
            "this stage predates the switch for the earlier run's records, so it leaves them in place anyway; run `ham config unset run.remove_earlier_records` or update the stage {STAGE}"
        )
    );
    assert_eq!(
        predates_short_copy_interval_words(30),
        format!(
            "this stage predates copy intervals under one minute, so it cannot start a run with run.copy_every_seconds = 30; set it to 60 or more, run `ham config unset run.copy_every_seconds` or update the stage {STAGE}"
        )
    );
    assert_eq!(
        predates_engine_choice_words("opencode"),
        format!(
            "this stage predates the choice of an engine, so it cannot run on opencode; run without --engine (or set default_engine to the default) or update the stage {STAGE}"
        )
    );
}

#[test]
fn b513_the_three_forms_of_the_copy_interval_text_end_the_same_way() {
    assert_eq!(
        predates_copy_interval_words(600, Some(Key::RunCopyEverySeconds)),
        format!(
            "this stage predates the copy interval of a run, so it cannot start a run with run.copy_every_seconds = 600; run `ham config unset run.copy_every_seconds` or update the stage {STAGE}"
        )
    );
    assert_eq!(
        predates_copy_interval_words(600, Some(Key::RunCopyEveryMinutes)),
        format!(
            "this stage predates the copy interval of a run, so it cannot start a run with run.copy_every_minutes = 10; run `ham config unset run.copy_every_minutes` or update the stage {STAGE}"
        )
    );
    assert_eq!(
        predates_copy_interval_words(600, None),
        format!(
            "this stage predates the copy interval of a run, so it cannot start a run with run.copy_every_minutes = 10 (or run.copy_every_seconds = 600, whichever your config sets); run `ham config unset run.copy_every_minutes` (or `ham config unset run.copy_every_seconds`) or update the stage {STAGE}"
        )
    );
    assert_eq!(
        predates_copy_interval_words(45, None),
        format!(
            "this stage predates the copy interval of a run, so it cannot start a run with run.copy_every_seconds = 45; run `ham config unset run.copy_every_seconds` or update the stage {STAGE}"
        )
    );
}

#[test]
fn b513_the_text_of_a_stage_that_predates_engine_settings_keeps_its_keys_last() {
    let cfg = engine_file::read_text(
        "schema = 1\n[reasoning]\neffort = \"low\"\n[tools]\ndisable = [\"spawn_agent\"]\n",
        "cline",
    )
    .config
    .unwrap();
    assert_eq!(
        predates_words(&cfg),
        format!(
            "this stage predates engine settings; run `ham engine cline unset <key>` or update the stage {STAGE} (keys set: reasoning.effort, tools.disable)"
        )
    );
}

#[test]
fn b513_a_stage_without_tag_edits_says_what_it_needs() {
    let unsupported = format!("this stage cannot edit tags yet: it needs a newer backend {STAGE}");
    assert_eq!(TAGS_UNSUPPORTED, unsupported);
    let no_route = ApiError::Http {
        status: 404,
        error: WireError {
            code: "not-found".into(),
            message: "no such route".into(),
            request_id: None,
        },
    };
    assert_eq!(tags_failure(&no_route), unsupported);
}

#[test]
fn b513_the_note_of_two_model_lists_that_differ_names_ham_update_for_both_ways() {
    let mut list = ham_core::models::builtin().list();
    let local = list.default.clone();
    list.default = "us.anthropic.claude-opus-5-5".into();
    assert_ne!(local, list.default);
    assert_eq!(
        ham::config_cmd::default_drift(&list).unwrap(),
        format!(
            "note: the server's default model is us.anthropic.claude-opus-5-5 and this ham's is {local}; the lists differ, so update ham or the stage {STAGE}"
        )
    );
}
