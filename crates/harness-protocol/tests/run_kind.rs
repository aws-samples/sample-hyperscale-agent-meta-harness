//! `RunItem.kind` is gone with the probe runs `hamh-dispatch` wrote: a run item written before the
//! field parses and writes back unchanged, and a stored `kind` (a run from the old line's table)
//! is ignored on read.
#![allow(clippy::unwrap_used)]

use harness_protocol::store::RunItem;
use serde_json::json;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const OWNER: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser-Testacct";

fn run_json() -> serde_json::Value {
    json!({
        "runId": RUN, "ownerPrincipalArn": OWNER, "promptSha256": "p", "target": {"mode": "s3-only"},
        "status": "active", "phase": "green", "enginePolicyVersion": 0, "createdAt": "2026-09-29T10:00:00Z",
        "version": 1
    })
}

#[test]
fn a_run_written_before_the_kind_field_parses_and_writes_back_without_one() {
    let r: RunItem = serde_json::from_value(run_json()).unwrap();
    assert_eq!(serde_json::to_value(&r).unwrap(), run_json());
}
