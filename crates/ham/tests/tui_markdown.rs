//! Markdown, code and diff rendering of transcript blocks as styled lines (`tui::markdown`). The module is a small parser
//! of the Markdown that models write (headings, paragraphs, emphasis, code spans and fences, quotes, nested lists, tables, rules, links)
//! and of unified diffs with word-level emphasis, with no new crate. Text is cleaned first (`display_text`), so no span ever holds an
//! escape byte or a control character; a link is its text followed by its address in parentheses, never a hyperlink; every row fits
//! the width by display cells; the work done is counted, never timed.
#![allow(clippy::unwrap_used)]

use ham::tui::markdown::{
    MAX_ROWS, Row, is_unified_diff, render, render_block, render_counted, render_diff, unified_diff,
};
use ham::tui::select::Join;
use ham_core::chat::{Block, GapCause, Origin, Role};
use ham_core::notice::NoticeKind;
use proptest::prelude::*;
use ratatui::style::{Color, Modifier, Style};

// ---- helpers ----------------------------------------------------------------------------------------------------------------------

fn text_of(r: &Row) -> String {
    r.line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn plain(rows: &[Row]) -> Vec<String> {
    rows.iter().map(text_of).collect()
}

fn joins(rows: &[Row]) -> Vec<Join> {
    rows.iter().map(|r| r.join).collect()
}

fn md(src: &str, width: usize) -> Vec<String> {
    plain(&render(src, width))
}

fn flags(st: Style) -> String {
    let mut v: Vec<String> = Vec::new();
    for (m, n) in [
        (Modifier::BOLD, "b"),
        (Modifier::ITALIC, "i"),
        (Modifier::UNDERLINED, "u"),
        (Modifier::CROSSED_OUT, "s"),
        (Modifier::REVERSED, "r"),
        (Modifier::DIM, "d"),
    ] {
        if st.add_modifier.contains(m) {
            v.push(n.to_string());
        }
    }
    if let Some(c) = st.fg {
        v.push(format!("{c:?}").to_lowercase());
    }
    v.join(",")
}

/// One line per row: a two-character join tag (`  ` new line, `␣ ` wrapped at a space, `⌁ ` cut inside a word), then the spans, each
/// styled span as `⟦flags:text⟧`.
fn dump(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| {
            let tag = match r.join {
                Join::Hard => "  ",
                Join::Space => "\u{2423} ",
                Join::Cut => "\u{232c} ",
            };
            let body: String = r
                .line
                .spans
                .iter()
                .map(|s| {
                    let f = flags(s.style);
                    if f.is_empty() {
                        s.content.to_string()
                    } else {
                        format!("\u{27e6}{f}:{}\u{27e7}", s.content)
                    }
                })
                .collect();
            format!("{tag}{body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn style_of(rows: &[Row], text: &str) -> Style {
    rows.iter()
        .flat_map(|r| r.line.spans.iter())
        .find(|s| s.content == text)
        .unwrap_or_else(|| panic!("no span {text:?} in\n{}", dump(rows)))
        .style
}

fn has(st: Style, m: Modifier) -> bool {
    st.add_modifier.contains(m)
}

fn alnum_sorted(s: &str) -> Vec<char> {
    let mut v: Vec<char> = s.chars().filter(|c| c.is_alphanumeric()).collect();
    v.sort_unstable();
    v
}

// ---- paragraphs and wrapping ------------------------------------------------------------------------------------------------------

#[test]
fn source_line_breaks_stay_line_breaks() {
    let r = render("one\ntwo", 40);
    assert_eq!(plain(&r), ["one", "two"]);
    assert_eq!(joins(&r), [Join::Hard, Join::Hard]);
    assert_eq!(md("a  \nb", 40), ["a", "b"]);
    assert_eq!(md("a\\\nb", 40), ["a", "b"]);
}

#[test]
fn a_paragraph_wraps_at_spaces_and_says_how() {
    let r = render("alpha beta gamma delta", 12);
    assert_eq!(plain(&r), ["alpha beta", "gamma delta"]);
    assert_eq!(joins(&r), [Join::Hard, Join::Space]);
}

#[test]
fn a_word_longer_than_the_width_is_cut_and_a_long_word_starts_a_row() {
    let r = render("abcdefghijklmnop", 6);
    assert_eq!(plain(&r), ["abcdef", "ghijkl", "mnop"]);
    assert_eq!(joins(&r), [Join::Hard, Join::Cut, Join::Cut]);
    let u = render("see https://example.org/very/long/path end", 16);
    assert_eq!(
        plain(&u),
        ["see", "https://example.", "org/very/long/pa", "th end"]
    );
    assert_eq!(joins(&u), [Join::Hard, Join::Space, Join::Cut, Join::Cut]);
}

#[test]
fn blocks_are_separated_by_one_empty_row_and_the_ends_are_trimmed() {
    assert_eq!(md("a\n\nb", 40), ["a", "", "b"]);
    assert_eq!(md("a\n\n\n\n\nb", 40), ["a", "", "b"]);
    assert_eq!(md("\n\n  a  \n\n", 40), ["a"]);
    assert!(md("", 40).is_empty());
    assert!(md("  \n\t\n", 40).is_empty());
    assert!(md("anything", 0).is_empty());
}

#[test]
fn crlf_tabs_and_wide_characters() {
    assert_eq!(md("a\r\nb\r\n", 40), ["a", "b"]);
    assert_eq!(md("a\tb", 40), ["a b"]);
    assert_eq!(
        md("\u{4f60}\u{597d}\u{4e16}\u{754c}", 4),
        ["\u{4f60}\u{597d}", "\u{4e16}\u{754c}"]
    );
    assert_eq!(md("\u{4f60}\u{597d}", 3), ["\u{4f60}", "\u{597d}"]);
    // A character wider than the whole width cannot be shown: a marker takes its place.
    assert_eq!(md("\u{4f60}a", 1), ["\u{2026}", "a"]);
}

// ---- headings and rules -------------------------------------------------------------------------------------------------------------

#[test]
fn headings_lose_their_hashes_and_are_bold() {
    let r = render("# Title\n\n## Sub\ntext", 40);
    assert_eq!(plain(&r), ["Title", "", "Sub", "", "text"]);
    assert!(has(style_of(&r, "Title"), Modifier::BOLD));
    assert!(has(style_of(&r, "Title"), Modifier::UNDERLINED));
    assert!(has(style_of(&r, "Sub"), Modifier::BOLD));
    assert!(!has(style_of(&r, "Sub"), Modifier::UNDERLINED));
    assert_eq!(md("# closing ##", 40), ["closing"]);
    assert_eq!(md("####### seven", 40), ["####### seven"]);
    assert_eq!(md("#nospace", 40), ["#nospace"]);
    assert_eq!(md("###### six", 40), ["six"]);
}

#[test]
fn rules_are_a_dim_line() {
    let line = "\u{2500}".repeat(40);
    for src in ["---", "***", "___", "- - -", "* * *", "-----"] {
        let r = render(&format!("a\n\n{src}\n\nb"), 40);
        assert_eq!(plain(&r), ["a", "", line.as_str(), "", "b"], "{src}");
        assert!(has(r[2].line.spans[0].style, Modifier::DIM));
    }
    assert_eq!(md("---", 10), ["\u{2500}".repeat(10)]);
    assert_eq!(md("--", 40), ["--"]);
    // A rule straight under a line of text is a rule; there are no setext headings.
    assert_eq!(md("a\n---", 40), ["a", "", line.as_str()]);
}

// ---- inline -----------------------------------------------------------------------------------------------------------------------

#[test]
fn emphasis_strong_strike_and_code_get_their_styles() {
    let r = render("a *b* **c** ***d*** ~~e~~ _f_ __g__ h_i_j `k`", 80);
    assert_eq!(plain(&r), ["a b c d e f g h_i_j k"]);
    assert!(has(style_of(&r, "b"), Modifier::ITALIC));
    assert!(!has(style_of(&r, "b"), Modifier::BOLD));
    assert!(has(style_of(&r, "c"), Modifier::BOLD));
    let d = style_of(&r, "d");
    assert!(has(d, Modifier::BOLD) && has(d, Modifier::ITALIC));
    assert!(has(style_of(&r, "e"), Modifier::CROSSED_OUT));
    assert!(has(style_of(&r, "f"), Modifier::ITALIC));
    assert!(has(style_of(&r, "g"), Modifier::BOLD));
    let k = style_of(&r, "k");
    assert_eq!(k.fg, Some(Color::Yellow));
    // Snake case is not emphasis; neither is a lone star or a star between spaces.
    assert!(
        r[0].line
            .spans
            .iter()
            .any(|s| s.content.contains("h_i_j") && s.style == Style::default())
    );
}

#[test]
fn emphasis_nests_and_unmatched_markers_stay_visible() {
    let r = render("**a *b* c**", 40);
    assert_eq!(plain(&r), ["a b c"]);
    assert!(has(style_of(&r, "a "), Modifier::BOLD));
    let b = style_of(&r, "b");
    assert!(has(b, Modifier::BOLD) && has(b, Modifier::ITALIC));
    assert!(has(style_of(&r, " c"), Modifier::BOLD));
    let cases = [
        ("*a", "*a"),
        ("a*", "a*"),
        ("**bold", "**bold"),
        ("a * b * c", "a * b * c"),
        ("2 * 3", "2 * 3"),
        ("**a*", "*a"),
        ("snake_case_name", "snake_case_name"),
        ("_a_b", "_a_b"),
        ("**Note:**text", "**Note:**text"),
        ("**Bold**text", "Boldtext"),
        ("~~a", "~~a"),
        ("~a~", "~a~"),
    ];
    for (src, want) in cases {
        assert_eq!(md(src, 40), [want], "{src:?}");
    }
}

#[test]
fn code_spans() {
    let cases = [
        ("use `x` here", "use x here"),
        ("``a`b``", "a`b"),
        ("` a `", "a"),
        ("`a  b`", "a  b"),
        ("unmatched ` tick", "unmatched ` tick"),
        ("`*not em*`", "*not em*"),
        ("`[a](b)`", "[a](b)"),
        ("`` ` ``", "`"),
    ];
    for (src, want) in cases {
        assert_eq!(md(src, 40), [want], "{src:?}");
    }
    // A span may run over a line break; the break becomes a space.
    assert_eq!(md("`a\nb`", 40), ["a b"]);
}

#[test]
fn a_link_is_its_text_and_its_address_in_parentheses_and_never_a_hyperlink() {
    let cases = [
        ("[docs](https://x.y/z)", "docs (https://x.y/z)"),
        ("[a](http://u \"a title\")", "a (http://u)"),
        ("[a](http://x/(y))", "a (http://x/(y))"),
        ("![logo](http://u/l.png)", "[image: logo] (http://u/l.png)"),
        ("<https://x.y>", "https://x.y"),
        ("see [one](u1) and [two](u2)", "see one (u1) and two (u2)"),
        ("[**b**](u)", "b (u)"),
        ("[a]()", "a"),
        ("[a](b", "[a](b"),
        ("[x]", "[x]"),
        ("[x][ref]", "[x][ref]"),
        ("[ref]: https://x.y", "[ref]: https://x.y"),
        ("[a](javascript:alert(1))", "a (javascript:alert(1))"),
        ("https://bare.example/path", "https://bare.example/path"),
    ];
    for (src, want) in cases {
        assert_eq!(md(src, 60), [want], "{src:?}");
    }
    let r = render("[docs](https://x.y/z)", 40);
    assert!(has(style_of(&r, "docs"), Modifier::UNDERLINED));
    assert!(has(style_of(&r, " (https://x.y/z)"), Modifier::DIM));
    // No span carries an escape or a bell: a link is text.
    for row in &r {
        for s in &row.line.spans {
            assert!(!s.content.contains('\u{1b}') && !s.content.contains('\u{7}'));
        }
    }
}

#[test]
fn escapes_entities_and_html_are_shown_as_written() {
    assert_eq!(md("\\*lit\\* \\\\ \\a \\_x\\_", 40), ["*lit* \\ \\a _x_"]);
    assert_eq!(md("&amp; &lt; &#35;", 40), ["&amp; &lt; &#35;"]);
    assert_eq!(
        md("<b>x</b> and <!-- c -->", 40),
        ["<b>x</b> and <!-- c -->"]
    );
    assert_eq!(md("<div>\nhi\n</div>", 40), ["<div>", "hi", "</div>"]);
}

// ---- code blocks ------------------------------------------------------------------------------------------------------------------

#[test]
fn a_fence_keeps_its_lines_exactly_and_colours_them() {
    let r = render("```rust\nfn main() {\n    println!();\n}\n```", 40);
    assert_eq!(plain(&r), ["fn main() {", "    println!();", "}"]);
    for row in &r {
        assert_eq!(row.line.spans[0].style.fg, Some(Color::Yellow));
    }
    assert_eq!(md("```\na\n\nb\n```", 40), ["a", "", "b"]);
    assert_eq!(md("```\n\tx\n```", 40), ["    x"]);
    assert_eq!(md("  ```\n  x\n   y\n  ```", 40), ["x", " y"]);
    assert_eq!(md("~~~\na\n~~~", 40), ["a"]);
    assert_eq!(md("```\nabc", 40), ["abc"]);
    assert_eq!(
        md("before\n```\ncode\n```\nafter", 40),
        ["before", "", "code", "", "after"]
    );
}

#[test]
fn a_longer_fence_holds_shorter_ones_and_other_characters() {
    assert_eq!(
        md("````\n```\ninner\n```\n````", 40),
        ["```", "inner", "```"]
    );
    assert_eq!(md("````\nx\n```\ny\n````", 40), ["x", "```", "y"]);
    assert_eq!(md("```\nx\n~~~\ny\n```", 40), ["x", "~~~", "y"]);
    assert_eq!(
        md("```\nx\n``` trailing\ny\n```", 40),
        ["x", "``` trailing", "y"]
    );
    assert_eq!(md("```\nx\n```   \nafter", 40), ["x", "", "after"]);
    // A backtick fence's info string cannot hold a backtick: this is text.
    assert_eq!(md("```a`b", 40), ["```a`b"]);
}

#[test]
fn long_code_lines_are_cut_at_the_width() {
    let r = render("```\nabcdefghij\n```", 5);
    assert_eq!(plain(&r), ["abcde", "fghij"]);
    assert_eq!(joins(&r), [Join::Hard, Join::Cut]);
    // Spaces stay where they were: a code row is never re-flowed.
    let s = render("```\na  b  c  d\n```", 5);
    assert_eq!(plain(&s), ["a  b ", " c  d"]);
}

#[test]
fn an_indented_block_after_a_blank_line_is_code() {
    assert_eq!(
        md("text\n\n    code\n    more\n\nafter", 40),
        ["text", "", "code", "more", "", "after"]
    );
    // Four spaces straight under a paragraph line continue it.
    assert_eq!(md("text\n    more", 40), ["text", "more"]);
}

// ---- quotes -----------------------------------------------------------------------------------------------------------------------

#[test]
fn quotes() {
    let cases: [(&str, &[&str]); 9] = [
        ("> a\n> b", &["\u{2502} a", "\u{2502} b"]),
        ("> a\n>\n> b", &["\u{2502} a", "\u{2502}", "\u{2502} b"]),
        ("> > c", &["\u{2502} \u{2502} c"]),
        (
            "> - x\n> - y",
            &["\u{2502} \u{2022} x", "\u{2502} \u{2022} y"],
        ),
        ("> ```\n> code\n> ```", &["\u{2502} code"]),
        (
            "> # h\n> text",
            &["\u{2502} h", "\u{2502}", "\u{2502} text"],
        ),
        ("> a\n\nb", &["\u{2502} a", "", "b"]),
        ("> a\nb", &["\u{2502} a", "\u{2502} b"]),
        (">no space", &["\u{2502} no space"]),
    ];
    for (src, want) in cases {
        assert_eq!(md(src, 40), want, "{src:?}");
    }
    let r = render("> alpha beta gamma", 10);
    assert_eq!(
        plain(&r),
        ["\u{2502} alpha", "\u{2502} beta", "\u{2502} gamma"]
    );
    assert_eq!(joins(&r), [Join::Hard, Join::Space, Join::Space]);
    assert!(has(render("> a", 10)[0].line.spans[0].style, Modifier::DIM));
}

// ---- lists ------------------------------------------------------------------------------------------------------------------------

#[test]
fn lists() {
    let cases: [(&str, usize, &[&str]); 18] = [
        ("- a\n- b", 40, &["\u{2022} a", "\u{2022} b"]),
        ("* a\n+ b", 40, &["\u{2022} a", "\u{2022} b"]),
        ("1. a\n2. b", 40, &["1. a", "2. b"]),
        ("1) a\n2) b", 40, &["1) a", "2) b"]),
        ("3. start", 40, &["3. start"]),
        ("10. ten\n11. eleven", 40, &["10. ten", "11. eleven"]),
        (
            "* a\n  * b\n    * c",
            40,
            &["\u{2022} a", "  \u{25e6} b", "    \u{25aa} c"],
        ),
        (
            "- a\n  1. x\n  2. y",
            40,
            &["\u{2022} a", "  1. x", "  2. y"],
        ),
        ("1. a\n   - b", 40, &["1. a", "   \u{25e6} b"]),
        (
            "- alpha beta gamma delta",
            14,
            &["\u{2022} alpha beta", "  gamma delta"],
        ),
        (
            "- [ ] todo\n- [x] done",
            40,
            &["\u{2022} [ ] todo", "\u{2022} [x] done"],
        ),
        ("- a\n\npara", 40, &["\u{2022} a", "", "para"]),
        ("- a\n\n  b", 40, &["\u{2022} a", "", "  b"]),
        ("- a\n\n- b", 40, &["\u{2022} a", "\u{2022} b"]),
        ("-\n- b", 40, &["\u{2022}", "\u{2022} b"]),
        ("-a", 40, &["-a"]),
        ("- a\nb", 40, &["\u{2022} a", "  b"]),
        (
            "- item\n  ```\n  code\n  ```",
            40,
            &["\u{2022} item", "  code"],
        ),
    ];
    for (src, width, want) in cases {
        assert_eq!(md(src, width), want, "{src:?}");
    }
}

#[test]
fn what_models_write_in_lists() {
    // A numbered step with an indented fence, then the next step.
    assert_eq!(
        md(
            "1. Install it\n   ```bash\n   cargo build\n   ```\n2. Run it",
            60
        ),
        ["1. Install it", "   cargo build", "2. Run it"]
    );
    // A bold label, a colon and a sentence.
    let r = render("- **Name**: the user's name\n- **Age**: in years", 60);
    assert_eq!(
        plain(&r),
        ["\u{2022} Name: the user's name", "\u{2022} Age: in years"]
    );
    assert!(has(style_of(&r, "Name"), Modifier::BOLD));
    // A paragraph, a blank line, an indented continuation paragraph and a nested list.
    assert_eq!(
        md("- first\n\n  more of first\n\n  - inner\n- second", 60),
        [
            "\u{2022} first",
            "",
            "  more of first",
            "  \u{25e6} inner",
            "\u{2022} second"
        ]
    );
}

#[test]
fn a_list_can_interrupt_a_paragraph_but_only_a_list_that_starts_at_one_if_it_is_numbered() {
    assert_eq!(md("text\n- a", 40), ["text", "", "\u{2022} a"]);
    assert_eq!(md("text\n1. a", 40), ["text", "", "1. a"]);
    assert_eq!(md("text\n2. a", 40), ["text", "2. a"]);
}

#[test]
fn a_wrapped_item_hangs_under_its_text_and_says_how_it_wrapped() {
    let r = render("1. alpha beta gamma delta epsilon", 16);
    assert_eq!(plain(&r), ["1. alpha beta", "   gamma delta", "   epsilon"]);
    assert_eq!(joins(&r), [Join::Hard, Join::Space, Join::Space]);
}

// ---- tables -----------------------------------------------------------------------------------------------------------------------

#[test]
fn tables_are_aligned_pipe_tables_that_are_still_markdown_when_copied() {
    let r = render("| a | b |\n|---|---|\n| 1 | 2 |", 40);
    assert_eq!(plain(&r), ["| a | b |", "|---|---|", "| 1 | 2 |"]);
    assert!(has(style_of(&r, "a"), Modifier::BOLD));
    let aligned = md(
        "| name | n |\n|:-----|--:|\n| bob | 7 |\n| alexandra | 12 |",
        40,
    );
    assert_eq!(
        aligned,
        [
            "| name      |  n |",
            "|:----------|---:|",
            "| bob       |  7 |",
            "| alexandra | 12 |"
        ]
    );
    let center = md("| x |\n|:-:|\n| abc |", 40);
    assert_eq!(center, ["|  x  |", "|:---:|", "| abc |"]);
}

#[test]
fn table_cells_may_hold_emphasis_escaped_pipes_and_breaks_and_rows_may_be_ragged() {
    let r = render("| a \\| b | **c** |\n|---|---|\n| 1 | `x` |", 40);
    assert_eq!(
        plain(&r),
        ["| a | b | c |", "|-------|---|", "| 1     | x |"]
    );
    assert!(has(style_of(&r, "c"), Modifier::BOLD));
    let ragged = md("| a | b | c |\n|---|---|---|\n| 1 |\n| 1 | 2 | 3 | 4 |", 40);
    assert_eq!(
        ragged,
        [
            "| a | b | c |",
            "|---|---|---|",
            "| 1 |   |   |",
            "| 1 | 2 | 3 |"
        ]
    );
    let br = md("| a |\n|---|\n| x<br>y |", 40);
    assert_eq!(br, ["| a |", "|---|", "| x |", "| y |"]);
    let wide = md("| \u{540d}\u{524d} | n |\n|---|---|\n| a | 1 |", 40);
    assert_eq!(
        wide,
        ["| \u{540d}\u{524d} | n |", "|------|---|", "| a    | 1 |"]
    );
}

#[test]
fn a_table_needs_its_delimiter_row_and_can_interrupt_a_paragraph() {
    assert_eq!(md("| a | b |\n| 1 | 2 |", 40), ["| a | b |", "| 1 | 2 |"]);
    assert_eq!(md("a | b\nc | d", 40), ["a | b", "c | d"]);
    assert_eq!(
        md("intro\n| a | b |\n|---|---|\n| 1 | 2 |", 40),
        ["intro", "", "| a | b |", "|---|---|", "| 1 | 2 |"]
    );
    assert_eq!(md("| a |\n|---|", 40), ["| a |", "|---|"]);
    // The delimiter row has to match the header's cell count.
    assert_eq!(
        md("| a | b |\n|---|\n| 1 | 2 |", 40),
        ["| a | b |", "|---|", "| 1 | 2 |"]
    );
}

#[test]
fn a_table_wider_than_the_screen_wraps_inside_its_cells() {
    let src = "| name | description |\n|---|---|\n| alpha | the first letter of the greek alphabet |\n| beta | the second |";
    let rows = md(src, 30);
    assert!(rows.iter().all(|r| r.chars().count() <= 30), "{rows:#?}");
    insta::assert_snapshot!("table_wrapped_30", rows.join("\n"));
    // Nothing is lost.
    assert_eq!(alnum_sorted(&rows.join("\n")), alnum_sorted(src));
    // Too narrow even for that: the source lines are shown instead.
    let tiny = md(src, 8);
    assert!(tiny.iter().all(|r| r.chars().count() <= 8), "{tiny:#?}");
    assert_eq!(alnum_sorted(&tiny.join("\n")), alnum_sorted(src));
}

// ---- control sequences in text -----------------------------------------------------------------------------------------------------------------

#[test]
fn escape_sequences_never_reach_a_span() {
    assert_eq!(md("a\u{1b}]0;title\u{7}b\u{1b}[31mc", 40), ["abc"]);
    assert_eq!(md("[x](\u{1b}]8;;http://test\u{7}u)", 40), ["x (u)"]);
    assert_eq!(md("```\n\u{1b}]8;;http://e\u{7}t\n```", 40), ["t"]);
    assert_eq!(md("# h\u{1b}[2J", 40), ["h"]);
    assert_eq!(md("x\u{202e}y\u{200b}z", 40), ["x<U+202E>y<U+200B>z"]);
    assert_eq!(
        md("| a\u{1b}[0m |\n|---|\n| b |", 40),
        ["| a |", "|---|", "| b |"]
    );
}

#[test]
fn text_cannot_draw_a_role_word_a_border_or_a_prompt() {
    // The renderer returns text rows only; what looks like chrome in the text is just text.
    let r = md(
        "you hello\n\n\u{2502} a quoted line\n\nPress Enter to finish",
        40,
    );
    assert_eq!(
        r,
        [
            "you hello",
            "",
            "\u{2502} a quoted line",
            "",
            "Press Enter to finish"
        ]
    );
}

// ---- work and size ----------------------------------------------------------------------------------------------------------------

const PER_CHAR: u64 = 200;

fn work_of(src: &str, width: usize) -> u64 {
    render_counted(src, width).1
}

#[test]
fn pathological_shapes_cost_a_few_steps_a_character() {
    let n = 20_000;
    let shapes: Vec<(&str, String)> = vec![
        ("star spaces", "*a ".repeat(n / 3)),
        ("open brackets", "[".repeat(n)),
        ("brackets and parens", "[a](".repeat(n / 4)),
        ("backticks", "`".repeat(n)),
        ("alternating backticks", "`a``b```c".repeat(n / 9)),
        ("underscores", "_a".repeat(n / 2)),
        ("tildes", "~".repeat(n)),
        ("deep quotes", "> ".repeat(n / 2) + "x"),
        ("deep lists", "- ".repeat(n / 2) + "x"),
        ("many fences", "```\n".repeat(n / 4)),
        (
            "table of pipes",
            format!("{}\n{}\n", "|".repeat(n / 2), "|-".repeat(n / 4)),
        ),
        (
            "one cell per column",
            format!(
                "{}|\n{}|\n{}|\n",
                "a|".repeat(2000),
                "-|".repeat(2000),
                "b|".repeat(2000)
            ),
        ),
        ("lines", "x\n".repeat(n / 2)),
        ("headings", "# h\n".repeat(n / 4)),
        ("emphasis soup", "*_~`[]()<>!\\ ".repeat(n / 13)),
    ];
    for (name, src) in &shapes {
        let w = work_of(src, 80);
        assert!(
            w <= PER_CHAR * src.len() as u64 + 4096,
            "{name}: {w} steps for {} bytes",
            src.len()
        );
    }
}

#[test]
fn doubling_the_input_doubles_the_work() {
    for unit in [
        "*a ",
        "[a](",
        "`a``b```c",
        "> ",
        "- ",
        "| a | b |\n|---|---|\n",
        "_a",
        "~~a ",
        "<b> ",
    ] {
        let small = unit.repeat(5_000);
        let big = unit.repeat(10_000);
        let (w1, w2) = (work_of(&small, 80), work_of(&big, 80));
        assert!(w2 <= w1 * 5 / 2 + 1000, "{unit:?}: {w1} then {w2}");
    }
}

#[test]
fn the_rows_are_capped_and_the_cap_is_said() {
    let src = "x\n".repeat(MAX_ROWS + 500);
    let rows = render(&src, 40);
    assert_eq!(rows.len(), MAX_ROWS + 1);
    assert!(
        text_of(rows.last().unwrap()).starts_with("[\u{2026} "),
        "{}",
        text_of(rows.last().unwrap())
    );
    assert!(text_of(rows.last().unwrap()).contains("more lines not shown"));
    // Width 1 makes a row per character; the cap holds there too.
    let thin = render(&"a".repeat(MAX_ROWS * 3), 1);
    assert_eq!(thin.len(), MAX_ROWS + 1);
}

#[test]
fn containers_nest_to_a_limit_and_the_rest_is_text() {
    let deep = format!("{}x", "> ".repeat(30));
    let r = md(&deep, 80);
    assert_eq!(r.len(), 1);
    assert!(r[0].ends_with('x'));
    assert!(
        r[0].contains("> > >"),
        "past the limit the marks are text: {r:?}"
    );
    let list = format!("{}x", "- ".repeat(30));
    let r = md(&list, 200);
    assert!(r.last().unwrap().ends_with('x'));
}

// ---- diffs ------------------------------------------------------------------------------------------------------------------------

const DIFF: &str = "--- a/src/todo.py\n+++ b/src/todo.py\n@@ -1,3 +1,3 @@\n def add(a, b):\n-    return a - b\n+    return a + b\n # end\n\\ No newline at end of file";

fn reversed_texts(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .flat_map(|r| r.line.spans.iter())
        .filter(|s| has(s.style, Modifier::REVERSED))
        .map(|s| s.content.to_string())
        .collect()
}

#[test]
fn a_unified_diff_is_coloured_by_line_kind() {
    let r = render_diff(DIFF, 60);
    assert_eq!(plain(&r), DIFF.lines().collect::<Vec<_>>());
    let first = |i: usize| r[i].line.spans[0].style;
    assert!(has(first(0), Modifier::BOLD) && has(first(1), Modifier::BOLD));
    assert_eq!(first(2).fg, Some(Color::Cyan));
    assert_eq!(first(3).fg, None, "a context line keeps the default colour");
    assert_eq!(first(4).fg, Some(Color::Red));
    assert_eq!(first(5).fg, Some(Color::Green));
    assert!(has(first(7), Modifier::DIM));
    insta::assert_snapshot!("diff_basic", dump(&r));
}

#[test]
fn the_words_that_changed_are_reversed_in_both_lines_of_a_pair() {
    let r = render_diff(DIFF, 60);
    assert_eq!(reversed_texts(&r), ["-", "+"]);
    let pairs = render_diff("@@ -1,2 +1,3 @@\n-a b\n-c d\n+a X\n+c Y\n+extra", 40);
    assert_eq!(reversed_texts(&pairs), ["b", "d", "X", "Y"]);
    let same = render_diff("@@ -1 +1 @@\n-same\n+same", 40);
    assert!(reversed_texts(&same).is_empty());
    let indent = render_diff("@@ -1 +1 @@\n-x\n+    x", 40);
    assert_eq!(reversed_texts(&indent), ["    "]);
    // A removal with nothing to pair with, and an addition alone, have no emphasis.
    let lone = render_diff("@@ -1 +0,0 @@\n-gone", 40);
    assert!(reversed_texts(&lone).is_empty());
}

#[test]
fn a_very_long_pair_is_coloured_without_word_emphasis() {
    let a = (0..400)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let b = a.replace("w200", "CHANGED");
    let r = render_diff(&format!("@@ -1 +1 @@\n-{a}\n+{b}"), 100_000);
    assert!(reversed_texts(&r).is_empty());
    assert_eq!(r[1].line.spans[0].style.fg, Some(Color::Red));
}

#[test]
fn a_removed_line_that_looks_like_a_file_header_is_still_a_removal() {
    let d = "@@ -1,2 +1,1 @@\n context\n--- comment";
    let r = render_diff(d, 40);
    assert_eq!(plain(&r), ["@@ -1,2 +1,1 @@", " context", "--- comment"]);
    assert_eq!(r[2].line.spans[0].style.fg, Some(Color::Red));
    assert!(!has(r[2].line.spans[0].style, Modifier::BOLD));
}

#[test]
fn two_files_in_one_diff_each_get_their_headers() {
    let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-c\n+d";
    let r = render_diff(d, 40);
    assert_eq!(r.len(), 12);
    for i in [0, 1, 2, 6, 7, 8] {
        assert!(has(r[i].line.spans[0].style, Modifier::BOLD), "row {i}");
    }
    assert_eq!(r[10].line.spans[0].style.fg, Some(Color::Red));
    assert_eq!(r[11].line.spans[0].style.fg, Some(Color::Green));
}

#[test]
fn long_diff_lines_are_cut_with_their_colour() {
    let r = render_diff("@@ -1 +1 @@\n+abcdefghijklmnopqrst", 12);
    assert_eq!(plain(&r), ["@@ -1 +1 @@", "+abcdefghijk", "lmnopqrst"]);
    assert_eq!(joins(&r), [Join::Hard, Join::Hard, Join::Cut]);
    for row in &r[1..] {
        assert_eq!(row.line.spans[0].style.fg, Some(Color::Green));
    }
}

#[test]
fn text_that_is_not_a_diff_is_shown_raw() {
    let cases = [
        ("@@ -1 +1 @@\n-a\n+b", true),
        ("--- a\n+++ b\n@@ -1 +1 @@\n-a\n+b", true),
        (
            "diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b",
            true,
        ),
        ("--- a/x\n+++ b/x", true),
        ("- item\n- item two", false),
        ("just text", false),
        ("+++ only", false),
        ("", false),
        ("a @@ b @@", false),
    ];
    for (text, want) in cases {
        assert_eq!(is_unified_diff(text), want, "{text:?}");
    }
    let raw = render_diff("- item\n+ other\nplain", 40);
    assert_eq!(plain(&raw), ["- item", "+ other", "plain"]);
    for row in &raw {
        for s in &row.line.spans {
            assert_eq!(s.style, Style::default());
        }
    }
}

#[test]
fn a_diff_goes_through_the_safe_renderer_too() {
    let r = render_diff("@@ -1 +1 @@\n-a\u{1b}]0;t\u{7}b\n+c\u{202e}d", 40);
    assert_eq!(plain(&r), ["@@ -1 +1 @@", "-ab", "+c<U+202E>d"]);
}

// ---- building a diff --------------------------------------------------------------------------------------------------------------

#[test]
fn a_unified_diff_of_two_texts() {
    let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
    let new = "a\nb\nc\nD\ne\nf\ng\nh\n";
    assert_eq!(
        unified_diff("f.txt", old, new),
        "--- a/f.txt\n+++ b/f.txt\n@@ -1,7 +1,7 @@\n a\n b\n c\n-d\n+D\n e\n f\n g\n"
    );
    assert_eq!(unified_diff("f.txt", old, old), "");
    assert_eq!(
        unified_diff("n.txt", "", "a\nb\n"),
        "--- a/n.txt\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n"
    );
    assert_eq!(
        unified_diff("n.txt", "a\nb\n", ""),
        "--- a/n.txt\n+++ b/n.txt\n@@ -1,2 +0,0 @@\n-a\n-b\n"
    );
    assert_eq!(
        unified_diff("one.txt", "x\n", "y\n"),
        "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-x\n+y\n"
    );
    // Two changes far apart are two hunks; two close together are one.
    let far_old = (1..=20).map(|i| format!("l{i}\n")).collect::<String>();
    let far_new = far_old.replace("l2\n", "L2\n").replace("l19\n", "L19\n");
    let d = unified_diff("f", &far_old, &far_new);
    assert_eq!(d.matches("\n@@ ").count(), 2, "{d}");
    let near_new = far_old.replace("l2\n", "L2\n").replace("l6\n", "L6\n");
    let d = unified_diff("f", &far_old, &near_new);
    assert_eq!(d.matches("\n@@ ").count(), 1, "{d}");
    assert!(is_unified_diff(&d));
}

/// Applies a unified diff made by `unified_diff` to `old`, checking every context and removed line against it.
fn apply(old: &str, diff: &str) -> Result<String, String> {
    let lines: Vec<&str> = if old.is_empty() {
        vec![]
    } else {
        old.strip_suffix('\n').unwrap().split('\n').collect()
    };
    let mut out: Vec<String> = Vec::new();
    let mut at = 0usize; // next line of `lines` not yet copied
    let mut it = diff.lines().skip(2).peekable();
    while let Some(h) = it.next() {
        let rest = h
            .strip_prefix("@@ -")
            .ok_or(format!("not a hunk header: {h}"))?;
        let (old_part, _) = rest.split_once(" +").ok_or("no plus")?;
        let start: usize = old_part
            .split(',')
            .next()
            .unwrap()
            .parse()
            .map_err(|e| format!("{e}"))?;
        let len: usize = old_part.split(',').nth(1).map_or(1, |l| l.parse().unwrap());
        let first = if len == 0 { start } else { start - 1 };
        while at < first {
            out.push(lines[at].to_string());
            at += 1;
        }
        while let Some(l) = it.peek() {
            if l.starts_with("@@ ") {
                break;
            }
            let l = it.next().unwrap();
            let (tag, text) = l.split_at(1);
            match tag {
                " " => {
                    if lines.get(at) != Some(&text) {
                        return Err(format!("context mismatch at {at}: {text:?}"));
                    }
                    out.push(text.to_string());
                    at += 1;
                }
                "-" => {
                    if lines.get(at) != Some(&text) {
                        return Err(format!("removed mismatch at {at}: {text:?}"));
                    }
                    at += 1;
                }
                "+" => out.push(text.to_string()),
                other => return Err(format!("bad tag {other:?}")),
            }
        }
    }
    while at < lines.len() {
        out.push(lines[at].to_string());
        at += 1;
    }
    Ok(if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    })
}

proptest! {
    #[test]
    fn applying_a_made_diff_to_the_old_text_gives_the_new_text(
        old in prop::collection::vec("[abcd]{0,2}", 0..14),
        new in prop::collection::vec("[abcd]{0,2}", 0..14),
    ) {
        let (o, n) = (
            old.iter().map(|l| format!("{l}\n")).collect::<String>(),
            new.iter().map(|l| format!("{l}\n")).collect::<String>(),
        );
        let d = unified_diff("f", &o, &n);
        if o == n {
            prop_assert_eq!(d, "");
        } else {
            prop_assert_eq!(apply(&o, &d).unwrap(), n, "diff was\n{}", d);
        }
    }
}

// ---- blocks -----------------------------------------------------------------------------------------------------------------------

#[test]
fn an_engine_message_is_markdown_and_everything_the_user_or_a_tool_wrote_is_text() {
    let engine = Block::message("a", Role::Engine, Origin::Engine, "*em* and `code`");
    let r = render_block(&engine, 40);
    assert_eq!(plain(&r), ["em and code"]);
    assert!(has(style_of(&r, "em"), Modifier::ITALIC));

    let you = Block::message(
        "p",
        Role::You,
        Origin::User,
        "*not em* # not a heading\n- not a list",
    );
    let r = render_block(&you, 40);
    assert_eq!(plain(&r), ["*not em* # not a heading", "- not a list"]);
    assert!(
        r.iter()
            .all(|row| row.line.spans.iter().all(|s| s.style == Style::default()))
    );

    let tool = Block::tool_call("t", "cat t.md", None, None, "| a | b |\n|---|---|\n# x");
    assert_eq!(
        plain(&render_block(&tool, 40)),
        ["| a | b |", "|---|---|", "# x"]
    );
}

#[test]
fn thoughts_usage_and_gaps_are_dim_text() {
    for b in [
        Block::thought("th", "*pondering*"),
        Block::usage("u", "113k tokens"),
        Block::gap("g", GapCause::Shed, None),
    ] {
        let r = render_block(&b, 40);
        assert!(!r.is_empty());
        for row in &r {
            for s in &row.line.spans {
                assert!(has(s.style, Modifier::DIM), "{b:?}");
            }
        }
    }
    assert_eq!(
        plain(&render_block(&Block::thought("th", "*pondering*"), 40)),
        ["*pondering*"]
    );
    let n = Block::notice("n", NoticeKind::RunStatus, "Run active");
    assert_eq!(plain(&render_block(&n, 40)), ["Run active"]);
}

#[test]
fn a_diff_block_is_a_diff_when_it_is_one_and_raw_when_it_is_not() {
    let d = Block::diff("d", "a.py", "@@ -1 +1 @@\n-a\n+b");
    let r = render_block(&d, 40);
    assert_eq!(r[1].line.spans[0].style.fg, Some(Color::Red));
    let raw = Block::diff("d", "a.py", "- not\n+ a diff");
    let r = render_block(&raw, 40);
    assert_eq!(plain(&r), ["- not", "+ a diff"]);
    assert_eq!(r[0].line.spans[0].style, Style::default());
}

// ---- corpora, as snapshots ----------------------------------------------------------------------------------------------------------

const ANSWER: &str = "\
# Plan

I will **start with the tests**, then write the code. See the [docs](https://docs.example/todo) for the API, and `pytest -q` to run them.

## Steps

1. Write the failing test for `add`
2. Make it pass
   - keep the change small
   - run the whole suite
3. Refactor

> Note: the old *client* is not touched.

```python
def add(a, b):
    return a + b
```

| file | status |
|------|:------:|
| src/todo.py | done |
| tests/test_todo.py | **new** |

---

~~Old plan~~ replaced. Done.";

#[test]
fn a_typical_answer_at_three_widths() {
    for w in [80usize, 40, 24] {
        let r = render(ANSWER, w);
        for row in &r {
            assert!(
                row.line.width() <= w,
                "{} > {w}: {}",
                row.line.width(),
                text_of(row)
            );
        }
        insta::assert_snapshot!(format!("answer_{w}"), dump(&r));
    }
}

#[test]
fn nested_fences_and_lists_in_one_answer() {
    let src = "Outer:\n\n- item with code:\n\n  ```sh\n  echo hi\n  ```\n\n- next\n\n````md\n```rust\nfn x() {}\n```\n````\n\n> ```\n> quoted code\n> ```";
    insta::assert_snapshot!("nested_containers", dump(&render(src, 40)));
}

#[test]
fn control_sequences_in_one_answer() {
    let src = "# \u{1b}[31mred\u{1b}[0m title\n\n[click](\u{1b}]8;;http://example.test\u{7}here) \u{202e}reversed\u{200b}\n\n```\n\u{1b}]0;title\u{7}code\u{8}\n```\n\n| a |\n|---|\n| \u{1b}[1mb |";
    let r = render(src, 40);
    for row in &r {
        for s in &row.line.spans {
            assert!(
                s.content.chars().all(|c| !c.is_control()),
                "{:?}",
                s.content
            );
        }
    }
    insta::assert_snapshot!("control_sequences_answer", dump(&r));
}

#[test]
fn a_diff_with_emphasis_as_a_snapshot() {
    let d = unified_diff(
        "src/calc.py",
        "def add(a, b):\n    return a - b\n\ndef mul(a, b):\n    return a * b\n",
        "def add(a, b):\n    return a + b\n\ndef mul(a, b, c=1):\n    return a * b * c\n",
    );
    insta::assert_snapshot!("diff_emphasis", dump(&render_diff(&d, 50)));
}

// ---- properties -------------------------------------------------------------------------------------------------------------------

fn arb_markdownish() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => any::<char>(),
            6 => prop::sample::select(vec![
                '*', '_', '`', '~', '#', '>', '-', '+', '|', '[', ']', '(', ')', '!', '\\', '<', ':', '\n', '\n', ' ', ' ',
                '\t', '1', '.', 'a', 'b', '\u{1b}', '\u{7}', '\u{4f60}', '\u{1f600}', '\u{301}', '\u{200d}', '\r',
            ]),
        ],
        0..300,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

proptest! {
    #[test]
    fn any_text_at_any_width_renders_inside_the_width_with_no_control_character(
        src in arb_markdownish(), width in 1usize..70,
    ) {
        let rows = render(&src, width);
        for (i, r) in rows.iter().enumerate() {
            prop_assert!(r.line.width() <= width, "row {i} is {} wide, width {width}: {:?}", r.line.width(), text_of(r));
            for s in &r.line.spans {
                prop_assert!(s.content.chars().all(|c| !c.is_control()), "control in {:?}", s.content);
                prop_assert!(!s.content.contains('\u{1b}'), "escape byte in {:?}", s.content);
            }
        }
        if let Some(first) = rows.first() {
            prop_assert_eq!(first.join, Join::Hard);
        }
        prop_assert!(rows.len() <= MAX_ROWS + 1);
        let (_, work) = render_counted(&src, width);
        prop_assert!(work <= PER_CHAR * src.len() as u64 + 4096, "{} steps for {} bytes", work, src.len());
    }

    #[test]
    fn markup_only_removes_punctuation(src in "[a-z0-9 \\n*_#>+-]{0,200}", width in 8usize..60) {
        let rows = render(&src, width);
        let out = plain(&rows).join("\n");
        prop_assert_eq!(alnum_sorted(&out), alnum_sorted(&src), "out:\n{}", out);
    }

    #[test]
    fn diffs_render_inside_the_width_with_no_control_character(src in arb_markdownish(), width in 1usize..70) {
        let rows = render_diff(&format!("@@ -1 +1 @@\n{src}"), width);
        for r in &rows {
            prop_assert!(r.line.width() <= width, "{:?}", text_of(r));
            for s in &r.line.spans {
                prop_assert!(s.content.chars().all(|c| !c.is_control()));
            }
        }
    }
}
