//! After the independent review: `ham doctor` with several backends checks all of them at once and gives each its own deadline, so one
//! backend that does not answer costs one deadline and no more, and the lines come out in config order.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::backends::{Backends, BoxBuild, BuildFailure};
use ham::doctor::{DoctorTarget, checks_many};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::backends::BackendDef;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Stand {
    delay: Duration,
    fail: bool,
}

impl ControlPlane for Stand {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail {
            return Err(TransportError::Network("connection refused".into()));
        }
        let mut headers = BTreeMap::new();
        headers.insert("x-hamh-backend-version".to_string(), "5.05".to_string());
        let (status, body) = if path == "/v1/whoami" {
            (
                200,
                serde_json::json!({"principalArn": "arn:aws:sts::1:assumed-role/A/b", "account": "111111111111"}),
            )
        } else {
            (
                404,
                serde_json::json!({"code": "not-found", "message": "no"}),
            )
        };
        Ok(Response {
            status,
            headers,
            body: body.to_string().into_bytes(),
        })
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

fn def(alias: &str) -> BackendDef {
    BackendDef {
        alias: alias.into(),
        api_endpoint: Some(format!("https://{alias}.example/direct")),
        region: None,
        aws_profile: Some(alias.into()),
        customer_content: false,
    }
}

fn target(alias: &str) -> DoctorTarget {
    DoctorTarget {
        alias: alias.into(),
        endpoint: format!("https://{alias}.example/direct"),
        profile: alias.into(),
    }
}

#[tokio::test]
async fn b584_doctor_checks_every_backend_at_once_each_with_its_own_deadline() {
    let planes: Arc<Mutex<BTreeMap<String, Stand>>> = Arc::new(Mutex::new(BTreeMap::from([
        (
            "work".to_string(),
            Stand {
                delay: Duration::ZERO,
                fail: false,
            },
        ),
        (
            "blackhole".to_string(),
            Stand {
                delay: Duration::from_secs(60),
                fail: false,
            },
        ),
        (
            "down".to_string(),
            Stand {
                delay: Duration::ZERO,
                fail: true,
            },
        ),
        (
            "blackhole2".to_string(),
            Stand {
                delay: Duration::from_secs(60),
                fail: false,
            },
        ),
    ])));
    let factory = move |d: BackendDef| -> BoxBuild<Stand> {
        let r = planes.lock().unwrap().remove(&d.alias).ok_or(BuildFailure {
            text: "none".into(),
            credentials: false,
        });
        Box::pin(async move { r })
    };
    let aliases = ["work", "blackhole", "down", "blackhole2"];
    let b = Arc::new(
        Backends::connect(aliases.iter().map(|a| def(a)).collect(), None, factory)
            .await
            .unwrap(),
    );
    let api = Api::new(Arc::clone(&b)).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let targets: Vec<DoctorTarget> = aliases.iter().map(|a| target(a)).collect();
    let started = Instant::now();
    let d = checks_many(&api, &targets, None, 0, Duration::from_millis(500)).await;
    let took = started.elapsed();
    // Two blackholes cost one deadline together, not a deadline each and not the 60 s of their calls.
    assert!(took < Duration::from_secs(5), "{took:?}");

    let lines: Vec<String> = d
        .checks
        .iter()
        .map(|c| format!("{} {} {}", c.level().to_uppercase(), c.name, c.detail))
        .collect();
    // Every line is led by the alias of its backend, and the blocks are in config order.
    let first = |alias: &str| {
        lines
            .iter()
            .position(|l| l.contains(&format!("{alias}: ")))
            .unwrap_or_else(|| panic!("{alias}: {lines:#?}"))
    };
    let at: Vec<usize> = aliases.iter().map(|a| first(a)).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{at:?}\n{lines:#?}");
    // The backend that answers has its identity line.
    assert!(
        lines.iter().any(|l| l.starts_with("PASS identity")
            && l.contains("work: ")
            && l.contains("111111111111")),
        "{lines:#?}"
    );
    // A backend that did not answer in time says so, as failed identity and api lines that name the deadline.
    for blackhole in ["blackhole", "blackhole2"] {
        assert!(
            lines.iter().any(|l| l.starts_with("FAIL identity")
                && l.contains(&format!("{blackhole}: "))
                && l.contains("no answer in")),
            "{blackhole}\n{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("FAIL api")
                && l.contains(&format!("{blackhole}: "))
                && l.contains("no answer in")),
            "{blackhole}\n{lines:#?}"
        );
    }
    // A backend that failed fast keeps the failure of its own call.
    assert!(
        lines.iter().any(|l| l.starts_with("FAIL identity")
            && l.contains("down: ")
            && l.contains("connection refused")),
        "{lines:#?}"
    );
    // Each backend has a versions line, and the slow ones are marked slow for the banners.
    for alias in aliases {
        assert!(
            lines
                .iter()
                .any(|l| l.contains(" versions ") && l.contains(&format!("{alias}: "))),
            "{alias}\n{lines:#?}"
        );
    }
    let banners = b.backend_view().unwrap().banners();
    assert!(
        banners
            .iter()
            .any(|x| x.starts_with("blackhole: did not answer in time")),
        "{banners:?}"
    );
}
