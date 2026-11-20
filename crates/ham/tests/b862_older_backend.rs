//! A newer client on an older stage says what is wrong.
//!
//! The model list is compiled into each binary, so a client can list a model that the stage it talks to does not list yet. The stage then
//! refuses the start with 422 `unknown-model` and its own list of ids, which tells a person nothing about why a model `ham models` shows is
//! refused. `ham` adds one sentence: the version of the backend that refused (from the `x-hamh-backend-version` header of that answer), the
//! model by name, and the two ways out. Nothing is substituted and the request is not sent again. On the command line the sentence is a
//! line under the service's message; in the New run screen, whose message area holds two or three lines, it comes before the service's words.
//!
//! The older stage here is the local `/v1` API with one switch: it answers a start on one model as a stage whose list lacks that model, in
//! the words the service uses. The version header is the stand-in's own switch. The real binary and the real transport read both.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, ApiError, Retry, older_backend_line};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, perform};
use ham_core::command::{Command, parse};
use ham_core::config::parse_config;
use ham_core::models::{ModelPrefs, builtin_catalog};
use ham_core::request::{DirectInputs, resolve_run_settings};
use harness_protocol::api::ApiError as WireError;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::Engine;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::{AKID, BackendHeader, Fake, REGION, SECRET, TOKEN, creds};

/// A model this client lists as selectable, by its full id, so that the test does not depend on what a short name means.
const MODEL: &str = "us.openai.gpt-6.1-sol";
const NAME: &str = "GPT-6.1 Sol";
const PROMPT: &str = "Add a percent_change(a, b) function with tests";
/// A valid version that is older than every client this test is built with.
const OLDER: &str = "4.01";
const WAYS_OUT: &str = "Update the stage (run ham update), or pick a model it lists (ham models --server shows the stage's list).";

fn named_line(version: &str) -> String {
    format!("This stage runs backend v{version}, which does not list {NAME}. {WAYS_OUT}")
}

fn unnamed_line() -> String {
    format!("This stage runs an older backend, which does not list {NAME}. {WAYS_OUT}")
}

/// The local stage as a stage whose list lacks [`MODEL`], answering with this version header.
async fn older_stage(header: BackendHeader) -> Arc<Fake> {
    let fake = Fake::start_with(true).await;
    *fake.faults.lacks_model.lock().unwrap() = Some(MODEL.to_string());
    *fake.faults.backend_header.lock().unwrap() = header;
    fake
}

/// A temp HOME with `~/.config/ham/config.toml` naming the fake, and a `credential_process` profile that prints the test stand-ins.
fn home(fake: &Fake, label: &str) -> PathBuf {
    let root = support::scratch(label);
    std::fs::create_dir_all(root.join(".config/ham")).unwrap();
    std::fs::write(
        root.join(".config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint, REGION
        ),
    )
    .unwrap();
    let script = root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            REGION
        ),
    )
    .unwrap();
    std::fs::write(root.join("aws-credentials"), "").unwrap();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    root
}

async fn ham(root: &PathBuf, args: &[&str]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", root)
        .env("HOME", root)
        .env("HAMH_DOWNLOAD_DIR", root.join("downloads"))
        .env("AWS_CONFIG_FILE", root.join("aws-config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", root.join("aws-credentials"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn start_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        PROMPT,
        "--s3-only",
        "--direct",
        "--model",
        MODEL,
        "--no-rules",
    ];
    a.extend(extra);
    a
}

/// TRIGGER PRESENT: the client sent one start that named the model, and the stage refused it as a model it does not list.
fn assert_trigger(fake: &Fake, said: &str) {
    let posts = fake.mem.posts("/v1/direct/runs");
    assert_eq!(posts.len(), 1, "one request and no second one: {said}");
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(
        body["model"], MODEL,
        "TRIGGER PRESENT: the request named the model"
    );
    assert!(
        said.contains("HTTP 422") && said.contains("(unknown-model)"),
        "TRIGGER PRESENT: the stage refused the model in the service's words: {said}"
    );
}

// ---- the command line -------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_an_older_backend_that_lacks_the_model_is_named_with_its_version() {
    let fake = older_stage(BackendHeader::Value(OLDER.into())).await;
    let root = home(&fake, "b862-older-named");
    let o = ham(&root, &start_args(&[])).await;
    let said = err(&o);
    assert_ne!(o.status.code(), Some(0), "{}\n{said}", out(&o));
    assert_trigger(&fake, &said);
    assert!(
        !out(&o).contains("started run"),
        "nothing started: {}",
        out(&o)
    );
    let line = named_line(OLDER);
    let at = said
        .find(&line)
        .unwrap_or_else(|| panic!("the line that names the backend is missing:\n{said}"));
    let service = said.find("(unknown-model)").unwrap();
    assert!(
        service < at,
        "the line stands under the service's message:\n{said}"
    );
    assert_eq!(
        said.matches("This stage runs").count(),
        1,
        "said once:\n{said}"
    );
    // The service's own list is still shown: nothing of its message is dropped.
    assert!(
        said.contains("is not one of the supported models"),
        "{said}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_a_backend_with_no_version_header_is_called_an_older_backend() {
    let fake = older_stage(BackendHeader::Absent).await;
    let root = home(&fake, "b862-older-unnamed");
    let o = ham(&root, &start_args(&[])).await;
    let said = err(&o);
    assert_ne!(o.status.code(), Some(0), "{said}");
    assert_trigger(&fake, &said);
    assert!(said.contains(&unnamed_line()), "{said}");
    assert!(
        !said.contains("runs backend v"),
        "no version is made up: {said}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_a_backend_of_the_clients_own_version_gets_the_services_words_alone() {
    // A stage of this client's version has this client's list. If it still refuses the model, updating the stage is not the answer, so
    // `ham` does not say it.
    let fake = older_stage(BackendHeader::Same).await;
    let root = home(&fake, "b862-older-same");
    let o = ham(&root, &start_args(&[])).await;
    let said = err(&o);
    assert_ne!(o.status.code(), Some(0), "{said}");
    assert_trigger(&fake, &said);
    assert!(!said.contains("This stage runs"), "{said}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_with_json_the_line_goes_to_standard_error_and_standard_output_stays_empty() {
    let fake = older_stage(BackendHeader::Value(OLDER.into())).await;
    let root = home(&fake, "b862-older-json");
    let o = ham(&root, &start_args(&["--json"])).await;
    let said = err(&o);
    assert_ne!(o.status.code(), Some(0), "{said}");
    assert_trigger(&fake, &said);
    assert!(said.contains(&named_line(OLDER)), "{said}");
    assert_eq!(
        out(&o),
        "",
        "a start that failed prints no JSON line, and the sentence is not on standard output"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_another_refusal_of_a_start_gets_no_line_about_the_backend() {
    // An older stage that refuses the copy interval names that key; the sentence about the model list has no place there.
    let fake = Fake::start_with(true).await;
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(OLDER.into());
    fake.faults
        .predates_copy_interval
        .store(1, Ordering::SeqCst);
    let root = home(&fake, "b862-older-other-refusal");
    let config = root.join(".config/ham/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        format!("configVersion = 1\n{text}\n[run]\ncopy_every_seconds = 120\n"),
    )
    .unwrap();
    let o = ham(&root, &start_args(&[])).await;
    let said = err(&o);
    assert_ne!(o.status.code(), Some(0), "{said}");
    assert!(
        said.contains("this stage predates the copy interval of a run"),
        "TRIGGER PRESENT: the other refusal: {said}"
    );
    assert!(!said.contains("This stage runs"), "{said}");
}

// ---- the New run screen -----------------------------------------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_the_new_run_screen_names_the_backend_before_the_services_words() {
    let fake = older_stage(BackendHeader::Value(OLDER.into())).await;
    let a = api(&fake);
    let dir = support::scratch("b862-older-composer");
    // The composer opens on the user's default, the model the stage lacks.
    let mut app = App::new_direct(
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            prefs: ModelPrefs {
                env: None,
                config: Some(MODEL.into()),
            },
            rules: None,
            no_rules: true,
        },
    );
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.chosen_model(), Some(MODEL));
    for ch in PROMPT.chars() {
        press(&mut app, KeyCode::Char(ch));
    }
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(effects.len(), 1, "{effects:?}");
    let msg = perform(&a, &dir, effects.into_iter().next().unwrap())
        .await
        .unwrap();
    let Msg::Started(Err(text)) = &msg else {
        panic!("the start was not refused: {msg:?}")
    };
    assert_trigger(&fake, text);
    assert!(
        text.starts_with(&named_line(OLDER)),
        "the sentence comes first, where the screen's two lines show it: {text}"
    );
    assert!(
        text.contains("is not one of the supported models"),
        "the service's words follow: {text}"
    );
    app.update(msg);
    assert_eq!(
        app.screen(),
        Screen::Composer,
        "the form stays, with the pick"
    );
    assert_eq!(app.chosen_model(), Some(MODEL));
    // At 80x24 the message area holds two lines: the backend, the model and the first way out are in them.
    let flat = |w, h| {
        screen_text(&app, w, h)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let small = flat(80, 24);
    assert!(
        small.contains(&format!(
            "This stage runs backend v{OLDER}, which does not list {NAME}. Update the stage (run ham update)"
        )),
        "{small}"
    );
    // A taller screen has three lines: the whole sentence.
    let large = flat(120, 40);
    assert!(large.contains(&named_line(OLDER)), "{large}");
}

// ---- the rule ---------------------------------------------------------------------------------------------------------------------

/// The request `ham` builds for a start on [`MODEL`], through the one function that resolves a run.
fn request() -> StartDirectRunRequest {
    let Command::StartDirect(args) =
        parse(&["Add a function", "--s3-only", "--direct", "--model", MODEL])
            .unwrap()
            .command
    else {
        panic!("not a start");
    };
    let cfg = parse_config("").unwrap();
    let run = resolve_run_settings(&cfg, &|_| None, &args, false).unwrap();
    run.request(DirectInputs::default()).unwrap()
}

fn refusal(status: u16, code: &str) -> ApiError {
    ApiError::Http {
        status,
        error: WireError {
            code: code.into(),
            message: "model \"x\" is not one of the supported models: a, b".into(),
            request_id: None,
        },
    }
}

#[test]
fn b862_the_sentence_is_made_only_for_an_older_backend_that_refuses_a_model_this_client_lists() {
    let req = request();
    assert_eq!(req.model, MODEL);
    let unknown = refusal(422, "unknown-model");
    // An older backend is named with its version; versions are compared as numbers.
    assert_eq!(
        older_backend_line(&unknown, &req, "5.24", Some("5.23")),
        Some(named_line("5.23"))
    );
    assert_eq!(
        older_backend_line(&unknown, &req, "10.01", Some("9.99")),
        Some(named_line("9.99"))
    );
    // The other code of a model the stage does not offer.
    assert_eq!(
        older_backend_line(
            &refusal(422, "model-not-smoked"),
            &req,
            "5.24",
            Some("5.23")
        ),
        Some(named_line("5.23"))
    );
    // No version header: a backend that predates the header.
    assert_eq!(
        older_backend_line(&unknown, &req, "5.24", None),
        Some(unnamed_line())
    );
    // The same version, a newer one, and text that is no version name no older backend.
    for backend in [
        "5.24", "5.25", "6.00", "mixed", "v5.23", "", "5.2", "5.230", "5.23\n",
    ] {
        assert_eq!(
            older_backend_line(&unknown, &req, "5.24", Some(backend)),
            None,
            "backend {backend:?}"
        );
    }
    // Another refusal, another status and a failure that is no answer of the service.
    for other in [
        refusal(422, "reasoning-level-not-accepted"),
        refusal(400, "unknown-model"),
        refusal(409, "unknown-model"),
        refusal(400, "bad-request"),
        ApiError::Decode {
            what: "StartRunResponse".into(),
            msg: "eof".into(),
        },
    ] {
        assert_eq!(
            older_backend_line(&other, &req, "5.24", Some("5.23")),
            None,
            "{other}"
        );
        assert_eq!(
            older_backend_line(&other, &req, "5.24", None),
            None,
            "{other}"
        );
    }
}

#[test]
fn b862_a_model_this_client_does_not_offer_gets_the_services_words_alone() {
    let unknown = refusal(422, "unknown-model");
    // An id this client's list does not hold.
    let mut req = request();
    req.model = "us.openai.gpt-9-zeta".into();
    assert!(
        builtin_catalog()
            .check(Engine::DEFAULT, &req.model)
            .is_err()
    );
    assert_eq!(
        older_backend_line(&unknown, &req, "5.24", Some("5.23")),
        None
    );
    assert_eq!(older_backend_line(&unknown, &req, "5.24", None), None);
    // The pair counts, not the id: a model this client lists for the engine of the request gets the sentence, and a model it lists for
    // another engine only gets none.
    let mut req = request();
    req.engine = Some(Engine::OPENCODE);
    assert!(
        builtin_catalog().check(Engine::OPENCODE, MODEL).is_ok(),
        "this client lists the model for the second engine"
    );
    assert_eq!(
        older_backend_line(&unknown, &req, "5.24", Some("5.23")),
        Some(named_line("5.23"))
    );
    let cline_only = builtin_catalog()
        .default_list()
        .models()
        .iter()
        .find(|m| m.selectable() && builtin_catalog().check(Engine::OPENCODE, &m.id).is_err())
        .expect("a model this client offers on the default engine only");
    req.model = cline_only.id.clone();
    assert_eq!(
        older_backend_line(&unknown, &req, "5.24", Some("5.23")),
        None
    );
}
