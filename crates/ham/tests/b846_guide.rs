//! Two sentences of the guide that the live proof found untrue on release 5.21, held to what the product does:
//!
//! - the card `retry did not start` (and `follow-up did not start`, `resume did not start`) is kept by the terminal app that saw the
//!   refusal, in memory: the client keeps no state of its own and the service records no start that it refused, so a newly opened app
//!   shows no such card. The guide said the card stays until a later retry starts.
#![allow(clippy::unwrap_used)]

mod chatfx;

use chatfx::*;
use ham::tui::ext::Done;
use ham::tui::{App, Msg};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use serde_json::{Value, json};

fn guide() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/user-guide.md");
    std::fs::read_to_string(path).unwrap()
}

/// The text of the section under `heading` (a line `## heading` or deeper), up to the next heading of the same or a higher level, as one
/// line of words.
fn section(guide: &str, heading: &str) -> String {
    let mut out = String::new();
    let mut level = 0;
    for line in guide.lines() {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        let title = line.trim_start_matches('#').trim();
        if level == 0 {
            if hashes > 0 && title == heading {
                level = hashes;
            }
            continue;
        }
        if hashes > 0 && hashes <= level && line.as_bytes().get(hashes) == Some(&b' ') {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(level > 0, "the guide has no section {heading:?}");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// `RUN` ended blocked: what `mark-blocked` writes after the usual events.
fn blocked_events() -> Vec<Envelope> {
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
    events
}

fn flat(text: &str) -> String {
    text.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const UPDATING: &str =
    "This stage is being updated, so it starts no new runs. Try again in a few minutes.";

// ---- 4: the card of a start that did not start is the app's, in memory ---------------------------------------------------------------

#[test]
fn b846_the_card_of_a_retry_that_did_not_start_is_kept_by_the_app_that_saw_it_and_by_no_other() {
    // The app that asked: the blocked run keeps the reason as a card.
    let mut first = ready(blocked_events());
    first.update(Msg::Ext(Done::RetryStarted {
        run_id: RUN.into(),
        result: Err(UPDATING.into()),
    }));
    let text = flat(&screen(&first, 120, 40));
    assert!(text.contains("retry did not start"), "{text}");
    assert!(text.contains(UPDATING), "{text}");
    // An app opened afterwards reads the same run from the service: the events hold no word of the refused retry, so it has no card.
    let second: App = ready(blocked_events());
    let text = flat(&screen(&second, 120, 40));
    assert!(text.contains("blocked · r retry"), "{text}");
    assert!(!text.contains("retry did not start"), "{text}");
    assert!(!text.contains(UPDATING), "{text}");
}

#[test]
fn b846_the_guide_says_the_cards_of_a_start_that_did_not_start_are_gone_when_ham_is_opened_again() {
    let guide = guide();
    let messages = section(&guide, "Messages to a working Cline");
    assert!(
        messages.contains(
            "The card stays until a later retry of the run starts, for as long as ham stays open."
        ),
        "the retry card: {messages}"
    );
    assert!(
        messages.contains("the card is gone when you open ham again"),
        "the retry card: {messages}"
    );
    assert!(
        messages.contains("Like the card of a retry, it is kept while ham stays open"),
        "the follow-up card: {messages}"
    );
    let resume = section(&guide, "Long runs, paused runs and resume");
    assert!(
        resume.contains("until a later resume of it starts or you close ham"),
        "the resume card: {resume}"
    );
    // No sentence is left that promises a card for longer than the app runs.
    for text in [&messages, &resume] {
        assert!(
            !text.contains("The card stays until a later retry of the run starts. "),
            "{text}"
        );
        assert!(
            !text.contains("until a later resume of it starts. "),
            "{text}"
        );
    }
}

// ---- 1: the keys of the retry offer on a short screen -------------------------------------------------------------------------------

#[test]
fn b846_the_guide_says_the_keys_of_the_retry_offer_stay_in_view_on_a_short_screen() {
    // The line the guide quotes is the last line of the offer as the product words it.
    let keys = "Enter starts it · Esc cancels";
    assert_eq!(
        ham_core::retry::confirmation(RUN, None, false)
            .lines()
            .last(),
        Some(keys)
    );
    let messages = section(&guide(), "Messages to a working Cline");
    assert!(
        messages.contains(&format!(
            "On a screen too short for the whole panel, the last row of the panel still reads \"{keys}\""
        )),
        "{messages}"
    );
    assert!(
        messages.contains("PgUp and PgDn scroll the sentences above that row and keep the offer"),
        "{messages}"
    );
}

// ---- 3: what `R` resumes on the screen of a paused run -------------------------------------------------------------------------------

#[test]
fn b846_the_guide_says_that_r_on_a_runs_screen_resumes_every_paused_and_stalled_run() {
    // The sentences the guide quotes are the ones `crates/ham/tests/b846_ended_run_screens.rs` reads off the screen.
    let guide = guide();
    let resume = section(&guide, "Long runs, paused runs and resume");
    for words in [
        "On the screen of a run, `R` shows the preview of `/resume --all`, and `R` again starts it: it resumes every paused and stalled run in your list that no later run continues, and never the run on screen alone.",
        "The preview names how many runs that is before anything starts.",
        "When that run is the only one `R` would resume, its box title reads \"Not available: run paused; Esc, then R resumes it\" and its browse key line reads `R resume`.",
        "the title ends \"Esc, then R resumes 3 runs\" and the key line reads `R resume all`",
        "the title ends \"Esc, then R resumes all\"",
        "To resume that run alone, type `/resume <runId>` in its box or run `ham resume <runId>`.",
    ] {
        assert!(resume.contains(words), "{words}: {resume}");
    }
    // The sentence that named the key without saying what it resumes is gone.
    assert!(
        !resume
            .contains("The screen of a paused or stalled run names the key: its box title reads"),
        "{resume}"
    );
    let messages = section(&guide, "Messages to a working Cline");
    for words in [
        "`R` on the screen of a run is `/resume --all`, so these words name that run alone only when it is the only paused or stalled run `R` would resume.",
        "`c` ends its sentence with \"R resumes 3 runs.\" and the key line reads `R resume all`",
        "\"This run ended stalled: nothing to cancel. R resumes 123 runs.\"",
    ] {
        assert!(messages.contains(words), "{words}: {messages}");
    }
    let whole = guide.split_whitespace().collect::<Vec<_>>().join(" ");
    for words in [
        "it ends \"R resumes 3 runs\" when `R` would resume three runs, and \"R resumes all\" when ham has not read your list of runs or the count has four digits or more",
        "and `R resume` on a paused or stalled run that `R` resumes, or `R resume all` when `R` would resume more runs than that one.",
    ] {
        assert!(whole.contains(words), "{words}");
    }
}

// ---- 3: the `PR` row of the status block ---------------------------------------------------------------------------------------------

#[test]
fn b846_the_guide_says_what_the_pr_row_reads_while_a_run_works_and_once_it_has_ended() {
    // `crates/ham/tests/b846_ended_run_screens.rs` reads the same rows off the screen: a zip-only run that still works says that it
    // delivers a zip once ham has read its target.
    let messages = section(&guide(), "Messages to a working Cline");
    for words in [
        "The status block of a run that works reads \"PR none yet\" when the run targets a repository, and \"PR none: this run delivers a zip\" for a zip-only run, which opens no pull request.",
        "Until ham has read the run's target, the row reads \"PR none yet\".",
        "Once the run has ended without a pull request, the block reads \"PR none: this run delivered a zip\", or \"PR none\" for a run that delivered nothing.",
    ] {
        assert!(messages.contains(words), "{words}: {messages}");
    }
    // The guide no longer says that the row knows whether the run can open a pull request, nor that a zip-only run reads "none yet"
    // for as long as it works.
    for gone in [
        "only while the run can still open a pull request",
        "whatever its target: a zip-only run, which opens no pull request, reads it too",
    ] {
        assert!(!messages.contains(gone), "{gone}: {messages}");
    }
}

// ---- 7: how much of a run's total time is work time ----------------------------------------------------------------------------------

/// Seconds as the guide writes a long time: `7 hours 45 minutes`.
fn hours_minutes(seconds: u32) -> String {
    format!("{} hours {} minutes", seconds / 3600, seconds % 3600 / 60)
}

/// A number with the thousands separator the guide uses: `27,900`.
fn grouped(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[test]
fn b846_the_guide_says_how_much_of_the_total_time_is_work_time_in_the_codes_own_numbers() {
    use harness_protocol::direct::{
        DEFAULT_MAX_RUN_S, MAX_DIRECT_TIMEOUT_S, MIN_SLICE_ENGINE_S, SLICE_RESERVE_S,
    };
    let text = section(&guide(), "Long runs, paused runs and resume");
    // Every session keeps its reserve for the end: saving the work and delivering it.
    let reserve_min = SLICE_RESERVE_S / 60;
    assert!(
        text.contains(&format!(
            "keeps its last {reserve_min} minutes ({} seconds) to save the work and deliver it",
            grouped(SLICE_RESERVE_S)
        )),
        "{text}"
    );
    // The longest one session works.
    assert!(
        text.contains(&format!(
            "at most {} ({} seconds) in one session",
            hours_minutes(MAX_DIRECT_TIMEOUT_S),
            grouped(MAX_DIRECT_TIMEOUT_S)
        )),
        "{text}"
    );
    // One hour in all: the run of the proof had 2,700 seconds and was paused 45 minutes after its start. The guide gives it in minutes:
    // the documents check takes a number of seconds for a limit of the code, and this one is a difference of two.
    let hour = 3600 - SLICE_RESERVE_S;
    assert!(
        text.contains(&format!(
            "With `--max-run-hours 1`, Cline works {} minutes, and the run is paused about {} minutes after it started",
            hour / 60,
            hour / 60
        )),
        "{text}"
    );
    // The default: as many whole sessions as the total holds, each with its reserve.
    let sessions = DEFAULT_MAX_RUN_S / (MAX_DIRECT_TIMEOUT_S + SLICE_RESERVE_S);
    assert!(
        text.contains(&format!(
            "With the default of {} hours, Cline works up to {} in {sessions} full sessions",
            DEFAULT_MAX_RUN_S / 3600,
            hours_minutes(sessions * MAX_DIRECT_TIMEOUT_S)
        )),
        "{text}"
    );
    // What the full sessions leave is less than a session: one more starts only when the least time for one is still left.
    assert!(
        text.contains(&format!(
            "a short fourth session follows only when {} minutes or more are still left",
            (SLICE_RESERVE_S + MIN_SLICE_ENGINE_S) / 60
        )),
        "{text}"
    );
    // The least time a new session is started for: its own reserve and the least work worth a session.
    assert!(
        text.contains(&format!(
            "a run with less than {} minutes left is paused",
            (SLICE_RESERVE_S + MIN_SLICE_ENGINE_S) / 60
        )),
        "{text}"
    );
}

#[test]
fn b846_the_numbers_the_guide_gives_are_the_ones_of_this_release() {
    // The sentence above is built from the constants; this pins what a reader sees today, so a changed constant is a changed guide.
    let text = section(&guide(), "Long runs, paused runs and resume");
    for words in [
        "keeps its last 15 minutes (900 seconds)",
        "at most 7 hours 45 minutes (27,900 seconds) in one session",
        "Cline works 45 minutes, and the run is paused about 45 minutes after it started",
        "Cline works up to 23 hours 15 minutes in 3 full sessions",
        "a short fourth session follows only when 30 minutes or more are still left",
        "less than 30 minutes left is paused",
    ] {
        assert!(text.contains(words), "{words}: {text}");
    }
}
