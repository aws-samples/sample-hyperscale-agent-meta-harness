//! The engine file reaches the request at the three places that start a run: the
//! command line (`ham "<prompt>" --direct`, the real binary against the local `/v1` API), the composer (`tui::submit_direct_with_env`) and the
//! follow-up (`Job::StartFollowUp`). All three read `engines/<engine>.toml` through `rules_cmd::direct_inputs_for`, once, and send the same
//! `engineConfig` member. A user with no engine file sends none. A file the shared check refuses stops the start before anything is sent, with the
//! line, the reason and "Nothing was sent.", and the command line exits 2.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::Api;
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham_core::command::{Command, parse};
use home::{Home, ham_env, home, text};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use support::Fake;

const PROMPT: &str = "Add a percent_change(a, b) function";
const ENDPOINT: &str = "/v1/direct/runs";
const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";

/// An engine file with two keys, comments and all.
const FILE: &str = "# my settings\nschema = 1\n[reasoning]\neffort = \"low\"   # quick\n[tools]\ndisable = [\"spawn_agent\"]\n";
/// The member that file makes.
fn member() -> Value {
    json!({"engine": "cline", "schema": 1,
           "settings": {"reasoning": {"effort": "low"}, "tools": {"disable": ["spawn_agent"]}}})
}
/// A file with a key the harness locks, on line 4.
const DENIED: &str = "# my settings\nschema = 1\n[aws]\nprofile = \"x\"\n";
const REFUSAL: &str = "engines/cline.toml line 4: key \"aws.profile\" is not allowed: the harness supplies credentials itself; this cannot be set (root invariants 2 and 8). Nothing was sent.";

fn put_engine_file(h: &Home, content: &str) {
    std::fs::create_dir_all(h.config.join("ham/engines")).unwrap();
    std::fs::write(h.config.join("ham/engines/cline.toml"), content).unwrap();
}

// ---- the three entry points -------------------------------------------------------------------------------------------------------

async fn command_line_output(
    engine_file: Option<&str>,
    label: &str,
) -> (std::process::Output, Arc<Fake>) {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    if let Some(f) = engine_file {
        put_engine_file(&h, f);
    }
    // No rules and no skills folder is needed.
    let o = ham_env(
        &h,
        &[PROMPT, "--s3-only", "--direct", "--json", "--no-rules"],
        &[("HAMH_IDEMPOTENCY_NONCE", "run-paths-cli")],
    )
    .await;
    (o, fake)
}

async fn command_line(engine_file: Option<&str>, label: &str) -> Value {
    let (o, fake) = command_line_output(engine_file, label).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[0].body).unwrap()
}

fn composer_env(h: &Home) -> impl Fn(&str) -> Option<String> {
    let (config_dir, home_dir) = (h.config.display().to_string(), h.root.display().to_string());
    move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config_dir.clone()),
        "HOME" => Some(home_dir.clone()),
        _ => None,
    }
}

async fn composer_result(
    engine_file: Option<&str>,
    label: &str,
) -> (
    Result<harness_protocol::api::v1::StartRunResponse, String>,
    Arc<Fake>,
) {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    if let Some(f) = engine_file {
        put_engine_file(&h, f);
    }
    let words: Vec<String> = ["--prompt", PROMPT, "--s3-only", "--direct", "--no-rules"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let Command::StartDirect(args) = parse(&words).unwrap().command else {
        panic!("the words do not make a direct start");
    };
    let env = composer_env(&h);
    let api = Api::new(Arc::new(
        ham::https::HttpsPlane::new(
            &fake.endpoint,
            ham::sigv4::Signer::new(support::REGION, support::creds()),
        )
        .unwrap(),
    ));
    let r = ham::tui::submit_direct_with_env(&api, &args, "run-paths-composer", &env).await;
    (r, fake)
}

async fn composer(engine_file: Option<&str>, label: &str) -> Value {
    let (r, fake) = composer_result(engine_file, label).await;
    r.unwrap();
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[0].body).unwrap()
}

type Call = (String, String, Value);

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<Call>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push((
            method.to_string(),
            path.to_string(),
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        ));
        Ok(Response {
            status: 202,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&json!({"runId": NEW, "status": "active"})).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("not used".into()))
    }
}

/// A home for the follow-up: the rules folder, a `config.toml` and, when given, the engine file.
fn follow_up_env(label: &str, engine_file: Option<&str>) -> impl Fn(&str) -> Option<String> {
    let root = support::scratch(label);
    let ham = root.join("config/ham");
    std::fs::create_dir_all(ham.join("rules")).unwrap();
    std::fs::write(ham.join("rules/10-tdd.md"), "Write the test first.\n").unwrap();
    std::fs::write(
        ham.join("config.toml"),
        "apiEndpoint = \"https://unused.example/v1\"\n",
    )
    .unwrap();
    if let Some(f) = engine_file {
        std::fs::create_dir_all(ham.join("engines")).unwrap();
        std::fs::write(ham.join("engines/cline.toml"), f).unwrap();
    }
    let (config_dir, home_dir) = (root.join("config"), root);
    move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config_dir.display().to_string()),
        "HOME" => Some(home_dir.display().to_string()),
        _ => None,
    }
}

async fn follow_up_result(
    engine_file: Option<&str>,
    label: &str,
) -> (Result<String, String>, Vec<Call>) {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let env = follow_up_env(label, engine_file);
    let done = perform_with_env(
        &api,
        Job::StartFollowUp {
            run_id: RUN.into(),
            text: "now add the docs".into(),
            head: None,
        },
        &env,
    )
    .await;
    let Done::FollowUpStarted { result, .. } = done else {
        panic!("{done:?}");
    };
    let calls = plane.calls.lock().unwrap().clone();
    (result, calls)
}

async fn follow_up(engine_file: Option<&str>, label: &str) -> Value {
    let (result, calls) = follow_up_result(engine_file, label).await;
    assert_eq!(result, Ok(NEW.to_string()));
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("POST", ENDPOINT)
    );
    calls[0].2.clone()
}

// ---- the tests --------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_composer_and_follow_up_send_the_same_member() {
    let cli = command_line(Some(FILE), "rp-cli").await;
    let tui = composer(Some(FILE), "rp-tui").await;
    let fu = follow_up(Some(FILE), "rp-fu").await;
    for (site, sent) in [
        ("command line", &cli),
        ("composer", &tui),
        ("follow-up", &fu),
    ] {
        assert_eq!(sent["engineConfig"], member(), "{site}: {sent}");
        // Comments never leave the Mac.
        assert!(!sent.to_string().contains("my settings"), "{site}");
        assert!(!sent.to_string().contains("quick"), "{site}");
    }
    println!(
        "RUN-PATHS TRIGGER PRESENT: the engine file holds {} keys; all three sites sent engineConfig {}",
        2, cli["engineConfig"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_without_an_engine_file_sends_no_member_on_any_path() {
    let cli = command_line(None, "rp-none-cli").await;
    let tui = composer(None, "rp-none-tui").await;
    let fu = follow_up(None, "rp-none-fu").await;
    for (site, sent) in [
        ("command line", &cli),
        ("composer", &tui),
        ("follow-up", &fu),
    ] {
        assert!(sent.get("engineConfig").is_none(), "{site}: {sent}");
    }
    // A file that sets no key sends none either, and the folder is not made by reading.
    let only_schema = command_line(Some("schema = 1\n"), "rp-schema-cli").await;
    assert!(only_schema.get("engineConfig").is_none(), "{only_schema}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_the_check_refuses_stops_every_path_before_anything_is_sent() {
    // The command line: exit 2, the line, the reason, the invariant and "Nothing was sent.", and no request reached the stage.
    let (o, fake) = command_line_output(Some(DENIED), "rp-denied-cli").await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains(REFUSAL), "{}", text(&o.stderr));
    assert!(fake.mem.posts(ENDPOINT).is_empty(), "a request was sent");
    // The composer: the same words as the error, no request.
    let (r, fake) = composer_result(Some(DENIED), "rp-denied-tui").await;
    let e = r.unwrap_err();
    assert!(e.contains(REFUSAL), "{e}");
    assert!(fake.mem.posts(ENDPOINT).is_empty(), "a request was sent");
    // The follow-up: the same words, and the plane saw no call.
    let (result, calls) = follow_up_result(Some(DENIED), "rp-denied-fu").await;
    assert!(result.unwrap_err().contains(REFUSAL));
    assert!(calls.is_empty(), "{calls:?}");
}
