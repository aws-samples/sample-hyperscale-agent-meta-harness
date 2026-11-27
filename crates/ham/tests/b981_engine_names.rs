//! The texts of the run screen that named the default engine where a run on another engine is meant. The follow-up box's marker
//! ("written by <engine>, untrusted") with its reader, the confirmations of the follow-up and of the retry ("The new <engine> receives"),
//! and a run this screen started itself, which is named by the engine it was started on before its session's first event. A run on Cline
//! reads the words of release 5.34 in each of them.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::ext::{Done, Job};
use ham::tui::keys::Mode;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use ham_core::head::{Changes, DeliveredHead};
use harness_protocol::api::v1::StartRunResponse;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::GoalStatus;
use serde_json::{Value, json};

/// The engines a person can choose: the id a session names, and the name a person reads.
const ENGINES: [(&str, &str); 2] = [("cline", "Cline"), ("opencode", "OpenCode")];
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
/// A second run: the one a start, a follow-up or a retry of these tests makes.
const NEW: &str = "01K6A0000000000000000000R2";
/// A run the screen neither shows nor started.
const OTHER: &str = "01K6A0000000000000000000R9";
const MESSAGE: &str = "hello.py printed 55. Everything is done.";

/// `events` as a run on `engine` has them: its session names the engine, and the label of its end is written with the engine's name.
fn on(engine: &str, mut events: Vec<Envelope>) -> Vec<Envelope> {
    let name = ham_core::engines::find(engine).unwrap().name;
    for e in &mut events {
        let Payload::Inline { value } = &mut e.payload else {
            continue;
        };
        if e.event_type == EventType::SessionStarted {
            value["engine"] = json!(engine);
        }
        let label = value
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(label) = label {
            value["label"] = json!(label.replace("Cline", name));
        }
    }
    events
}

fn capability() -> Value {
    let path = format!(
        "{}/../ham-core/tests/fixtures/capability/first-engine.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn flat(s: &str) -> String {
    s.chars()
        .map(|c| {
            if ('\u{2500}'..='\u{257f}').contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn ctrl_l(app: &mut App, clock: &mut Clock, wait_ms: u64) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL), at)
}

/// A finished run on `engine` whose delivered head has arrived with the engine's last message.
fn finished_with_message(engine: &str) -> (App, Clock) {
    let mut events = on(engine, finished_events());
    events.push(capability_event(6, capability()));
    let mut app = ready(events);
    let asked = app.tick();
    assert!(
        jobs(&asked)
            .iter()
            .any(|j| matches!(j, Job::Head { run_id } if run_id == RUN)),
        "{engine}: {asked:?}"
    );
    app.update(Msg::Ext(Done::Head {
        run_id: RUN.into(),
        result: Ok(DeliveredHead {
            run_id: RUN.into(),
            head_sha: HEAD.into(),
            run_status: GoalStatus::Complete,
            cline_message: Some(MESSAGE.into()),
            changes: Changes::Unavailable,
        }),
    }));
    (app, Clock::new())
}

fn harness_event(seq: u64, event_type: EventType, value: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq,
        ts: T0.into(),
        event_type,
        authority: Authority::Harness,
        payload: Payload::Inline { value },
    }
}

/// A run on `engine` that ended blocked, on screen.
fn blocked_on(engine: &str) -> App {
    let mut events = base_events();
    events.extend([
        harness_event(
            20,
            EventType::GoalBlocked,
            json!({"reason": "the run machine failed"}),
        ),
        harness_event(
            21,
            EventType::GoalStatus,
            json!({"status": "blocked", "phase": "delivering"}),
        ),
        harness_event(
            22,
            EventType::RunCompleted,
            json!({"status": "blocked", "outcome": "delivery_failed", "label": "Not delivered"}),
        ),
    ]);
    ready(on(engine, events))
}

fn key_press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// The New run screen with a prompt typed. `file_engine` is `default_engine` of the config file as the process read it.
fn composer_on(file_engine: Option<&str>) -> App {
    let mut app = App::new_direct(
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            no_rules: true,
            ..DirectSettings::default()
        },
    );
    app.set_default_engine(file_engine);
    key_press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    for ch in "Add a percent_change function".chars() {
        key_press(&mut app, KeyCode::Char(ch));
    }
    app
}

/// Chooses OpenCode in the Engine box of the New run screen.
fn choose_opencode(app: &mut App) {
    key_press(app, KeyCode::Tab);
    key_press(app, KeyCode::Down);
}

/// Submits the New run screen and answers the start with run `run_id`, which opens with no event of its session. Returns the engine
/// the start carried.
fn started(app: &mut App, run_id: &str) -> Option<String> {
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let engine = match effects.as_slice() {
        [Effect::SubmitDirect { args, .. }] => args.engine.clone(),
        other => panic!("{other:?}"),
    };
    app.update(Msg::Started(Ok(StartRunResponse {
        run_id: run_id.into(),
        status: GoalStatus::Active,
    })));
    assert_eq!(app.focused_run(), Some(run_id));
    engine
}

/// The reply box and the preview of the cancel key of the run on screen name `name`.
fn assert_named(app: &mut App, name: &str, why: &str) {
    let mut clock = Clock::new();
    assert_eq!(app.mode(), Mode::Browse, "{why}");
    let text = flat(&screen(app, 120, 30));
    assert!(
        text.contains(&format!("Message to {name}")),
        "{why}: {text}"
    );
    let effects = press(app, &mut clock, KeyCode::Char('c'), 400);
    assert!(effects.is_empty(), "{why}: {effects:?}");
    let text = flat(&screen(app, 120, 30));
    assert!(
        text.contains(&format!(
            "Cancel run {RUN}: {name} is asked to stop and the work so far is kept."
        )),
        "{why}: {text}"
    );
}

// ---- the follow-up box (text 1) -------------------------------------------------------------------------------------------------------

/// The block that Ctrl-L adds is marked with the engine that wrote the message, and the reader of the marker finds it again: a second
/// press adds no second block. A run on Cline reads the words of release 5.34.
#[test]
fn b981_the_follow_up_box_marks_the_message_with_the_engine_that_wrote_it_and_reads_its_own_marker()
{
    for (engine, name) in ENGINES {
        let (mut app, mut clock) = finished_with_message(engine);
        send(&mut app, &mut clock, "now add the docs");
        ctrl_l(&mut app, &mut clock, 100);
        let once = app.chat_text().unwrap();
        let open = once
            .find(&format!("--- written by {name}, untrusted ["))
            .unwrap_or_else(|| panic!("{engine}: {once}"));
        let close = once
            .find(&format!("--- end of text written by {name} ["))
            .unwrap_or_else(|| panic!("{engine}: {once}"));
        assert!(
            open < close && once[open..close].contains(MESSAGE),
            "{engine}: {once}"
        );
        if engine != "cline" {
            assert!(!once.contains("written by Cline"), "{engine}: {once}");
        }
        // The reader: the second press finds this draft's marker and changes nothing.
        ctrl_l(&mut app, &mut clock, 100);
        assert_eq!(app.chat_text().unwrap(), once, "{engine}: no second block");
        let text = flat(&screen(&app, 140, 40));
        assert!(
            text.contains(&format!("{name}'s last message is already in the draft")),
            "{engine}: {text}"
        );
    }
}

// ---- the confirmations (text 2) -------------------------------------------------------------------------------------------------------

/// The confirmation of the follow-up names the engine the new run starts on, which is the engine the second Enter puts into the start.
#[test]
fn b981_the_follow_up_confirmation_names_the_engine_the_new_run_starts_on() {
    for (engine, name) in ENGINES {
        let (mut app, mut clock) = finished_with_message(engine);
        send(&mut app, &mut clock, "now add the docs");
        let text = flat(&screen(&app, 160, 50));
        assert!(
            text.contains(&format!(
                "The new {name} receives exactly the text in the box: edit it first if you like."
            )),
            "{engine}: {text}"
        );
        if engine != "cline" {
            assert!(!text.contains("The new Cline"), "{engine}: {text}");
        }
        let second = enter(&mut app, &mut clock, 400);
        let found = app.update(Msg::Ext(Done::FollowUpFound {
            run_id: RUN.into(),
            result: Ok(None),
        }));
        let all: Vec<Effect> = second.into_iter().chain(found).collect();
        let sent: Vec<Option<String>> = jobs(&all)
            .into_iter()
            .filter_map(|j| match j {
                Job::StartFollowUp { engine, .. } => Some(engine.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![Some(engine.to_string())], "{engine}: {all:?}");
    }
}

/// The confirmation of the retry names the engine of the blocked run: the retry starts the same request again.
#[test]
fn b981_the_retry_confirmation_names_the_engine_of_the_blocked_run() {
    for (engine, name) in ENGINES {
        let mut app = blocked_on(engine);
        let mut clock = Clock::new();
        let effects = send(&mut app, &mut clock, "try it again");
        assert!(effects.is_empty(), "{engine}: {effects:?}");
        let text = flat(&screen(&app, 160, 50));
        assert!(
            text.contains(&format!(
                "The new {name} receives the request only, not this run's history."
            )),
            "{engine}: {text}"
        );
        if engine != "cline" {
            assert!(!text.contains("The new Cline"), "{engine}: {text}");
        }
    }
}

// ---- a run before its session's first event --------------------------------------------------------------------------------------------

/// A run this screen started is named by the engine it was started on, in the reply box and in the preview of the cancel key, before
/// an event of its session has arrived. A person who chose nothing reads the words of release 5.34.
#[test]
fn b981_a_run_this_screen_started_is_named_by_its_engine_before_its_sessions_first_event() {
    // The person chose OpenCode in the Engine box: the start carries it.
    let mut app = composer_on(None);
    choose_opencode(&mut app);
    assert_eq!(started(&mut app, RUN).as_deref(), Some("opencode"));
    assert_named(&mut app, "OpenCode", "chosen in the Engine box");
    // `default_engine` of the config file names OpenCode: the start carries no engine, and the file decides.
    let mut app = composer_on(Some("opencode"));
    assert_eq!(started(&mut app, RUN), None);
    assert_named(&mut app, "OpenCode", "default_engine of the file");
    // Nothing chosen anywhere.
    let mut app = composer_on(None);
    assert_eq!(started(&mut app, RUN), None);
    assert_named(&mut app, "Cline", "nothing chosen");
}

/// What the session says of its engine wins over what the screen remembered, and a run this screen did not start reads as in release
/// 5.34 until its session names its engine (the run's record carries no engine).
#[test]
fn b981_the_sessions_word_wins_and_a_run_not_started_here_reads_as_before() {
    let mut app = composer_on(None);
    choose_opencode(&mut app);
    started(&mut app, RUN);
    feed(&mut app, base_events());
    assert_named(&mut app, "Cline", "the session named Cline");
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_default_engine(Some("opencode"));
    app.open_run(RUN);
    assert_named(&mut app, "Cline", "not started here, no event yet");
}

/// A follow-up and a retry that this screen started open on the engine of the run they come from.
#[test]
fn b981_a_follow_up_and_a_retry_started_here_are_named_by_the_engine_of_their_source() {
    for (engine, name) in ENGINES {
        let (mut app, mut clock) = finished_with_message(engine);
        send(&mut app, &mut clock, "now add the docs");
        app.update(Msg::Ext(Done::FollowUpStarted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
        }));
        assert_eq!(app.focused_run(), Some(NEW), "{engine}");
        let text = flat(&screen(&app, 120, 30));
        assert!(
            text.contains(&format!("Message to {name}")),
            "{engine} follow-up: {text}"
        );
        let mut app = blocked_on(engine);
        app.update(Msg::Ext(Done::RetryStarted {
            run_id: RUN.into(),
            result: Ok(NEW.into()),
        }));
        assert_eq!(app.focused_run(), Some(NEW), "{engine}");
        let text = flat(&screen(&app, 120, 30));
        assert!(
            text.contains(&format!("Message to {name}")),
            "{engine} retry: {text}"
        );
    }
}

// ---- a run that is not on screen ------------------------------------------------------------------------------------------------------

/// The preview for a run that is not on screen names its engine when this screen started that run; for a run it neither shows nor
/// started it says "the engine", as in release 5.34.
#[test]
fn b981_a_preview_for_a_run_started_here_names_its_engine_when_another_run_is_on_screen() {
    let mut app = composer_on(None);
    choose_opencode(&mut app);
    started(&mut app, NEW);
    app.open_run(RUN);
    feed(&mut app, base_events());
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert_eq!(app.mode(), Mode::Typing);
    let effects = send(&mut app, &mut clock, &format!("/cancel {NEW}"));
    assert!(effects.is_empty(), "{effects:?}");
    let text = flat(&screen(&app, 140, 40));
    assert!(
        text.contains(&format!(
            "Cancel run {NEW}: OpenCode is asked to stop and the work so far is kept."
        )),
        "{text}"
    );
    let mut app = ready(base_events());
    let mut clock = Clock::new();
    send(&mut app, &mut clock, &format!("/cancel {OTHER}"));
    let text = flat(&screen(&app, 140, 40));
    assert!(
        text.contains(&format!(
            "Cancel run {OTHER}: the engine is asked to stop and the work so far is kept."
        )),
        "{text}"
    );
}
