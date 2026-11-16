//! The delivery rule of the default rules says where the engine learns which kind of run it is in. The rule names both cases: a
//! delivery to a repository (a pull request) refuses a run that adds or changes a file under `.github/workflows/` or a root
//! `buildspec.yml`, and a run that delivers only a zip takes such files. An engine in a zip run read the rule, could not tell which case
//! was its own, and held a workflow file back that its prompt asked for. The harness now says it at the start of every session
//! (`crates/agentd/src/direct/delivery_word.rs`), and the rule ends with the sentence that points there. The sentence is conditional: the
//! rules file comes from the client and the words come from the stage, and a stage that has not been updated sends none.
//!
//! The rule, the worker's words and the bundle validator are each tested against one list of the two paths
//! (`harness_protocol::direct::REPOSITORY_REFUSES`); the phrases below are the rule's own. The worker's block says the same of a zip. Of a
//! pull request it says what the validator does with every kind of change ("adds, changes or removes"), where the rule says "adds or
//! changes".
#![allow(clippy::unwrap_used)]

use ham_core::rules::DEFAULT_RULES;
use harness_protocol::direct::REPOSITORY_REFUSES;

/// The first rule of the section `## Delivery`: the one about a repository and a zip.
fn rule() -> &'static str {
    DEFAULT_RULES
        .lines()
        .skip_while(|l| l.trim() != "## Delivery")
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .find(|l| l.starts_with("- A delivery to a repository"))
        .expect("the default rules have the rule about a delivery to a repository")
}

#[test]
fn b847_the_rule_says_that_the_first_words_of_the_run_say_which_kind_of_delivery_it_is() {
    let r = rule();
    assert!(
        r.ends_with(
            " When the first words of the run are written by the harness, they say which of the two this run is."
        ),
        "{r}"
    );
}

/// A newer client on a stage that has not been updated sends this rule into a run whose first words are the user's prompt alone. The rule
/// therefore promises the harness's words to no run: it says what they mean when the run has them.
#[test]
fn b847_the_rule_does_not_promise_the_harness_words_to_a_run_that_has_none() {
    let r = rule();
    assert!(
        !r.contains("The first words of the run, written by the harness, say"),
        "the sentence that holds for every run: {r}"
    );
    assert!(
        r.contains(" When the first words of the run are written by the harness, they say"),
        "{r}"
    );
}

#[test]
fn b847_the_rule_keeps_both_cases_in_its_own_words() {
    let r = rule();
    for phrase in [
        "A delivery to a repository (a pull request) refuses a run that adds or changes a file under",
        "the run ends blocked and delivers nothing",
        "A run that delivers only a zip takes such files",
        "name each one you add in your last message",
    ] {
        assert!(r.contains(phrase), "{phrase:?} in: {r}");
    }
}

#[test]
fn b847_the_rule_names_the_two_paths_of_the_one_list() {
    let r = rule();
    for path in REPOSITORY_REFUSES {
        assert!(r.contains(&format!("`{path}`")), "{path} in: {r}");
    }
    assert!(r.contains("a root `buildspec.yml`"), "{r}");
}
