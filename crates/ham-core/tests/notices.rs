//! The harness notices agentd writes into the event stream as `turn.progress` (kinds `provider-retry`,
//! `provider-switch`, `engine-stop`, `notify`, `provider-wait`, `provider-outage`) and as `permission.requested` get one plain
//! line each, and the status line learns the route in use. The payloads are the members `crates/agentd/src/direct/collector.rs`
//! writes (`producer_members_match_the_collector` pins them to its source); the question and answer kinds use the golden files of
//! `fixtures/direct-input/`, which agentd's own writer produced.

#![allow(clippy::unwrap_used)]

use ham_core::notice::{Notice, NoticeKind, notice_of};
use ham_core::state::{RunState, RunView};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

/// ULIDs that sort by `n`.
fn id(n: u32) -> String {
    format!("01M5A2R7K3C9XJ4D8E6F{n:06}")
}

fn event(n: u32, ty: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "agentId": "direct", "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": ty, "authority": "agent", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn progress(n: u32, value: Value) -> Envelope {
    event(n, "turn.progress", value)
}

fn view_of(events: &[Envelope]) -> RunView {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e.clone());
    }
    s.view()
}

fn only(e: &Envelope) -> Notice {
    notice_of(e).unwrap_or_else(|| panic!("no notice for {e:?}"))
}

fn retry(n: u32, attempt: u32) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "provider-retry", "attempt": attempt, "of": 4, "class": "transient", "backoffS": 12,
               "detail": "ThrottlingException: Too many requests, please wait before trying again."}),
    )
}

fn switch(n: u32, switch_kind: &str, from: &str, to: &str) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "provider-switch", "n": 1, "from": from, "to": to, "switchKind": switch_kind,
               "class": "transient", "detail": "ServiceUnavailableException"}),
    )
}

#[test]
fn a_provider_retry_says_the_class_the_try_and_the_wait() {
    let n = only(&retry(1, 2));
    assert_eq!(n.kind, NoticeKind::ProviderRetry);
    assert_eq!(
        n.text,
        "Provider error (temporary): try 2 of 4 in 12 s. ThrottlingException: Too many requests, please wait before trying again."
    );
    assert_eq!(n.event_id, id(1));
}

#[test]
fn a_provider_retry_with_a_long_detail_is_cut_to_one_short_line() {
    let long = "x".repeat(900);
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "provider-retry", "attempt": 1, "of": 4, "class": "unavailable", "backoffS": 3, "detail": long}),
    ));
    assert!(
        n.text
            .starts_with("Provider error (route unavailable): try 1 of 4 in 3 s. xxx")
    );
    assert!(n.text.chars().count() <= 160, "{}", n.text.chars().count());
    assert!(n.text.ends_with('\u{2026}'));
    assert!(!n.text.contains('\n'));
}

#[test]
fn a_provider_switch_to_another_model_names_both_models() {
    let n = only(&switch(
        1,
        "model",
        "global.anthropic.claude-sonnet-5-5",
        "us.openai.gpt-6.1-sol",
    ));
    assert_eq!(n.kind, NoticeKind::ProviderSwitch);
    assert_eq!(
        n.text,
        "Switched to another model (switch 1): Claude Sonnet 5.5 to GPT-6.1 Sol, after a temporary failure."
    );
}

#[test]
fn a_provider_switch_between_two_routes_of_one_model_shows_the_route_ids() {
    let n = only(&switch(
        1,
        "route",
        "global.anthropic.claude-sonnet-5-5",
        "us.anthropic.claude-sonnet-5-5",
    ));
    assert_eq!(
        n.text,
        "Switched route (switch 1): global.anthropic.claude-sonnet-5-5 to us.anthropic.claude-sonnet-5-5, after a temporary failure."
    );
}

#[test]
fn a_return_to_the_chosen_model_says_so() {
    let n = only(&switch(
        1,
        "return",
        "us.openai.gpt-6.1-sol",
        "global.anthropic.claude-sonnet-5-5",
    ));
    assert_eq!(
        n.text,
        "Back on the model you chose (switch 1): GPT-6.1 Sol to Claude Sonnet 5.5."
    );
}

#[test]
fn an_unknown_model_id_is_shown_as_the_harness_wrote_it() {
    let n = only(&switch(1, "model", "a.b.c", "x.y.z"));
    assert_eq!(
        n.text,
        "Switched to another model (switch 1): a.b.c to x.y.z, after a temporary failure."
    );
}

#[test]
fn an_engine_stop_names_what_the_engine_did_and_when_the_next_try_comes() {
    let cases = [
        ("loop-guard", "repeated the same tool call"),
        ("mistake-limit", "too many failed turns"),
        ("unexplained", "no cause found"),
        ("brand-new-hint", "brand-new-hint"),
    ];
    for (hint, words) in cases {
        let n = only(&progress(
            1,
            json!({"turn": 1, "kind": "engine-stop", "hint": hint, "attempt": 1, "of": 2, "backoffS": 8,
                   "detail": "the engine stopped the run itself (5 identical calls); the model stays x; one more try follows in 8 s"}),
        ));
        assert_eq!(n.kind, NoticeKind::EngineStop);
        assert_eq!(
            n.text,
            format!("The engine stopped its own prompt ({words}): try 1 of 2 follows in 8 s.")
        );
    }
}

#[test]
fn a_notify_about_an_outage_says_how_long_and_that_the_run_waits() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "notify", "reason": "outage", "outageMinutes": 30, "detail": "all routes failed"}),
    ));
    assert_eq!(n.kind, NoticeKind::Notify);
    assert_eq!(
        n.text,
        "Every model route has been down for 30 min; the run keeps waiting."
    );
}

#[test]
fn a_notify_about_an_unusable_history_says_a_fresh_session_starts_over() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "notify", "reason": "history_unusable", "outageMinutes": null,
               "detail": "the provider refuses the conversation the engine replays"}),
    ));
    assert_eq!(
        n.text,
        "The provider rejects the saved conversation; a fresh session starts it over."
    );
}

#[test]
fn a_notify_with_a_reason_this_client_does_not_know_shows_the_reason() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "notify", "reason": "disk_low", "outageMinutes": null, "detail": "free space 3%"}),
    ));
    assert_eq!(n.text, "Notice from the run (disk_low). free space 3%");
}

#[test]
fn a_provider_wait_says_the_pass_and_the_seconds() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "provider-wait", "pass": 2, "waitS": 60, "detail": "every route failed"}),
    ));
    assert_eq!(n.kind, NoticeKind::ProviderWait);
    assert_eq!(
        n.text,
        "All model routes failed (pass 2); waiting 60 s before the next try."
    );
}

#[test]
fn a_provider_outage_says_the_minutes_and_the_time_limit() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "provider-outage", "waitedS": 1800, "outageMinutes": 30}),
    ));
    assert_eq!(n.kind, NoticeKind::ProviderOutage);
    assert_eq!(
        n.text,
        "Every model route has been down for 30 min; the run keeps trying until its time limit."
    );
}

#[test]
fn a_permission_request_says_what_it_was_for_and_what_became_of_it() {
    let n = only(&event(
        1,
        "permission.requested",
        json!({"toolCallId": "tooluse_a", "allowed": true, "auto": true, "title": "run_commands: ls -la"}),
    ));
    assert_eq!(n.kind, NoticeKind::Permission);
    assert_eq!(
        n.text,
        "Permission for run_commands: ls -la was allowed automatically."
    );
    let refused = only(&event(
        2,
        "permission.requested",
        json!({"toolCallId": "tooluse_a", "allowed": false, "auto": false, "title": "editor: x"}),
    ));
    assert_eq!(refused.text, "Permission for editor: x was refused.");
    let untitled = only(&event(
        3,
        "permission.requested",
        json!({"toolCallId": "tooluse_a", "allowed": true, "auto": false, "title": null}),
    ));
    assert_eq!(untitled.text, "Permission for a tool call was allowed.");
}

/// The golden envelope of `fixtures/direct-input/<name>`, written by agentd's own writer.
fn golden(name: &str) -> Envelope {
    let path = format!(
        "{}/../../fixtures/direct-input/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_question_and_answer_events_get_their_own_words() {
    let n = only(&golden("needs-input-engine-stop.json"));
    assert_eq!(n.kind, NoticeKind::NeedsInput);
    assert_eq!(n.text, "The run needs you: Cline stopped its own prompt");
    let o = only(&golden("needs-input-outage-notice.json"));
    assert_eq!(o.kind, NoticeKind::NeedsInput);
    assert_eq!(o.text, "Every model route is down");
    let r = only(&golden("input-received.json"));
    assert_eq!(r.kind, NoticeKind::InputReceived);
    assert_eq!(r.text, "Your message reached Cline (same session).");
    let i = only(&golden("input-ignored.json"));
    assert_eq!(i.kind, NoticeKind::InputIgnored);
    assert_eq!(
        i.text,
        "Cline did not take your message: the window is closed."
    );
    let c = only(&golden("input-closed.json"));
    assert_eq!(c.kind, NoticeKind::InputClosed);
    assert_eq!(
        c.text,
        "Nobody answered within 30 min, so the run finishes with what exists."
    );
    let s = only(&golden("new-session-user.json"));
    assert_eq!(s.kind, NoticeKind::NewSession);
    assert_eq!(
        s.text,
        "Restarted from the work tree: fresh Cline session 2."
    );
}

#[test]
fn events_that_are_not_notices_give_none() {
    let none = [
        progress(
            1,
            json!({"turn": 1, "kind": "message", "text": "I'll start"}),
        ),
        progress(2, json!({"toolCallId": "c1", "status": "completed"})),
        event(
            3,
            "tool.summary",
            json!({"toolCallId": "c1", "title": "x", "kind": "edit", "status": "pending"}),
        ),
        event(4, "turn.started", json!({"turn": 1})),
    ];
    for e in &none {
        assert!(notice_of(e).is_none(), "{e:?}");
    }
}

#[test]
fn a_turn_progress_kind_this_client_has_no_name_for_is_kept_as_an_other_notice() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "future-kind", "detail": "something new"}),
    ));
    assert_eq!(n.kind, NoticeKind::Other);
    assert_eq!(n.text, "Event future-kind");
}

#[test]
fn notice_text_is_one_line_and_terminal_safe() {
    let n = only(&progress(
        1,
        json!({"turn": 1, "kind": "provider-retry", "attempt": 1, "of": 4, "class": "transient", "backoffS": 2,
               "detail": "line one\nline\ttwo \u{1b}]0;title\u{7}\u{1b}[31mred\u{202e}"}),
    ));
    assert!(
        !n.text.contains(['\n', '\t', '\u{1b}', '\u{7}', '\u{202e}']),
        "{:?}",
        n.text
    );
    // The bidi override is shown as a marker, not dropped: provider error text is outside text the harness wrote.
    assert!(
        n.text.ends_with("line one line two red<U+202E>"),
        "{:?}",
        n.text
    );
}

#[test]
fn the_view_lists_the_run_notices_in_event_order_and_not_the_question_kinds() {
    let v = view_of(&[
        retry(1, 1),
        golden("input-received.json"),
        switch(
            3,
            "model",
            "global.anthropic.claude-sonnet-5-5",
            "us.openai.gpt-6.1-sol",
        ),
        event(
            4,
            "permission.requested",
            json!({"toolCallId": "c", "allowed": true, "auto": true, "title": "editor: x"}),
        ),
    ]);
    let kinds: Vec<NoticeKind> = v.notices.iter().map(|n| n.kind).collect();
    assert_eq!(
        kinds,
        [
            NoticeKind::ProviderRetry,
            NoticeKind::ProviderSwitch,
            NoticeKind::Permission
        ]
    );
}

#[test]
fn the_route_in_use_follows_the_switches_and_shows_in_the_status_line() {
    let v = view_of(&[
        switch(
            1,
            "model",
            "global.anthropic.claude-sonnet-5-5",
            "us.openai.gpt-6.1-sol",
        ),
        switch(
            2,
            "route",
            "us.openai.gpt-6.1-sol",
            "global.openai.gpt-6.1-sol",
        ),
    ]);
    assert_eq!(v.route.switches, 2);
    assert_eq!(v.route.model.as_deref(), Some("global.openai.gpt-6.1-sol"));
    assert_eq!(
        v.route_line().as_deref(),
        Some("running on GPT-6.1 Sol (global.openai.gpt-6.1-sol) after 2 switches")
    );
}

#[test]
fn a_retry_shows_in_the_status_line_until_the_engine_works_again() {
    let during = view_of(&[retry(1, 2)]);
    assert_eq!(
        during.route_line().as_deref(),
        Some("provider error, try 2 of 4")
    );
    let after = view_of(&[
        retry(1, 2),
        event(
            2,
            "tool.summary",
            json!({"toolCallId": "c1", "title": "editor: x", "kind": "edit", "status": "pending"}),
        ),
    ]);
    assert_eq!(after.route_line(), None);
}

#[test]
fn a_wait_and_an_outage_show_in_the_status_line() {
    let waiting = view_of(&[progress(
        1,
        json!({"turn": 1, "kind": "provider-wait", "pass": 1, "waitS": 60, "detail": "x"}),
    )]);
    assert_eq!(
        waiting.route_line().as_deref(),
        Some("waiting on the model provider")
    );
    let out = view_of(&[progress(
        1,
        json!({"turn": 1, "kind": "provider-outage", "waitedS": 1800, "outageMinutes": 30}),
    )]);
    assert_eq!(
        out.route_line().as_deref(),
        Some("every model route down for 30 min")
    );
}

#[test]
fn a_switch_after_a_retry_clears_the_retry_and_keeps_the_route() {
    let v = view_of(&[
        retry(1, 3),
        switch(
            2,
            "model",
            "global.anthropic.claude-sonnet-5-5",
            "us.openai.gpt-6.1-sol",
        ),
    ]);
    assert_eq!(
        v.route_line().as_deref(),
        Some("running on GPT-6.1 Sol after 1 switch")
    );
}

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";

/// An event only the harness may write (the end of the run is its word).
fn harness_event(n: u32, ty: &str, value: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": id(n), "runId": RUN, "seq": n, "ts": "2026-10-16T08:15:00Z",
        "type": ty, "authority": "harness", "payload": {"kind": "inline", "value": value}
    }))
    .unwrap()
}

fn wait(n: u32) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "provider-wait", "pass": 1, "waitS": 60, "detail": "x"}),
    )
}

fn outage(n: u32) -> Envelope {
    progress(
        n,
        json!({"turn": 1, "kind": "provider-outage", "waitedS": 1800, "outageMinutes": 30}),
    )
}

/// The header of the chat view names the model in use itself, so the part it adds is only what is going wrong.
#[test]
fn the_trouble_part_names_only_what_is_going_wrong_with_the_model_calls() {
    assert_eq!(view_of(&[]).route_trouble(), None);
    // A switch alone is no trouble: the model in use is the status line's model.
    let switched = switch(1, "model", SONNET, SOL);
    assert_eq!(
        view_of(std::slice::from_ref(&switched)).route_trouble(),
        None
    );
    assert_eq!(
        view_of(&[retry(1, 2)]).route_trouble().as_deref(),
        Some("provider error, try 2 of 4")
    );
    // A retry on the route after a switch: the retry, without the "running on" part of the route line.
    let on_sol = view_of(&[switched, retry(2, 3)]);
    assert_eq!(
        on_sol.route_trouble().as_deref(),
        Some("provider error, try 3 of 4")
    );
    assert_eq!(
        on_sol.route_line().as_deref(),
        Some("running on GPT-6.1 Sol after 1 switch; provider error, try 3 of 4")
    );
    // A wait and an outage together read as the route line reads them.
    assert_eq!(
        view_of(&[wait(1), outage(2)]).route_trouble().as_deref(),
        Some("waiting on the model provider; every model route down for 30 min")
    );
}

#[test]
fn a_run_that_is_over_has_nothing_live_to_say_about_its_route() {
    let ended = harness_event(
        3,
        "run.completed",
        json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
    );
    let over = view_of(&[switch(1, "model", SONNET, SOL), retry(2, 2), ended]);
    assert!(over.completed, "TRIGGER: the run is over");
    assert_eq!(over.route_trouble(), None);
    assert_eq!(over.route_now(), None);
    // The route line itself stays what the events said: it is the record, `route_now` is the live part.
    assert_eq!(
        over.route_line().as_deref(),
        Some("running on GPT-6.1 Sol after 1 switch; provider error, try 2 of 4")
    );
    // While the run goes on, the live line is the route line.
    let live = view_of(&[switch(1, "model", SONNET, SOL), retry(2, 2)]);
    assert_eq!(live.route_now(), live.route_line());
    assert!(live.route_now().is_some());
}

/// agentd begins a fresh session on the route the user chose, whatever the ladder did before (`direct/fresh.rs`), and writes a `new-session`
/// progress event; the route in use is the chosen one again until the next switch.
#[test]
fn a_fresh_session_puts_the_run_back_on_the_model_the_user_chose() {
    let fresh = progress(
        3,
        json!({"turn": 1, "kind": "new-session", "text": "A fresh Cline session started.", "requestId": null,
               "session": 2, "trigger": "harness", "reason": "engine_stop", "restartedProcess": false,
               "digestBytes": 1200, "promptBytes": 1400}),
    );
    let before = view_of(&[switch(1, "model", SONNET, SOL), retry(2, 2)]);
    assert_eq!(before.route.model.as_deref(), Some(SOL), "TRIGGER: on Sol");
    let after = view_of(&[switch(1, "model", SONNET, SOL), retry(2, 2), fresh.clone()]);
    assert_eq!(after.sessions, 2, "TRIGGER: a fresh session");
    assert_eq!(after.route.model, None);
    assert_eq!(after.route_line(), None);
    // A switch after the fresh session is a new route again.
    let again = view_of(&[
        switch(1, "model", SONNET, SOL),
        fresh,
        switch(4, "route", SONNET, "us.anthropic.claude-sonnet-5-5"),
    ]);
    assert_eq!(
        again.route.model.as_deref(),
        Some("us.anthropic.claude-sonnet-5-5")
    );
}

/// The producer pin: each member this client reads is written by the collector next to its kind word. A rename in agentd fails here
/// before it blanks a notice on a live stage.
#[test]
fn producer_members_match_the_collector() {
    let src = include_str!("../../agentd/src/direct/collector.rs");
    let kinds: [(&str, &[&str]); 6] = [
        (
            "provider-retry",
            &["attempt", "of", "class", "backoffS", "detail"],
        ),
        (
            "provider-switch",
            &["n", "from", "to", "switchKind", "class", "detail"],
        ),
        (
            "engine-stop",
            &["hint", "attempt", "of", "backoffS", "detail"],
        ),
        ("notify", &["reason", "outageMinutes", "detail"]),
        ("provider-wait", &["pass", "waitS", "detail"]),
        ("provider-outage", &["waitedS", "outageMinutes"]),
    ];
    for (kind, members) in kinds {
        let at = src
            .find(&format!("\"kind\": \"{kind}\""))
            .unwrap_or_else(|| panic!("the collector writes no kind {kind}"));
        let block: String = src[at..].chars().take(420).collect();
        for m in members {
            assert!(
                block.contains(&format!("\"{m}\"")),
                "kind {kind}: the collector no longer writes {m}"
            );
        }
    }
    let at = src.find("EventType::PermissionRequested").unwrap();
    let block: String = src[at..].chars().take(600).collect();
    for m in ["toolCallId", "allowed", "auto", "title"] {
        assert!(block.contains(&format!("\"{m}\"")), "permission: {m}");
    }
}
