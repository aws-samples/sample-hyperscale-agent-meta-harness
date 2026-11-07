//! Steps 2 to 8 in the `ham` crate: the set of backends behind one `ControlPlane`, the merged reads of `Api`, the routing of every
//! call about one run, the start rules and the version rule, against stand-in planes that record what they were asked.
#![allow(clippy::unwrap_used)]

use ham::api::Api;
use ham::backends::{Backends, BoxBuild, BuildFailure};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::backends::{BackendDef, Health, Local};
use ham_core::runfilter::{Refresh, RunsRead};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Handler = dyn Fn(&str, &str) -> Result<Response, TransportError> + Send + Sync;

/// A stand-in for one stage: answers by a function of the method and path and records every call.
struct Fake {
    handler: Arc<Handler>,
    log: Arc<Mutex<Vec<String>>>,
    delay: Duration,
}

impl Fake {
    fn new(
        log: &Arc<Mutex<Vec<String>>>,
        name: &str,
        f: impl Fn(&str, &str) -> Result<Response, TransportError> + Send + Sync + 'static,
    ) -> Fake {
        let name = name.to_string();
        let log = Arc::clone(log);
        let _ = &name;
        Fake {
            handler: Arc::new(f),
            log,
            delay: Duration::ZERO,
        }
    }
}

impl ControlPlane for Fake {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let hdr = headers
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        self.log
            .lock()
            .unwrap()
            .push(format!("{method} {path} [{hdr}]"));
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        (self.handler)(method, path)
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

fn json(
    status: u16,
    version: Option<&str>,
    body: serde_json::Value,
) -> Result<Response, TransportError> {
    let mut headers = BTreeMap::new();
    if let Some(v) = version {
        headers.insert("x-hamh-backend-version".to_string(), v.to_string());
    }
    Ok(Response {
        status,
        headers,
        body: body.to_string().into_bytes(),
    })
}

fn row(id: &str, at: &str) -> serde_json::Value {
    serde_json::json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": at, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"})
}

fn def(alias: &str, cc: bool) -> BackendDef {
    BackendDef {
        alias: alias.into(),
        api_endpoint: Some(format!("https://{alias}.example/direct")),
        region: None,
        aws_profile: Some(alias.into()),
        customer_content: cc,
    }
}

type Planes = BTreeMap<String, Result<Fake, BuildFailure>>;

async fn backends(
    defs: Vec<BackendDef>,
    default: Option<&str>,
    planes: Planes,
) -> Result<Backends<Fake>, String> {
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
    Backends::connect(defs, default.map(str::to_string), factory).await
}

fn ok_list(
    rows: Vec<serde_json::Value>,
) -> impl Fn(&str, &str) -> Result<Response, TransportError> + Send + Sync + 'static {
    move |_m, p| {
        if p.starts_with("/v1/runs") && !p.starts_with("/v1/runs/") {
            json(200, Some("5.05"), serde_json::json!({"runs": rows}))
        } else {
            json(
                404,
                Some("5.05"),
                serde_json::json!({"code": "not-found", "message": "no such run"}),
            )
        }
    }
}

fn log() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn calls(l: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    l.lock().unwrap().clone()
}

// ---- one backend is what it was

#[tokio::test]
async fn b584_one_backend_forwards_every_call_unchanged_and_makes_no_extra_call() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "default".into(),
        Ok(Fake::new(
            &l,
            "default",
            ok_list(vec![row("R1", "2026-11-04T10:00:00Z")]),
        )),
    );
    let b = backends(vec![def("default", false)], None, planes)
        .await
        .unwrap();
    assert!(b.backend_view().is_none(), "no view, no banners, no column");
    let r = b
        .call(
            "GET",
            "/v1/runs/R1",
            &[("x".into(), "y".into())],
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 404);
    b.call("POST", "/v1/runs/R1/cancel", &[], Vec::new())
        .await
        .unwrap();
    b.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(
        calls(&l),
        [
            "GET /v1/runs/R1 [x=y]",
            "POST /v1/runs/R1/cancel []",
            "GET /v1/whoami []"
        ]
    );
}

#[tokio::test]
async fn b584_one_backend_that_cannot_be_built_aborts_with_the_text_of_before() {
    let mut planes = Planes::new();
    planes.insert(
        "default".into(),
        Err(BuildFailure {
            text: "apiEndpoint: not a URL".into(),
            credentials: false,
        }),
    );
    let e = backends(vec![def("default", false)], None, planes)
        .await
        .err()
        .unwrap();
    assert_eq!(e, "apiEndpoint: not a URL");
}

#[tokio::test]
async fn b584_with_two_backends_one_that_cannot_be_built_is_down_and_never_stops_the_other() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T10:00:00Z")]),
        )),
    );
    planes.insert(
        "spare".into(),
        Err(BuildFailure {
            text: "AWS credentials: no profile spare".into(),
            credentials: true,
        }),
    );
    let b = backends(vec![def("work", false), def("spare", false)], None, planes)
        .await
        .unwrap();
    let view = b.backend_view().expect("two backends have a view");
    assert_eq!(view.backends[0].health, Health::Up);
    assert!(
        matches!(view.backends[1].health, Health::SignedOut { .. }),
        "{:?}",
        view.backends[1].health
    );
    assert!(
        view.banners()[0].starts_with("spare: sign in needed"),
        "{:?}",
        view.banners()
    );
}

// ---- merged reads

#[tokio::test]
async fn b584_the_list_merges_every_backend_newest_first_and_learns_where_each_run_is() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![
                row("W2", "2026-11-04T10:00:09Z"),
                row("W1", "2026-11-04T10:00:01Z"),
            ]),
        )),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(
            &l,
            "spare",
            ok_list(vec![row("S5", "2026-11-04T10:00:05Z")]),
        )),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("spare", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(Arc::clone(&b));
    let RunsRead::All(runs) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["W2", "S5", "W1"]);
    assert_eq!(b.backend_of("S5").as_deref(), Some("spare"));
    assert_eq!(b.backend_of("W2").as_deref(), Some("work"));
}

#[tokio::test]
async fn b584_a_backend_that_is_down_or_slow_leaves_the_others_on_screen_and_gets_a_banner() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T10:00:01Z")]),
        )),
    );
    planes.insert(
        "lab".into(),
        Ok(Fake::new(&l, "lab", |_m, _p| {
            Err(TransportError::Network("connection refused".into()))
        })),
    );
    let mut slow = Fake::new(&l, "slow", ok_list(vec![row("L1", "2026-11-04T10:00:02Z")]));
    slow.delay = Duration::from_secs(30);
    planes.insert("slow".into(), Ok(slow));
    let b = Arc::new(
        backends(
            vec![def("work", false), def("lab", false), def("slow", false)],
            None,
            planes,
        )
        .await
        .unwrap(),
    );
    let api = Api::new(Arc::clone(&b))
        .with_list_deadlines(Duration::from_millis(200), Duration::from_millis(200));
    let started = std::time::Instant::now();
    let RunsRead::First { runs, has_more } = api.read_runs(Refresh::Timer).await.unwrap() else {
        panic!()
    };
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the slow backend must not hold the merge"
    );
    assert_eq!(
        runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["W1"]
    );
    assert!(!has_more);
    let banners = b.backend_view().unwrap().banners();
    assert_eq!(banners.len(), 2, "{banners:?}");
    assert!(
        banners
            .iter()
            .any(|x| x.starts_with("lab: not reachable") && x.contains("connection refused")),
        "{banners:?}"
    );
    assert!(
        banners
            .iter()
            .any(|x| x.starts_with("slow: did not answer in time")),
        "{banners:?}"
    );
}

#[tokio::test]
async fn b584_when_every_backend_fails_the_read_fails_and_names_each() {
    let l = log();
    let mut planes = Planes::new();
    for a in ["a", "b"] {
        planes.insert(
            a.into(),
            Ok(Fake::new(&l, a, |_m, _p| {
                Err(TransportError::Network("connection refused".into()))
            })),
        );
    }
    let b = Arc::new(
        backends(vec![def("a", false), def("b", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(b);
    let e = api
        .read_runs(Refresh::Manual)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("a:") && e.contains("b:") && e.contains("connection refused"),
        "{e}"
    );
}

#[tokio::test]
async fn b584_a_short_first_page_is_held_back_when_a_backend_has_more() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert("a".into(), Ok(Fake::new(&l, "a", |_m, _p| {
        json(200, Some("5.05"), serde_json::json!({"runs": [row("A9", "2026-11-04T10:00:09Z"), row("A5", "2026-11-04T10:00:05Z")], "nextToken": "t1"}))
    })));
    planes.insert(
        "b".into(),
        Ok(Fake::new(
            &l,
            "b",
            ok_list(vec![
                row("B7", "2026-11-04T10:00:07Z"),
                row("B4", "2026-11-04T10:00:04Z"),
            ]),
        )),
    );
    let b = Arc::new(
        backends(vec![def("a", false), def("b", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(b);
    let RunsRead::First { runs, has_more } = api.read_runs(Refresh::Timer).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["A9", "B7"]
    );
    assert!(has_more, "a list that holds rows back says there is more");
}

// ---- routing of calls about one run

#[tokio::test]
async fn b584_a_call_about_a_run_goes_to_the_backend_that_listed_it() {
    let l = log();
    let ll = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T10:00:01Z")]),
        )),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(
            &ll,
            "spare",
            ok_list(vec![row("S1", "2026-11-04T10:00:02Z")]),
        )),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("spare", false)], None, planes)
            .await
            .unwrap(),
    );
    Api::new(Arc::clone(&b))
        .read_runs(Refresh::Manual)
        .await
        .unwrap();
    l.lock().unwrap().clear();
    ll.lock().unwrap().clear();
    b.call("POST", "/v1/runs/S1/cancel", &[], Vec::new())
        .await
        .unwrap();
    b.call("GET", "/v1/runs/W1/events?shard=3", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(calls(&ll), ["POST /v1/runs/S1/cancel []"]);
    assert_eq!(calls(&l), ["GET /v1/runs/W1/events?shard=3 []"]);
}

fn owner_of(
    id: &'static str,
    version: &'static str,
) -> impl Fn(&str, &str) -> Result<Response, TransportError> + Send + Sync + 'static {
    move |m, p| {
        if p.starts_with(&format!("/v1/runs/{id}")) {
            json(200, Some(version), serde_json::json!({"ok": m}))
        } else {
            json(
                404,
                Some(version),
                serde_json::json!({"code": "not-found", "message": "no such run"}),
            )
        }
    }
}

#[tokio::test]
async fn b584_an_unknown_run_id_is_asked_of_every_backend_until_one_recognises_it() {
    let (l1, l2) = (log(), log());
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l1, "work", owner_of("X1", "5.05"))),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l2, "spare", owner_of("S9", "5.05"))),
    );
    let b = backends(vec![def("work", false), def("spare", false)], None, planes)
        .await
        .unwrap();
    let r = b
        .call("POST", "/v1/runs/S9/cancel", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert!(
        calls(&l1).iter().all(|c| c.starts_with("GET /v1/runs/S9")),
        "{:?}",
        calls(&l1)
    );
    assert!(
        calls(&l2).iter().any(|c| c == "POST /v1/runs/S9/cancel []"),
        "{:?}",
        calls(&l2)
    );
    assert_eq!(b.backend_of("S9").as_deref(), Some("spare"));
    // the next call needs no asking
    l1.lock().unwrap().clear();
    b.call("GET", "/v1/runs/S9/events", &[], Vec::new())
        .await
        .unwrap();
    assert!(calls(&l1).is_empty());
}

#[tokio::test]
async fn b584_a_run_id_found_in_two_backends_is_refused() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", owner_of("D1", "5.05"))),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", owner_of("D1", "5.05"))),
    );
    let b = backends(vec![def("work", false), def("spare", false)], None, planes)
        .await
        .unwrap();
    let e = b
        .call("POST", "/v1/runs/D1/cancel", &[], Vec::new())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::Ambiguous { .. })),
        "{e:?}"
    );
    assert!(!e.is_retryable());
    assert!(
        calls(&l).iter().all(|c| c.starts_with("GET")),
        "nothing was changed: {:?}",
        calls(&l)
    );
}

#[tokio::test]
async fn b584_nothing_found_while_a_backend_could_not_answer_is_never_no_such_run() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", owner_of("X1", "5.05"))),
    );
    planes.insert(
        "lab".into(),
        Ok(Fake::new(&l, "lab", |_m, _p| {
            Err(TransportError::Credentials(
                "the credentials expired. Run: aws sso login --profile lab".into(),
            ))
        })),
    );
    let b = backends(vec![def("work", false), def("lab", false)], None, planes)
        .await
        .unwrap();
    let e = b
        .call("GET", "/v1/runs/NOPE", &[], Vec::new())
        .await
        .err()
        .unwrap();
    let TransportError::Local(Local::CannotTell {
        unresolved,
        checked,
        ..
    }) = &e
    else {
        panic!("{e:?}")
    };
    assert_eq!(checked, &vec!["work".to_string()]);
    assert_eq!(unresolved[0].0, "lab");
    assert!(unresolved[0].1.contains("aws sso login --profile lab"));
}

#[tokio::test]
async fn b584_nothing_found_and_every_backend_answered_is_the_404_of_the_first_backend() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l, "work", owner_of("X1", "5.05"))),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", owner_of("X2", "5.05"))),
    );
    let b = backends(vec![def("work", false), def("spare", false)], None, planes)
        .await
        .unwrap();
    let r = b
        .call("GET", "/v1/runs/NOPE", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn b584_a_named_backend_skips_the_asking_and_a_404_there_is_final() {
    let (l1, l2) = (log(), log());
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l1, "work", owner_of("W1", "5.05"))),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l2, "spare", owner_of("S9", "5.05"))),
    );
    let b = backends(vec![def("work", false), def("spare", false)], None, planes)
        .await
        .unwrap();
    let r = b
        .call_on(Some("work"), "GET", "/v1/runs/S9", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(r.status, 404);
    assert!(calls(&l2).is_empty(), "the other backend was not asked");
    let e = b
        .call_on(Some("nope"), "GET", "/v1/runs/S9", &[], Vec::new())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::UnknownAlias { .. })),
        "{e:?}"
    );
}

// ---- starting

#[tokio::test]
async fn b584_a_start_needs_a_backend_when_there_are_two_and_none_is_the_default() {
    let l = log();
    let mut planes = Planes::new();
    for a in ["work", "spare"] {
        planes.insert(
            a.into(),
            Ok(Fake::new(&l, a, |_m, _p| {
                json(
                    200,
                    Some("5.05"),
                    serde_json::json!({"runId": "N1", "status": "active"}),
                )
            })),
        );
    }
    let b = backends(vec![def("work", false), def("spare", false)], None, planes)
        .await
        .unwrap();
    let e = b
        .call("POST", "/v1/direct/runs", &[], b"{}".to_vec())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::NoBackendChosen { .. })),
        "{e:?}"
    );
    assert!(calls(&l).is_empty(), "no call was made");
    b.call_on(
        Some("spare"),
        "POST",
        "/v1/direct/runs",
        &[],
        b"{}".to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(writes(&l).len(), 1);
    assert_eq!(
        b.backend_of("N1").as_deref(),
        Some("spare"),
        "the answer teaches the map"
    );
}

#[tokio::test]
async fn b584_default_backend_takes_a_start_that_names_none() {
    let (l1, l2) = (log(), log());
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l1, "work", |_m, _p| {
            json(
                200,
                Some("5.05"),
                serde_json::json!({"runId": "N1", "status": "active"}),
            )
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l2, "spare", |_m, _p| {
            json(
                200,
                Some("5.05"),
                serde_json::json!({"runId": "N2", "status": "active"}),
            )
        })),
    );
    let b = backends(
        vec![def("work", false), def("spare", false)],
        Some("spare"),
        planes,
    )
    .await
    .unwrap();
    b.call("POST", "/v1/direct/runs", &[], b"{}".to_vec())
        .await
        .unwrap();
    assert!(calls(&l1).is_empty());
    assert_eq!(writes(&l2).len(), 1);
}

#[tokio::test]
async fn b584_a_start_that_fails_is_retried_on_the_same_backend_with_the_same_key_and_never_on_another()
 {
    let (l1, l2) = (log(), log());
    let mut planes = Planes::new();
    let n = Arc::new(Mutex::new(0u32));
    let n2 = Arc::clone(&n);
    planes.insert(
        "work".into(),
        Ok(Fake::new(&l1, "work", move |m, _p| {
            if m != "POST" {
                return json(200, Some("5.05"), serde_json::json!({}));
            }
            let mut k = n2.lock().unwrap();
            *k += 1;
            if *k < 3 {
                json(
                    503,
                    None,
                    serde_json::json!({"code": "unavailable", "message": "later"}),
                )
            } else {
                json(
                    200,
                    None,
                    serde_json::json!({"runId": "N1", "status": "active"}),
                )
            }
        })),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l2, "spare", |_m, _p| {
            json(
                200,
                None,
                serde_json::json!({"runId": "N2", "status": "active"}),
            )
        })),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("spare", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(b)
        .with_retry(ham::api::Retry {
            attempts: 4,
            base: Duration::from_millis(1),
        })
        .on("work");
    let req = ham_core::request::direct_run_request(
        &match ham_core::command::parse(&["do it", "--s3-only", "--direct"])
            .unwrap()
            .command
        {
            ham_core::command::Command::StartDirect(a) => a,
            _ => panic!(),
        },
        ham_core::request::DirectInputs::default(),
        &ham_core::models::ModelPrefs::default(),
    )
    .unwrap();
    let started = ham::start::start_direct(&api, &req, "nonce").await.unwrap();
    assert_eq!(started.run_id, "N1");
    let keys: Vec<String> = writes(&l1)
        .iter()
        .map(|c| {
            c.split("idempotency-key=")
                .nth(1)
                .unwrap()
                .trim_end_matches(']')
                .to_string()
        })
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| *k == keys[0]), "{keys:?}");
    assert_eq!(
        keys[0],
        ham_core::request::direct_idempotency_key_on(&req, "nonce", Some("work"))
    );
    assert!(calls(&l2).is_empty(), "never on the other backend");
}

// ---- the version rule

#[tokio::test]
async fn b584_a_backend_below_the_minimum_is_read_only_and_the_title_says_mixed() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "new".into(),
        Ok(Fake::new(
            &l,
            "new",
            ok_list(vec![row("N1", "2026-11-04T10:00:01Z")]),
        )),
    );
    planes.insert(
        "old".into(),
        Ok(Fake::new(&l, "old", |m, _p| {
            json(200, Some("4.99"), serde_json::json!({"runs": [], "m": m}))
        })),
    );
    let b = backends(vec![def("new", false), def("old", false)], None, planes)
        .await
        .unwrap();
    // a read teaches the versions
    b.call_on(Some("old"), "GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap();
    b.call_on(Some("new"), "GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap();
    let view = b.backend_view().unwrap();
    assert_eq!(view.info("old").unwrap().version.as_deref(), Some("4.99"));
    assert_eq!(
        ham_core::backends::title_version(&view.backends).word(),
        "mixed"
    );
    l.lock().unwrap().clear();
    let e = b
        .call_on(Some("old"), "POST", "/v1/direct/runs", &[], b"{}".to_vec())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::ReadOnly { .. })),
        "{e:?}"
    );
    assert!(calls(&l).is_empty(), "the write was never sent");
    b.call_on(Some("old"), "GET", "/v1/whoami", &[], Vec::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn b584_a_write_to_a_backend_with_no_known_version_first_learns_it_with_one_read() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "new".into(),
        Ok(Fake::new(&l, "new", |_m, _p| {
            json(
                200,
                Some("5.05"),
                serde_json::json!({"runId": "N1", "status": "active"}),
            )
        })),
    );
    planes.insert(
        "old".into(),
        Ok(Fake::new(&l, "old", |_m, _p| {
            json(200, Some("6.01"), serde_json::json!({"x": 1}))
        })),
    );
    let b = backends(vec![def("new", false), def("old", false)], None, planes)
        .await
        .unwrap();
    let e = b
        .call_on(Some("old"), "POST", "/v1/runs/R/cancel", &[], Vec::new())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::UnknownMajor { .. })),
        "{e:?}"
    );
    assert_eq!(calls(&l), ["GET /v1/whoami []"], "one read, no write");
}

// ---- lapsed logins

#[tokio::test]
async fn b584_a_lapsed_login_pauses_the_timer_reads_of_that_backend_and_a_manual_refresh_tries_again()
 {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T10:00:01Z")]),
        )),
    );
    let lapsed = Arc::new(Mutex::new(true));
    let lapsed2 = Arc::clone(&lapsed);
    planes.insert(
        "spare".into(),
        Ok(Fake::new(&l, "spare", move |_m, _p| {
            if *lapsed2.lock().unwrap() {
                Err(TransportError::Credentials(
                    "the credentials expired. Run: aws sso login --profile spare".into(),
                ))
            } else {
                json(
                    200,
                    Some("5.05"),
                    serde_json::json!({"runs": [row("S1", "2026-11-04T10:00:03Z")]}),
                )
            }
        })),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("spare", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(Arc::clone(&b));
    api.read_runs(Refresh::Timer).await.unwrap();
    assert!(matches!(
        b.backend_view().unwrap().info("spare").unwrap().health,
        Health::SignedOut { .. }
    ));
    let spare_calls = |l: &Arc<Mutex<Vec<String>>>| calls(l).len();
    let before = spare_calls(&l);
    api.read_runs(Refresh::Timer).await.unwrap();
    assert_eq!(
        spare_calls(&l) - before,
        1,
        "only work was asked: the lapsed login is not polled"
    );
    *lapsed.lock().unwrap() = false;
    let RunsRead::All(runs) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["S1", "W1"]
    );
    assert_eq!(
        b.backend_view().unwrap().info("spare").unwrap().health,
        Health::Up
    );
}

#[tokio::test]
async fn b584_rows_of_a_backend_that_failed_stay_on_screen_marked_stale() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T10:00:01Z")]),
        )),
    );
    let up = Arc::new(Mutex::new(true));
    let up2 = Arc::clone(&up);
    planes.insert(
        "lab".into(),
        Ok(Fake::new(&l, "lab", move |_m, _p| {
            if *up2.lock().unwrap() {
                json(
                    200,
                    Some("5.05"),
                    serde_json::json!({"runs": [row("L1", "2026-11-04T10:00:03Z")]}),
                )
            } else {
                Err(TransportError::Network("connection refused".into()))
            }
        })),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("lab", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(Arc::clone(&b));
    api.read_runs(Refresh::Manual).await.unwrap();
    *up.lock().unwrap() = false;
    let RunsRead::All(runs) = api.read_runs(Refresh::Manual).await.unwrap() else {
        panic!()
    };
    assert_eq!(
        runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["L1", "W1"],
        "the last rows of lab are kept"
    );
    assert!(b.backend_view().unwrap().info("lab").unwrap().stale);
}

// ---- tags

#[tokio::test]
async fn b584_tag_edits_for_runs_of_two_backends_are_refused_in_one_batch() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T10:00:01Z")]),
        )),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(
            &l,
            "spare",
            ok_list(vec![row("S1", "2026-11-04T10:00:02Z")]),
        )),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("spare", false)], None, planes)
            .await
            .unwrap(),
    );
    Api::new(Arc::clone(&b))
        .read_runs(Refresh::Manual)
        .await
        .unwrap();
    l.lock().unwrap().clear();
    let body = serde_json::json!({"edits": [{"runId": "W1", "add": ["a"]}, {"runId": "S1", "add": ["a"]}]}).to_string().into_bytes();
    let e = b
        .call("POST", "/v1/tags/edit", &[], body)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::MixedBackends { .. })),
        "{e:?}"
    );
    assert!(calls(&l).is_empty());
    let body = serde_json::json!({"edits": [{"runId": "W1", "add": ["a"]}]})
        .to_string()
        .into_bytes();
    b.call("POST", "/v1/tags/edit", &[], body).await.unwrap();
    assert_eq!(calls(&l).len(), 1);
}

#[test]
fn b584_the_minimum_backend_version_is_of_the_clients_own_major_and_not_above_it() {
    let client = ham::version::CLIENT;
    let (cm, cc) = client.split_once('.').unwrap();
    let (mm, mc) = ham_core::backends::MIN_BACKEND.split_once('.').unwrap();
    assert_eq!(cm, mm, "the minimum is of this client's release line");
    assert!(
        mc <= cc,
        "the minimum {} is above the client {client}",
        ham_core::backends::MIN_BACKEND
    );
}

/// The calls that change something. A write to a backend whose version is not known yet is preceded by one read that learns it.
fn writes(l: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    calls(l)
        .into_iter()
        .filter(|c| c.starts_with("POST"))
        .collect()
}

fn titled(id: &str, at: &str, name: &str) -> serde_json::Value {
    let mut v = row(id, at);
    v["title"] = serde_json::json!("fix the thing");
    v["model"] = serde_json::json!("sonnet");
    v["name"] = serde_json::json!(name);
    v["nameSource"] = serde_json::json!("ai");
    v["tags"] = serde_json::json!(["project:x"]);
    v
}

#[tokio::test]
async fn b584_every_read_of_the_list_merges_and_the_index_entries_keep_their_rows() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![titled("W1", "2026-11-04T10:00:01Z", "first")]),
        )),
    );
    planes.insert(
        "spare".into(),
        Ok(Fake::new(
            &l,
            "spare",
            ok_list(vec![titled("S5", "2026-11-04T10:00:05Z", "second")]),
        )),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("spare", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(Arc::clone(&b));
    assert_eq!(api.target(), None);
    assert_eq!(api.on("work").target(), Some("work"));
    let runs = api.list_runs().await.unwrap();
    assert_eq!(
        runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["S5", "W1"]
    );
    let rows = api.list_run_rows().await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.name.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    for how in [Refresh::Timer, Refresh::Manual] {
        let read = api.read_runs_indexed(how).await.unwrap();
        assert_eq!(read.index.len(), 2, "{how:?}");
        assert!(
            read.index
                .iter()
                .all(|e| e.title.as_deref() == Some("fix the thing") && e.tags.is_some())
        );
    }
    // The name of a run is read from the backend that holds it.
    l.lock().unwrap().clear();
    let name = api.run_name_of("W1").await.unwrap();
    assert_eq!(name, Some(("first".to_string(), Some("ai".to_string()))));
    assert!(
        calls(&l).iter().all(|c| c.contains("/v1/runs")),
        "{:?}",
        calls(&l)
    );
}

#[tokio::test]
async fn b584_a_call_with_no_target_and_no_default_goes_to_the_first_backend_that_is_up() {
    let (l1, l2) = (log(), log());
    let mut planes = Planes::new();
    planes.insert(
        "down".into(),
        Err(BuildFailure {
            text: "connection refused".into(),
            credentials: false,
        }),
    );
    planes.insert("work".into(), Ok(Fake::new(&l1, "work", ok_list(vec![]))));
    planes.insert("spare".into(), Ok(Fake::new(&l2, "spare", ok_list(vec![]))));
    let b = backends(
        vec![def("down", false), def("work", false), def("spare", false)],
        None,
        planes,
    )
    .await
    .unwrap();
    let view = b.backend_view().unwrap();
    assert!(
        matches!(&view.backends[0].health, Health::Down(why) if why.contains("refused")),
        "{:?}",
        view.backends[0]
    );
    b.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(calls(&l1), ["GET /v1/whoami []"]);
    assert!(calls(&l2).is_empty());
    // A call at a backend that could not be built says so and is not retried.
    let e = b
        .call_on(Some("down"), "GET", "/v1/whoami", &[], Vec::new())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, TransportError::Local(Local::BackendDown { .. })),
        "{e:?}"
    );
    assert!(!e.is_retryable());
    // `select` answers for the aliases it knows.
    assert_eq!(b.select(Some("work")).unwrap(), Some("work".to_string()));
    assert!(b.select(Some("nope")).is_err());
    assert!(b.select(None).is_err());
    assert!(b.def_of("work").is_some() && b.def_of("nope").is_none());
    // The fetch of a presigned address needs no identity: any backend that is up does it.
    assert_eq!(
        b.fetch("https://example.invalid/x", None, None)
            .await
            .unwrap()
            .status,
        0
    );
}

#[tokio::test]
async fn b584_the_stale_rows_of_a_failed_backend_are_cut_to_the_first_page_on_a_timer_read() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "work".into(),
        Ok(Fake::new(
            &l,
            "work",
            ok_list(vec![row("W1", "2026-11-04T11:00:00Z")]),
        )),
    );
    let up = Arc::new(Mutex::new(true));
    let up2 = Arc::clone(&up);
    let many: Vec<serde_json::Value> = (0..130)
        .map(|i| {
            row(
                &format!("L{:03}", 130 - i),
                &format!("2026-11-04T10:{:02}:{:02}Z", (130 - i) / 60, (130 - i) % 60),
            )
        })
        .collect();
    planes.insert(
        "lab".into(),
        Ok(Fake::new(&l, "lab", move |_m, _p| {
            if *up2.lock().unwrap() {
                json(200, Some("5.05"), serde_json::json!({"runs": many}))
            } else {
                Err(TransportError::Network("connection refused".into()))
            }
        })),
    );
    let b = Arc::new(
        backends(vec![def("work", false), def("lab", false)], None, planes)
            .await
            .unwrap(),
    );
    let api = Api::new(Arc::clone(&b));
    api.read_runs(Refresh::Manual).await.unwrap();
    *up.lock().unwrap() = false;
    let RunsRead::First { runs, has_more } = api.read_runs(Refresh::Timer).await.unwrap() else {
        panic!()
    };
    assert!(has_more, "the stale list is longer than a page");
    assert!(runs.len() <= 101, "{}", runs.len());
    assert!(b.backend_view().unwrap().info("lab").unwrap().stale);
}

#[tokio::test]
async fn b584_the_title_version_reads_the_backends_and_goes_back_when_removed() {
    let l = log();
    let mut planes = Planes::new();
    planes.insert(
        "a".into(),
        Ok(Fake::new(&l, "a", |_m, _p| {
            json(200, Some("5.05"), serde_json::json!({"runs": []}))
        })),
    );
    planes.insert(
        "b".into(),
        Ok(Fake::new(&l, "b", |_m, _p| {
            json(200, Some("5.04"), serde_json::json!({"runs": []}))
        })),
    );
    let b = backends(vec![def("a", false), def("b", false)], None, planes)
        .await
        .unwrap();
    b.install_title_version();
    assert_eq!(ham::version::backend(), None, "nothing answered yet");
    b.call_on(Some("a"), "GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(ham::version::backend().as_deref(), Some("5.05"));
    b.call_on(Some("b"), "GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(ham::version::backend().as_deref(), Some("mixed"));
    ham::version::set_backend_override(None);
}
