//! Escape sanitizer: every agent-originated string is rendered
//! after stripping ESC sequences and C0/C1 controls, including OSC 52 and OSC 8.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::sanitize::{sanitize, sanitize_line};
use proptest::prelude::*;

fn is_forbidden(c: char) -> bool {
    let n = c as u32;
    (n < 0x20 && c != '\n' && c != '\t') || n == 0x7f || (0x80..=0x9f).contains(&n)
}

#[test]
fn plain_text_passes_unchanged() {
    assert_eq!(
        sanitize("Implementing percent_change (a, b) -> 10.0%"),
        "Implementing percent_change (a, b) -> 10.0%"
    );
    assert_eq!(sanitize("wide ✓ 日本語 émoji 🚀"), "wide ✓ 日本語 émoji 🚀");
}

#[test]
fn osc52_clipboard_write_is_removed() {
    assert_eq!(sanitize("a\u{1b}]52;c;aGk=\u{7}b"), "ab");
    assert_eq!(sanitize("a\u{1b}]52;c;aGk=\u{1b}\\b"), "ab");
}

#[test]
fn osc8_hyperlink_keeps_only_the_visible_text() {
    let s = "Open \u{1b}]8;;https://example.test/pr\u{7}the PR\u{1b}]8;;\u{7} now";
    assert_eq!(sanitize(s), "Open the PR now");
}

#[test]
fn csi_and_two_byte_escapes_are_removed() {
    assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m \u{1b}[2J\u{1b}[H"), "red ");
    assert_eq!(sanitize("x\u{1b}cy\u{1b}(Bz"), "xyz");
}

#[test]
fn c1_controls_and_their_sequences_are_removed() {
    // U+009B is the 8-bit CSI, U+009D the 8-bit OSC.
    assert_eq!(sanitize("a\u{9b}31mb"), "ab");
    assert_eq!(sanitize("a\u{9d}0;t\u{7}b"), "ab");
    assert_eq!(sanitize("a\u{85}b\u{9f}c"), "abc");
}

#[test]
fn c0_controls_except_newline_and_tab_are_removed() {
    assert_eq!(
        sanitize("a\u{7}b\u{8}c\u{0}d\re\u{7f}f\ng\th"),
        "abcdef\ng\th"
    );
}

#[test]
fn unterminated_sequences_are_dropped_to_the_end() {
    assert_eq!(sanitize("safe\u{1b}]0;never-terminated"), "safe");
    assert_eq!(sanitize("safe\u{1b}"), "safe");
    assert_eq!(sanitize("safe\u{1b}[12"), "safe");
}

#[test]
fn sanitize_line_flattens_newlines_and_tabs() {
    assert_eq!(sanitize_line("one\ntwo\tthree\r\n"), "one two three ");
}

proptest! {
    #[test]
    fn output_never_holds_escapes_or_controls(s in any::<String>()) {
        let out = sanitize(&s);
        prop_assert!(!out.chars().any(is_forbidden), "{:?} -> {:?}", s, out);
        prop_assert!(!out.contains('\u{1b}'), "ESC left in {:?}", out);
    }

    #[test]
    fn sanitize_is_idempotent(s in any::<String>()) {
        let once = sanitize(&s);
        prop_assert_eq!(sanitize(&once), once);
    }

    #[test]
    fn printable_text_is_preserved(s in "[ -~]{0,200}") {
        prop_assert_eq!(sanitize(&s), s);
    }

    #[test]
    fn line_output_is_single_line(s in any::<String>()) {
        let out = sanitize_line(&s);
        prop_assert!(!out.contains('\n') && !out.contains('\t'));
        prop_assert!(!out.chars().any(is_forbidden));
    }
}

// ---- Sol's finding: wrapping a very long word was quadratic -----------------------------------------

#[test]
fn a_very_long_word_wraps_in_linear_time() {
    // Counting the characters of what is left after every cut of 80 made this quadratic: the work grows with the square of the length.
    let word = "x".repeat(4_000_000);
    let start = std::time::Instant::now();
    let lines = ham_core::sanitize::sanitize_block(&word, 8, 80);
    let took = start.elapsed();
    assert_eq!(lines.len(), 8);
    assert!(
        lines[..7].iter().all(|l| l.chars().count() == 80),
        "{lines:?}"
    );
    assert!(
        took < std::time::Duration::from_millis(1_500),
        "wrapping a 4,000,000 character word took {took:?}"
    );
}

#[test]
fn a_long_word_is_cut_where_it_always_was() {
    let lines = ham_core::sanitize::sanitize_block("aaaaaaaaaaaaaaaaaaaaaaaaa bb", 10, 10);
    assert_eq!(lines, ["aaaaaaaaaa", "aaaaaaaaaa", "aaaaa bb"]);
    // Wide characters count as one character each here, as before.
    let lines = ham_core::sanitize::sanitize_block("ééééééééééé", 10, 4);
    assert_eq!(lines, ["éééé", "éééé", "ééé"]);
}
