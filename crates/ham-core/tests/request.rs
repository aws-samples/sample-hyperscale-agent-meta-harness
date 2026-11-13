//! StartDirectRunRequest building and the Idempotency-Key.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_idempotency_key, direct_run_request};
use harness_protocol::api::v1::{RunTarget, StartDirectRunRequest};
use harness_protocol::target::RepoTarget;

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn start_args(args: &[&str]) -> DirectArgs {
    let mut full = args.to_vec();
    full.push("--direct");
    match parse(&full).unwrap().command {
        Command::StartDirect(s) => s,
        other => panic!("{other:?}"),
    }
}

fn request(args: &[&str]) -> StartDirectRunRequest {
    direct_run_request(
        &start_args(args),
        DirectInputs::default(),
        &ModelPrefs::default(),
    )
    .unwrap()
}

#[test]
fn request_carries_prompt_target_model_and_base() {
    let req = request(&[
        "Add a percent_change(a, b) function with tests",
        "--repo",
        "cc:hamh-e2e-python-calc-20260925",
        "--base",
        "main",
    ]);
    assert_eq!(req.prompt, "Add a percent_change(a, b) function with tests");
    assert_eq!(
        req.target,
        RunTarget::Repo {
            repo: RepoTarget::CodeCommit {
                name: "hamh-e2e-python-calc-20260925".into()
            }
        }
    );
    assert_eq!(req.base_ref.as_deref(), Some("main"));
    assert_eq!(req.model, SONNET, "no model set: Claude Sonnet 5.5");
    assert!(req.rules.is_empty() && req.nudge.is_none());
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(
        json["target"],
        serde_json::json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc-20260925"})
    );
}

#[test]
fn s3_only_and_new_repo_map_to_run_targets() {
    assert_eq!(request(&["p", "--s3-only"]).target, RunTarget::S3Only);
    assert_eq!(
        request(&["p", "--new-repo", "cc:n"]).target,
        RunTarget::NewRepo {
            repo: RepoTarget::CodeCommit { name: "n".into() }
        }
    );
}

#[test]
fn idempotency_key_is_stable_per_request_and_nonce() {
    let req = request(&["p", "--s3-only"]);
    let k1 = direct_idempotency_key(&req, "launch-1");
    assert_eq!(
        k1,
        direct_idempotency_key(&req, "launch-1"),
        "a retry reuses the key"
    );
    assert_ne!(
        k1,
        direct_idempotency_key(&req, "launch-2"),
        "a new launch gets a new key"
    );
    let other = request(&["q", "--s3-only"]);
    assert_ne!(k1, direct_idempotency_key(&other, "launch-1"));
    assert_eq!(k1.len(), 64);
    assert!(
        k1.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}
