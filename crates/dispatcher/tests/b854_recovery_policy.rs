//! The inline session policy of the credentials that read a stopped session's storage back.
//!
//! The worker that reads the storage writes one object, the read-back bundle of that session, and reads at most one, the saved copy it
//! compares with. The credentials are minted from `harness-worker-session` like a working session's, and the effective rights are the
//! intersection of the role's policy and this one, so this policy names every action the read needs and nothing else: no other object, no
//! listing, no internal API, no model.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use dispatcher::policy::{MAX_SESSION_POLICY_CHARS, PolicyError, recovery_policy};
use serde_json::Value;

const BUCKET: &str = "hamh-direct-harness-111122223333";
const RUN: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

fn key() -> String {
    format!("runs/{RUN}/agents/recovery/turns/3/recovery.bundle")
}

fn baseline() -> String {
    format!("runs/{RUN}/agents/direct/turns/3/snapshot.bundle")
}

fn statements(policy: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(policy).unwrap();
    assert_eq!(v["Version"], "2012-10-17");
    v["Statement"].as_array().unwrap().clone()
}

fn actions(s: &Value) -> Vec<String> {
    match &s["Action"] {
        Value::String(a) => vec![a.clone()],
        Value::Array(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
        other => panic!("Action {other}"),
    }
}

#[test]
fn b854_the_recovery_policy_allows_one_put_and_nothing_else() {
    // With a baseline to compare with: one PUT, one GET, the two data-key actions.
    let policy = recovery_policy(BUCKET, &key(), Some(&baseline())).unwrap();
    assert!(
        policy.chars().count() <= MAX_SESSION_POLICY_CHARS,
        "{}",
        policy.len()
    );
    let st = statements(&policy);
    assert!(st.iter().all(|s| s["Effect"] == "Allow"));
    let mut all: Vec<String> = st.iter().flat_map(actions).collect();
    all.sort();
    assert_eq!(
        all,
        [
            "kms:Decrypt",
            "kms:GenerateDataKey",
            "s3:GetObject",
            "s3:PutObject"
        ]
    );
    let put: Vec<&Value> = st
        .iter()
        .filter(|s| actions(s).contains(&"s3:PutObject".to_string()))
        .collect();
    assert_eq!(put.len(), 1);
    assert_eq!(actions(put[0]), ["s3:PutObject"]);
    assert_eq!(
        put[0]["Resource"],
        format!("arn:aws:s3:::{BUCKET}/{}", key())
    );
    let get: Vec<&Value> = st
        .iter()
        .filter(|s| actions(s).contains(&"s3:GetObject".to_string()))
        .collect();
    assert_eq!(get.len(), 1);
    assert_eq!(actions(get[0]), ["s3:GetObject"]);
    assert_eq!(
        get[0]["Resource"],
        format!("arn:aws:s3:::{BUCKET}/{}", baseline())
    );
    // Nothing is a wildcard over objects, and no statement reaches the internal API, a model, a listing or another service.
    for s in &st {
        for a in actions(s) {
            assert!(!a.contains('*'), "{a}");
            assert!(
                !a.starts_with("execute-api:")
                    && !a.starts_with("bedrock")
                    && !a.starts_with("sts:")
                    && !a.starts_with("dynamodb:"),
                "{a}"
            );
            assert_ne!(a, "s3:ListBucket");
            assert_ne!(a, "s3:AbortMultipartUpload");
        }
        if actions(s).iter().any(|a| a.starts_with("s3:")) {
            let r = s["Resource"].as_str().unwrap();
            assert!(!r.contains('*') && !r.contains('?'), "{r}");
        }
    }

    // Without a baseline object (the baseline is the commit the session started on): one PUT, no read at all.
    let policy = recovery_policy(BUCKET, &key(), None).unwrap();
    let st = statements(&policy);
    let mut all: Vec<String> = st.iter().flat_map(actions).collect();
    all.sort();
    assert_eq!(all, ["kms:Decrypt", "kms:GenerateDataKey", "s3:PutObject"]);
    assert!(!policy.contains("GetObject"));
}

#[test]
fn b854_the_recovery_policy_refuses_a_name_that_could_widen_it() {
    for (bucket, key, baseline) in [
        (
            "hamh-direct-harness-111122223333",
            "runs/*/agents/recovery/turns/3/recovery.bundle",
            None,
        ),
        (
            "hamh-direct-harness-111122223333",
            "runs/x/agents/recovery/turns/?/recovery.bundle",
            None,
        ),
        ("hamh-direct-harness-111122223333", "", None),
        (
            "Bad_Bucket",
            "runs/x/agents/recovery/turns/3/recovery.bundle",
            None,
        ),
        ("*", "runs/x/agents/recovery/turns/3/recovery.bundle", None),
        (
            "hamh-direct-harness-111122223333",
            "runs/x/agents/recovery/turns/3/recovery.bundle",
            Some("runs/x/agents/direct/turns/*/snapshot.bundle"),
        ),
        (
            "hamh-direct-harness-111122223333",
            "runs/x/agents/recovery/turns/3/recovery.bundle",
            Some(""),
        ),
    ] {
        assert!(
            matches!(
                recovery_policy(bucket, key, baseline),
                Err(PolicyError::BadObject(_))
            ),
            "{bucket} {key} {baseline:?}"
        );
    }
}
