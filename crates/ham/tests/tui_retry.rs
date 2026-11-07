//! A blocked run in the terminal app. A run that ended `blocked` delivered nothing and its session is over, so a message to it cannot
//! reach the agent and `/cancel` has nothing to stop. The message box says so and says what Enter does instead: it offers to start the run
//! again with the request it was started with, linked to the blocked run (`POST /v1/runs/{id}/retry`; the pure parts are `ham_core::retry`).
//! Enter on text offers the retry and keeps the text; the second Enter, 300 ms or more later, starts it. `r` in browse mode and `/retry` give the
//! same offer. A run that is not blocked behaves as it did.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::ext::{Done, Job};
use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::runfilter::{IndexEntry, IndexedRead, RunsRead};
use harness_protocol::api::v1::{RunDetail, RunSummary};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use serde_json::{Value, json};

const NEW: &str = "01K6A0000000000000000000R2";
const REASON: &str = "the run machine failed: Lambda could not read its container image";

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

/// What `mark-blocked` writes: the reason, the status, and the end of the run.
fn blocked_tail() -> Vec<Envelope> {
    vec![
        harness_event(20, EventType::GoalBlocked, json!({"reason": REASON})),
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
    ]
}

fn blocked() -> App {
    let mut events = base_events();
    events.extend(blocked_tail());
    ready(events)
}

fn working() -> App {
    ready(base_events())
}

fn finished() -> App {
    ready(finished_events())
}

fn row_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no row has {needle:?}:\n{text}"))
}

fn retry_jobs(effects: &[Effect]) -> Vec<String> {
    jobs(effects)
        .into_iter()
        .filter_map(|j| match j {
            Job::RetryRun { run_id } => Some(run_id.clone()),
            _ => None,
        })
        .collect()
}

fn started(app: &mut App, result: Result<String, String>) -> Vec<Effect> {
    app.update(Msg::Ext(Done::RetryStarted {
        run_id: RUN.into(),
        result,
    }))
}

// ---- what the box and the footer say --------------------------------------------------------------------------------------------

#[test]
fn the_box_of_a_blocked_run_says_what_enter_does_and_never_offers_cancel() {
    let app = blocked();
    let text = screen(&app, 120, 40);
    let title = row_with(&text, "Message to Cline");
    assert!(
        title.contains("Enter: Retry blocked run 01K6A000: new billable run"),
        "{text}"
    );
    assert!(!text.contains("Cline is not waiting"), "{text}");
    assert!(!text.contains("/cancel"), "{text}");
    assert!(!text.contains("c cancel"), "{text}");
    // What happened: the status and the reason the service gave.
    assert!(text.contains("blocked"), "{text}");
    assert!(text.contains(REASON), "{text}");
}

#[test]
fn the_card_of_a_blocked_run_offers_the_retry_in_place_of_cancel() {
    let app = blocked();
    let text = screen(&app, 120, 40);
    assert!(text.contains("blocked · r retry"), "{text}");
    assert!(!text.contains("blocked · c cancel"), "{text}");
}

#[test]
fn the_footers_of_a_blocked_run_name_r_and_never_c() {
    let mut app = blocked();
    let mut clock = Clock::new();
    // Typing mode: the keys that work are in browse mode, and the footer says the way there.
    let typing = screen(&app, 120, 40);
    let footer = typing.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    assert!(footer.contains("Esc, then r retry"), "{footer}");
    assert!(!footer.contains("cancel"), "{footer}");
    // Browse mode: `r retry` stands where `c cancel` stood.
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let browse = screen(&app, 120, 40);
    let footer = browse.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    assert!(footer.contains("r retry"), "{footer}");
    assert!(
        footer.contains("Esc runs"),
        "Esc still opens the Runs list: {footer}"
    );
    assert!(!footer.contains("cancel"), "{footer}");
}

#[test]
fn the_typing_footer_of_a_blocked_run_never_offers_cancel_at_any_width() {
    // The typing footer names the hotkeys that follow Esc and puts `c c cancel` among them. A blocked run has ended and has nothing to
    // cancel: none of the wordings names cancel, `a artifacts` stays while it fits, and the line ends with the way to `r retry`.
    let app = blocked();
    let footer = |width: u16| {
        screen(&app, width, 40)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap()
            .trim_end()
            .to_string()
    };
    let line = |esc: &str| {
        format!(
            "Enter send \u{b7} Ctrl-J new line \u{b7} {esc} \u{b7} Ctrl-C clear, then quit \u{b7} Esc, then r retry"
        )
    };
    // Room for the longest wording, and room for nothing but the usual line.
    for width in [120, 200] {
        assert_eq!(
            footer(width),
            line("Esc browse, then a artifacts"),
            "{width}"
        );
    }
    assert_eq!(footer(90), line("Esc browse"), "90");
    for width in [60, 80, 90, 120, 200] {
        let text = footer(width);
        assert!(!text.contains("cancel"), "{width}: {text}");
    }
}

#[test]
fn a_stale_question_on_a_blocked_run_does_not_take_enter() {
    // The run asked a question and was then blocked: nothing can answer it any more, so Enter offers the retry, not an answer.
    let mut events = base_events();
    let mut q = {
        let mut e = base_events().remove(0);
        e.event_type = EventType::TurnProgress;
        e.authority = Authority::Agent;
        e.agent_id = Some("direct".into());
        e.event_id = "01K5A2R7K3C9XJ4D8E6FQ0WT1N".into();
        e.seq = 0;
        e.payload = Payload::Inline {
            value: json!({
                "turn": 1, "kind": "needs-input", "text": "Needs your input", "requestId": "01K5A2R7K3C9XJ4D8E6FQ0WT1N", "n": 1,
                "reason": "engine_stop", "title": "Cline stopped its own prompt", "stopOutcome": "stopped_engine_cancelled",
                "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                             "lastError": "x", "runS": 412, "sessions": 1},
                "lastMessage": "I will keep polling.", "files": [], "filesTotal": 0, "blocking": true,
                "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
                "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-16T09:15:00Z"
            }),
        };
        e
    };
    q.run_id = RUN.into();
    events.push(q);
    events.extend(blocked_tail());
    let mut app = ready(events);
    let mut clock = Clock::new();
    let title = row_with(&screen(&app, 140, 40), "Message to Cline").to_string();
    assert!(title.contains("Retry blocked run"), "{title}");
    let effects = send(&mut app, &mut clock, "use the other API");
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Answer { .. })),
        "{effects:?}"
    );
    assert!(
        retry_jobs(&effects).is_empty(),
        "the first Enter only offers"
    );
}

// ---- Enter ------------------------------------------------------------------------------------------------------------------------

#[test]
fn enter_on_text_offers_the_retry_keeps_the_text_and_starts_nothing() {
    let mut app = blocked();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "please try again");
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("please try again"));
    let text = screen(&app, 120, 40);
    assert!(text.contains("Retry blocked run?"), "{text}");
    assert!(text.contains("billable"), "{text}");
    assert!(text.contains("same request"), "{text}");
    assert!(
        !text.contains("Target:"),
        "no target is named before the run's detail has arrived: {text}"
    );
    assert!(
        text.contains("Your text stays in the box and is not sent"),
        "{text}"
    );
    assert!(text.contains("Esc cancels"), "{text}");
}

#[test]
fn the_offer_names_the_target_of_the_blocked_run_once_its_detail_is_known() {
    let mut app = blocked();
    let mut clock = Clock::new();
    let detail: RunDetail = serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "blocked", "phase": "delivering", "target": {"mode": "repo", "repo": "cc:calc"},
                    "createdAt": "2026-10-16T08:15:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Build a todo API", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap();
    app.update(Msg::Detail(Ok(detail)));
    send(&mut app, &mut clock, "go");
    let text = screen(&app, 120, 40);
    assert!(text.contains("Target: cc:calc"), "{text}");
}

#[test]
fn enter_on_an_empty_box_of_a_blocked_run_offers_the_retry_too() {
    // The title says that Enter retries; an Enter that did nothing would be the silent no-op the box is meant to end.
    let mut app = blocked();
    let mut clock = Clock::new();
    let first = enter(&mut app, &mut clock, 10);
    assert!(first.is_empty(), "{first:?}");
    let text = screen(&app, 120, 40);
    assert!(text.contains("Retry blocked run?"), "{text}");
    assert!(
        !text.contains("Your text stays"),
        "no text, nothing to keep: {text}"
    );
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(retry_jobs(&effects), vec![RUN.to_string()], "{effects:?}");
    // On a run that is not blocked an empty Enter is still nothing.
    for mut other in [working(), finished()] {
        let mut clock = Clock::new();
        let effects = enter(&mut other, &mut clock, 10);
        assert!(effects.is_empty(), "{effects:?}");
        assert!(!screen(&other, 120, 40).contains("Retry blocked run?"));
    }
}

#[test]
fn the_text_in_the_box_shows_the_offer_before_any_enter() {
    let mut app = blocked();
    let mut clock = Clock::new();
    type_text(&mut app, &mut clock, "try once more");
    let text = screen(&app, 120, 40);
    assert!(text.contains("This run is blocked"), "{text}");
    assert!(text.contains("takes no messages"), "{text}");
    assert!(!text.contains("Follow-up"), "{text}");
}

#[test]
fn the_second_enter_starts_the_retry_and_not_the_first() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    // Within 300 ms of the offer: ignored, and the offer stays.
    let early = enter(&mut app, &mut clock, 100);
    assert!(early.is_empty(), "{early:?}");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    // After it: the start.
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(retry_jobs(&effects), vec![RUN.to_string()], "{effects:?}");
    assert_eq!(
        app.chat_text().as_deref(),
        Some("go"),
        "the text is kept until the service answers"
    );
}

#[test]
fn a_confirming_key_that_arrives_in_a_burst_never_starts_the_retry_however_late_it_is() {
    // A held Enter, or a paste that ends in Enter, arrives as keys less than 30 ms apart (burst.rs). The second key of the offer is a billable
    // start, so it needs a calm key. The first confirming key comes 290 ms after the offer (too early, ignored, the offer stays) and the second
    // 20 ms after that: 310 ms after the offer, but inside the burst.
    // Typing mode: Enter.
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    let early = enter(&mut app, &mut clock, 290);
    assert!(early.is_empty(), "{early:?}");
    let burst = enter(&mut app, &mut clock, 20);
    assert!(retry_jobs(&burst).is_empty(), "{burst:?}");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    // The same key, calm, starts it.
    let calm = enter(&mut app, &mut clock, 400);
    assert_eq!(retry_jobs(&calm), vec![RUN.to_string()], "{calm:?}");
    // Browse mode: `r`.
    let mut app = blocked();
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('r'), 50);
    let early = press(&mut app, &mut clock, KeyCode::Char('r'), 290);
    assert!(early.is_empty(), "{early:?}");
    let burst = press(&mut app, &mut clock, KeyCode::Char('r'), 20);
    assert!(retry_jobs(&burst).is_empty(), "{burst:?}");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    let calm = press(&mut app, &mut clock, KeyCode::Char('r'), 400);
    assert_eq!(retry_jobs(&calm), vec![RUN.to_string()], "{calm:?}");
}

#[test]
fn enter_with_a_modifier_never_starts_the_retry() {
    // Shift-Enter and Alt-Enter are keys of the box (a new line, the literal send); a billable start needs the plain Enter.
    for mods in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
    ] {
        let mut app = blocked();
        let mut clock = Clock::new();
        send(&mut app, &mut clock, "go");
        let at = clock.after(400);
        let effects = app.handle_key_at(KeyEvent::new(KeyCode::Enter, mods), at);
        assert!(retry_jobs(&effects).is_empty(), "{mods:?}: {effects:?}");
        // Shift-Enter is a new line in the box: it withdraws the offer like any key that edits the text. Alt-Enter sends the box as chat,
        // which on a blocked run is the same offer again; it starts nothing either.
        if mods == KeyModifiers::SHIFT {
            assert!(
                !screen(&app, 120, 40).contains("Retry blocked run?"),
                "{mods:?}: the offer is withdrawn like any other key's"
            );
        }
    }
}

#[test]
fn esc_withdraws_the_offer_and_keeps_the_text() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    let effects = press(&mut app, &mut clock, KeyCode::Esc, 400);
    assert!(effects.is_empty());
    let text = screen(&app, 120, 40);
    assert!(!text.contains("Retry blocked run?"), "{text}");
    assert_eq!(app.chat_text().as_deref(), Some("go"));
    // Enter offers again; it does not start.
    let again = enter(&mut app, &mut clock, 400);
    assert!(retry_jobs(&again).is_empty(), "{again:?}");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
}

#[test]
fn typing_after_the_offer_withdraws_it() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    type_text(&mut app, &mut clock, "x");
    assert!(!screen(&app, 120, 40).contains("Retry blocked run?"));
    assert_eq!(app.chat_text().as_deref(), Some("gox"));
    let effects = enter(&mut app, &mut clock, 400);
    assert!(retry_jobs(&effects).is_empty(), "{effects:?}");
}

// ---- the answer ---------------------------------------------------------------------------------------------------------------------

#[test]
fn the_new_run_opens_and_the_text_goes_with_the_person() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    enter(&mut app, &mut clock, 400);
    let effects = started(&mut app, Ok(NEW.into()));
    assert_eq!(app.focused_run(), Some(NEW), "{effects:?}");
    assert_eq!(
        app.chat_text().as_deref(),
        Some("go"),
        "the text that was never sent is in the box of the new run"
    );
    let text = screen(&app, 120, 40);
    assert!(
        text.contains(RUN) && text.contains("retry"),
        "the notice names the blocked run: {text}"
    );
}

#[test]
fn a_refusal_keeps_the_run_the_text_and_the_words() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    enter(&mut app, &mut clock, 400);
    started(
        &mut app,
        Err("model m is not offered any more: start a new run".into()),
    );
    assert_eq!(app.focused_run(), Some(RUN));
    assert_eq!(app.chat_text().as_deref(), Some("go"));
    let text = screen(&app, 120, 40);
    assert!(text.contains("not offered any more"), "{text}");
    // A new offer is possible, and starts nothing by itself.
    let again = enter(&mut app, &mut clock, 400);
    assert!(retry_jobs(&again).is_empty(), "{again:?}");
}

#[test]
fn an_answer_for_a_run_that_is_not_on_screen_is_a_notice_only() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    enter(&mut app, &mut clock, 400);
    // The person went to another run before the answer came.
    app.open_run("01K6A0000000000000000000R9");
    let effects = started(&mut app, Ok(NEW.into()));
    assert_eq!(
        app.focused_run(),
        Some("01K6A0000000000000000000R9"),
        "{effects:?}"
    );
}

#[test]
fn a_run_that_stopped_being_blocked_under_the_offer_starts_nothing_and_says_so() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    // The run is active again (a newer goal.status arrives while the offer waits): the second Enter checks the run once more.
    feed(
        &mut app,
        vec![harness_event(
            30,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        )],
    );
    let effects = enter(&mut app, &mut clock, 400);
    assert!(retry_jobs(&effects).is_empty(), "{effects:?}");
    let text = screen(&app, 120, 40);
    assert!(text.contains("the run changed while you looked"), "{text}");
    assert!(!text.contains("starting the retry"), "{text}");
}

#[test]
fn a_second_start_while_the_first_is_out_starts_nothing_and_an_answer_frees_the_key() {
    let mut app = blocked();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "go");
    let first = enter(&mut app, &mut clock, 400);
    assert_eq!(retry_jobs(&first), vec![RUN.to_string()], "{first:?}");
    // The answer has not come. The text is still in the box, so Enter offers again, and the next Enter would start a second run.
    enter(&mut app, &mut clock, 400);
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    let second = enter(&mut app, &mut clock, 400);
    assert!(retry_jobs(&second).is_empty(), "{second:?}");
    let text = screen(&app, 120, 40);
    assert!(text.contains("the retry is being started"), "{text}");
    // The service refuses the first call: the flag is cleared and a new offer can start.
    started(&mut app, Err("the service is busy".into()));
    enter(&mut app, &mut clock, 400);
    let third = enter(&mut app, &mut clock, 400);
    assert_eq!(retry_jobs(&third), vec![RUN.to_string()], "{third:?}");
}

// ---- r and /retry --------------------------------------------------------------------------------------------------------------

#[test]
fn r_in_browse_mode_offers_the_retry_and_the_same_key_again_starts_it() {
    let mut app = blocked();
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let first = press(&mut app, &mut clock, KeyCode::Char('r'), 50);
    assert!(first.is_empty(), "{first:?}");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    let early = press(&mut app, &mut clock, KeyCode::Char('r'), 100);
    assert!(early.is_empty(), "{early:?}");
    let effects = press(&mut app, &mut clock, KeyCode::Char('r'), 400);
    assert_eq!(retry_jobs(&effects), vec![RUN.to_string()], "{effects:?}");
}

#[test]
fn in_browse_mode_esc_and_any_other_key_withdraw_the_offer() {
    let mut app = blocked();
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('r'), 50);
    // Esc withdraws it and stays on the run.
    press(&mut app, &mut clock, KeyCode::Esc, 400);
    let text = screen(&app, 120, 40);
    assert!(!text.contains("Retry blocked run?"), "{text}");
    assert!(
        text.contains("Message to Cline"),
        "still on the run: {text}"
    );
    // Another key withdraws it and acts as itself.
    press(&mut app, &mut clock, KeyCode::Char('r'), 400);
    press(&mut app, &mut clock, KeyCode::Char('j'), 400);
    assert!(!screen(&app, 120, 40).contains("Retry blocked run?"));
    let effects = press(&mut app, &mut clock, KeyCode::Char('r'), 400);
    assert!(
        retry_jobs(&effects).is_empty(),
        "a new offer, not a start: {effects:?}"
    );
}

#[test]
fn r_on_a_run_that_is_not_blocked_still_opens_the_runs_list() {
    for mut app in [working(), finished()] {
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Esc, 50);
        let effects = press(&mut app, &mut clock, KeyCode::Char('r'), 50);
        assert!(
            matches!(effects.as_slice(), [Effect::LoadRuns]),
            "{effects:?}"
        );
        assert!(screen(&app, 120, 40).contains("Runs"));
    }
}

#[test]
fn slash_retry_offers_the_retry_and_the_next_enter_starts_it() {
    let mut app = blocked();
    let mut clock = Clock::new();
    let first = send(&mut app, &mut clock, "/retry");
    assert!(first.is_empty(), "{first:?}");
    assert!(screen(&app, 120, 40).contains("Retry blocked run?"));
    let effects = enter(&mut app, &mut clock, 400);
    assert_eq!(retry_jobs(&effects), vec![RUN.to_string()], "{effects:?}");
}

#[test]
fn slash_retry_on_a_run_that_is_not_blocked_says_so_and_starts_nothing() {
    for mut app in [working(), finished()] {
        let mut clock = Clock::new();
        let effects = send(&mut app, &mut clock, "/retry");
        assert!(retry_jobs(&effects).is_empty(), "{effects:?}");
        let text = screen(&app, 120, 40);
        assert!(text.contains("only a blocked run can be retried"), "{text}");
        let again = enter(&mut app, &mut clock, 400);
        assert!(retry_jobs(&again).is_empty(), "{again:?}");
    }
}

// ---- cancel is not offered for a run that has ended ---------------------------------------------------------------------------

#[test]
fn c_on_a_blocked_run_starts_no_cancel_and_points_to_the_retry() {
    let mut app = blocked();
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 50);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 120, 40);
    assert!(!text.contains("press c again"), "no cancel preview: {text}");
    assert!(text.contains("already ended"), "{text}");
    assert!(text.contains("r retries it"), "{text}");
    // The second `c` is nothing either.
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(!screen(&app, 120, 40).contains("press c again"));
}

#[test]
fn c_on_a_working_run_still_previews_the_cancel() {
    let mut app = working();
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('c'), 50);
    assert!(screen(&app, 120, 40).contains("press c again to cancel run"));
}

#[test]
fn the_command_popup_leaves_out_cancel_on_a_blocked_run_and_retry_on_the_others() {
    // The popup lists the words that start with what follows the slash (it shows the first rows of the list, so the prefix is typed).
    let popup = |mut app: App, typed: &str| {
        let mut clock = Clock::new();
        type_text(&mut app, &mut clock, typed);
        screen(&app, 120, 40)
    };
    let on_blocked = popup(blocked(), "/ret");
    assert!(on_blocked.contains("/retry"), "{on_blocked}");
    assert!(
        !popup(blocked(), "/can").contains("/cancel"),
        "a run that ended has nothing to cancel"
    );
    let on_working = popup(working(), "/can");
    assert!(on_working.contains("/cancel"), "{on_working}");
    assert!(
        !popup(working(), "/ret").contains("/retry"),
        "only a blocked run can be retried"
    );
    assert!(!popup(finished(), "/ret").contains("/retry"));
}

// ---- the Runs list ------------------------------------------------------------------------------------------------------------------

#[test]
fn a_run_that_continues_another_run_is_marked_in_the_runs_list() {
    // The marker is drawn from the summary's `continueFrom` (`ham_core::retry::link_note` and the NAME cell); the Runs screen tests of the
    // table (`tui_runs_screen`, `tui_runs_titles`) hold the layouts it must leave alone.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    let rows: Vec<RunSummary> = serde_json::from_value(json!([
        {"runId": NEW, "status": "active", "phase": "green", "target": {"mode": "s3-only"}, "createdAt": "2026-10-16T08:20:00Z",
         "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b", "continueFrom": RUN},
        {"runId": RUN, "status": "blocked", "phase": "green", "target": {"mode": "s3-only"}, "createdAt": "2026-10-16T08:15:00Z",
         "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"}
    ]))
    .unwrap();
    app.update(Msg::Runs(Ok(rows)));
    let text = screen(&app, 140, 30);
    let linked = row_with(&text, NEW);
    assert!(linked.contains('↳'), "{text}");
    let source = row_with(&text, RUN);
    assert!(!source.contains('↳'), "{text}");
    // The note under the table names the run the selected row continues (the newest row is selected).
    assert!(text.contains(&format!("continues run {RUN}")), "{text}");
}

// ---- the NAME cell of a run that continues another -----------------------------------------------------------------------------

/// Where the NAME cell starts at 80 and 100 columns: the border, the gutter, a gap, RUN (28), a gap, STATUS (14) and a gap.
const NAME_X: u16 = 1 + 1 + 1 + 28 + 1 + 14 + 1;
const FIRST_Y: u16 = 3;
const OTHER: &str = "01K6A0000000000000000000R8";

/// The Runs list with one run, `NEW`, that continues `OTHER` and has `name`, and the run it continues; the filter `word` is typed.
fn filtered_linked_runs(name: &str, word: &str) -> App {
    let summary = |id: &str, from: Option<&str>| -> RunSummary {
        let mut v = json!({"runId": id, "status": "complete", "phase": "planning", "target": {"mode": "s3-only"},
            "createdAt": "2026-10-20T12:00:00Z", "ownerPrincipalArn": "arn:aws:sts::111122223333:assumed-role/Admin/tester"});
        if let Some(from) = from {
            v["continueFrom"] = json!(from);
        }
        serde_json::from_value(v).unwrap()
    };
    let entry = |id: &str, name: &str| IndexEntry {
        run_id: id.into(),
        title: None,
        model: None,
        name: Some(name.into()),
        name_source: Some("ai".into()),
        tags: None,
    };
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.update(Msg::RunsIndexed(Ok(IndexedRead {
        read: RunsRead::All(vec![summary(NEW, Some(OTHER)), summary(OTHER, None)]),
        index: vec![entry(NEW, name), entry(OTHER, "Something else")],
    })));
    app.handle_key(key(KeyCode::Char('/')));
    for c in word.chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
    app
}

fn buffer(app: &App, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    t.backend().buffer().clone()
}

fn underlined(buf: &Buffer, y: u16) -> Vec<u16> {
    (0..buf.area.width)
        .filter(|&x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
        .collect()
}

#[test]
fn the_filter_underlines_the_characters_it_found_after_the_mark_of_a_linked_run() {
    // "↳ alpha beta": the mark and a blank come first, so `beta` starts at the eighth cell of the NAME cell and not at the sixth.
    let app = filtered_linked_runs("alpha beta", "beta");
    let buf = buffer(&app, 100, 24);
    let row: String = (0..100).map(|x| buf[(x, FIRST_Y)].symbol()).collect();
    assert!(row.contains(NEW), "the linked run is the first row: {row}");
    assert_eq!(
        (
            buf[(NAME_X, FIRST_Y)].symbol(),
            buf[(NAME_X + 2, FIRST_Y)].symbol()
        ),
        ("\u{21b3}", "a"),
        "the mark comes first, then the name: {row}"
    );
    assert_eq!(
        underlined(&buf, FIRST_Y),
        (NAME_X + 8..NAME_X + 12).collect::<Vec<u16>>(),
        "only the four characters of `beta`: {row}"
    );
}

#[test]
fn a_match_that_straddles_the_cut_of_a_linked_run_never_underlines_the_ellipsis() {
    // At 80 columns the NAME cell is 32 wide: the mark and a blank, 29 characters of the name, and the ellipsis. `list` starts at the 29th
    // character of the name, so its first letter is the last one drawn and the ellipsis follows it.
    let name = "Add pagination to the order list endpoint with cursor tokens";
    let app = filtered_linked_runs(name, "list");
    let buf = buffer(&app, 80, 24);
    let row: String = (0..80).map(|x| buf[(x, FIRST_Y)].symbol()).collect();
    assert_eq!(
        buf[(NAME_X + 31, FIRST_Y)].symbol(),
        "\u{2026}",
        "the cell is cut with an ellipsis: {row}"
    );
    assert_eq!(
        underlined(&buf, FIRST_Y),
        vec![NAME_X + 30],
        "the `l` of `list` is the one character drawn, and the ellipsis is not underlined: {row}"
    );
}
