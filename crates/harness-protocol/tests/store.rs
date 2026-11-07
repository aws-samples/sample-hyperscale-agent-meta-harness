//! DynamoDB key builders, item keys and the S3 layout.

use harness_protocol::envelope::Authority;
use harness_protocol::store::{EVENT_SHARDS, IdempotencyItem, Keyed, StoreItem, event_shard, keys};

#[test]
fn turn_sort_keys_sort_numerically() {
    let mut sks: Vec<String> = [10u32, 2, 1, 100]
        .iter()
        .map(|n| keys::turn_sk("a", *n))
        .collect();
    sks.sort();
    assert_eq!(
        sks,
        [
            "TURN#a#000001",
            "TURN#a#000002",
            "TURN#a#000010",
            "TURN#a#000100"
        ]
    );
}

#[test]
fn harness_events_use_shard_zero_and_agent_events_hash_into_32() {
    assert_eq!(EVENT_SHARDS, 32);
    assert_eq!(event_shard(Authority::Harness, Some("a1")), 0);
    assert_eq!(event_shard(Authority::Harness, None), 0);
    let s = event_shard(Authority::Agent, Some("agent-2"));
    assert!(s < 32);
    assert_eq!(
        s,
        event_shard(Authority::Agent, Some("agent-2")),
        "deterministic"
    );
    // FNV-1a 64 mod 32, pinned so every writer and reader agree.
    assert_eq!(s, 25);
    assert_eq!(event_shard(Authority::Agent, Some("agent-3")), 6);
    assert_eq!(
        event_shard(Authority::Agent, Some("01J8ZQ4Y6X3M2K7N9P5R8T1V0W")),
        22
    );
}

#[test]
fn idempotency_claims_are_keyed_by_caller_and_key_and_stay_out_of_gsi1() {
    let owner = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
    assert_eq!(
        keys::idempotency_pk(owner, "k-1"),
        format!("IDEM#{owner}#k-1")
    );
    let claim = IdempotencyItem {
        principal_arn: owner.into(),
        idempotency_key: "k-1".into(),
        run_id: "01K5ZQ4T8M3N6P7R9S0V1W2X3Y".into(),
        request_sha256: "ab".repeat(32),
        claimed_at: "2026-09-25T01:00:00Z".into(),
    };
    let k = claim.key();
    assert_eq!(
        (k.pk.as_str(), k.sk.as_str()),
        (format!("IDEM#{owner}#k-1").as_str(), "META")
    );
    let item = StoreItem::Idempotency(claim);
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["entity"], "idempotency");
    // GSI1 (ownerPrincipalArn + createdAt) is sparse: only Run items carry those attributes.
    assert!(v.get("ownerPrincipalArn").is_none(), "{v}");
    assert!(v.get("createdAt").is_none(), "{v}");
    let back: StoreItem = serde_json::from_value(v).unwrap();
    assert_eq!(back, item);
}

#[test]
fn a_turn_item_records_when_it_was_invoked() {
    // The dispatcher sets invokedAt after InvokeAgentRuntime accepts the turn, so a retried
    // Step Functions state never invokes it twice.
    let v = serde_json::json!({
        "runId": "r1", "agentId": "a1", "n": 1, "taskTokenCiphertext": "ct", "startedAt": "2026-09-25T20:00:00Z",
        "rulesDigest": "", "version": 1
    });
    let t: harness_protocol::store::TurnItem = serde_json::from_value(v).unwrap();
    assert_eq!(t.invoked_at, None);
    let mut t2 = t.clone();
    t2.invoked_at = Some("2026-09-25T20:00:01Z".into());
    assert_eq!(
        serde_json::to_value(&t2).unwrap()["invokedAt"],
        "2026-09-25T20:00:01Z"
    );
    assert!(serde_json::to_value(&t).unwrap().get("invokedAt").is_none());
}

#[test]
fn a_run_item_names_the_deployment_it_ran_on_and_older_items_still_read() {
    use harness_protocol::store::RunItem;
    // deployment-keyed evidence resolves through the run's META item.
    let v = serde_json::json!({
        "runId": "r1", "ownerPrincipalArn": "arn:aws:sts::800000000022:assumed-role/Admin/x",
        "promptSha256": "p", "target": {"mode": "repo", "repo": "cc:hamh-e2e-python-calc"},
        "status": "active", "phase": "planning", "enginePolicyVersion": 1,
        "createdAt": "2026-09-26T00:00:00Z", "version": 1,
        "deploymentDigest": "d".repeat(64), "sourceTree": "t".repeat(40)
    });
    let r: RunItem = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(
        r.deployment_digest.as_deref(),
        Some("d".repeat(64).as_str())
    );
    assert_eq!(r.source_tree.as_deref(), Some("t".repeat(40).as_str()));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    let mut old = v;
    old.as_object_mut().unwrap().remove("deploymentDigest");
    old.as_object_mut().unwrap().remove("sourceTree");
    let r: RunItem = serde_json::from_value(old.clone()).unwrap();
    assert_eq!((r.deployment_digest, r.source_tree), (None, None));
}
