//! Provider data sharing with several backends: the AWS account whose Bedrock data retention mode ham reads, names and switches is the
//! account of the backend the run starts on, on every route that starts a run. A follow-up, a resume and a retry start on the backend that holds
//! the run they continue; a new run starts on the backend it is aimed at, else on `default_backend`. With several backends and none chosen
//! nothing is read and nothing is switched.
//!
//! Each backend is a stand-in with its own account, Region and mode; every call is recorded with the alias of the backend that got it. No call
//! leaves the machine.
#![allow(clippy::unwrap_used)]

use ham::api::Api;
use ham::backends::{Backends, BoxBuild, BuildFailure};
use ham::http::{ControlPlane, Response, RetentionAnswer, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham::tui::{Msg, submit_msg_with_env};
use ham_core::backends::BackendDef;
use ham_core::command::{Command, DirectArgs, parse};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const FABLE: &str = "global.anthropic.claude-fable-5-2";
const SOURCE_RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const NEW_RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTX";
const WORK: (&str, &str, &str) = ("work", "111111111111", "us-east-1");
const LAB: (&str, &str, &str) = ("lab", "222222222222", "us-west-2");

type Answer = Option<Result<RetentionAnswer, TransportError>>;
type Log = Arc<Mutex<Vec<String>>>;

/// One backend: its own account and Region, its own Bedrock data retention mode, and whether it holds the source run.
struct Stand {
    alias: String,
    account: String,
    region: String,
    holds_source: bool,
    /// The model the source run was started on, as its row of the run list names it.
    row_model: String,
    mode: Mutex<String>,
    log: Log,
}

impl Stand {
    fn retention(&self) -> Answer {
        let m = self.mode.lock().unwrap().clone();
        Some(Ok(RetentionAnswer {
            region: self.region.clone(),
            response: Response {
                status: 200,
                body: format!(r#"{{"mode":"{m}","updatedAt":"2026-11-10T16:00:00Z"}}"#)
                    .into_bytes(),
                ..Response::default()
            },
        }))
    }

    /// `path` is the resume or the retry of the source run, and this backend holds that run.
    fn continues_here(&self, path: &str) -> bool {
        let source = format!("/v1/runs/{SOURCE_RUN}");
        self.holds_source
            && (path == format!("{source}/resume") || path == format!("{source}/retry"))
    }

    fn say(&self, what: String) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} {what}", self.alias));
    }
}

/// The run list: `GET /v1/runs`, with or without a filter.
fn is_list(path: &str) -> bool {
    path == "/v1/runs" || path.starts_with("/v1/runs?")
}

impl ControlPlane for Stand {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.say(format!("{method} {path}"));
        let mut headers = BTreeMap::new();
        headers.insert(
            "x-hamh-backend-version".to_string(),
            ham::version::CLIENT.to_string(),
        );
        let source = format!("/v1/runs/{SOURCE_RUN}");
        let started = serde_json::json!({"runId": NEW_RUN, "status": "active"});
        let (status, body) = if path == "/v1/whoami" {
            (
                200,
                serde_json::json!({"principalArn": format!("arn:aws:sts::{}:assumed-role/A/b", self.account), "account": self.account}),
            )
        } else if method == "GET" && path == source && self.holds_source {
            (200, serde_json::json!({"summary": {"runId": SOURCE_RUN}}))
        } else if method == "GET" && is_list(path) && self.holds_source {
            // The run list: the row of the source run names the model it was started on.
            (
                200,
                serde_json::json!({"runs": [{
                    "runId": SOURCE_RUN, "status": "paused", "phase": "green", "target": {"mode": "s3-only"},
                    "createdAt": "2026-11-10T16:00:00Z", "ownerPrincipalArn": format!("arn:aws:sts::{}:assumed-role/A/b", self.account),
                    "title": "build x", "model": self.row_model, "resumeMode": "follow_up"
                }]}),
            )
        } else if method == "GET" && is_list(path) {
            (200, serde_json::json!({"runs": []}))
        } else if method == "POST" && (path == "/v1/direct/runs" || self.continues_here(path)) {
            // A new run starts on any backend; a resume and a retry start only where the source run is.
            (201, started)
        } else {
            (
                404,
                serde_json::json!({"code": "not-found", "message": "no"}),
            )
        };
        Ok(Response {
            status,
            headers,
            body: body.to_string().into_bytes(),
        })
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Ok(Response::default())
    }

    async fn data_retention(&self, _target: Option<&str>) -> Answer {
        self.say("GET bedrock /data-retention".to_string());
        self.retention()
    }

    async fn put_data_retention(&self, _target: Option<&str>, mode: &str) -> Answer {
        self.say(format!(
            "PUT bedrock /data-retention {mode} (account {} {})",
            self.account, self.region
        ));
        *self.mode.lock().unwrap() = mode.to_string();
        self.retention()
    }
}

/// The backends of `config.toml` in file order, with `default` as `default_backend`; the finished run is on `holder`. Every account is in the
/// mode `inherit` and the source run was started on Claude Fable 5.2.
async fn backends(
    stands: &[(&str, &str, &str)],
    default: Option<&str>,
    holder: &str,
    log: &Log,
) -> Arc<Backends<Stand>> {
    backends_in(stands, default, holder, "inherit", FABLE, log).await
}

/// [`backends`] with the mode every account is in and the model of the source run.
async fn backends_in(
    stands: &[(&str, &str, &str)],
    default: Option<&str>,
    holder: &str,
    mode: &str,
    row_model: &str,
    log: &Log,
) -> Arc<Backends<Stand>> {
    let planes: Arc<Mutex<BTreeMap<String, Stand>>> = Arc::new(Mutex::new(
        stands
            .iter()
            .map(|(alias, account, region)| {
                (
                    (*alias).to_string(),
                    Stand {
                        alias: (*alias).into(),
                        account: (*account).into(),
                        region: (*region).into(),
                        holds_source: *alias == holder,
                        row_model: row_model.into(),
                        mode: Mutex::new(mode.into()),
                        log: Arc::clone(log),
                    },
                )
            })
            .collect(),
    ));
    let factory = move |d: BackendDef| -> BoxBuild<Stand> {
        let r = planes.lock().unwrap().remove(&d.alias).ok_or(BuildFailure {
            text: "none".into(),
            credentials: false,
        });
        Box::pin(async move { r })
    };
    let defs = stands
        .iter()
        .map(|(alias, _, _)| BackendDef {
            alias: (*alias).into(),
            api_endpoint: Some(format!("https://{alias}.example/direct")),
            region: None,
            aws_profile: Some((*alias).into()),
            customer_content: false,
        })
        .collect();
    Arc::new(
        Backends::connect(defs, default.map(str::to_string), factory)
            .await
            .unwrap(),
    )
}

/// A config folder whose `config.toml` makes Claude Fable 5.2 the default model, with `run.provider_sharing` as given, and the environment that
/// finds it.
fn env_with(label: &str, sharing_on: bool) -> impl Fn(&str) -> Option<String> + Sync + use<> {
    env_for(label, FABLE, sharing_on)
}

/// [`env_with`] with `model` as the default model of the config.
fn env_for(
    label: &str,
    model: &str,
    sharing_on: bool,
) -> impl Fn(&str) -> Option<String> + Sync + use<> {
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "ham-test-fix515-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(root.join("config/ham")).unwrap();
    let sharing = if sharing_on {
        "provider_sharing = true\n"
    } else {
        ""
    };
    std::fs::write(
        root.join("config/ham/config.toml"),
        format!("defaultModel = \"{model}\"\n\n[run]\n{sharing}rules = false\n"),
    )
    .unwrap();
    let (config, home) = (
        root.join("config").display().to_string(),
        root.display().to_string(),
    );
    move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config.clone()),
        "HOME" => Some(home.clone()),
        _ => None,
    }
}

fn follow_up() -> Job {
    Job::StartFollowUp {
        run_id: SOURCE_RUN.into(),
        text: "now the docs".into(),
        head: None,
        engine: None,
    }
}

fn args(extra: &[&str]) -> DirectArgs {
    let mut words: Vec<String> = ["--prompt", "build x", "--s3-only", "--no-rules"]
        .iter()
        .map(ToString::to_string)
        .collect();
    words.extend(extra.iter().map(ToString::to_string));
    words.extend(["--direct", "--model", FABLE].map(ToString::to_string));
    match parse(&words).unwrap().command {
        Command::StartDirect(a) => a,
        other => panic!("not a start: {other:?}"),
    }
}

fn calls(log: &Log) -> Vec<String> {
    let calls = log.lock().unwrap().clone();
    for c in &calls {
        println!("FIX515 CALL: {c}");
    }
    calls
}

fn bedrock_calls(calls: &[String], alias: &str) -> Vec<String> {
    calls
        .iter()
        .filter(|c| c.starts_with(&format!("{alias} ")) && c.contains(" bedrock "))
        .cloned()
        .collect()
}

/// The mode of `alias`'s account was read, switched to `provider_data_share` and the run started there; no other backend's account was read or
/// switched, and no run started anywhere else.
fn only_this_account(calls: &[String], this: (&str, &str, &str), other: &str, start: &str) {
    let (alias, account, region) = this;
    println!(
        "FIX515 TRIGGER PRESENT: several backends, the run starts on {alias} (account {account}, {region})"
    );
    assert!(
        calls.contains(&format!("{alias} GET bedrock /data-retention")),
        "the mode of the run's own account is read: {calls:#?}"
    );
    assert!(
        calls.contains(&format!(
            "{alias} PUT bedrock /data-retention provider_data_share (account {account} {region})"
        )),
        "the mode of the run's own account is switched: {calls:#?}"
    );
    assert_eq!(
        bedrock_calls(calls, other),
        Vec::<String>::new(),
        "the account of a backend the run does not start on is neither read nor switched"
    );
    assert!(
        calls.contains(&format!("{alias} POST {start}")),
        "{calls:#?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with(&format!("{other} POST "))),
        "{calls:#?}"
    );
}

fn names_only(line: &str, this: (&str, &str, &str), other: (&str, &str, &str)) {
    assert!(
        line.contains(this.1) && line.contains(this.2),
        "the line names the account and the Region that changed: {line}"
    );
    assert!(
        !line.contains(other.1) && !line.contains(other.2),
        "the line names no other account or Region: {line}"
    );
}

// ---- the follow-up of the terminal app ------------------------------------------------------------------------------------------------------

/// `work` is first in the file and is `default_backend`; the finished run is on `lab`. The terminal app's client is aimed at no backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_follow_up_reads_and_switches_the_account_of_the_backend_that_holds_its_run() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    assert_eq!(api.target(), None);
    let done = perform_with_env(&api, follow_up(), &env_with("followup", true)).await;
    let calls = calls(&log);
    let Done::FollowUpNoted {
        result: Ok(new_run),
        note,
        ..
    } = done
    else {
        panic!("the follow-up did not start with a line: {done:?}")
    };
    assert_eq!(new_run, NEW_RUN);
    only_this_account(&calls, LAB, "work", "/v1/direct/runs");
    names_only(&note, LAB, WORK);
}

/// The same with `lab`, the second backend of the file, as `default_backend`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_follow_up_names_the_account_it_changed_when_the_second_backend_is_the_default() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("lab"), "lab", &log).await);
    let done = perform_with_env(&api, follow_up(), &env_with("followup-default", true)).await;
    let calls = calls(&log);
    let Done::FollowUpNoted {
        result: Ok(_),
        note,
        ..
    } = done
    else {
        panic!("the follow-up did not start with a line: {done:?}")
    };
    only_this_account(&calls, LAB, "work", "/v1/direct/runs");
    names_only(&note, LAB, WORK);
}

/// The finished run is on `work`, the first backend, and `lab` is `default_backend`: the run's backend decides, not the default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_follow_up_of_a_run_on_the_first_backend_leaves_the_default_backend_alone() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("lab"), "work", &log).await);
    let done = perform_with_env(&api, follow_up(), &env_with("followup-first", true)).await;
    let calls = calls(&log);
    let Done::FollowUpNoted {
        result: Ok(_),
        note,
        ..
    } = done
    else {
        panic!("the follow-up did not start with a line: {done:?}")
    };
    only_this_account(&calls, WORK, "lab", "/v1/direct/runs");
    names_only(&note, WORK, LAB);
}

/// A client aimed at `work` (the dashboard of a run that was started with `--backend work`) and a follow-up of a run that `lab` holds: the start
/// is refused, and it is refused before any account is read or switched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_follow_up_aimed_at_another_backend_than_its_run_changes_no_account() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await).on("work");
    let done = perform_with_env(&api, follow_up(), &env_with("followup-elsewhere", true)).await;
    let calls = calls(&log);
    let (Done::FollowUpStarted { result, .. } | Done::FollowUpNoted { result, .. }) = done else {
        panic!("not the answer of a follow-up")
    };
    let why = result.expect_err("a follow-up starts where its run is");
    assert!(why.contains("lab") && why.contains(SOURCE_RUN), "{why}");
    assert_eq!(bedrock_calls(&calls, "work"), Vec::<String>::new());
    assert_eq!(bedrock_calls(&calls, "lab"), Vec::<String>::new());
    assert!(!calls.iter().any(|c| c.contains(" POST ")), "{calls:#?}");
}

/// A client aimed at the backend that holds the run (`--backend lab`, or the dashboard of a run on `lab`): the follow-up starts there, and the
/// account of that backend is the one read and switched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_follow_up_aimed_at_the_backend_that_holds_its_run_switches_that_account() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await).on("lab");
    let done = perform_with_env(&api, follow_up(), &env_with("followup-aimed", true)).await;
    let calls = calls(&log);
    let Done::FollowUpNoted {
        result: Ok(_),
        note,
        ..
    } = done
    else {
        panic!("the follow-up did not start with a line: {done:?}")
    };
    only_this_account(&calls, LAB, "work", "/v1/direct/runs");
    names_only(&note, LAB, WORK);
}

// ---- a new run of the composer --------------------------------------------------------------------------------------------------------------

/// A start that names no backend goes to `default_backend`; the account of that backend is the one read and switched, also when another
/// backend is first in the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_new_run_that_names_no_backend_reads_and_switches_the_account_of_the_default_backend()
 {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("lab"), "lab", &log).await);
    let env = env_with("composer-default", true);
    let msg = submit_msg_with_env(&api, &args(&[]), "fix515-nonce", None, &env).await;
    let calls = calls(&log);
    let Msg::StartedWith { note, .. } = msg else {
        panic!("the start did not answer with a line: {msg:?}")
    };
    only_this_account(&calls, LAB, "work", "/v1/direct/runs");
    names_only(&note, LAB, WORK);
}

/// A start aimed at a backend (the composer's Backend field, `--backend`) reads and switches the account of that backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_new_run_aimed_at_a_backend_reads_and_switches_the_account_of_that_backend() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "work", &log).await).on("lab");
    let env = env_with("composer-aimed", true);
    let msg = submit_msg_with_env(&api, &args(&[]), "fix515-nonce", Some("lab".into()), &env).await;
    let calls = calls(&log);
    let Msg::StartedWith { note, .. } = msg else {
        panic!("the start did not answer with a line: {msg:?}")
    };
    only_this_account(&calls, LAB, "work", "/v1/direct/runs");
    names_only(&note, LAB, WORK);
}

/// Several backends, no `default_backend` and no backend named: the start is refused as before, and no account is read or switched first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_new_run_with_no_backend_chosen_reads_and_switches_no_account() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], None, "lab", &log).await);
    let env = env_with("composer-none", true);
    let msg = submit_msg_with_env(&api, &args(&[]), "fix515-nonce", None, &env).await;
    let calls = calls(&log);
    let Msg::Started(Err(why)) = msg else {
        panic!("a start with no backend chosen is refused: {msg:?}")
    };
    assert!(
        why.contains("--backend") && why.contains("default_backend"),
        "{why}"
    );
    assert_eq!(bedrock_calls(&calls, "work"), Vec::<String>::new());
    assert_eq!(bedrock_calls(&calls, "lab"), Vec::<String>::new());
    assert!(!calls.iter().any(|c| c.contains(" POST ")), "{calls:#?}");
}

/// The question of the composer (the setting off) names the account and the Region of the backend the run would start on, and reads no other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_the_question_names_the_account_of_the_backend_the_run_would_start_on() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("lab"), "lab", &log).await);
    let env = env_with("composer-ask", false);
    let msg = submit_msg_with_env(&api, &args(&[]), "fix515-nonce", None, &env).await;
    let calls = calls(&log);
    let Msg::SharingAsk(ask) = msg else {
        panic!("a pick of a model that needs sharing asks first: {msg:?}")
    };
    println!("FIX515 QUESTION: {}", ask.question);
    names_only(&ask.question, LAB, WORK);
    assert!(
        ask.question.contains("from inherit to provider_data_share"),
        "{}",
        ask.question
    );
    assert_eq!(
        bedrock_calls(&calls, "lab"),
        vec!["lab GET bedrock /data-retention".to_string()]
    );
    assert_eq!(bedrock_calls(&calls, "work"), Vec::<String>::new());
    assert!(!calls.iter().any(|c| c.contains(" POST ")), "{calls:#?}");
}

// ---- the set of backends itself -------------------------------------------------------------------------------------------------------------

/// With several backends the read and the switch name their backend. A call that names none is refused by the client and reaches no backend:
/// no account is picked by its place in the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_with_several_backends_a_read_or_a_switch_that_names_no_backend_reaches_none() {
    let log = Log::default();
    let set = backends(&[WORK, LAB], Some("work"), "lab", &log).await;
    let read = set.data_retention(None).await;
    let put = set.put_data_retention(None, "provider_data_share").await;
    for (what, answer) in [("read", read), ("switch", put)] {
        let Some(Err(e)) = answer else {
            panic!("the {what} that names no backend is refused")
        };
        let why = e.to_string();
        assert!(why.contains("work") && why.contains("lab"), "{what}: {why}");
    }
    assert_eq!(calls(&log), Vec::<String>::new(), "no backend was called");

    let read = set.data_retention(Some("lab")).await.unwrap().unwrap();
    assert_eq!(read.region, LAB.2);
    let put = set
        .put_data_retention(Some("lab"), "provider_data_share")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(put.region, LAB.2);
    assert_eq!(
        calls(&log),
        vec![
            "lab GET bedrock /data-retention".to_string(),
            format!(
                "lab PUT bedrock /data-retention provider_data_share (account {} {})",
                LAB.1, LAB.2
            ),
        ]
    );
}

/// A client with one backend names none: the read and the switch go to that backend, as they always did, and a follow-up switches its account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_with_one_backend_the_read_and_the_switch_go_to_it() {
    let log = Log::default();
    let set = backends(&[LAB], None, "lab", &log).await;
    let read = set.data_retention(None).await.unwrap().unwrap();
    assert_eq!(read.region, LAB.2);
    assert!(
        String::from_utf8_lossy(&read.response.body).contains("inherit"),
        "{read:?}"
    );
    let api = Api::new(set);
    let done = perform_with_env(&api, follow_up(), &env_with("one-backend", true)).await;
    let calls = calls(&log);
    let Done::FollowUpNoted {
        result: Ok(_),
        note,
        ..
    } = done
    else {
        panic!("the follow-up did not start with a line: {done:?}")
    };
    assert!(note.contains(LAB.1) && note.contains(LAB.2), "{note}");
    assert!(
        calls.contains(&format!(
            "lab PUT bedrock /data-retention provider_data_share (account {} {})",
            LAB.1, LAB.2
        )),
        "{calls:#?}"
    );
    assert!(
        calls.contains(&"lab POST /v1/direct/runs".to_string()),
        "{calls:#?}"
    );
}

// ---- a resume and a retry start from the stored request ------------------------------------------------------------------------------------

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

fn resume() -> Job {
    Job::ResumeRuns {
        run_ids: vec![SOURCE_RUN.into()],
    }
}

fn retry() -> Job {
    Job::RetryRun {
        run_id: SOURCE_RUN.into(),
    }
}

/// What the screen is told, as text: the outcome of each run and, when the account was looked at, its line.
fn told(done: &Done) -> String {
    let text = format!("{done:?}");
    println!("FIX515 ANSWER: {text}");
    text
}

/// `R` on the Runs screen with `run.provider_sharing` on: the stored request of the run on `lab` needs provider data sharing, so the mode of
/// `lab`'s account is switched before the service is asked to start the follow-up, and the answer carries the line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_resume_switches_the_account_of_the_runs_backend_when_the_setting_is_on() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let done = perform_with_env(&api, resume(), &env_with("resume-on", true)).await;
    let calls = calls(&log);
    let text = told(&done);
    only_this_account(
        &calls,
        LAB,
        "work",
        &format!("/v1/runs/{SOURCE_RUN}/resume"),
    );
    assert!(text.contains(NEW_RUN), "the follow-up started: {text}");
    names_only(&text, LAB, WORK);
    assert!(text.contains("run.provider_sharing is on"), "{text}");
    let put = calls
        .iter()
        .position(|c| c.contains(" PUT bedrock "))
        .unwrap();
    let post = calls.iter().position(|c| c.ends_with("/resume")).unwrap();
    assert!(put < post, "the switch comes before the start: {calls:#?}");
}

/// The same with the setting off: nobody agreed to a switch for this new run, so ham switches nothing and starts nothing, and says why in
/// one plain message that names the account, the Region, the mode and both ways on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_resume_with_the_setting_off_stops_before_the_run_with_one_plain_message() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let done = perform_with_env(&api, resume(), &env_with("resume-off", false)).await;
    let calls = calls(&log);
    let text = told(&done);
    println!(
        "FIX515 TRIGGER PRESENT: a paused run on a model that needs sharing, the mode set back to inherit"
    );
    assert_eq!(
        bedrock_calls(&calls, "lab"),
        vec!["lab GET bedrock /data-retention".to_string()],
        "the mode is read and not switched"
    );
    assert_eq!(bedrock_calls(&calls, "work"), Vec::<String>::new());
    assert!(
        !calls.iter().any(|c| c.contains(" POST ")),
        "no run is started: {calls:#?}"
    );
    assert!(!text.contains(NEW_RUN), "{text}");
    for want in [
        "Claude Fable 5.2",
        LAB.1,
        LAB.2,
        "inherit",
        "ham config set run.provider_sharing on",
        "aws bedrock put-account-data-retention --region us-west-2 --mode provider_data_share",
        "No run was started",
    ] {
        assert!(text.contains(want), "{want:?} missing: {text}");
    }
}

/// A mode that already lets the model run: the resume goes as it always did, and nothing is switched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_resume_in_a_mode_that_lets_the_model_run_switches_nothing() {
    for (label, sharing_on) in [("resume-keep-on", true), ("resume-keep-off", false)] {
        let log = Log::default();
        let set = backends_in(&[WORK, LAB], Some("work"), "lab", "aws_review", FABLE, &log).await;
        let done = perform_with_env(&Api::new(set), resume(), &env_with(label, sharing_on)).await;
        let calls = calls(&log);
        let text = told(&done);
        assert!(text.contains(NEW_RUN), "{label}: {text}");
        assert!(
            !calls.iter().any(|c| c.contains(" PUT bedrock ")),
            "{label}: {calls:#?}"
        );
        assert!(
            calls.contains(&format!("lab POST /v1/runs/{SOURCE_RUN}/resume")),
            "{label}: {calls:#?}"
        );
    }
}

/// A run on a model that needs nothing: no mode is read, in any account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_resume_of_a_run_on_a_model_that_needs_nothing_reads_no_mode() {
    let log = Log::default();
    let set = backends_in(&[WORK, LAB], Some("work"), "lab", "inherit", SONNET, &log).await;
    let done = perform_with_env(&Api::new(set), resume(), &env_with("resume-sonnet", true)).await;
    let calls = calls(&log);
    let text = told(&done);
    assert!(text.contains(NEW_RUN), "{text}");
    assert!(!calls.iter().any(|c| c.contains(" bedrock ")), "{calls:#?}");
}

/// The retry of a blocked run takes the same path: with the setting on the account of the run's backend is switched first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_retry_switches_the_account_of_the_runs_backend_when_the_setting_is_on() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let done = perform_with_env(&api, retry(), &env_with("retry-on", true)).await;
    let calls = calls(&log);
    let text = told(&done);
    only_this_account(&calls, LAB, "work", &format!("/v1/runs/{SOURCE_RUN}/retry"));
    assert!(text.contains(NEW_RUN), "the retry started: {text}");
    names_only(&text, LAB, WORK);
}

/// The retry with the setting off: no switch, no start, one plain message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_retry_with_the_setting_off_stops_before_the_run_with_one_plain_message() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let done = perform_with_env(&api, retry(), &env_with("retry-off", false)).await;
    let calls = calls(&log);
    let text = told(&done);
    assert!(
        !calls
            .iter()
            .any(|c| c.contains(" PUT bedrock ") || c.contains(" POST ")),
        "nothing is switched and nothing is started: {calls:#?}"
    );
    assert!(!text.contains(NEW_RUN), "{text}");
    for want in [
        LAB.1,
        LAB.2,
        "ham config set run.provider_sharing on",
        "No run was started",
    ] {
        assert!(text.contains(want), "{want:?} missing: {text}");
    }
}

// ---- ham resume on the command line ---------------------------------------------------------------------------------------------------------

/// What `ham <argv>` prints with the config of `env`: the exit code, standard output and standard error.
async fn ham_cli(
    api: &Api<Backends<Stand>>,
    env: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    argv: &[&str],
) -> (i32, String, String) {
    let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
    let invocation = parse(&argv).unwrap();
    let (cfg, _) = ham::config_cmd::load(env).unwrap();
    let downloads = std::env::temp_dir();
    let out = ham::cli::run_captured(
        &invocation,
        &ham::cli::ExecContext {
            api,
            cfg: &cfg,
            downloads: &downloads,
            chunk: 16 << 20,
            env,
            stdin: None,
        },
    )
    .await;
    (out.code, out.stdout_text(), out.stderr_text())
}

/// `ham resume <run>` with the setting on: the row of the run is read on the backend that holds it, the mode of that account is switched, the
/// line of the switch goes to standard error and the follow-up starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_ham_resume_switches_the_account_of_the_runs_backend_and_says_so_on_standard_error()
{
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let (code, out, err) = ham_cli(
        &api,
        &env_with("cli-resume-on", true),
        &["resume", SOURCE_RUN],
    )
    .await;
    let calls = calls(&log);
    println!("FIX515 CLI: code={code}\nSTDOUT:\n{out}\nSTDERR:\n{err}");
    assert_eq!(code, 0, "{out}\n{err}");
    only_this_account(
        &calls,
        LAB,
        "work",
        &format!("/v1/runs/{SOURCE_RUN}/resume"),
    );
    assert!(
        calls.contains(&format!("lab GET /v1/runs?q={SOURCE_RUN}")),
        "the row is read on the backend that holds the run: {calls:#?}"
    );
    assert!(
        err.contains("ham switched the Bedrock data retention mode"),
        "{err}"
    );
    names_only(&err, LAB, WORK);
    assert!(out.contains(NEW_RUN), "{out}");
}

/// `ham resume --all` reads the rows of the list once and needs no second read for the model of a run; with the setting off the run is not
/// started, its line says why, and the exit code is 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_ham_resume_all_takes_the_model_from_the_list_it_read() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let (code, out, err) = ham_cli(
        &api,
        &env_with("cli-resume-all-on", true),
        &["resume", "--all"],
    )
    .await;
    let calls = calls(&log);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        !calls.iter().any(|c| c.contains("?q=")),
        "the list was read already: {calls:#?}"
    );
    only_this_account(
        &calls,
        LAB,
        "work",
        &format!("/v1/runs/{SOURCE_RUN}/resume"),
    );
    assert!(err.contains("run.provider_sharing is on"), "{err}");

    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await);
    let (code, out, err) = ham_cli(
        &api,
        &env_with("cli-resume-all-off", false),
        &["resume", "--all"],
    )
    .await;
    let made = log.lock().unwrap().clone();
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(
        !made
            .iter()
            .any(|c| c.contains(" PUT bedrock ") || c.contains(" POST ")),
        "{made:#?}"
    );
    assert!(
        out.contains(SOURCE_RUN) && out.contains("No run was started"),
        "{out}"
    );
    assert!(!err.contains("ham switched"), "{err}");
}

// ---- backends that cannot be asked ----------------------------------------------------------------------------------------------------------

/// A backend that answers the read of the mode and has no call that switches it.
struct ReadsOnly(Stand);

impl ControlPlane for ReadsOnly {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.0.call(method, path, headers, body).await
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        self.0.fetch(url, range, if_match).await
    }

    async fn data_retention(&self, target: Option<&str>) -> Answer {
        self.0.data_retention(target).await
    }
}

/// A backend that cannot switch the mode: the start stops before anything is sent and says that the mode was not changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_backend_with_no_way_to_switch_the_mode_stops_the_follow_up() {
    let log = Log::default();
    let api = Api::new(Arc::new(ReadsOnly(Stand {
        alias: "only".into(),
        account: LAB.1.into(),
        region: LAB.2.into(),
        holds_source: true,
        row_model: FABLE.into(),
        mode: Mutex::new("inherit".into()),
        log: Arc::clone(&log),
    })));
    let done = perform_with_env(&api, follow_up(), &env_with("reads-only", true)).await;
    let calls = calls(&log);
    let (Done::FollowUpStarted { result, .. } | Done::FollowUpNoted { result, .. }) = done else {
        panic!("not the answer of a follow-up")
    };
    let why = result.expect_err("the mode cannot be switched");
    assert!(
        why.contains("Bedrock refused the switch")
            && why.contains("this backend cannot change the mode"),
        "{why}"
    );
    assert!(why.ends_with("No run was started."), "{why}");
    assert!(!calls.iter().any(|c| c.contains(" POST ")), "{calls:#?}");
}

/// A backend the config does not name: the read, the switch and the start are refused by the client with the names of the backends there are,
/// and no backend is called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_backend_that_the_config_does_not_name_is_refused_before_any_call() {
    let log = Log::default();
    let set = backends(&[WORK, LAB], Some("work"), "lab", &log).await;
    for answer in [
        set.data_retention(Some("nope")).await,
        set.put_data_retention(Some("nope"), "provider_data_share")
            .await,
    ] {
        let Some(Err(e)) = answer else {
            panic!("a backend that is not there is refused")
        };
        let why = e.to_string();
        assert!(
            why.contains("nope") && why.contains("work") && why.contains("lab"),
            "{why}"
        );
    }
    let api = Api::new(set).on("nope");
    let env = env_with("unknown-backend", true);
    let msg =
        submit_msg_with_env(&api, &args(&[]), "fix515-nonce", Some("nope".into()), &env).await;
    let Msg::Started(Err(why)) = msg else {
        panic!("a start on a backend that is not there is refused: {msg:?}")
    };
    assert!(why.contains("nope"), "{why}");
    assert_eq!(calls(&log), Vec::<String>::new(), "no backend was called");
}

// ---- starts that need no look at an account, and a run that is nowhere ----------------------------------------------------------------------

/// A resume of a run that no backend holds: ham cannot tell which account it would be, so it looks at none, and the call itself says that
/// the run is not there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_resume_of_a_run_that_no_backend_holds_looks_at_no_account() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "neither", &log).await);
    let done = perform_with_env(&api, resume(), &env_with("resume-nowhere", true)).await;
    let calls = calls(&log);
    let text = told(&done);
    assert!(!calls.iter().any(|c| c.contains(" bedrock ")), "{calls:#?}");
    assert!(!calls.iter().any(|c| c.contains(" POST ")), "{calls:#?}");
    assert!(text.contains("Err(") && !text.contains(NEW_RUN), "{text}");
}

/// A follow-up on a model that needs nothing, from a client aimed at the backend that holds the run: no account is looked at, and the run
/// starts on that backend as it always did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_an_aimed_follow_up_on_a_model_that_needs_nothing_looks_at_no_account() {
    let log = Log::default();
    let api = Api::new(backends(&[WORK, LAB], Some("work"), "lab", &log).await).on("lab");
    let done = perform_with_env(&api, follow_up(), &env_for("followup-sonnet", SONNET, true)).await;
    let calls = calls(&log);
    let Done::FollowUpStarted {
        result: Ok(new_run),
        ..
    } = done
    else {
        panic!("the follow-up starts with no line about an account: {done:?}")
    };
    assert_eq!(new_run, NEW_RUN);
    assert!(!calls.iter().any(|c| c.contains(" bedrock ")), "{calls:#?}");
    assert!(
        calls.contains(&"lab POST /v1/direct/runs".to_string()),
        "{calls:#?}"
    );
    assert!(
        !calls.iter().any(|c| c.starts_with("work ")),
        "the client is aimed at lab: {calls:#?}"
    );
}
