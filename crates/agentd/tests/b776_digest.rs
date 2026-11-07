//! The notes a fresh session starts from are sized for a model with a window of a million tokens. The user's own task goes in whole and
//! outside the budget, the budget is 200,000 bytes, and the caps of the sections are eight times what they were.
#![allow(clippy::unwrap_used)]

use agentd::direct::input::{DigestSources, StopWhy, ToolNote, digest, new_session_prompt};
use harness_protocol::direct::{
    DIGEST_BUDGET_BYTES, INLINE_SUMMARY_BYTES, MAX_STOP_DIGEST_BYTES, NEEDS_INPUT_PAYLOAD_BUDGET,
};
use harness_protocol::envelope::MAX_INLINE_PAYLOAD_BYTES;
use std::path::Path;

fn sources(task: &str) -> DigestSources {
    DigestSources {
        task: task.into(),
        attempts: vec![],
        shape: vec![],
        git_log: Some("a1b2c3d harness: turn 1".into()),
        diff_stat: Some(" src/app.py | 12 ++++\n 1 file changed".into()),
        changed_files: vec!["src/app.py".into()],
        files_total: 1,
        tools: vec![ToolNote {
            title: "Run pytest".into(),
            status: "failed".into(),
        }],
        last_message: Some("I will keep polling the build.".into()),
        progress_file: Some("# Progress\n- step 1 done".into()),
        test_tail: Some("FAILED tests/test_app.py::test_add".into()),
        stderr_tail: Some("warning: something".into()),
        stop_outcome: Some("stopped_engine_cancelled".into()),
        secrets: vec![],
    }
}

/// A task of `n` lines with a mark in the first, the middle and the last.
fn task_of(n: usize) -> String {
    let mut lines: Vec<String> = (0..n)
        .map(|i| format!("requirement {i}: the system shall do thing {i}"))
        .collect();
    lines[0] = "FIRST-MARK ".to_string() + &lines[0];
    lines[n / 2] = "MIDDLE-MARK ".to_string() + &lines[n / 2];
    lines[n - 1] = "LAST-MARK ".to_string() + &lines[n - 1];
    lines.join("\n")
}

#[test]
fn b776_the_budgets_are_sized_for_a_window_of_a_million_tokens() {
    assert_eq!(DIGEST_BUDGET_BYTES, 200_000);
    assert_eq!(INLINE_SUMMARY_BYTES, 24_000);
    assert_eq!(MAX_STOP_DIGEST_BYTES, 256 * 1024);
    assert_eq!(NEEDS_INPUT_PAYLOAD_BUDGET, MAX_INLINE_PAYLOAD_BYTES - 296);
}

#[test]
fn b776_the_users_task_is_whole_in_the_digest_and_does_not_use_the_budget() {
    let task = task_of(4_000);
    assert!(task.len() > 150_000, "{}", task.len());
    let d = digest(&sources(&task), DIGEST_BUDGET_BYTES);
    let text = d.text();
    assert!(text.contains(&task), "the task is whole");
    assert!(!text.contains("[cut:"), "nothing in the middle is cut");
    assert!(
        d.dropped().is_empty(),
        "the task took none of the room of the other sections: {:?}",
        d.dropped()
    );
    let ns: Vec<u8> = d.sections().iter().map(|s| s.n).collect();
    assert_eq!(ns, vec![1, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn b776_the_prompt_of_a_fresh_session_holds_the_task_whole_and_keeps_its_summary() {
    let task = task_of(4_000);
    let src = sources(&task);
    let d = digest(&src, DIGEST_BUDGET_BYTES);
    let p = new_session_prompt(
        &src.task,
        &StopWhy::default(),
        &d,
        None,
        None,
        Path::new("/var/lib/agentd/run1/context"),
    );
    assert!(p.contains(&task), "the task is whole in the prompt");
    assert!(
        p.contains("a1b2c3d harness: turn 1"),
        "the summary of the earlier session is still in the prompt: {}",
        &p[p.len().saturating_sub(1500)..]
    );
}

#[test]
fn b776_a_test_tail_and_a_last_message_longer_than_the_old_caps_are_kept() {
    let mut src = sources("Build it.");
    let tail: String = (0..1_500).map(|i| format!("FAILED test_{i}\n")).collect();
    assert!(tail.len() > 20_000 && tail.len() < 32_000);
    src.test_tail = Some(tail.clone());
    let message: String = (0..1_800)
        .map(|i| format!("I did step {i} and it worked\n"))
        .collect();
    assert!(message.len() > 40_000 && message.len() < 64_000);
    src.last_message = Some(message.clone());
    let text = digest(&src, DIGEST_BUDGET_BYTES).text();
    assert!(
        text.contains(tail.trim_end()),
        "the whole failing test output is in the digest"
    );
    assert!(
        text.contains("I did step 0 and it worked"),
        "the start of the message is kept"
    );
    assert!(
        text.contains("I did step 1799 and it worked"),
        "the end of the message is kept"
    );
}
