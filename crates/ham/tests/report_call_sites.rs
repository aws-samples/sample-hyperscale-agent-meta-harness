//! `ham_core::request::with_report_setting` has three call sites, and each one is exercised here through its real path. Before this
//! file, removing a call from any of them failed no test, so a user who set `report_tests = false` could keep getting the clean-checkout test
//! report without anything noticing:
//!
//! - the command line: the real `ham` binary runs `ham "<prompt>" --direct ...` against the local `/v1` API (`cli.rs`, the start of a direct run);
//! - the composer: the real `submit_direct` of the dashboard (`tui/mod.rs`), through `tui::submit_direct_with_env`;
//! - the follow-up: the second Enter of a finished run's follow-up (`tui/ext.rs`, `Job::StartFollowUp`), against a scripted plane that keeps the
//!   body the client sent.
//!
//! The setting is a key of `config.toml`, read by each site at the moment it starts a run. The report is on by default, so the request carries
//! `reportTests: false` only when the user switched it off; any other value, or no key, leaves the member out and the request as it always was.
//! The stand-in API runs the real control-api handler, so a request that reaches it is also the request the service stores.
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

const MODEL: &str = "global.anthropic.claude-sonnet-5-5";
const PROMPT: &str = "Add a percent_change(a, b) function";
const ENDPOINT: &str = "/v1/direct/runs";
const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW: &str = "01M5A2R7K3C9XJ4D8E6FNEWRUN";

/// The config lines a user can write, in both spellings of the key, and what each one means for the request.
const OFF: [&str; 2] = ["reportTests = false", "report_tests = false"];
const NOT_OFF: [&str; 3] = ["reportTests = true", "report_tests = true", ""];

fn config_line(h: &Home, line: &str) {
    use std::io::Write;
    if line.is_empty() {
        return;
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(h.config.join("ham/config.toml"))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

fn body(fake: &Fake, n: usize) -> Value {
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[n].body).unwrap()
}

fn marker(site: &str, config: &str, member: &Value) {
    println!(
        "REPORT-CALL-SITE TRIGGER PRESENT: site={site} config={:?} reportTests={member}",
        if config.is_empty() { "(none)" } else { config }
    );
}

// ---- the command line -------------------------------------------------------------------------------------------------------------

async fn command_line(config: &str, label: &str) -> (Arc<Fake>, Value, Value) {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    config_line(&h, config);
    let o = ham_env(
        &h,
        &[
            PROMPT,
            "--s3-only",
            "--direct",
            "--model",
            MODEL,
            "--no-rules",
            "--json",
        ],
        &[("HAMH_IDEMPOTENCY_NONCE", "report-cli")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let first: Value = serde_json::from_str(text(&o.stdout).lines().next().unwrap()).unwrap();
    let stored = fake.mem.stored_request_of(first["runId"].as_str().unwrap());
    let sent = body(&fake, 0);
    (fake, sent, stored)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_line_sends_report_tests_false_only_when_the_user_switched_it_off() {
    for (i, line) in OFF.iter().enumerate() {
        let (_fake, sent, stored) = command_line(line, &format!("report-cli-off-{i}")).await;
        assert_eq!(sent["reportTests"], json!(false), "{line}: {sent}");
        assert_eq!(
            stored["reportTests"],
            json!(false),
            "{line}: the service kept it: {stored}"
        );
        marker("command-line", line, &sent["reportTests"]);
    }
    for (i, line) in NOT_OFF.iter().enumerate() {
        let (_fake, sent, stored) = command_line(line, &format!("report-cli-on-{i}")).await;
        assert!(sent.get("reportTests").is_none(), "{line}: {sent}");
        assert!(stored.get("reportTests").is_none(), "{line}: {stored}");
    }
}

// ---- the composer -----------------------------------------------------------------------------------------------------------------

async fn composer(config: &str, label: &str) -> (Arc<Fake>, Value, Value) {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, label);
    config_line(&h, config);
    let words: Vec<String> = [
        "--prompt",
        PROMPT,
        "--s3-only",
        "--no-rules",
        "--direct",
        "--model",
        MODEL,
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let Command::StartDirect(args) = parse(&words).unwrap().command else {
        panic!("the words do not make a direct start");
    };
    let (config_dir, home_dir) = (h.config.display().to_string(), h.root.display().to_string());
    let env = move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config_dir.clone()),
        "HOME" => Some(home_dir.clone()),
        _ => None,
    };
    let api = Api::new(Arc::new(
        ham::https::HttpsPlane::new(
            &fake.endpoint,
            ham::sigv4::Signer::new(support::REGION, support::creds()),
        )
        .unwrap(),
    ));
    let started = ham::tui::submit_direct_with_env(&api, &args, "report-composer", &env)
        .await
        .unwrap();
    let stored = fake.mem.stored_request_of(&started.run_id);
    let sent = body(&fake, 0);
    (fake, sent, stored)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_composer_sends_report_tests_false_only_when_the_user_switched_it_off() {
    for (i, line) in OFF.iter().enumerate() {
        let (_fake, sent, stored) = composer(line, &format!("report-tui-off-{i}")).await;
        assert_eq!(sent["reportTests"], json!(false), "{line}: {sent}");
        assert_eq!(stored["reportTests"], json!(false), "{line}: {stored}");
        marker("composer", line, &sent["reportTests"]);
    }
    for (i, line) in NOT_OFF.iter().enumerate() {
        let (_fake, sent, stored) = composer(line, &format!("report-tui-on-{i}")).await;
        assert!(sent.get("reportTests").is_none(), "{line}: {sent}");
        assert!(stored.get("reportTests").is_none(), "{line}: {stored}");
    }
}

// ---- the follow-up ----------------------------------------------------------------------------------------------------------------

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

/// A home with the user's rules, as `ham rules init` leaves it, and `config` appended to `config.toml`.
fn follow_up_env(label: &str, config: &str) -> impl Fn(&str) -> Option<String> {
    let root = support::scratch(label);
    let ham = root.join("config/ham");
    std::fs::create_dir_all(ham.join("rules")).unwrap();
    std::fs::write(ham.join("rules/10-tdd.md"), "Write the test first.\n").unwrap();
    std::fs::write(ham.join("config.toml"), format!("{config}\n")).unwrap();
    let (config_dir, home_dir) = (root.join("config"), root);
    move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config_dir.display().to_string()),
        "HOME" => Some(home_dir.display().to_string()),
        _ => None,
    }
}

async fn follow_up(config: &str, label: &str) -> Value {
    let plane = Arc::new(Scripted::default());
    let api = Api::new(plane.clone());
    let env = follow_up_env(label, config);
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
    match done {
        Done::FollowUpStarted { result, .. } => assert_eq!(result, Ok(NEW.to_string())),
        other => panic!("{other:?}"),
    }
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].0, "POST");
    assert_eq!(calls[0].1, ENDPOINT);
    assert_eq!(calls[0].2["continueFrom"], json!({"runId": RUN}));
    calls[0].2.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_follow_up_sends_report_tests_false_only_when_the_user_switched_it_off() {
    for (i, line) in OFF.iter().enumerate() {
        let sent = follow_up(line, &format!("report-fu-off-{i}")).await;
        assert_eq!(sent["reportTests"], json!(false), "{line}: {sent}");
        marker("follow-up", line, &sent["reportTests"]);
    }
    for (i, line) in NOT_OFF.iter().enumerate() {
        let sent = follow_up(line, &format!("report-fu-on-{i}")).await;
        assert!(sent.get("reportTests").is_none(), "{line}: {sent}");
    }
}

// ---- all three ----------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_three_sites_agree_for_the_same_setting() {
    for line in ["reportTests = false", ""] {
        let (_f1, cli, _) = command_line(line, "report-all-cli").await;
        let (_f2, tui, _) = composer(line, "report-all-tui").await;
        let fu = follow_up(line, "report-all-fu").await;
        let want = if line.is_empty() {
            None
        } else {
            Some(json!(false))
        };
        for (site, sent) in [
            ("command line", &cli),
            ("composer", &tui),
            ("follow-up", &fu),
        ] {
            assert_eq!(sent.get("reportTests").cloned(), want, "{site}: {sent}");
        }
    }
}
