//! Guard: the request of a user with no engine file is byte for byte the
//! request that protocol 1.38.0 builds. It is green before the engine file exists and must stay green after: the `engineConfig` member is left out
//! when absent. The bytes were captured from the build before the change (main at `e5a2ea75e`, protocol 1.39.0 with no sender of the member).
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, parse};
use ham_core::models::ModelPrefs;
use ham_core::request::{DirectInputs, direct_run_request};

fn request_body(words: &[&str]) -> String {
    let Command::StartDirect(args) = parse(words).unwrap().command else {
        panic!("not a start");
    };
    let r = direct_run_request(&args, DirectInputs::default(), &ModelPrefs::default()).unwrap();
    serde_json::to_string(&r).unwrap()
}

#[test]
fn a_default_request_has_no_engine_member_and_is_the_v4_02_bytes() {
    let body = request_body(&["Add a function", "--s3-only", "--direct"]);
    assert!(!body.contains("engineConfig"), "{body}");
    assert_eq!(body, GOLDEN_S3_ONLY);
    let body = request_body(&[
        "Add a function",
        "--repo",
        "cc:calc",
        "--direct",
        "--model",
        "opus",
        "--goal",
        "off",
    ]);
    assert!(!body.contains("engineConfig"), "{body}");
    assert_eq!(body, GOLDEN_REPO);
}

const GOLDEN_S3_ONLY: &str = r#"{"prompt":"Add a function","target":{"mode":"s3-only"},"model":"global.anthropic.claude-sonnet-5-5","rules":[]}"#;
const GOLDEN_REPO: &str = r#"{"prompt":"Add a function","target":{"mode":"repo","repo":"cc:calc"},"model":"us.anthropic.claude-opus-5-5","rules":[],"goal":{"enabled":false}}"#;
