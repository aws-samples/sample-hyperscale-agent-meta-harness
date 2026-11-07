//! The result contract of the tool server: what a tool's answer becomes on the wire.
//!
//! The engine's client turns the whole MCP result object into JSON text and cuts it in the middle at 8,000 characters. The server
//! therefore sends `content` text only, keeps the SERIALIZED result under the profile's cap (7,500) counted the way a JavaScript string is counted
//! (UTF-16 code units), puts a failure in the first words, and says when it shortened an answer.
#![allow(clippy::unwrap_used)]

use agentd::direct::tools::backend::{FailureWord, ToolOutput};
use agentd::direct::tools::result::{
    DEFAULT_RESULT_CHARS, DETAIL_MAX_CHARS, MIN_RESULT_CHARS, failure_text, render, serialized_len,
};
use harness_protocol::tools::RESULT_FAILURE_WORDS;
use serde_json::{Value, json};

fn text_of(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap()
}

/// The length a JavaScript client measures: `JSON.stringify(result).length`.
fn js_len(v: &Value) -> usize {
    serde_json::to_string(v).unwrap().encode_utf16().count()
}

#[test]
fn the_cap_and_the_floor_are_the_profiles_numbers() {
    assert_eq!(DEFAULT_RESULT_CHARS, 7_500);
    assert_eq!(MIN_RESULT_CHARS, 256);
}

#[test]
fn an_answer_that_fits_is_sent_as_it_is() {
    let r = render(&ToolOutput::text("src/a.rs:1: fn main() {}"), 7_500);
    assert_eq!(
        r.value,
        json!({"content": [{"type": "text", "text": "src/a.rs:1: fn main() {}"}], "isError": false})
    );
    assert!(!r.truncated);
    assert_eq!(r.text_bytes, "src/a.rs:1: fn main() {}".len());
    assert_eq!(r.failure, None);
    // An empty answer is still one text block: some clients refuse an empty list.
    let r = render(&ToolOutput::text(""), 7_500);
    assert_eq!(r.value["content"], json!([{"type": "text", "text": ""}]));
}

#[test]
fn serialized_len_counts_utf16_units_of_the_compact_form() {
    for v in [
        json!({"a": 1}),
        json!({"t": "\"quoted\" and \\ slash"}),
        json!({"t": "tab\tnew\nline\u{1}ctl"}),
        json!({"t": "\u{1F600}\u{1F600}"}),
        json!({"t": "\u{e9}\u{4e2d}\u{2028}\u{7f}"}),
        json!({"content": [{"type": "text", "text": "x"}], "isError": false}),
    ] {
        assert_eq!(serialized_len(&v), js_len(&v), "{v}");
    }
}

#[test]
fn a_long_answer_is_cut_to_the_cap_whatever_it_is_made_of() {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let alphabet: Vec<char> = "ab \n\t\"\\\u{1}\u{7f}\u{e9}\u{4e2d}\u{2028}\u{1F600}/{}[]:,"
        .chars()
        .collect();
    for round in 0..60 {
        let len = 200 + (round * 977) % 40_000;
        let mut s = String::new();
        for _ in 0..len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            s.push(alphabet[(seed % alphabet.len() as u64) as usize]);
        }
        for cap in [256, 300, 1_000, 7_500, 16_000] {
            let r = render(&ToolOutput::text(s.clone()), cap);
            assert!(
                js_len(&r.value) <= cap.max(MIN_RESULT_CHARS),
                "round {round} cap {cap}: {} units",
                js_len(&r.value)
            );
            assert!(serialized_len(&r.value) <= cap.max(MIN_RESULT_CHARS));
            // The result is a result: parseable, content only.
            let back: Value =
                serde_json::from_str(&serde_json::to_string(&r.value).unwrap()).unwrap();
            assert_eq!(back, r.value);
            assert!(back.get("structuredContent").is_none());
            let t = text_of(&r.value);
            if r.truncated {
                assert!(
                    t.contains("[shortened: showing "),
                    "round {round} cap {cap}"
                );
                assert!(t.chars().count() < s.chars().count() + 80);
            } else {
                assert_eq!(t, s);
            }
        }
    }
}

#[test]
fn the_cut_keeps_the_head_says_how_much_and_cuts_at_a_line_when_one_is_near() {
    let all: String = (0..3000)
        .map(|i| format!("src/file.rs:{i}: let x = {i};\n"))
        .collect();
    let r = render(&ToolOutput::text(all.clone()), 7_500);
    assert!(r.truncated);
    let t = text_of(&r.value);
    let (body, notice) = t.rsplit_once("\n[shortened: showing ").unwrap();
    // The head is kept, in order, up to a whole line.
    assert!(all.starts_with(body));
    assert!(body.ends_with(';'), "{:?}", &body[body.len() - 30..]);
    let shown = body.chars().count();
    assert_eq!(
        notice,
        format!("{shown} of {} characters]", all.chars().count())
    );
    assert!(shown as f64 > 0.8 * 7_000.0, "{shown} characters shown");
    // One long line has no line end to cut at: it is cut on a character, not in the middle of one.
    let long = "\u{1F600}".repeat(20_000);
    let r = render(&ToolOutput::text(long), 7_500);
    let t = text_of(&r.value);
    let body = t.rsplit_once("\n[shortened").unwrap().0;
    assert!(body.chars().all(|c| c == '\u{1F600}'));
    // The budget is the cap: close to it, not far under it.
    assert!(js_len(&r.value) > 7_400, "{}", js_len(&r.value));
}

#[test]
fn a_line_end_far_from_the_end_of_what_fits_is_not_used() {
    // One short line and then a very long one: cutting at the only line end would show 9 characters of 20,000, so the cut is on a character.
    let text = format!("head line\n{}", "z".repeat(20_000));
    let r = render(&ToolOutput::text(text), 7_500);
    assert!(r.truncated);
    let t = text_of(&r.value);
    let body = t.rsplit_once("\n[shortened: showing ").unwrap().0;
    assert!(body.starts_with("head line\nzzzz"), "{:?}", &body[..20]);
    assert!(
        body.chars().count() > 7_000,
        "{} characters",
        body.chars().count()
    );
    // The same text with the second line end near the end of what fits is cut at that line end.
    let near = format!("{}\n{}", "a".repeat(7_300), "b".repeat(5_000));
    let r = render(&ToolOutput::text(near), 7_500);
    let t = text_of(&r.value);
    let body = t.rsplit_once("\n[shortened: showing ").unwrap().0;
    assert_eq!(body, "a".repeat(7_300));
}

#[test]
fn a_cap_below_the_floor_is_raised_to_it() {
    let r = render(&ToolOutput::text("x".repeat(5_000)), 10);
    assert!(js_len(&r.value) <= MIN_RESULT_CHARS);
    assert!(
        js_len(&r.value) > MIN_RESULT_CHARS - 80,
        "{}",
        js_len(&r.value)
    );
    let r = render(&ToolOutput::text("x".repeat(5_000)), 0);
    assert!(js_len(&r.value) <= MIN_RESULT_CHARS);
}

#[test]
fn a_failure_starts_with_error_and_its_word_and_marks_the_result() {
    let r = render(
        &ToolOutput::failed(FailureWord::Usage, "pattern: is required"),
        7_500,
    );
    assert_eq!(text_of(&r.value), "error: usage: pattern: is required");
    assert_eq!(r.value["isError"], true);
    assert_eq!(r.failure, Some(FailureWord::Usage));
    let r = render(&ToolOutput::failed_bare(FailureWord::Busy), 7_500);
    assert!(text_of(&r.value).starts_with("error: busy"));
    // The words that the table of the protocol names, and no others.
    let ours: Vec<&str> = FailureWord::ALL.iter().map(|w| w.as_str()).collect();
    assert_eq!(ours, RESULT_FAILURE_WORDS);
    // Which of them mean "did not run" (a refusal) and which "ran and failed".
    let refusals: Vec<&str> = FailureWord::ALL
        .iter()
        .filter(|w| w.is_refusal())
        .map(|w| w.as_str())
        .collect();
    assert_eq!(refusals, ["busy", "disabled", "refused", "usage"]);
}

#[test]
fn a_detail_is_one_short_plain_line() {
    let t = failure_text(FailureWord::Unavailable, Some("a\nb\r\nc\td\u{0}e   f"));
    assert_eq!(t, "error: unavailable: a b c d e f");
    let long = "w".repeat(1_000);
    let t = failure_text(FailureWord::Refused, Some(&long));
    assert_eq!(
        t.chars().count(),
        "error: refused: ".len() + DETAIL_MAX_CHARS
    );
    assert_eq!(
        failure_text(FailureWord::Timeout, Some("  ")),
        "error: timeout"
    );
    assert_eq!(failure_text(FailureWord::Timeout, None), "error: timeout");
    // A failure keeps its first words when the answer is shortened: the word is never the part that is cut.
    let r = render(
        &ToolOutput::failed(FailureWord::Refused, "x".repeat(5_000)),
        256,
    );
    assert!(text_of(&r.value).starts_with("error: refused: "));
}

#[test]
fn a_backends_own_cut_is_reported_and_not_repeated() {
    let r = render(&ToolOutput::shortened("100 of 400 matches"), 7_500);
    assert!(r.truncated);
    assert_eq!(text_of(&r.value), "100 of 400 matches");
}
