//! The pure parts of the texts that name a run's engine. The marker of the follow-up box is made by one function that the writer
//! and the reader share; the confirmations of the follow-up and of the retry name the engine they are given; the refusal of a rules file
//! named AGENTS.md names the engine a person's next run is on. With no engine every text is the default engine's, word for word the one
//! of release 5.34.
#![allow(clippy::unwrap_used)]

use ham_core::followup::{self, AddRefused};
use ham_core::retry;
use ham_core::rules::{Problem, RuleError, validate_rule_name};

const RUN: &str = "01K6A0000000000000000000R1";
const NONCE: &str = "n7Qk2ZxV9bLm4RtY";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const ENGINES: [(Option<&str>, &str); 3] = [
    (None, "Cline"),
    (Some("cline"), "Cline"),
    (Some("opencode"), "OpenCode"),
];

#[test]
fn b981_the_marker_is_one_text_for_the_writer_and_the_reader() {
    for (engine, name) in ENGINES {
        let marker = followup::marker(engine, NONCE);
        assert_eq!(marker, format!("written by {name}, untrusted [{NONCE}]"));
        // The writer: the opening line of the block is the marker between the dashes of the fence.
        let block = followup::cline_block_for(engine, "the message", NONCE);
        assert_eq!(
            block,
            format!(
                "--- {marker} ---\nthe message\n--- end of text written by {name} [{NONCE}] ---"
            )
        );
        // The reader: a box that holds the block is not given a second one.
        let once =
            followup::add_cline_message_for(engine, "my text", "the message", NONCE).unwrap();
        assert_eq!(once, format!("my text\n\n{block}"));
        assert_eq!(
            followup::add_cline_message_for(engine, &once, "the message", NONCE),
            Err(AddRefused::Already)
        );
    }
    // The forms without an engine are the default engine's, in the words of release 5.34.
    assert_eq!(
        followup::cline_block("m", "n1"),
        "--- written by Cline, untrusted [n1] ---\nm\n--- end of text written by Cline [n1] ---"
    );
    assert_eq!(
        followup::add_cline_message("t", "m", "n1"),
        followup::add_cline_message_for(None, "t", "m", "n1")
    );
}

/// The message stands in the block as the engine wrote it: only the two lines of the fence carry the engine's name.
#[test]
fn b981_the_message_in_the_block_is_not_rewritten() {
    let block = followup::cline_block_for(Some("opencode"), "set {engine} in the template", NONCE);
    assert!(
        block.contains("\nset {engine} in the template\n"),
        "{block}"
    );
}

#[test]
fn b981_the_follow_up_confirmation_names_the_engine_it_is_given() {
    let cline = followup::confirmation(RUN, Some(HEAD), Some("s3-only"));
    assert!(
        cline.ends_with(
            "\nThe new Cline receives exactly the text in the box: edit it first if you like."
        ),
        "{cline}"
    );
    for engine in [None, Some("cline")] {
        assert_eq!(
            followup::confirmation_for(engine, RUN, Some(HEAD), Some("s3-only")),
            cline
        );
    }
    let opencode = followup::confirmation_for(Some("opencode"), RUN, Some(HEAD), Some("s3-only"));
    assert_eq!(
        opencode,
        cline.replace("The new Cline receives", "The new OpenCode receives")
    );
    assert!(!opencode.contains("Cline"), "{opencode}");
}

#[test]
fn b981_the_retry_confirmation_names_the_engine_it_is_given() {
    let cline = retry::confirmation(RUN, Some("zip only"), true);
    assert!(
        cline.contains("\nThe new Cline receives the request only, not this run's history.\n"),
        "{cline}"
    );
    for engine in [None, Some("cline")] {
        assert_eq!(
            retry::confirmation_for(engine, RUN, Some("zip only"), true, None),
            cline
        );
    }
    let opencode = retry::confirmation_for(Some("opencode"), RUN, Some("zip only"), true, None);
    assert_eq!(
        opencode,
        cline.replace("The new Cline receives", "The new OpenCode receives")
    );
    assert!(!opencode.contains("Cline"), "{opencode}");
}

#[test]
fn b981_the_refusal_of_a_rule_named_agents_md_names_the_engine_it_is_given() {
    const CLINE: &str = "cannot be called AGENTS.md: Cline would load it as a repository instruction file; rename it";
    const OPENCODE: &str = "cannot be called AGENTS.md: OpenCode would load it as a repository instruction file; rename it";
    let problem = validate_rule_name("AGENTS.md").unwrap_err();
    assert_eq!(problem, Problem::ReservedName);
    assert_eq!(problem.to_string(), CLINE);
    assert_eq!(problem.words_for(None), CLINE);
    assert_eq!(problem.words_for(Some("cline")), CLINE);
    assert_eq!(problem.words_for(Some("opencode")), OPENCODE);
    let error = RuleError::new("AGENTS.md", problem);
    assert_eq!(error.to_string(), format!("rule \"AGENTS.md\" {CLINE}"));
    assert_eq!(error.words_for(None), error.to_string());
    assert_eq!(
        error.words_for(Some("opencode")),
        format!("rule \"AGENTS.md\" {OPENCODE}")
    );
    // Every other problem reads as before, whatever the engine.
    let other = RuleError::new("a b.md", Problem::BadName);
    assert_eq!(other.words_for(Some("opencode")), other.to_string());
}

/// A view with no event of a session names no engine, and reads as the default engine where a name is needed.
#[test]
fn b981_a_view_with_no_session_names_no_engine() {
    let view = ham_core::state::RunState::new(RUN).view();
    assert_eq!(ham_core::engines::named_in(&view), None);
    assert_eq!(
        ham_core::engines::of_view(&view).id,
        ham_core::engines::DEFAULT_ENGINE_ID
    );
}
