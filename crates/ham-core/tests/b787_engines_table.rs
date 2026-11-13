//! The engine table of the client: which engines a person can choose, the name each prints, the engine a run is on (read from its own
//! `session.started` event) and the evidence heading each engine's last message sits under. More engines will come, so every test reads the table
//! and none repeats a list of names.

#![allow(clippy::unwrap_used)]

use ham_core::chat::final_message_from_evidence;
use ham_core::engines::{self, Capabilities, EngineState};
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

/// The harness writes `session.started`; an event of agent authority could not change what the run view says.
fn event(n: u32, ty: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": RUN, "agentId": "direct", "seq": n,
        "ts": "2026-10-16T08:15:00Z", "type": ty, "authority": "harness", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn started_on(engine: &str) -> RunState {
    let mut s = RunState::new(RUN);
    s.apply(event(
        1,
        "session.started",
        json!({"agentId": "direct", "engine": engine, "modelId": "m"}),
    ));
    s
}

#[test]
fn b787_cline_and_opencode_are_selectable_and_every_other_engine_says_it_is_not_available() {
    let ids: Vec<&str> = engines::available().iter().map(|e| e.id).collect();
    assert_eq!(
        ids,
        ["cline", "opencode"],
        "the roadmap order, available engines only"
    );
    assert_eq!(engines::default_engine().id, "cline");
    for e in engines::roadmap() {
        let selectable = engines::choose(e.id).is_ok();
        assert_eq!(selectable, e.state == EngineState::Available, "{e:?}");
    }
    assert!(
        engines::roadmap()
            .iter()
            .any(|e| e.state == EngineState::NotAvailable)
    );
    assert_eq!(engines::find("opencode").unwrap().name, "OpenCode");
    assert_eq!(engines::choices_text(), "cline, opencode");
}

#[test]
fn b787_no_engine_id_is_a_word_of_the_engine_command() {
    // `ham engine <name> <action>`: a name that is also an action word would make `ham engine show` ambiguous.
    for e in engines::roadmap() {
        assert!(
            !["init", "show", "check", "edit", "set", "unset", "import"].contains(&e.id),
            "{} is an action of `ham engine`",
            e.id
        );
        assert!(
            e.id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && e.id.len() <= 32,
            "{} is not a plain id",
            e.id
        );
    }
}

#[test]
fn b787_a_choice_is_read_without_regard_to_case_and_a_refusal_lists_the_available_engines() {
    assert_eq!(engines::choose(" OpenCode ").unwrap().id, "opencode");
    let e = engines::choose("pi").unwrap_err().to_string();
    assert_eq!(
        e,
        "pi is not available in this release; the engines are cline, opencode"
    );
    let e = engines::choose("gemini").unwrap_err().to_string();
    assert_eq!(
        e,
        "unknown engine \"gemini\"; the engines are cline, opencode"
    );
    // A word with a control character is shown escaped, never raw.
    let e = engines::choose("a\u{1b}[31mb").unwrap_err().to_string();
    assert!(!e.contains('\u{1b}'), "{e:?}");
}

#[test]
fn b787_say_for_puts_the_name_of_the_runs_engine_into_a_sentence() {
    let t = "{engine} took your message";
    assert_eq!(
        engines::say_for(Some("opencode"), t),
        "OpenCode took your message"
    );
    assert_eq!(
        engines::say_for(Some("cline"), t),
        "Cline took your message"
    );
    assert_eq!(
        engines::say_for(None, t),
        "Cline took your message",
        "no run yet: the default engine"
    );
    assert_eq!(
        engines::say_for(Some("unheard-of"), t),
        "Cline took your message",
        "an engine this build lacks: the default engine"
    );
    assert_eq!(engines::say(t), engines::say_for(None, t));
    assert_eq!(
        engines::find("opencode").unwrap().say(t),
        "OpenCode took your message"
    );
}

#[test]
fn b787_the_engine_of_a_run_is_the_one_its_session_started_event_names() {
    let on = |id: &str| engines::of_view(&started_on(id).view()).id;
    assert_eq!(on("opencode"), "opencode");
    assert_eq!(on("cline"), "cline");
    assert_eq!(on("OpenCode"), "opencode", "an id is read in lower case");
    assert_eq!(
        on("unheard-of"),
        "cline",
        "an engine this build lacks reads as the default engine"
    );
    assert_eq!(
        engines::of_view(&RunState::new(RUN).view()).id,
        "cline",
        "a run with no session yet"
    );
}

#[test]
fn b787_the_last_message_sits_under_one_heading_per_engine_and_both_parse() {
    let quote = |heading: &str| {
        format!("# Run\n\n{heading}\n\n> All done.\n> Two files changed.\n\n## Delivery\n\nzip\n")
    };
    for e in engines::roadmap() {
        let text = quote(&e.evidence_heading());
        let msg = final_message_from_evidence(&text);
        if e.state == EngineState::Available {
            assert_eq!(
                msg.as_deref(),
                Some("All done.\nTwo files changed."),
                "{}",
                e.id
            );
        }
    }
    assert_eq!(
        engines::find("cline").unwrap().evidence_heading(),
        "## Cline's last message",
        "Cline's heading is unchanged"
    );
    assert_eq!(
        engines::find("opencode").unwrap().evidence_heading(),
        "## OpenCode's last message"
    );
    assert_eq!(
        final_message_from_evidence("# Run\n\n## Somebody's last message\n\n> x\n"),
        None,
        "a heading of no engine is no message"
    );
}

#[test]
fn b787_the_panel_marks_the_engine_of_the_run_selected_and_lists_the_others_by_state() {
    let opencode = engines::find("opencode").unwrap();
    let rows = engines::panel_rows(&Capabilities::unknown(), opencode);
    let row = |id: &str| {
        rows.iter()
            .find(|r| r.starts_with(id))
            .unwrap_or_else(|| panic!("{rows:?}"))
            .clone()
    };
    assert!(row("opencode").contains("selected"), "{rows:?}");
    assert!(
        row("cline").contains("available") && !row("cline").contains("selected"),
        "{rows:?}"
    );
    assert!(row("pi").contains(engines::NOT_AVAILABLE), "{rows:?}");
    // For a run on the default engine the other available engine reads "available".
    let rows = engines::panel_rows(&Capabilities::unknown(), engines::default_engine());
    assert!(
        rows.iter()
            .any(|r| r.starts_with("cline") && r.contains("selected")),
        "{rows:?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("selected")).count(),
        1,
        "{rows:?}"
    );
    for r in &rows {
        assert!(
            r.chars().count() <= 76,
            "a row must not wrap in a 76-column panel: {r}"
        );
    }
}
