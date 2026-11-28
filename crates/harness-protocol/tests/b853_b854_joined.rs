//! The wire changes of one protocol version in one tree: reading a stopped session's storage back, a cancel that sticks with the
//! session facts of the run list, and the service's word that it reads storage back. Each was built on its own. These
//! tests hold what one run's items, requests and answers carry when all are in force, and what a client and a service of different
//! releases read from each other:
//!
//! * one start request that carries every new member is the request control-api stores, run-session reads and the worker never sees:
//!   the invocation of a session takes none of them;
//! * a stage of the release before refuses a start request with a member it does not know and names the member, which is how `ham`
//!   names the key;
//! * a client of the release before reads every answer and event of this service: the new members are optional, left out when absent,
//!   and the types that carry them take members they do not know.
#![allow(clippy::unwrap_used)]

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::internal::HeartbeatRequest;
use harness_protocol::api::v1::{RunControlResponse, RunSummary, StartDirectRunRequest};
use harness_protocol::direct::{DirectInvocationRequest, validate_direct_invocation};
use harness_protocol::export::all_schemas;
use harness_protocol::orchestration::SessionStartedPayload;
use harness_protocol::store::{AgentItem, RunItem};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const BUCKET: &str = "hamh-direct-harness-111122223333";
const AT: &str = "2026-11-16T07:05:22Z";
const SHA: &str = "3333333333333333333333333333333333333333";

fn docs() -> BTreeMap<&'static str, Value> {
    all_schemas().into_iter().collect()
}

/// Whether the contract file `doc` holds the member `member` of the type `name`, wherever the file keeps its types.
fn has(doc: &Value, name: &str, member: &str) -> bool {
    ["/$defs", "/definitions", "/components/schemas"]
        .iter()
        .any(|at| {
            doc.pointer(&format!("{at}/{name}/properties/{member}"))
                .is_some()
        })
}

/// One version carries the members of every change, each where its reader finds it.
#[test]
fn b853_b854_one_protocol_version_carries_the_read_the_cancel_and_the_session_facts() {
    assert_eq!(PROTOCOL_VERSION, "1.65.0");
    let docs = docs();
    for file in ["openapi-v1.json", "openapi-internal.json"] {
        assert_eq!(docs[file]["info"]["version"], "1.65.0", "{file}");
    }
    for (file, name, member) in [
        // reading a stopped session's storage back
        (
            "openapi-v1.json",
            "StartDirectRunRequest",
            "recoverAfterStop",
        ),
        ("items.schema.json", "RunItem", "recoveredCopy"),
        ("items.schema.json", "AgentItem", "fencedGeneration"),
        ("items.schema.json", "AgentItem", "recoverAfterStop"),
        ("openapi-internal.json", "HeartbeatRequest", "ending"),
        (
            "orchestration.schema.json",
            "SessionStartedPayload",
            "storageReadBack",
        ),
        // a cancel that sticks, and the session facts of the run list
        ("items.schema.json", "RunItem", "cancelRequestedAt"),
        ("openapi-v1.json", "RunControlResponse", "cancel"),
        ("openapi-v1.json", "RunSummary", "cancelRequestedAt"),
        ("openapi-v1.json", "RunSummary", "sessionSilentSince"),
    ] {
        assert!(
            has(&docs[file], name, member),
            "{file} names {name}.{member}"
        );
    }
    assert!(
        docs["runtime.schema.json"]
            .to_string()
            .contains("RecoveryInvocationRequest"),
        "the runtime schema holds the body of the read"
    );
    for file in ["openapi-v1.json", "openapi-internal.json"] {
        assert!(
            docs[file].to_string().contains("\"answeredTokens\""),
            "{file} names the tokens of the prompts that answered"
        );
    }
}

/// One Run item and one Agent item hold the facts of both changes: neither writer's member displaces the other's.
#[test]
fn b853_b854_one_run_item_holds_the_cancel_and_the_copy_read_back() {
    let run = json!({
        "runId": RUN, "ownerPrincipalArn": "arn:aws:sts::123456789012:assumed-role/Admin/me", "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": "active", "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-11-16T07:00:00Z", "version": 3,
        "cancelRequestedAt": AT,
        "recoveredCopy": {"session": 1, "key": format!("runs/{RUN}/agents/recovery/turns/1/recovery.bundle"), "headSha": SHA,
                          "at": "2026-11-16T07:06:40Z", "bundleBytes": 60}
    });
    let item: RunItem = serde_json::from_value(run.clone()).unwrap();
    assert_eq!(item.cancel_requested_at.as_deref(), Some(AT));
    assert_eq!(item.recovered_copy.as_ref().map(|c| c.session), Some(1));
    assert_eq!(serde_json::to_value(&item).unwrap(), run);
    // Each member alone, and neither: the item of the release before.
    for drop in [
        vec!["cancelRequestedAt"],
        vec!["recoveredCopy"],
        vec!["cancelRequestedAt", "recoveredCopy"],
    ] {
        let mut v = run.clone();
        for member in &drop {
            v.as_object_mut().unwrap().remove(*member);
        }
        let item: RunItem = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&item).unwrap(), v, "{drop:?}");
    }
    let agent = json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": "us.anthropic.claude-opus-5-5",
        "pool": "direct", "status": "running", "version": 1, "sessionGeneration": 1,
        "fencedGeneration": 1, "recoverAfterStop": false
    });
    let item: AgentItem = serde_json::from_value(agent.clone()).unwrap();
    assert_eq!(item.fenced_generation, item.session_generation);
    assert_eq!(item.recover_after_stop, Some(false));
    assert_eq!(serde_json::to_value(&item).unwrap(), agent);
}

fn invocation() -> Value {
    json!({
        "kind": "direct", "runId": RUN, "agentId": "direct", "turn": 1,
        "runtimeSessionId": format!("{RUN}-direct-g00000001"),
        "credentials": {"Version": 1, "AccessKeyId": "ASIAEXAMPLEEXAMPLE", "SecretAccessKey": "s", "SessionToken": "t",
                        "Expiration": "2026-11-03T18:10:00Z"},
        "nonce": "n", "internalApiUrl": "https://abc.execute-api.us-east-1.amazonaws.com/direct", "region": "us-east-1",
        "engine": "cline",
        "model": {"id": "us.anthropic.claude-sonnet-5-5", "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 64000,
                  "protocol": "converse"},
        "request": {"uri": format!("s3://{BUCKET}/runs/{RUN}/input/request.json"), "sha256": "ab".repeat(32)},
        "repo": {"kind": "s3Bundle", "uri": format!("s3://{BUCKET}/runs/{RUN}/input/repo.bundle")},
        "timeoutS": 600
    })
}

/// One start request with every member a run can switch in this version and the one before. It is the request control-api takes and
/// stores and run-session reads back. The worker's invocation carries none of these members and refuses each: a session reads the
/// stored request, so a worker image of the release before starts a run of this service.
#[test]
fn b853_b854_one_start_request_carries_every_new_member_and_the_invocation_of_a_session_none() {
    let body = json!({
        "prompt": "Add a workflow file.", "target": {"mode": "s3-only"}, "model": "us.anthropic.claude-sonnet-5-5", "rules": [],
        "recoverAfterStop": false, "deliveryNote": false, "snapshotEveryS": 15
    });
    let request: StartDirectRunRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(request.recover_after_stop, Some(false));
    // What is stored is what is read back.
    let stored = serde_json::to_vec(&request).unwrap();
    let read: StartDirectRunRequest = serde_json::from_slice(&stored).unwrap();
    assert_eq!(serde_json::to_value(&read).unwrap(), body);
    // The invocation of the run's sessions: valid without the members, and it takes none of them.
    let plain: DirectInvocationRequest = serde_json::from_value(invocation()).unwrap();
    assert_eq!(validate_direct_invocation(&plain), Ok(()));
    for member in [
        "recoverAfterStop",
        "deliveryNote",
        "cancelRequestedAt",
        "storageReadBack",
    ] {
        let mut v = invocation();
        v[member] = json!(false);
        let e = serde_json::from_value::<DirectInvocationRequest>(v)
            .expect_err("the invocation takes no member it does not know");
        assert!(e.to_string().contains(member), "{member}: {e}");
    }
}

/// A stage of the release before takes no start request with a member it does not know: its parser is as strict as this one, and its
/// refusal names the member. `ham` reads that name and says which key to unset.
#[test]
fn b853_b854_a_start_request_with_an_unknown_member_is_refused_by_name() {
    let body = json!({
        "prompt": "Add a workflow file.", "target": {"mode": "s3-only"}, "model": "us.anthropic.claude-sonnet-5-5",
        "aMemberOfALaterRelease": false
    });
    let e = serde_json::from_value::<StartDirectRunRequest>(body)
        .expect_err("a member the parser does not know is refused");
    assert!(
        e.to_string()
            .contains("unknown field `aMemberOfALaterRelease`"),
        "{e}"
    );
    let docs = docs();
    assert_eq!(
        docs["openapi-v1.json"]["components"]["schemas"]["StartDirectRunRequest"]["additionalProperties"],
        json!(false)
    );
}

/// A client of the release before reads this service: what it is answered when nothing new is to say is byte for byte what the service
/// of its own release wrote, and the types that gained a member take members they do not know, as they did in that release.
#[test]
fn b853_b854_a_client_of_the_release_before_reads_the_answers_and_events_of_this_service() {
    // The answer of a cancel that clears (`--discard`, or a run without a session record): no `cancel` member.
    let control = json!({"runId": RUN, "status": "cleared"});
    let parsed: RunControlResponse = serde_json::from_value(control.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), control);
    // A row of the run list of a run that is not cancelled and whose session is not silent.
    let row = json!({
        "runId": RUN, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-11-16T07:00:00Z", "ownerPrincipalArn": "arn:aws:sts::123456789012:assumed-role/Admin/me"
    });
    let parsed: RunSummary = serde_json::from_value(row.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), row);
    // The service's first event of a run without the read.
    let started =
        json!({"agentId": "direct", "engine": "cline", "modelId": "us.anthropic.claude-opus-5-5"});
    let parsed: SessionStartedPayload = serde_json::from_value(started.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), started);
    // A heartbeat of a worker of the release before has no `ending`, and this service reads it.
    let beat = json!({"runId": RUN, "agentId": "direct", "turn": 3});
    let parsed: HeartbeatRequest = serde_json::from_value(beat).unwrap();
    assert_eq!(parsed.ending, None);
    // The types a client reads take a member they do not know.
    let mut later = control.clone();
    later["cancel"] = json!({"requestedAt": AT, "sessionLive": false, "aLaterMember": 1});
    later["aLaterMember"] = json!(1);
    assert!(serde_json::from_value::<RunControlResponse>(later).is_ok());
    let mut later = row.clone();
    later["aLaterMember"] = json!(1);
    assert!(serde_json::from_value::<RunSummary>(later).is_ok());
    let mut later = started.clone();
    later["aLaterMember"] = json!(1);
    assert!(serde_json::from_value::<SessionStartedPayload>(later).is_ok());
}
