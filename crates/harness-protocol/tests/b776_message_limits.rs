//! Cline's final message is no longer cut to its last 8,000 characters. `summary.json` and `evidence/direct.json` (S3 objects) keep it
//! up to a guard of 200,000 characters. The copies that ride in Step Functions state (256 KiB for the whole state) are previews held to a byte
//! budget of their JSON text, the head and the tail of the message with a marker between them.
#![allow(clippy::unwrap_used, clippy::assertions_on_constants)] // the tests pin the values of constants on purpose

use harness_protocol::direct::{
    DirectSummary, DirectSummaryError, MAX_CLINE_MESSAGE_CHARS, MAX_DIRECT_ATTEMPTS,
    MAX_DIRECT_DETAIL_CHARS, MAX_DIRECT_INTERVENTIONS, MAX_DIRECT_SUMMARY_BYTES,
    MAX_DIRECT_SWITCHES, MAX_SLICE_BRIEF_BYTES, MAX_STATE_MESSAGE_BYTES, json_len, message_preview,
};

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const BUCKET: &str = "hamh-direct-bucket";

fn summary() -> DirectSummary {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": 1,
        "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": "reported_done",
        "label": "Cline reported done",
        "detail": null,
        "stopReason": "end_turn",
        "turnOutcome": "completed",
        "clineMessage": "Added percent_change(a, b) and its tests.",
        "error": null,
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1", "pinned": "us.anthropic.claude-opus-5-5"},
        "engine": {"name": "cline", "version": "3.0.71", "binarySha256": "cd".repeat(32)},
        "rules": [{"name": "00-my-rules.md", "sha256": "ef".repeat(32), "bytes": 2943}],
        "rulesPreflight": {"ran": true, "ms": 812},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": {"inputTokens": 1000, "outputTokens": 200, "cacheReadInputTokens": 30, "cacheWriteInputTokens": 4},
        "toolCalls": 41,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T19:02:11Z", "wallS": 3731, "promptS": 3690, "maxSilentGapS": 84},
        "git": {"baseSha": "1".repeat(40), "headSha": "2".repeat(40), "committedByHarness": true, "filesChanged": 12,
                "bundleUri": format!("s3://{BUCKET}/runs/{RUN}/agents/direct/turns/1/bundle"), "bundleBytes": 123_456},
        "snapshots": {"count": 5, "lastAt": "2026-10-13T18:50:00Z"},
        "packages": {"enabled": true, "requests": 57, "bytes": 123_456_789,
                     "byPrefix": {"go": 0, "maven": 0, "npm": 10, "pypi": 40}, "statuses": {"200": 55, "404": 2}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.30.0"}
    }))
    .unwrap()
}

#[test]
fn b776_json_len_counts_the_bytes_a_string_takes_inside_json() {
    assert_eq!(json_len("abc"), 3);
    assert_eq!(json_len("a\"b"), 4, "a quote takes two bytes");
    assert_eq!(json_len("a\\b"), 4, "a backslash takes two bytes");
    assert_eq!(
        json_len("a\nb\tc"),
        7,
        "a newline and a tab take two bytes each"
    );
    assert_eq!(json_len("\u{1}"), 6, "another control character takes six");
    assert_eq!(json_len("é"), 2);
    assert_eq!(json_len("€"), 3);
    assert_eq!(json_len("\u{1F680}"), 4);
    // It is what serde_json writes, less the two quotes.
    for s in ["plain", "q\"q", "n\nn", "\u{1}\u{1f}", "é€\u{1F680}", "\\"] {
        assert_eq!(
            json_len(s),
            serde_json::to_string(s).unwrap().len() - 2,
            "{s:?}"
        );
    }
}

#[test]
fn b776_a_message_inside_the_budget_is_not_touched() {
    let m = "Added percent_change.\nRan the suite: 3 passed.\n";
    assert_eq!(message_preview(m, MAX_STATE_MESSAGE_BYTES, ""), m);
    let exact = "x".repeat(MAX_STATE_MESSAGE_BYTES);
    assert_eq!(message_preview(&exact, MAX_STATE_MESSAGE_BYTES, ""), exact);
}

#[test]
fn b776_a_long_message_becomes_its_head_a_marker_and_its_tail_inside_the_budget() {
    let lines: Vec<String> = (0..20_000)
        .map(|i| format!("line {i} of the report"))
        .collect();
    let text = lines.join("\n");
    assert!(json_len(&text) > 10 * MAX_STATE_MESSAGE_BYTES);
    let p = message_preview(
        &text,
        MAX_STATE_MESSAGE_BYTES,
        " (the whole text is in the evidence)",
    );
    assert!(json_len(&p) <= MAX_STATE_MESSAGE_BYTES, "{}", json_len(&p));
    assert!(
        json_len(&p) > MAX_STATE_MESSAGE_BYTES * 9 / 10,
        "most of the budget is used: {}",
        json_len(&p)
    );
    assert!(
        p.starts_with("line 0 of the report\nline 1 of"),
        "{:?}",
        &p[..40]
    );
    assert!(p.ends_with("line 19999 of the report"), "the end is kept");
    assert!(
        p.contains(" characters omitted (the whole text is in the evidence)"),
        "{p}"
    );
    // The tail is the larger part: the conclusions come last.
    let at = p.find("characters omitted").unwrap();
    assert!(p.len() - at > at, "the tail is longer than the head");
}

#[test]
fn b776_the_marker_names_how_many_characters_were_left_out() {
    let text = "é".repeat(50_000);
    let p = message_preview(&text, 4_096, "");
    let kept = p.chars().filter(|c| *c == 'é').count();
    let omitted = 50_000 - kept;
    assert!(p.contains(&format!("{omitted} characters omitted")), "{p}");
}

#[test]
fn b776_quotes_emoji_and_control_text_stay_inside_the_budget() {
    for unit in [
        "\"",
        "\\",
        "\n",
        "\u{1F680}",
        "\u{2603}",
        "\u{1}",
        "a\"\n\\é",
    ] {
        let text = unit.repeat(200_000);
        let p = message_preview(&text, MAX_STATE_MESSAGE_BYTES, "");
        assert!(
            json_len(&p) <= MAX_STATE_MESSAGE_BYTES,
            "{unit:?}: {}",
            json_len(&p)
        );
        assert!(!p.is_empty());
    }
}

#[test]
fn b776_the_state_budget_leaves_room_for_two_copies_and_the_rest_of_the_state() {
    // The message sits in the state twice (the turn result and the evidence of the plan), the pull request text is held to the comment limit of
    // the repository service (10,240 characters), and the state is 262,144 bytes.
    assert_eq!(MAX_STATE_MESSAGE_BYTES, 32 * 1024);
    assert!(2 * MAX_STATE_MESSAGE_BYTES + 10_240 * 4 < 262_144 / 2);
}

#[test]
fn b776_summary_json_keeps_a_message_of_200_thousand_characters() {
    assert_eq!(MAX_CLINE_MESSAGE_CHARS, 200_000);
    assert!(MAX_DIRECT_SUMMARY_BYTES >= 1024 * 1024);
    let mut s = summary();
    s.cline_message = Some("m".repeat(MAX_CLINE_MESSAGE_CHARS));
    assert_eq!(s.validate(), Ok(()));
    // Four bytes a character is the widest: 200,000 of them are 800,000 bytes, still inside the file bound.
    s.cline_message = Some("\u{1F680}".repeat(MAX_CLINE_MESSAGE_CHARS));
    assert_eq!(s.validate(), Ok(()));
    s.cline_message = Some("m".repeat(MAX_CLINE_MESSAGE_CHARS + 1));
    assert_eq!(s.validate(), Err(DirectSummaryError::MessageTooLong));
}

#[test]
fn b776_a_detail_may_be_a_thousand_characters_and_the_lists_are_longer() {
    assert_eq!(MAX_DIRECT_DETAIL_CHARS, 1_000);
    assert_eq!(MAX_DIRECT_ATTEMPTS, 96);
    assert_eq!(MAX_DIRECT_SWITCHES, 96);
    assert_eq!(MAX_DIRECT_INTERVENTIONS, 32);
    assert_eq!(MAX_SLICE_BRIEF_BYTES, 16 * 1024);
    let mut s = summary();
    s.detail = Some("d".repeat(MAX_DIRECT_DETAIL_CHARS));
    s.error = Some("e".repeat(MAX_DIRECT_DETAIL_CHARS));
    assert_eq!(s.validate(), Ok(()));
    s.detail = Some("d".repeat(MAX_DIRECT_DETAIL_CHARS + 1));
    assert_eq!(s.validate(), Err(DirectSummaryError::DetailTooLong));
}

#[test]
fn b776_a_budget_too_small_for_the_marker_gives_the_end_of_the_text_inside_it() {
    let text = "abcdefghij".repeat(100);
    for max in [0usize, 1, 10, 40] {
        let p = message_preview(&text, max, "");
        assert!(json_len(&p) <= max, "{max}: {p:?}");
        assert!(text.ends_with(&p));
    }
}
