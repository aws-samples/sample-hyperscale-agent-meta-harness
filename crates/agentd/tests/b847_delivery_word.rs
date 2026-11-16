//! The harness tells the engine how the run delivers, at the start of every session (`src/direct/delivery_word.rs`).
//!
//! A run with the zip target left out a workflow file its prompt asked for: the rules say that a pull request refuses such a file and that
//! a zip takes it, and the engine could not tell which run it was in. The target of the stored request says it
//! and agentd puts ONE short block of
//! its own words before whatever the engine reads first: the first prompt of the first session, the first prompt of a later session of
//! the run, and the first prompt of a fresh engine session inside a session. A run that switched the block off (`deliveryNote: false`
//! in the stored request, the user's `run.delivery_note`) gets the prompt it always got.
//!
//! Whole turns: the whole agent runs in process against the fake AWS and the fake ACP engine, and the tests read the `session/prompt`
//! requests the engine received.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::Config;
use agentd::direct::delivery_word::{HEADING, block};
use direct_support::{Answer, BUCKET, CfgEdit, Direct, Opts, PROMPT, RUN};
use harness_protocol::direct::{
    DirectDelivery, DirectSlice, InstructAction, REPOSITORY_REFUSES, s3 as keys,
    validate_direct_invocation,
};
use harness_protocol::runtime::{RepoSource, TurnOutcome};
use std::time::Duration;

const REPO: &str = "cc:hamh-e2e-python-calc";
const BRIEF: &str = "I added pct(a, b) and its test; the workflow file is still open.";

fn zip_block() -> String {
    block(Some(&DirectDelivery::zip())).unwrap()
}

fn pull_request_block() -> String {
    block(Some(&DirectDelivery::pull_request(REPO))).unwrap()
}

/// The options of a run that delivers as `delivery` says: its stored request names the target and the user kept the default, so the
/// worker writes the block. `None` is a run that switched the block off (`deliveryNote: false`), which is what the harness of
/// these tests writes unless a test says otherwise.
fn opts_of(delivery: Option<&DirectDelivery>, base: Opts) -> Opts {
    match delivery {
        None => base,
        Some(d) => Opts {
            delivery_note: None,
            target: d
                .repo
                .as_ref()
                .map(|repo| serde_json::json!({"mode": "repo", "repo": repo})),
            ..base
        },
    }
}

/// Runs the first session of a run to its end and returns the first prompt the engine received.
async fn first_prompt_of(delivery: Option<DirectDelivery>) -> String {
    let d = Direct::start(opts_of(delivery.as_ref(), Opts::default())).await;
    validate_direct_invocation(&d.req).expect("the invocation is valid");
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let prompts = d.prompts_received();
    assert!(!prompts.is_empty(), "the engine got a prompt");
    prompts[0].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn b847_the_first_prompt_of_a_zip_only_run_says_that_the_run_delivers_only_a_zip() {
    let p = first_prompt_of(Some(DirectDelivery::zip())).await;
    assert_eq!(
        p,
        format!("{}\n\n{PROMPT}", zip_block()),
        "the block, an empty line, then the user's text byte for byte"
    );
    assert!(p.starts_with(HEADING), "{p}");
    assert!(p.contains("this run delivers only a zip"), "{p}");
    assert!(p.contains("A zip takes files under"), "{p}");
    for path in REPOSITORY_REFUSES {
        assert!(p.contains(&format!("`{path}`")), "{path} in: {p}");
    }
    assert!(
        p.contains("name each one you add in your last message"),
        "{p}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b847_the_first_prompt_of_a_pull_request_run_names_the_repository_and_what_the_delivery_refuses()
 {
    let p = first_prompt_of(Some(DirectDelivery::pull_request(REPO))).await;
    assert_eq!(p, format!("{}\n\n{PROMPT}", pull_request_block()));
    assert!(
        p.contains(&format!("this run delivers a pull request to {REPO}.")),
        "{p}"
    );
    assert!(
        p.contains("the run ends blocked and delivers nothing"),
        "{p}"
    );
    assert!(
        p.contains("refuses a run that adds, changes or removes a file under"),
        "{p}"
    );
    assert!(
        p.contains("Hold such a change back and name the file in your last message"),
        "{p}"
    );
    assert!(!p.contains("only a zip"), "{p}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b847_a_run_that_switched_the_block_off_gets_the_prompt_as_the_user_typed_it() {
    let p = first_prompt_of(None).await;
    assert_eq!(p, PROMPT, "the block is off: nothing is added");
}

// ---- the second session of a run ----------------------------------------------------------------------------------------------------

/// `None` is a run that switched the block off.
async fn first_slice(delivery: Option<&DirectDelivery>) -> (Vec<u8>, String, String) {
    let d1 = Direct::start(opts_of(delivery, Opts::default())).await;
    let (_, resp) = d1.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d1.summary();
    let head = s.git.head_sha.clone().expect("slice 1 delivered a head");
    let base = s.git.base_sha.clone().expect("slice 1 has a base");
    assert_ne!(head, base, "the trigger: slice 1 changed the repository");
    let bundle = d1
        .aws
        .get(&keys::bundle_key(RUN))
        .expect("slice 1 bundled its work");
    (bundle, head, base)
}

/// The stored request is the run's, so it names the same target (`None`: the run switched the block off).
async fn second_slice(
    bundle: Vec<u8>,
    head: &str,
    base: &str,
    delivery: Option<&DirectDelivery>,
) -> Direct {
    let mut d2 = Direct::start(opts_of(delivery, Opts::default())).await;
    d2.aws.put(&keys::bundle_key_t(RUN, 1), bundle);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, &keys::bundle_key_t(RUN, 1)),
    };
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(1),
        brief: Some(BRIEF.to_string()),
    });
    validate_direct_invocation(&d2.req).expect("the invocation of slice 2 is valid");
    d2
}

#[tokio::test(flavor = "multi_thread")]
async fn b847_the_first_prompt_of_the_second_session_of_a_zip_only_run_says_it_again() {
    let zip = DirectDelivery::zip();
    let (bundle, head, base) = first_slice(Some(&zip)).await;
    let d2 = second_slice(bundle, &head, &base, Some(&zip)).await;
    d2.invoke().await;
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let p = &prompts[0];
    println!(
        "B847 TRIGGER PRESENT: the second session got the harness's continuation prompt: {}",
        p.contains("You are continuing a task")
    );
    assert!(
        p.contains("You are continuing a task"),
        "the trigger: this is the prompt of a later session, not the task alone: {p}"
    );
    assert!(
        p.starts_with(&format!("{}\n\n", zip_block())),
        "the block comes first: {p}"
    );
    assert_eq!(p.matches(HEADING).count(), 1, "the block is said once: {p}");
    assert!(
        p.contains(PROMPT),
        "the task as the user first wrote it: {p}"
    );
    assert!(p.contains(BRIEF), "the earlier session's last message: {p}");
}

#[tokio::test(flavor = "multi_thread")]
async fn b847_the_first_prompt_of_the_second_session_of_a_pull_request_run_says_it_again() {
    let pr = DirectDelivery::pull_request(REPO);
    let (bundle, head, base) = first_slice(Some(&pr)).await;
    let d2 = second_slice(bundle, &head, &base, Some(&pr)).await;
    d2.invoke().await;
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].starts_with(&format!("{}\n\n", pull_request_block())),
        "{}",
        prompts[0]
    );
    assert!(
        prompts[0].contains("You are continuing a task"),
        "{}",
        prompts[0]
    );
}

/// The second session of a run that switched the block off. The first prompt of the later session is the harness's continuation prompt as
/// it always was, with no block before it.
#[tokio::test(flavor = "multi_thread")]
async fn b847_the_second_session_of_a_run_that_switched_the_block_off_is_told_nothing_about_the_delivery()
 {
    let (bundle, head, base) = first_slice(None).await;
    let d2 = second_slice(bundle, &head, &base, None).await;
    d2.invoke().await;
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let p = &prompts[0];
    println!(
        "B847 TRIGGER PRESENT: the second session got the harness's continuation prompt: {}",
        p.contains("You are continuing a task")
    );
    assert!(
        p.starts_with("You are continuing a task"),
        "the trigger: the prompt of a later session, and its opening is the first thing the engine reads: {p}"
    );
    assert!(!p.contains(HEADING), "no block on the delivery: {p}");
    assert!(!p.contains("this run delivers"), "{p}");
    assert!(
        p.contains(PROMPT),
        "the task as the user first wrote it: {p}"
    );
    assert!(p.contains(BRIEF), "the earlier session's last message: {p}");
}

// ---- a run that works in the earlier records folder ---------------------------------------------------------------------------------

/// A run that works in the earlier records folder gets one line before the user's text that names the folder (`records::prompt_line`).
/// The block on the delivery comes before that line too: the block, an empty line, the line on the records folder, an empty line, the
/// user's text.
#[tokio::test(flavor = "multi_thread")]
async fn b847_the_block_comes_before_the_line_on_the_earlier_records_folder() {
    let folder = agentd::direct::records::EARLIER_BASE;
    let records_line = agentd::direct::records::prompt_line(folder)
        .expect("a run in the earlier records folder gets a line");
    // The trigger of the line: the repository holds a file of the lifecycle records in the earlier folder and none in the current one.
    let log: &'static str = Box::leak(format!("{folder}/lifecycle/LOG.md").into_boxed_str());
    for delivery in [DirectDelivery::zip(), DirectDelivery::pull_request(REPO)] {
        let d = Direct::start(opts_of(
            Some(&delivery),
            Opts {
                repo_files: vec![(log, "# log\n")],
                ..Opts::default()
            },
        ))
        .await;
        let (_, resp) = d.invoke().await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
        let prompts = d.prompts_received();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        println!(
            "B847 TRIGGER PRESENT: the first prompt holds the line on the records folder: {}",
            prompts[0].contains(&records_line)
        );
        assert!(
            prompts[0].contains(&records_line),
            "the trigger: the run works in the earlier records folder: {}",
            prompts[0]
        );
        assert_eq!(
            prompts[0],
            format!(
                "{}\n\n{records_line}\n\n{PROMPT}",
                block(Some(&delivery)).unwrap()
            ),
            "{delivery:?}: the block, the line on the records folder, the user's text"
        );
    }
}

// ---- a fresh engine session inside a session ------------------------------------------------------------------------------------------

fn quick() -> Option<CfgEdit> {
    Some(Box::new(move |c: &mut Config| {
        c.direct.provider_retries = 3;
        c.direct.engine_stop_resumes = 1;
        c.direct.provider_retry_pause = Duration::from_millis(20);
        c.direct.failover_wait = Duration::from_millis(100);
        c.direct.failover_wait_cap = Duration::from_millis(200);
        c.direct.failover_probe = false;
        c.direct.input_margin = Duration::ZERO;
        c.direct.input_min_window = Duration::from_secs(1);
    }))
}

/// A loop-guard stop, a question, the user's request for a fresh session (the scenario of the golden stream `question_and_fresh_session`).
/// The new session starts without the earlier conversation, so its first prompt says how the run delivers once more.
#[tokio::test(flavor = "multi_thread")]
async fn b847_a_fresh_session_inside_a_session_is_told_how_the_run_delivers() {
    for delivery in [DirectDelivery::zip(), DirectDelivery::pull_request(REPO)] {
        let d = Direct::start(opts_of(
            Some(&delivery),
            Opts {
                scenario: "stuck_engine_stop_then_done",
                engine_env: vec![
                    ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done".into()),
                    ("FAKE_ACP_RESTART_SCENARIO", "fresh_session_done".into()),
                ],
                input_wait_s: Some(30),
                edit_cfg: quick(),
                snapshot_every_s: Some(0),
                ..Opts::default()
            },
        ))
        .await;
        d.aws.instruct_on(
            "needs-input",
            Answer::new(InstructAction::NewSessionWithPrompt).text("Poll the log file instead."),
        );
        d.invoke().await;
        d.wait_healthy().await;
        let want = block(Some(&delivery)).unwrap();
        let prompts = d.prompts_received();
        let fresh: Vec<&String> = prompts
            .iter()
            .filter(|p| p.contains("You are continuing a task"))
            .collect();
        println!(
            "B847 TRIGGER PRESENT: {} prompt(s) of a fresh session among {}",
            fresh.len(),
            prompts.len()
        );
        assert_eq!(
            fresh.len(),
            1,
            "the trigger: one fresh session started: {prompts:?}"
        );
        assert!(
            fresh[0].starts_with(&format!("{want}\n\n")),
            "{delivery:?}: the fresh session reads the block first: {}",
            fresh[0]
        );
        assert_eq!(fresh[0].matches(HEADING).count(), 1, "{}", fresh[0]);
        assert!(
            prompts[0].starts_with(&format!("{want}\n\n")),
            "{delivery:?}: and so did the first session: {}",
            prompts[0]
        );
    }
}

/// The same scenario for a run that switched the block off: the fresh session's prompt is the one it always was.
#[tokio::test(flavor = "multi_thread")]
async fn b847_a_fresh_session_of_a_run_that_switched_the_block_off_is_told_nothing_about_the_delivery()
 {
    let d = Direct::start(Opts {
        scenario: "stuck_engine_stop_then_done",
        engine_env: vec![
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done".into()),
            ("FAKE_ACP_RESTART_SCENARIO", "fresh_session_done".into()),
        ],
        input_wait_s: Some(30),
        edit_cfg: quick(),
        snapshot_every_s: Some(0),
        ..Opts::default()
    })
    .await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt).text("Poll the log file instead."),
    );
    d.invoke().await;
    d.wait_healthy().await;
    let prompts = d.prompts_received();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("You are continuing a task")),
        "the trigger: a fresh session started: {prompts:?}"
    );
    for p in &prompts {
        assert!(!p.contains(HEADING), "{p}");
    }
}

// ---- the block with the look every 15 seconds, and with the words of a later session (the lanes of one release together) -------------

/// An invocation that names a look every 15 seconds and a delivery (what a service of protocol 1.61.0 sends) is one the worker takes:
/// the member agrees with the stored target, the session runs to its end, the engine reads the block first and then the prompt as typed,
/// and the copy of the end state is in the bucket.
#[tokio::test(flavor = "multi_thread")]
async fn b823_b847_a_session_with_a_look_every_15_seconds_and_a_delivery_runs_and_reads_the_block_first()
 {
    for delivery in [DirectDelivery::zip(), DirectDelivery::pull_request(REPO)] {
        let mut d = Direct::start(opts_of(Some(&delivery), Opts::default())).await;
        d.req.snapshot_every_s = Some(15);
        d.req.delivery = Some(delivery.clone());
        validate_direct_invocation(&d.req).expect("the invocation with both members is valid");
        let (_, resp) = d.invoke().await;
        assert_eq!(
            resp.outcome,
            TurnOutcome::Completed,
            "{delivery:?}: {resp:?}"
        );
        let prompts = d.prompts_received();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(
            prompts[0],
            format!("{}\n\n{PROMPT}", block(Some(&delivery)).unwrap()),
            "{delivery:?}: the block, then the prompt as typed"
        );
        let s = d.summary();
        println!(
            "B823-B847 TRIGGER PRESENT: the session changed the repository: {}",
            s.git.head_sha != s.git.base_sha
        );
        assert_ne!(
            s.git.head_sha, s.git.base_sha,
            "the trigger: the session has work to copy"
        );
        assert!(
            d.aws.get(&keys::snapshot_bundle_key(RUN)).is_some(),
            "{delivery:?}: the copy of the end state is stored"
        );
    }
}

/// The first prompt of a later session holds, in this order: the block on the delivery, the harness's opening of a continued task, the
/// task as the user first wrote it, and the words on how the earlier session ended (its machine reached its time limit, or the service
/// lost the session) with the sentence on the last saved copy. The block is the first thing the engine reads and is said once.
#[tokio::test(flavor = "multi_thread")]
async fn b842_b847_a_later_session_reads_the_block_first_and_then_the_words_on_the_earlier_session()
{
    const OPENING: &str = "You are continuing a task";
    const TASK: &str = "Task, as the user first wrote it:";
    const WHY: &str = "Why the earlier session stopped (written by the harness):\nThe earlier session of this run ended: its machine reached its time limit, or the service lost the session.";
    const COPY: &str = "This session starts in a new machine from the last saved copy of the earlier session's work. Changes the earlier session made after that copy may be missing.";
    for delivery in [DirectDelivery::zip(), DirectDelivery::pull_request(REPO)] {
        let (bundle, head, base) = first_slice(Some(&delivery)).await;
        let d2 = second_slice(bundle, &head, &base, Some(&delivery)).await;
        d2.invoke().await;
        let prompts = d2.prompts_received();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        let p = &prompts[0];
        let b = block(Some(&delivery)).unwrap();
        println!(
            "B842-B847 TRIGGER PRESENT: the later session got the block and the words on the earlier session: {} {}",
            p.contains(&b),
            p.contains(WHY)
        );
        let at = |needle: &str| {
            p.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} is in the prompt: {p}"))
        };
        let order = [at(&b), at(OPENING), at(TASK), at(WHY), at(COPY)];
        assert_eq!(
            order[0], 0,
            "the block is the first thing the engine reads: {p}"
        );
        assert_eq!(
            order[1],
            b.len() + 2,
            "the opening follows the block after one empty line: {p}"
        );
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "the block, the opening, the task, why the earlier session stopped, the last saved copy: {order:?} in {p}"
        );
        assert_eq!(p.matches(HEADING).count(), 1, "the block is said once: {p}");
        assert!(p.contains(PROMPT) && p.contains(BRIEF), "{p}");
    }
}

/// The same later session for a run that switched the note off.
/// The harness's opening of a continued task is the first thing the engine reads, the task and the words on how the earlier
/// session ended follow in the order they have with the block, and nothing about the delivery is said or recorded: the setting holds
/// in every session of the run, because every session reads the same stored request.
#[tokio::test(flavor = "multi_thread")]
async fn b842_b850_a_later_session_of_a_run_that_switched_the_note_off_reads_the_words_on_the_earlier_session_in_their_order()
 {
    const OPENING: &str = "You are continuing a task";
    const TASK: &str = "Task, as the user first wrote it:";
    const WHY: &str = "Why the earlier session stopped (written by the harness):\nThe earlier session of this run ended: its machine reached its time limit, or the service lost the session.";
    const COPY: &str = "This session starts in a new machine from the last saved copy of the earlier session's work. Changes the earlier session made after that copy may be missing.";
    let (bundle, head, base) = first_slice(None).await;
    let d2 = second_slice(bundle, &head, &base, None).await;
    d2.invoke().await;
    let prompts = d2.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let p = &prompts[0];
    println!(
        "B842-B850 TRIGGER PRESENT: the later session of a run with the note off got the words on the earlier session: {}",
        p.contains(WHY)
    );
    let at = |needle: &str| {
        p.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is in the prompt: {p}"))
    };
    let order = [at(OPENING), at(TASK), at(WHY), at(COPY)];
    assert_eq!(
        order[0], 0,
        "the opening is the first thing the engine reads: {p}"
    );
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "the opening, the task, why the earlier session stopped, the last saved copy: {order:?} in {p}"
    );
    assert!(!p.contains(HEADING), "no block on the delivery: {p}");
    assert!(p.contains(PROMPT) && p.contains(BRIEF), "{p}");
    // A later session stores its summary under its own turn.
    let bytes = d2
        .aws
        .get(&keys::summary_key_t(RUN, 2))
        .expect("the summary of the second session is in the bucket");
    let summary: harness_protocol::direct::DirectSummary = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        summary.delivery_note, None,
        "the later session records that nothing was said about the delivery"
    );
}
