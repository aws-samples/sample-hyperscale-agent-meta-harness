//! A question rings the bell once, whichever screen shows it first. The Runs list keeps its alerts by run (`Alerts`) and the run
//! screen keeps the questions it rang for by request id, so a question that rang on the list rang a second time when the run was opened, and
//! one that rang on the run's screen rang again when the person went back to the list. The two screens now share the fact that a request
//! already rang. A run that starts to wait while its screen is open still rings once, and a new question rings again.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::{EventsPage, RunSummary};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const RUN: &str = "01K6A0000000000000000000R1";
const OTHER: &str = "01K6A0000000000000000000R2";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const REQ2: &str = "01M5B8T4W2N6QJ3C7D9EH1FZ0A";
const OPENED: &str = "2026-10-15T13:41:07Z";

fn list_row(id: &str, request: Option<&str>) -> RunSummary {
    let mut v = json!({"runId": id, "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": OPENED, "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"});
    if let Some(r) = request {
        v["needsInput"] = json!({"requestId": r, "reason": "engine_stop", "title": "Cline stopped",
                                 "expiresAt": "2026-10-15T14:11:07Z"});
    }
    serde_json::from_value(v).unwrap()
}

fn envelope(run: &str, n: u32, agent: Option<&str>, t: EventType, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: run.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: 0,
        ts: OPENED.into(),
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

/// The run while Cline works: created, one session, active.
fn working(run: &str) -> Vec<Envelope> {
    vec![
        envelope(
            run,
            1,
            None,
            EventType::RunCreated,
            json!({"target": {"mode": "s3-only"}}),
        ),
        envelope(
            run,
            2,
            None,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": ham_core::models::DEFAULT_MODEL_ID}),
        ),
        envelope(
            run,
            3,
            None,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        ),
    ]
}

/// A blocking question of the direct agent with this request id and number `n`; its event id is the request id, so a later request sorts
/// after an earlier one.
fn question(run: &str, request: &str, n: u32) -> Envelope {
    let mut e = envelope(
        run,
        40,
        Some("direct"),
        EventType::TurnProgress,
        json!({
            "turn": 1, "kind": "needs-input",
            "text": "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)",
            "requestId": request, "n": n, "reason": "engine_stop", "title": "Cline stopped its own prompt",
            "stopOutcome": "stopped_engine_cancelled",
            "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                         "lastError": "x", "runS": 412, "sessions": 1},
            "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
            "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-15T14:11:07Z"
        }),
    );
    e.event_id = request.to_string();
    e
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

fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn screen(app: &App) -> String {
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..30 {
        for x in 0..100 {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// The Runs list after its first read (which only seeds the memory of which runs wait), listing `RUN` as running.
fn listing() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let effects = app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    assert_eq!(rings(&effects), 0, "the first read only seeds: {effects:?}");
    assert_eq!(app.screen(), Screen::Runs);
    app
}

/// Opens the selected run from the Runs list with Enter.
fn open(app: &mut App) {
    key(app, KeyCode::Enter);
    assert_eq!(app.screen(), Screen::Dashboard);
}

#[test]
fn a_question_that_rang_on_the_runs_list_does_not_ring_again_when_the_run_is_opened() {
    let mut app = listing();
    // The run starts to wait while the app shows the list: one bell there. TRIGGER PRESENT: the list rang.
    let on_list = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&on_list), 1, "TRIGGER ABSENT: {on_list:?}");
    // The person opens the run; the run's events show the same question. TRIGGER PRESENT: the card is up.
    open(&mut app);
    app.update(pages(working(RUN)));
    let on_open = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert!(
        screen(&app).contains("Cline needs you"),
        "TRIGGER ABSENT: the question is not on the run's screen\n{}",
        screen(&app)
    );
    assert_eq!(
        rings(&on_open),
        0,
        "the question already rang on the list: {on_open:?}"
    );
    // Further polls of the open question ring nothing either.
    let again = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&again), 0, "{again:?}");
}

#[test]
fn a_question_that_rang_on_the_run_screen_does_not_ring_again_on_the_runs_list() {
    let mut app = listing();
    open(&mut app);
    app.update(pages(working(RUN)));
    // The run starts to wait while its screen is open: one bell there. TRIGGER PRESENT.
    let on_screen = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&on_screen), 1, "TRIGGER ABSENT: {on_screen:?}");
    // Back on the list, whose next read shows the run waiting for the first time.
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    let on_list = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(
        rings(&on_list),
        0,
        "the question already rang on the run's screen: {on_list:?}"
    );
    // The next reads, with the run still waiting, ring nothing.
    let more = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&more), 0, "{more:?}");
}

#[test]
fn a_run_that_starts_to_wait_while_its_screen_is_open_still_rings_once() {
    let mut app = listing();
    open(&mut app);
    let quiet = app.update(pages(working(RUN)));
    assert_eq!(rings(&quiet), 0, "{quiet:?}");
    let asked = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&asked), 1, "{asked:?}");
    let polled = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&polled), 0, "{polled:?}");
}

#[test]
fn a_run_that_waited_before_the_app_looked_still_rings_once_when_it_is_opened() {
    // The first read of the list makes no sound, so nothing has rung for this question: opening the run rings once, as it always did.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    open(&mut app);
    app.update(pages(working(RUN)));
    let opened = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&opened), 1, "{opened:?}");
}

#[test]
fn a_new_question_of_the_same_run_rings_again_on_the_list_and_on_the_screen() {
    let mut app = listing();
    let first = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&first), 1, "TRIGGER ABSENT: {first:?}");
    // Answered: the list shows the run working; then it asks again with a new request id.
    let working_again = app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    assert_eq!(rings(&working_again), 0, "{working_again:?}");
    let second = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ2))])));
    assert_eq!(rings(&second), 1, "a new question rings: {second:?}");
    // On the run's screen the second question has rung already; a third one rings there.
    open(&mut app);
    app.update(pages(working(RUN)));
    let on_open = app.update(pages(vec![question(RUN, REQ2, 2)]));
    assert_eq!(rings(&on_open), 0, "{on_open:?}");
    let third = app.update(pages(vec![question(RUN, "01M5C8T4W2N6QJ3C7D9EH1FZ0B", 3)]));
    assert_eq!(rings(&third), 1, "{third:?}");
}

#[test]
fn a_run_that_stops_waiting_and_waits_again_on_the_list_rings_again() {
    // The rule the list always had: a run that is answered and waits again rings again (the same request id is a test double here).
    let mut app = listing();
    let first = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&first), 1, "{first:?}");
    app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    let again = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&again), 1, "{again:?}");
}

#[test]
fn the_notification_is_not_sent_twice_either() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_notify(true);
    app.set_focus(false);
    app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    let on_list = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&on_list), 1, "TRIGGER ABSENT: {on_list:?}");
    assert_eq!(notifications(&on_list), 1, "TRIGGER ABSENT: {on_list:?}");
    open(&mut app);
    app.update(pages(working(RUN)));
    let on_open = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&on_open), 0, "{on_open:?}");
    assert_eq!(notifications(&on_open), 0, "{on_open:?}");
}

#[test]
fn the_bell_of_a_batch_goes_to_the_first_run_that_has_not_rung() {
    // RUN rang on its own screen. Then RUN and OTHER both show as waiting in one read of the list: nothing is rung for RUN, the one bell of the
    // batch and the one notification are for OTHER.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_notify(true);
    app.set_focus(false);
    app.update(Msg::Runs(Ok(vec![
        list_row(RUN, None),
        list_row(OTHER, None),
    ])));
    // The first row is RUN: open it.
    open(&mut app);
    app.update(pages(working(RUN)));
    let on_screen = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&on_screen), 1, "TRIGGER ABSENT: {on_screen:?}");
    key(&mut app, KeyCode::Esc);
    let batch = app.update(Msg::Runs(Ok(vec![
        list_row(RUN, Some(REQ)),
        list_row(OTHER, Some(REQ2)),
    ])));
    assert_eq!(rings(&batch), 1, "OTHER has not rung yet: {batch:?}");
    let told: Vec<&str> = batch
        .iter()
        .filter_map(|e| match e {
            Effect::Notify(a) => Some(a.run_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(told, [OTHER], "{batch:?}");
}

#[test]
fn a_new_question_asked_between_two_reads_of_the_list_rings_on_the_list() {
    // The run answered its first question and asked again before the list was read once more, so no read saw it working in between: the
    // request id is what tells the two questions apart.
    let mut app = listing();
    let first = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&first), 1, "TRIGGER ABSENT: {first:?}");
    let same = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&same), 0, "the same question rings once: {same:?}");
    let second = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ2))])));
    assert_eq!(rings(&second), 1, "a new question rings: {second:?}");
}

#[test]
fn a_new_question_between_two_reads_sends_its_notification_for_the_right_run() {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.set_notify(true);
    app.set_focus(false);
    app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    let second = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ2))])));
    let told: Vec<&str> = second
        .iter()
        .filter_map(|e| match e {
            Effect::Notify(a) => Some(a.run_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(told, [RUN], "{second:?}");
}

#[test]
fn a_list_that_loses_sight_of_the_question_does_not_make_it_ring_again_when_the_run_is_opened() {
    // A run that keeps writing events can push its open question out of the tail the list reads, so the list shows it as working for a read
    // while the question is still open. That is not a new question.
    let mut app = listing();
    let on_list = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&on_list), 1, "TRIGGER ABSENT: {on_list:?}");
    app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    open(&mut app);
    app.update(pages(working(RUN)));
    let on_open = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert!(
        screen(&app).contains("Cline needs you"),
        "TRIGGER ABSENT: the question is not on the run's screen"
    );
    assert_eq!(rings(&on_open), 0, "{on_open:?}");
}

#[test]
fn a_question_that_rang_on_the_screen_stays_quiet_on_the_list_after_a_stale_read() {
    let mut app = listing();
    open(&mut app);
    app.update(pages(working(RUN)));
    let on_screen = app.update(pages(vec![question(RUN, REQ, 1)]));
    assert_eq!(rings(&on_screen), 1, "TRIGGER ABSENT: {on_screen:?}");
    key(&mut app, KeyCode::Esc);
    // The list reads the run as working (stale), then as waiting for the same question.
    let stale = app.update(Msg::Runs(Ok(vec![list_row(RUN, None)])));
    assert_eq!(rings(&stale), 0, "{stale:?}");
    let back = app.update(Msg::Runs(Ok(vec![list_row(RUN, Some(REQ))])));
    assert_eq!(rings(&back), 0, "{back:?}");
}

#[test]
fn a_request_id_that_rang_for_one_run_does_not_silence_another_run() {
    // Request ids are unique in the service; the record is still kept per run, so a repeated id (a test double, an old backend) never
    // silences a different run.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::Runs(Ok(vec![
        list_row(RUN, None),
        list_row(OTHER, None),
    ])));
    let on_list = app.update(Msg::Runs(Ok(vec![
        list_row(RUN, Some(REQ)),
        list_row(OTHER, None),
    ])));
    assert_eq!(rings(&on_list), 1, "TRIGGER ABSENT: {on_list:?}");
    // OTHER is the second row: select it and open it; its question has the same request id.
    key(&mut app, KeyCode::Down);
    open(&mut app);
    app.update(pages(working(OTHER)));
    let on_open = app.update(pages(vec![question(OTHER, REQ, 1)]));
    assert_eq!(
        rings(&on_open),
        1,
        "OTHER's question has not rung: {on_open:?}"
    );
}
