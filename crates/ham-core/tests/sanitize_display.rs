//! terminal-safe rendering of engine text. `sanitize::display_text` is the sanitizer the transcript uses: it strips every
//! C0 and C1 control except newline and tab (carriage return, backspace, bell, DEL and the 8-bit forms included), every escape
//! sequence (CSI, OSC with 8 and 52, DCS, SOS, PM, APC), and shows bidirectional controls, zero-width and other invisible format
//! characters as visible `<U+XXXX>` markers. URLs stay plain text. `sanitize::sanitize` (single-line cells, titles) is unchanged.

#![allow(clippy::unwrap_used)]

use ham_core::sanitize::{COMBINING_MAX, display_line, display_text, marker, sanitize};
use proptest::prelude::*;

const BIDI: [char; 9] = [
    '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}',
];
const ZERO_WIDTH: [char; 7] = [
    '\u{200b}', '\u{200e}', '\u{200f}', '\u{2060}', '\u{feff}', '\u{61c}', '\u{ad}',
];

#[test]
fn bidi_controls_become_visible_markers() {
    for c in BIDI {
        let want = format!("a<U+{:04X}>b", c as u32);
        assert_eq!(display_text(&format!("a{c}b")), want, "{c:?}");
    }
}

#[test]
fn zero_width_and_invisible_format_characters_become_visible_markers() {
    for c in ZERO_WIDTH {
        let want = format!("a<U+{:04X}>b", c as u32);
        assert_eq!(display_text(&format!("a{c}b")), want, "{c:?}");
    }
    for c in [
        '\u{2061}', '\u{2064}', '\u{206a}', '\u{206f}', '\u{180e}', '\u{fff9}', '\u{fffb}',
    ] {
        let want = format!("x<U+{:04X}>y", c as u32);
        assert_eq!(display_text(&format!("x{c}y")), want, "{c:?}");
    }
}

#[test]
fn unicode_tag_characters_that_smuggle_ascii_are_shown() {
    // U+E0041 is the tag form of "A"; models read it, people do not see it.
    assert_eq!(
        display_text("ok\u{e0049}\u{e0067}\u{e006e}\u{e006f}\u{e0072}\u{e0065}"),
        "ok<U+E0049><U+E0067><U+E006E><U+E006F><U+E0072><U+E0065>"
    );
}

#[test]
fn the_single_line_sanitizer_keeps_deleting_them() {
    assert_eq!(sanitize("a\u{202e}b\u{200b}c"), "ab\u{200b}c");
}

#[test]
fn marker_names_the_characters_it_marks_and_only_those() {
    assert_eq!(marker('\u{202e}').as_deref(), Some("<U+202E>"));
    assert_eq!(marker('\u{200b}').as_deref(), Some("<U+200B>"));
    assert_eq!(marker('a'), None);
    assert_eq!(marker('\u{1f600}'), None);
    // The text and emoji selectors can be marked; display_text keeps one right after a symbol (see its own test).
    assert_eq!(marker('\u{fe0f}').as_deref(), Some("<U+FE0F>"));
}

#[test]
fn every_c0_and_c1_control_goes_except_newline_and_tab() {
    for n in (0u32..0x20).chain(0x7f..=0x9f) {
        let c = char::from_u32(n).unwrap();
        let out = display_text(&format!("a{c}b"));
        if c == '\n' || c == '\t' {
            assert_eq!(out, format!("a{c}b"), "U+{n:04X}");
        } else if c == '\u{1b}' || c == '\u{9b}' || c == '\u{9d}' || c == '\u{90}' {
            // A lone introducer opens a sequence and swallows the rest of the input.
            assert!(!out.contains(c), "U+{n:04X}");
        } else {
            assert_eq!(out, "ab", "U+{n:04X}");
        }
    }
}

#[test]
fn carriage_returns_and_backspaces_cannot_overwrite_what_the_reader_sees() {
    assert_eq!(display_text("good\rNEW"), "goodNEW");
    assert_eq!(display_text("good\r\nnext"), "good\nnext");
    assert_eq!(display_text("abc\u{8}\u{8}\u{8}xyz"), "abcxyz");
}

#[test]
fn escape_sequences_leave_only_their_visible_text() {
    assert_eq!(display_text("\u{1b}[31mred\u{1b}[0m"), "red");
    assert_eq!(display_text("a\u{1b}]52;c;aGk=\u{7}b"), "ab");
    assert_eq!(display_text("a\u{1b}]52;c;aGk=\u{1b}\\b"), "ab");
    assert_eq!(
        display_text("\u{1b}]8;;https://example.test/\u{7}click me\u{1b}]8;;\u{7}"),
        "click me"
    );
    assert_eq!(display_text("a\u{1b}Pq#0;2;0;0;0\u{1b}\\b"), "ab");
    assert_eq!(display_text("a\u{9d}52;c;aGk=\u{9c}b"), "ab");
    assert_eq!(display_text("a\u{9b}31mred"), "ared");
    assert_eq!(display_text("a\u{1b}[2J\u{1b}[Hb"), "ab");
}

#[test]
fn urls_stay_plain_text() {
    let t = "see https://example.com/a/b?c=1&d=2#frag and ssh://git@host/repo.git ok";
    assert_eq!(display_text(t), t);
}

#[test]
fn line_and_paragraph_separators_become_newlines() {
    assert_eq!(display_text("a\u{2028}b\u{2029}c"), "a\nb\nc");
}

#[test]
fn a_run_of_combining_marks_is_cut_to_the_cap() {
    let zalgo: String = std::iter::once('a')
        .chain(std::iter::repeat_n('\u{0301}', 40))
        .chain(std::iter::once('b'))
        .collect();
    let out = display_text(&zalgo);
    assert_eq!(
        out.chars().filter(|c| *c == '\u{0301}').count(),
        COMBINING_MAX
    );
    assert!(out.starts_with('a') && out.ends_with('b'));
}

#[test]
fn ordinary_text_is_kept_as_it_is() {
    for t in [
        "plain ascii, tabs\tand newlines\nkept",
        "caf\u{e9} \u{4f60}\u{597d} \u{1f600}",
        "e\u{0301} (one combining mark)",
        "\u{2713} done \u{b7} 12 files",
        // An emoji family is joined with U+200D, and Persian and Indic words use U+200C and U+200D between letters.
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
        "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}",
    ] {
        assert_eq!(display_text(t), t);
    }
}

#[test]
fn a_joiner_between_ascii_letters_is_not_part_of_a_script_and_is_shown() {
    assert_eq!(display_text("a\u{200d}b"), "a<U+200D>b");
    assert_eq!(display_text("a\u{200c}b"), "a<U+200C>b");
    assert_eq!(display_text("\u{200d}\u{1f468}"), "<U+200D>\u{1f468}");
    assert_eq!(display_text("\u{1f468}\u{200d}"), "\u{1f468}<U+200D>");
    // A joiner next to another invisible character is shown, so the output stays the same when it is read again.
    assert_eq!(
        display_text("\u{e9}\u{200d}\u{202e}"),
        "\u{e9}<U+200D><U+202E>"
    );
}

#[test]
fn display_line_is_one_line() {
    assert_eq!(display_line("a\nb\tc\r\nd"), "a b c d");
    assert_eq!(display_line("x\u{202e}y"), "x<U+202E>y");
}

/// Scenario T63: a tool block whose output holds text that looks like the client's own chrome and terminal control sequences.
#[test]
fn the_control_sequence_sample_shows_as_text_and_nothing_acts() {
    let raw = "Running tests...\r\u{1b}[2K\u{1b}[31mFAILED\u{1b}[0m\n\
               Press Enter to finish\n\
               /cancel\n\
               ham push 01ABC git@example.com:x.git\n\
               \u{1b}]52;c;aGk=\u{7}\n\
               \u{1b}]8;;https://example.test/\u{7}click me\u{1b}]8;;\u{7}\n\
               user\u{202e}gnp.exe\n\
               erase\u{8}\u{8}\u{8}d\n\
               bel\u{7} nul\u{0} 8bit\u{9b}31m\n";
    let got = display_text(raw);
    assert_eq!(
        got,
        "Running tests...FAILED\n\
         Press Enter to finish\n\
         /cancel\n\
         ham push 01ABC git@example.com:x.git\n\
         \n\
         click me\n\
         user<U+202E>gnp.exe\n\
         erased\n\
         bel nul 8bit\n"
    );
}

const ESC: char = '\u{1b}';

fn forbidden(c: char) -> bool {
    let n = c as u32;
    (n < 0x20 && c != '\n' && c != '\t') || (0x7f..=0x9f).contains(&n)
}

/// A character the output must not hold: marked ones except the two joiners and the two text/emoji selectors (each checked on its
/// own), and the separators.
fn invisible(c: char) -> bool {
    (marker(c).is_some() && !is_joiner(c) && !is_selector(c))
        || matches!(c, '\u{2028}' | '\u{2029}')
}

fn is_selector(c: char) -> bool {
    matches!(c, '\u{fe0e}' | '\u{fe0f}')
}

fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}')
}

/// The scripts and symbols where U+200C, U+200D and the emoji selectors shape text: emoji and symbols (not the Braille block), Arabic and
/// Persian, the Indic and South-East Asian scripts.
fn shaping_script(c: char) -> bool {
    matches!(c as u32,
        0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff
        | 0x0900..=0x0dff | 0x0e00..=0x0eff | 0x0f00..=0x0fff | 0x1000..=0x109f | 0x1780..=0x17ff
        | 0x00a9 | 0x00ae | 0x203c | 0x2049 | 0x2122 | 0x2139 | 0x2190..=0x27ff | 0x2900..=0x2bff
        | 0x3030 | 0x303d | 0x3297 | 0x3299 | 0x1f000..=0x1faff)
}

/// A joiner that stays has a character of a shaping script on both sides, and that character is not itself invisible.
fn joiners_have_script_neighbours(out: &str) -> bool {
    let cs: Vec<char> = out.chars().collect();
    cs.iter().enumerate().all(|(i, c)| {
        !is_joiner(*c)
            || (i > 0
                && i + 1 < cs.len()
                && shaping_script(cs[i - 1])
                && shaping_script(cs[i + 1])
                && !is_joiner(cs[i - 1])
                && !is_joiner(cs[i + 1]))
    })
}

/// A selector that stays follows a character of a shaping script that is not a selector or a joiner.
fn selectors_follow_a_symbol(out: &str) -> bool {
    let cs: Vec<char> = out.chars().collect();
    cs.iter().enumerate().all(|(i, c)| {
        !is_selector(*c)
            || (i > 0
                && shaping_script(cs[i - 1])
                && !is_selector(cs[i - 1])
                && !is_joiner(cs[i - 1]))
    })
}

#[test]
fn eight_bit_sos_pm_and_apc_strings_go_with_their_payload() {
    for intro in ['\u{98}', '\u{9e}', '\u{9f}', '\u{90}'] {
        assert_eq!(
            display_text(&format!("a{intro}HIDDEN\u{9c}b")),
            "ab",
            "{intro:?}"
        );
    }
}

#[test]
fn bel_ends_an_osc_string_and_no_other_string() {
    assert_eq!(display_text("\u{1b}]0;title\u{7}tail"), "tail");
    // DCS, SOS, PM and APC end at ST only; a BEL inside them is payload.
    for intro in ["\u{1b}P", "\u{1b}X", "\u{1b}^", "\u{1b}_"] {
        assert_eq!(
            display_text(&format!("{intro}abc\u{7}HIDDEN\u{1b}\\tail")),
            "tail",
            "{intro:?}"
        );
    }
    // An unfinished one hides the rest of the text, as an unfinished OSC does.
    assert_eq!(display_text("a\u{1b}Pabc\u{7}HIDDEN"), "a");
}

#[test]
fn escape_sequences_with_intermediate_bytes_go_whole() {
    assert_eq!(display_text("\u{1b}$(BVISIBLE"), "VISIBLE");
    assert_eq!(display_text("\u{1b}(0x"), "x");
    assert_eq!(display_text("\u{1b}#8x"), "x");
    assert_eq!(display_text("\u{1b} Fx"), "x");
    assert_eq!(display_text("\u{1b}%Gx"), "x");
    assert_eq!(display_text("\u{1b}=x"), "x");
}

#[test]
fn more_invisible_characters_are_shown() {
    let more: Vec<char> = [
        0x115f, 0x1160, 0x3164, 0xffa0, 0x2800, 0x34f, 0x17b4, 0x17b5, 0x180b, 0x180c, 0x180d,
        0x180f, 0xfe00, 0xfe0d, 0xe0100, 0xe01ef, 0x1d173, 0x1d17a, 0x13430,
    ]
    .iter()
    .map(|n| char::from_u32(*n).unwrap())
    .collect();
    for c in more {
        let want = format!("a<U+{:04X}>b", c as u32);
        assert_eq!(display_text(&format!("a{c}b")), want, "{c:?}");
        assert!(marker(c).is_some(), "{c:?}");
    }
}

#[test]
fn a_run_of_combining_grapheme_joiners_is_shown_not_cut() {
    let out = display_text(&format!("a{}", "\u{34f}".repeat(4)));
    assert_eq!(out, format!("a{}", "<U+034F>".repeat(4)));
}

#[test]
fn a_variation_selector_stays_only_right_after_a_symbol_and_only_once() {
    assert_eq!(display_text("\u{2764}\u{fe0f}"), "\u{2764}\u{fe0f}");
    assert_eq!(display_text("\u{2714}\u{fe0e}"), "\u{2714}\u{fe0e}");
    assert_eq!(display_text("a\u{fe0f}"), "a<U+FE0F>");
    assert_eq!(
        display_text("\u{2764}\u{fe0f}\u{fe0f}"),
        "\u{2764}\u{fe0f}<U+FE0F>"
    );
    assert_eq!(display_text("a\u{301}\u{fe0f}"), "a\u{301}<U+FE0F>");
    assert_eq!(display_text("\u{fe0f}"), "<U+FE0F>");
}

#[test]
fn joiners_stay_only_in_the_scripts_that_use_them() {
    // Latin, CJK and Braille blanks carry no shaping: a joiner there is hidden data.
    assert_eq!(display_text("\u{e9}\u{200d}\u{f1}"), "\u{e9}<U+200D>\u{f1}");
    assert_eq!(display_text("\u{e9}\u{200c}\u{f1}"), "\u{e9}<U+200C>\u{f1}");
    assert_eq!(
        display_text("\u{4f60}\u{200d}\u{597d}"),
        "\u{4f60}<U+200D>\u{597d}"
    );
    assert_eq!(
        display_text("\u{2800}\u{200d}\u{2800}"),
        "<U+2800><U+200D><U+2800>"
    );
    // Emoji, Persian and Devanagari keep theirs.
    assert_eq!(
        display_text("\u{1f468}\u{200d}\u{1f469}"),
        "\u{1f468}\u{200d}\u{1f469}"
    );
    assert_eq!(
        display_text("\u{645}\u{6cc}\u{200c}\u{62e}"),
        "\u{645}\u{6cc}\u{200c}\u{62e}"
    );
    assert_eq!(
        display_text("\u{915}\u{94d}\u{200d}\u{937}"),
        "\u{915}\u{94d}\u{200d}\u{937}"
    );
}

#[test]
fn hebrew_and_arabic_marks_are_capped_and_a_selector_cannot_reset_the_cap() {
    let hebrew = display_text(&format!("a{}", "\u{591}".repeat(1000)));
    assert_eq!(
        hebrew.chars().filter(|c| *c == '\u{591}').count(),
        COMBINING_MAX
    );
    let arabic = display_text(&format!("a{}", "\u{64b}".repeat(1000)));
    assert_eq!(
        arabic.chars().filter(|c| *c == '\u{64b}').count(),
        COMBINING_MAX
    );
    let mixed = display_text(&format!("a{}", "\u{301}\u{fe0f}".repeat(1000)));
    assert!(mixed.chars().filter(|c| *c == '\u{301}').count() <= COMBINING_MAX);
}

proptest! {
    #[test]
    fn no_control_character_and_no_escape_survives(s in ".{0,200}") {
        let out = display_text(&s);
        prop_assert!(!out.chars().any(forbidden), "{:?}", out);
        prop_assert!(!out.contains(ESC));
        prop_assert!(!out.chars().any(invisible), "{:?}", out);
        prop_assert!(joiners_have_script_neighbours(&out), "{:?}", out);
        prop_assert!(selectors_follow_a_symbol(&out), "{:?}", out);
    }

    #[test]
    fn nasty_alphabets_are_safe_too(parts in prop::collection::vec(prop::sample::select(vec![
        "\u{1b}", "\u{1b}[", "\u{1b}]0;", "\u{7}", "\u{9c}", "\u{9b}", "\u{9d}", "\r", "\u{8}", "\u{0}", "\u{202e}",
        "\u{200b}", "\u{200d}", "\u{200c}", "\u{fe0f}", "\u{fe00}", "\u{34f}", "\u{2800}", "\u{98}", "\u{9e}", "\u{9f}", "\u{591}", "\u{1b}P", "\u{1b}$(B", "\u{2764}", "\u{1f468}", "\u{645}", "\u{feff}", "\u{e0041}", "\u{301}", "\u{2028}", "\u{1b}\\", "\u{1b}P", "x", "é", "😀", "\n", "\t", "/cancel",
        "Press Enter", "https://e.example/a",
    ]), 0..60)) {
        let s: String = parts.concat();
        let out = display_text(&s);
        prop_assert!(!out.chars().any(forbidden), "{:?} -> {:?}", s, out);
        prop_assert!(!out.contains(ESC));
        prop_assert!(!out.chars().any(invisible), "{:?}", out);
        prop_assert!(joiners_have_script_neighbours(&out), "{:?}", out);
        prop_assert!(selectors_follow_a_symbol(&out), "{:?}", out);
    }

    #[test]
    fn display_text_is_idempotent(s in ".{0,200}") {
        let once = display_text(&s);
        prop_assert_eq!(display_text(&once), once);
    }

    #[test]
    fn display_line_has_no_line_break_at_all(s in ".{0,200}") {
        let out = display_line(&s);
        prop_assert!(!out.contains(['\n', '\t', '\r']));
        prop_assert!(!out.chars().any(forbidden));
    }
}
