//! The files of a run have no size of their own, so the lists `summary.json` makes of them can grow. `fit_summary` keeps the file inside
//! its 1 MiB whatever the user sent: the message is halved first, then skills, then rules are cut from their ends. The inputs below are larger than
//! `validate` and the counts allow, because the function is the safety net for what got past them.
#![allow(clippy::unwrap_used)]

use agentd::direct::summary::fit_summary;
use harness_protocol::direct::{DirectSummary, MAX_DIRECT_SUMMARY_BYTES};
use serde_json::{Value, json};

fn summary(rules: usize, skills: usize, message: Option<&str>) -> DirectSummary {
    let rule_refs: Vec<Value> = (0..rules)
        .map(|i| json!({"name": format!("rule-{i:05}.md"), "sha256": "ab".repeat(32), "bytes": 1234}))
        .collect();
    let skill_refs: Vec<Value> = (0..skills)
        .map(|i| json!({"name": format!("skill-{i:05}"), "sha256": "cd".repeat(32), "files": 3, "bytes": 99}))
        .collect();
    let v = json!({
        "schemaVersion": 1, "runId": "01JABCDEFGHJKMNPQRSTVWXYZ0", "agentId": "direct", "turn": 1,
        "outcome": "stopped_setup_failed", "label": "Cline did not start",
        "detail": "request.json sha256 differs", "stopReason": null, "turnOutcome": "failed",
        "clineMessage": message, "error": "request.json sha256 differs",
        "model": {"id": "us.anthropic.claude-opus-5-5", "region": "us-east-1"},
        "engine": {"name": "cline"},
        "rules": rule_refs, "skills": skill_refs, "rulesPreflight": {"ran": false},
        "request": {"sha256": "9f".repeat(32), "promptBytes": 0},
        "nudge": {"requested": false, "honored": false},
        "usage": null, "toolCalls": 0,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T18:00:01Z", "wallS": 1},
        "git": {"committedByHarness": false, "filesChanged": 0},
        "snapshots": {"count": 0},
        "packages": {"enabled": false, "requests": 0, "bytes": 0, "byPrefix": {}, "statuses": {}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.30.0"}
    });
    serde_json::from_value(v).unwrap()
}

fn size(s: &DirectSummary) -> usize {
    serde_json::to_vec(s).unwrap().len()
}

#[test]
fn b776_a_summary_that_fits_is_left_as_it_is() {
    let mut s = summary(100, 32, Some("the last message"));
    let before = s.clone();
    fit_summary(&mut s);
    assert_eq!(s, before);
}

#[test]
fn b776_only_the_message_is_cut_while_cutting_it_is_enough() {
    // 300,000 four-byte characters weigh 1.2 MB, more than the 200,000 characters `validate` allows and more than the file may hold, so the file is
    // over its 1 MiB until the message is halved once.
    let message = "\u{1f600}".repeat(300_000);
    let mut s = summary(300, 32, Some(&message));
    assert!(size(&s) > MAX_DIRECT_SUMMARY_BYTES);
    fit_summary(&mut s);
    assert!(size(&s) <= MAX_DIRECT_SUMMARY_BYTES);
    assert_eq!((s.rules.len(), s.skills.len()), (300, 32));
    let kept = s.cline_message.as_ref().map_or(0, |m| m.chars().count());
    assert!(kept > 0 && kept < 300_000, "{kept} characters are left");
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn b776_lists_that_cannot_fit_lose_their_ends_skills_first_and_the_summary_stays_valid() {
    let mut s = summary(10_000, 100, Some("a message"));
    assert!(size(&s) > MAX_DIRECT_SUMMARY_BYTES);
    let first_rule = s.rules[0].clone();
    fit_summary(&mut s);
    assert!(size(&s) <= MAX_DIRECT_SUMMARY_BYTES);
    assert!(s.cline_message.is_none(), "the message goes first");
    assert!(s.skills.is_empty(), "the skills go before any rule");
    assert!(
        s.rules.len() > 100 && s.rules.len() < 10_000,
        "{}",
        s.rules.len()
    );
    assert_eq!(s.rules[0], first_rule, "the lists are cut from their ends");
    assert_eq!(s.validate(), Ok(()));
}
