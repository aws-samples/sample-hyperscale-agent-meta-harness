//! The Lambda glue shared by the three binaries: the error kind becomes the Lambda `errorType` (the
//! state machine catches `Refused` and `Invalid` and retries `Unavailable`), a malformed input is
//! `Invalid`, the configuration comes from the function environment, and the one log line per
//! invocation names the operation, run and outcome and never the request body.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use common::*;
use delivery::error::DeliveryError;
use delivery::gateway::{Gateway, GatewayRequest};
use delivery::git::Git;
use delivery::lambda::{
    Config, diagnostic, gateway, gateway_event, invocation_line, load_aws, now_unix, packager,
    packager_event, parse, validator, validator_event,
};
use delivery::packager::Packager;
use delivery::validator::{Gitleaks, Validator};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
fn each_error_kind_is_its_own_error_type_and_the_refusal_message_is_the_reason() {
    let d = diagnostic(&DeliveryError::Refused(
        "refused: cc:x is outside the hamh-e2e- prefix".into(),
    ));
    assert_eq!(d.error_type, "Refused");
    assert_eq!(
        d.error_message,
        "refused: cc:x is outside the hamh-e2e- prefix"
    );
    assert_eq!(
        diagnostic(&DeliveryError::invalid("bad")).error_type,
        "Invalid"
    );
    assert_eq!(
        diagnostic(&DeliveryError::unavailable("s3 down")).error_type,
        "Unavailable"
    );
    // S3's refusal of a key or an action: the run machine catches it by this name and does not retry it.
    let d = diagnostic(&DeliveryError::AccessDenied("s3 get k: no grant".into()));
    assert_eq!(d.error_type, "AccessDenied");
    assert_eq!(d.error_message, "access denied: s3 get k: no grant");
}

#[test]
fn a_malformed_input_is_invalid_with_the_parse_reason() {
    let err = parse::<GatewayRequest>(json!({"operation": "merge"})).unwrap_err();
    assert!(
        matches!(&err, DeliveryError::Invalid(m) if m.contains("merge")),
        "{err}"
    );
    let err =
        parse::<GatewayRequest>(json!({"operation": "stage", "runId": "r", "target": "ftp:x"}))
            .unwrap_err();
    assert!(matches!(err, DeliveryError::Invalid(_)));
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn the_configuration_comes_from_the_environment_with_defaults() {
    let e = env(&[
        ("BUCKET_NAME", "hamh-live-harness-800000000022"),
        ("TABLE_NAME", "hamh-live-harness"),
        ("HAMH_STAGE", "live"),
        ("HAMH_OWNER", "team-a"),
    ]);
    let c = Config::from_lookup(|k| e.get(k).cloned()).unwrap();
    assert_eq!(c.bucket, "hamh-live-harness-800000000022");
    assert_eq!(c.table, "hamh-live-harness");
    assert_eq!(c.stage, "live");
    assert_eq!(c.owner, "team-a");
    assert_eq!(c.git_bin, PathBuf::from("git"));
    assert_eq!(c.gitleaks_bin, PathBuf::from("gitleaks"));
    assert_eq!(c.work_root, std::env::temp_dir().join("delivery"));
    let e = env(&[
        ("BUCKET_NAME", "b"),
        ("TABLE_NAME", "t"),
        ("HAMH_STAGE", "drill"),
        ("HAMH_OWNER", "team-a"),
        ("GIT_BIN", "/usr/bin/git"),
        ("GITLEAKS_BIN", "/opt/delivery/bin/gitleaks"),
        ("WORK_ROOT", "/tmp/w"),
    ]);
    let c = Config::from_lookup(|k| e.get(k).cloned()).unwrap();
    assert_eq!(c.stage, "drill");
    assert_eq!(c.git_bin, PathBuf::from("/usr/bin/git"));
    assert_eq!(c.gitleaks_bin, PathBuf::from("/opt/delivery/bin/gitleaks"));
    assert_eq!(c.work_root, PathBuf::from("/tmp/w"));
}

#[test]
fn the_direct_stage_is_a_stage() {
    let e = env(&[
        ("BUCKET_NAME", "hamh-direct-harness-700000000011"),
        ("TABLE_NAME", "hamh-direct-harness"),
        ("HAMH_STAGE", "direct"),
        ("HAMH_OWNER", "team-a"),
    ]);
    let c = Config::from_lookup(|k| e.get(k).cloned()).unwrap();
    assert_eq!(c.stage, "direct");
    assert_eq!(c.table, "hamh-direct-harness");
}

#[test]
fn a_missing_variable_or_an_unknown_stage_is_an_error_naming_it() {
    let e = env(&[
        ("TABLE_NAME", "t"),
        ("HAMH_STAGE", "live"),
        ("HAMH_OWNER", "team-a"),
    ]);
    assert!(
        Config::from_lookup(|k| e.get(k).cloned())
            .unwrap_err()
            .contains("BUCKET_NAME")
    );
    let e = env(&[
        ("BUCKET_NAME", "b"),
        ("TABLE_NAME", "t"),
        ("HAMH_STAGE", "prod"),
        ("HAMH_OWNER", "team-a"),
    ]);
    assert!(
        Config::from_lookup(|k| e.get(k).cloned())
            .unwrap_err()
            .contains("HAMH_STAGE")
    );
}

#[test]
fn the_owner_tag_value_comes_from_the_environment_and_is_required() {
    let base = [
        ("BUCKET_NAME", "b"),
        ("TABLE_NAME", "t"),
        ("HAMH_STAGE", "direct"),
    ];
    let with = |owner: Option<&str>| {
        let mut pairs = base.to_vec();
        if let Some(o) = owner {
            pairs.push(("HAMH_OWNER", o));
        }
        let e = env(&pairs);
        Config::from_lookup(|k| e.get(k).cloned())
    };
    for good in ["team-a", "hamh-user", "A1+=.@_-z", "x", &"a".repeat(64)] {
        assert_eq!(with(Some(good)).unwrap().owner, good);
    }
    assert!(with(None).unwrap_err().contains("HAMH_OWNER"));
    // The value is a tag value of the repositories the gateway creates, and the stack's IAM condition compares it for equality: a value the rule of the account
    // file refuses is refused here as well, at the start of the function and not at the first CreateRepository.
    for bad in [
        "",
        "team a",
        "team/a",
        "team:a",
        "a,b",
        "team*",
        "${aws:username}",
        "a\nb",
        &"a".repeat(65),
    ] {
        let err = with(Some(bad)).unwrap_err();
        assert!(err.contains("HAMH_OWNER"), "{bad:?}: {err}");
    }
}

#[test]
fn the_invocation_line_names_operation_run_and_outcome_only() {
    let input = json!({"operation": "push-pr", "runId": "01K5ZQ4T8M3N6P7R9S0V1W2X3Y", "title": "secret title", "description": "body"});
    let ok: Value = serde_json::from_str(&invocation_line(
        "repo-gateway",
        &input,
        &Ok(json!({"url": "u"})),
    ))
    .unwrap();
    assert_eq!(
        ok,
        json!({"function": "repo-gateway", "operation": "push-pr", "runId": "01K5ZQ4T8M3N6P7R9S0V1W2X3Y", "outcome": "ok"})
    );
    let refused: Value = serde_json::from_str(&invocation_line(
        "repo-gateway",
        &input,
        &Err(DeliveryError::Refused(
            "refused: push to refs/heads/main".into(),
        )),
    ))
    .unwrap();
    assert_eq!(refused["outcome"], "Refused");
    assert_eq!(refused["reason"], "refused: push to refs/heads/main");
    let line = invocation_line("packager", &json!({"runId": 5}), &Ok(json!({})));
    assert!(!line.contains("secret title"));
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["operation"], Value::Null);
    assert_eq!(v["runId"], Value::Null);
}

fn sdk(with_credentials: bool) -> SdkConfig {
    let b = SdkConfig::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-west-2"));
    let b = if with_credentials {
        b.credentials_provider(SharedCredentialsProvider::new(Credentials::new(
            "AKIDEXAMPLE",
            "test-secret",
            None,
            None,
            "test",
        )))
    } else {
        b
    };
    b.build()
}

fn config(root: &std::path::Path) -> Config {
    let e = env(&[
        ("BUCKET_NAME", "hamh-live-harness-800000000022"),
        ("TABLE_NAME", "hamh-live-harness"),
        ("HAMH_STAGE", "live"),
        ("HAMH_OWNER", "team-a"),
        ("GITLEAKS_BIN", "/opt/delivery/bin/gitleaks"),
        ("WORK_ROOT", root.join("w").to_str().unwrap()),
    ]);
    Config::from_lookup(|k| e.get(k).cloned()).unwrap()
}

#[tokio::test]
async fn the_aws_handlers_carry_the_configuration_and_a_home_for_git() {
    let t = tmp();
    let cfg = config(t.path());
    let gw = gateway(&cfg, &sdk(true)).unwrap();
    assert_eq!(
        (gw.bucket.as_str(), gw.stage.as_str(), gw.owner.as_str()),
        ("hamh-live-harness-800000000022", "live", "team-a")
    );
    assert_eq!(gw.work_root, t.path().join("w"));
    assert_eq!(gw.allowlist.table, "hamh-live-harness");
    assert_eq!(gw.blobs.bucket, "hamh-live-harness-800000000022");
    assert_eq!(gw.remotes.region, REGION);
    assert!((gw.clock)() >= 1_790_000_000);
    assert!(t.path().join("w/home").is_dir());
    let v = validator(&cfg, &sdk(true)).unwrap();
    assert_eq!(v.scanner.bin, PathBuf::from("/opt/delivery/bin/gitleaks"));
    assert_eq!(v.scanner.home, t.path().join("w/home"));
    assert_eq!(v.bucket, "hamh-live-harness-800000000022");
    let p = packager(&cfg, &sdk(true)).unwrap();
    assert_eq!(p.artifacts.table, "hamh-live-harness");
    assert_eq!(p.work_root, t.path().join("w"));
    assert!(now_unix() >= 1_790_000_000);
}

#[tokio::test]
async fn a_gateway_without_role_credentials_is_unavailable() {
    let t = tmp();
    assert!(matches!(
        gateway(&config(t.path()), &sdk(false)),
        Err(DeliveryError::Unavailable(_))
    ));
}

#[tokio::test]
async fn the_sdk_configuration_is_pinned_to_the_e2e_region() {
    assert_eq!(
        load_aws().await.region().map(|r| r.to_string()),
        Some(REGION.to_string())
    );
}

#[tokio::test]
async fn each_event_parses_its_request_and_answers_json() {
    let t = tmp();
    let home = t.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let gw = Gateway {
        codecommit: FakeCodeCommit::new(t.path()),
        allowlist: FakeAllowlist::default(),
        blobs: MemBlobs::default(),
        remotes: LocalRemotes::new(t.path()),
        git: Git::new("git", &home),
        stage: "live".into(),
        owner: "team-a".into(),
        bucket: BUCKET.into(),
        work_root: t.path().join("w"),
        clock: || 1_790_337_600,
        repo_names: Default::default(),
    };
    let refused = gateway_event(
        &gw,
        json!({"operation": "stage", "runId": RUN, "target": "cc:python-calc"}),
    )
    .await;
    assert!(
        matches!(refused, Err(DeliveryError::Refused(m)) if m.contains("outside the hamh-e2e- prefix"))
    );
    assert!(matches!(
        gateway_event(&gw, json!({"operation": "stage"})).await,
        Err(DeliveryError::Invalid(_))
    ));
    let v = Validator {
        blobs: MemBlobs::default(),
        scanner: Gitleaks {
            bin: PathBuf::from("gitleaks"),
            home: home.clone(),
        },
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: t.path().join("w"),
    };
    let out = validator_event(
        &v,
        json!({"runId": RUN, "headSha": "a".repeat(40), "allowedGlobs": ["src/**"]}),
    )
    .await
    .unwrap();
    assert_eq!(out["pass"], false);
    assert_eq!(out["violations"][0]["rule"], "bundle");
    assert!(matches!(
        validator_event(&v, json!({"runId": RUN})).await,
        Err(DeliveryError::Invalid(_))
    ));
    let p = Packager {
        blobs: MemBlobs::default(),
        artifacts: FakeArtifacts::default(),
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: t.path().join("w"),
    };
    assert!(matches!(
        packager_event(&p, json!({"runId": RUN})).await,
        Err(DeliveryError::Invalid(_))
    ));
    let direct = json!({"outcome": "reported_done", "label": "Cline reported done", "modelId": "m", "rules": [], "headSource": "final"});
    let missing = packager_event(&p, json!({"runId": RUN, "repo": "hamh-e2e-x", "headSha": "a".repeat(40), "objective": "o", "direct": direct})).await;
    assert!(matches!(missing, Err(DeliveryError::Invalid(m)) if m.contains("no verified bundle")));
}
