//! The records folder of a run, whole: the agent in process against the fake AWS and the fake ACP engine, over a
//! repository that holds its lifecycle records in the earlier folder `.cah`, in `.ham`, in neither or in both.
//!
//! The folder is chosen once, when the repository is restored. A repository with state only in `.cah` is worked in `.cah`: the first line of the
//! first prompt names the folder, so the engine and the harness agree. A repository with state in `.ham`, or in neither folder, gets the user's text
//! as typed. A repository with state in both folders does not start: no model call, and the words name both folders.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, PROMPT};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::runtime::TurnOutcome;

/// What the first line of the first prompt says when the run works in `.cah`.
const NAMES_CAH: &str = "Records folder of this run: .cah (this repository keeps its lifecycle records there; where the rules, skills and tools say .ham, use .cah).";

async fn run(d: &Direct) -> harness_protocol::runtime::InvocationResponse {
    agentd::direct::run_with(&d.app, d.req.clone(), &agentd::direct::Deps::default()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_keeps_its_records_in_cah_gets_a_first_line_that_names_the_folder() {
    let d = Direct::start(Opts {
        repo_files: vec![(".cah/lifecycle/LOG.md", "# log\n")],
        ..Opts::default()
    })
    .await;
    let resp = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let prompts = d.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let mut lines = prompts[0].lines();
    assert_eq!(lines.next(), Some(NAMES_CAH), "{}", prompts[0]);
    assert_eq!(lines.next(), Some(""), "a blank line, then the user's text");
    assert_eq!(
        &prompts[0][NAMES_CAH.len() + 2..],
        PROMPT,
        "the user's text as typed"
    );
    println!(
        "RECORDS TRIGGER PRESENT: a repository with .cah/lifecycle, first prompt line names .cah"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_records_in_ham_or_in_neither_folder_gets_the_text_as_typed() {
    for files in [
        vec![(".ham/lifecycle/LOG.md", "# log\n")],
        vec![(".ham/test-command", "pytest\n")],
        Vec::new(),
    ] {
        let d = Direct::start(Opts {
            repo_files: files.clone(),
            ..Opts::default()
        })
        .await;
        let resp = run(&d).await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{files:?}: {resp:?}");
        assert_eq!(d.prompts_received(), [PROMPT.to_string()], "{files:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn state_in_both_folders_stops_the_run_before_any_model_call_and_names_both() {
    let d = Direct::start(Opts {
        repo_files: vec![
            (".ham/lifecycle/LOG.md", "# log\n"),
            (".cah/lifecycle/LOG.md", "# log\n"),
        ],
        ..Opts::default()
    })
    .await;
    let resp = run(&d).await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        d.prompts_received().is_empty(),
        "the engine never got a prompt"
    );
    let detail = s.detail.unwrap();
    assert!(
        detail.contains(".ham") && detail.contains(".cah"),
        "{detail}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_new_folder_does_not_hide_the_earlier_one() {
    // `.ham/` exists in the commit only through a file that is no state (a placeholder), `.cah/` holds the records.
    let d = Direct::start(Opts {
        repo_files: vec![
            (".ham/README", "placeholder\n"),
            (".cah/lifecycle/GOAL.md", "1. the tests pass\n"),
        ],
        ..Opts::default()
    })
    .await;
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let prompts = d.prompts_received();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].starts_with(NAMES_CAH), "{}", prompts[0]);
}
