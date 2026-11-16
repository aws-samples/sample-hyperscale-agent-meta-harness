//! The small points the live proof found on the screens of a blocked and of a paused run. Each
//! is a sentence the screen cut, or a thing the screen said that was not true for the run on it:
//!
//! - the offer "Retry blocked run?" keeps its sentences whole: they wrap, at 80 and at 120 columns, and a screen too short for them
//!   scrolls the offer without withdrawing it and keeps the line that names its keys as the last row of the panel;
//! - the typing key line of a blocked run names the retry whole at 80 columns ("Esc browse (r retry)") and never ends "Esc, then…";
//! - the notice of a retry that did not start says "your text is kept" only when the box holds a text: after `/retry` the typed line was
//!   the command itself;
//! - a run that has ended offers no cancel: not in the title of its box, not on its key lines, not in the command list, and `c` says
//!   that the run has ended. A run that ended paused or stalled names `R`, the key that resumes it, and what `R` resumes: `R` is
//!   `/resume --all`. The status block of a run that has ended does not say "PR none yet"; a run that works says it unless the screen
//!   knows that it is a zip-only run, which says "PR none: this run delivers a zip".
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use crossterm::event::KeyCode;
use ham::tui::ext::{Done, Job};
use ham::tui::{App, Effect, Msg};
use harness_protocol::api::v1::{EventsPage, RunDetail};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

const REASON: &str = "bundle validation failed: buildspec buildspec.yml: buildspec.yml is the build specification that the pull request check runs; a delivery to a repository refuses the change (M).";

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

/// `RUN` ended blocked: what `mark-blocked` writes after the usual events.
fn blocked_events() -> Vec<Envelope> {
    let mut events = base_events();
    events.extend([
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
    ]);
    events
}

/// The detail of `RUN` with `status` and `target` (the service's own words for a target: `{"mode": "repo", "repo": "cc:<name>"}`).
fn detail(status: &str, target: Value) -> RunDetail {
    serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": status, "phase": "delivering", "target": target,
                    "createdAt": "2026-10-16T08:15:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/A/b"},
        "objective": "Add a comment line to the root buildspec.yml", "tokensUsed": 0, "costUsd": 0.0
    }))
    .unwrap()
}

/// The blocked run `RUN` on screen in typing mode; its detail names a repository target, as the run of the proof did.
fn blocked() -> App {
    let mut app = ready(blocked_events());
    app.update(Msg::Detail(Ok(detail(
        "blocked",
        json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
    ))));
    app
}

/// The screen as one line of words, the side borders of its boxes taken out.
fn flat(text: &str) -> String {
    text.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The rows of the box whose top border holds `title`, without its borders.
fn box_rows(text: &str, title: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let top = lines
        .iter()
        .position(|l| l.starts_with('┌') && l.contains(title))
        .unwrap_or_else(|| panic!("no box titled {title:?}:\n{text}"));
    lines[top + 1..]
        .iter()
        .take_while(|l| l.starts_with('│'))
        .map(|l| l.trim_matches('│').trim_end().to_string())
        .collect()
}

/// The bottom border of the box whose top border holds `title`: the row that says when the box holds more than it shows.
fn box_bottom(text: &str, title: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let top = lines
        .iter()
        .position(|l| l.starts_with('┌') && l.contains(title))
        .unwrap_or_else(|| panic!("no box titled {title:?}:\n{text}"));
    lines[top + 1..]
        .iter()
        .find(|l| l.starts_with('└'))
        .unwrap_or_else(|| panic!("the box titled {title:?} has no bottom border:\n{text}"))
        .to_string()
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

/// The three ways the offer comes up: `/retry`, Enter on a typed text, and `r` in browse mode. `true` when the box then holds a text.
fn offer(app: &mut App, clock: &mut Clock, way: &str) -> bool {
    match way {
        "/retry" => {
            send(app, clock, "/retry");
            false
        }
        "text" => {
            send(app, clock, "Keep this text in the box.");
            true
        }
        _ => {
            press(app, clock, KeyCode::Esc, 50);
            press(app, clock, KeyCode::Char('r'), 50);
            false
        }
    }
}

const WAYS: [&str; 3] = ["/retry", "text", "r"];

// ---- 1: the offer of the retry keeps its sentences whole ----------------------------------------------------------------------------

#[test]
fn b846_the_offer_of_the_retry_wraps_its_sentences_at_80_and_at_120_columns() {
    for (w, h) in [(80, 30), (120, 30), (120, 40)] {
        for way in WAYS {
            let mut app = blocked();
            let mut clock = Clock::new();
            let kept = offer(&mut app, &mut clock, way);
            let text = screen(&app, w, h);
            let rows = box_rows(&text, "Retry blocked run?");
            assert!(
                rows.iter().all(|r| !r.contains('…')),
                "{way} {w}x{h}: a line of the offer is cut:\n{text}"
            );
            // Every sentence of the offer, in its order: the run by its whole id, the target, the work, the text, the keys.
            let whole = flat(&ham_core::retry::confirmation_with(
                RUN,
                Some("cc:hamh-e2e-python-calc"),
                kept,
                None,
            ));
            assert!(
                flat(&rows.join("\n")).contains(&whole),
                "{way} {w}x{h}: the offer is not whole:\n{text}"
            );
        }
    }
}

/// The line of the offer that names its keys: the key that starts a billable run and the key that withdraws the offer.
const OFFER_KEYS: &str = "Enter starts it · Esc cancels";

#[test]
fn b846_a_screen_too_short_for_the_offer_keeps_its_keys_in_view_and_scrolls_the_rest() {
    // 80x24 and 80x20: the panel may take two fifths of the screen, which is less than the wrapped offer. The line that names the key
    // that starts a billable run stays the last row of the panel, before any scrolling and on every page; the border says that there is
    // more, and the page keys bring every other sentence into view. They do not withdraw the offer, and the confirming key still starts
    // the retry.
    for (w, h) in [(80, 24), (80, 20)] {
        for way in WAYS {
            let what = format!("{way} {w}x{h}");
            let mut app = blocked();
            let mut clock = Clock::new();
            let kept = offer(&mut app, &mut clock, way);
            let first = screen(&app, w, h);
            let rows = box_rows(&first, "Retry blocked run?");
            assert!(
                rows.iter().all(|r| !r.contains('…')),
                "{what}: a line of the offer is cut:\n{first}"
            );
            // The offer is longer than its panel, and the panel says so.
            assert!(
                box_bottom(&first, "Retry blocked run?").contains("PgUp PgDn scroll"),
                "{what}:\n{first}"
            );
            assert_eq!(
                rows.last().map(|r| r.trim()),
                Some(OFFER_KEYS),
                "{what}: the keys of the offer are not in view before any scrolling:\n{first}"
            );
            let mut pages = vec![flat(&rows.join("\n"))];
            for _ in 0..6 {
                let effects = press(&mut app, &mut clock, KeyCode::PageDown, 400);
                assert!(effects.is_empty(), "{what}: {effects:?}");
                let text = screen(&app, w, h);
                let rows = box_rows(&text, "Retry blocked run?");
                assert_eq!(
                    rows.last().map(|r| r.trim()),
                    Some(OFFER_KEYS),
                    "{what}: a page key moved the keys of the offer out of view:\n{text}"
                );
                assert_eq!(
                    flat(&rows.join("\n")).matches(OFFER_KEYS).count(),
                    1,
                    "{what}: the keys of the offer are drawn once:\n{text}"
                );
                pages.push(flat(&rows.join("\n")));
            }
            // Every sentence of the offer came into view, whole, on one page or another.
            let whole = ham_core::retry::confirmation_with(
                RUN,
                Some("cc:hamh-e2e-python-calc"),
                kept,
                None,
            );
            for sentence in whole.lines() {
                assert!(
                    pages.iter().any(|p| p.contains(&flat(sentence))),
                    "{what}: {sentence:?} never came into view: {pages:#?}"
                );
            }
            press(&mut app, &mut clock, KeyCode::PageUp, 400);
            assert!(
                screen(&app, w, h).contains("Retry blocked run?"),
                "{what}: a page key withdrew the offer"
            );
            // The offer still waits for its key: Enter in typing mode, `r` in browse mode.
            let confirm = if way == "r" {
                KeyCode::Char('r')
            } else {
                KeyCode::Enter
            };
            let effects = press(&mut app, &mut clock, confirm, 400);
            assert_eq!(
                retry_jobs(&effects),
                vec![RUN.to_string()],
                "{what}: {effects:?}"
            );
        }
    }
}

#[test]
fn b846_the_keys_of_the_offer_are_its_last_row_where_the_whole_offer_fits() {
    // A screen with room for the whole offer shows it as before: every sentence in its order, the keys last, nothing to scroll.
    for (w, h) in [(80, 30), (120, 30), (120, 40)] {
        for way in WAYS {
            let mut app = blocked();
            let mut clock = Clock::new();
            offer(&mut app, &mut clock, way);
            let text = screen(&app, w, h);
            let rows = box_rows(&text, "Retry blocked run?");
            assert_eq!(
                rows.last().map(|r| r.trim()),
                Some(OFFER_KEYS),
                "{way} {w}x{h}:\n{text}"
            );
            assert_eq!(
                flat(&rows.join("\n")).matches(OFFER_KEYS).count(),
                1,
                "{way} {w}x{h}:\n{text}"
            );
            assert!(
                !box_bottom(&text, "Retry blocked run?").contains("scroll"),
                "{way} {w}x{h}: the whole offer fits, so its panel has nothing to scroll:\n{text}"
            );
        }
    }
}

// ---- 2: the typing key line of a blocked run is whole at 80 columns ------------------------------------------------------------------

/// The key line: the last row of the screen with text.
fn key_line(app: &App, w: u16, h: u16) -> String {
    screen(app, w, h)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .trim_end()
        .to_string()
}

#[test]
fn b846_the_typing_key_line_of_a_blocked_run_names_the_retry_whole_at_80_columns() {
    let app = blocked();
    // 80 columns have no room for the ending "Esc, then r retry" (the line would be 87 wide): the retry is named inside the Esc part.
    let short = "Enter send · Ctrl-J new line · Esc browse (r retry) · Ctrl-C clear, then quit";
    assert_eq!(key_line(&app, 80, 30), short);
    // The short form is used from the width it needs up to the last width the ending does not fit.
    assert_eq!(short.chars().count(), 77);
    assert_eq!(key_line(&app, 77, 30), short);
    assert_eq!(key_line(&app, 86, 30), short);
    let ending =
        "Enter send · Ctrl-J new line · Esc browse · Ctrl-C clear, then quit · Esc, then r retry";
    assert_eq!(ending.chars().count(), 87);
    assert_eq!(key_line(&app, 87, 30), ending);
    // At every width from there on the line is whole, names the retry once and never a cancel.
    for w in 77..=200 {
        let line = key_line(&app, w, 30);
        assert!(!line.contains('…'), "{w}: {line}");
        assert_eq!(line.matches("r retry").count(), 1, "{w}: {line}");
        assert!(!line.contains("cancel"), "{w}: {line}");
        assert!(line.chars().count() <= usize::from(w), "{w}: {line}");
    }
}

#[test]
fn b846_the_short_key_line_keeps_the_way_to_the_retry_true() {
    // The line says Esc, then r: both keys do what it says at 80 columns.
    let mut app = blocked();
    let mut clock = Clock::new();
    assert!(key_line(&app, 80, 30).contains("Esc browse (r retry)"));
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    press(&mut app, &mut clock, KeyCode::Char('r'), 50);
    assert!(screen(&app, 80, 30).contains("Retry blocked run?"));
    let effects = press(&mut app, &mut clock, KeyCode::Char('r'), 400);
    assert_eq!(retry_jobs(&effects), vec![RUN.to_string()], "{effects:?}");
}

// ---- 6: "your text is kept" is said only when a text is kept -----------------------------------------------------------------------

/// The one sentence a stage that is being updated answers a start with.
const UPDATING: &str =
    "This stage is being updated, so it starts no new runs. Try again in a few minutes.";

/// The offer by `way`, its confirming key, and the service's refusal `why`.
fn refused_retry(way: &str, why: &str) -> App {
    let mut app = blocked();
    let mut clock = Clock::new();
    offer(&mut app, &mut clock, way);
    let confirm = if way == "r" {
        KeyCode::Char('r')
    } else {
        KeyCode::Enter
    };
    let effects = press(&mut app, &mut clock, confirm, 400);
    assert_eq!(
        retry_jobs(&effects),
        vec![RUN.to_string()],
        "{way}: {effects:?}"
    );
    app.update(Msg::Ext(Done::RetryStarted {
        run_id: RUN.into(),
        result: Err(why.into()),
    }));
    app
}

#[test]
fn b846_a_retry_that_did_not_start_calls_the_text_kept_only_when_the_box_holds_one() {
    for (w, h) in [(80, 30), (120, 30)] {
        // `/retry` and `r`: the box is empty (the typed line was the command), so nothing is called kept.
        for way in ["/retry", "r"] {
            let app = refused_retry(way, UPDATING);
            assert_eq!(app.chat_text().as_deref(), Some(""), "{way}");
            let text = flat(&screen(&app, w, h));
            assert!(
                text.contains("the retry did not start (the card above says why)"),
                "{way} {w}x{h}: {text}"
            );
            assert!(!text.contains("your text is kept"), "{way} {w}x{h}: {text}");
            // The card has the reason, whole.
            assert!(
                text.contains("retry did not start"),
                "{way} {w}x{h}: {text}"
            );
            assert!(text.contains(UPDATING), "{way} {w}x{h}: {text}");
        }
        // Enter on a typed text: the text is still in the box, and the notice says so.
        let app = refused_retry("text", UPDATING);
        assert_eq!(
            app.chat_text().as_deref(),
            Some("Keep this text in the box.")
        );
        let text = flat(&screen(&app, w, h));
        assert!(
            text.contains("the retry did not start (the card above says why); your text is kept"),
            "{w}x{h}: {text}"
        );
    }
}

#[test]
fn b846_a_short_reason_stays_whole_on_the_notice_row_with_and_without_a_kept_text() {
    let why = "the service is busy";
    let empty = flat(&screen(&refused_retry("/retry", why), 80, 30));
    assert!(
        empty.contains("the retry did not start: the service is busy"),
        "{empty}"
    );
    assert!(!empty.contains("your text is kept"), "{empty}");
    let kept = flat(&screen(&refused_retry("text", why), 80, 30));
    assert!(
        kept.contains("the retry did not start: the service is busy; your text is kept"),
        "{kept}"
    );
}

// ---- 3: a run that has ended offers no cancel, and its status block does not wait for a pull request ---------------------------------

/// `RUN` stopped with `status` (`paused` or `stalled`) and delivered its zip: what the service writes after the usual events.
fn stopped_events(status: &str, label: &str) -> Vec<Envelope> {
    let mut events = base_events();
    events.extend([
        harness_event(
            20,
            EventType::GoalStatus,
            json!({"status": status, "phase": "delivering"}),
        ),
        harness_event(
            21,
            EventType::DeliveryReady,
            json!({"artifactId": "01M5A2R7K3C9XJ4D8E6F0000AA", "sha256": "ab".repeat(32), "sizeBytes": 1234}),
        ),
        harness_event(
            22,
            EventType::RunCompleted,
            json!({"status": status, "outcome": "stopped_timeout", "label": label}),
        ),
    ]);
    events
}

/// `RUN` cleared: its execution was stopped before it delivered anything.
fn cleared_events() -> Vec<Envelope> {
    let mut events = base_events();
    events.push(harness_event(
        20,
        EventType::RunAborted,
        json!({"status": "ABORTED", "cause": "stopped by the user"}),
    ));
    events
}

/// A row of the Runs list for `RUN`.
fn row(status: &str, extra: Value) -> Value {
    let mut v = json!({
        "runId": RUN, "status": status, "phase": "delivering", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-16T08:15:00Z", "ownerPrincipalArn": "arn:aws:iam::111122223333:role/test"
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn list(app: &mut App, rows: Vec<Value>) {
    let rows: Vec<harness_protocol::api::v1::RunSummary> =
        serde_json::from_value(Value::Array(rows)).unwrap();
    app.update(Msg::Runs(Ok(rows)));
}

/// `RUN` on screen, stopped with `status`, as the Runs list holds it: `R` would resume it.
fn stopped(status: &str) -> App {
    let label = if status == "paused" {
        "Cline stopped: the run reached its time limit"
    } else {
        "Cline stopped: the session was lost"
    };
    let mut app = ready(stopped_events(status, label));
    list(
        &mut app,
        vec![row(
            status,
            json!({"statusReason": "total_time: the run used the total time it was given", "resumeMode": "follow_up", "session": 1}),
        )],
    );
    app
}

fn title_row(text: &str) -> String {
    text.lines()
        .find(|l| l.contains("Message to Cline"))
        .unwrap_or_else(|| panic!("no message box:\n{text}"))
        .to_string()
}

/// No row of the screen offers a cancel: not the command, not the key, not the two presses.
fn assert_no_cancel_offer(text: &str, what: &str) {
    for words in ["/cancel", "c cancel", "c c cancel"] {
        assert!(
            !text.contains(words),
            "{what}: the screen offers {words:?}:\n{text}"
        );
    }
}

/// The screens of `app` in typing mode and in browse mode at one size.
fn both_modes(app: &mut App, w: u16, h: u16) -> (String, String) {
    let typing = screen(app, w, h);
    let mut clock = Clock::new();
    press(app, &mut clock, KeyCode::Esc, 50);
    let browse = screen(app, w, h);
    (typing, browse)
}

#[test]
fn b846_the_screen_of_a_run_that_ended_paused_or_stalled_offers_no_cancel_and_names_the_key_that_resumes_it()
 {
    for status in ["paused", "stalled"] {
        for (w, h) in [(80, 30), (120, 30)] {
            let mut app = stopped(status);
            let (typing, browse) = both_modes(&mut app, w, h);
            let what = format!("{status} {w}x{h}");
            assert_no_cancel_offer(&typing, &what);
            assert_no_cancel_offer(&browse, &what);
            // The box says why Enter does nothing here and what does something, whole at this width: in typing mode R is a letter.
            let title = title_row(&typing);
            assert!(
                title.contains(&format!(
                    "Not available: run {status}; Esc, then R resumes it"
                )),
                "{what}: {title}"
            );
            assert!(!title.contains('…'), "{what}: {title}");
            // Browse mode: `R resume` stands where `c cancel` stood.
            let keys = browse.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
            assert!(
                keys.contains("a artifacts · R resume · Esc runs"),
                "{what}: {keys}"
            );
        }
    }
}

#[test]
fn b846_c_on_a_run_that_ended_paused_says_so_and_shows_no_preview_and_r_resumes_it() {
    let mut app = stopped("paused");
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Esc, 50);
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 50);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app, 80, 30);
    assert!(
        !text.contains("press c again"),
        "no cancel preview:\n{text}"
    );
    assert!(
        text.contains("This run already ended (paused): there is nothing to cancel. R resumes it."),
        "{text}"
    );
    // The second `c` is nothing either.
    let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(effects.is_empty(), "{effects:?}");
    // What the line promises: R shows the preview of the resume, for this run.
    press(&mut app, &mut clock, KeyCode::Char('R'), 400);
    assert!(
        flat(&screen(&app, 80, 30)).contains("press R again to resume 1 paused or stalled run"),
        "{}",
        screen(&app, 80, 30)
    );
}

#[test]
fn b846_enter_in_the_box_of_a_run_that_ended_paused_sends_nothing_and_says_what_resumes_it() {
    let mut app = stopped("paused");
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "go on with the tests");
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("go on with the tests"));
    let text = screen(&app, 80, 30);
    assert!(
        text.contains("this run ended paused: Esc, then R resumes it. The text stays in the box."),
        "{text}"
    );
    assert_no_cancel_offer(&text, "after Enter");
}

// ---- what `R` does: it is `/resume --all`, and the screen of a paused run says how many runs that is ----------------------------------

/// `RUN` on screen, stopped with `status`, in a Runs list that holds `others` more runs `R` would resume (stalled and paused in turn),
/// a run that works and a run that is complete.
fn stopped_among(status: &str, others: usize) -> App {
    let label = if status == "paused" {
        "Cline stopped: the run reached its time limit"
    } else {
        "Cline stopped: the session was lost"
    };
    let mut app = ready(stopped_events(status, label));
    let reason = json!({"statusReason": "total_time: the run used the total time it was given", "resumeMode": "follow_up", "session": 1});
    let mut rows = vec![row(status, reason.clone())];
    for i in 0..others {
        let word = if i % 2 == 0 { "stalled" } else { "paused" };
        let mut other = row(word, reason.clone());
        other["runId"] = json!(format!("01K6B{i:021}"));
        rows.push(other);
    }
    for (id, word) in [
        ("01K6C000000000000000000001", "active"),
        ("01K6C000000000000000000002", "complete"),
    ] {
        let mut other = row(word, json!({}));
        other["runId"] = json!(id);
        rows.push(other);
    }
    list(&mut app, rows);
    app
}

/// The key line: the last row of `text` with text.
fn last_row(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .trim_end()
        .to_string()
}

#[test]
fn b846_the_screen_of_a_paused_run_says_how_many_runs_r_resumes_when_the_list_holds_more() {
    // `R` on a run's screen is `/resume --all`: with three paused and stalled runs in the list it starts three billable runs. The
    // title, the key lines, `c` and the preview say the same count, and none says that `R` resumes this run alone.
    for status in ["paused", "stalled"] {
        for (w, h) in [(80, 30), (120, 30)] {
            let mut app = stopped_among(status, 2);
            let (typing, browse) = both_modes(&mut app, w, h);
            let what = format!("{status} {w}x{h}");
            let title = title_row(&typing);
            assert!(
                title.contains(&format!(
                    "Not available: run {status}; Esc, then R resumes 3 runs"
                )),
                "{what}: {title}"
            );
            assert!(!title.contains('…'), "{what}: {title}");
            assert!(!typing.contains("resumes it"), "{what}: {typing}");
            // The key lines have no room for the count: they say that it is more than this run.
            if w >= 120 {
                assert!(
                    last_row(&typing).contains("a artifacts, R resume all · Ctrl-C"),
                    "{what}: {typing}"
                );
            }
            let keys = last_row(&browse);
            assert!(
                keys.contains("a artifacts · R resume all · Esc runs"),
                "{what}: {keys}"
            );
            assert!(
                keys.ends_with("q quit") && !keys.contains('…'),
                "{what}: the key line is cut: {keys}"
            );
            let mut clock = Clock::new();
            press(&mut app, &mut clock, KeyCode::Char('c'), 50);
            let text = screen(&app, w, h);
            assert!(
                text.contains(&format!(
                    "This run already ended ({status}): there is nothing to cancel. R resumes 3 runs."
                )),
                "{what}: {text}"
            );
            // What the words promise: the preview of the first `R` names the same three runs.
            press(&mut app, &mut clock, KeyCode::Char('R'), 400);
            assert!(
                flat(&screen(&app, w, h))
                    .contains("press R again to resume 3 paused or stalled runs"),
                "{what}: {}",
                screen(&app, w, h)
            );
        }
    }
}

#[test]
fn b846_enter_in_the_box_of_a_paused_run_names_the_count_of_the_runs_r_resumes() {
    let mut app = stopped_among("paused", 2);
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "go on with the tests");
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.chat_text().as_deref(), Some("go on with the tests"));
    let text = screen(&app, 80, 30);
    assert!(
        text.contains(
            "this run ended paused: Esc, then R resumes 3 runs. The text stays in the box."
        ),
        "{text}"
    );
}

#[test]
fn b846_a_paused_run_whose_list_was_not_read_says_that_r_resumes_all() {
    // The dashboard of a start command holds no Runs list: `ham resume --all` reads the list itself, so the screen cannot count and
    // does not say that `R` resumes this run alone.
    for status in ["paused", "stalled"] {
        for (w, h) in [(80, 30), (120, 30)] {
            let mut app = ready(stopped_events(
                status,
                "Cline stopped: the run reached its time limit",
            ));
            let (typing, browse) = both_modes(&mut app, w, h);
            let what = format!("{status} {w}x{h}");
            let title = title_row(&typing);
            assert!(
                title.contains(&format!(
                    "Not available: run {status}; Esc, then R resumes all"
                )),
                "{what}: {title}"
            );
            assert!(!title.contains('…'), "{what}: {title}");
            assert!(!typing.contains("resumes it"), "{what}: {typing}");
            let keys = last_row(&browse);
            assert!(
                keys.contains("a artifacts · R resume all · Esc runs"),
                "{what}: {keys}"
            );
            assert!(
                keys.ends_with("q quit") && !keys.contains('…'),
                "{what}: the key line is cut: {keys}"
            );
            let mut clock = Clock::new();
            press(&mut app, &mut clock, KeyCode::Char('c'), 50);
            let text = screen(&app, w, h);
            assert!(
                text.contains(&format!(
                    "This run already ended ({status}): there is nothing to cancel. R resumes all."
                )),
                "{what}: {text}"
            );
            press(&mut app, &mut clock, KeyCode::Char('R'), 400);
            assert!(
                flat(&screen(&app, w, h))
                    .contains("press R again to resume every paused and stalled run"),
                "{what}: {}",
                screen(&app, w, h)
            );
        }
    }
}

// ---- a Runs list that is older than what the run's own events say ----------------------------------------------------------------------

/// `events` as one read of the run's pages, with the effects the screen asks for.
fn read_pages(app: &mut App, events: Vec<Envelope>) -> Vec<Effect> {
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    app.update(Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect())))
}

const TIME_USED: &str = "total_time: the run used the total time it was given";

/// `RUN` on screen while it works, with the Runs list as it was read then: the run's row says `active`, and `others` more runs are
/// paused. The run then stops with `status` while its screen stays open; the effects of that read of its events are returned too.
fn stopped_while_watched(status: &str, others: usize) -> (App, Vec<Effect>) {
    let mut app = ready(base_events());
    let mut rows = vec![row("active", json!({"phase": "green", "session": 1}))];
    for i in 0..others {
        let mut other = row(
            "paused",
            json!({"statusReason": TIME_USED, "resumeMode": "follow_up", "session": 1}),
        );
        other["runId"] = json!(format!("01K6B{i:021}"));
        rows.push(other);
    }
    list(&mut app, rows);
    let label = if status == "paused" {
        "Cline stopped: the run reached its time limit"
    } else {
        "Cline stopped: the session was lost"
    };
    let stop = stopped_events(status, label).split_off(base_events().len());
    let effects = read_pages(&mut app, stop);
    (app, effects)
}

fn reads_the_list(effects: &[Effect]) -> bool {
    effects.iter().any(|e| matches!(e, Effect::RefreshRuns(_)))
}

#[test]
fn b846_a_run_that_stops_while_its_screen_is_open_names_r_and_reads_the_list_again() {
    // The list was read while the run worked, so its row still says `active`. `ham resume --all` reads the list itself: the screen
    // names `R`, takes no count from a list that is older than the run's own events, and asks for the list again.
    for status in ["paused", "stalled"] {
        let (mut app, effects) = stopped_while_watched(status, 0);
        assert!(reads_the_list(&effects), "{status}: {effects:?}");
        let (typing, browse) = both_modes(&mut app, 80, 30);
        let title = title_row(&typing);
        assert!(
            title.contains(&format!(
                "Not available: run {status}; Esc, then R resumes all"
            )),
            "{status}: {title}"
        );
        assert!(
            last_row(&browse).contains("a artifacts · R resume all · Esc runs"),
            "{status}: {browse}"
        );
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Char('c'), 50);
        let text = screen(&app, 80, 30);
        assert!(
            text.contains(&format!(
                "This run already ended ({status}): there is nothing to cancel. R resumes all."
            )),
            "{status}: {text}"
        );
        // What the words promise: `R` shows the preview of the resume and refuses nothing.
        press(&mut app, &mut clock, KeyCode::Char('R'), 400);
        let text = screen(&app, 80, 30);
        assert!(
            !text.contains("no paused or stalled run to resume"),
            "{status}: {text}"
        );
        assert!(
            flat(&text).contains("press R again to resume every paused and stalled run"),
            "{status}: {text}"
        );
    }
}

#[test]
fn b846_a_list_older_than_the_run_on_screen_gives_no_count_of_the_runs_r_resumes() {
    // The held list names two other paused runs and this run as `active`. The command reads the list fresh and would resume
    // three: the preview does not say two, and no sentence says how many.
    let (mut app, _) = stopped_while_watched("paused", 2);
    let (typing, _) = both_modes(&mut app, 80, 30);
    assert!(
        title_row(&typing).contains("Not available: run paused; Esc, then R resumes all"),
        "{typing}"
    );
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('R'), 400);
    let text = flat(&screen(&app, 80, 30));
    assert!(
        text.contains("press R again to resume every paused and stalled run"),
        "{text}"
    );
    assert!(
        !text.contains("to resume 2 paused or stalled runs"),
        "{text}"
    );
}

#[test]
fn b846_the_list_read_after_the_run_stopped_gives_the_count_and_is_asked_for_once() {
    let (mut app, _) = stopped_while_watched("paused", 0);
    // A read that fails keeps the held list: the screen does not ask a second time for the same stop.
    let _ = app.update(Msg::RunsRead(Err("the service did not answer".to_string())));
    assert!(!reads_the_list(&read_pages(&mut app, vec![])));
    assert!(
        title_row(&screen(&app, 80, 30)).contains("Esc, then R resumes all"),
        "{}",
        screen(&app, 80, 30)
    );
    // The list as the service holds it now: the run is paused, and it is the only one `R` resumes.
    list(
        &mut app,
        vec![row(
            "paused",
            json!({"statusReason": TIME_USED, "resumeMode": "follow_up", "session": 1}),
        )],
    );
    let (typing, browse) = both_modes(&mut app, 80, 30);
    assert!(
        title_row(&typing).contains("Not available: run paused; Esc, then R resumes it"),
        "{typing}"
    );
    assert!(
        last_row(&browse).contains("a artifacts · R resume · Esc runs"),
        "{browse}"
    );
    assert!(!reads_the_list(&read_pages(&mut app, vec![])));
}

#[test]
fn b846_the_words_of_a_paused_run_stay_whole_at_80_columns_with_many_runs_to_resume() {
    // A long count gives way by rule, never to an ellipsis or to a cut at the edge: the title is whole, the sentence of `c` takes its
    // short form when the long one does not fit the one row of the notice, and a count of four digits reads "all".
    for status in ["paused", "stalled"] {
        // 123 runs: the title of the box has room for a count of three digits at 80 columns.
        let mut app = stopped_among(status, 122);
        let (typing, _) = both_modes(&mut app, 80, 30);
        let title = title_row(&typing);
        assert!(
            title.contains(&format!(
                "Not available: run {status}; Esc, then R resumes 123 runs"
            )),
            "{status}: {title}"
        );
        assert!(!title.contains('…'), "{status}: {title}");
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Char('c'), 50);
        let text = screen(&app, 80, 30);
        let said = text
            .lines()
            .find(|l| l.contains("R resumes 123 runs."))
            .unwrap_or_else(|| panic!("{status}: the sentence of c is cut or missing:\n{text}"));
        // The long sentence is 80 columns wide for a paused run and 81 for a stalled one, which takes the short form.
        let long = format!(
            "This run already ended ({status}): there is nothing to cancel. R resumes 123 runs."
        );
        let short = format!("This run ended {status}: nothing to cancel. R resumes 123 runs.");
        assert_eq!(long.chars().count() <= 80, status == "paused");
        assert_eq!(
            said.trim(),
            if status == "paused" { long } else { short },
            "{status}"
        );
        // Ninety-nine runs fit the long form at 80 columns for both.
        let mut app = stopped_among(status, 98);
        press(&mut app, &mut clock, KeyCode::Esc, 50);
        press(&mut app, &mut clock, KeyCode::Char('c'), 50);
        let text = screen(&app, 80, 30);
        assert!(
            text.contains(&format!(
                "This run already ended ({status}): there is nothing to cancel. R resumes 99 runs."
            )),
            "{status}: {text}"
        );
        // 1234 runs: a count of four digits would be cut at the edge of the title, so the words read "all", and the preview of the
        // first `R` still gives the count.
        let mut app = stopped_among(status, 1233);
        let (typing, _) = both_modes(&mut app, 80, 30);
        let title = title_row(&typing);
        assert!(
            title.contains(&format!(
                "Not available: run {status}; Esc, then R resumes all"
            )),
            "{status}: {title}"
        );
        assert!(!typing.contains("1234"), "{status}: {typing}");
        press(&mut app, &mut clock, KeyCode::Char('c'), 50);
        let text = screen(&app, 80, 30);
        assert!(
            text.contains(&format!(
                "This run already ended ({status}): there is nothing to cancel. R resumes all."
            )),
            "{status}: {text}"
        );
        press(&mut app, &mut clock, KeyCode::Char('R'), 400);
        assert!(
            flat(&screen(&app, 80, 30))
                .contains("press R again to resume 1234 paused or stalled runs"),
            "{status}: {}",
            screen(&app, 80, 30)
        );
    }
}

#[test]
fn b846_the_command_list_leaves_cancel_out_on_every_run_that_has_ended() {
    let popup = |mut app: App| {
        let mut clock = Clock::new();
        type_text(&mut app, &mut clock, "/can");
        screen(&app, 120, 40)
    };
    for (what, app) in [
        ("paused", stopped("paused")),
        ("stalled", stopped("stalled")),
        ("complete", ready(finished_events())),
        ("cleared", ready(cleared_events())),
        ("blocked", blocked()),
    ] {
        let text = popup(app);
        assert!(!text.contains("/cancel"), "{what}:\n{text}");
    }
    // A run that works still lists it.
    assert!(popup(ready(base_events())).contains("/cancel"));
}

#[test]
fn b846_a_run_that_a_later_run_continues_does_not_promise_that_r_resumes_it() {
    // The Runs list holds a run that continues this one: `R` leaves this run out, so the screen does not name the key.
    let mut app = ready(stopped_events(
        "paused",
        "Cline stopped: the run reached its time limit",
    ));
    let mut later = row("active", json!({"continueFrom": RUN}));
    later["runId"] = json!("01K6A0000000000000000000R2");
    list(
        &mut app,
        vec![
            later,
            row(
                "paused",
                json!({"statusReason": "total_time: the run used the total time it was given", "resumeMode": "follow_up", "session": 1}),
            ),
        ],
    );
    let (typing, browse) = both_modes(&mut app, 80, 30);
    assert_no_cancel_offer(&typing, "continued");
    assert_no_cancel_offer(&browse, "continued");
    assert!(
        title_row(&typing).contains("Not available: the run has ended"),
        "{typing}"
    );
    assert!(
        !typing.contains("R resume") && !browse.contains("R resume"),
        "{browse}"
    );
}

#[test]
fn b846_a_run_that_delivered_or_was_cleared_offers_no_cancel() {
    for (w, h) in [(80, 30), (120, 30)] {
        // Complete: the box prepares a follow-up, and no key line names a cancel.
        let mut app = ready(finished_events());
        let (typing, browse) = both_modes(&mut app, w, h);
        assert_no_cancel_offer(&typing, "complete");
        assert_no_cancel_offer(&browse, "complete");
        assert!(
            title_row(&typing).contains("Enter: Prepare follow-up"),
            "{typing}"
        );
        let keys = browse.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert!(
            keys.contains("a artifacts · Esc runs · n new run · q quit"),
            "{keys}"
        );
        let mut clock = Clock::new();
        let effects = press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        assert!(effects.is_empty(), "{effects:?}");
        let text = screen(&app, w, h);
        assert!(!text.contains("press c again"), "{text}");
        assert!(
            text.contains("This run already ended (complete): there is nothing to cancel."),
            "{text}"
        );
        // Cleared: nothing to follow up from, nothing to cancel.
        let mut app = ready(cleared_events());
        let (typing, browse) = both_modes(&mut app, w, h);
        assert_no_cancel_offer(&typing, "cleared");
        assert_no_cancel_offer(&browse, "cleared");
        assert!(
            title_row(&typing).contains("Not available: the run has ended"),
            "{typing}"
        );
        let mut clock = Clock::new();
        press(&mut app, &mut clock, KeyCode::Char('c'), 400);
        assert!(
            screen(&app, w, h)
                .contains("This run already ended (cleared): there is nothing to cancel."),
            "{}",
            screen(&app, w, h)
        );
    }
}

#[test]
fn b846_a_run_that_works_still_offers_the_cancel() {
    let mut app = ready(base_events());
    let (typing, browse) = both_modes(&mut app, 120, 30);
    assert!(typing.contains("c c cancel"), "{typing}");
    assert!(
        browse.contains("a artifacts · c cancel · Esc runs"),
        "{browse}"
    );
    let mut clock = Clock::new();
    press(&mut app, &mut clock, KeyCode::Char('c'), 400);
    assert!(screen(&app, 120, 30).contains("press c again to cancel run"));
}

/// The `PR` row of the status block.
fn pr_row(app: &App) -> String {
    screen(app, 120, 40)
        .lines()
        .find(|l| l.trim_start().starts_with("PR     "))
        .unwrap_or_else(|| panic!("no PR row:\n{}", screen(app, 120, 40)))
        .trim()
        .trim_end_matches('│')
        .trim()
        .to_string()
}

#[test]
fn b846_the_status_block_says_none_yet_while_the_run_works_and_none_once_it_has_ended() {
    // A run that works reads "none yet" while the screen holds no target of it (the row is read from the run's events, which name
    // none) and when it targets a repository. A zip-only run opens no pull request: once the screen holds its target, from the
    // run's detail or from its row of the Runs list, the row says that the run delivers a zip.
    assert_eq!(pr_row(&ready(base_events())), "PR     none yet");
    for (target, words) in [
        (
            json!({"mode": "s3-only"}),
            "PR     none: this run delivers a zip",
        ),
        (
            json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
            "PR     none yet",
        ),
        (
            json!({"mode": "new-repo", "repo": "cc:hamh-e2e-python-calc"}),
            "PR     none yet",
        ),
    ] {
        let mut app = ready(base_events());
        app.update(Msg::Detail(Ok(detail("active", target.clone()))));
        assert_eq!(pr_row(&app), words, "{target}");
    }
    // The row of the Runs list names the target too: the screen a list opens holds it before the run's detail is read.
    let mut app = ready(base_events());
    list(&mut app, vec![row("active", json!({}))]);
    assert_eq!(pr_row(&app), "PR     none: this run delivers a zip");
    // A run that ended with a zip and no pull request delivered what it delivers: paused, stalled or complete.
    for app in [
        stopped("paused"),
        stopped("stalled"),
        ready(finished_events()),
    ] {
        assert_eq!(pr_row(&app), "PR     none: this run delivered a zip");
    }
    // A run that ended without a delivery opens none.
    assert_eq!(pr_row(&blocked()), "PR     none");
    assert_eq!(pr_row(&ready(cleared_events())), "PR     none");
    // A pull request that was opened is named, whatever the status.
    let mut events = finished_events();
    events.push(harness_event(
        30,
        EventType::DeliveryPr,
        json!({"url": "https://git.example/pull/7"}),
    ));
    assert_eq!(pr_row(&ready(events)), "PR     https://git.example/pull/7");
}

// ---- the client lanes of 5.22 together: a paused run whose session was asked to end -----------------------------------------------

/// The worker's sentence when its session is asked to end and the copy of that moment cannot be saved (`agentd::direct`), as the
/// run's events carry it.
fn end_request_notice(seq: u64, ts: &str) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{seq:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: Some("direct".to_string()),
        seq,
        ts: ts.into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null,
                          "detail": "The session was asked to end, and a copy of the work could not be saved."}),
        },
    }
}

/// A run pauses when the time it was given is used: its session is asked to end and then the run ends paused. The screen of that
/// run says what the screen of a paused run says (what `R` resumes, no cancel) and names no session the run would be missing:
/// "session asked to end", "session silent" and "waiting for session" belong to a run that goes on.
#[test]
fn b852_b846_a_paused_run_whose_session_was_asked_to_end_names_r_and_no_cancel_and_no_missing_session()
 {
    let label = "Cline stopped: the run reached its time limit";
    let mut events = stopped_events("paused", label);
    let at = base_events().len();
    events.insert(at, end_request_notice(12, "2026-10-16T08:16:30Z"));
    for (w, h) in [(80, 30), (120, 30)] {
        let mut app = ready(events.clone());
        list(
            &mut app,
            vec![row(
                "paused",
                json!({"statusReason": "total_time: the run used the total time it was given", "resumeMode": "follow_up", "session": 1}),
            )],
        );
        // An hour later: 2026-10-16T09:30:00Z. A run that goes on would call its session silent by now.
        app.set_now(1_792_143_000);
        let _ = app.update(Msg::Pages(Ok(vec![])));
        let (typing, browse) = both_modes(&mut app, w, h);
        let what = format!("{w}x{h}");
        for text in [&typing, &browse] {
            assert_no_cancel_offer(text, &what);
            let words = flat(text);
            assert!(
                words.contains("paused"),
                "TRIGGER PRESENT: the paused run is on screen: {what}: {text}"
            );
            for gap in [
                "session asked to end",
                "session silent",
                "waiting for session",
            ] {
                assert!(!words.contains(gap), "{what}: {gap:?} on screen:\n{text}");
            }
            assert!(
                words.contains("PR none: this run delivered a zip"),
                "{what}: {text}"
            );
        }
        let title = title_row(&typing);
        assert!(
            title.contains("Not available: run paused; Esc, then R resumes it"),
            "{what}: {title}"
        );
        let keys = browse.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert!(keys.contains("R resume · Esc runs"), "{what}: {keys}");
    }
}
