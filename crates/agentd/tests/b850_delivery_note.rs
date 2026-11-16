//! A run can switch off the harness's block on how the run delivers, and the worker reads how the run delivers from the stored
//! request (`src/direct/delivery_word.rs`, `harness_protocol::direct::delivery_to_say`).
//!
//! * The service of this version writes no `delivery` member into the invocation (a worker image that predates the member refuses an
//!   invocation that holds it). The worker reads the target of the stored request and writes the block before the first prompt.
//! * `deliveryNote: false` in the stored request (the user's `run.delivery_note`) and the worker writes no block: the first words the
//!   engine reads are the prompt, as before the block existed.
//! * A member of the invocation (a service of protocol 1.61.0 writes one) that differs from the stored target: the worker stands behind
//!   neither and writes no block.
//! * `summary.json` records the block the worker wrote (`deliveryNote`), so the run's records say what the engine was told.
//! * A prompt that begins with a slash follows the block like any other prompt.
//!
//! Whole turns: the whole agent runs in process against the fake AWS and the fake ACP engine, and the tests read the `session/prompt`
//! requests the engine received.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::delivery_word::{HEADING, block, with_block};
use direct_support::{Direct, Opts, PROMPT};
use harness_protocol::direct::{DirectDelivery, DirectOutcome, validate_direct_invocation};
use harness_protocol::runtime::TurnOutcome;
use serde_json::json;

const REPO: &str = "cc:hamh-e2e-python-calc";

fn zip_block() -> String {
    block(Some(&DirectDelivery::zip())).unwrap()
}

fn pull_request_block() -> String {
    block(Some(&DirectDelivery::pull_request(REPO))).unwrap()
}

/// A run whose user kept the default: the stored request has no `deliveryNote`.
fn default_note() -> Opts {
    Opts {
        delivery_note: None,
        ..Opts::default()
    }
}

/// Runs the session to its end and returns the harness with the prompts the engine received.
async fn run(opts: Opts, member: Option<DirectDelivery>) -> Direct {
    let mut d = Direct::start(opts).await;
    d.req.delivery = member;
    validate_direct_invocation(&d.req).expect("the invocation is valid");
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(!d.prompts_received().is_empty(), "the engine got a prompt");
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn b850_a_zip_run_reads_the_block_although_the_invocation_names_no_delivery() {
    let d = run(default_note(), None).await;
    let p = &d.prompts_received()[0];
    println!(
        "B850 TRIGGER PRESENT: the invocation carries no delivery member: {}",
        d.req.delivery.is_none()
    );
    assert_eq!(
        *p,
        format!("{}\n\n{PROMPT}", zip_block()),
        "the block of the stored target, an empty line, then the user's text byte for byte"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b850_a_pull_request_run_reads_the_block_of_the_repository_its_stored_request_names() {
    let d = run(
        Opts {
            target: Some(json!({"mode": "repo", "repo": REPO})),
            ..default_note()
        },
        None,
    )
    .await;
    assert_eq!(
        d.prompts_received()[0],
        format!("{}\n\n{PROMPT}", pull_request_block())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b850_a_run_that_switched_the_note_off_reads_its_prompt_first() {
    // The switch in the stored request, with and without a member in the invocation (a service of protocol 1.61.0 writes one).
    for member in [None, Some(DirectDelivery::zip())] {
        let d = run(
            Opts {
                delivery_note: Some(false),
                ..Opts::default()
            },
            member.clone(),
        )
        .await;
        let prompts = d.prompts_received();
        assert_eq!(
            prompts[0], PROMPT,
            "{member:?}: the first words the engine reads are the prompt as typed"
        );
        for p in &prompts {
            assert!(!p.contains(HEADING), "{member:?}: {p}");
        }
        assert_eq!(
            d.summary().delivery_note,
            None,
            "{member:?}: the summary records that nothing was said"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b850_a_member_of_the_invocation_that_agrees_with_the_stored_target_changes_nothing() {
    let d = run(default_note(), Some(DirectDelivery::zip())).await;
    assert_eq!(
        d.prompts_received()[0],
        format!("{}\n\n{PROMPT}", zip_block())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b850_a_member_of_the_invocation_that_differs_from_the_stored_target_says_nothing() {
    // The stored request names the zip target; the invocation says a pull request.
    let d = run(default_note(), Some(DirectDelivery::pull_request(REPO))).await;
    let prompts = d.prompts_received();
    println!(
        "B850 TRIGGER PRESENT: the invocation and the stored request differ: {}",
        d.req.delivery != Some(DirectDelivery::zip())
    );
    assert_eq!(
        prompts[0], PROMPT,
        "two sources that differ: no block is written"
    );
    assert_eq!(d.summary().delivery_note, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn b850_the_summary_records_the_block_the_engine_read() {
    let d = run(default_note(), None).await;
    let s = d.summary();
    assert_eq!(
        s.delivery_note.as_deref(),
        Some(zip_block().as_str()),
        "the block, word for word"
    );
    s.validate().expect("the summary is valid");
    let e = s.to_evidence(harness_protocol::direct::HeadSource::Final);
    assert_eq!(e.delivery_note.as_deref(), Some(zip_block().as_str()));
}

// ---- a session in which no prompt was written ---------------------------------------------------------------------------------------

/// The summary records the block that was written before the first prompt of the session. An engine that exits before it answers
/// `initialize` gets no prompt, so it was told nothing: the summary of that session records no block, although the run keeps the default
/// and the same run's other sessions record theirs.
#[tokio::test(flavor = "multi_thread")]
async fn b850_a_session_whose_engine_got_no_prompt_records_no_block() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_EXIT", "7".into())],
        ..default_note()
    })
    .await;
    validate_direct_invocation(&d.req).expect("the invocation is valid");
    d.invoke().await;
    let s = d.summary();
    println!(
        "B850 TRIGGER PRESENT: the note is on and the engine got no prompt: {} (outcome {})",
        d.prompts_received().is_empty(),
        s.outcome.as_str()
    );
    assert!(
        d.prompts_received().is_empty(),
        "the trigger: the engine exited before a prompt was written"
    );
    assert_eq!(
        s.outcome,
        DirectOutcome::StoppedSetupFailed,
        "the trigger: the session failed in its setup"
    );
    assert_eq!(
        s.delivery_note, None,
        "no prompt was written, so the engine was told nothing and the summary records no block"
    );
    assert_eq!(
        s.to_evidence(harness_protocol::direct::HeadSource::Final)
            .delivery_note,
        None,
        "and the evidence of the zip has no section for it"
    );
}

// ---- a prompt that begins with a slash ----------------------------------------------------------------------------------------------

/// The engine's own code was read at the pinned commit (`engine/sources/cline.toml`): under ACP it sends the prompt text to the session as
/// it is and resolves no slash command (the resolver runs only in its interactive, agent and connector paths, and only for a text whose
/// first byte is the slash). So a prompt that begins with a slash is text to the engine with or without the block before it, and the block
/// keeps its place.
#[tokio::test(flavor = "multi_thread")]
async fn b850_a_prompt_that_begins_with_a_slash_follows_the_block_byte_for_byte() {
    let task = "/deep-planning add a percent_change(a, b) function with tests\n";
    let d = run(
        Opts {
            prompt: task.to_string(),
            ..default_note()
        },
        None,
    )
    .await;
    assert_eq!(
        d.prompts_received()[0],
        format!("{}\n\n{task}", zip_block())
    );
    // And with the note off it is the first thing the engine reads, as before the block existed.
    let off = run(
        Opts {
            prompt: task.to_string(),
            ..Opts::default()
        },
        None,
    )
    .await;
    assert_eq!(off.prompts_received()[0], task);
}

/// An engine that reads a prompt whose first character is a slash as one of its commands has a guard that puts one plain line before
/// such a prompt. With the block before it the prompt no longer starts with the slash, so the
/// guard adds nothing and the task reaches the model as a task; with the note off the guard does what it always did.
#[test]
fn b850_the_guard_of_an_engine_that_reads_commands_sees_the_block_first() {
    let guard = agentd::engine::config::prompt_guard("opencode").expect("the engine has a guard");
    let zip = DirectDelivery::zip();
    for word in ["/goal", "/init", "/review", "/compact"] {
        let task = format!("{word} do it");
        let with = with_block(Some(&zip), &task);
        assert_eq!(
            guard(&with),
            with,
            "{word}: the prompt starts with the block, and the guard leaves it as it is"
        );
        assert!(with.ends_with(&task), "{with}");
        let off = guard(&task);
        assert!(
            !off.trim_start().starts_with('/') && off.ends_with(&task),
            "{word}: with the note off the guard puts its line first: {off}"
        );
    }
    assert!(
        agentd::engine::config::prompt_guard("cline").is_none(),
        "the trigger: the default engine reads no prompt as a command and has no guard"
    );
}
