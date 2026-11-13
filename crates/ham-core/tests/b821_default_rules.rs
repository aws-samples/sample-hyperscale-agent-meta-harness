//! The rules every run gets tell the engine, before it starts, which paths a delivery refuses. A run with the zip target ended with
//! no zip after 87 minutes because the engine added `.github/workflows/ci.yml` and nothing had told it that a delivery could refuse the
//! file. The default rules now have a section `## Delivery`: a delivery to a repository refuses a change under `.github/workflows/` and
//! to a root `buildspec.yml`, a run that delivers only a zip takes them, and no delivery takes a change to the engine's own executable
//! configuration.
#![allow(clippy::unwrap_used)]

use ham_core::rules::DEFAULT_RULES;

/// The lines of the section `heading`, up to the next `## ` heading.
fn section(heading: &str) -> String {
    let mut lines = DEFAULT_RULES
        .lines()
        .skip_while(|l| l.trim() != heading)
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[test]
fn b821_the_default_rules_say_which_paths_a_repository_delivery_refuses_and_that_a_zip_takes_them()
{
    let delivery = section("## Delivery");
    assert!(
        !delivery.is_empty(),
        "the default rules have a section ## Delivery"
    );
    for needle in [
        ".github/workflows/",
        "buildspec.yml",
        "repository",
        "pull request",
        "delivers nothing",
        "only a zip",
        "last message",
    ] {
        assert!(delivery.contains(needle), "{needle:?} in:\n{delivery}");
    }
    // The engine is not told that a user's wish lifts the rule: a repository run that made the change would lose its delivery.
    assert!(!delivery.contains("asks for it"), "{delivery}");
}

#[test]
fn b821_the_default_rules_name_the_engine_configuration_that_no_delivery_takes() {
    let delivery = section("## Delivery");
    for path in [
        ".cline/hooks/",
        ".cline/plugins/",
        ".cline/agents/",
        ".clinerules/hooks/",
        "opencode.json",
        "opencode.jsonc",
        ".opencode/",
    ] {
        assert!(
            delivery.contains(&format!("`{path}`")),
            "{path} in:\n{delivery}"
        );
    }
    assert!(delivery.contains("Every delivery refuses"), "{delivery}");
}

#[test]
fn b821_the_delivery_section_is_two_short_rules() {
    let delivery = section("## Delivery");
    let bullets: Vec<&str> = delivery.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(bullets.len(), 2, "{delivery}");
    assert_eq!(
        delivery.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "each rule is one line, like every rule of the file:\n{delivery}"
    );
    for b in bullets {
        assert!(
            b.chars().count() <= 420,
            "{} characters: {b}",
            b.chars().count()
        );
    }
}
