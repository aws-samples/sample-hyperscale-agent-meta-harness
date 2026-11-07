//! `plan-delivery` and the request's `thirdParty` member: the user's per-run setting that lets a delivered
//! copy of a stored third-party file pass reaches the bundle validator's payload. The default (`block`, or no member) stays off the wire, so the
//! payload of a run that keeps the default is byte for byte the payload it was before the member.
#![allow(clippy::unwrap_used)]

mod support;

use delivery::validator::{ThirdPartyMode, ValidateRequest};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{DirectOutcome, s3};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

fn seeded(
    target: RunTarget,
    third_party: Option<harness_protocol::research::ThirdPartyMode>,
) -> Mem {
    let mem = Mem::default();
    mem.seed(target, "Add percent_change(a, b) with tests", MODEL);
    // Replace the stored request's member (and its recorded hash) the way control-api would have stored it.
    let mut req: StartDirectRunRequest =
        serde_json::from_slice(&mem.0.blobs.lock().unwrap()[&s3::request_key(RUN)]).unwrap();
    req.third_party = third_party;
    let body = serde_json::to_vec(&req).unwrap();
    mem.0
        .blobs
        .lock()
        .unwrap()
        .insert(s3::request_key(RUN), body.clone());
    for e in mem.0.events.lock().unwrap().values_mut() {
        if let harness_protocol::envelope::Payload::Inline { value } = &mut e.payload {
            value["requestSha256"] = json!(hex(&body));
        }
    }
    mem.put_bundle();
    mem.put_summary(&summary(
        DirectOutcome::ReportedDone,
        Some("Added pct(a, b)."),
        true,
    ));
    mem
}

async fn plan(mem: &Mem) -> Value {
    let op: Op = serde_json::from_value(json!({
        "op": "plan-delivery", "runId": RUN, "staged": staged(),
        "session": {"turn": turn_output(true), "failure": {}}
    }))
    .unwrap();
    handle(&mem.deps(), &op).await.unwrap()
}

#[tokio::test]
async fn allow_reaches_the_validators_payload() {
    let mem = seeded(
        repo_target(),
        Some(harness_protocol::research::ThirdPartyMode::Allow),
    );
    let p = plan(&mem).await;
    assert_eq!(p["deliver"], true, "{p}");
    assert_eq!(p["validate"]["thirdParty"], "allow", "{p}");
    let v: ValidateRequest = serde_json::from_value(p["validate"].clone()).unwrap();
    assert_eq!(v.third_party, ThirdPartyMode::Allow);
    // Nothing else of the payload moved.
    assert_eq!(p["validate"]["engineConfig"], "executable-only");
}

#[tokio::test]
async fn no_member_and_block_leave_the_payload_as_it_was() {
    for member in [
        None,
        Some(harness_protocol::research::ThirdPartyMode::Block),
    ] {
        let mem = seeded(repo_target(), member);
        let p = plan(&mem).await;
        assert_eq!(p["deliver"], true, "{p}");
        assert!(p["validate"].get("thirdParty").is_none(), "{member:?}: {p}");
        let v: ValidateRequest = serde_json::from_value(p["validate"].clone()).unwrap();
        assert_eq!(v.third_party, ThirdPartyMode::Block);
    }
}

#[tokio::test]
async fn allow_works_for_an_s3_only_run_too() {
    let mem = seeded(
        RunTarget::S3Only,
        Some(harness_protocol::research::ThirdPartyMode::Allow),
    );
    let p = plan(&mem).await;
    assert_eq!(p["validate"]["thirdParty"], "allow", "{p}");
}
