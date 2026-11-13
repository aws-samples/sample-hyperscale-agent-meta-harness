//! The follow-up of a repository run. The chat's second Enter starts a new run on the target the finished run used, so the new
//! run opens a pull request in the same repository: the confirmation says so before the start, `Job::StartRepoFollowUp` carries the repository,
//! and the start is the ordinary `POST /v1/direct/runs` with that target, `continueFrom` (the run and the delivered commit the person was
//! shown) and the key of the follow-up. A finished `--s3-only` run keeps the follow-up it had: a zip and no pull request.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use ham::api::Api;
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham::tui::{App, Effect, Msg};
use ham_core::followup::start_key;
use harness_protocol::api::v1::RunDetail;
use harness_protocol::target::RepoTarget;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";

fn flat(s: &str) -> String {
    s.chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn calc() -> RepoTarget {
    "cc:calc".parse().unwrap()
}

/// A finished run on screen whose summary names `target`.
fn finished_on(target: Value) -> App {
    let mut app = ready(finished_events());
    let detail: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "green", "target": target,
                    "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "add percent_change", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(detail)));
    app
}

fn repo_target() -> Value {
    json!({"mode": "repo", "repo": "cc:calc"})
}

/// The jobs that start a follow-up: (run, text, repository when it is a repository follow-up).
fn starts(effects: &[Effect]) -> Vec<(String, String, Option<RepoTarget>)> {
    jobs(effects)
        .into_iter()
        .filter_map(|j| match j {
            Job::StartFollowUp { run_id, text, .. } => Some((run_id.clone(), text.clone(), None)),
            Job::StartRepoFollowUp {
                run_id, text, repo, ..
            } => Some((run_id.clone(), text.clone(), Some(repo.clone()))),
            _ => None,
        })
        .collect()
}

fn found_none() -> Msg {
    Msg::Ext(Done::FollowUpFound {
        run_id: RUN.into(),
        result: Ok(None),
    })
}

/// Types the follow-up, presses the second Enter, answers "no follow-up is going" and returns the effects of the start.
fn second_enter(app: &mut App, clock: &mut Clock) -> (String, Vec<Effect>) {
    send(app, clock, "now add the docs");
    let draft = app.chat_text().unwrap();
    let looking = enter(app, clock, 400);
    assert!(
        jobs(&looking)
            .iter()
            .any(|j| matches!(j, Job::FindFollowUp { .. })),
        "{looking:?}"
    );
    (draft, app.update(found_none()))
}

#[test]
fn the_confirmation_of_a_repository_run_says_the_new_run_opens_a_pull_request_in_the_same_repository()
 {
    let mut app = finished_on(repo_target());
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    let text = flat(&screen(&app, 160, 50));
    for want in [
        "new billable run",
        RUN,
        "it targeted cc:calc",
        "Target: cc:calc",
        "pull request",
        &format!("harness/{RUN}"),
        "merged or deleted",
        "moved",
        "exactly the text in the box",
    ] {
        assert!(text.contains(want), "{want}\n{text}");
    }
    assert!(!text.contains("Target: s3-only"), "{text}");
    assert!(!text.contains("with no pull request"), "{text}");
}

#[test]
fn the_second_enter_of_a_repository_run_starts_a_follow_up_on_its_repository() {
    let mut app = finished_on(repo_target());
    let mut clock = Clock::new();
    let (draft, start) = second_enter(&mut app, &mut clock);
    assert_eq!(
        starts(&start),
        vec![(RUN.to_string(), draft, Some(calc()))],
        "{start:?}"
    );
}

#[test]
fn a_run_made_with_new_repo_is_followed_on_the_repository_it_made() {
    let mut app = finished_on(json!({"mode": "new-repo", "repo": "cc:calc"}));
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("Target: cc:calc") && text.contains("pull request"),
        "{text}"
    );
    let looking = enter(&mut app, &mut clock, 400);
    assert!(!jobs(&looking).is_empty());
    let start = app.update(found_none());
    assert_eq!(starts(&start).len(), 1);
    assert_eq!(starts(&start)[0].2, Some(calc()), "{start:?}");
}

#[test]
fn an_s3_only_run_keeps_the_follow_up_it_had() {
    let mut app = finished_on(json!({"mode": "s3-only"}));
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "now add the docs");
    let text = flat(&screen(&app, 160, 50));
    assert!(
        text.contains("Target: s3-only") && text.contains("with no pull request"),
        "{text}"
    );
    let (draft, start) = {
        let draft = app.chat_text().unwrap();
        enter(&mut app, &mut clock, 400);
        (draft, app.update(found_none()))
    };
    assert_eq!(
        starts(&start),
        vec![(RUN.to_string(), draft, None)],
        "{start:?}"
    );
}

// ---- the start itself --------------------------------------------------------------------------------------------------------

type Call = (String, String, Vec<(String, String)>, Value);

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push((
            method.to_string(),
            path.to_string(),
            headers.to_vec(),
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        ));
        Ok(Response {
            status: 202,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&json!({"runId": NEW, "status": "active"})).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("not used".into()))
    }
}

fn env_with_rules(label: &str) -> impl Fn(&str) -> Option<String> {
    let root =
        std::env::temp_dir().join(format!("ham-followup-repo-{label}-{}", std::process::id()));
    let rules = root.join("config/ham/rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(rules.join("10-tdd.md"), "Write the test first.\n").unwrap();
    let config = root.join("config");
    move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config.display().to_string()),
        "HOME" => Some(root.display().to_string()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_posts_the_repository_as_the_target_with_the_commit_the_person_was_shown() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let env = env_with_rules("post");
    let done = perform_with_env(
        &api,
        Job::StartRepoFollowUp {
            run_id: RUN.into(),
            text: "now add the docs".into(),
            head: Some(HEAD.into()),
            repo: calc(),
            engine: None,
        },
        &env,
    )
    .await;
    match done {
        Done::FollowUpStarted { run_id, result } => {
            assert_eq!(run_id, RUN);
            assert_eq!(result, Ok(NEW.to_string()));
        }
        other => panic!("{other:?}"),
    }
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let (method, path, headers, body) = &calls[0];
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/v1/direct/runs")
    );
    assert_eq!(
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Idempotency-Key"))
            .map(|(_, v)| v.as_str()),
        Some(start_key(RUN, HEAD, "now add the docs").as_str())
    );
    assert_eq!(body["target"], json!({"mode": "repo", "repo": "cc:calc"}));
    assert_eq!(body["continueFrom"], json!({"runId": RUN, "headSha": HEAD}));
    assert!(
        body.get("baseRef").is_none_or(Value::is_null),
        "the base is the delivered commit, not a branch: {body}"
    );
    assert_eq!(body["prompt"], "now add the docs");
    assert!(
        body["rules"].as_array().is_some_and(|r| !r.is_empty()),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_head_the_start_leaves_it_to_control_api() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let env = env_with_rules("nohead");
    perform_with_env(
        &api,
        Job::StartRepoFollowUp {
            run_id: RUN.into(),
            text: "t".into(),
            head: None,
            repo: calc(),
            engine: None,
        },
        &env,
    )
    .await;
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls[0].3["continueFrom"], json!({"runId": RUN}));
    assert_eq!(calls[0].3["target"]["mode"], "repo");
}
