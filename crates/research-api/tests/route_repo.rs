//! `POST /internal/research/repo`, `/job` and `/limits`: the job start, its idempotency, the lazy time limits of a job, the owner check and
//! the limits answer. Ordinary input only.

mod support;

use harness_protocol::research::{
    ItemKind, ItemRef, JobState, ResearchError, ResearchErrorCode, route,
};
use research_api::fakes::RecordingInvoker;
use research_api::ports::{Clock, Table};
use serde_json::json;
use support::*;

const REPO: &str = "https://github.com/sindresorhus/is-plain-obj";

fn start_body(key: &str) -> serde_json::Value {
    json!({"url": REPO, "ref": "main", "idempotencyKey": key})
}

#[tokio::test]
async fn a_repository_request_creates_one_queued_job_and_starts_it_once() {
    let d = deps(fixture_provider());
    let (status, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    assert_eq!(status, 202, "{v}");
    assert_eq!(v["state"], "queued");
    let job_id = v["jobId"].as_str().unwrap().to_string();
    assert!(v["pollAfterMs"].as_u64().unwrap() > 0);
    assert_eq!(v["usage"]["repos"], 1);
    assert_eq!(d.invoker.events().len(), 1);
    assert_eq!(d.invoker.events()[0].job_id, job_id);
    assert_eq!(d.invoker.events()[0].run, RUN);
    let jobs = d.table.jobs(RUN);
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (
            jobs[0].state,
            jobs[0].url.as_str(),
            jobs[0].git_ref.as_deref()
        ),
        (JobState::Queued, REPO, Some("main"))
    );

    // The same key and arguments: the same job, no second start, no second charge.
    let (status, v2, _) = call(&d, route::REPO, start_body("repo-1")).await;
    assert_eq!((status, v2["jobId"].as_str()), (202, Some(job_id.as_str())));
    assert_eq!(d.invoker.events().len(), 1);
    assert_eq!(d.table.usage(RUN).await.unwrap().repos, 1);
    // The same key with other arguments conflicts.
    let (status, v3, _) = call(
        &d,
        route::REPO,
        json!({"url": REPO, "ref": "v2", "idempotencyKey": "repo-1"}),
    )
    .await;
    assert_eq!((status, v3["code"].as_str()), (409, Some("conflict")));

    // One audit object per call, naming the job.
    let a = audits(&d, RUN);
    assert_eq!(a.iter().filter(|x| x["op"] == "repo").count(), 3);
    assert!(
        a.iter()
            .any(|x| x["jobId"] == job_id.as_str() && x["input"]["urls"] == json!([REPO]))
    );
}

#[tokio::test]
async fn repository_requests_with_a_wrong_host_name_ref_or_missing_key_are_refused() {
    let d = deps(fixture_provider());
    let cases = [
        (
            json!({"url": "https://gitlab.com/o/r", "idempotencyKey": "k"}),
            403,
            "refused_host",
        ),
        (
            json!({"url": "https://github.com/o/r/tree/main", "idempotencyKey": "k"}),
            403,
            "refused_url",
        ),
        (
            json!({"url": "http://github.com/o/r", "idempotencyKey": "k"}),
            403,
            "refused_url",
        ),
        (json!({"url": REPO}), 400, "usage"),
        (
            json!({"url": REPO, "ref": "a b", "idempotencyKey": "k"}),
            400,
            "usage",
        ),
        (
            json!({"url": REPO, "paths": ["../x"], "idempotencyKey": "k"}),
            400,
            "usage",
        ),
    ];
    for (body, status, code) in cases {
        let (s, v, _) = call(&d, route::REPO, body.clone()).await;
        assert_eq!((s, v["code"].as_str()), (status, Some(code)), "{body}");
    }
    assert!(d.invoker.events().is_empty());
    assert_eq!(d.table.usage(RUN).await.unwrap().repos, 0);
}

#[tokio::test]
async fn the_repository_count_of_a_run_is_limited() {
    let d = deps(fixture_provider());
    let mut p = policy(RUN);
    p.limits.repos = 1;
    d.table.put_policy(p);
    assert_eq!(call(&d, route::REPO, start_body("a")).await.0, 202);
    let (status, v, _) = call(
        &d,
        route::REPO,
        json!({"url": "https://github.com/a/b", "idempotencyKey": "b"}),
    )
    .await;
    assert_eq!((status, v["code"].as_str()), (429, Some("limit_calls")));
}

#[tokio::test]
async fn a_job_that_could_not_be_started_is_failed_and_says_so() {
    let mut d = deps(fixture_provider());
    d.invoker = RecordingInvoker::refusing();
    let (status, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    assert_eq!((status, v["code"].as_str()), (500, Some("internal")));
    let jobs = d.table.jobs(RUN);
    assert_eq!(jobs[0].state, JobState::Failed);
    assert_eq!(
        jobs[0].error.as_ref().unwrap().code,
        ResearchErrorCode::Internal
    );
}

fn done_item() -> ItemRef {
    ItemRef {
        id: "i-0001".into(),
        kind: ItemKind::Repo,
        origin: REPO.into(),
        manifest_key: format!("runs/{RUN}/agents/direct/research/items/i-0001/manifest.json"),
        manifest_sha256: "ab".repeat(32),
        body_key: format!("runs/{RUN}/agents/direct/research/items/i-0001/body.tar.gz"),
        body_sha256: "cd".repeat(32),
        bytes: 2101,
        licence: Some("MIT".into()),
        truncated: false,
        files: Some(15),
        dropped: Some(2),
    }
}

#[tokio::test]
async fn the_job_route_reports_the_state_and_the_finished_item() {
    let d = deps(fixture_provider());
    let (_, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    let id = v["jobId"].as_str().unwrap().to_string();
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": id})).await;
    assert_eq!((status, v["state"].as_str()), (202, Some("queued")));

    d.table
        .job_move(
            RUN,
            &id,
            &[JobState::Queued],
            JobState::Running,
            NOW_MS / 1000,
            None,
            None,
        )
        .await
        .unwrap();
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": id})).await;
    assert_eq!((status, v["state"].as_str()), (202, Some("running")));

    d.table
        .job_move(
            RUN,
            &id,
            &[JobState::Running],
            JobState::Succeeded,
            NOW_MS / 1000,
            Some(&done_item()),
            None,
        )
        .await
        .unwrap();
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": id})).await;
    assert_eq!(
        (status, v["state"].as_str(), v["item"]["id"].as_str()),
        (200, Some("succeeded"), Some("i-0001"))
    );
    assert_eq!(v["item"]["licence"], "MIT");
    // A repeat of the start after the job finished answers the finished job.
    let (status, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    assert_eq!((status, v["state"].as_str()), (200, Some("succeeded")));
    assert_eq!(d.invoker.events().len(), 1);
}

#[tokio::test]
async fn a_failed_job_reports_its_error() {
    let d = deps(fixture_provider());
    let (_, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    let id = v["jobId"].as_str().unwrap().to_string();
    let err = ResearchError::new(ResearchErrorCode::NotFound, "GitHub has no such repository");
    d.table
        .job_move(
            RUN,
            &id,
            &[JobState::Queued],
            JobState::Failed,
            NOW_MS / 1000,
            None,
            Some(&err),
        )
        .await
        .unwrap();
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": id})).await;
    assert_eq!(
        (status, v["state"].as_str(), v["error"]["code"].as_str()),
        (200, Some("failed"), Some("not_found"))
    );
}

#[tokio::test]
async fn a_job_that_never_starts_or_stops_updating_fails_when_it_is_read() {
    let d = deps(fixture_provider());
    let (_, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    let id = v["jobId"].as_str().unwrap().to_string();
    d.clock.advance_s(119);
    let (_, v, _) = call(&d, route::JOB, json!({"jobId": id})).await;
    assert_eq!(v["state"], "queued", "two minutes have not passed");
    d.clock.advance_s(2);
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": id})).await;
    assert_eq!(
        (status, v["state"].as_str(), v["error"]["code"].as_str()),
        (200, Some("failed"), Some("not_started"))
    );

    // A running job not updated for 16 minutes times out.
    let (_, v, _) = call(
        &d,
        route::REPO,
        json!({"url": REPO, "ref": "dev", "idempotencyKey": "repo-2"}),
    )
    .await;
    let id2 = v["jobId"].as_str().unwrap().to_string();
    let now_s = d.clock.now_ms() / 1000;
    d.table
        .job_move(
            RUN,
            &id2,
            &[JobState::Queued],
            JobState::Running,
            now_s,
            None,
            None,
        )
        .await
        .unwrap();
    d.clock.advance_s(16 * 60 - 1);
    assert_eq!(
        call(&d, route::JOB, json!({"jobId": id2})).await.1["state"],
        "running"
    );
    d.clock.advance_s(2);
    let (_, v, _) = call(&d, route::JOB, json!({"jobId": id2})).await;
    assert_eq!(
        (v["state"].as_str(), v["error"]["code"].as_str()),
        (Some("failed"), Some("timeout"))
    );
}

#[tokio::test]
async fn another_runs_job_and_a_made_up_job_answer_not_found() {
    let d = deps(fixture_provider());
    d.table.put_policy(policy(OTHER_RUN));
    let (_, v, _) = call(&d, route::REPO, start_body("repo-1")).await;
    let id = v["jobId"].as_str().unwrap().to_string();
    let mut req = request(route::JOB, &json!({"jobId": id}));
    req.caller_arn = Some(caller_arn(OTHER_RUN));
    let resp = research_api::handler::handle(&d, &req).await;
    assert_eq!(
        (resp.status, resp.log.code.as_deref()),
        (404, Some("not_found"))
    );
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": "j-doesnotexist00"})).await;
    assert_eq!((status, v["code"].as_str()), (404, Some("not_found")));
    let (status, v, _) = call(&d, route::JOB, json!({"jobId": "bad id"})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")));
}

#[tokio::test]
async fn limits_reports_use_and_caps_touches_nothing_and_leaves_no_audit_object() {
    let d = deps(fixture_provider().then_search(Ok(outcome(vec![]))));
    assert_eq!(call(&d, route::SEARCH, q("one")).await.0, 200);
    let audit_count = audits(&d, RUN).len();
    let (status, v, _) = call(&d, route::LIMITS, json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["provider"], "fixture");
    assert_eq!(v["usage"]["searches"], 1);
    assert_eq!(v["limits"]["searches"], 200);
    assert!(v["expiresAt"].as_u64().unwrap() > NOW_MS / 1000);
    assert_eq!(audits(&d, RUN).len(), audit_count);
    let (status, v, _) = call(&d, route::LIMITS, json!({"verbose": true})).await;
    assert_eq!((status, v["code"].as_str()), (400, Some("usage")));
    d.table.switch_off(RUN);
    let (status, v, _) = call(&d, route::LIMITS, json!({})).await;
    assert_eq!((status, v["code"].as_str()), (503, Some("disabled")));
}

#[tokio::test]
async fn a_start_the_limits_refuse_fails_its_job_and_a_repeat_answers_that_failure() {
    let d = deps(fixture_provider());
    let mut p = policy(RUN);
    p.limits.repos = 1;
    d.table.put_policy(p);
    assert_eq!(call(&d, route::REPO, start_body("a")).await.0, 202);
    let second = json!({"url": "https://github.com/a/b", "idempotencyKey": "b"});
    let (status, v, _) = call(&d, route::REPO, second.clone()).await;
    assert_eq!((status, v["code"].as_str()), (429, Some("limit_calls")));
    // The refused start left a failed job, not a queued one that nobody starts, and nothing was charged for it.
    let jobs = d.table.jobs(RUN);
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().any(|j| {
        j.state == JobState::Failed
            && j.error
                .as_ref()
                .is_some_and(|e| e.code == ResearchErrorCode::LimitCalls)
    }));
    assert_eq!(d.invoker.events().len(), 1, "only the first start ran");
    assert_eq!(d.table.usage(RUN).await.unwrap().repos, 1);
    // The same request again reads the failure.
    let (status, v, _) = call(&d, route::REPO, second).await;
    assert_eq!(
        (status, v["state"].as_str(), v["error"]["code"].as_str()),
        (200, Some("failed"), Some("limit_calls"))
    );
}
