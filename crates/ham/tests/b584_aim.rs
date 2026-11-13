//! Steps 5 to 7 in the `ham` crate: how a command is aimed (`--backend`, `default_backend`, the source run of a continue) and the
//! customer-content refusal, against stand-in planes.
#![allow(clippy::unwrap_used)]

use ham::api::Api;
use ham::backends::{Aim, Backends, BoxBuild, BuildFailure, aim};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::backends::BackendDef;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

struct Quiet(Arc<Mutex<Vec<String>>>, &'static str);

impl ControlPlane for Quiet {
    async fn call(
        &self,
        m: &str,
        p: &str,
        _h: &[(String, String)],
        _b: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.0.lock().unwrap().push(format!("{} {m} {p}", self.1));
        let has = p.contains("HAS1");
        let status = if p.starts_with("/v1/runs/") && !has {
            404
        } else {
            200
        };
        let mut headers = BTreeMap::new();
        headers.insert("x-hamh-backend-version".to_string(), "5.05".to_string());
        Ok(Response {
            status,
            headers,
            body: b"{}".to_vec(),
        })
    }
    async fn fetch(
        &self,
        _u: &str,
        _r: Option<(u64, u64)>,
        _i: Option<&str>,
    ) -> Result<Response, TransportError> {
        Ok(Response::default())
    }
}

fn def(alias: &str, cc: bool) -> BackendDef {
    BackendDef {
        alias: alias.into(),
        api_endpoint: Some(format!("https://{alias}.example/direct")),
        region: None,
        aws_profile: None,
        customer_content: cc,
    }
}

async fn api(
    defs: Vec<BackendDef>,
    default: Option<&str>,
    log: &Arc<Mutex<Vec<String>>>,
) -> Api<Backends<Quiet>> {
    let log = Arc::clone(log);
    let factory = move |d: BackendDef| -> BoxBuild<Quiet> {
        let log = Arc::clone(&log);
        let name: &'static str = Box::leak(d.alias.clone().into_boxed_str());
        Box::pin(async move { Ok::<_, BuildFailure>(Quiet(log, name)) })
    };
    Api::new(Arc::new(
        Backends::connect(defs, default.map(str::to_string), factory)
            .await
            .unwrap(),
    ))
}

#[tokio::test]
async fn b584_a_client_with_one_backend_is_never_aimed() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let a = api(vec![def("default", false)], None, &log).await;
    for named in [None, Some("default")] {
        let aimed = aim(&a, named, false, Aim::Start).await.unwrap();
        assert_eq!(aimed.target(), None, "{named:?}");
    }
    let e = aim(&a, Some("work"), false, Aim::Start)
        .await
        .err()
        .unwrap();
    assert!(e.contains("work") && e.contains("default"), "{e}");
}

#[tokio::test]
async fn b584_with_two_backends_a_start_needs_a_choice_and_default_backend_is_one() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let a = api(vec![def("work", false), def("spare", false)], None, &log).await;
    let e = aim(&a, None, false, Aim::Start).await.err().unwrap();
    assert!(
        e.contains("--backend") && e.contains("default_backend"),
        "{e}"
    );
    assert_eq!(
        aim(&a, Some("spare"), false, Aim::Start)
            .await
            .unwrap()
            .target(),
        Some("spare")
    );
    assert_eq!(
        aim(&a, None, false, Aim::Other).await.unwrap().target(),
        None,
        "a call about a run finds its backend"
    );
    let with_default = api(
        vec![def("work", false), def("spare", false)],
        Some("work"),
        &log,
    )
    .await;
    assert_eq!(
        aim(&with_default, None, false, Aim::Start)
            .await
            .unwrap()
            .target(),
        Some("work")
    );
    assert_eq!(
        aim(&with_default, Some("spare"), false, Aim::Start)
            .await
            .unwrap()
            .target(),
        Some("spare"),
        "the flag beats the default"
    );
}

#[tokio::test]
async fn b584_a_continue_starts_where_the_source_run_is() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let a = api(
        vec![def("work", false), def("spare", false)],
        Some("work"),
        &log,
    )
    .await;
    // only `spare` answers 200 for the run; `work` answers 404 for everything under /v1/runs/ in this stand-in unless the id is HAS1 on both,
    // so name a backend explicitly to make the holder unique
    let aimed = aim(&a, Some("spare"), false, Aim::Continue("HAS1"))
        .await
        .unwrap();
    assert_eq!(aimed.target(), Some("spare"));
}

#[tokio::test]
async fn b584_a_start_marked_as_customer_content_needs_a_backend_declared_safe() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let a = api(vec![def("work", true), def("spare", false)], None, &log).await;
    assert_eq!(
        aim(&a, Some("work"), true, Aim::Start)
            .await
            .unwrap()
            .target(),
        Some("work")
    );
    let e = aim(&a, Some("spare"), true, Aim::Start)
        .await
        .err()
        .unwrap();
    assert!(e.contains("spare") && e.contains("customerContent"), "{e}");
    assert!(log.lock().unwrap().is_empty(), "refused before any call");
    let e = aim(&a, Some("spare"), true, Aim::Other)
        .await
        .err()
        .unwrap();
    assert!(e.contains("--customer-content"), "{e}");
    // a lone backend is the default one; undeclared, it refuses too
    let one = api(vec![def("default", false)], None, &log).await;
    assert!(aim(&one, None, true, Aim::Start).await.is_err());
    let one = api(vec![def("default", true)], None, &log).await;
    assert!(aim(&one, None, true, Aim::Start).await.is_ok());
}
