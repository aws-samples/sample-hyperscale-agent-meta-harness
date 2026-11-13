//! After the independent review, in the `ham` crate: a follow-up goes to the backend that holds its run, an HTTP failure of a backend gets
//! a banner, the kept rows of a backend survive a first-page read, a POST that only fetches an address is not a change, a backend that
//! answers without any version is read-only, and the index backfill is no run called `index`. Stand-in planes record every call.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::backends::{Backends, BoxBuild, BuildFailure};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham_core::backends::{BackendDef, Local};
use ham_core::runfilter::{Refresh, RunsRead};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Handler = dyn Fn(&str, &str, &Value) -> Result<Response, TransportError> + Send + Sync;

/// One call as the log holds it: the name of the stand-in, the method and path, and the JSON body.
type Entry = (String, Value);
type Log = Arc<Mutex<Vec<Entry>>>;

struct Fake {
    name: String,
    handler: Arc<Handler>,
    log: Log,
}

impl Fake {
    fn new(
        log: &Log,
        name: &str,
        f: impl Fn(&str, &str, &Value) -> Result<Response, TransportError> + Send + Sync + 'static,
    ) -> Fake {
        Fake {
            name: name.to_string(),
            handler: Arc::new(f),
            log: Arc::clone(log),
        }
    }
}

impl ControlPlane for Fake {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        self.log
            .lock()
            .unwrap()
            .push((format!("{} {method} {path}", self.name), value.clone()));
        (self.handler)(method, path, &value)
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Ok(Response::default())
    }
}

fn answer(status: u16, tag: Option<(&str, &str)>, body: Value) -> Result<Response, TransportError> {
    let mut headers = BTreeMap::new();
    if let Some((name, value)) = tag {
        headers.insert(name.to_string(), value.to_string());
    }
    Ok(Response {
        status,
        headers,
        body: body.to_string().into_bytes(),
    })
}

/// An answer of the service itself: it carries the version header.
fn svc(status: u16, body: Value) -> Result<Response, TransportError> {
    answer(status, Some(("x-hamh-backend-version", "5.05")), body)
}

/// An answer of the gateway in front of the service: no version header.
fn gateway(status: u16, message: &str) -> Result<Response, TransportError> {
    answer(status, None, json!({ "message": message }))
}

fn row(id: &str, at: &str) -> Value {
    json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": at, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"})
}

fn def(alias: &str) -> BackendDef {
    BackendDef {
        alias: alias.into(),
        api_endpoint: Some(format!("https://{alias}.example/direct")),
        region: None,
        aws_profile: Some(alias.into()),
        customer_content: false,
    }
}

type Planes = BTreeMap<String, Result<Fake, BuildFailure>>;

async fn backends(aliases: &[&str], default: Option<&str>, planes: Planes) -> Arc<Backends<Fake>> {
    let planes = Arc::new(Mutex::new(planes));
    let factory = move |d: BackendDef| -> BoxBuild<Fake> {
        let r = planes.lock().unwrap().remove(&d.alias).unwrap_or_else(|| {
            Err(BuildFailure {
                text: format!("no plane for {}", d.alias),
                credentials: false,
            })
        });
        Box::pin(async move { r })
    };
    let defs = aliases.iter().map(|a| def(a)).collect();
    Arc::new(
        Backends::connect(defs, default.map(str::to_string), factory)
            .await
            .unwrap(),
    )
}

fn log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn lines(l: &Log) -> Vec<String> {
    l.lock().unwrap().iter().map(|(s, _)| s.clone()).collect()
}

fn is_list(p: &str) -> bool {
    p.starts_with("/v1/runs") && !p.starts_with("/v1/runs/")
}

fn quick_api(b: &Arc<Backends<Fake>>) -> Api<Backends<Fake>> {
    Api::new(Arc::clone(b)).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    })
}

// ---- F1: a follow-up started in the terminal app

fn env_with_rules(label: &str) -> (impl Fn(&str) -> Option<String>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("ham-b584-fix-{label}-{}", std::process::id()));
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

/// `work` holds `W1`, `spare` holds `R1`; both take a start and answer with a run id of their own.
fn two_stages(l: &Log, spare_run_probe: u16) -> Planes {
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(l, "work", |m, p, _| match (m, p) {
            ("POST", "/v1/direct/runs") => svc(202, json!({"runId": "WNEW", "status": "active"})),
            ("GET", p) if is_list(p) => {
                svc(200, json!({"runs": [row("W1", "2026-11-04T10:00:01Z")]}))
            }
            _ => svc(404, json!({"code": "not-found", "message": "no such run"})),
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(l, "spare", move |m, p, _| match (m, p) {
            ("POST", "/v1/direct/runs") => svc(202, json!({"runId": "SNEW", "status": "active"})),
            ("GET", "/v1/runs/R1") if spare_run_probe == 200 => {
                svc(200, json!({"runId": "R1", "status": "succeeded"}))
            }
            ("GET", p) if is_list(p) => {
                svc(200, json!({"runs": [row("R1", "2026-11-04T10:00:02Z")]}))
            }
            _ => svc(404, json!({"code": "not-found", "message": "no such run"})),
        })),
    );
    planes
}

fn posts(l: &Log) -> Vec<(String, Value)> {
    l.lock()
        .unwrap()
        .iter()
        .filter(|(s, _)| s.contains(" POST /v1/direct/runs"))
        .cloned()
        .collect()
}

async fn follow_up(api: &Api<Backends<Fake>>, label: &str) -> Result<String, String> {
    let (env, _root) = env_with_rules(label);
    match perform_with_env(
        api,
        Job::StartFollowUp {
            run_id: "R1".into(),
            text: "now add the docs".into(),
            head: None,
            engine: None,
        },
        &env,
    )
    .await
    {
        Done::FollowUpStarted { run_id, result } => {
            assert_eq!(run_id, "R1");
            result
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_a_follow_up_in_the_terminal_app_starts_on_the_backend_that_holds_the_source_run() {
    // No default backend: before the fix every follow-up was refused for want of a backend.
    let l = log();
    let b = backends(&["work", "spare"], None, two_stages(&l, 200)).await;
    let api = quick_api(&b);
    api.list_runs().await.unwrap();
    assert_eq!(follow_up(&api, "a").await, Ok("SNEW".to_string()));
    let sent = posts(&l);
    assert_eq!(sent.len(), 1, "{:?}", lines(&l));
    assert!(sent[0].0.starts_with("spare "), "{:?}", sent[0].0);
    assert_eq!(sent[0].1["continueFrom"]["runId"], "R1");

    // The same follow-up with the repository as its target.
    let (env, _root) = env_with_rules("a2");
    let done = perform_with_env(
        &api,
        Job::StartRepoFollowUp {
            run_id: "R1".into(),
            text: "now add the docs".into(),
            head: None,
            repo: "cc:my-service".parse().unwrap(),
            engine: None,
        },
        &env,
    )
    .await;
    assert!(
        matches!(&done, Done::FollowUpStarted { result: Ok(id), .. } if id == "SNEW"),
        "{done:?}"
    );
    assert!(posts(&l).iter().all(|(s, _)| s.starts_with("spare ")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_a_default_backend_never_takes_the_follow_up_of_a_run_that_lives_elsewhere() {
    let l = log();
    let b = backends(&["work", "spare"], Some("work"), two_stages(&l, 200)).await;
    let api = quick_api(&b);
    api.list_runs().await.unwrap();
    assert_eq!(follow_up(&api, "b").await, Ok("SNEW".to_string()));
    let sent = posts(&l);
    assert_eq!(sent.len(), 1, "{:?}", lines(&l));
    assert!(sent[0].0.starts_with("spare "), "{:?}", sent[0].0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_a_follow_up_of_a_run_not_listed_yet_asks_the_backends_and_sends_the_post_not_the_probe_answer()
 {
    let l = log();
    let b = backends(&["work", "spare"], None, two_stages(&l, 200)).await;
    let api = quick_api(&b);
    // The map knows nothing about R1: both backends are asked, and the new run is the one the POST answered.
    assert_eq!(follow_up(&api, "c").await, Ok("SNEW".to_string()));
    let all = lines(&l);
    assert!(all.contains(&"work GET /v1/runs/R1".to_string()), "{all:?}");
    assert!(
        all.contains(&"spare GET /v1/runs/R1".to_string()),
        "{all:?}"
    );
    let sent = posts(&l);
    assert_eq!(sent.len(), 1, "{all:?}");
    assert!(sent[0].0.starts_with("spare "));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b584_a_follow_up_of_a_run_no_backend_holds_or_that_could_not_be_checked_sends_nothing() {
    // Every backend says it has no such run.
    let l = log();
    let b = backends(&["work", "spare"], Some("work"), two_stages(&l, 404)).await;
    let api = quick_api(&b);
    let e = follow_up(&api, "d").await.unwrap_err();
    assert!(e.contains("R1") && e.contains("not found"), "{e}");
    assert!(posts(&l).is_empty(), "{:?}", lines(&l));

    // One backend says no and the other cannot answer: the run may be in the one that cannot, so nothing is started on the default.
    let l = log();
    let mut planes = two_stages(&l, 404);
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", |_m, _p, _| {
            Err(TransportError::Network("connection refused".into()))
        })),
    );
    let b = backends(&["work", "spare"], Some("work"), planes).await;
    let api = quick_api(&b);
    let e = follow_up(&api, "d2").await.unwrap_err();
    assert!(e.contains("could not check work"), "{e}");
    assert!(posts(&l).is_empty(), "{:?}", lines(&l));
}

#[tokio::test]
async fn b584_a_backend_named_for_a_follow_up_must_be_the_one_that_holds_the_run_when_the_client_knows()
 {
    let l = log();
    let b = backends(&["work", "spare"], None, two_stages(&l, 200)).await;
    let api = quick_api(&b);
    api.list_runs().await.unwrap();
    let body = json!({"prompt": "x", "continueFrom": {"runId": "R1"}}).to_string();
    let e = b
        .call_on(
            Some("work"),
            "POST",
            "/v1/direct/runs",
            &[],
            body.clone().into_bytes(),
        )
        .await
        .err()
        .unwrap();
    match e {
        TransportError::Local(Local::ContinueElsewhere {
            run_id,
            owner,
            named,
        }) => assert_eq!(
            (run_id.as_str(), owner.as_str(), named.as_str()),
            ("R1", "spare", "work")
        ),
        other => panic!("{other:?}"),
    }
    assert!(posts(&l).is_empty());
    // The right one, and a start that continues nothing, behave as before.
    b.call_on(
        Some("spare"),
        "POST",
        "/v1/direct/runs",
        &[],
        body.into_bytes(),
    )
    .await
    .unwrap();
    let plain = json!({"prompt": "x"}).to_string();
    let e = b
        .call("POST", "/v1/direct/runs", &[], plain.into_bytes())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::NoBackendChosen { .. })),
        "{e:?}"
    );
}

// ---- F3: an HTTP failure of a backend gets a banner

async fn banners_for(spare: Result<Response, TransportError>) -> Vec<String> {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", |_m, p, _| {
            if is_list(p) {
                svc(200, json!({"runs": [row("W1", "2026-11-04T10:00:01Z")]}))
            } else {
                svc(404, json!({}))
            }
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", move |_m, _p, _| spare.clone())),
    );
    let b = backends(&["work", "spare"], None, planes).await;
    let api = quick_api(&b);
    let RunsRead::All(runs) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    assert_eq!(runs.len(), 1, "the healthy backend stays on screen");
    b.backend_view().unwrap().banners()
}

#[tokio::test]
async fn b584_a_backend_that_answers_with_an_http_failure_gets_one_banner_that_says_what_happened()
{
    let b = banners_for(gateway(
        403,
        "User: arn:aws:sts::1:assumed-role/X/y is not authorized to perform: execute-api:Invoke",
    ))
    .await;
    assert_eq!(b.len(), 1, "{b:?}");
    assert!(
        b[0].starts_with("spare: access refused: HTTP 403")
            && b[0].contains("not authorized to perform")
            && b[0].contains("profile"),
        "{b:?}"
    );
    let b = banners_for(gateway(503, "Service Unavailable")).await;
    assert_eq!(b, ["spare: not reachable: HTTP 503: Service Unavailable"]);
    let b = banners_for(answer(
        502,
        Some(("x-hamh-backend-version", "5.05")),
        json!({"message": "boom"}),
    ))
    .await;
    assert_eq!(b, ["spare: not reachable: HTTP 502: boom"]);
    let b = banners_for(gateway(429, "Too Many Requests")).await;
    assert_eq!(b.len(), 1, "{b:?}");
    assert!(b[0].starts_with("spare: did not answer in time"), "{b:?}");
    let b = banners_for(gateway(
        403,
        "The security token included in the request is expired",
    ))
    .await;
    assert_eq!(b.len(), 1, "{b:?}");
    assert!(b[0].starts_with("spare: sign in needed"), "{b:?}");
    // The service's own answers about one request are no failure of the backend.
    for ok in [
        svc(404, json!({"code": "not-found", "message": "no such run"})),
        svc(403, json!({"code": "not-an-operator", "message": "no"})),
        svc(409, json!({"code": "conflict", "message": "no"})),
    ] {
        assert!(banners_for(ok).await.is_empty());
    }
}

#[tokio::test]
async fn b584_a_refused_backend_is_left_alone_by_the_timer_and_a_manual_refresh_tries_it_again() {
    let l = log();
    let healthy = Arc::new(Mutex::new(false));
    let h = Arc::clone(&healthy);
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", |_m, p, _| {
            if is_list(p) {
                svc(200, json!({"runs": [row("W1", "2026-11-04T10:00:01Z")]}))
            } else {
                svc(404, json!({}))
            }
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", move |_m, p, _| {
            if *h.lock().unwrap() && is_list(p) {
                svc(200, json!({"runs": [row("S1", "2026-11-04T10:00:02Z")]}))
            } else {
                gateway(403, "not authorized to perform: execute-api:Invoke")
            }
        })),
    );
    let b = backends(&["work", "spare"], None, planes).await;
    let api = quick_api(&b);
    api.read_runs(Refresh::Manual).await.unwrap();
    let spare_calls = |l: &Log| lines(l).iter().filter(|s| s.starts_with("spare ")).count();
    let first = spare_calls(&l);
    assert_eq!(first, 1);
    api.read_runs(Refresh::Timer).await.unwrap();
    assert_eq!(
        spare_calls(&l),
        first,
        "the timer leaves a refused backend alone"
    );
    *healthy.lock().unwrap() = true;
    let RunsRead::All(runs) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    assert_eq!(runs.len(), 2);
    assert!(b.backend_view().unwrap().banners().is_empty());
}

// ---- F4: the kept rows survive a first-page read

fn paged(
    rows: Vec<Value>,
    up: Arc<Mutex<bool>>,
) -> impl Fn(&str, &str, &Value) -> Result<Response, TransportError> + Send + Sync + 'static {
    move |_m, p, _| {
        if !*up.lock().unwrap() {
            return Err(TransportError::Network("connection refused".into()));
        }
        // Pages of 100 rows; the token is the offset.
        let from: usize = p
            .split("nextToken=")
            .nth(1)
            .map(|t| t.split('&').next().unwrap_or_default())
            .and_then(|t| t.trim_start_matches('o').parse().ok())
            .unwrap_or(0);
        let page: Vec<Value> = rows.iter().skip(from).take(100).cloned().collect();
        let mut body = json!({ "runs": page });
        if from + 100 < rows.len() {
            body["nextToken"] = json!(format!("o{}", from + 100));
        }
        svc(200, body)
    }
}

fn many(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let k = n - i;
            row(
                &format!("L{k:04}"),
                &format!(
                    "2026-11-04T{:02}:{:02}:{:02}Z",
                    k / 3600,
                    (k / 60) % 60,
                    k % 60
                ),
            )
        })
        .collect()
}

#[tokio::test]
async fn b584_a_failed_full_read_still_shows_every_row_of_the_last_full_read_after_a_timer_page() {
    let l = log();
    let up = Arc::new(Mutex::new(true));
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", |_m, p, _| {
            if is_list(p) {
                svc(200, json!({"runs": [row("W1", "2026-11-04T23:00:00Z")]}))
            } else {
                svc(404, json!({}))
            }
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", paged(many(250), Arc::clone(&up)))),
    );
    let b = backends(&["work", "spare"], None, planes).await;
    let api = quick_api(&b);
    let RunsRead::All(all) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    assert_eq!(all.len(), 251);
    // The timer reads the newest page only; the rows older than that page are kept.
    api.read_runs(Refresh::Timer).await.unwrap();
    // The backend goes down and the person presses r.
    *up.lock().unwrap() = false;
    let RunsRead::All(after) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        after.len(),
        251,
        "the 150 older rows of the backend that is down stay"
    );
    let mut ids: Vec<&str> = after.iter().map(|r| r.run_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 251, "one row for each run id");
    assert!(b.backend_view().unwrap().info("spare").unwrap().stale);
}

#[tokio::test]
async fn b584_a_first_page_without_a_next_page_replaces_the_kept_rows_and_a_new_run_joins_them() {
    let l = log();
    let rows = Arc::new(Mutex::new(many(3)));
    let r = Arc::clone(&rows);
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", |_m, _p, _| {
            svc(200, json!({"runs": []}))
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", move |_m, _p, _| {
            svc(200, json!({"runs": r.lock().unwrap().clone()}))
        })),
    );
    let b = backends(&["work", "spare"], None, planes).await;
    let api = quick_api(&b);
    let ids = |read: RunsRead| match read {
        RunsRead::All(r) | RunsRead::First { runs: r, .. } => {
            r.into_iter().map(|x| x.run_id).collect::<Vec<_>>()
        }
    };
    assert_eq!(
        ids(api.read_runs(Refresh::Timer).await.unwrap()),
        ["L0003", "L0002", "L0001"]
    );
    // L0002 is gone at the backend and L0004 is new: the page that says nothing follows is the whole list.
    *rows.lock().unwrap() = vec![
        row("L0004", "2026-11-04T00:01:04Z"),
        row("L0003", "2026-11-04T00:00:03Z"),
        row("L0001", "2026-11-04T00:00:01Z"),
    ];
    assert_eq!(
        ids(api.read_runs(Refresh::Timer).await.unwrap()),
        ["L0004", "L0003", "L0001"]
    );
}

// ---- F5, F7: what counts as a change

fn two_with(l: &Log, old_version: &'static str) -> Planes {
    let mut planes = Planes::new();
    planes.insert(
        "new".into(),
        Ok(Fake::new(l, "new", |_m, _p, _| {
            svc(200, json!({"runs": []}))
        })),
    );
    planes.insert(
        "old".into(),
        Ok(Fake::new(l, "old", move |_m, _p, _| {
            answer(
                200,
                Some(("x-hamh-backend-version", old_version)),
                json!({"url": "https://example.invalid/x", "runs": []}),
            )
        })),
    );
    planes
}

#[tokio::test]
async fn b584_a_post_that_only_fetches_an_address_works_against_a_read_only_backend_and_a_change_does_not()
 {
    let l = log();
    let b = backends(&["new", "old"], None, two_with(&l, "4.99")).await;
    // Teach the version and the owner of R.
    b.call_on(Some("old"), "GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap();
    for path in [
        "/v1/runs/R/artifacts/zip/download-url",
        "/v1/runs/R/snapshot/download-url",
    ] {
        let r = b
            .call_on(Some("old"), "POST", path, &[], b"{}".to_vec())
            .await;
        assert!(r.is_ok(), "{path}: {r:?}");
    }
    let e = b
        .call_on(
            Some("old"),
            "POST",
            "/v1/runs/R/cancel",
            &[],
            b"{}".to_vec(),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::ReadOnly { .. })),
        "{e:?}"
    );
    let sent = lines(&l);
    assert!(
        sent.contains(&"old POST /v1/runs/R/snapshot/download-url".to_string()),
        "{sent:?}"
    );
    assert!(!sent.iter().any(|s| s.contains("/cancel")), "{sent:?}");
}

#[tokio::test]
async fn b584_the_run_index_backfill_goes_to_one_backend_and_no_backend_is_asked_about_a_run_called_index()
 {
    let l = log();
    let b = backends(&["new", "old"], Some("new"), two_with(&l, "5.05")).await;
    b.call("POST", "/v1/runs/index", &[], b"{}".to_vec())
        .await
        .unwrap();
    let sent = lines(&l);
    assert!(
        sent.contains(&"new POST /v1/runs/index".to_string()),
        "{sent:?}"
    );
    assert!(
        !sent.iter().any(|s| s.contains("GET /v1/runs/index")),
        "{sent:?}"
    );
    assert!(
        !sent
            .iter()
            .any(|s| s.starts_with("old ") && s.contains("/v1/runs/index")),
        "{sent:?}"
    );
    // With no default backend a change that names no run has no home: it is refused, not sent to the first backend that is up.
    let l = log();
    let b = backends(&["new", "old"], None, two_with(&l, "5.05")).await;
    let e = b
        .call("POST", "/v1/runs/index", &[], b"{}".to_vec())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::NoBackendChosen { .. })),
        "{e:?}"
    );
    assert!(lines(&l).is_empty(), "{:?}", lines(&l));
}

// ---- F9: a backend that answers without any version

const EARLIER: &str = ham::version::EARLIER_BACKEND_HEADER;

#[tokio::test]
async fn b584_a_change_to_a_backend_that_reports_no_version_is_refused_but_a_failed_probe_lets_it_through()
 {
    let l = log();
    let mut planes = Planes::new();
    let mut add = |name: &str, whoami: Result<Response, TransportError>| {
        let name2 = name.to_string();
        planes.insert(
            name.to_string(),
            Ok(Fake::new(&l, &name2, move |m, p, _| {
                if p == "/v1/whoami" {
                    whoami.clone()
                } else if m == "POST" {
                    svc(200, json!({"ok": true}))
                } else {
                    svc(404, json!({}))
                }
            })),
        );
    };
    // Predates the tag: a plain 200 with no header at all.
    add(
        "untagged",
        answer(200, None, json!({"principalArn": "a", "account": "1"})),
    );
    // The probe itself fails, or the gateway answers it: nothing is known, so the change goes out as it did before the rule.
    add(
        "down",
        Err(TransportError::Network("connection refused".into())),
    );
    add("gw", gateway(503, "Service Unavailable"));
    // A stage of the earlier names says its version with the earlier header: valid and compared like the new one.
    add("early5", answer(200, Some((EARLIER, "5.01")), json!({})));
    add("early4", answer(200, Some((EARLIER, "4.01")), json!({})));
    let b = backends(
        &["untagged", "down", "gw", "early5", "early4"],
        None,
        planes,
    )
    .await;
    let change = |alias: &'static str| {
        let b = Arc::clone(&b);
        async move {
            b.call_on(
                Some(alias),
                "POST",
                "/v1/runs/R/cancel",
                &[],
                b"{}".to_vec(),
            )
            .await
        }
    };
    let e = change("untagged").await.err().unwrap();
    match e {
        TransportError::Local(Local::Untagged { alias, min }) => {
            assert_eq!((alias.as_str(), min.as_str()), ("untagged", "5.00"))
        }
        other => panic!("{other:?}"),
    }
    assert!(
        !lines(&l).iter().any(|s| s.starts_with("untagged POST")),
        "the change was never sent"
    );
    for ok in ["down", "gw", "early5"] {
        let r = change(ok).await;
        assert!(r.is_ok(), "{ok}: {r:?}");
    }
    let e = change("early4").await.err().unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::ReadOnly { .. })),
        "{e:?}"
    );
}
