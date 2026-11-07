//! The engine-neutral surface of the chat client. `ham_core::engines` is the one module
//! that names an engine: its display names, the roadmap and the words "not available in this release". The rest of the client reads one
//! capability record per session (the `session.capabilities` event of agentd) and shows only what the record offers. These tests feed two
//! recorded handshakes (`tests/fixtures/capability/`, copies of agentd's golden records) to the same code and expect different popups, status
//! words and command lists with no code change.

#![allow(clippy::unwrap_used)]

use ham_core::engines::{
    self, CAPABILITIES_UNKNOWN, Capabilities, EngineState, NOT_AVAILABLE, panel_rows, panel_title,
    roadmap,
};
use ham_core::registry;
use ham_core::state::RunState;
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn record(file: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/capability/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn event(n: u32, ty: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": ty, "authority": "agent", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn state_with(records: &[Value]) -> RunState {
    let mut s = RunState::new(RUN);
    for (i, r) in records.iter().enumerate() {
        s.apply(event(i as u32 + 1, "session.capabilities", r.clone()));
    }
    s
}

fn first() -> Capabilities {
    Capabilities::from_state(&state_with(&[record("first-engine.json")]))
}

fn second() -> Capabilities {
    Capabilities::from_state(&state_with(&[record("second-engine.json")]))
}

// ---- the roadmap and /engines -----------------------------------------------------------------------------------------------

#[test]
fn the_roadmap_has_one_selected_engine_and_names_the_rest_as_not_available() {
    let all = roadmap();
    let selected: Vec<_> = all
        .iter()
        .filter(|e| e.state == EngineState::Selected)
        .collect();
    assert_eq!(selected.len(), 1, "{all:?}");
    assert_eq!(selected[0].id, "cline");
    let others: Vec<_> = all
        .iter()
        .filter(|e| e.state == EngineState::NotAvailable)
        .collect();
    assert!(others.len() >= 3, "{all:?}");
    assert!(
        others.iter().any(|e| e.id == "pi"),
        "Pi is second on the roadmap"
    );
    assert_eq!(NOT_AVAILABLE, "not available in this release");
}

/// The judge of scenario T58: exactly one row names cline and says it is selected; every other row is an
/// engine that says it is not available in this release; a heading ends with a colon.
fn judge_t58(rows: &[String]) -> Result<(), String> {
    let rows: Vec<String> = rows
        .iter()
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|r| !r.is_empty())
        .collect();
    let cline: Vec<&String> = rows
        .iter()
        .filter(|r| r.to_ascii_lowercase().contains("cline"))
        .collect();
    if cline.len() != 1 {
        return Err(format!("{} row(s) name cline: {rows:?}", cline.len()));
    }
    if !cline[0].to_ascii_lowercase().contains("selected") || cline[0].contains(NOT_AVAILABLE) {
        return Err(format!("the cline row: {:?}", cline[0]));
    }
    let others: Vec<&String> = rows
        .iter()
        .filter(|r| !r.to_ascii_lowercase().contains("cline"))
        .filter(|r| !r.ends_with(':'))
        .collect();
    if others.is_empty() {
        return Err("no other engine".into());
    }
    if let Some(bad) = others.iter().find(|r| !r.contains(NOT_AVAILABLE)) {
        return Err(format!("not marked: {bad:?}"));
    }
    Ok(())
}

#[test]
fn the_engines_panel_lists_cline_selected_and_the_others_not_available() {
    assert!(panel_title().to_ascii_lowercase().contains("engines"));
    for caps in [Capabilities::unknown(), first(), second()] {
        let rows = panel_rows(&caps);
        judge_t58(&rows).unwrap_or_else(|e| panic!("{e}\n{rows:#?}"));
    }
}

#[test]
fn without_a_record_the_panel_says_capabilities_unknown_and_shows_the_catalogs_defaults() {
    let caps = Capabilities::unknown();
    assert!(!caps.is_known());
    let rows = panel_rows(&caps);
    let cline = rows
        .iter()
        .find(|r| r.to_ascii_lowercase().contains("cline"))
        .unwrap();
    assert!(cline.contains(CAPABILITIES_UNKNOWN), "{cline}");
    assert_eq!(CAPABILITIES_UNKNOWN, "capabilities unknown");
    // The defaults come from the model catalog: the default model's name is on the row.
    let default_name = &ham_core::models::builtin().default_model().unwrap().name;
    assert!(cline.contains(default_name.as_str()), "{cline}");
    // A known record does not carry the label.
    let known = panel_rows(&first());
    assert!(
        !known.iter().any(|r| r.contains(CAPABILITIES_UNKNOWN)),
        "{known:?}"
    );
}

// ---- the capability record ------------------------------------------------------------------------------------------------------

#[test]
fn a_recorded_handshake_becomes_capabilities() {
    let c = first();
    assert!(c.is_known());
    assert_eq!(c.engine_label(), "Cline");
    assert_eq!(c.agent_text().as_deref(), Some("cline 3.0.65"));
    assert_eq!(c.session(), 1);
    assert_eq!(c.current_mode().as_deref(), Some("act"));
    assert_eq!(c.modes(), ["plan", "act"]);
    assert!(c.commands().is_empty());
    assert!(c.offers("message.next") && c.offers("message.now"));
    assert!(c.image_prompts());
}

#[test]
fn the_newest_session_record_wins() {
    let mut later = record("first-engine.json");
    later["session"] = json!(2);
    later["modes"]["current"] = json!("plan");
    let s = state_with(&[record("first-engine.json"), later]);
    let c = Capabilities::from_state(&s);
    assert_eq!(c.session(), 2);
    assert_eq!(c.current_mode().as_deref(), Some("plan"));
}

#[test]
fn a_run_with_no_record_is_unknown_and_never_claims_a_missing_feature() {
    let c = Capabilities::from_state(&RunState::new(RUN));
    assert!(!c.is_known());
    // A missing record does not identify an old image (E4): nothing is hidden because of it.
    assert!(c.offers("message.now") && c.offers("message.next"));
    assert_eq!(c.status_words(), "");
}

#[test]
fn a_record_that_lists_other_controls_hides_what_it_does_not_offer() {
    let mut r = record("first-engine.json");
    r["harness"]["controls"] = json!(["message.next"]);
    let c = Capabilities::from_state(&state_with(&[r]));
    assert!(c.offers("message.next"));
    assert!(!c.offers("message.now"));
}

// ---- no code change between two engines ---------------------------------------------------------------------------------------

#[test]
fn two_recorded_handshakes_give_different_status_words_and_commands_with_the_same_code() {
    let (a, b) = (first(), second());
    assert_ne!(a.status_words(), b.status_words());
    assert!(
        a.status_words().contains("mode act"),
        "{}",
        a.status_words()
    );
    assert!(
        b.status_words().contains("mode build"),
        "{}",
        b.status_words()
    );
    let names: Vec<&str> = b.commands().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["customize-opencode", "init", "review"]);
    // The popup of the second engine lists its commands under a double slash; the first lists none.
    let pa: Vec<&str> = engines::popup(&a, "").iter().map(|e| e.name).collect();
    let pb: Vec<&str> = engines::popup(&b, "").iter().map(|e| e.name).collect();
    assert!(pa.iter().all(|n| !n.starts_with('/')), "{pa:?}");
    assert!(
        pb.contains(&"/review") && pb.contains(&"/init"),
        "the second engine's commands are listed: {pb:?}"
    );
    // The built commands are in both, in registry order, before the engine's.
    assert!(pa.contains(&"cancel") && pb.contains(&"cancel"));
    assert!(
        pb.iter().position(|n| *n == "cancel").unwrap()
            < pb.iter().position(|n| *n == "/review").unwrap()
    );
    // The engine's commands are found by prefix, with or without the second slash.
    let one: Vec<&str> = engines::popup(&b, "re").iter().map(|e| e.name).collect();
    assert!(one.contains(&"/review"), "{one:?}");
    let two: Vec<&str> = engines::popup(&b, "/re").iter().map(|e| e.name).collect();
    assert_eq!(two, ["/review"]);
    assert!(engines::popup(&a, "/re").is_empty());
}

#[test]
fn a_command_the_record_does_not_offer_is_hidden_from_the_popup() {
    let mut r = record("first-engine.json");
    r["harness"]["controls"] = json!(["message.next"]);
    let without = Capabilities::from_state(&state_with(&[r]));
    let with = first();
    let names = |c: &Capabilities| -> Vec<&'static str> {
        engines::popup(c, "n").iter().map(|e| e.name).collect()
    };
    assert!(names(&with).contains(&"now"), "{:?}", names(&with));
    assert!(!names(&without).contains(&"now"), "{:?}", names(&without));
    // An unknown record hides nothing.
    assert!(names(&Capabilities::unknown()).contains(&"now"));
    // The registry's own popup is unchanged for callers that do not ask.
    assert!(registry::popup("n").iter().any(|e| e.name == "now"));
}

#[test]
fn an_advertised_command_is_forwarded_with_a_double_slash_and_nothing_else_is() {
    let b = second();
    assert_eq!(
        b.forward("//review the diff").as_deref(),
        Some("/review the diff")
    );
    assert_eq!(b.forward("//review").as_deref(), Some("/review"));
    // Not advertised, a single slash (a local command), plain text and an upper-case word are never forwarded.
    assert_eq!(b.forward("//nonsense x"), None);
    assert_eq!(b.forward("/review"), None);
    assert_eq!(b.forward("review"), None);
    assert_eq!(b.forward("//Review"), None);
    // The first engine advertises none.
    assert_eq!(first().forward("//review"), None);
    // A command word with an odd shape in a record is never listed or forwarded.
    let mut r = record("second-engine.json");
    r["commands"] = json!([{"name": "ok-one"}, {"name": "has space"}, {"name": "../etc"}, {"name": "x\u{1b}[31m"}, {"name": ""}]);
    let odd = Capabilities::from_state(&state_with(&[r]));
    let names: Vec<&str> = odd.commands().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["ok-one"]);
    assert_eq!(odd.forward("//has space").as_deref(), None);
}

#[test]
fn text_from_the_record_is_cleaned_before_it_is_shown() {
    let mut r = record("first-engine.json");
    r["agent"]["name"] = json!("cl\u{1b}]8;;http://x\u{7}ine\u{202e}");
    r["engine"] = json!("En\u{1b}[31mgine");
    r["modes"]["current"] = json!("a\u{1b}[0mct");
    r["modes"]["available"] = json!([{"id": "a", "name": "A\u{7}ct"}]);
    let c = Capabilities::from_state(&state_with(&[r]));
    for text in [
        c.engine_label(),
        c.agent_text().unwrap_or_default(),
        c.status_words(),
        panel_rows(&c).join("\n"),
    ] {
        assert!(
            !text.chars().any(|ch| ch == '\u{1b}' || ch == '\u{7}'),
            "{text:?}"
        );
        assert!(!text.contains('\u{202e}'), "{text:?}");
    }
}

// ---- later updates of the stored conversation -----------------------------------------------------------

#[test]
fn later_updates_in_the_conversation_change_the_mode_and_the_commands_and_last_write_wins() {
    let mut c = first();
    assert_eq!(c.current_mode().as_deref(), Some("act"));
    c.apply_update(
        "current_mode_update",
        &json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
    );
    assert_eq!(c.current_mode().as_deref(), Some("plan"));
    assert!(
        c.status_words().contains("mode plan"),
        "{}",
        c.status_words()
    );
    c.apply_update(
        "available_commands_update",
        &json!({"sessionUpdate": "available_commands_update",
                "availableCommands": [{"name": "deploy", "description": "ship it"}, {"name": "bad name"}]}),
    );
    let names: Vec<&str> = c.commands().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["deploy"], "an odd command word is dropped");
    assert_eq!(c.forward("//deploy now").as_deref(), Some("/deploy now"));
    c.apply_update(
        "available_commands_update",
        &json!({"availableCommands": []}),
    );
    assert!(c.commands().is_empty(), "the last update wins");
}

#[test]
fn an_update_changes_nothing_when_there_is_no_record_or_the_kind_is_another() {
    let mut c = Capabilities::unknown();
    c.apply_update("current_mode_update", &json!({"currentModeId": "plan"}));
    assert!(!c.is_known());
    assert_eq!(c.current_mode(), None);
    let mut c = first();
    let before = c.clone();
    c.apply_update("usage_update", &json!({"used": 1}));
    c.apply_update("current_mode_update", &json!({"currentModeId": 7}));
    c.apply_update(
        "agent_message_chunk",
        &json!({"content": {"type": "text", "text": "x"}}),
    );
    assert_eq!(c, before);
}
