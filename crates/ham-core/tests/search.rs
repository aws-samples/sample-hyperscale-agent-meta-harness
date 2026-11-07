//! Search inside one chat. The matcher folds case and a few Unicode letters, finds non-overlapping matches in characters,
//! and runs in time linear in the text; the search over a chat model hits each block's head and body (never the role word or the
//! progress-feed label) and says how many non-blank characters come before the match, so the transcript can find the wrapped row;
//! `ChatSearch` is the state machine behind the slash bar: open, type, accept, `n` and `N`, cancel with the scroll position put back.

#![allow(clippy::unwrap_used)]

use ham_core::chat::{Block, ChatModel, Origin, Role, Source};
use ham_core::search::{
    ChatSearch, Effect, Key, MAX_MATCHES, MAX_QUERY_CHARS, Part, Phase, ScrollMark, find_ranges,
    find_ranges_counted, fold, search_blocks,
};
use proptest::prelude::*;

fn r(text: &str, query: &str) -> Vec<(usize, usize)> {
    find_ranges(text, query)
        .into_iter()
        .map(|x| (x.start, x.end))
        .collect()
}

fn model(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::FullHistory, "cline", blocks)
}

fn you(i: usize, text: &str) -> Block {
    Block::message(&format!("u{i}"), Role::You, Origin::User, text)
}

fn cline(i: usize, text: &str) -> Block {
    Block::message(&format!("m{i}"), Role::Engine, Origin::Engine, text)
}

/// A short conversation with `test` in six places: two of them in the head and body of a folded tool call.
fn chat() -> ChatModel {
    model(vec![
        you(0, "Build a todo API with tests"),
        cline(1, "I will start with the failing test."),
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "collected 3 items\nFAILED test_api.py::test_create - AssertionError",
        ),
        cline(3, "Tests pass now."),
    ])
}

fn mark(top: usize, following: bool, anchor: Option<usize>) -> ScrollMark {
    ScrollMark {
        top_line: top,
        following,
        selected: None,
        anchor,
    }
}

fn type_str(s: &mut ChatSearch, m: &ChatModel, text: &str) -> Vec<Effect> {
    let mut last = Vec::new();
    for c in text.chars() {
        last = s.key(Key::Char(c), m);
    }
    last
}

fn shown(effects: &[Effect]) -> Option<&ham_core::search::Hit> {
    effects.iter().find_map(|e| match e {
        Effect::Show(h) => Some(h),
        Effect::Restore(_) => None,
    })
}

// ---- the matcher --------------------------------------------------------------------------------------------------------------------

#[test]
fn an_empty_or_blank_query_finds_nothing() {
    assert!(find_ranges("abc", "").is_empty());
    assert!(find_ranges("a b c", "  ").is_empty());
    assert!(find_ranges("", "a").is_empty());
}

#[test]
fn case_is_ignored_in_both_directions() {
    assert_eq!(
        r("Hello HELLO hello", "hello"),
        vec![(0, 5), (6, 11), (12, 17)]
    );
    assert_eq!(r("hello", "HeLLo"), vec![(0, 5)]);
}

#[test]
fn unicode_letters_fold_and_ranges_count_characters() {
    // The sharp s folds to "ss" in both the text and the query; the range is the original characters.
    assert_eq!(r("Straße 5", "STRASSE"), vec![(0, 6)]);
    assert_eq!(r("STRASSE", "straße"), vec![(0, 7)]);
    assert_eq!(r("ÉCOLE école", "école"), vec![(0, 5), (6, 11)]);
    // A final sigma is the same letter as a medial one.
    assert_eq!(r("ΟΔΟΣ", "οδος"), vec![(0, 4)]);
    // A capital dotted I folds to "i" plus a combining dot: the query "i" finds it.
    assert_eq!(r("İstanbul", "i"), vec![(0, 1)]);
    // Ranges are characters, not bytes or UTF-16 units.
    assert_eq!(r("日本語 テキスト 日本語", "日本語"), vec![(0, 3), (9, 12)]);
    assert_eq!(r("a😀b😀", "😀"), vec![(1, 2), (3, 4)]);
}

#[test]
fn text_that_is_not_normalized_is_not_matched_across_forms() {
    // No Unicode normalization: a precomposed letter and a letter with a combining accent are different strings.
    assert!(r("cafe\u{301}", "café").is_empty());
    assert_eq!(r("café", "café"), vec![(0, 4)]);
}

#[test]
fn matches_do_not_overlap_and_are_leftmost() {
    assert_eq!(r("aaaa", "aa"), vec![(0, 2), (2, 4)]);
    assert_eq!(r("aaa", "aa"), vec![(0, 2)]);
    assert_eq!(r("abababa", "aba"), vec![(0, 3), (4, 7)]);
    // A sharp s that holds two matches of "s" is one range.
    assert_eq!(r("ß", "s"), vec![(0, 1)]);
}

#[test]
fn a_query_with_a_space_matches_across_the_space_only() {
    assert_eq!(r("hello world", "o w"), vec![(4, 7)]);
    assert!(r("hello\nworld", "o w").is_empty());
}

#[test]
fn the_worst_pattern_takes_linear_work() {
    let text = "a".repeat(100_000);
    let query = format!("{}b", "a".repeat(255));
    let (hits, steps) = find_ranges_counted(&text, &query);
    assert!(hits.is_empty());
    assert!(steps <= 4 * 100_000 + 1024, "steps {steps}");
}

#[test]
fn a_huge_block_with_a_thousand_matches_is_found_whole() {
    let mut text = String::new();
    for i in 0..1000 {
        text.push_str(&format!("line {i} needle\n"));
        text.push_str(&"x".repeat(90));
        text.push('\n');
    }
    assert!(text.chars().count() > 90_000);
    let found = find_ranges(&text, "NEEDLE");
    assert_eq!(found.len(), 1000);
    let chars: Vec<char> = text.chars().collect();
    for f in &found {
        assert_eq!(chars[f.clone()].iter().collect::<String>(), "needle");
    }
}

#[test]
fn fold_lowercases_and_expands_the_sharp_s() {
    assert_eq!(fold("Straße ΟΔΟΣ"), "strasse οδοσ");
}

fn naive(text: &str, query: &str) -> Vec<(usize, usize)> {
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while !q.is_empty() && i + q.len() <= t.len() {
        if t[i..i + q.len()] == q[..] {
            out.push((i, i + q.len()));
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

proptest! {
    #[test]
    fn ascii_matches_equal_a_naive_scan(text in "[abAB ]{0,60}", query in "[abAB]{1,4}") {
        prop_assert_eq!(r(&text, &query), naive(&text, &query));
    }

    #[test]
    fn any_text_gives_sorted_disjoint_in_bounds_ranges(text in "\\PC{0,80}", query in "\\PC{0,6}") {
        let n = text.chars().count();
        let found = find_ranges(&text, &query);
        let mut end = 0;
        for f in &found {
            prop_assert!(f.start < f.end && f.end <= n);
            prop_assert!(f.start >= end);
            end = f.end;
            let slice: String = text.chars().skip(f.start).take(f.end - f.start).collect();
            prop_assert!(fold(&slice).contains(&fold(&query)), "{slice:?} vs {query:?}");
        }
    }

    #[test]
    fn a_substring_of_the_text_is_always_found(text in "[a-zA-Zß ]{1,50}", a in 0usize..50, b in 0usize..50) {
        let chars: Vec<char> = text.chars().collect();
        let (x, y) = (a % chars.len(), b % chars.len());
        let (lo, hi) = (x.min(y), x.max(y));
        let sub: String = chars[lo..=hi].iter().collect();
        prop_assume!(!sub.trim().is_empty());
        prop_assert!(!find_ranges(&text, &sub).is_empty(), "{sub:?} in {text:?}");
    }
}

// ---- hits over a chat model ---------------------------------------------------------------------------------------------------------

#[test]
fn hits_cover_each_blocks_head_and_body_in_order() {
    let m = chat();
    let hits = search_blocks(&m, "test");
    let got: Vec<(usize, Part, usize)> = hits
        .iter()
        .map(|h| (h.block, h.part, h.range.start))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, Part::Body, 22), // "tests"
            (1, Part::Body, 30), // "failing test."
            (2, Part::Head, 16), // "pytest"
            (2, Part::Body, 25), // "test_api.py"
            (2, Part::Body, 38), // "test_create"
            (3, Part::Body, 0),  // "Tests"
        ]
    );
    assert_eq!(hits[3].id, "t1");
}

#[test]
fn a_hit_counts_the_non_blank_characters_before_it_head_first() {
    let m = chat();
    let hits = search_blocks(&m, "test_api");
    assert_eq!(hits.len(), 1);
    // head "run_commands: pytest -q" has 13 + 6 + 2 = 21 non-blank characters;
    // the body "collected 3 items\nFAILED " has 9 + 1 + 5 + 6 = 21 before the match.
    assert_eq!(hits[0].nb_start, 42);
    assert_eq!(hits[0].nb_len, 8);
    let head = search_blocks(&m, "pytest -q");
    assert_eq!(head[0].nb_start, 13);
    assert_eq!(head[0].nb_len, 8, "the space is not counted");
}

#[test]
fn role_words_and_the_progress_feed_label_are_not_searched() {
    let m = ChatModel::from_blocks(
        Source::ProgressFeed,
        "cline",
        vec![you(0, "hello there"), cline(1, "hi")],
    );
    assert!(m.label().is_some());
    assert!(search_blocks(&m, "progress feed").is_empty());
    assert!(search_blocks(&m, "you").is_empty());
    assert!(search_blocks(&m, "cline").is_empty());
}

#[test]
fn text_is_searched_as_it_is_drawn() {
    // A bidi mark is drawn as a visible marker, so the marker is what a person can search for.
    let m = model(vec![cline(0, "a\u{202e}b")]);
    assert_eq!(search_blocks(&m, "<U+202E>").len(), 1);
    assert!(search_blocks(&m, "\u{202e}").is_empty());
}

#[test]
fn the_number_of_hits_is_capped() {
    let m = model(vec![cline(0, &"a ".repeat(20_000))]);
    assert_eq!(search_blocks(&m, "a").len(), MAX_MATCHES);
}

// ---- the state machine --------------------------------------------------------------------------------------------------------------

#[test]
fn idle_search_does_nothing() {
    let m = chat();
    let mut s = ChatSearch::new();
    assert_eq!(s.phase(), Phase::Idle);
    assert!(!s.is_active());
    assert!(s.next(&m).is_empty());
    assert!(s.prev(&m).is_empty());
    assert!(s.key(Key::Esc, &m).is_empty());
    assert_eq!(s.bar_text(40), "");
}

#[test]
fn typing_jumps_to_the_first_match_at_or_after_the_top_of_the_view() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(40, false, Some(2)));
    assert_eq!(s.phase(), Phase::Editing);
    assert!(s.is_active());
    let effects = type_str(&mut s, &m, "test");
    assert_eq!(s.query(), "test");
    assert_eq!(s.hits().len(), 6);
    assert_eq!(s.current(), Some(2), "the first hit in block 2 or later");
    let h = shown(&effects).unwrap();
    assert_eq!((h.block, h.part), (2, Part::Head));
    assert_eq!(s.count_text(), "3/6");
}

#[test]
fn a_view_at_the_bottom_starts_at_the_newest_match() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(99, true, Some(0)));
    type_str(&mut s, &m, "test");
    assert_eq!(s.current(), Some(5));
}

#[test]
fn a_view_past_the_last_match_wraps_to_the_first() {
    let m = model(vec![cline(0, "needle"), cline(1, "hay"), cline(2, "hay")]);
    let mut s = ChatSearch::new();
    s.open(mark(10, false, Some(2)));
    type_str(&mut s, &m, "needle");
    assert_eq!(s.current(), Some(0));
}

#[test]
fn enter_accepts_and_n_and_shift_n_walk_the_matches_with_wrap() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    assert_eq!(s.current(), Some(0));
    assert!(s.key(Key::Enter, &m).is_empty());
    assert_eq!(s.phase(), Phase::Active);

    let e = s.next(&m);
    assert_eq!(s.current(), Some(1));
    assert_eq!(shown(&e).unwrap().block, 1);
    assert!(!s.wrapped());

    // Walk to the end and past it.
    for _ in 0..4 {
        s.next(&m);
    }
    assert_eq!(s.current(), Some(5));
    let e = s.next(&m);
    assert_eq!(s.current(), Some(0));
    assert!(s.wrapped());
    assert_eq!(shown(&e).unwrap().block, 0);

    // Backwards from the first goes to the last.
    let e = s.prev(&m);
    assert_eq!(s.current(), Some(5));
    assert!(s.wrapped());
    assert_eq!(shown(&e).unwrap().block, 3);
    s.prev(&m);
    assert!(!s.wrapped(), "a step inside the range clears the flag");
    assert_eq!(s.count_text(), "5/6");
}

#[test]
fn escape_while_typing_puts_the_scroll_position_back() {
    let m = chat();
    let origin = ScrollMark {
        top_line: 40,
        following: false,
        selected: Some("m1".into()),
        anchor: Some(1),
    };
    let mut s = ChatSearch::new();
    s.open(origin.clone());
    type_str(&mut s, &m, "tes");
    let e = s.key(Key::Esc, &m);
    assert_eq!(e, vec![Effect::Restore(origin)]);
    assert_eq!(s.phase(), Phase::Idle);
    assert!(s.hits().is_empty());
    assert!(!s.is_active());
}

#[test]
fn escape_after_accepting_clears_the_search_and_leaves_the_view() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    s.next(&m);
    assert!(s.key(Key::Esc, &m).is_empty());
    assert_eq!(s.phase(), Phase::Idle);
    assert!(s.hits().is_empty());
    assert_eq!(s.query(), "");
}

#[test]
fn deleting_the_query_goes_back_to_where_the_view_was() {
    let m = chat();
    let origin = mark(7, false, Some(0));
    let mut s = ChatSearch::new();
    s.open(origin.clone());
    type_str(&mut s, &m, "t");
    assert!(s.hits().len() > 1);
    let e = s.key(Key::Backspace, &m);
    assert_eq!(e, vec![Effect::Restore(origin)]);
    assert_eq!(s.phase(), Phase::Editing);
    assert!(s.hits().is_empty());
}

#[test]
fn a_query_with_no_match_goes_back_too_and_enter_keeps_the_bar_open() {
    let m = chat();
    let origin = mark(3, false, Some(0));
    let mut s = ChatSearch::new();
    s.open(origin.clone());
    let e = type_str(&mut s, &m, "zzz");
    assert_eq!(e, vec![Effect::Restore(origin)]);
    assert_eq!(s.count_text(), "no matches");
    assert!(s.key(Key::Enter, &m).is_empty());
    assert_eq!(s.phase(), Phase::Editing);
}

#[test]
fn enter_on_an_empty_bar_closes_it_and_restores_the_view() {
    let m = chat();
    let origin = mark(3, false, Some(0));
    let mut s = ChatSearch::new();
    s.open(origin.clone());
    let e = s.key(Key::Enter, &m);
    assert_eq!(e, vec![Effect::Restore(origin)]);
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn editing_keys_change_the_query() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "foo bar");
    s.key(Key::DeleteWord, &m);
    assert_eq!(s.query(), "foo ");
    s.key(Key::DeleteWord, &m);
    assert_eq!(s.query(), "");
    type_str(&mut s, &m, "abc");
    s.key(Key::Clear, &m);
    assert_eq!(s.query(), "");
    type_str(&mut s, &m, "é");
    s.key(Key::Backspace, &m);
    assert_eq!(s.query(), "", "a backspace removes a whole character");
}

#[test]
fn control_characters_and_overlong_queries_are_not_taken() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    for c in ['\n', '\r', '\t', '\u{1b}', '\u{7}', '\u{0}'] {
        s.key(Key::Char(c), &m);
    }
    assert_eq!(s.query(), "");
    for _ in 0..MAX_QUERY_CHARS + 50 {
        s.key(Key::Char('a'), &m);
    }
    assert_eq!(s.query().chars().count(), MAX_QUERY_CHARS);
}

#[test]
fn the_count_stops_at_the_cap_and_says_so() {
    let m = model(vec![cline(0, &"a ".repeat(20_000))]);
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "a");
    assert_eq!(s.hits().len(), MAX_MATCHES);
    assert!(s.truncated());
    assert_eq!(s.count_text(), "1/10000+");
}

fn alphabet(n: usize) -> ChatModel {
    let mut blocks = Vec::new();
    for i in 0..n {
        let text = if [7, 19, 33].contains(&i) {
            format!("alpha {i} zebra crossing")
        } else {
            format!("alpha {i}")
        };
        blocks.push(cline(i, &text));
    }
    model(blocks)
}

#[test]
fn a_longer_query_looks_only_where_the_shorter_one_matched() {
    let m = alphabet(50);
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    s.key(Key::Char('z'), &m);
    assert_eq!(s.hits().len(), 3);
    assert_eq!(s.blocks_scanned(), 50);
    s.key(Key::Char('e'), &m);
    assert_eq!(s.blocks_scanned(), 3);
    s.key(Key::Char('b'), &m);
    assert_eq!(s.blocks_scanned(), 3);
    // A shorter query cannot narrow.
    s.key(Key::Backspace, &m);
    assert_eq!(s.blocks_scanned(), 50);
    // A query that does not extend the last one cannot narrow either.
    s.key(Key::Clear, &m);
    s.key(Key::Char('q'), &m);
    assert_eq!(s.blocks_scanned(), 50);
}

#[test]
fn a_changed_chat_is_searched_whole_again() {
    let m = alphabet(10);
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    s.key(Key::Char('z'), &m);
    assert_eq!(s.hits().len(), 1);
    // The chat grew a block with a "ze" in it: narrowing by the old matches would miss it.
    let mut blocks = m.blocks.clone();
    blocks.push(cline(99, "a zebra arrived"));
    let m2 = model(blocks);
    s.key(Key::Char('e'), &m2);
    assert_eq!(s.hits().len(), 2);
    assert_eq!(s.blocks_scanned(), 11);
}

#[test]
fn a_chat_that_grows_while_searching_keeps_the_current_match() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    s.next(&m);
    s.next(&m);
    assert_eq!(s.current(), Some(2));
    let before = s.hits()[2].clone();

    let mut blocks = m.blocks.clone();
    blocks.push(cline(9, "one more test line"));
    s.model_changed(&model(blocks));
    assert_eq!(s.hits().len(), 7);
    assert_eq!(s.count_text(), "3/7");
    assert_eq!(s.hits()[s.current().unwrap()], before);
}

#[test]
fn the_current_match_moves_on_when_its_block_goes_away() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    s.next(&m);
    s.next(&m);
    s.next(&m);
    assert_eq!(s.current(), Some(3));
    // The tool call (block 2) is gone: the hits of block 3 are what is left after the current place.
    let mut blocks = m.blocks.clone();
    blocks.remove(2);
    s.model_changed(&model(blocks));
    assert_eq!(s.hits().len(), 3);
    assert_eq!(s.hits()[s.current().unwrap()].id, "m3");

    // Nothing left at all.
    s.model_changed(&model(vec![cline(0, "nothing here")]));
    assert!(s.hits().is_empty());
    assert_eq!(s.current(), None);
    assert_eq!(s.count_text(), "no matches");
}

#[test]
fn the_bar_text_shows_the_query_the_position_and_a_wrap_note() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    assert_eq!(s.bar_text(30), "/");
    type_str(&mut s, &m, "test");
    assert_eq!(s.bar_text(40), "/test  1/6");
    // The end of the query stays visible when the bar is short.
    assert_eq!(s.bar_text(8), "…st  1/6");
    s.key(Key::Enter, &m);
    s.prev(&m);
    assert_eq!(s.bar_text(40), "/test  6/6 wrapped");
    assert_eq!(s.bar_text(0), "");
    assert_eq!(s.bar_text(3).chars().count(), 3);
}

#[test]
fn reopening_starts_a_fresh_query_and_remembers_the_new_origin() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    let again = mark(12, false, Some(1));
    s.open(again.clone());
    assert_eq!(s.phase(), Phase::Editing);
    assert_eq!(s.query(), "");
    assert!(s.hits().is_empty());
    assert_eq!(s.key(Key::Esc, &m), vec![Effect::Restore(again)]);
}

proptest! {
    #[test]
    fn typing_any_text_never_panics_and_keeps_the_position_in_range(
        words in proptest::collection::vec("\\PC{0,12}", 1..6),
        typed in "\\PC{0,10}",
    ) {
        let blocks: Vec<Block> = words.iter().enumerate().map(|(i, w)| cline(i, w)).collect();
        let m = model(blocks);
        let mut s = ChatSearch::new();
        s.open(mark(0, false, Some(0)));
        for c in typed.chars() {
            s.key(Key::Char(c), &m);
            if let Some(i) = s.current() {
                prop_assert!(i < s.hits().len());
            } else {
                prop_assert!(s.hits().is_empty());
            }
        }
        s.next(&m);
        s.prev(&m);
        s.key(Key::Enter, &m);
        s.key(Key::Esc, &m);
    }
}

// ---- found by the second-opinion review -----------------------------------

#[test]
fn a_match_that_ends_inside_a_folded_letter_does_not_spoil_the_next_one() {
    // The sharp s is two folded characters. After a match ends on its first "s", the second "s" belongs to the same letter and is
    // part of the match already; it must not start the next match.
    assert_eq!(r("saßasas", "sas"), vec![(0, 3), (4, 7)]);
    assert_eq!(r("ßßsss", "sss"), vec![(0, 2), (2, 5)]);
    assert_eq!(r("ßss", "ss"), vec![(0, 1), (1, 3)]);
}

/// The matcher's rule written the slow way: fold the text into characters with the original character each came from, find the
/// leftmost occurrence, take whole original characters, and go on after the last one taken.
fn reference(text: &str, query: &str) -> Vec<(usize, usize)> {
    let q: Vec<char> = fold(query).chars().collect();
    if query.trim().is_empty() || q.is_empty() {
        return Vec::new();
    }
    let mut folded: Vec<(char, usize)> = Vec::new();
    for (i, c) in text.chars().enumerate() {
        for f in fold(&c.to_string()).chars() {
            folded.push((f, i));
        }
    }
    let mut out = Vec::new();
    let mut p = 0;
    while p + q.len() <= folded.len() {
        if folded[p..p + q.len()]
            .iter()
            .map(|x| x.0)
            .eq(q.iter().copied())
        {
            let (start, end) = (folded[p].1, folded[p + q.len() - 1].1);
            out.push((start, end + 1));
            p = folded
                .iter()
                .position(|x| x.1 > end)
                .unwrap_or(folded.len());
        } else {
            p += 1;
        }
    }
    out
}

proptest! {
    #[test]
    fn folded_letters_match_the_slow_reference(text in "[sSaAßẞſ ]{0,24}", query in "[sSaAßẞſ]{1,5}") {
        prop_assert_eq!(r(&text, &query), reference(&text, &query));
    }

    #[test]
    fn mixed_scripts_match_the_slow_reference(text in "[sSaAßΣσςİiI\u{307} ]{0,24}", query in "[sSaAßΣσςiI]{1,4}") {
        prop_assert_eq!(r(&text, &query), reference(&text, &query));
    }
}

#[test]
fn the_current_match_moves_forward_when_its_own_hit_goes_away_inside_one_block() {
    let one = |t: &str| model(vec![cline(0, t)]);
    let m = one("test a test b test c");
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    s.next(&m);
    assert_eq!(s.hits()[s.current().unwrap()].range.start, 7);
    // The second "test" is spoilt: the next hit after its place is the third, not the first.
    s.model_changed(&one("test a tes b test c"));
    assert_eq!(s.hits().len(), 2);
    assert_eq!(s.hits()[s.current().unwrap()].range.start, 13);
}

#[test]
fn an_unchanged_chat_is_not_searched_again() {
    let m = chat();
    let mut s = ChatSearch::new();
    s.open(mark(0, false, Some(0)));
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    assert!(s.blocks_scanned() > 0);
    s.model_changed(&m);
    assert_eq!(
        s.blocks_scanned(),
        0,
        "same blocks, same revisions: nothing to do"
    );
    let mut blocks = m.blocks.clone();
    blocks.push(cline(9, "one more test"));
    s.model_changed(&model(blocks));
    assert!(s.blocks_scanned() > 0);
}

#[test]
fn n_belongs_to_the_search_only_once_it_is_accepted() {
    let m = chat();
    let mut s = ChatSearch::new();
    assert!(!s.takes_text() && !s.walks_matches());
    s.open(mark(0, false, Some(0)));
    assert!(s.takes_text(), "typing: every character, an n too, is text");
    assert!(!s.walks_matches());
    assert!(s.is_active(), "but Esc already belongs to the search");
    type_str(&mut s, &m, "test");
    s.key(Key::Enter, &m);
    assert!(!s.takes_text());
    assert!(s.walks_matches(), "accepted: n and N walk the matches");
    s.key(Key::Esc, &m);
    assert!(!s.takes_text() && !s.walks_matches());
}
