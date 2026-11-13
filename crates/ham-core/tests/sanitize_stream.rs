//! The sanitizer on streams and on bidirectional-text controls:
//! `ham shell` output arrives in frames that may split an escape sequence or a
//! UTF-8 character, and the stream filter must give the same text as filtering the whole output
//! at once. Bidirectional overrides and isolates (Trojan Source, CVE-2021-42574) are removed too,
//! so agent text cannot reorder what the terminal shows.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::sanitize::{StreamSanitizer, sanitize, sanitize_line};
use proptest::prelude::*;

const BIDI: [char; 9] = [
    '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}',
];

#[test]
fn bidirectional_overrides_and_isolates_are_removed() {
    for c in BIDI {
        assert_eq!(sanitize(&format!("a{c}b")), "ab", "{:#x}", c as u32);
    }
    assert_eq!(
        sanitize_line("access \u{202e}nimda\u{202c} granted"),
        "access nimda granted"
    );
    // Other format characters that render nothing harmful stay (zero-width joiner in emoji).
    assert_eq!(sanitize("👩\u{200d}💻"), "👩\u{200d}💻");
}

#[test]
fn a_sequence_split_across_frames_is_removed_whole() {
    let mut s = StreamSanitizer::default();
    let mut out = String::new();
    for frame in [
        &b"ls \x1b]5"[..],
        b"2;c;ZXZp",
        b"bA==\x07ok \x1b[3",
        b"1mred\x1b",
        b"[0m done\r\n",
    ] {
        out.push_str(&s.push(frame));
    }
    out.push_str(&s.finish());
    assert_eq!(out, "ls ok red done\n");
}

#[test]
fn a_utf8_character_split_across_frames_is_kept() {
    let text = "数据 🚀 é";
    let bytes = text.as_bytes();
    let mut s = StreamSanitizer::default();
    let mut out = String::new();
    for b in bytes {
        out.push_str(&s.push(std::slice::from_ref(b)));
    }
    out.push_str(&s.finish());
    assert_eq!(out, text);
}

#[test]
fn invalid_utf8_becomes_the_replacement_character_and_an_unterminated_string_is_bounded() {
    let mut s = StreamSanitizer::default();
    assert_eq!(s.push(b"a\xffb"), "a\u{fffd}b");
    // An OSC that never ends is held only up to the limit, then dropped; text after it shows.
    let mut out = s.push(b"\x1b]0;");
    out.push_str(&s.push(&vec![b'A'; StreamSanitizer::MAX_HELD + 10]));
    out.push_str(&s.push(b"\x07after"));
    out.push_str(&s.finish());
    assert!(!out.contains('A'), "{out:?}");
    assert!(out.ends_with("after"), "{out:?}");
}

proptest! {
    #[test]
    fn streaming_equals_sanitizing_the_whole(s in any::<String>(), cuts in proptest::collection::vec(0usize..64, 0..12)) {
        let bytes = s.as_bytes();
        let mut points: Vec<usize> = cuts.iter().map(|c| c * bytes.len() / 64).collect();
        points.sort_unstable();
        let mut st = StreamSanitizer::default();
        let mut out = String::new();
        let mut last = 0;
        for p in points {
            out.push_str(&st.push(&bytes[last..p.max(last)]));
            last = p.max(last);
        }
        out.push_str(&st.push(&bytes[last..]));
        out.push_str(&st.finish());
        prop_assert_eq!(out, sanitize(&s));
    }

    #[test]
    fn stream_output_never_holds_escapes_controls_or_bidi(b in proptest::collection::vec(any::<u8>(), 0..400)) {
        let mut st = StreamSanitizer::default();
        let mut out = st.push(&b);
        out.push_str(&st.finish());
        for c in out.chars() {
            let n = c as u32;
            prop_assert!(!((n < 0x20 && c != '\n' && c != '\t') || n == 0x7f || (0x80..=0x9f).contains(&n)));
            prop_assert!(!BIDI.contains(&c));
        }
    }
}

// ---- Sol's finding: a string terminator lost when the held sequence is cut ----------------------------

/// What the whole input gives at once.
fn whole(input: &str) -> String {
    sanitize(input)
}

/// What the stream gives for the frames, then the end of the stream.
fn framed(frames: &[&[u8]]) -> String {
    let mut s = StreamSanitizer::default();
    let mut out = String::new();
    for f in frames {
        out.push_str(&s.push(f));
    }
    out.push_str(&s.finish());
    out
}

#[test]
fn the_escape_of_a_terminator_that_ends_a_frame_past_the_held_limit_is_not_dropped() {
    // A string sequence (an OSC) with a body longer than the stream sanitizer keeps; the frame ends on the ESC that starts the ST
    // terminator (ESC \), and the backslash and the text that follows arrive in the next frame.
    let body = "x".repeat(StreamSanitizer::MAX_HELD + 10);
    let first = format!("before \u{1b}]0;{body}\u{1b}");
    let expected = whole(&format!("before \u{1b}]0;{body}\u{1b}\\after"));
    assert_eq!(expected, "before after");
    assert_eq!(framed(&[first.as_bytes(), b"\\after"]), expected);
}

#[test]
fn the_same_holds_when_the_body_arrives_in_several_frames() {
    let body = "y".repeat(StreamSanitizer::MAX_HELD * 2 + 7);
    let (a, b) = body.split_at(StreamSanitizer::MAX_HELD + 3);
    let f1 = format!("\u{1b}P1;{a}");
    let f2 = format!("{b}\u{1b}");
    let expected = whole(&format!("\u{1b}P1;{body}\u{1b}\\tail"));
    assert_eq!(expected, "tail");
    assert_eq!(framed(&[f1.as_bytes(), f2.as_bytes(), b"\\tail"]), expected);
}

#[test]
fn a_bell_terminator_after_a_long_body_still_ends_the_sequence() {
    let body = "z".repeat(StreamSanitizer::MAX_HELD + 50);
    let f1 = format!("\u{1b}]52;c;{body}");
    assert_eq!(framed(&[f1.as_bytes(), b"\x07done"]), "done");
}
