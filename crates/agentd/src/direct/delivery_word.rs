//! What the harness says to the engine about how the run delivers, at the start of every session.
//!
//! The rules every run gets say that a delivery to a repository (a pull request) refuses a run that adds or changes a file under
//! `.github/workflows/` or a root `buildspec.yml`, and that a run that delivers only a zip takes such files. Nothing told the engine which
//! of the two its run was, so an engine that was asked for a workflow file in a zip run could hold the file back. The service knows (the
//! run's target) and tells the worker in the invocation of every session (`DirectInvocationRequest.delivery`, protocol 1.61.0). This module
//! turns that member into ONE short block of the harness's own words, and the block comes before whatever the engine reads first:
//!
//! * the first prompt of a session (`direct/mod.rs`, the prompt plan): the user's text in the first session of a run, and the harness's
//!   continuation prompt ([`crate::direct::slice::first_prompt`]) in every later session;
//! * the first prompt of a fresh engine session inside a session (`direct/driver.rs`, `fresh_spec`), which starts without the earlier
//!   conversation.
//!
//! A follow-up run (`ham continue`), a resumed run (`ham resume`) and a blocked run started again are new runs with invocations of their
//! own, so their first prompt is the first case.
//!
//! The block is a fact about the run. It is no outcome label and it judges nothing: the engine still decides when the work is done. An
//! unknown delivery (no member: the invocation of a service that predates it) adds nothing. The block does not depend on the DOX starter,
//! so `DOX off` as the first line of the starter does not switch it off.
//!
//! The block has no setting and no switch of its own. What it says is set by the run's target (`--s3-only`, or a repository), and the run
//! cannot deliver in another way than the block says. The engine has to know that fact whatever the user's rules say: without it a zip
//! run held back a file its prompt asked for. How the engine works stays in the user's rules.
//!
//! The sentences say what the delivery really does, and each side has a test against the one list of paths
//! ([`harness_protocol::direct::REPOSITORY_REFUSES`]): the bundle validator (`crates/delivery/tests/b847_delivery_word.rs`), the default
//! rules (`crates/ham-core/tests/b847_default_rules.rs`) and this module. For a pull request the validator refuses every kind of change to
//! such a file, a removal and a change of type included, so the block says "adds, changes or removes"; the rule of the default rules, which
//! is the user's file, says "adds or changes".

use harness_protocol::direct::{
    DirectDelivery, DirectDeliveryKind, REPOSITORY_REFUSES, check_delivery,
};

/// The first words of the block: the harness names itself as the writer.
pub const HEADING: &str = "How this run delivers (written by the harness):";

/// The block for `delivery`. `None` for an unknown delivery, and for a member the protocol's own check refuses: such a value is never
/// written into a prompt.
pub fn block(delivery: Option<&DirectDelivery>) -> Option<String> {
    let d = delivery.filter(|d| check_delivery(d))?;
    let [workflows, buildspec] = REPOSITORY_REFUSES;
    match (d.kind, d.repo.as_deref()) {
        (DirectDeliveryKind::Zip, _) => Some(format!(
            "{HEADING} this run delivers only a zip. A zip takes files under `{workflows}` and a root `{buildspec}`, so add such a file \
             when the task asks for it, and name each one you add in your last message."
        )),
        (DirectDeliveryKind::PullRequest, Some(repo)) => Some(format!(
            "{HEADING} this run delivers a pull request to {repo}. Such a delivery refuses a run that adds, changes or removes a file \
             under `{workflows}` or a root `{buildspec}`: the run ends blocked and delivers nothing. Hold such a change back and name \
             the file in your last message."
        )),
        (DirectDeliveryKind::PullRequest, None) => None,
    }
}

/// `prompt` with the block before it and one empty line between the two; `prompt` as it is when there is no block.
pub fn with_block(delivery: Option<&DirectDelivery>, prompt: &str) -> String {
    match block(delivery) {
        Some(b) => format!("{b}\n\n{prompt}"),
        None => prompt.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_protocol::common::ENGINE_DISPLAY_NAMES;
    use harness_protocol::direct::{BANNED_CLAIM_WORDS, DirectOutcome};

    const REPO: &str = "cc:hamh-e2e-python-calc";

    fn zip() -> String {
        block(Some(&DirectDelivery::zip())).unwrap_or_default()
    }

    fn pull_request() -> String {
        block(Some(&DirectDelivery::pull_request(REPO))).unwrap_or_default()
    }

    #[test]
    fn b847_a_zip_only_run_is_told_that_a_zip_takes_the_two_paths_and_to_name_each_file() {
        let b = zip();
        assert!(b.starts_with(HEADING), "{b}");
        assert!(b.contains("this run delivers only a zip"), "{b}");
        for path in REPOSITORY_REFUSES {
            assert!(b.contains(&format!("`{path}`")), "{path} in: {b}");
        }
        assert!(b.contains("A zip takes files under"), "{b}");
        assert!(
            b.contains("name each one you add in your last message"),
            "{b}"
        );
        assert!(
            !b.contains("pull request") && !b.contains("blocked"),
            "a zip run reads nothing about the other kind: {b}"
        );
    }

    #[test]
    fn b847_a_pull_request_run_is_told_the_repository_what_the_delivery_refuses_and_to_hold_the_change_back()
     {
        let b = pull_request();
        assert!(b.starts_with(HEADING), "{b}");
        assert!(
            b.contains(&format!("this run delivers a pull request to {REPO}.")),
            "{b}"
        );
        for path in REPOSITORY_REFUSES {
            assert!(b.contains(&format!("`{path}`")), "{path} in: {b}");
        }
        // The validator refuses every kind of change to such a file (A, M, D, T: `crates/delivery/tests/b847_delivery_word.rs`), so the
        // block names a removal too.
        assert!(
            b.contains("refuses a run that adds, changes or removes a file under"),
            "{b}"
        );
        assert!(
            b.contains("the run ends blocked and delivers nothing"),
            "{b}"
        );
        assert!(
            b.contains("Hold such a change back and name the file in your last message"),
            "{b}"
        );
        assert!(!b.contains("only a zip"), "{b}");
    }

    #[test]
    fn b847_an_unknown_delivery_adds_nothing() {
        assert_eq!(block(None), None);
        assert_eq!(with_block(None, "the task"), "the task");
    }

    #[test]
    fn b847_a_member_the_protocol_check_refuses_is_never_written_into_a_prompt() {
        let no_repo = DirectDelivery {
            kind: DirectDeliveryKind::PullRequest,
            repo: None,
        };
        let two_lines = DirectDelivery::pull_request("cc:demo\nand a second line");
        let zip_with_repo = DirectDelivery {
            kind: DirectDeliveryKind::Zip,
            repo: Some(REPO.to_string()),
        };
        for bad in [no_repo, two_lines, zip_with_repo] {
            assert_eq!(block(Some(&bad)), None, "{bad:?}");
            assert_eq!(with_block(Some(&bad), "the task"), "the task", "{bad:?}");
        }
    }

    #[test]
    fn b847_the_block_comes_first_and_the_prompt_follows_byte_for_byte() {
        let task = "Add a workflow file.\n\n  Keep it small.\n";
        for d in [DirectDelivery::zip(), DirectDelivery::pull_request(REPO)] {
            let b = block(Some(&d)).unwrap_or_default();
            assert_eq!(with_block(Some(&d), task), format!("{b}\n\n{task}"));
        }
    }

    /// One short block: a single paragraph on one line, so it cannot be taken for a part of the task below it.
    #[test]
    fn b847_the_block_is_one_short_paragraph() {
        for b in [zip(), pull_request()] {
            assert!(!b.contains('\n'), "{b}");
            assert!(
                b.chars().count() <= 400,
                "{} characters: {b}",
                b.chars().count()
            );
        }
    }

    /// Root invariant 1: the block is a fact about the run. It holds no outcome label, no word that claims the harness checked the work,
    /// and it names no engine.
    #[test]
    fn b847_the_block_is_no_outcome_label_and_claims_nothing_about_the_work() {
        for b in [zip(), pull_request()] {
            let lower = b.to_lowercase();
            for word in BANNED_CLAIM_WORDS {
                assert!(!lower.contains(word), "{word} in: {b}");
            }
            for outcome in DirectOutcome::ALL {
                // The sentence of every engine the harness names: the block names no engine and holds no engine's sentence.
                for (_, shown) in ENGINE_DISPLAY_NAMES {
                    let sentence = outcome.label_text_named(shown);
                    assert!(!b.contains(&sentence), "{outcome:?} of {shown} in: {b}");
                }
                assert!(!b.contains(outcome.as_str()), "{outcome:?} in: {b}");
            }
            for word in ["reported done", "complete"] {
                assert!(!lower.contains(word), "{word} in: {b}");
            }
        }
    }
}
