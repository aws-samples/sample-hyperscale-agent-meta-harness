//! The second key of a hotkey preview on the screen of a run. `R` shows the preview of `/resume --all` ("press R again to resume 1
//! paused or stalled run") and the same key again, 0.3 seconds or more later, runs it. A terminal sends an upper-case letter as the letter
//! with the Shift modifier (crossterm reads the byte `R` that way) and some send the letter alone; the key table reads both as `R` for the
//! first press, so the second press must read them the same way. On release 5.21 the second `R` carried Shift, was not taken as the
//! confirming key, and armed the preview again: nothing ran and no request left the client.
//!
//! The tests send the key events a terminal sends, twice, on the screen of a paused run; walk the key table for every hotkey that shows a
//! preview and press each again as a terminal sends it; keep the rules of the preview (a press inside 0.3 seconds is ignored and said, Esc
//! withdraws it, any other key withdraws it and acts as itself); and run the command the second `R` hands over against a stand-in stage that
//! refuses the start because it is being updated: the result panel says so in the words every other way to resume shows.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::cli::{ExecContext, run_captured};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::keys::{Mode, TABLE};
use ham::tui::{App, Effect, Msg, Screen};
use ham_core::config::Config;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OTHER: &str = "01K6A0000000000000000000A1";
/// The one sentence a stage that is being updated answers a start with (control-api `admission`).
const UPDATING: &str =
    "This stage is being updated, so it starts no new runs. Try again in a few minutes.";

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

/// `RUN` ended paused at its total time and delivered its zip: what the service writes after the usual events.
fn paused_events() -> Vec<Envelope> {
    let mut events = base_events();
    events.extend([
        harness_event(
            20,
            EventType::GoalStatus,
            json!({"status": "paused", "phase": "delivering"}),
        ),
        harness_event(
            21,
            EventType::DeliveryReady,
            json!({"artifactId": "01M5A2R7K3C9XJ4D8E6F0000AA", "sha256": "ab".repeat(32), "sizeBytes": 1234}),
        ),
        harness_event(
            22,
            EventType::RunCompleted,
            json!({"status": "paused", "outcome": "stopped_timeout", "label": "Cline stopped: the run reached its time limit"}),
        ),
    ]);
    events
}

/// A row of the Runs list.
fn row(id: &str, status: &str) -> Value {
    let mut v = json!({
        "runId": id, "status": status, "phase": "delivering", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-16T08:15:00Z", "ownerPrincipalArn": "arn:aws:iam::111122223333:role/test"
    });
    if status == "paused" {
        v["statusReason"] = json!("total_time: the run used the total time it was given");
        v["resumeMode"] = json!("follow_up");
        v["session"] = json!(1);
    }
    v
}

fn list(app: &mut App, rows: Vec<Value>) {
    let rows: Vec<harness_protocol::api::v1::RunSummary> =
        serde_json::from_value(Value::Array(rows)).unwrap();
    app.update(Msg::Runs(Ok(rows)));
}

/// The paused run `RUN` on screen in browse mode, with the Runs list read (it holds that one run).
fn paused_run_in_browse_mode() -> (App, Clock) {
    let mut app = ready(paused_events());
    list(&mut app, vec![row(RUN, "paused")]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert_eq!(app.mode(), Mode::Browse);
    assert_eq!(app.screen(), Screen::Dashboard);
    (app, clock)
}

/// A working run on screen in browse mode; the Runs list holds it and a paused run, so every hotkey has something to act on.
fn working_run_in_browse_mode() -> (App, Clock) {
    let mut app = ready(base_events());
    list(&mut app, vec![row(RUN, "active"), row(OTHER, "paused")]);
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    assert_eq!(app.mode(), Mode::Browse);
    (app, clock)
}

fn key_at(
    app: &mut App,
    clock: &mut Clock,
    code: KeyCode,
    mods: KeyModifiers,
    wait_ms: u64,
) -> Vec<Effect> {
    let at = clock.after(wait_ms);
    app.handle_key_at(KeyEvent::new(code, mods), at)
}

/// The letter as a terminal sends it: crossterm reads the byte of an upper-case letter as the letter with Shift.
fn as_a_terminal_sends(c: char) -> KeyModifiers {
    if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    }
}

/// The screen as one line of words, the side borders of its boxes taken out.
fn flat(text: &str) -> String {
    text.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn words(app: &App) -> String {
    flat(&screen(app, 120, 40))
}

/// The commands the effects hand to the runner: each `argv`.
fn commands(effects: &[Effect]) -> Vec<Vec<String>> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::RunCommand { argv, .. } => Some(argv.clone()),
            _ => None,
        })
        .collect()
}

fn resume_all() -> Vec<Vec<String>> {
    vec![vec!["resume".to_string(), "--all".to_string()]]
}

const PREVIEW: &str = "press R again to resume 1 paused or stalled run";

// ---- the defect: the second R as a terminal sends it ------------------------------------------------------------------------------

#[test]
fn b852_an_upper_case_r_twice_on_the_screen_of_a_paused_run_runs_the_previewed_resume() {
    // The byte `R` is read as the letter with Shift; some terminals send the letter with no modifier.
    for mods in [KeyModifiers::SHIFT, KeyModifiers::NONE] {
        let (mut app, mut clock) = paused_run_in_browse_mode();
        let first = key_at(&mut app, &mut clock, KeyCode::Char('R'), mods, 50);
        assert!(first.is_empty(), "{mods:?}: {first:?}");
        let text = words(&app);
        assert!(text.contains("Run ham resume --all."), "{mods:?}: {text}");
        assert!(text.contains(PREVIEW), "{mods:?}: {text}");
        // The same key again, 0.3 seconds or more later: the previewed command runs, once.
        let second = key_at(&mut app, &mut clock, KeyCode::Char('R'), mods, 400);
        assert_eq!(commands(&second), resume_all(), "{mods:?}: {second:?}");
        let text = words(&app);
        assert!(
            text.contains("ham resume --all · running"),
            "{mods:?}: {text}"
        );
        assert!(
            !text.contains("press R again"),
            "{mods:?}: the preview is gone once it ran: {text}"
        );
    }
}

#[test]
fn b852_the_two_presses_need_not_carry_the_same_modifier() {
    // The first with Shift and the second without, and the other way round: both are the key `R`.
    for (one, two) in [
        (KeyModifiers::SHIFT, KeyModifiers::NONE),
        (KeyModifiers::NONE, KeyModifiers::SHIFT),
    ] {
        let (mut app, mut clock) = paused_run_in_browse_mode();
        key_at(&mut app, &mut clock, KeyCode::Char('R'), one, 50);
        let second = key_at(&mut app, &mut clock, KeyCode::Char('R'), two, 400);
        assert_eq!(
            commands(&second),
            resume_all(),
            "{one:?} then {two:?}: {second:?}"
        );
    }
}

#[test]
fn b852_esc_and_the_r_in_one_read_is_the_same_key() {
    // crossterm reads Esc and a letter that arrive in one read as Alt and the letter; in browse mode that is the letter (the key table's rule).
    for mods in [KeyModifiers::ALT, KeyModifiers::ALT | KeyModifiers::SHIFT] {
        let (mut app, mut clock) = paused_run_in_browse_mode();
        key_at(
            &mut app,
            &mut clock,
            KeyCode::Char('R'),
            KeyModifiers::SHIFT,
            50,
        );
        let second = key_at(&mut app, &mut clock, KeyCode::Char('R'), mods, 400);
        assert_eq!(commands(&second), resume_all(), "{mods:?}: {second:?}");
    }
}

// ---- every hotkey of the key table that shows a preview ------------------------------------------------------------------------------

#[test]
fn b852_every_hotkey_that_shows_a_preview_takes_the_same_key_again_as_a_terminal_sends_it() {
    // The letters of browse mode, in the order of the key table.
    let letters: Vec<char> = TABLE
        .iter()
        .filter(|r| r.mode == Mode::Browse && r.chord.mods.is_empty())
        .filter_map(|r| match r.chord.code {
            KeyCode::Char(c) if c.is_alphabetic() => Some(c),
            _ => None,
        })
        .collect();
    let upper: Vec<char> = letters
        .iter()
        .copied()
        .filter(|c| c.is_uppercase())
        .collect();
    assert_eq!(
        upper,
        vec!['R', 'G', 'C'],
        "the upper-case hotkeys of the key table: a new one is pressed below like the others"
    );
    let mut with_preview = Vec::new();
    for c in letters {
        let (mut app, mut clock) = working_run_in_browse_mode();
        let mods = as_a_terminal_sends(c);
        key_at(&mut app, &mut clock, KeyCode::Char(c), mods, 50);
        if !words(&app).contains(&format!("press {c} again")) {
            continue;
        }
        with_preview.push(c);
        let second = key_at(&mut app, &mut clock, KeyCode::Char(c), mods, 400);
        assert!(
            !second.is_empty(),
            "the second {c} ran nothing: {}",
            screen(&app, 120, 40)
        );
        assert!(
            !words(&app).contains(&format!("press {c} again")),
            "the preview of {c} is still up after the second press: {}",
            screen(&app, 120, 40)
        );
    }
    assert_eq!(
        with_preview,
        vec!['R', 'c'],
        "the hotkeys that show a preview: resume every paused and stalled run, cancel the run"
    );
}

// ---- the rules of the preview stay -----------------------------------------------------------------------------------------------------

#[test]
fn b852_a_second_r_inside_the_delay_is_ignored_and_said_and_the_next_one_runs() {
    let (mut app, mut clock) = paused_run_in_browse_mode();
    let shift = KeyModifiers::SHIFT;
    key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
    // 100 ms after the preview: a key held down must not confirm.
    let early = key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 100);
    assert!(early.is_empty(), "{early:?}");
    let text = words(&app);
    assert!(text.contains(PREVIEW), "the preview stays: {text}");
    assert!(
        text.contains("too soon: a second R within 0.3 s of the first is ignored; press R again"),
        "{text}"
    );
    let late = key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 400);
    assert_eq!(commands(&late), resume_all(), "{late:?}");
}

#[test]
fn b852_esc_withdraws_the_preview_and_stays_on_the_run() {
    let (mut app, mut clock) = paused_run_in_browse_mode();
    let shift = KeyModifiers::SHIFT;
    key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
    let esc = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(esc.is_empty(), "{esc:?}");
    assert_eq!(app.screen(), Screen::Dashboard, "Esc did not leave the run");
    assert!(!words(&app).contains("press R again"));
    // The next R is a first press again: a new preview, nothing runs.
    let again = key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 400);
    assert!(again.is_empty(), "{again:?}");
    assert!(words(&app).contains(PREVIEW));
}

#[test]
fn b852_any_other_key_withdraws_the_preview_and_acts_as_itself() {
    let shift = KeyModifiers::SHIFT;
    // `j` scrolls the conversation: the preview goes and nothing runs.
    let (mut app, mut clock) = paused_run_in_browse_mode();
    key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
    let other = press(&mut app, &mut clock, KeyCode::Char('j'), 400);
    assert!(commands(&other).is_empty(), "{other:?}");
    assert!(!words(&app).contains("press R again"));
    let again = key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 400);
    assert!(
        again.is_empty(),
        "a first press again, not a confirmation: {again:?}"
    );
    // `i` starts typing: the preview goes and the box has the focus.
    let (mut app, mut clock) = paused_run_in_browse_mode();
    key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
    press(&mut app, &mut clock, KeyCode::Char('i'), 400);
    assert_eq!(app.mode(), Mode::Typing);
    assert!(!words(&app).contains("press R again"));
    // The letter with Control held is not the key: it withdraws the preview and runs nothing.
    let (mut app, mut clock) = paused_run_in_browse_mode();
    key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
    let ctrl = key_at(
        &mut app,
        &mut clock,
        KeyCode::Char('R'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        400,
    );
    assert!(commands(&ctrl).is_empty(), "{ctrl:?}");
    assert!(!words(&app).contains("press R again"));
    // A lower-case r is another key (on a run that is not blocked it opens the Runs list).
    let (mut app, mut clock) = paused_run_in_browse_mode();
    key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
    let lower = press(&mut app, &mut clock, KeyCode::Char('r'), 400);
    assert!(commands(&lower).is_empty(), "{lower:?}");
    assert_eq!(app.screen(), Screen::Runs);
}

// ---- the stage refuses the start: the panel says why --------------------------------------------------------------------------------

/// A stand-in stage that lists one paused run and refuses every resume because it is being updated; it keeps what it was asked.
#[derive(Default)]
struct Updating {
    calls: Mutex<Vec<(String, String)>>,
}

impl ControlPlane for Updating {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), path.to_string()));
        let route = path.split('?').next().unwrap_or_default();
        let (status, body) = match (method, route) {
            ("GET", "/v1/runs") => (200, json!({"runs": [row(RUN, "paused")]})),
            ("POST", r) if r == format!("/v1/runs/{RUN}/resume") => (
                409,
                json!({"code": ham_core::update::STAGE_UPDATING, "message": UPDATING}),
            ),
            _ => (
                404,
                json!({"code": "not-found", "message": "no such route"}),
            ),
        };
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        })
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("a resume fetches nothing")
    }
}

#[tokio::test]
async fn b852_the_second_r_ends_in_the_panel_with_the_reason_while_the_stage_is_being_updated() {
    for (w, h) in [(80, 30), (120, 30)] {
        let (mut app, mut clock) = paused_run_in_browse_mode();
        let shift = KeyModifiers::SHIFT;
        key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 50);
        let second = key_at(&mut app, &mut clock, KeyCode::Char('R'), shift, 400);
        // The command the second R hands to the runner, run as the event loop runs it, against the stand-in stage.
        let (invocation, argv) = second
            .iter()
            .find_map(|e| match e {
                Effect::RunCommand { invocation, argv } => Some((invocation.clone(), argv.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{w}x{h}: the second R ran no command: {second:?}"));
        let stage = Arc::new(Updating::default());
        let api = Api::new(stage.clone()).with_retry(Retry {
            attempts: 1,
            base: Duration::from_millis(1),
        });
        let env = |_: &str| None;
        let downloads = std::env::temp_dir();
        let out = run_captured(
            &invocation,
            &ExecContext {
                api: &api,
                cfg: &Config::default(),
                downloads: &downloads,
                chunk: 16 << 20,
                env: &env,
                stdin: None,
            },
        )
        .await;
        // The request reached the stage: one resume, for the paused run.
        let posts: Vec<String> = stage
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "POST")
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(posts, vec![format!("/v1/runs/{RUN}/resume")], "{w}x{h}");
        assert_eq!(out.code, 1, "{w}x{h}: {}", out.stdout_text());
        app.update(Msg::CommandDone { argv, out });
        let text = flat(&screen(&app, w, h));
        assert!(
            text.contains("ham resume --all · exit 1"),
            "{w}x{h}: {text}"
        );
        // The words the other ways to resume show, whole at this width.
        assert!(
            text.contains(&format!("run {RUN}: not started: {UPDATING}")),
            "{w}x{h}: {text}"
        );
    }
}
