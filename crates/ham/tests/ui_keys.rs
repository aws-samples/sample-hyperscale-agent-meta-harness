//! The `ui.notify` and `ui.bell` keys of `config.toml`. `HAMH_NOTIFY` keeps its exact parse (`1`, `true` or `on`, lower case) and wins over
//! the file whenever it is set; with the variable unset the file decides. The bell was always on (`Alerts::set_bell` had only a test as
//! caller); `ui.bell = false` silences it for a question on the run screen and for a run that starts to wait in the Runs list. The
//! terminal notification does not depend on the bell.
#![allow(clippy::unwrap_used)]

use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::config::{Config, UiTable, bell_on, notify_on, parse_config};
use harness_protocol::api::v1::{EventsPage, RunSummary};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const START: &str = "2026-10-14T20:00:00Z";

fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k: &str| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

fn ui(notify: Option<bool>, bell: Option<bool>) -> Config {
    Config {
        config_version: Some(1),
        ui: Some(UiTable {
            notify,
            bell,
            mascot: None,
            mascot_motion: None,
            deprecation_notices: None,
        }),
        ..Config::default()
    }
}

#[test]
fn notify_follows_env_over_file_and_keeps_the_exact_parse() {
    // (variable, file, answer): a set variable wins, and only 1, true and on (lower case) mean yes.
    let table: [(Option<&'static str>, Option<bool>, bool); 12] = [
        (None, None, false),
        (None, Some(true), true),
        (None, Some(false), false),
        (Some("1"), Some(false), true),
        (Some("true"), Some(false), true),
        (Some("on"), Some(false), true),
        (Some("0"), Some(true), false),
        (Some("off"), Some(true), false),
        (Some("ON"), None, false),
        (Some("yes"), None, false),
        (Some(""), Some(true), true),
        (Some("   "), Some(true), true),
    ];
    for (var, file, want) in table {
        let pairs: Vec<(&'static str, &'static str)> =
            var.map(|v| ("HAMH_NOTIFY", v)).into_iter().collect();
        let cfg = ui(file, None);
        assert_eq!(
            notify_on(&env_of(&pairs), &cfg),
            want,
            "HAMH_NOTIFY={var:?} notify={file:?}"
        );
    }
    // The key reads from a real file.
    let cfg = parse_config("configVersion = 1\n\n[ui]\nnotify = true\n").unwrap();
    assert!(notify_on(&env_of(&[]), &cfg));
    assert!(!notify_on(&env_of(&[("HAMH_NOTIFY", "0")]), &cfg));
}

#[test]
fn the_bell_is_on_unless_the_file_turns_it_off() {
    assert!(bell_on(&Config::default()));
    assert!(bell_on(&ui(Some(true), None)));
    assert!(bell_on(&ui(None, Some(true))));
    assert!(!bell_on(&ui(None, Some(false))));
}

fn env_event(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: START.into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

fn pages(events: Vec<Envelope>) -> Msg {
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect()))
}

fn asking() -> Vec<Envelope> {
    vec![
        env_event(
            1,
            EventType::RunCreated,
            None,
            json!({"target": {"mode": "s3-only"}}),
        ),
        env_event(
            2,
            EventType::SessionStarted,
            None,
            json!({"agentId": "direct", "engine": "cline", "modelId": ham_core::models::DEFAULT_MODEL_ID}),
        ),
        env_event(
            3,
            EventType::GoalStatus,
            None,
            json!({"status": "active", "phase": "green"}),
        ),
        env_event(
            40,
            EventType::TurnProgress,
            Some("direct"),
            json!({
                "turn": 1, "kind": "needs-input",
                "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
                "requestId": REQ, "n": 1, "reason": "engine_stop", "title": "Cline stopped its own prompt",
                "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": "x", "runS": 412, "sessions": 1},
                "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
                "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
                "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-14T20:30:00Z"
            }),
        ),
    ]
}

fn waiting_run(id: &str, waiting: bool) -> RunSummary {
    let mut v = json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": START, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"});
    if waiting {
        v["needsInput"] = json!({"requestId": REQ, "reason": "engine_stop", "title": "Cline stopped",
                                 "expiresAt": "2026-10-14T20:30:00Z"});
    }
    serde_json::from_value(v).unwrap()
}

fn rings(effects: &[Effect]) -> usize {
    effects.iter().filter(|e| **e == Effect::Bell).count()
}

fn notifications(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Notify(_)))
        .count()
}

/// The app as `cli::tui_app` sets it up from the file and the environment.
fn app_for(cfg: &Config, env: &[(&'static str, &'static str)]) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    ham::cli::apply_ui_settings(&mut app, &env_of(env), cfg);
    app
}

#[test]
fn ui_bell_false_turns_the_bell_off_and_notify_follows_env_over_file() {
    // The default rings once for a question that first appears (TRIGGER PRESENT: the question is there).
    let mut on = app_for(&Config::default(), &[]);
    on.open_run(RUN);
    let effects = on.update(pages(asking()));
    assert_eq!(rings(&effects), 1, "the default rings: {effects:?}");
    println!(
        "UI-KEYS TRIGGER PRESENT: a question opened and the default bell rang {} time",
        rings(&effects)
    );

    // ui.bell = false: no bell for the question, none for the Runs list, and no other effect changes.
    let cfg = ui(None, Some(false));
    let mut off = app_for(&cfg, &[]);
    off.open_run(RUN);
    let effects = off.update(pages(asking()));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    let mut list = app_for(&cfg, &[]);
    list.update(Msg::Runs(Ok(vec![waiting_run(
        "01K6A0000000000000000000A1",
        false,
    )])));
    let effects = list.update(Msg::Runs(Ok(vec![waiting_run(
        "01K6A0000000000000000000A1",
        true,
    )])));
    assert_eq!(rings(&effects), 0, "{effects:?}");
    // The same list with the bell on rings once.
    let mut list = app_for(&Config::default(), &[]);
    list.update(Msg::Runs(Ok(vec![waiting_run(
        "01K6A0000000000000000000A1",
        false,
    )])));
    let effects = list.update(Msg::Runs(Ok(vec![waiting_run(
        "01K6A0000000000000000000A1",
        true,
    )])));
    assert_eq!(rings(&effects), 1, "{effects:?}");

    // The terminal notification: the file turns it on, HAMH_NOTIFY wins when set, and the bell is not needed for it.
    for (env, file, want) in [
        (vec![], Some(true), 1),
        (vec![], Some(false), 0),
        (vec![("HAMH_NOTIFY", "1")], Some(false), 1),
        (vec![("HAMH_NOTIFY", "0")], Some(true), 0),
        (vec![], None, 0),
    ] {
        let cfg = ui(file, Some(false));
        let mut app = app_for(&cfg, &env);
        app.set_focus(false);
        app.open_run(RUN);
        let effects = app.update(pages(asking()));
        assert_eq!(
            notifications(&effects),
            want,
            "env {env:?} notify {file:?}: {effects:?}"
        );
        assert_eq!(rings(&effects), 0, "the bell stays off: {effects:?}");
    }
}
