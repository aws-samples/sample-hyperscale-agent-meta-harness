//! The Runs screen's filter. Words typed in the find-or-command box narrow the list on the id, the status, the repository
//! and, once the run index exists, the prompt title and the model; a gutter marks runs that are running, that need the user, or that
//! failed; a scope toggle limits the list to the repository of the focused run; the timer reads only the newest page and `r` reads all
//! of them; a run that starts needing the user is found by comparing two lists. All of it is pure: the screen draws what it returns.

#![allow(clippy::unwrap_used)]

use ham_core::runfilter::{
    FIRST_PAGE, Gutter, IndexEntry, Pages, Refresh, RunRow, RunsFilter, RunsRead, Scope,
    merge_first_page, newly_needing_you, select_after,
};
use harness_protocol::api::v1::RunSummary;
use proptest::prelude::*;
use serde_json::{Value, json};

const ARN: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";

fn run(id: &str, status: &str, target: Value, created: &str, extra: Value) -> RunSummary {
    let mut v = json!({"runId": id, "status": status, "phase": "planning", "target": target,
        "createdAt": created, "ownerPrincipalArn": ARN});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::from_value(v).unwrap()
}

fn repo(name: &str) -> Value {
    json!({"mode": "repo", "repo": format!("cc:{name}")})
}

fn plain(id: &str, status: &str, name: &str) -> RunSummary {
    run(id, status, repo(name), "2026-10-16T08:00:00Z", json!({}))
}

fn row(id: &str, status: &str, name: &str) -> RunRow {
    RunRow::from_summary(&plain(id, status, name))
}

fn needs_input() -> Value {
    json!({"needsInput": {"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1N", "reason": "engine_stop",
        "title": "Cline stopped", "expiresAt": "2026-10-16T09:00:00Z"}})
}

fn aborted() -> Value {
    json!({"aborted": {"status": "FAILED", "stoppedAt": "2026-10-16T08:30:00Z"}})
}

// ---- the row ------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_row_carries_the_words_the_screen_shows() {
    let r = RunRow::from_summary(&run(
        "01M5287J0000000000000000AB",
        "active",
        repo("calc-service"),
        "2026-10-16T08:00:00Z",
        json!({}),
    ));
    assert_eq!(r.run_id, "01M5287J0000000000000000AB");
    assert_eq!(r.status, "active");
    assert_eq!(r.target, "cc:calc-service");
    assert_eq!(r.project, "cc:calc-service");
    assert_eq!(r.created_at, "2026-10-16T08:00:00Z");
    assert_eq!(r.title, None);
    assert_eq!(r.model, None);

    let new = RunRow::from_summary(&run(
        "A",
        "complete",
        json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261016"}),
        "2026-10-16T08:00:00Z",
        json!({}),
    ));
    assert_eq!(new.target, "new cc:hamh-e2e-new-20261016");
    assert_eq!(
        new.project, "cc:hamh-e2e-new-20261016",
        "a new repository is the same project as later runs on it"
    );

    let s3 = RunRow::from_summary(&run(
        "B",
        "active",
        json!({"mode": "s3-only"}),
        "2026-10-16T08:00:00Z",
        json!({}),
    ));
    assert_eq!(s3.target, "s3-only");
    assert_eq!(s3.project, "s3-only");

    let limited = RunRow::from_summary(&plain("C", "usage_limited", "x"));
    assert_eq!(
        limited.status, "usage limited",
        "an underscore reads as a space"
    );
}

#[test]
fn the_index_adds_a_title_and_a_model() {
    let r = row("A", "active", "calc").with_index(
        Some("Build a todo API"),
        Some("Claude Sonnet 5.5"),
        None,
        None,
        None,
    );
    assert_eq!(r.title.as_deref(), Some("Build a todo API"));
    assert_eq!(r.model.as_deref(), Some("Claude Sonnet 5.5"));
}

#[test]
fn index_text_is_cleaned_of_controls_and_escape_sequences() {
    let r = row("A", "active", "calc").with_index(
        Some("Build\u{1b}[2J a\ntodo\u{7} API"),
        Some("m\u{1b}]0;title\u{7}odel"),
        None,
        None,
        None,
    );
    assert_eq!(r.title.as_deref(), Some("Build a todo API"));
    assert_eq!(r.model.as_deref(), Some("model"));
}

const AI_NAME: &str = "Make uploads idempotent across retries now";

#[test]
fn with_index_carries_name_and_source() {
    let r = row("A", "active", "calc").with_index(
        Some("Build a todo API"),
        Some("Claude Sonnet 5.5"),
        Some(AI_NAME),
        Some("ai"),
        None,
    );
    assert_eq!(r.name.as_deref(), Some(AI_NAME));
    assert_eq!(r.name_source.as_deref(), Some("ai"));
    // The title and the model are unchanged by the name.
    assert_eq!(r.title.as_deref(), Some("Build a todo API"));
    assert_eq!(r.model.as_deref(), Some("Claude Sonnet 5.5"));
    // A seed carries its source too.
    let seed =
        row("A", "active", "calc").with_index(None, None, Some("Fix the bug"), Some("seed"), None);
    assert_eq!(seed.name.as_deref(), Some("Fix the bug"));
    assert_eq!(seed.name_source.as_deref(), Some("seed"));
    // Without a name the row has neither member, and a source with no name is dropped.
    let bare = row("A", "active", "calc").with_index(Some("x"), None, None, Some("ai"), None);
    assert_eq!(
        (bare.name.as_deref(), bare.name_source.as_deref()),
        (None, None)
    );
    assert_eq!(row("A", "active", "calc").name, None);
    assert_eq!(row("A", "active", "calc").name_source, None);
}

#[test]
fn holds_matches_the_name() {
    let named = row("A", "active", "calc").with_index(
        Some("Fix the uploader"),
        None,
        Some(AI_NAME),
        Some("ai"),
        None,
    );
    let unnamed =
        row("A", "active", "calc").with_index(Some("Fix the uploader"), None, None, None, None);
    let mut f = RunsFilter::new();
    f.set_text("idempotent");
    assert!(f.matches(&named));
    assert!(!f.matches(&unnamed), "the word is only in the name");
    f.set_text("IDEMPOTENT retries");
    assert!(f.matches(&named), "every word, any case, any order");
    f.set_text("idempotent kubernetes");
    assert!(!f.matches(&named), "every word must be found");
    // The title still finds the run, and the highlight marks the drawn characters of the name.
    f.set_text("uploader");
    assert!(f.matches(&named) && f.matches(&unnamed));
    f.set_text("idempotent");
    let at = AI_NAME.find("idempotent").unwrap();
    assert_eq!(f.highlight(AI_NAME), vec![at..at + "idempotent".len()]);
}

#[test]
fn label_is_name_else_title() {
    let both = row("A", "active", "calc").with_index(
        Some("Fix the uploader"),
        None,
        Some(AI_NAME),
        Some("ai"),
        None,
    );
    assert_eq!(both.label(), Some(AI_NAME));
    let title_only =
        row("A", "active", "calc").with_index(Some("Fix the uploader"), None, None, None, None);
    assert_eq!(title_only.label(), Some("Fix the uploader"));
    let neither = row("A", "active", "calc");
    assert_eq!(neither.label(), None);
    let name_only =
        row("A", "active", "calc").with_index(None, None, Some(AI_NAME), Some("ai"), None);
    assert_eq!(name_only.label(), Some(AI_NAME));
}

#[test]
fn an_escape_byte_in_a_name_is_cleaned() {
    let r = row("A", "active", "calc").with_index(
        None,
        None,
        Some("Fix\u{1b}[31m login\u{1b}[0m\u{7} bug\u{1b}]0;x\u{7} now please"),
        Some("ai\u{7}"),
        None,
    );
    assert_eq!(r.name.as_deref(), Some("Fix login bug now please"));
    assert_eq!(r.name_source.as_deref(), Some("ai"));
    let name = r.name.unwrap();
    assert!(name.chars().all(|c| !c.is_control()), "{name:?}");
    // A name of nothing but controls is no name at all.
    let none =
        row("A", "active", "calc").with_index(None, None, Some("\u{1b}[2J\u{7}"), Some("ai"), None);
    assert_eq!(
        (none.name.as_deref(), none.name_source.as_deref()),
        (None, None)
    );
}

#[test]
fn the_gutter_marks_running_needing_you_and_failed() {
    let g = |s: RunSummary| RunRow::from_summary(&s).gutter;
    assert_eq!(g(plain("A", "active", "x")), Gutter::Running);
    assert_eq!(
        g(run(
            "B",
            "active",
            repo("x"),
            "2026-10-16T08:00:00Z",
            needs_input()
        )),
        Gutter::NeedsYou,
        "waiting for an answer beats running"
    );
    assert_eq!(
        g(run(
            "C",
            "cleared",
            repo("x"),
            "2026-10-16T08:00:00Z",
            aborted()
        )),
        Gutter::Failed
    );
    assert_eq!(g(plain("D", "blocked", "x")), Gutter::Failed);
    for quiet in ["complete", "usage_limited", "budget_limited", "cleared"] {
        assert_eq!(g(plain("E", quiet, "x")), Gutter::None, "{quiet}");
    }
    // A finished run does not need anyone, whatever an old tail read said.
    assert_eq!(
        g(run(
            "F",
            "complete",
            repo("x"),
            "2026-10-16T08:00:00Z",
            needs_input()
        )),
        Gutter::None
    );
    // An aborted run that is still marked needing you is a failure.
    assert_eq!(
        g(run(
            "G",
            "active",
            repo("x"),
            "2026-10-16T08:00:00Z",
            json!({"aborted": {"status": "ABORTED", "stoppedAt": "z"}, "needsInput": needs_input()["needsInput"]})
        )),
        Gutter::Failed
    );
}

#[test]
fn each_gutter_has_a_glyph_and_a_word() {
    assert_eq!(Gutter::Running.word(), "running");
    assert_eq!(Gutter::NeedsYou.word(), "needs you");
    assert_eq!(Gutter::Failed.word(), "failed");
    assert_eq!(Gutter::None.word(), "");
    let glyphs = [
        Gutter::Running,
        Gutter::NeedsYou,
        Gutter::Failed,
        Gutter::None,
    ]
    .map(Gutter::glyph);
    // paused and stalled have their own marks
    assert_eq!(
        (Gutter::Paused.glyph(), Gutter::Stalled.glyph()),
        ("=", "~")
    );
    for g in glyphs {
        assert_eq!(g.chars().count(), 1);
    }
    assert_eq!(
        glyphs
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4,
        "four different marks"
    );
}

// ---- the filter ---------------------------------------------------------------------------------------------------------------------

/// 300 runs: seven on a repository that holds "calc", three whose title holds it (once an index exists), the rest on other names.
fn three_hundred() -> Vec<RunRow> {
    let mut rows = Vec::new();
    for i in 0..300 {
        let name = match i % 43 {
            0 => format!("calc-service-{i}"),
            _ => format!("service-{i}"),
        };
        let mut r = row(
            &format!("01M{i:023}"),
            if i % 7 == 0 { "complete" } else { "active" },
            &name,
        );
        if [11, 99, 250].contains(&i) {
            r = r.with_index(
                Some("Fix the CALC rounding bug"),
                Some("Claude Opus 5.5"),
                None,
                None,
                None,
            );
        }
        rows.push(r);
    }
    rows
}

#[test]
fn typing_calc_shows_only_runs_whose_target_or_title_holds_it() {
    let rows = three_hundred();
    let mut f = RunsFilter::new();
    assert_eq!(f.visible(&rows).len(), 300, "no words: every run");
    f.set_text("calc");
    let seen = f.visible(&rows);
    // i % 43 == 0 for 0, 43, 86, 129, 172, 215, 258 (seven) plus the three titles.
    assert_eq!(seen.len(), 10);
    for i in &seen {
        let r = &rows[*i];
        assert!(
            r.target.contains("calc")
                || r.title
                    .as_deref()
                    .is_some_and(|t| t.to_lowercase().contains("calc")),
            "{r:?}"
        );
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    assert_eq!(seen, sorted, "the list keeps its order");
}

#[test]
fn every_word_must_match_and_case_does_not_matter() {
    let rows = vec![
        row("01AAA", "active", "calc-service"),
        row("01BBB", "complete", "calc-service"),
        row("01CCC", "active", "other"),
    ];
    let mut f = RunsFilter::new();
    f.set_text("CALC  active");
    assert_eq!(f.visible(&rows), vec![0]);
    f.set_text("calc complete");
    assert_eq!(f.visible(&rows), vec![1]);
    f.set_text("zzz");
    assert!(f.visible(&rows).is_empty());
    f.set_text("   ");
    assert_eq!(f.visible(&rows).len(), 3, "blanks are no filter");
    assert!(!f.is_filtering());
}

#[test]
fn a_word_may_match_the_id_the_status_the_model_or_a_state_word() {
    let waiting = RunRow::from_summary(&run(
        "01WWW",
        "active",
        repo("x"),
        "2026-10-16T08:00:00Z",
        needs_input(),
    ));
    let failed = RunRow::from_summary(&run(
        "01FFF",
        "cleared",
        repo("y"),
        "2026-10-16T08:00:00Z",
        aborted(),
    ));
    let indexed =
        row("01III", "complete", "z").with_index(None, Some("GPT-6.1 Sol"), None, None, None);
    let rows = vec![waiting, failed, indexed];
    let mut f = RunsFilter::new();
    for (q, want) in [
        ("01www", vec![0]),
        ("needs", vec![0]),
        ("you", vec![0]),
        ("failed", vec![1]),
        ("cleared", vec![1]),
        ("sol", vec![2]),
        ("gpt-6.1", vec![2]),
        ("complete", vec![2]),
    ] {
        f.set_text(q);
        assert_eq!(f.visible(&rows), want, "{q}");
    }
}

#[test]
fn a_command_attempt_does_not_filter() {
    let rows = vec![row("A", "active", "calc"), row("B", "active", "other")];
    let mut f = RunsFilter::new();
    for text in [
        "/cancel",
        "/runs",
        "ham push",
        "ham runs --json",
        "/send hello",
        "//x",
        "/",
        "ham \"build\" --s3-only",
    ] {
        f.set_text(text);
        assert!(!f.is_filtering(), "{text}");
        assert_eq!(f.visible(&rows).len(), 2, "{text}");
    }
    // Chat by the grammar: these are plain words, so they filter.
    for text in ["Ham push", " /cancel", "ham", "/Cancel", "/ x", "calc"] {
        f.set_text(text);
        assert!(f.is_filtering(), "{text}");
    }
    f.set_text("calc");
    assert_eq!(f.visible(&rows), vec![0]);
}

#[test]
fn the_scope_toggle_limits_the_list_to_the_focused_runs_repository() {
    let rows = vec![
        row("A", "active", "calc"),
        row("B", "active", "other"),
        row("C", "complete", "calc"),
        RunRow::from_summary(&run(
            "D",
            "active",
            json!({"mode": "s3-only"}),
            "2026-10-16T08:00:00Z",
            json!({}),
        )),
        RunRow::from_summary(&run(
            "E",
            "active",
            json!({"mode": "new-repo", "repo": "cc:calc"}),
            "2026-10-16T08:00:00Z",
            json!({}),
        )),
    ];
    let mut f = RunsFilter::new();
    assert_eq!(*f.scope(), Scope::All);
    f.toggle_scope(Some(&rows[0]));
    assert_eq!(*f.scope(), Scope::Project("cc:calc".into()));
    assert_eq!(
        f.visible(&rows),
        vec![0, 2, 4],
        "a new repository joins the project"
    );
    // Words and scope together.
    f.set_text("complete");
    assert_eq!(f.visible(&rows), vec![2]);
    f.set_text("");
    // A second toggle goes back to all, whatever is focused.
    f.toggle_scope(Some(&rows[1]));
    assert_eq!(*f.scope(), Scope::All);
    assert_eq!(f.visible(&rows).len(), 5);
    // S3-only runs are a project too.
    f.toggle_scope(Some(&rows[3]));
    assert_eq!(f.visible(&rows), vec![3]);
    // Turning it off needs no focused run; turning it on needs one.
    f.toggle_scope(None);
    assert_eq!(*f.scope(), Scope::All);
    f.toggle_scope(None);
    assert_eq!(*f.scope(), Scope::All);
}

#[test]
fn highlights_are_the_character_ranges_of_the_words_in_a_cell() {
    let mut f = RunsFilter::new();
    f.set_text("calc ser");
    let r = row("01M5287J", "active", "calc-service");
    assert_eq!(f.highlight(&r.target), vec![3..7, 8..11]);
    assert!(f.highlight(&r.run_id).is_empty());
    f.set_text("CAL");
    assert_eq!(f.highlight("xx Calc"), vec![3..6]);
    f.set_text("");
    assert!(f.highlight("calc").is_empty());
    // Overlapping and touching words give merged ranges.
    f.set_text("calc alc");
    assert_eq!(f.highlight("calc"), vec![0..4]);
    f.set_text("ca lc");
    assert_eq!(f.highlight("calc"), vec![0..4]);
}

#[test]
fn the_summary_line_says_what_is_shown() {
    let mut f = RunsFilter::new();
    assert_eq!(f.summary(300, 300), "300 runs");
    f.set_text("calc");
    assert_eq!(f.summary(10, 300), "10 of 300 runs match \"calc\"");
    assert_eq!(f.summary(0, 300), "no run matches \"calc\" (300 runs)");
    f.set_text("a   b");
    assert_eq!(
        f.summary(2, 5),
        "2 of 5 runs match \"a b\"",
        "words are joined by one space"
    );
    f.set_text("calc");
    let rows = [row("A", "active", "calc")];
    f.set_text("");
    f.toggle_scope(Some(&rows[0]));
    assert_eq!(f.summary(4, 300), "4 of 300 runs in cc:calc");
    f.set_text("x");
    assert_eq!(f.summary(1, 300), "1 of 300 runs in cc:calc match \"x\"");
    assert_eq!(f.summary(1, 1), "1 of 1 run in cc:calc match \"x\"");
}

#[test]
fn the_selection_stays_on_its_run_when_the_filter_changes() {
    let rows = three_hundred();
    let mut f = RunsFilter::new();
    f.set_text("calc");
    let vis = f.visible(&rows);
    // The run at screen position 3 was selected; the list narrows and it is still there, now at another position.
    let id = rows[vis[3]].run_id.clone();
    f.set_text("calc-service");
    let narrower = f.visible(&rows);
    let at = select_after(&rows, &narrower, Some(id.as_str()), 3);
    assert_eq!(rows[narrower[at.unwrap()]].run_id, id);
    // The selected run is filtered out: the position clamps into the new list.
    f.set_text("rounding");
    let titles = f.visible(&rows);
    assert_eq!(titles.len(), 3);
    assert_eq!(select_after(&rows, &titles, Some(id.as_str()), 7), Some(2));
    assert_eq!(select_after(&rows, &titles, None, 1), Some(1));
    // Nothing visible: nothing selected.
    f.set_text("zzz");
    assert_eq!(
        select_after(&rows, &f.visible(&rows), Some(id.as_str()), 0),
        None
    );
}

proptest! {
    #[test]
    fn any_text_and_any_rows_never_panic_and_visible_is_an_ordered_subset(
        text in "\\PC{0,12}",
        names in proptest::collection::vec("[a-z-]{1,8}", 0..12),
    ) {
        let rows: Vec<RunRow> = names.iter().enumerate().map(|(i, n)| row(&format!("01{i:02}"), "active", n)).collect();
        let mut f = RunsFilter::new();
        f.set_text(&text);
        let v = f.visible(&rows);
        prop_assert!(v.windows(2).all(|w| w[0] < w[1]));
        prop_assert!(v.iter().all(|i| *i < rows.len()));
        for r in &rows {
            let _ = f.highlight(&r.target);
        }
        let _ = f.summary(v.len(), rows.len());
    }
}

// ---- refresh ------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_timer_reads_the_newest_page_only_and_r_reads_all_of_them() {
    assert_eq!(Refresh::Timer.pages(), Pages::First);
    assert_eq!(Refresh::Manual.pages(), Pages::All);
    assert_eq!(FIRST_PAGE, 100);
}

fn listing(n: usize, status: &str) -> Vec<RunSummary> {
    // Newest first: run 0 is the newest.
    (0..n)
        .map(|i| {
            run(
                &format!("01M{i:023}"),
                status,
                repo("calc"),
                &format!(
                    "2026-10-16T{:02}:{:02}:00Z",
                    23 - (i / 60) % 24,
                    59 - i % 60
                ),
                json!({}),
            )
        })
        .collect()
}

#[test]
fn a_refreshed_first_page_replaces_the_newest_rows_and_keeps_the_older_ones() {
    let old = listing(250, "active");
    // Two brand new runs on top, then the newest ninety-eight old ones, now complete.
    let mut page: Vec<RunSummary> = (0..2)
        .map(|i| {
            run(
                &format!("01N{i:023}"),
                "active",
                repo("fresh"),
                &format!("2026-10-17T00:0{i}:00Z"),
                json!({}),
            )
        })
        .rev()
        .collect();
    page.extend(listing(250, "complete").into_iter().take(98));
    assert_eq!(page.len(), FIRST_PAGE);
    let merged = merge_first_page(&old, page, true);
    assert_eq!(merged.len(), 252);
    assert!(merged[..2].iter().all(|r| r.run_id.starts_with("01N")));
    assert!(
        merged[2..100]
            .iter()
            .all(|r| r.status == ham_status("complete"))
    );
    assert!(
        merged[100..]
            .iter()
            .all(|r| r.status == ham_status("active")),
        "older rows are kept as read"
    );
    let ids: std::collections::HashSet<_> = merged.iter().map(|r| &r.run_id).collect();
    assert_eq!(ids.len(), merged.len(), "no run twice");
}

fn ham_status(s: &str) -> harness_protocol::goal::GoalStatus {
    serde_json::from_value(json!(s)).unwrap()
}

#[test]
fn a_first_page_with_no_next_token_is_the_whole_list() {
    let old = listing(250, "active");
    let page = listing(40, "complete");
    let merged = merge_first_page(&old, page, false);
    assert_eq!(
        merged.len(),
        40,
        "the server said there is nothing after it"
    );
}

#[test]
fn a_short_page_with_more_behind_it_is_merged_by_its_edge() {
    // The server skips a run item it cannot parse, so a page can hold fewer than a hundred rows and still have a next page.
    let old = listing(250, "active");
    let page: Vec<RunSummary> = listing(250, "complete").into_iter().take(97).collect();
    let merged = merge_first_page(&old, page, true);
    assert_eq!(merged.len(), 250);
    assert!(
        merged[..97]
            .iter()
            .all(|r| r.status == ham_status("complete"))
    );
    assert!(
        merged[97..]
            .iter()
            .all(|r| r.status == ham_status("active"))
    );
}

#[test]
fn a_row_that_left_the_newest_page_is_dropped_and_the_older_rows_are_kept() {
    let old = listing(130, "active");
    // The newest page lacks old run 5 (it is gone) and ends on the timestamp of old run 99.
    let mut page = listing(130, "complete");
    page.remove(5);
    page.truncate(FIRST_PAGE);
    let edge = page.last().unwrap().created_at.clone();
    let merged = merge_first_page(&old, page, true);
    assert!(!merged.iter().any(|r| r.run_id == old[5].run_id));
    // Every old run at or older than the edge, that the page does not hold, is still there.
    for r in old.iter().filter(|r| r.created_at <= edge) {
        assert!(merged.iter().any(|m| m.run_id == r.run_id), "{}", r.run_id);
    }
}

#[test]
fn a_run_that_starts_needing_you_is_found_by_comparing_two_lists() {
    let before = vec![
        plain("A", "active", "x"),
        plain("B", "active", "x"),
        run(
            "C",
            "active",
            repo("x"),
            "2026-10-16T08:00:00Z",
            needs_input(),
        ),
    ];
    let after = vec![
        run(
            "A",
            "active",
            repo("x"),
            "2026-10-16T08:00:00Z",
            needs_input(),
        ),
        plain("B", "active", "x"),
        run(
            "C",
            "active",
            repo("x"),
            "2026-10-16T08:00:00Z",
            needs_input(),
        ),
        run(
            "D",
            "active",
            repo("x"),
            "2026-10-16T08:00:00Z",
            needs_input(),
        ),
    ];
    assert_eq!(
        newly_needing_you(&before, &after),
        vec!["A".to_string(), "D".to_string()]
    );
    assert!(newly_needing_you(&after, &after).is_empty());
    assert!(
        newly_needing_you(&after, &before).is_empty(),
        "answered runs are not new"
    );
    // The first list ever read has no 'before': every run that waits counts.
    assert_eq!(newly_needing_you(&[], &after).len(), 3);
}

#[test]
fn the_router_names_command_attempts_by_the_grammar_of_decision_0178() {
    use ham_core::router::is_command_attempt;
    for yes in [
        "/cancel",
        "/send hi",
        "//x",
        "/",
        "ham push",
        "ham\tpush",
        "ham -x",
        "ham \"p\" --s3-only",
    ] {
        assert!(is_command_attempt(yes), "{yes:?}");
    }
    for no in [
        "", "calc", "Ham push", "ham", "ham  ", " /cancel", "/Cancel", "/ x", "/1x", "hampush",
        "ham Push",
    ] {
        assert!(!is_command_attempt(no), "{no:?}");
    }
}

// ---- found by the second-opinion review ----------------------------------------------

#[test]
fn running_is_a_word_for_the_runs_the_gutter_marks_running() {
    let rows = vec![row("A", "active", "x"), row("B", "complete", "y")];
    assert_eq!(rows[0].gutter, Gutter::Running);
    let mut f = RunsFilter::new();
    f.set_text("running");
    assert_eq!(f.visible(&rows), vec![0]);
}

#[test]
fn a_run_that_waited_while_finished_and_waits_again_counts_as_new() {
    // The held list carried a stale question on a finished run; the new list has the run active and waiting.
    let before = vec![run(
        "A",
        "complete",
        repo("x"),
        "2026-10-16T08:00:00Z",
        needs_input(),
    )];
    let after = vec![run(
        "A",
        "active",
        repo("x"),
        "2026-10-16T08:00:00Z",
        needs_input(),
    )];
    assert_eq!(newly_needing_you(&before, &after), vec!["A".to_string()]);
    // And a finished run that still carries one is never new.
    assert!(newly_needing_you(&after, &before).is_empty());
}

#[test]
fn created_times_are_compared_as_times_not_as_text() {
    let at = |id: &str, t: &str| run(id, "active", repo("x"), t, json!({}));
    let mut page: Vec<RunSummary> = (0..FIRST_PAGE - 1)
        .map(|i| {
            at(
                &format!("P{i:03}"),
                &format!("2026-10-16T09:{:02}:{:02}Z", 59 - i / 60, 59 - i % 60),
            )
        })
        .collect();
    // The page ends on a time with a fraction; a held run of the same second without one is not older than the edge by a letter.
    page.push(at("EDGE", "2026-10-16T08:00:00.500Z"));
    let old = vec![
        at("SAME", "2026-10-16T08:00:00Z"),
        at("NEWER", "2026-10-16T08:00:01Z"),
        at("OLDER", "2026-10-16T07:59:59Z"),
    ];
    let merged = merge_first_page(&old, page, true);
    let ids: Vec<&str> = merged.iter().map(|r| r.run_id.as_str()).collect();
    assert!(ids.contains(&"SAME"), "the same second is kept");
    assert!(ids.contains(&"OLDER"));
    assert!(
        !ids.contains(&"NEWER"),
        "newer than the edge and not on the page: gone"
    );
}

#[test]
fn a_read_is_laid_over_the_list_held_when_it_is_applied() {
    let at = |id: &str, m: u32| {
        run(
            id,
            "active",
            repo("x"),
            &format!("2026-10-16T08:{m:02}:00Z"),
            json!({}),
        )
    };
    // A timer read of the newest page, and a longer list that a manual read delivered meanwhile.
    let newest = vec![at("R4", 4), at("R3", 3)];
    let longer = vec![at("R3", 3), at("R2", 2), at("R1", 1), at("R0", 0)];
    let timer = RunsRead::First {
        runs: newest.clone(),
        has_more: true,
    };
    let ids = |v: &[RunSummary]| v.iter().map(|r| r.run_id.clone()).collect::<Vec<_>>();
    assert_eq!(
        ids(&timer.apply(&longer)),
        ["R4", "R3", "R2", "R1", "R0"],
        "laid over the newer list, not over an older copy"
    );
    let manual = RunsRead::All(longer.clone());
    assert_eq!(ids(&manual.apply(&newest)), ids(&longer));
    let whole = RunsRead::First {
        runs: newest,
        has_more: false,
    };
    assert_eq!(ids(&whole.apply(&longer)), ["R4", "R3"]);
}

// ---- run tags ----------------------------------------------------------------------------------------------
// The list row carries the run's effective tags. The index entry holds the server's list; a row with no list shows the project its
// repository gives, and a list the server sent is authoritative (a tag the person removed is not given back). Picked tags narrow the
// list like words and the scope do.

fn tags(list: &[&str]) -> Vec<String> {
    list.iter().map(|t| (*t).to_string()).collect()
}

fn with_tags(r: RunRow, list: &[&str]) -> RunRow {
    r.with_index(None, None, None, None, Some(tags(list).as_slice()))
}

#[test]
fn with_index_carries_tags_and_falls_back_to_the_derived_project() {
    // A row nobody indexed shows the project of its repository.
    let r = row("A", "active", "calc");
    assert_eq!(r.tags, ["project:calc"]);
    // No list from the server (an older backend): the same derived project, however the index fills the other members.
    let none = r.clone().with_index(Some("Fix it"), None, None, None, None);
    assert_eq!(none.tags, ["project:calc"]);
    let new = RunRow::from_summary(&run(
        "N",
        "active",
        json!({"mode": "new-repo", "repo": "cc:fresh"}),
        "2026-10-16T08:00:00Z",
        json!({}),
    ));
    assert_eq!(
        new.tags,
        ["project:fresh"],
        "a new repository is a project too"
    );
    let s3 = RunRow::from_summary(&run(
        "S",
        "active",
        json!({"mode": "s3-only"}),
        "2026-10-16T08:00:00Z",
        json!({}),
    ));
    assert!(s3.tags.is_empty(), "s3-only is not a project");
    // A list from the server replaces the fallback: no derived project is added to it.
    let some = with_tags(r.clone(), &["topic:csv-export", "tech:python"]);
    assert_eq!(some.tags, ["topic:csv-export", "tech:python"]);
    // An empty list is an answer: the person removed everything, the project included. No tag comes back.
    let removed = with_tags(r.clone(), &[]);
    assert!(removed.tags.is_empty(), "{:?}", removed.tags);
}

#[test]
fn the_client_checks_the_tags_the_server_sent() {
    let esc = format!("topic:{}x", '\u{1b}');
    let sent = [
        esc.as_str(),
        "topic:UPPER",
        "project:calc",
        "tech:aws-s3",
        "tech:s3",
        "topic:verified-login",
        "topic:ok",
    ];
    let r = with_tags(row("A", "active", "x"), &sent);
    // A tag with an escape byte, one that breaks the grammar or the policy, and a second spelling of a tag already there are not kept.
    assert_eq!(r.tags, ["project:calc", "tech:aws-s3", "topic:ok"]);
    assert!(r.tags.iter().all(|t| !t.contains('\u{1b}')));
    // At most 19 tags, as the server sends.
    let many: Vec<String> = (0..40).map(|n| format!("topic:t{n}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(with_tags(row("B", "active", "x"), &many).tags.len(), 19);
}

proptest! {
    #[test]
    fn what_the_server_sends_the_client_keeps(
        auto in proptest::collection::vec("(topic|tech):[a-z][a-z0-9]{0,5}(-[a-z][a-z0-9]{0,5}){0,2}", 0..8),
        user in proptest::collection::vec("(topic|tech|project):[a-z][a-z0-9]{0,5}(-[a-z][a-z0-9]{0,5}){0,2}", 0..8),
        off in proptest::collection::vec("(topic|tech|project):[a-z][a-z0-9]{0,5}(-[a-z][a-z0-9]{0,5}){0,2}", 0..8),
        repository in prop_oneof![Just("s3-only"), Just("cc:calc"), Just("new cc:fresh")],
    ) {
        let server = ham_core::runtag::effective::effective(repository, &auto, &user, &off).tags;
        let client = RunRow::from_summary(&plain("A", "active", "calc"))
            .with_index(None, None, None, None, Some(server.as_slice()));
        prop_assert_eq!(client.tags, server);
    }
}

#[test]
fn a_word_of_a_tag_is_found_by_the_filter() {
    let plain_row = row("A", "active", "calc");
    let tagged = with_tags(
        row("B", "active", "calc"),
        &["topic:csv-export", "tech:python"],
    );
    let rows = vec![plain_row, tagged];
    let mut f = RunsFilter::new();
    f.set_text("csv");
    assert_eq!(
        f.visible(&rows),
        [1],
        "only the run whose tag holds the word"
    );
    f.set_text("CSV-EXPORT python");
    assert_eq!(f.visible(&rows), [1], "every word, any case");
    f.set_text("project:calc");
    assert_eq!(
        f.visible(&rows),
        [0],
        "the derived project is a tag too; the other run's list replaced it"
    );
    // A removed tag is not found: the server's list has no derived project, so the word of it finds nothing.
    let gone = with_tags(row("C", "active", "calc"), &["tech:python"]);
    f.set_text("project:calc");
    assert!(f.visible(&[gone]).is_empty());
}

#[test]
fn picked_tags_are_and_and_clear_with_the_other_filters() {
    let a = with_tags(row("A", "active", "calc"), &["topic:a", "topic:b"]);
    let b = with_tags(row("B", "active", "calc"), &["topic:a"]);
    let c = with_tags(row("C", "complete", "web"), &["topic:b"]);
    let rows = vec![a, b, c.clone()];
    let mut f = RunsFilter::new();
    assert!(!f.is_narrowed());
    f.set_tags(tags(&["topic:a"]));
    assert_eq!(f.tags(), ["topic:a"]);
    assert!(f.is_narrowed(), "a pick narrows the list");
    assert!(!f.is_filtering(), "words only: a pick is not a word");
    assert_eq!(f.visible(&rows), [0, 1]);
    // Picks are AND.
    f.set_tags(tags(&["topic:a", "topic:b"]));
    assert_eq!(f.visible(&rows), [0]);
    f.set_tags(tags(&["topic:b"]));
    assert_eq!(f.visible(&rows), [0, 2]);
    // A pick is an exact tag: a part of it, or another case, is a different tag.
    f.set_tags(tags(&["topic"]));
    assert!(f.visible(&rows).is_empty());
    f.set_tags(tags(&["TOPIC:B"]));
    assert!(f.visible(&rows).is_empty());
    // Words, the scope and the picks all apply.
    f.set_tags(tags(&["topic:b"]));
    f.set_text("calc");
    assert_eq!(f.visible(&rows), [0], "calc finds A and B, topic:b keeps A");
    f.set_text("");
    f.toggle_scope(Some(&c));
    assert_eq!(f.visible(&rows), [2], "the web project and topic:b");
    f.set_tags(tags(&["topic:a"]));
    assert!(
        f.visible(&rows).is_empty(),
        "no run of the web project holds topic:a"
    );
    // Clearing empties all three.
    f.set_text("calc");
    f.clear();
    assert!(!f.is_narrowed());
    assert_eq!((f.text(), f.tags().len()), ("", 0));
    assert_eq!(f.scope(), &Scope::All);
    assert_eq!(f.visible(&rows), [0, 1, 2]);
    // The Esc of the screen builds a fresh filter: the same thing.
    let fresh = RunsFilter::new();
    assert!(!fresh.is_narrowed() && fresh.tags().is_empty());
    // A pick listed twice counts once.
    f.set_tags(tags(&["topic:a", "topic:a"]));
    assert_eq!(f.tags(), ["topic:a"]);
}

#[test]
fn the_summary_reads_the_picks_as_plus_tokens() {
    let mut f = RunsFilter::new();
    f.set_tags(tags(&["topic:a", "tech:b"]));
    assert_eq!(f.summary(12, 212), "12 of 212 runs match +topic:a +tech:b");
    f.set_tags(tags(&["tech:go"]));
    assert_eq!(f.summary(0, 212), "no run matches +tech:go (212 runs)");
    assert_eq!(f.summary(1, 1), "1 of 1 run match +tech:go");
    // With words and a scope: the scope first, the words quoted, then the picks.
    let calc = row("A", "active", "calc");
    f.toggle_scope(Some(&calc));
    f.set_text("x y");
    assert_eq!(
        f.summary(4, 300),
        "4 of 300 runs in cc:calc match \"x y\" +tech:go"
    );
    assert_eq!(
        f.summary(0, 300),
        "no run in cc:calc matches \"x y\" +tech:go (300 runs)"
    );
    // A tag drawn in the line is cleaned like any other text.
    let esc = format!("topic:{}x", '\u{1b}');
    let mut g = RunsFilter::new();
    g.set_tags(vec![esc]);
    assert!(!g.summary(1, 5).contains('\u{1b}'));
    // No pick, no word, no scope: the plain count.
    assert_eq!(RunsFilter::new().summary(300, 300), "300 runs");
}

#[test]
fn index_entry_keeps_tags_and_the_newest_page_refresh_overwrites_them() {
    let entry = |list: Option<&[&str]>| IndexEntry {
        run_id: "A".into(),
        title: Some("Fix the uploader".into()),
        model: None,
        name: None,
        name_source: None,
        tags: list.map(tags),
    };
    let built = |held: &std::collections::HashMap<String, IndexEntry>| {
        let e = held.get("A");
        RunRow::from_summary(&plain("A", "active", "calc")).with_index(
            e.and_then(|e| e.title.as_deref()),
            e.and_then(|e| e.model.as_deref()),
            e.and_then(|e| e.name.as_deref()),
            e.and_then(|e| e.name_source.as_deref()),
            e.and_then(|e| e.tags.as_deref()),
        )
    };
    let mut held = std::collections::HashMap::new();
    // The screen holds the entry of the last read of each run; a later read of the same run replaces it.
    held.insert("A".to_string(), entry(Some(&["topic:old"])));
    assert_eq!(built(&held).tags, ["topic:old"]);
    held.insert("A".to_string(), entry(Some(&["topic:new", "tech:go"])));
    assert_eq!(built(&held).tags, ["topic:new", "tech:go"]);
    // An empty list from the server is the answer, not a gap: the derived project does not come back.
    held.insert("A".to_string(), entry(Some(&[])));
    assert!(built(&held).tags.is_empty());
    // A server without tags sends none: the row shows its derived project.
    held.insert("A".to_string(), entry(None));
    assert_eq!(built(&held).tags, ["project:calc"]);
    assert_eq!(entry(Some(&["topic:a"])), entry(Some(&["topic:a"])));
    assert_ne!(entry(Some(&["topic:a"])), entry(None));
}

#[test]
fn b689_paused_and_stalled_runs_have_their_words_and_the_filter_finds_the_ones_r_resumes() {
    assert_eq!(Gutter::Paused.word(), "paused");
    assert_eq!(Gutter::Stalled.word(), "stalled");
    let stopped = |id: &str, status: &str, mode: &str| {
        RunRow::from_summary(&run(
            id,
            status,
            repo("calc"),
            "2026-11-03T08:00:00Z",
            json!({"resumeMode": mode, "statusReason": "total_time: the run used the total time it was given"}),
        ))
    };
    let paused = stopped("A", "paused", "follow_up");
    let stalled = stopped("B", "stalled", "follow_up");
    let nothing = stopped("C", "stalled", "none");
    let complete = row("D", "complete", "calc");
    assert_eq!(
        (paused.gutter, stalled.gutter),
        (Gutter::Paused, Gutter::Stalled)
    );
    let find = |text: &str| {
        let mut f = RunsFilter::new();
        f.set_text(text);
        [&paused, &stalled, &nothing, &complete]
            .map(|r| f.matches(r))
            .to_vec()
    };
    // `resumable` finds the runs the R key takes, and not the stalled run that delivered nothing
    assert_eq!(find("resumable"), [true, true, false, false]);
    assert_eq!(find("stalled"), [false, true, true, false]);
    assert_eq!(find("paused"), [true, false, false, false]);
}
