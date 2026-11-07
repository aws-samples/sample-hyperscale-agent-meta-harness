//! The follow-up reads the engine file afresh and says when it changed. `ham continue` and
//! the TUI follow-up read `engines/<engine>.toml` when they start; they do not copy the source run's settings. When the first Enter of a follow-up
//! prepares the draft, the app asks for the settings hash of the file as it is now (`Job::EngineSettings`, no network) and, if it differs from the
//! hash the source run's card shows, the confirmation says "Engine settings changed since run <id>". The follow-up that starts sends the file as it
//! is now (`run_paths.rs` pins the member).
#![allow(clippy::unwrap_used)]

mod chatfx;
mod support;

use chatfx::*;
use ham::api::Api;
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham::tui::{App, Effect, Msg};
use ham_core::engine_file;
use harness_protocol::envelope::Envelope;
use serde_json::json;
use std::sync::Arc;

const FILE_A: &str = "schema = 1\n[reasoning]\neffort = \"low\"\n";
const FILE_B: &str = "schema = 1\n[reasoning]\neffort = \"high\"\n";

fn hash_of(text: &str) -> String {
    engine_file::read_text(text, "cline")
        .config
        .unwrap()
        .canonical_sha256()
}

/// The `engine-config` event of a source run that was given `text`.
fn placed(text: &str) -> Envelope {
    let cfg = engine_file::read_text(text, "cline").config.unwrap();
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": "01K6A000000000000000000099", "runId": RUN, "agentId": "direct", "seq": 99,
        "ts": "2026-10-21T08:15:00Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": {
            "turn": 1, "kind": "engine-config", "engine": "cline", "sha256": cfg.canonical_sha256(), "keys": cfg.key_count(),
            "settings": [{"key": "reasoning.effort", "value": cfg.get("reasoning.effort").unwrap().as_str().unwrap()}],
            "readBack": "ok"}}
    }))
    .unwrap()
}

fn capability() -> serde_json::Value {
    let path = format!(
        "{}/../ham-core/tests/fixtures/capability/first-engine.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A finished source run (with the engine settings of `source`, when it had any), the follow-up prepared with the first Enter. Returns the app and
/// the effects of that Enter.
fn prepared(source: Option<&str>) -> (App, Clock, Vec<Effect>) {
    let mut events = finished_events();
    events.push(capability_event(6, capability()));
    if let Some(text) = source {
        events.push(placed(text));
    }
    let mut app = ready(events);
    app.tick();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "now add the docs");
    (app, clock, effects)
}

fn answer(app: &mut App, hash: Option<String>) {
    app.update(Msg::Ext(Done::EngineSettings { result: Ok(hash) }));
}

fn flat(app: &App) -> String {
    screen(app, 160, 50)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_first_enter_asks_for_the_hash_of_the_file_as_it_is_now() {
    let (_app, _clock, effects) = prepared(Some(FILE_A));
    let asks = jobs(&effects)
        .into_iter()
        .filter(|j| matches!(j, Job::EngineSettings))
        .count();
    assert_eq!(asks, 1, "{effects:?}");
}

#[test]
fn the_confirmation_says_engine_settings_changed_when_the_hash_differs_from_the_source_runs() {
    let (mut app, _clock, _) = prepared(Some(FILE_A));
    assert!(
        !flat(&app).contains("Engine settings changed"),
        "{}",
        flat(&app)
    );
    answer(&mut app, Some(hash_of(FILE_B)));
    let text = flat(&app);
    assert!(
        text.contains(&format!("Engine settings changed since run {RUN}")),
        "{text}"
    );
    println!(
        "FOLLOWUP-ENGINE TRIGGER PRESENT: the source run had {}, the file now has {}",
        &hash_of(FILE_A)[..12],
        &hash_of(FILE_B)[..12]
    );
}

#[test]
fn the_same_settings_and_the_same_absence_say_nothing() {
    // The same file as the source run's.
    let (mut app, _clock, _) = prepared(Some(FILE_A));
    answer(&mut app, Some(hash_of(FILE_A)));
    assert!(
        !flat(&app).contains("Engine settings changed"),
        "{}",
        flat(&app)
    );
    // Neither the source run nor the file has settings.
    let (mut app, _clock, _) = prepared(None);
    answer(&mut app, None);
    assert!(
        !flat(&app).contains("Engine settings changed"),
        "{}",
        flat(&app)
    );
}

#[test]
fn a_file_that_now_sets_nothing_or_now_sets_something_is_a_change_too() {
    let (mut app, _clock, _) = prepared(Some(FILE_A));
    answer(&mut app, None);
    assert!(
        flat(&app).contains("the file now sets none"),
        "{}",
        flat(&app)
    );
    let (mut app, _clock, _) = prepared(None);
    answer(&mut app, Some(hash_of(FILE_A)));
    let text = flat(&app);
    assert!(text.contains("Engine settings changed since run"), "{text}");
    assert!(text.contains(&hash_of(FILE_A)[..12]), "{text}");
}

#[test]
fn an_unreadable_answer_adds_nothing_and_the_start_will_say_why() {
    let (mut app, _clock, _) = prepared(Some(FILE_A));
    app.update(Msg::Ext(Done::EngineSettings {
        result: Err("engines/cline.toml line 2: ...".into()),
    }));
    assert!(
        !flat(&app).contains("Engine settings changed"),
        "{}",
        flat(&app)
    );
}

// ---- the job reads the file ------------------------------------------------------------------------------------------------------------

struct NoNetwork;

impl ControlPlane for NoNetwork {
    async fn call(
        &self,
        _m: &str,
        _p: &str,
        _h: &[(String, String)],
        _b: Vec<u8>,
    ) -> Result<Response, TransportError> {
        panic!("the engine settings job makes no call");
    }
    async fn fetch(
        &self,
        _u: &str,
        _r: Option<(u64, u64)>,
        _i: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("not used".into()))
    }
}

fn env_with(label: &str, engine_file: Option<&str>) -> impl Fn(&str) -> Option<String> {
    let root = support::scratch(label);
    let ham = root.join("config/ham");
    std::fs::create_dir_all(&ham).unwrap();
    if let Some(f) = engine_file {
        std::fs::create_dir_all(ham.join("engines")).unwrap();
        std::fs::write(ham.join("engines/cline.toml"), f).unwrap();
    }
    let config = root.join("config");
    move |k: &str| (k == "XDG_CONFIG_HOME").then(|| config.display().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_job_answers_with_the_settings_hash_of_the_file_and_none_without_one() {
    let api = Api::new(Arc::new(NoNetwork));
    let env = env_with("fu-eng-a", Some(FILE_A));
    let Done::EngineSettings { result } = perform_with_env(&api, Job::EngineSettings, &env).await
    else {
        panic!("not the answer of the job");
    };
    assert_eq!(result, Ok(Some(hash_of(FILE_A))));
    let env = env_with("fu-eng-none", None);
    let Done::EngineSettings { result } = perform_with_env(&api, Job::EngineSettings, &env).await
    else {
        panic!("not the answer of the job");
    };
    assert_eq!(result, Ok(None));
    let env = env_with(
        "fu-eng-denied",
        Some("schema = 1\n[aws]\nprofile = \"x\"\n"),
    );
    let Done::EngineSettings { result } = perform_with_env(&api, Job::EngineSettings, &env).await
    else {
        panic!("not the answer of the job");
    };
    assert!(result.unwrap_err().contains("aws.profile"));
}
