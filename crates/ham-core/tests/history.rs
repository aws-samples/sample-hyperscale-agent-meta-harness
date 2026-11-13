//! The prompt history of the chat box. Up at the first row and Down at the last row step through earlier prompts: this session's,
//! then the run's earlier user messages, then the prompts of earlier runs (the first 2,000 characters; the full text is fetched when
//! one is recalled). Ctrl-R searches them. Nothing is written to disk (invariant 5), and every recalled text is tainted: it never
//! runs a command at once.

#![allow(clippy::unwrap_used)]

use ham_core::history::{
    Draft, EARLIER_PROMPT_CHARS, Earlier, HistoryRing, ReverseSearch, SESSION_MAX,
    SESSION_MAX_BYTES,
};
use proptest::prelude::*;

/// A draft no generated prompt can equal (they are lower-case letters and spaces).
const DRAFT: &str = "Draft!";

fn earlier(run: &str, text: &str) -> Earlier {
    Earlier::from_prompt(run, text)
}

/// session: first, second; the run's messages: early run message, second; earlier runs (newest first): older run prompt, oldest.
fn ring() -> HistoryRing {
    let mut r = HistoryRing::new();
    r.record("first");
    r.record("second");
    r.set_run_messages(vec!["early run message".into(), "second".into()]);
    r.set_earlier_runs(vec![
        earlier("01RUNA", "older run prompt"),
        earlier("01RUNB", "oldest"),
    ]);
    r
}

fn walk_up(r: &mut HistoryRing, current: &str) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(rec) = r.prev(current, false) {
        out.push(rec.text);
        if out.len() > 1000 {
            break;
        }
    }
    out
}

#[test]
fn up_walks_this_session_then_the_run_then_earlier_runs_newest_first_without_repeats() {
    let mut r = ring();
    assert_eq!(
        walk_up(&mut r, ""),
        [
            "second",
            "first",
            "early run message",
            "older run prompt",
            "oldest"
        ]
    );
    assert!(r.is_browsing());
    assert!(r.prev("", false).is_none(), "the oldest stays put");
}

#[test]
fn down_walks_back_and_the_draft_comes_back_at_the_bottom() {
    let mut r = ring();
    assert_eq!(r.prev("my draft", false).unwrap().text, "second");
    assert_eq!(r.prev("my draft", false).unwrap().text, "first");
    assert_eq!(r.next().unwrap().text, "second");
    let back = r.next().unwrap();
    assert_eq!(back.text, "my draft");
    assert!(
        !back.tainted,
        "the user's own words are not tainted by the trip"
    );
    assert!(!r.is_browsing());
    assert!(r.next().is_none(), "not browsing: nothing to do");
}

#[test]
fn the_first_up_skips_entries_equal_to_what_is_in_the_box() {
    let mut r = ring();
    assert_eq!(r.prev("second", false).unwrap().text, "first");
    let mut r = ring();
    assert_eq!(r.prev("something else", false).unwrap().text, "second");
}

#[test]
fn every_recall_is_tainted() {
    let mut r = ring();
    for _ in 0..5 {
        assert!(r.prev("", false).unwrap().tainted);
    }
}

#[test]
fn recording_trims_ignores_blanks_and_repeats_and_starts_browsing_over() {
    let mut r = HistoryRing::new();
    r.record("  \n\t ");
    r.record("");
    assert!(r.prev("", false).is_none(), "nothing recorded");
    r.record("build it\n\n");
    r.record("build it");
    r.record("then test");
    assert_eq!(walk_up(&mut r, ""), ["then test", "build it"]);
    // Sending while browsing ends the trip; the new prompt is the newest.
    r.record("one more");
    assert!(!r.is_browsing());
    assert_eq!(r.prev("", false).unwrap().text, "one more");
}

#[test]
fn only_the_newest_session_prompts_are_kept() {
    let mut r = HistoryRing::new();
    for i in 0..SESSION_MAX + 25 {
        r.record(&format!("prompt {i}"));
    }
    let all = walk_up(&mut r, "");
    assert_eq!(all.len(), SESSION_MAX);
    assert_eq!(all[0], format!("prompt {}", SESSION_MAX + 24));
    assert_eq!(all[SESSION_MAX - 1], "prompt 25");
}

#[test]
fn cancelling_a_trip_gives_the_draft_back() {
    let mut r = ring();
    r.prev("half typed", false);
    assert_eq!(r.cancel().map(|d| d.text).as_deref(), Some("half typed"));
    assert!(!r.is_browsing());
    assert_eq!(r.cancel(), None);
}

#[test]
fn a_prompt_of_an_earlier_run_is_cut_at_two_thousand_characters_and_fetched_in_full_when_recalled()
{
    let full = "x".repeat(EARLIER_PROMPT_CHARS + 500);
    let e = Earlier::from_prompt("01RUNC", &full);
    assert!(e.truncated);
    assert_eq!(e.text.chars().count(), EARLIER_PROMPT_CHARS);
    let short = Earlier::from_prompt("01RUND", "short");
    assert!(!short.truncated);
    assert_eq!(short.text, "short");

    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![e, short]);
    let first = r.prev("", false).unwrap();
    assert_eq!(first.text.chars().count(), EARLIER_PROMPT_CHARS);
    assert_eq!(first.fetch_full.as_deref(), Some("01RUNC"));
    assert!(first.tainted);
    let second = r.prev("", false).unwrap();
    assert_eq!(second.text, "short");
    assert_eq!(second.fetch_full, None);

    // Back on the long one; the full text arrives while it is on screen.
    r.next();
    let got = r.supply_full("01RUNC", &full).unwrap();
    assert_eq!(got.text, full);
    assert_eq!(got.fetch_full, None);
    // The ring keeps it: stepping away and back shows the full text without another fetch.
    r.prev("", false);
    let again = r.next().unwrap();
    assert_eq!(again.text, full);
    assert_eq!(again.fetch_full, None);
}

#[test]
fn a_full_text_for_another_entry_changes_the_ring_but_not_the_screen() {
    let full = "y".repeat(EARLIER_PROMPT_CHARS + 1);
    let mut r = HistoryRing::new();
    r.record("recent");
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    r.prev("", false);
    assert!(
        r.supply_full("01RUNC", &full).is_none(),
        "the screen shows 'recent'"
    );
    r.prev("", false);
    assert_eq!(r.next().unwrap().text, "recent");
    let up = r.prev("", false).unwrap();
    assert_eq!(up.text, full);
    assert_eq!(up.fetch_full, None);
    assert!(r.supply_full("01NOPE", "z").is_none());
}

// ---- Ctrl-R -------------------------------------------------------------------------------------------------------------------------

fn search_ring() -> HistoryRing {
    let mut r = HistoryRing::new();
    r.record("deploy to staging");
    r.record("write the Tests first");
    r.record("run the tests again");
    r.set_run_messages(vec!["fix the TEST data".into()]);
    r.set_earlier_runs(vec![earlier("01RUNA", "an old test prompt")]);
    r
}

fn typed(s: &mut ReverseSearch, text: &str) {
    for c in text.chars() {
        s.type_char(c);
    }
}

#[test]
fn ctrl_r_finds_the_newest_match_and_ctrl_r_again_goes_older() {
    let r = search_ring();
    let mut s = r.search("my draft", false);
    assert_eq!(s.current(), None, "an empty query matches nothing");
    typed(&mut s, "test");
    assert_eq!(s.query(), "test");
    assert_eq!(s.current(), Some("run the tests again"));
    assert_eq!(s.position(), Some((1, 4)));
    assert!(s.older());
    assert_eq!(s.current(), Some("write the Tests first"));
    assert!(s.older());
    assert_eq!(s.current(), Some("fix the TEST data"));
    assert!(s.older());
    assert_eq!(s.current(), Some("an old test prompt"));
    assert!(!s.older(), "no match older than the oldest");
    assert_eq!(s.position(), Some((4, 4)));
    assert!(s.newer());
    assert_eq!(s.current(), Some("fix the TEST data"));
}

#[test]
fn typing_narrows_and_backspace_widens_and_starts_at_the_newest_again() {
    let r = search_ring();
    let mut s = r.search("", false);
    typed(&mut s, "te");
    s.older();
    typed(&mut s, "sts");
    assert_eq!(
        s.current(),
        Some("run the tests again"),
        "a longer query starts again at the newest"
    );
    typed(&mut s, " zzz");
    assert_eq!(s.current(), None, "'tests zzz' is nowhere");
    assert_eq!(s.position(), None);
    for _ in 0..4 {
        s.backspace();
    }
    assert_eq!(s.query(), "tests");
    assert_eq!(s.current(), Some("run the tests again"));
    s.backspace();
    s.backspace();
    assert_eq!(s.query(), "tes");
    assert!(s.current().is_some());
    s.clear();
    assert_eq!(s.query(), "");
    assert_eq!(s.current(), None);
}

#[test]
fn accepting_recalls_the_match_as_tainted_text_and_cancelling_gives_the_draft_back() {
    let mut r = search_ring();
    let mut s = r.search("my draft", false);
    typed(&mut s, "staging");
    let rec = r.accept_search(s).unwrap();
    assert_eq!(rec.text, "deploy to staging");
    assert!(rec.tainted);
    assert_eq!(rec.fetch_full, None);

    let mut s = r.search("my draft", false);
    typed(&mut s, "zzz");
    assert!(r.accept_search(s).is_none(), "no match: nothing to accept");
    let s = r.search("my draft", false);
    assert_eq!(s.cancel().text, "my draft");
}

#[test]
fn a_truncated_match_asks_for_the_full_text() {
    let full = "needle ".repeat(500);
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    let mut s = r.search("", false);
    typed(&mut s, "needle");
    let rec = r.accept_search(s).unwrap();
    assert_eq!(rec.fetch_full.as_deref(), Some("01RUNC"));
}

#[test]
fn the_match_is_highlighted_where_the_query_is() {
    let r = search_ring();
    let mut s = r.search("", false);
    typed(&mut s, "TESTS");
    assert_eq!(s.current(), Some("run the tests again"));
    assert_eq!(s.highlights(), vec![8..13]);
    s.clear();
    assert!(s.highlights().is_empty());
}

#[test]
fn the_search_bar_text_names_the_query_and_the_position() {
    let r = search_ring();
    let mut s = r.search("", false);
    assert_eq!(s.bar_text(60), "history search:");
    typed(&mut s, "test");
    assert_eq!(s.bar_text(60), "history search: test  1/4");
    typed(&mut s, "zz");
    assert_eq!(s.bar_text(60), "history search: testzz  no match");
    assert_eq!(s.bar_text(0), "");
    assert!(s.bar_text(10).chars().count() <= 10);
}

#[test]
fn a_search_works_on_a_snapshot_and_does_not_disturb_the_ring() {
    let mut r = search_ring();
    let mut s = r.search("", false);
    typed(&mut s, "deploy");
    r.record("a newer prompt");
    assert_eq!(s.current(), Some("deploy to staging"));
    assert_eq!(r.prev("", false).unwrap().text, "a newer prompt");
}

// ---- no history file ----------------------------------------------------------------------------------------------------------------

#[test]
fn the_history_modules_have_no_way_to_touch_the_disk() {
    // Invariant 5: no history file. The sources of the ring and of the paste store name no file, process or output API.
    let banned: Vec<String> = [
        ["std::", "fs"].concat(),
        ["File", "::"].concat(),
        ["Open", "Options"].concat(),
        ["std::io::", "Write"].concat(),
        ["std::", "process"].concat(),
        ["std::", "env"].concat(),
        ["write", "_all"].concat(),
        ["remove", "_file"].concat(),
        ["create", "_dir"].concat(),
    ]
    .to_vec();
    for (name, src) in [
        ("history.rs", include_str!("../src/history.rs")),
        ("paste.rs", include_str!("../src/paste.rs")),
    ] {
        for b in &banned {
            assert!(!src.contains(b.as_str()), "{name} names {b}");
        }
    }
}

// ---- found by the second-opinion review ----------------------------------------------

#[test]
fn the_draft_that_comes_back_keeps_the_taint_it_had() {
    // A draft that was pasted or came from an editor is tainted; walking away from it and back must not wash that off.
    let mut r = ring();
    r.prev("pasted text", true);
    r.prev("pasted text", true);
    let back = {
        r.next();
        r.next().unwrap()
    };
    assert_eq!(back.text, "pasted text");
    assert!(back.tainted, "the draft was tainted when the walk began");

    // A draft the user typed comes back untainted.
    let mut r2 = ring();
    r2.prev("typed text", false);
    assert!(!r2.next().unwrap().tainted);

    let mut r3 = ring();
    r3.prev("pasted text", true);
    assert_eq!(
        r3.cancel(),
        Some(Draft {
            text: "pasted text".into(),
            tainted: true
        })
    );
    let s = ring().search("pasted text", true);
    assert_eq!(
        s.cancel(),
        Draft {
            text: "pasted text".into(),
            tainted: true
        }
    );
}

fn long_prompt(tail: &str) -> String {
    format!("{}{tail}", "p".repeat(EARLIER_PROMPT_CHARS))
}

#[test]
fn prompts_that_share_their_first_two_thousand_characters_are_not_one_prompt() {
    let a = long_prompt(" ends one");
    let b = long_prompt(" ends two");
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNA", &a), earlier("01RUNB", &b)]);
    let seen = walk_up(&mut r, "");
    assert_eq!(seen.len(), 2, "both truncated prompts are listed");
    // A complete prompt that happens to equal a truncated prefix is another prompt too.
    let mut r = HistoryRing::new();
    r.record(&"p".repeat(EARLIER_PROMPT_CHARS));
    r.set_earlier_runs(vec![earlier("01RUNA", &a)]);
    assert_eq!(walk_up(&mut r, "").len(), 2);
    // Two complete prompts of the same text are still one.
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNA", "same"), earlier("01RUNB", "same")]);
    assert_eq!(walk_up(&mut r, "").len(), 1);
}

#[test]
fn a_recalled_prefix_is_pending_until_the_full_text_arrives_and_is_never_sent_short() {
    let full = long_prompt(" and the end of it");
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    assert_eq!(r.pending_full(), None);
    let rec = r.prev("", false).unwrap();
    assert_eq!(
        r.pending_full(),
        Some("01RUNC"),
        "the screen must not send the prefix"
    );
    // The full text arrives: it replaces the prefix, and only if the box still holds the prefix.
    let got = r.supply_full("01RUNC", &full).unwrap();
    assert_eq!(got.text, full);
    assert_eq!(got.replaces.as_deref(), Some(rec.text.as_str()));
    assert_eq!(r.pending_full(), None);
    // A text that does not begin with the prefix is not that prompt: it is refused and the wait goes on.
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    r.prev("", false);
    assert!(r.supply_full("01RUNC", "something else entirely").is_none());
    assert_eq!(r.pending_full(), Some("01RUNC"));
    // Editing the box ends the wait.
    r.forget_pending();
    assert_eq!(r.pending_full(), None);
    // So does sending.
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    r.prev("", false);
    assert_eq!(r.pending_full(), Some("01RUNC"));
    r.record("sent");
    assert_eq!(r.pending_full(), None);
}

#[test]
fn a_prefix_accepted_from_a_search_is_pending_too() {
    let full = long_prompt(" needle at the end");
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    let mut s = r.search("", false);
    typed(&mut s, "pppp");
    let rec = r.accept_search(s).unwrap();
    assert_eq!(rec.fetch_full.as_deref(), Some("01RUNC"));
    assert_eq!(r.pending_full(), Some("01RUNC"));
    let got = r.supply_full("01RUNC", &full).unwrap();
    assert_eq!(got.replaces.as_deref(), Some(rec.text.as_str()));
    assert_eq!(got.text, full);
}

#[test]
fn the_session_is_held_to_a_number_of_bytes_as_well_as_of_prompts() {
    assert_eq!(SESSION_MAX_BYTES, 8 * 1024 * 1024);
    let mib4 = |c: char| c.to_string().repeat(4 * 1024 * 1024);
    let mut r = HistoryRing::new();
    r.record(&mib4('a'));
    r.record(&mib4('b'));
    r.record(&mib4('c'));
    let seen = walk_up(&mut r, "");
    assert_eq!(
        seen.len(),
        2,
        "the oldest went to keep the total under the cap"
    );
    assert!(seen[0].starts_with('c') && seen[1].starts_with('b'));
    // A single prompt over the cap is kept (the newest is never dropped).
    let mut r = HistoryRing::new();
    r.record(&"z".repeat(SESSION_MAX_BYTES + 10));
    assert_eq!(walk_up(&mut r, "").len(), 1);
}

#[test]
fn text_that_comes_from_the_server_is_cleaned_of_escape_sequences() {
    let e = Earlier::from_prompt("01RUND", "a\u{1b}[2Jb\u{7}c");
    assert_eq!(e.text, "abc");
    let mut r = HistoryRing::new();
    r.set_run_messages(vec!["x\u{1b}]0;title\u{7}y".into()]);
    r.set_earlier_runs(vec![e]);
    assert_eq!(walk_up(&mut r, ""), ["xy", "abc"]);
}

// ---- the prompts of earlier runs come from the run index as titles ------------------------------------------------------------

/// The title the run index keeps for `prompt`, made by the rule the server uses (`run_index::title_of` over the first 2,000 characters).
fn title_for(prompt: &str) -> String {
    let (head, _) = ham_core::run_index::prompt_head(prompt);
    ham_core::run_index::title_of(&head)
}

fn long_first_line() -> String {
    format!(
        "{} the whole of this first line is longer than the eighty characters a title keeps\n\n   second   line\twith  odd   blanks\nthird line",
        "Build a calculator library with tests for every function and"
    )
}

#[test]
fn a_title_from_the_run_index_is_a_short_form_that_is_fetched_in_full_when_recalled() {
    let e = Earlier::from_title("01RUNT", "Build a todo API with tests");
    assert_eq!(e.text, "Build a todo API with tests");
    assert!(
        e.truncated,
        "a title is only the first line: the full prompt is fetched"
    );
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![e]);
    let rec = r.prev("", false).unwrap();
    assert_eq!(rec.text, "Build a todo API with tests");
    assert_eq!(rec.fetch_full.as_deref(), Some("01RUNT"));
    assert_eq!(r.pending_full(), Some("01RUNT"));
    assert!(rec.tainted);
}

#[test]
fn two_runs_with_the_same_title_are_two_entries() {
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![
        Earlier::from_title("01RUNA", "Fix the bug"),
        Earlier::from_title("01RUNB", "Fix the bug"),
    ]);
    assert_eq!(walk_up(&mut r, "").len(), 2);
}

#[test]
fn the_full_prompt_replaces_a_title_that_was_cut_and_collapsed_by_the_servers_rule() {
    let full = long_first_line();
    let title = title_for(&full);
    assert!(
        title.ends_with('\u{2026}'),
        "the title is cut with an ellipsis: {title}"
    );
    assert!(
        !full.starts_with(&title),
        "so it is not a literal prefix of the prompt"
    );
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", &title)]);
    let rec = r.prev("", false).unwrap();
    assert_eq!(rec.text, title);
    let got = r.supply_full("01RUNT", &full).unwrap();
    assert_eq!(got.text, full);
    assert_eq!(got.replaces.as_deref(), Some(title.as_str()));
    assert_eq!(r.pending_full(), None);
}

#[test]
fn a_title_that_is_a_literal_prefix_is_replaced_too() {
    let full = "Build a todo API with tests\nand a README";
    let title = title_for(full);
    assert_eq!(title, "Build a todo API with tests");
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", &title)]);
    r.prev("", false);
    let got = r.supply_full("01RUNT", full).unwrap();
    assert_eq!(got.text, full);
}

#[test]
fn a_full_prompt_whose_title_is_another_is_refused_and_the_wait_goes_on() {
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", "Build a todo API")]);
    r.prev("", false);
    assert!(
        r.supply_full("01RUNT", "Write the release notes\nfor the team")
            .is_none()
    );
    assert_eq!(r.pending_full(), Some("01RUNT"));
}

#[test]
fn a_title_with_escape_sequences_is_cleaned() {
    let e = Earlier::from_title("01RUNT", "Fix\u{1b}[2J the\u{7} bug");
    assert_eq!(e.text, "Fix the bug");
    assert!(e.truncated);
}

#[test]
fn a_refresh_of_the_list_keeps_the_prompts_already_fetched() {
    let full = long_first_line();
    let title = title_for(&full);
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", &title)]);
    r.prev("", false);
    assert!(r.supply_full("01RUNT", &full).is_some());
    r.cancel();
    // The Runs list is read again (every few seconds): the same title comes back, and the full prompt is still known.
    r.set_earlier_runs(vec![
        Earlier::from_title("01RUNN", "A newer run"),
        Earlier::from_title("01RUNT", &title),
    ]);
    let newer = r.prev("", false).unwrap();
    assert_eq!(newer.text, "A newer run");
    let again = r.prev("", false).unwrap();
    assert_eq!(again.text, full);
    assert_eq!(
        again.fetch_full, None,
        "the full prompt was fetched once and is kept"
    );
}

#[test]
fn a_refresh_that_names_another_title_for_the_run_drops_the_old_full_prompt() {
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", "First title")]);
    r.prev("", false);
    assert!(r.supply_full("01RUNT", "First title\nmore").is_some());
    r.cancel();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", "Quite another title")]);
    let rec = r.prev("", false).unwrap();
    assert_eq!(rec.text, "Quite another title");
    assert_eq!(rec.fetch_full.as_deref(), Some("01RUNT"));
}

#[test]
fn ctrl_r_searches_the_titles_of_earlier_runs_and_a_match_asks_for_the_full_prompt() {
    let mut r = HistoryRing::new();
    r.record("deploy to staging");
    r.set_earlier_runs(vec![
        Earlier::from_title("01RUNA", "Add retries to the uploader"),
        Earlier::from_title("01RUNB", "Write the release notes"),
    ]);
    let mut s = r.search("", false);
    typed(&mut s, "RETRIES");
    assert_eq!(s.current(), Some("Add retries to the uploader"));
    assert_eq!(s.position(), Some((1, 1)));
    let rec = r.accept_search(s).unwrap();
    assert_eq!(rec.text, "Add retries to the uploader");
    assert_eq!(rec.fetch_full.as_deref(), Some("01RUNA"));
    assert_eq!(r.pending_full(), Some("01RUNA"));
}

#[test]
fn a_title_is_checked_by_the_title_rule_and_not_by_being_a_prefix() {
    // `Fix` begins the prompt below, but the title of that prompt is its whole first line: this is another run's prompt.
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", "Fix")]);
    r.prev("", false);
    assert!(
        r.supply_full("01RUNT", "Fix the wrong repository\nmore words")
            .is_none()
    );
    assert_eq!(r.pending_full(), Some("01RUNT"));
    // The prompt that has that title is accepted.
    assert!(r.supply_full("01RUNT", "Fix\nand then the rest").is_some());
}

#[test]
fn a_prompt_with_a_control_character_in_its_first_line_is_still_its_titles_prompt() {
    // The server makes the title from the raw prompt (a control character becomes a space); the client stores the prompt without the
    // control character. The check uses the raw prompt too.
    let raw = "red\u{7}blue\nadditional instructions";
    let title = title_for(raw);
    assert_eq!(title, "red blue");
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![Earlier::from_title("01RUNT", &title)]);
    r.prev("", false);
    let got = r.supply_full("01RUNT", raw).unwrap();
    assert_eq!(got.text, "redblue\nadditional instructions");
    assert_eq!(got.replaces.as_deref(), Some("red blue"));
}

#[test]
fn a_head_of_a_prompt_is_still_checked_as_a_prefix() {
    let full = long_prompt(" and its end");
    let mut r = HistoryRing::new();
    r.set_earlier_runs(vec![earlier("01RUNC", &full)]);
    r.prev("", false);
    // The head is not a title: a text that is not its continuation is refused, whatever its first line.
    assert!(r.supply_full("01RUNC", "p").is_none());
    assert!(r.supply_full("01RUNC", &full).is_some());
}

// ---- the marks of the query in the match row ----------------------------------------------------------------------------------

fn shown_for(entries: &[&str], query: &str) -> Option<ham_core::history::Shown> {
    let mut r = HistoryRing::new();
    for e in entries {
        r.record(e);
    }
    let mut s = r.search("", false);
    typed(&mut s, query);
    s.shown()
}

#[test]
fn the_shown_match_marks_each_occurrence_and_letter_case_follows_the_search() {
    let sh = shown_for(&["Run the tests, then the TESTS again"], "tests").unwrap();
    assert_eq!(sh.text, "Run the tests, then the TESTS again");
    assert_eq!(sh.marks, vec![8..13, 24..29]);
}

#[test]
fn the_shown_match_counts_a_line_break_as_the_three_characters_that_show_it() {
    let sh = shown_for(&["first line\nthe tests here"], "tests").unwrap();
    assert_eq!(sh.text, "first line \u{23ce} the tests here");
    let marked: String = sh
        .text
        .chars()
        .skip(sh.marks[0].start)
        .take(sh.marks[0].len())
        .collect();
    assert_eq!(marked, "tests");
    assert_eq!(sh.marks.len(), 1);
}

#[test]
fn the_shown_match_has_no_marks_for_an_empty_query_and_nothing_for_no_match() {
    let mut r = HistoryRing::new();
    r.record("some prompt");
    let mut s = r.search("", false);
    assert!(
        s.shown().is_none(),
        "an empty query matches nothing, so nothing is shown"
    );
    typed(&mut s, "zzz");
    assert!(s.shown().is_none());
    s.clear();
    typed(&mut s, "   ");
    assert!(s.shown().is_none(), "a blank query marks nothing");
}

#[test]
fn the_shown_match_maps_the_marks_after_a_character_that_is_shown_differently() {
    // A tab is shown as a space and a zero-width space as a marker of several characters: the marks still sit on the query.
    let sh = shown_for(&["a\tb\u{200b}c needle d"], "needle").unwrap();
    assert!(
        sh.text.contains("<U+200B>"),
        "the invisible character is shown: {}",
        sh.text
    );
    let marked: String = sh
        .text
        .chars()
        .skip(sh.marks[0].start)
        .take(sh.marks[0].len())
        .collect();
    assert_eq!(marked, "needle");
}

// ---- properties ---------------------------------------------------------------------------------------------------------------------

proptest! {
    #[test]
    fn any_mix_of_steps_keeps_the_ring_consistent(
        sent in proptest::collection::vec("[a-z ]{0,8}", 0..8),
        steps in proptest::collection::vec(0u8..5, 0..40),
    ) {
        let mut r = HistoryRing::new();
        for s in &sent {
            r.record(s);
        }
        r.set_earlier_runs(vec![earlier("01RUNA", "old one")]);
        let kept: Vec<String> = {
            let mut probe = r.clone();
            walk_up(&mut probe, "")
        };
        let mut depth: Option<usize> = None;
        for s in steps {
            match s {
                0 | 1 => {
                    if let Some(rec) = r.prev(DRAFT, false) {
                        let d = depth.map_or(0, |d| d + 1);
                        prop_assert_eq!(&rec.text, &kept[d]);
                        depth = Some(d);
                    }
                }
                2 => {
                    if let Some(rec) = r.next() {
                        match depth {
                            Some(0) => { prop_assert_eq!(rec.text, DRAFT); depth = None; }
                            Some(d) => { prop_assert_eq!(&rec.text, &kept[d - 1]); depth = Some(d - 1); }
                            None => prop_assert!(false, "next while not browsing"),
                        }
                    }
                }
                3 => { r.cancel(); depth = None; }
                _ => {
                    let mut srch = r.search(DRAFT, false);
                    srch.type_char('o');
                    let _ = srch.current();
                    let _ = srch.older();
                }
            }
            prop_assert_eq!(r.is_browsing(), depth.is_some());
        }
    }
}
