//! The summary a turn reports to Step Functions is a preview inside a byte budget; the whole message is in summary.json.
#![allow(clippy::unwrap_used)]

use agentd::turn::summary_of;
use harness_protocol::api::internal::MAX_TURN_SUMMARY_BYTES;
use harness_protocol::direct::json_len;

#[test]
fn b776_a_short_message_is_kept_as_it_is_with_control_characters_removed() {
    assert_eq!(
        summary_of("Added it.\u{7}\nDone.").as_deref(),
        Some("Added it.\nDone.")
    );
    assert_eq!(summary_of("  \n "), None);
}

#[test]
fn b776_a_long_message_keeps_its_last_32_thousand_characters_inside_the_budget() {
    let long: String = (0..30_000).map(|i| format!("step {i} passed\n")).collect();
    let s = summary_of(&long).unwrap();
    assert!(json_len(&s) <= MAX_TURN_SUMMARY_BYTES, "{}", json_len(&s));
    assert!(
        s.trim_end().ends_with("step 29999 passed"),
        "the end is kept"
    );
    assert!(
        !s.contains("step 0 passed"),
        "the start of a message this long is not in the summary"
    );
    // Lines count for two bytes in JSON, so the last 32,000 characters are a little over the byte budget: a start, a marker and an end.
    assert!(s.contains(" characters omitted"), "{}", &s[..200]);
}

#[test]
fn b776_a_message_of_plain_text_under_the_budget_is_kept_whole() {
    let text = "a".repeat(30_000);
    assert_eq!(summary_of(&text).as_deref(), Some(text.as_str()));
}

#[test]
fn b776_text_that_json_writes_in_many_bytes_stays_inside_the_budget() {
    for unit in ["\"", "\u{1F680}", "x\n", "\\"] {
        let s = summary_of(&unit.repeat(100_000)).unwrap();
        assert!(json_len(&s) <= MAX_TURN_SUMMARY_BYTES, "{unit:?}");
    }
}
