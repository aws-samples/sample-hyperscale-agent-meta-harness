//! `plan-delivery` and the flow a run declared: agentd records `quick`, `full` or `none` in `summary.flow`; `plan-delivery` writes the
//! summary whole into `evidence/direct.json` (where the client reads `summary.flow` out of the delivered zip) and puts the same word in the
//! evidence the packager renders. A summary with no flow gives neither. The flow changes no outcome, label or delivery.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::packager::PackageRequest;
use harness_protocol::direct::{DirectFlow, DirectOutcome, s3};
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

async fn plan(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(json!({
        "op": "plan-delivery", "runId": RUN, "staged": staged(),
        "session": {"turn": turn_output(true), "failure": {}}
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

fn delivered(flow: Option<DirectFlow>) -> Mem {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    let mut s = summary(DirectOutcome::ReportedDone, Some("Added pct(a, b)."), true);
    s.flow = flow;
    mem.put_summary(&s);
    mem
}

#[tokio::test]
async fn the_evidence_file_holds_the_flow_where_the_client_reads_it() {
    for flow in [DirectFlow::Quick, DirectFlow::Full, DirectFlow::Undeclared] {
        let mem = delivered(Some(flow));
        let p = plan(&mem).await;
        assert_eq!(p["deliver"], true);
        let written: Value =
            serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
        assert_eq!(
            ham_core::head::flow_in_evidence(&written),
            Some(flow),
            "the word must stand where the reader looks for it: {written}"
        );
        assert!(
            written.get("flow").is_none(),
            "the envelope has no flow of its own, so no reader may look for one there: {written}"
        );
        // The evidence the packager renders carries the same word, and the label is the summary's own.
        let pkg: PackageRequest = serde_json::from_value(p["package"].clone()).unwrap();
        assert_eq!(pkg.direct.flow, Some(flow));
        assert_eq!(pkg.direct.label, "Cline reported done");
        assert_eq!(p["outcome"], "reported_done");
        assert_eq!(p["label"], "Cline reported done");
    }
}

#[tokio::test]
async fn a_summary_without_a_flow_leaves_both_places_without_one() {
    let mem = delivered(None);
    let p = plan(&mem).await;
    let written: Value =
        serde_json::from_slice(&mem.blob(&s3::evidence_key(RUN)).unwrap()).unwrap();
    assert_eq!(
        ham_core::head::flow_in_evidence(&written),
        None,
        "{written}"
    );
    assert!(written["summary"].get("flow").is_none(), "{written}");
    let pkg: PackageRequest = serde_json::from_value(p["package"].clone()).unwrap();
    assert_eq!(pkg.direct.flow, None);
    assert!(p["package"]["direct"].get("flow").is_none(), "{p}");
}

#[tokio::test]
async fn a_session_that_wrote_no_summary_delivers_without_a_flow() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.put_bundle();
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true);
    let pkg: PackageRequest = serde_json::from_value(p["package"].clone()).unwrap();
    assert_eq!(pkg.direct.flow, None);
}
