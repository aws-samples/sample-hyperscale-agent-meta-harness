//! The test-first order of the lifecycle pack as the client carries it.
//!
//! The client embeds the three texts of the contract and the seven skills; `ham rules init` writes them. These tests read the embedded texts, so they show what a run is sent:
//! the failing test is run with the repository's own test command before any other edit (the red run), the red record's three lines come from that one run, a run made after
//! the code change is not a red run, the test and its record are committed alone before the code, and a red record is added only with its test. The Python tests of the same
//! issues (`scripts/tests/test_b476_*.py` to `test_b480_*.py`) read the files of the repository; these read what the binary holds.
#![allow(clippy::unwrap_used)]

use ham_core::config::LifecycleFlow;
use ham_core::pack;

fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Step 4 of a text that holds the eight steps, flattened: from `4. Test first` to the start of step 5.
fn step4(flow: LifecycleFlow) -> String {
    let text = flat(pack::contract_text(flow));
    let from = text.find("4. Test first").unwrap();
    let to = text[from..].find(" 5. Build and test").unwrap();
    text[from..from + to].to_string()
}

/// Q2 of a text that holds the quick path, flattened: from `Q2 ` to the start of Q3.
fn q2(flow: LifecycleFlow) -> String {
    let text = flat(pack::contract_text(flow));
    let from = text.find("Q2 ").unwrap();
    let to = text[from..].find(" Q3 ").unwrap();
    text[from..from + to].to_string()
}

fn tdd_skill() -> &'static str {
    pack::files()
        .iter()
        .find(|f| f.path == ".agents/skills/lifecycle-tdd/SKILL.md")
        .unwrap()
        .content
}

/// Step `n` of the lifecycle-tdd skill, flattened: from the line that starts `n. ` to the next numbered line.
fn skill_step(n: u32) -> String {
    let lines: Vec<&str> = tdd_skill().lines().collect();
    let starts = |l: &str, k: u32| l.starts_with(&format!("{k}. "));
    let from = lines.iter().position(|l| starts(l, n)).unwrap();
    let to = lines[from + 1..]
        .iter()
        .position(|l| (1..=20).any(|k| starts(l, k)))
        .map_or(lines.len(), |p| from + 1 + p);
    flat(&lines[from..to].join("\n"))
}

fn idx(hay: &str, needle: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not found in: {hay}"))
}

const STEPS: [LifecycleFlow; 2] = [LifecycleFlow::Full, LifecycleFlow::Auto];
const QUICK: [LifecycleFlow; 2] = [LifecycleFlow::Quick, LifecycleFlow::Auto];
const RUN_IT: &str =
    "before any other edit, run it with the repository's own test command (such as pytest)";

#[test]
fn b476_the_client_texts_run_the_test_with_the_repositorys_own_command_before_any_other_edit() {
    for flow in STEPS {
        let s = step4(flow);
        assert!(s.contains(RUN_IT), "{flow:?}: {s}");
        assert!(s.contains("This is the red run."), "{flow:?}: {s}");
        assert!(idx(&s, "(a) write one failing test") < idx(&s, RUN_IT));
        assert!(idx(&s, RUN_IT) < idx(&s, "Save .ham/lifecycle/red/"));
    }
    for flow in QUICK {
        let q = q2(flow);
        assert!(q.contains("write a test first"), "{flow:?}: {q}");
        assert!(
            q.contains("Before any other edit, run it with the repository's own test command (such as pytest)"),
            "{flow:?}: {q}"
        );
        assert!(!q.to_lowercase().contains("red record"), "{flow:?}: {q}");
    }
    let s = skill_step(3);
    assert!(s.contains("before any other edit"), "{s}");
    assert!(
        s.contains("the repository's own test command (for example pytest)"),
        "{s}"
    );
    assert!(s.contains("This is the red run."), "{s}");
    for flow in STEPS {
        let s = step4(flow);
        assert!(
            s.contains("(a) write one failing test (with the helper or fixture file it needs);"),
            "{flow:?}: {s}"
        );
    }
}

#[test]
fn b477_the_three_lines_of_the_red_record_come_from_that_one_run() {
    for flow in STEPS {
        let s = step4(flow);
        assert!(
            s.contains("three lines copied from that one run"),
            "{flow:?}: {s}"
        );
        let marks = [
            "(a) write one failing test",
            "before any other edit, run it",
            "Save .ham/lifecycle/red/NN-slug.txt",
            "(c) commit the test",
            "(d) write the smallest code",
        ];
        let places: Vec<usize> = marks.iter().map(|m| idx(&s, m)).collect();
        assert!(
            places.windows(2).all(|w| w[0] < w[1]),
            "{flow:?}: {places:?}"
        );
    }
    assert!(skill_step(4).contains("three lines copied from that one run"));
    let body = tdd_skill();
    assert!(idx(body, "3. Run it now") < idx(body, "4. Save that run"));
}

#[test]
fn b478_a_run_made_after_the_code_change_is_not_a_red_run() {
    for flow in STEPS {
        let s = step4(flow);
        assert!(
            s.contains("This is the red run. A run made after the code change is not a red run."),
            "{flow:?}: {s}"
        );
        assert!(!s.contains("(a run made after"), "{flow:?}: {s}");
    }
    for flow in QUICK {
        assert!(
            q2(flow).contains("a run made after the fix does not count"),
            "{flow:?}"
        );
    }
    assert!(skill_step(3).contains(
        "A run made after you changed code for this behavior is not a red run, whatever it shows."
    ));
}

#[test]
fn b478_a_session_that_changed_code_first_makes_its_red_run_in_a_clean_worktree() {
    // Skill step 9: the late run is made on the original code in a clean worktree of the base commit with only the test files copied in; the session's own files are
    // never restored, stashed or moved, and a record for a run that was not made is still refused.
    let s = skill_step(9);
    for want in [
        "say so in your last message",
        "a clean worktree of the base commit outside the repository (git worktree add)",
        "copy only the test files into it",
        "commit just the test files and the record in your own tree, before the code",
        "If you cannot, write no red record.",
        "Never write a red record for a run you did not make.",
    ] {
        assert!(s.contains(want), "`{want}` not in: {s}");
    }
    for word in ["git restore", "stash", "checkout --"] {
        assert!(!s.contains(word), "`{word}` in: {s}");
    }
}

#[test]
fn b479_the_test_and_its_red_record_are_committed_alone_and_a_record_committed_with_code_does_not_count()
 {
    for flow in STEPS {
        let s = step4(flow);
        assert!(
            s.contains("(c) commit the test and that file alone, with no code"),
            "{flow:?}: {s}"
        );
        assert!(
            s.contains("committed with code does not count"),
            "{flow:?}: {s}"
        );
        assert!(
            idx(&s, "(c) commit the test and that file alone")
                < idx(&s, "(d) write the smallest code")
        );
    }
    let s = skill_step(5);
    assert!(
        s.contains("Commit the test and the red record alone"),
        "{s}"
    );
    assert!(s.contains("before any code"), "{s}");
    assert!(
        s.contains("A red record committed with code does not count"),
        "{s}"
    );
}

#[test]
fn b480_a_red_record_is_added_only_with_its_test_is_said_once_where_the_record_is_described() {
    for flow in STEPS {
        let s = step4(flow);
        assert_eq!(
            s.to_lowercase()
                .matches("a red record is added only with its test")
                .count(),
            1,
            "{flow:?}: {s}"
        );
        assert!(
            idx(&s, "\"failure: <the failing line>\"")
                < idx(&s, "A red record is added only with its test")
                && idx(&s, "A red record is added only with its test")
                    < idx(&s, "(c) commit the test"),
            "{flow:?}: {s}"
        );
    }
    assert!(skill_step(4).contains("A red record is added only with its test."));
    assert!(
        !skill_step(5)
            .to_lowercase()
            .contains("a red record is added only with its test")
    );
}
