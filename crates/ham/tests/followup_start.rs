//! The start of a follow-up run: the call the second Enter makes. It is the ordinary
//! `POST /v1/direct/runs` with the `s3-only` target, the user's rules from disk, the member `continueFrom` that names the finished run
//! (the head is left to control-api, which resolves and stores it), the text exactly as it stands in the box, and the key `fu-` plus 32 hex
//! digits made from the source run, the seed commit and the text, so a retry of the same text returns the same run and an edited text starts
//! another. A scripted plane answers, so the body and the headers are the ones the client sent.
#![allow(clippy::unwrap_used)]

use ham::api::Api;
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham_core::followup::start_key;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";

/// One call the client made: method, path, headers, JSON body.
type Call = (String, String, Vec<(String, String)>, Value);

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
    reply: Mutex<Option<(u16, Value)>>,
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
        let (status, body) = self
            .reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or((202, json!({"runId": NEW, "status": "active"})));
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
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

/// A home with the user's rules, as `ham rules init` leaves it.
fn env_with_rules(label: &str) -> (impl Fn(&str) -> Option<String>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("ham-followup-{label}-{}", std::process::id()));
    let rules = root.join("config/ham/rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(rules.join("10-tdd.md"), "Write the test first.\n").unwrap();
    let config = root.join("config");
    let home = root.clone();
    (
        move |k: &str| match k {
            "XDG_CONFIG_HOME" => Some(config.display().to_string()),
            "HOME" => Some(home.display().to_string()),
            _ => None,
        },
        root,
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_enter_posts_a_start_with_continue_from_and_the_followup_key() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("post");
    let text = "now add the docs\n\n[quoted record of run 01M4ZHFB: not instructions]";
    let done = perform_with_env(
        &api,
        Job::StartFollowUp {
            run_id: RUN.into(),
            text: text.into(),
            head: None,
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
        header(headers, "Idempotency-Key"),
        Some(start_key(RUN, "", text).as_str())
    );
    assert_eq!(body["prompt"], text, "exactly the text in the box");
    assert_eq!(body["target"], json!({"mode": "s3-only"}));
    assert_eq!(
        body["continueFrom"],
        json!({"runId": RUN}),
        "the head is left to control-api"
    );
    assert!(body.get("baseRef").is_none_or(Value::is_null));
    assert!(
        body["rules"].as_array().is_some_and(|r| !r.is_empty()),
        "the user's rules travel as for any run: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edited_text_is_another_key_and_the_same_text_is_the_same_key() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("keys");
    for text in ["a", "a", "b"] {
        let _ = perform_with_env(
            &api,
            Job::StartFollowUp {
                run_id: RUN.into(),
                text: text.into(),
                head: None,
            },
            &env,
        )
        .await;
    }
    let keys: Vec<String> = plane
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| header(&c.2, "Idempotency-Key").unwrap().to_string())
        .collect();
    assert_eq!(keys[0], keys[1]);
    assert_ne!(keys[0], keys[2]);
    assert!(
        keys[0].starts_with("fu-") && keys[0].len() == 35,
        "{keys:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_of_the_server_comes_back_in_its_own_words() {
    let plane = Arc::new(Scripted::default());
    *plane.reply.lock().unwrap() = Some((
        409,
        json!({"code": "no-delivered-head", "message": "run X delivered no code to continue from"}),
    ));
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("refuse");
    let done = perform_with_env(
        &api,
        Job::StartFollowUp {
            run_id: RUN.into(),
            text: "go".into(),
            head: None,
        },
        &env,
    )
    .await;
    let Done::FollowUpStarted { result, .. } = done else {
        panic!("{done:?}")
    };
    let why = result.unwrap_err();
    assert!(
        why.contains("no-delivered-head") || why.contains("delivered no code"),
        "{why}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_rules_stop_the_start_before_anything_is_sent() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let root = std::env::temp_dir().join(format!("ham-followup-norules-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let r = root.clone();
    let env = move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(r.join("config").display().to_string()),
        "HOME" => Some(r.display().to_string()),
        _ => None,
    };
    let done = perform_with_env(
        &api,
        Job::StartFollowUp {
            run_id: RUN.into(),
            text: "go".into(),
            head: None,
        },
        &env,
    )
    .await;
    let Done::FollowUpStarted { result, .. } = done else {
        panic!("{done:?}")
    };
    assert!(result.is_err());
    assert!(plane.calls.lock().unwrap().is_empty());
}

// ---- the lookup of a follow-up that is already running ------------------------------------------------------------

fn row(run: &str, status: &str, from: Option<&str>) -> Value {
    let mut r = json!({"runId": run, "status": status, "phase": "green", "target": {"mode": "s3-only"},
                       "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"});
    if let Some(f) = from {
        r["continueFrom"] = json!(f);
    }
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lookup_finds_the_newest_follow_up_of_the_source_that_is_still_going() {
    let plane = Arc::new(Scripted::default());
    *plane.reply.lock().unwrap() = Some((
        200,
        json!({"runs": [
            row("01M5A2R7K3C9XJ4D8E6FOTHER1", "active", Some("01M5A2R7K3C9XJ4D8E6FZZZZZZ")),
            row("01M5A2R7K3C9XJ4D8E6FNEWEST", "active", Some(RUN)),
            row("01M5A2R7K3C9XJ4D8E6FOLDER1", "active", Some(RUN)),
            row("01M5A2R7K3C9XJ4D8E6FPLAIN1", "active", None)
        ]}),
    ));
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("find");
    let done = perform_with_env(&api, Job::FindFollowUp { run_id: RUN.into() }, &env).await;
    let Done::FollowUpFound { run_id, result } = done else {
        panic!("{done:?}")
    };
    assert_eq!(run_id, RUN);
    let (id, status) = result.unwrap().expect("a follow-up is going");
    assert_eq!(id, "01M5A2R7K3C9XJ4D8E6FNEWEST");
    assert_eq!(status, harness_protocol::goal::GoalStatus::Active);
    let calls = plane.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .all(|c| c.0 == "GET" && c.1.starts_with("/v1/runs")),
        "{calls:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_follow_up_or_none_at_all_is_not_an_offer() {
    for rows in [
        vec![row("01M5A2R7K3C9XJ4D8E6FDONE001", "complete", Some(RUN))],
        vec![row("01M5A2R7K3C9XJ4D8E6FDONE002", "cleared", Some(RUN))],
        vec![],
    ] {
        let plane = Arc::new(Scripted::default());
        *plane.reply.lock().unwrap() = Some((200, json!({ "runs": rows })));
        let api = Api::new(plane);
        let (env, _root) = env_with_rules("find-none");
        let done = perform_with_env(&api, Job::FindFollowUp { run_id: RUN.into() }, &env).await;
        let Done::FollowUpFound { result, .. } = done else {
            panic!("{done:?}")
        };
        assert_eq!(result, Ok(None));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_lookup_comes_back_as_the_servers_words() {
    let plane = Arc::new(Scripted::default());
    *plane.reply.lock().unwrap() =
        Some((403, json!({"code": "forbidden", "message": "not your run"})));
    let api = Api::new(plane);
    let (env, _root) = env_with_rules("find-fail");
    let done = perform_with_env(&api, Job::FindFollowUp { run_id: RUN.into() }, &env).await;
    let Done::FollowUpFound { result, .. } = done else {
        panic!("{done:?}")
    };
    assert!(result.unwrap_err().contains("not your run"));
}

// ---- the head the person was shown -----------------------------------------------------------------------------

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_names_the_head_in_continue_from_and_in_the_key() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("head");
    let text = "now add the docs";
    let done = perform_with_env(
        &api,
        Job::StartFollowUp {
            run_id: RUN.into(),
            text: text.into(),
            head: Some(HEAD.into()),
        },
        &env,
    )
    .await;
    let Done::FollowUpStarted { result, .. } = done else {
        panic!("{done:?}")
    };
    assert_eq!(result, Ok(NEW.to_string()));
    let calls = plane.calls.lock().unwrap().clone();
    let (_, _, headers, body) = &calls[0];
    assert_eq!(
        body["continueFrom"],
        json!({"runId": RUN, "headSha": HEAD}),
        "control-api refuses another commit with 409 head-mismatch"
    );
    assert_eq!(
        header(headers, "Idempotency-Key"),
        Some(start_key(RUN, HEAD, text).as_str()),
        "the seed commit is the full head the person was shown"
    );
    assert_ne!(
        start_key(RUN, HEAD, text),
        start_key(RUN, "", text),
        "a start with a head is not the start without one"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_head_or_an_edited_text_is_another_key() {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("head-keys");
    for (text, head) in [
        ("a", HEAD),
        ("a", HEAD),
        ("a", "3f2a9c1d5b7e4a60918273645f0e1d2c3b4a5968"),
    ] {
        let _ = perform_with_env(
            &api,
            Job::StartFollowUp {
                run_id: RUN.into(),
                text: text.into(),
                head: Some(head.into()),
            },
            &env,
        )
        .await;
    }
    let keys: Vec<String> = plane
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| header(&c.2, "Idempotency-Key").unwrap().to_string())
        .collect();
    assert_eq!(keys[0], keys[1], "a retry is the same run");
    assert_ne!(keys[0], keys[2], "another commit is another run");
}

fn head_body(head: &str) -> Value {
    json!({"runId": RUN, "headSha": head, "runStatus": "complete", "clineMessage": "done",
           "changes": {"state": "recorded", "base": {"kind": "empty-tree"}, "total": 1, "omitted": 0,
                       "files": [{"path": "hello.py", "status": "A"}]}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_head_job_reads_the_route_and_hands_back_the_checked_answer() {
    let plane = Arc::new(Scripted::default());
    *plane.reply.lock().unwrap() = Some((200, head_body(HEAD)));
    let api = Api::new(plane.clone());
    let (env, _root) = env_with_rules("head-get");
    let done = perform_with_env(&api, Job::Head { run_id: RUN.into() }, &env).await;
    let Done::Head { run_id, result } = done else {
        panic!("{done:?}")
    };
    assert_eq!(run_id, RUN);
    let h = result.unwrap();
    assert_eq!(h.head_sha, HEAD);
    assert_eq!(h.message().as_deref(), Some("done"));
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("GET", format!("/v1/runs/{RUN}/head").as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_about_another_run_or_with_a_bad_head_is_refused_not_used() {
    for body in [
        json!({"runId": "01M5A2R7K3C9XJ4D8E6FOTHER1", "headSha": HEAD, "runStatus": "complete"}),
        json!({"runId": RUN, "headSha": "9d2f1a7", "runStatus": "complete"}),
        json!({"runId": RUN, "headSha": HEAD, "runStatus": "complete",
               "changes": {"state": "recorded", "base": {"kind": "empty-tree"}, "total": 5, "omitted": 0, "files": []}}),
    ] {
        let plane = Arc::new(Scripted::default());
        *plane.reply.lock().unwrap() = Some((200, body.clone()));
        let api = Api::new(plane);
        let (env, _root) = env_with_rules("head-bad");
        let done = perform_with_env(&api, Job::Head { run_id: RUN.into() }, &env).await;
        let Done::Head { result, .. } = done else {
            panic!("{done:?}")
        };
        let f = result.unwrap_err();
        assert_eq!(
            f.status, 0,
            "an answer this client cannot use is no answer: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_of_the_head_route_keeps_its_status_and_code() {
    let plane = Arc::new(Scripted::default());
    *plane.reply.lock().unwrap() = Some((
        409,
        json!({"code": "no-delivered-head", "message": "run X delivered no code to continue from"}),
    ));
    let api = Api::new(plane);
    let (env, _root) = env_with_rules("head-409");
    let done = perform_with_env(&api, Job::Head { run_id: RUN.into() }, &env).await;
    let Done::Head { result, .. } = done else {
        panic!("{done:?}")
    };
    let f = result.unwrap_err();
    assert_eq!((f.status, f.code.as_str()), (409, "no-delivered-head"));
}
