//! The saved copy of a run's work in the client's view of a run. The service tells the run of each stored copy with a progress
//! event that has no `kind`; the run view keeps the newest, the run screen and the card show when it was saved, and a client that does
//! not know the member shows nothing for it.
#![allow(clippy::unwrap_used)]

use ham_core::chat::{ChatInputs, ChatModel};
use ham_core::engines;
use ham_core::render::{FinalCard, final_card};
use ham_core::saved_copy::{self, SavedCopy};
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use serde_json::{Value, json};

const RUN: &str = "01M76EJVX4BSZ814X2DVVN834F";
const HEAD: &str = "ad2645e679230000000000000000000000000000";

fn ev(n: u8, t: EventType, authority: Authority, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: u64::from(n),
        ts: "2026-11-12T00:00:00Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: v },
    }
}

fn harness(n: u8, t: EventType, v: Value) -> Envelope {
    ev(n, t, Authority::Harness, None, v)
}

fn agent(n: u8, t: EventType, v: Value) -> Envelope {
    ev(n, t, Authority::Agent, Some("direct"), v)
}

fn copy(n: u8, at: &str, count: u32, session: u32, last: bool) -> Envelope {
    let mut c = json!({"at": at, "count": count, "headSha": HEAD, "session": session});
    if last {
        c["final"] = json!(true);
    }
    agent(
        n,
        EventType::TurnProgress,
        json!({"turn": 1, "savedCopy": c}),
    )
}

fn started(engine: &str) -> RunState {
    let mut s = RunState::new(RUN);
    s.apply(harness(1, EventType::RunCreated, json!({})));
    s.apply(harness(
        2,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": engine, "modelId": "global.anthropic.claude-sonnet-5-5"}),
    ));
    s.apply(agent(
        3,
        EventType::TurnStarted,
        json!({"turn": 1, "engine": engine, "model": "global.anthropic.claude-sonnet-5-5", "role": "direct"}),
    ));
    s
}

fn utc(t: i64) -> String {
    ham_core::render::utc_clock(t)
}

#[test]
fn b823_the_run_view_keeps_the_newest_saved_copy() {
    let mut s = started("cline");
    assert_eq!(
        s.view().saved,
        None,
        "a run that saved nothing shows nothing"
    );
    s.apply(copy(10, "2026-11-12T00:30:22Z", 7, 1, false));
    s.apply(copy(11, "2026-11-12T00:40:22Z", 8, 1, false));
    let v = s.view();
    let c = v.saved.clone().unwrap();
    assert_eq!((c.count, c.session, c.final_copy), (8, 1, false));
    assert_eq!(c.at, "2026-11-12T00:40:22Z");
    assert_eq!(c.head_sha, HEAD);
    // The copy of the end replaces it.
    s.apply(copy(12, "2026-11-12T00:47:40Z", 9, 1, true));
    assert!(s.view().saved.unwrap().final_copy);
    // An event that arrives late, or a copy of an earlier session, never puts an older copy back.
    s.apply(copy(13, "2026-11-12T00:20:22Z", 6, 1, false));
    assert_eq!(s.view().saved.unwrap().count, 9);
    s.apply(copy(14, "2026-11-12T01:10:00Z", 1, 2, false));
    let c = s.view().saved.unwrap();
    assert_eq!(
        (c.session, c.count),
        (2, 1),
        "a later session's first copy is newer"
    );
}

#[test]
fn b823_a_member_that_is_no_copy_is_ignored() {
    for bad in [
        json!({"turn": 1, "savedCopy": {"at": "yesterday", "count": 1, "headSha": HEAD}}),
        json!({"turn": 1, "savedCopy": {"at": "2026-11-12T00:40:22Z", "count": 1, "headSha": "not hex"}}),
        json!({"turn": 1, "savedCopy": {"at": "2026-11-12T00:40:22Z", "headSha": HEAD}}),
        json!({"turn": 1, "savedCopy": "soon"}),
    ] {
        let mut s = started("cline");
        s.apply(agent(10, EventType::TurnProgress, bad.clone()));
        assert_eq!(s.view().saved, None, "{bad}");
    }
    // Only an agent's progress event carries one.
    let e = harness(
        10,
        EventType::GoalStatus,
        json!({"status": "active", "savedCopy": {"at": "2026-11-12T00:40:22Z", "count": 1, "headSha": HEAD}}),
    );
    assert_eq!(saved_copy::parse(&e), None);
}

/// What a client that does not know the member does with the event: nothing. The event has no `kind`, no `text` and no tool call id, so
/// it makes no notice, no tool row, no chat block, and it leaves the agent's last words alone.
#[test]
fn b823_the_event_adds_no_notice_no_tool_row_and_no_chat_block() {
    let mut s = started("cline");
    s.apply(agent(
        9,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "message", "text": "Working on it"}),
    ));
    let before = s.view();
    let chat_before = ChatModel::from_progress_feed(&s, &ChatInputs::default());
    s.apply(copy(10, "2026-11-12T00:40:22Z", 8, 1, false));
    let after = s.view();
    assert_eq!(after.notices, before.notices);
    assert_eq!(after.tools, before.tools);
    assert_eq!(
        after.agents, before.agents,
        "the agent's last words are not replaced"
    );
    assert_eq!(after.status, before.status);
    let chat_after = ChatModel::from_progress_feed(&s, &ChatInputs::default());
    assert_eq!(chat_after.blocks, chat_before.blocks);
}

#[test]
fn b823_the_run_screen_says_when_the_last_copy_was_saved() {
    let cline = engines::find("cline").unwrap();
    let c = SavedCopy {
        at: "2026-11-12T00:40:22Z".into(),
        at_unix: 1_794_444_022,
        count: 8,
        head_sha: HEAD.into(),
        session: 1,
        final_copy: false,
    };
    // While the run works: the time and how long ago.
    let line = saved_copy::screen_line(&c, cline, false, Some(c.at_unix + 130), &utc);
    assert_eq!(line, format!("Saved  {}, 2 min ago", utc(c.at_unix)));
    // A run that is over and whose last copy is not the copy of the end: no age that keeps growing.
    let line = saved_copy::screen_line(&c, cline, true, Some(c.at_unix + 7_200), &utc);
    assert_eq!(
        line,
        format!("Saved  {}, while the run worked", utc(c.at_unix))
    );
    // The copy of the end names the engine that ran.
    let end = SavedCopy {
        final_copy: true,
        ..c.clone()
    };
    assert_eq!(
        saved_copy::screen_line(&end, cline, true, Some(c.at_unix + 7_200), &utc),
        format!("Saved  {}, when Cline stopped", utc(c.at_unix))
    );
    let opencode = engines::find("opencode").unwrap();
    assert!(
        saved_copy::screen_line(&end, opencode, true, None, &utc)
            .ends_with("when OpenCode stopped")
    );
}

#[test]
fn b823_the_card_of_a_blocked_run_says_where_its_work_is() {
    let mut s = started("cline");
    s.apply(copy(12, "2026-11-12T00:47:40Z", 9, 1, true));
    s.apply(harness(20, EventType::GoalBlocked, json!({"reason": "bundle validation failed: workflow-file .github/workflows/ci.yml: refused"})));
    s.apply(harness(
        21,
        EventType::GoalStatus,
        json!({"status": "blocked", "phase": "delivering"}),
    ));
    s.apply(harness(
        22,
        EventType::RunCompleted,
        json!({"status": "blocked", "outcome": "reported_done", "label": "Cline reported done"}),
    ));
    let v = s.view();
    assert_eq!(v.status, Some(GoalStatus::Blocked));
    let card = final_card(&v, &FinalCard::default());
    assert!(card.contains("Zip       n/a"), "{card}");
    assert!(
        card.contains(&format!(
            "Saved     2026-11-12 00:47 UTC, when Cline stopped; ham pull {RUN} --snapshot downloads it"
        )),
        "{card}"
    );
    // A run that saved nothing has no such line.
    let none = started("cline");
    assert!(!final_card(&none.view(), &FinalCard::default()).contains("Saved"));
}

#[test]
fn b823_a_command_that_needs_a_zip_names_the_snapshot_command_for_every_status() {
    let all = [
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::UsageLimited,
        GoalStatus::BudgetLimited,
        GoalStatus::Complete,
        GoalStatus::Cleared,
        GoalStatus::Stalled,
    ];
    for status in all {
        let text = saved_copy::no_zip(RUN, Some(status));
        assert!(
            text.ends_with(&format!(
                "ham pull {RUN} --snapshot downloads the latest saved copy of its work."
            )),
            "{status:?}: {text}"
        );
        if status == GoalStatus::Active {
            assert_eq!(
                text,
                format!(
                    "run {RUN} is still working and has no zip yet. Follow it with ham watch {RUN}; ham pull {RUN} --snapshot downloads the latest saved copy of its work."
                )
            );
        } else {
            // A run that ended is never told "yet" and never sent to watch a run that is over.
            assert!(
                !text.contains("yet") && !text.contains("ham watch"),
                "{status:?}: {text}"
            );
            assert!(
                text.starts_with(&format!("run {RUN} ended ")),
                "{status:?}: {text}"
            );
        }
    }
    assert_eq!(
        saved_copy::no_zip(RUN, Some(GoalStatus::Blocked)),
        format!(
            "run {RUN} ended blocked and delivered no zip. ham pull {RUN} --snapshot downloads the latest saved copy of its work."
        )
    );
    assert!(
        saved_copy::no_zip(RUN, Some(GoalStatus::UsageLimited)).contains("ended usage limited")
    );
    // The status could not be read: no claim about it.
    assert_eq!(
        saved_copy::no_zip(RUN, None),
        format!(
            "run {RUN} has no zip. ham pull {RUN} --snapshot downloads the latest saved copy of its work."
        )
    );
    // The keys of the run screen say it in one line.
    assert_eq!(
        saved_copy::no_delivery(Some(GoalStatus::Blocked)).as_deref(),
        Some(
            "this run ended blocked and delivered no zip; /pull --snapshot downloads the latest saved copy of its work"
        )
    );
    // A run that works keeps the key's own words ("nothing is delivered yet").
    assert_eq!(saved_copy::no_delivery(Some(GoalStatus::Active)), None);
    assert_eq!(saved_copy::no_delivery(None), None);
}

#[test]
fn b823_the_retry_panel_and_the_hint_say_where_the_work_of_a_blocked_run_is() {
    use ham_core::retry;
    let saved = SavedCopy {
        at: "2026-11-12T00:47:40Z".into(),
        at_unix: 1_794_444_460,
        count: 9,
        head_sha: HEAD.into(),
        session: 1,
        final_copy: true,
    };
    // The run's events show a copy: the panel says that the work stays saved and how to get it.
    let panel = retry::confirmation_with(RUN, Some("zip only"), false, Some(&saved));
    assert!(
        panel.contains(&format!(
            "This run's work stays saved: ham pull {RUN} --snapshot downloads it."
        )),
        "{panel}"
    );
    assert!(
        panel.contains("This starts a new billable run with the same request"),
        "{panel}"
    );
    // No copy in the events (an older service, or a run that saved none): the panel says what the command does and promises nothing.
    let panel = retry::confirmation_with(RUN, None, true, None);
    assert!(
        panel.contains(&format!(
            "ham pull {RUN} --snapshot downloads this run's last saved copy, if it saved one."
        )),
        "{panel}"
    );
    assert!(!panel.contains("stays saved"), "{panel}");
    // The hint of a blocked run names the command as well.
    let hint = retry::hint(RUN, Some(&saved));
    assert!(hint.starts_with(retry::HINT), "{hint}");
    assert!(
        hint.ends_with(&format!(
            "This run's work stays saved: ham pull {RUN} --snapshot downloads it."
        )),
        "{hint}"
    );
}

#[test]
fn b823_the_help_says_what_a_blocked_run_is_and_what_the_snapshot_option_does() {
    // The help wraps its lines: the sentences are compared with the breaks taken out.
    let flat = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let help = flat(ham_core::help::top_text());
    assert!(
        help.contains("The run is blocked: it ended without a delivery. ham pull <runId> --snapshot downloads the latest saved copy of its work."),
        "{help}"
    );
    assert!(
        !help.contains("waits for you"),
        "a blocked run has ended: {help}"
    );
    let pull = flat(ham_core::help::command_text("pull").unwrap());
    assert!(
        pull.contains("Download the latest saved copy of the run's work, while the run works or after it ended; this option cannot accompany --artifact."),
        "{pull}"
    );
    assert!(!pull.contains("from an active run"), "{pull}");
}

#[test]
fn b823_a_notice_that_a_copy_could_not_be_saved_is_the_services_sentence() {
    let mut s = started("cline");
    s.apply(agent(
        10,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "notify", "reason": "copy_not_saved", "outageMinutes": null,
            "detail": "A copy of the work could not be saved; ham keeps trying."}),
    ));
    let notices = s.view().notices;
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].text,
        "A copy of the work could not be saved; ham keeps trying."
    );
}

#[test]
fn b823_the_copy_of_an_end_is_not_the_end_once_the_engine_started_again() {
    let again = |n: u8| {
        // The service writes `session.started` for the first session only; every session's worker writes `turn.started`.
        let mut e = agent(
            n,
            EventType::TurnStarted,
            json!({"turn": 1, "engine": "cline", "model": "global.anthropic.claude-sonnet-5-5", "role": "direct"}),
        );
        e.ts = "2026-11-12T00:48:30Z".into();
        e
    };
    // A long run renews its session: the first session ends with its copy of the end, then the engine starts again in the next one.
    let mut s = started("cline");
    s.apply(copy(10, "2026-11-12T00:47:40Z", 9, 1, true));
    assert!(
        s.view().saved.unwrap().final_copy,
        "the end of a run of one session"
    );
    s.apply(again(11));
    let c = s.view().saved.unwrap();
    assert!(
        !c.final_copy,
        "the engine works again: that copy is one from before"
    );
    assert_eq!(c.count, 9);
    let cline = engines::find("cline").unwrap();
    let line = saved_copy::screen_line(&c, cline, false, Some(c.at_unix + 130), &utc);
    assert!(line.ends_with("2 min ago"), "{line}");
    // The events of the two sessions in the other order.
    let mut s = started("cline");
    s.apply(again(10));
    s.apply(copy(11, "2026-11-12T00:47:40Z", 9, 1, true));
    assert!(!s.view().saved.unwrap().final_copy);
    // The copy of the end of the next session is the end.
    s.apply(copy(12, "2026-11-12T02:00:00Z", 4, 2, true));
    assert!(s.view().saved.unwrap().final_copy);
}

/// The copy of the end was not saved (or the service is older than it): the newest copy the run's events show was made while the run worked.
/// The panel and the hint name the command with that copy's time and say that later work is not in it; they do not say that the work stays
/// saved.
#[test]
fn b823_the_retry_panel_promises_no_more_than_a_copy_made_while_the_run_worked() {
    use ham_core::retry;
    let saved = SavedCopy {
        at: "2026-11-12T00:47:40Z".into(),
        at_unix: 1_794_444_460,
        count: 8,
        head_sha: HEAD.into(),
        session: 1,
        final_copy: false,
    };
    let words = format!(
        "ham pull {RUN} --snapshot downloads the copy of this run's work saved at 2026-11-12 00:47 UTC; work done after that is not in it."
    );
    let panel = retry::confirmation_with(RUN, Some("zip only"), false, Some(&saved));
    assert!(
        !panel.contains("stays saved"),
        "TRIGGER PRESENT: the copy of the end was not saved and the panel says the work stays saved: {panel}"
    );
    assert!(panel.contains(&words), "{panel}");
    let hint = retry::hint(RUN, Some(&saved));
    assert!(hint.ends_with(&words), "{hint}");
}
