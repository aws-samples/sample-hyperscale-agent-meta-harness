//! The large-paste placeholder. A paste over 150 characters or of three lines or more goes into the chat box as
//! `[Pasted n lines]` and the text is kept on the side; a key expands one placeholder in the box, and sending (or opening the draft in an
//! editor, or recording it in the history) expands them all. A paste is text, never keys, and it is cleaned of escape sequences and
//! controls first. Pasted text taints the draft; that is the caller's flag, the store only decides what is shown.

#![allow(clippy::unwrap_used)]

use ham_core::paste::{PASTE_MAX_CHARS, PASTE_MIN_LINES, PasteStore, Pasted};
use proptest::prelude::*;

fn token_of(p: Pasted) -> String {
    match p {
        Pasted::Placeholder(t) => t,
        Pasted::Inline(t) => panic!("inline: {t:?}"),
    }
}

fn inline_of(p: Pasted) -> String {
    match p {
        Pasted::Inline(t) => t,
        Pasted::Placeholder(t) => panic!("placeholder: {t:?}"),
    }
}

#[test]
fn the_thresholds_are_150_characters_and_three_lines() {
    assert_eq!(PASTE_MAX_CHARS, 150);
    assert_eq!(PASTE_MIN_LINES, 3);
    let mut s = PasteStore::new();
    // 150 characters stay; 151 become a placeholder.
    assert_eq!(inline_of(s.offer(&"a".repeat(150))).len(), 150);
    assert_eq!(token_of(s.offer(&"a".repeat(151))), "[Pasted 1 line]");
    // Two lines stay; three become a placeholder.
    assert_eq!(inline_of(s.offer("one\ntwo")), "one\ntwo");
    assert_eq!(token_of(s.offer("one\ntwo\nthree")), "[Pasted 3 lines]");
    // A final line break does not make a line.
    assert_eq!(inline_of(s.offer("one\ntwo\n")), "one\ntwo\n");
    assert_eq!(token_of(s.offer("1\n2\n3\n")), "[Pasted 3 lines #2]");
}

#[test]
fn characters_are_counted_not_bytes() {
    let mut s = PasteStore::new();
    assert!(matches!(s.offer(&"é".repeat(150)), Pasted::Inline(_)));
    assert!(matches!(s.offer(&"é".repeat(151)), Pasted::Placeholder(_)));
    assert!(matches!(s.offer(&"日".repeat(150)), Pasted::Inline(_)));
}

#[test]
fn line_breaks_of_every_kind_count_once() {
    let mut s = PasteStore::new();
    assert_eq!(token_of(s.offer("a\r\nb\r\nc")), "[Pasted 3 lines]");
    let mut s = PasteStore::new();
    assert_eq!(token_of(s.offer("a\rb\rc")), "[Pasted 3 lines]");
    let mut s = PasteStore::new();
    assert_eq!(
        inline_of(s.offer("a\r\nb")),
        "a\nb",
        "line breaks are normalized to \\n"
    );
}

#[test]
fn a_second_placeholder_of_the_same_size_is_told_apart() {
    let mut s = PasteStore::new();
    let a = token_of(s.offer("1\n2\n3"));
    let b = token_of(s.offer("4\n5\n6"));
    let c = token_of(s.offer("7\n8\n9"));
    assert_eq!(a, "[Pasted 3 lines]");
    assert_eq!(b, "[Pasted 3 lines #2]");
    assert_eq!(c, "[Pasted 3 lines #3]");
    let text = format!("{a} and {b} and {c}");
    assert_eq!(s.expand_all(&text), "1\n2\n3 and 4\n5\n6 and 7\n8\n9");
}

#[test]
fn a_paste_is_cleaned_of_escape_sequences_and_controls() {
    let mut s = PasteStore::new();
    assert_eq!(inline_of(s.offer("a\u{1b}[2Jb\u{7}c")), "abc");
    let t = token_of(s.offer("x\u{1b}]0;title\u{7}1\ny\u{8}2\nz3"));
    assert_eq!(s.expand_all(&t), "x1\ny2\nz3");
}

#[test]
fn expand_all_puts_the_text_back_and_leaves_everything_else_alone() {
    let mut s = PasteStore::new();
    let t = token_of(s.offer("l1\nl2\nl3\nl4"));
    assert_eq!(t, "[Pasted 4 lines]");
    assert_eq!(
        s.expand_all(&format!("see {t}, please")),
        "see l1\nl2\nl3\nl4, please"
    );
    assert_eq!(s.expand_all("no tokens here"), "no tokens here");
    // A token this store never made is plain text.
    assert_eq!(s.expand_all("[Pasted 9 lines]"), "[Pasted 9 lines]");
    // A token typed twice expands twice.
    assert_eq!(
        s.expand_all(&format!("{t}{t}")),
        "l1\nl2\nl3\nl4l1\nl2\nl3\nl4"
    );
}

#[test]
fn pasted_text_that_looks_like_a_placeholder_is_not_expanded_again() {
    let mut s = PasteStore::new();
    let first = token_of(s.offer("a\nb\nc"));
    // The second paste holds the first one's token as text.
    let second = token_of(s.offer(&format!("{first}\nx\ny")));
    assert_eq!(second, "[Pasted 3 lines #2]");
    assert_eq!(s.expand_all(&second), format!("{first}\nx\ny"));
    // And the longer token is not read as the shorter one.
    assert_eq!(
        s.expand_all(&format!("{first}{second}")),
        format!("a\nb\nc{first}\nx\ny")
    );
}

#[test]
fn expand_at_expands_the_placeholder_under_or_beside_the_caret() {
    let mut s = PasteStore::new();
    let t = token_of(s.offer("l1\nl2\nl3"));
    let text = format!("ab {t} cd");
    // The caret is counted in characters.
    let start = 3;
    let end = start + t.chars().count();
    for caret in [start, start + 4, end] {
        let got = s.expand_at(&text, caret).unwrap();
        assert_eq!(got.text, "ab l1\nl2\nl3 cd", "caret {caret}");
        assert_eq!(
            got.caret,
            start + "l1\nl2\nl3".chars().count(),
            "the caret ends after the text"
        );
    }
    assert!(s.expand_at(&text, 1).is_none(), "the caret is in 'ab'");
    assert!(s.expand_at(&text, end + 2).is_none());
    assert!(s.expand_at("no token", 3).is_none());
}

#[test]
fn between_two_placeholders_the_one_before_the_caret_goes_first() {
    let mut s = PasteStore::new();
    let a = token_of(s.offer("1\n2\n3"));
    let b = token_of(s.offer("4\n5\n6"));
    let text = format!("{a}{b}");
    let caret = a.chars().count();
    let got = s.expand_at(&text, caret).unwrap();
    assert_eq!(got.text, format!("1\n2\n3{b}"));
}

#[test]
fn expand_at_counts_wide_and_multi_byte_characters_before_the_token() {
    let mut s = PasteStore::new();
    let t = token_of(s.offer("l1\nl2\nl3"));
    let text = format!("日本語é{t}");
    let got = s.expand_at(&text, 6).unwrap();
    assert_eq!(got.text, "日本語él1\nl2\nl3");
}

#[test]
fn the_ranges_of_the_live_placeholders_are_known_for_drawing() {
    let mut s = PasteStore::new();
    let a = token_of(s.offer("1\n2\n3"));
    let b = token_of(s.offer("4\n5\n6"));
    let text = format!("é {a} x {b}");
    let r = s.placeholders_in(&text);
    let a_len = a.chars().count();
    let b_len = b.chars().count();
    assert_eq!(r, vec![2..2 + a_len, 2 + a_len + 3..2 + a_len + 3 + b_len]);
    assert!(s.placeholders_in("nothing").is_empty());
}

#[test]
fn clear_forgets_the_pastes_but_never_reuses_a_token() {
    let mut s = PasteStore::new();
    let t = token_of(s.offer("1\n2\n3"));
    assert_eq!(s.len(), 1);
    s.clear();
    assert!(s.is_empty());
    assert_eq!(s.expand_all(&t), t, "an old token is only text now");
    // An undo that brings the old token back must not meet a new paste that wears the same token.
    let again = token_of(s.offer("4\n5\n6"));
    assert_eq!(again, "[Pasted 3 lines #2]");
    assert_ne!(again, t);
    assert_eq!(s.expand_all(&t), t);
    assert_eq!(s.expand_all(&again), "4\n5\n6");
}

#[test]
fn a_token_that_was_issued_and_is_no_longer_live_is_found_before_a_send() {
    let mut s = PasteStore::new();
    let t = token_of(s.offer("1\n2\n3"));
    let live = token_of(s.offer("4\n5\n6"));
    assert!(s.dangling(&format!("{t} and {live}")).is_empty());
    s.clear();
    let again = token_of(s.offer("7\n8\n9"));
    let text = format!("é {t} then {again}");
    // The old token (the draft was undone to it) is dangling; the live one is not.
    assert_eq!(s.dangling(&text), vec![2..2 + t.chars().count()]);
    assert!(s.dangling("nothing to see").is_empty());
    assert!(
        s.dangling("[Pasted 99 lines]").is_empty(),
        "a token this store never issued is text"
    );
}

#[test]
fn a_paste_that_holds_the_shape_of_a_placeholder_is_always_kept_aside() {
    let mut s = PasteStore::new();
    let a = token_of(s.offer("a\nb\nc"));
    // Short, but it carries a token: its text must never sit in the box where a send would read it as a placeholder.
    let b = token_of(s.offer(&format!("see {a} above")));
    assert_eq!(b, "[Pasted 1 line]");
    assert_eq!(
        s.expand_all(&b),
        format!("see {a} above"),
        "expanded once, never again"
    );
    let c = token_of(s.offer("x [Pasted 99 lines] y"));
    assert_eq!(s.expand_all(&c), "x [Pasted 99 lines] y");
}

#[test]
fn a_backspace_or_delete_at_a_token_takes_the_whole_token() {
    let mut s = PasteStore::new();
    let t = token_of(s.offer("1\n2\n3"));
    let n = t.chars().count();
    let text = format!("é{t}x");
    assert_eq!(s.token_ending_at(&text, 1 + n), Some(1..1 + n));
    assert_eq!(s.token_starting_at(&text, 1), Some(1..1 + n));
    assert_eq!(
        s.token_ending_at(&text, n),
        None,
        "inside the token is not its end"
    );
    assert_eq!(s.token_starting_at(&text, 2), None);
    assert_eq!(s.token_ending_at("plain", 3), None);
    s.clear();
    assert_eq!(
        s.token_ending_at(&text, 1 + n),
        None,
        "a token that is not live is text"
    );
}

proptest! {
    #[test]
    fn a_placeholder_expands_to_exactly_what_was_pasted(text in "(?s)[a-z \\n]{0,400}") {
        let mut s = PasteStore::new();
        match s.offer(&text) {
            Pasted::Inline(t) => prop_assert_eq!(t, text),
            Pasted::Placeholder(tok) => {
                prop_assert_eq!(s.expand_all(&tok), text.clone());
                prop_assert_eq!(s.expand_all(&format!("<{tok}>")), format!("<{text}>"));
            }
        }
    }

    #[test]
    fn any_paste_never_panics_and_leaves_no_control_character(text in "\\PC{0,300}|[\\x00-\\x1f\\x7f]{0,40}") {
        let mut s = PasteStore::new();
        let shown = match s.offer(&text) {
            Pasted::Inline(t) => t,
            Pasted::Placeholder(t) => {
                prop_assert!(t.starts_with("[Pasted "));
                s.expand_all(&t)
            }
        };
        prop_assert!(shown.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'));
        let _ = s.expand_at(&shown, 3);
        let _ = s.placeholders_in(&shown);
    }
}
