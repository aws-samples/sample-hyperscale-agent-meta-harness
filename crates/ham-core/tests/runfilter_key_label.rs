//! The words the `p` key carries on the Runs screen's key line, and the position of the selected run in the shown list. The key
//! limits the list to the repository of the selected run, or lifts that limit. Its label says which of the two a press does and, when
//! the line has room, which repository it would pick. [`RunsFilter::scope_key_labels`] returns the wordings from the longest to the
//! shortest; the screen takes the first that fits the width it has left. All of it is pure.

#![allow(clippy::unwrap_used)]

use ham_core::runfilter::{RunRow, RunsFilter, position_text};
use harness_protocol::api::v1::RunSummary;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::1:assumed-role/A/b";

fn run(id: &str, target: Value) -> RunSummary {
    serde_json::from_value(
        json!({"runId": id, "status": "complete", "phase": "planning", "target": target,
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN}),
    )
    .unwrap()
}

fn repo_row(name: &str) -> RunRow {
    RunRow::from_summary(&run(
        "01M5A00000000000000000R001",
        json!({"mode": "repo", "repo": format!("cc:{name}")}),
    ))
}

fn s3_row() -> RunRow {
    RunRow::from_summary(&run(
        "01M5A00000000000000000R002",
        json!({"mode": "s3-only"}),
    ))
}

#[test]
fn a_repository_run_offers_its_repository_then_this_repo_then_repo() {
    let f = RunsFilter::new();
    assert_eq!(
        f.scope_key_labels(Some(&repo_row("calc"))),
        ["p only cc:calc", "p this repo", "p repo"]
    );
}

#[test]
fn a_run_with_no_repository_names_s3_only_and_never_says_repo() {
    let f = RunsFilter::new();
    assert_eq!(
        f.scope_key_labels(Some(&s3_row())),
        ["p only s3-only", "p s3-only"]
    );
}

#[test]
fn with_no_selected_run_the_label_is_the_shortest_one() {
    let f = RunsFilter::new();
    assert_eq!(f.scope_key_labels(None), ["p repo"]);
}

#[test]
fn once_the_limit_is_on_the_label_says_how_to_lift_it() {
    let mut f = RunsFilter::new();
    let r = repo_row("calc");
    f.toggle_scope(Some(&r));
    assert_eq!(
        f.scope_key_labels(Some(&r)),
        ["p show all runs", "p all runs", "p all"]
    );
    f.toggle_scope(Some(&r));
    assert_eq!(f.scope_key_labels(Some(&r))[0], "p only cc:calc");
}

#[test]
fn a_long_repository_name_is_cut_with_an_ellipsis_so_the_label_stays_bounded() {
    let f = RunsFilter::new();
    let long = repo_row("a-very-long-repository-name-that-keeps-going-and-going");
    let labels = f.scope_key_labels(Some(&long));
    assert!(labels[0].starts_with("p only cc:a-very-long"), "{labels:?}");
    assert!(labels[0].ends_with('\u{2026}'), "{labels:?}");
    assert!(
        labels[0].chars().count() <= "p only ".len() + 24,
        "{labels:?}"
    );
    assert_eq!(labels[1], "p this repo");
}

/// The first wording for a run on the repository `name` (the project is `cc:<name>`, and that is the text the label cuts).
fn first_label(name: &str) -> String {
    RunsFilter::new()
        .scope_key_labels(Some(&repo_row(name)))
        .remove(0)
}

/// The first `n` characters of the letters and digits, repeated when `n` is longer than they are.
fn name_of(n: usize) -> String {
    "abcdefghijklmnopqrstuvwxyz0123456789"
        .chars()
        .cycle()
        .take(n)
        .collect()
}

#[test]
fn a_project_of_exactly_24_characters_is_spelled_out_whole() {
    // `cc:` and 21 more characters: 24 in all, the most the label spells out.
    let name = name_of(21);
    assert_eq!(first_label(&name), format!("p only cc:{name}"));
    // One character less is whole as well.
    let name = name_of(20);
    assert_eq!(first_label(&name), format!("p only cc:{name}"));
}

#[test]
fn a_project_of_25_characters_is_cut_to_23_characters_and_an_ellipsis() {
    let name = name_of(22);
    let project = format!("cc:{name}");
    assert_eq!(project.chars().count(), 25);
    let label = first_label(&name);
    let cut: String = project.chars().take(23).collect();
    assert_eq!(label, format!("p only {cut}\u{2026}"));
    // The cut name is 24 characters in all, the ellipsis counted.
    assert_eq!(label.chars().count(), "p only ".len() + 24, "{label:?}");
}

#[test]
fn every_name_length_around_the_limit_cuts_when_it_is_over_and_only_then() {
    for n in 1..=40usize {
        let name = name_of(n);
        let project = format!("cc:{name}");
        let label = first_label(&name);
        let spelled = label.strip_prefix("p only ").unwrap();
        if project.chars().count() <= 24 {
            assert_eq!(spelled, project, "name of {n} characters");
        } else {
            assert_eq!(spelled.chars().count(), 24, "name of {n} characters");
            assert!(spelled.ends_with('\u{2026}'), "name of {n} characters");
            assert!(
                project.starts_with(spelled.trim_end_matches('\u{2026}')),
                "name of {n} characters"
            );
        }
    }
}

/// A row whose project is exactly `project`. A repository name is ASCII only (`RepoTarget` accepts letters, digits, `.`, `_` and `-`), so
/// no run of the service gives a project with wide characters; the field is public, and the label code must not assume it.
fn row_with_project(project: &str) -> RunRow {
    let mut row = repo_row("calc");
    row.project = project.to_string();
    row
}

#[test]
fn a_project_text_of_wide_characters_is_cut_by_characters_not_by_columns() {
    let wide = |n: usize| "\u{5b57}".repeat(n);
    let first = |project: &str| {
        RunsFilter::new()
            .scope_key_labels(Some(&row_with_project(project)))
            .remove(0)
    };
    // `cc:` and 12 wide characters are 15 characters: whole, though the label is 34 columns wide.
    assert_eq!(
        first(&format!("cc:{}", wide(12))),
        format!("p only cc:{}", wide(12))
    );
    // `cc:` and 21 are exactly 24 characters (45 columns): whole as well.
    assert_eq!(
        first(&format!("cc:{}", wide(21))),
        format!("p only cc:{}", wide(21))
    );
    // `cc:` and 22 are 25 characters: `cc:`, 20 wide characters and the ellipsis.
    assert_eq!(
        first(&format!("cc:{}", wide(22))),
        format!("p only cc:{}\u{2026}", wide(20))
    );
    // 30 are cut the same way, and the shorter wordings are the same as for any repository.
    let labels =
        RunsFilter::new().scope_key_labels(Some(&row_with_project(&format!("cc:{}", wide(30)))));
    assert_eq!(
        labels,
        [
            format!("p only cc:{}\u{2026}", wide(20)),
            "p this repo".to_string(),
            "p repo".to_string()
        ]
    );
}

#[test]
fn the_labels_are_ordered_from_the_longest_and_the_last_is_the_shortest() {
    let f = RunsFilter::new();
    for r in [repo_row("calc"), s3_row()] {
        let labels = f.scope_key_labels(Some(&r));
        let widths: Vec<usize> = labels.iter().map(|l| l.chars().count()).collect();
        let mut sorted = widths.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(widths, sorted, "{labels:?}");
        assert!(labels.iter().all(|l| l.starts_with("p ")), "{labels:?}");
    }
}

#[test]
fn the_position_reads_run_n_of_the_number_shown() {
    assert_eq!(position_text(12, 769), "run 12 of 769");
    assert_eq!(position_text(1, 1), "run 1 of 1");
}
