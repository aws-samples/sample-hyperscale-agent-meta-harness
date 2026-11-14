//! The usage of a run counts the engine runs that no answered prompt reported. The pure parts: the counts of an observer end line
//! (`goal::rounds::parse_usage`), the rule that matches runs to the prompts whose answers reported usage (`goal::usage::OutsideUsage`), the
//! reader that hands the counts on, and the seam that adds up what the modules saw (`Hooks::usage_outside_prompts`). The whole run is in
//! `direct_goal.rs` (`b824_the_run_usage_counts_the_failed_prompt_the_goal_loop_round_and_the_review_round`).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::rounds::{
    MAX_LINE, MAX_RUN_TOKENS, POLL_BYTES, RoundsReader, RunUsage, parse_usage, read_to_end,
};
use agentd::direct::goal::usage::OutsideUsage;
use agentd::direct::hooks::{DirectHook, Hooks};
use harness_protocol::common::Usage;
use std::sync::Arc;

fn usage(input: u64, output: u64, read: u64, write: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: read,
        cache_write_input_tokens: write,
    }
}

fn run(t_ms: u64, u: Usage) -> RunUsage {
    RunUsage { t_ms, usage: u }
}

#[test]
fn b824_an_end_line_with_all_four_counts_gives_the_run_usage_with_the_cache_taken_out_of_the_input()
{
    let line = r#"{"ev":"end","n":3,"t":1700000000123,"status":"failed","iterations":33,"inputTokens":112003,"outputTokens":4100,"cacheReadTokens":91000,"cacheWriteTokens":21000,"text":""}"#;
    assert_eq!(
        parse_usage(line),
        Some(run(1_700_000_000_123, usage(3, 4100, 91_000, 21_000)))
    );
}

#[test]
fn b824_an_end_line_is_believed_only_within_its_bounds() {
    let base = |extra: &str| format!(r#"{{"ev":"end","n":1,"t":5,"outputTokens":2,{extra}}}"#);
    // the observer of the image before the cache counts: its input holds the cache tokens and could not be priced
    assert_eq!(parse_usage(&base(r#""inputTokens":7"#)), None);
    assert_eq!(
        parse_usage(&base(r#""inputTokens":7,"cacheReadTokens":1"#)),
        None
    );
    // not whole numbers
    assert_eq!(
        parse_usage(&base(
            r#""inputTokens":7.5,"cacheReadTokens":1,"cacheWriteTokens":1"#
        )),
        None
    );
    assert_eq!(
        parse_usage(&base(
            r#""inputTokens":-7,"cacheReadTokens":1,"cacheWriteTokens":1"#
        )),
        None
    );
    assert_eq!(
        parse_usage(&base(
            r#""inputTokens":"7","cacheReadTokens":1,"cacheWriteTokens":1"#
        )),
        None
    );
    // cache counts larger than the input they are part of
    assert_eq!(
        parse_usage(&base(
            r#""inputTokens":7,"cacheReadTokens":5,"cacheWriteTokens":3"#
        )),
        None
    );
    // a count past the bound
    let big = MAX_RUN_TOKENS + 1;
    assert_eq!(
        parse_usage(&base(&format!(
            r#""inputTokens":{big},"cacheReadTokens":0,"cacheWriteTokens":0"#
        ))),
        None
    );
    assert_eq!(
        parse_usage(&base(&format!(
            r#""inputTokens":{MAX_RUN_TOKENS},"cacheReadTokens":0,"cacheWriteTokens":0"#
        ))),
        Some(run(5, usage(MAX_RUN_TOKENS, 2, 0, 0)))
    );
    // no time, a start line, not JSON
    assert_eq!(
        parse_usage(
            r#"{"ev":"end","n":1,"inputTokens":7,"outputTokens":2,"cacheReadTokens":0,"cacheWriteTokens":0}"#
        ),
        None
    );
    assert_eq!(parse_usage(r#"{"ev":"start","n":1,"t":5}"#), None);
    assert_eq!(parse_usage("not json"), None);
    // the lowest line that counts
    assert_eq!(
        parse_usage(&base(
            r#""inputTokens":7,"cacheReadTokens":4,"cacheWriteTokens":3"#
        )),
        Some(run(5, usage(0, 2, 4, 3)))
    );
}

/// The shape of the run of 2026-11-11: the first prompt ends in an error, the resume answers, the engine runs a round of its own, the review
/// answers. The failed prompt's run and the round are added; the two runs inside answers that reported usage are not.
#[test]
fn b824_the_runs_of_a_failed_prompt_and_of_a_round_between_two_prompts_are_added_and_answered_runs_are_not()
 {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 1000);
    o.run_ended(run(1900, usage(3, 4100, 91_000, 21_000)));
    o.prompt_replied(1, 2000, None); // the error: no usage
    o.prompt_sent(2, 2100);
    o.run_ended(run(4900, usage(5, 7300, 402_000, 31_000)));
    o.prompt_replied(2, 5000, Some(usage(1, 1, 1, 1)));
    o.run_ended(run(6000, usage(4, 900, 120_000, 60_000))); // the round: no prompt at all
    o.prompt_sent(3, 6100);
    o.run_ended(run(7900, usage(2, 1500, 150_000, 2500)));
    o.prompt_replied(3, 8000, Some(usage(1, 1, 1, 1)));
    assert_eq!(o.total(), Some(usage(7, 5000, 211_000, 81_000)));
}

#[test]
fn b824_a_run_read_before_its_answer_arrived_is_matched_when_the_sum_is_taken() {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 100);
    o.run_ended(run(150, usage(1, 1, 1, 1)));
    // the log was read before the answer: the window is still open
    o.prompt_replied(1, 200, Some(usage(1, 1, 1, 1)));
    assert_eq!(o.total(), None);
}

#[test]
fn b824_the_edges_of_a_window_belong_to_it() {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 100);
    o.prompt_replied(1, 200, Some(usage(1, 1, 1, 1)));
    o.run_ended(run(100, usage(1, 0, 0, 0)));
    o.run_ended(run(200, usage(2, 0, 0, 0)));
    o.run_ended(run(99, usage(4, 0, 0, 0)));
    o.run_ended(run(201, usage(8, 0, 0, 0)));
    assert_eq!(o.total(), Some(usage(12, 0, 0, 0)));
}

#[test]
fn b824_a_run_whose_answer_had_no_usage_or_never_came_is_added() {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 100);
    o.run_ended(run(150, usage(1, 0, 0, 0)));
    o.prompt_replied(1, 200, None); // answered, no usage in it
    o.prompt_sent(2, 300);
    o.run_ended(run(350, usage(2, 0, 0, 0))); // the engine is gone before the answer
    o.prompt_replied(9, 400, Some(usage(1, 1, 1, 1))); // an answer to a prompt never seen as sent changes nothing
    assert_eq!(o.total(), Some(usage(3, 0, 0, 0)));
}

#[test]
fn b824_nothing_outside_the_prompts_keeps_the_usage_the_run_had() {
    let mut o = OutsideUsage::default();
    assert_eq!(o.total(), None);
    o.prompt_sent(1, 100);
    o.prompt_replied(1, 200, Some(usage(1, 1, 1, 1)));
    assert_eq!(o.total(), None);
}

#[test]
fn b824_the_reader_hands_on_the_counts_of_the_end_lines_it_reads() {
    let dir = common::scratch("b824-rounds");
    let path = dir.join("rounds.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"ev":"start","n":1,"t":10}"#, "\n",
            r#"{"ev":"end","n":1,"t":20,"status":"completed","iterations":2,"inputTokens":100,"outputTokens":10,"text":"old observer"}"#, "\n",
            r#"{"ev":"start","n":2,"t":30}"#, "\n",
            r#"{"ev":"end","n":2,"t":40,"status":"failed","iterations":3,"inputTokens":60,"outputTokens":5,"cacheReadTokens":40,"cacheWriteTokens":15,"text":""}"#, "\n",
        ),
    )
    .unwrap();
    let mut reader = RoundsReader::new(path);
    let polled = reader.poll();
    assert_eq!(polled.lines.len(), 4, "{polled:?}");
    assert_eq!(polled.usage, vec![run(40, usage(5, 5, 40, 15))]);
    assert!(
        reader.poll().usage.is_empty(),
        "each line is handed on once"
    );
}

struct Saw(Option<Usage>);

impl DirectHook for Saw {
    fn name(&self) -> &'static str {
        "saw"
    }
    fn usage_outside_prompts(&self) -> Option<Usage> {
        self.0
    }
}

struct Panics;

impl DirectHook for Panics {
    fn name(&self) -> &'static str {
        "panics"
    }
    fn usage_outside_prompts(&self) -> Option<Usage> {
        panic!("a module that breaks adds nothing")
    }
}

#[test]
fn b824_the_seam_adds_up_what_the_modules_saw_and_a_module_that_panics_adds_nothing() {
    assert_eq!(Hooks::none().usage_outside_prompts(), None);
    let hooks = Hooks::new(vec![
        Arc::new(Saw(Some(usage(1, 2, 3, 4)))) as Arc<dyn DirectHook>,
        Arc::new(Panics),
        Arc::new(Saw(None)),
        Arc::new(Saw(Some(usage(10, 20, 30, 40)))),
    ]);
    assert_eq!(hooks.usage_outside_prompts(), Some(usage(11, 22, 33, 44)));
    assert_eq!(
        hooks.usage_outside_prompts(),
        Some(usage(11, 22, 33, 44)),
        "the module that panicked is off and the others answer again"
    );
}

/// The engine answered a try `cancelled` by itself and the harness resumed: the ladder drops that answer, so its usage is not in what the
/// prompts reported, and its run is counted from the log.
#[test]
fn b824_a_try_the_engine_ended_itself_and_the_harness_resumed_leaves_its_run_outside() {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 100);
    o.run_ended(run(150, usage(1, 10, 100, 1000)));
    o.prompt_replied(1, 200, Some(usage(1, 10, 100, 1000)));
    o.try_dropped();
    // a second notice for the same try takes no other window back
    o.try_dropped();
    o.prompt_sent(2, 300);
    o.run_ended(run(350, usage(2, 20, 200, 2000)));
    o.prompt_replied(2, 400, Some(usage(2, 20, 200, 2000)));
    assert_eq!(
        o.total(),
        Some(usage(1, 10, 100, 1000)),
        "the answered window of prompt 2 stays"
    );
}

/// An answer whose usage is all zero added nothing to what the prompts reported (a prompt queued during a round, a prompt the engine did not
/// take): a run that ended in its window is still counted.
#[test]
fn b824_an_answer_with_zero_usage_covers_no_run() {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 100);
    o.run_ended(run(150, usage(3, 0, 0, 0)));
    o.prompt_replied(1, 200, Some(Usage::default()));
    assert_eq!(o.total(), Some(usage(3, 0, 0, 0)));
}

/// The drain when the engine is gone reads on past a read that only skipped part of a line longer than the line limit: the end line after it
/// is still found.
#[test]
fn b824_the_drain_reads_past_a_very_long_line_to_the_end_lines_after_it() {
    let dir = common::scratch("b824-drain");
    let path = dir.join("rounds.jsonl");
    let long = format!(
        "{{\"ev\":\"end\",\"n\":1,\"t\":1,\"text\":\"{}\"}}\n",
        "x".repeat(3 * POLL_BYTES)
    );
    assert!(long.len() > 2 * POLL_BYTES && MAX_LINE < POLL_BYTES);
    let after = r#"{"ev":"end","n":2,"t":9,"status":"completed","iterations":1,"inputTokens":12,"outputTokens":3,"cacheReadTokens":4,"cacheWriteTokens":5,"text":""}"#;
    std::fs::write(&path, format!("{long}{after}\n")).unwrap();
    let mut reader = RoundsReader::new(path);
    let mut found = Vec::new();
    read_to_end(&mut reader, 64, |p| found.extend(p.usage));
    assert_eq!(found, vec![run(9, usage(3, 3, 4, 5))]);
}

/// A run that failed before its first model response (the account cannot call the model) ends with counts of zero. It adds nothing: a run
/// whose only outside runs spent nothing keeps `usage: null` (not measured), as it did, and does not show measured zeros.
#[test]
fn b824_outside_runs_that_spent_nothing_leave_the_usage_the_run_had() {
    let mut o = OutsideUsage::default();
    o.run_ended(run(150, Usage::default()));
    o.run_ended(run(300, Usage::default()));
    assert_eq!(o.total(), None, "no prompt answered, every run empty");
    // an empty run beside one that spent adds nothing to the sum
    o.run_ended(run(400, usage(2, 3, 0, 0)));
    assert_eq!(o.total(), Some(usage(2, 3, 0, 0)));
}

/// A run is matched to a prompt by its end alone (module documentation): a round the engine began inside a prompt's window and ended after the
/// answer is outside every window and is counted in full, though a call it finished before the answer may be in the prompt's figure as well
/// (at most about one call; the totals are an estimate, see the user guide).
#[test]
fn b824_a_run_that_began_inside_a_window_and_ended_after_it_is_counted_in_full() {
    let mut o = OutsideUsage::default();
    o.prompt_sent(1, 100);
    o.prompt_replied(1, 200, Some(usage(1, 1, 1, 1)));
    // began at 150, ended at 250: its end is after the answer
    o.run_ended(run(250, usage(5, 6, 7, 8)));
    assert_eq!(o.total(), Some(usage(5, 6, 7, 8)));
    // the run before the window began and the run inside the next one
    o.run_ended(run(90, usage(1, 0, 0, 0)));
    assert_eq!(o.total(), Some(usage(6, 6, 7, 8)));
}
