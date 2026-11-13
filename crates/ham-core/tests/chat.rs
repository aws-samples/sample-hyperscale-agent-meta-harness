//! The chat view model built from today's data (the prompt, Cline's message tails, tool rows, notices, the final message
//! and the delivery card) as ordered blocks, labelled "progress feed only: not the full conversation". The same `Block` type takes
//! the full-history source of a later issue: message, thought, tool call with a folded body, diff, plan, usage, notice, and
//! unknown kinds kept collapsed and never dropped.

#![allow(clippy::unwrap_used)]

use ham_core::chat::{
    Block, BlockKind, ChatInputs, ChatModel, FinalMessage, GapCause, MAX_BODY_CHARS, Origin,
    PROGRESS_FEED_LABEL, Role, Source, final_message_from_evidence, stitch_tail,
};
use ham_core::notice::NoticeKind;
use ham_core::state::RunState;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use proptest::prelude::*;
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
const RECORDED: &str = include_str!("fixtures/tool-events-recorded.ndjson");

fn ulid(n: u64) -> String {
    const A: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut s = String::from("01J9ZZZZZZ");
    let mut v = n;
    let mut tail = [b'0'; 16];
    for i in (0..16).rev() {
        tail[i] = A[(v % 32) as usize];
        v /= 32;
    }
    s.push_str(std::str::from_utf8(&tail).unwrap());
    s
}

fn ev(n: u64, t: EventType, authority: Authority, agent: Option<&str>, payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: ulid(n),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: n,
        ts: "2026-10-16T08:00:00Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: payload },
    }
}

fn agent(n: u64, t: EventType, payload: Value) -> Envelope {
    ev(n, t, Authority::Agent, Some("direct"), payload)
}

fn harness(n: u64, t: EventType, payload: Value) -> Envelope {
    ev(n, t, Authority::Harness, None, payload)
}

fn tail(n: u64, text: &str) -> Envelope {
    agent(
        n,
        EventType::TurnProgress,
        json!({"turn": 1, "kind": "message", "text": text}),
    )
}

fn call(n: u64, id: &str, title: &str, kind: &str) -> Envelope {
    agent(
        n,
        EventType::ToolSummary,
        json!({"toolCallId": id, "title": title, "kind": kind, "status": "pending"}),
    )
}

fn update(n: u64, id: &str, status: &str) -> Envelope {
    agent(
        n,
        EventType::TurnProgress,
        json!({"toolCallId": id, "status": status}),
    )
}

fn state_of(events: impl IntoIterator<Item = Envelope>) -> RunState {
    let mut s = RunState::new(RUN);
    for e in events {
        s.apply(e);
    }
    s
}

fn session(n: u64) -> Envelope {
    harness(
        n,
        EventType::SessionStarted,
        json!({"agentId": "direct", "engine": "cline", "modelId": "global.anthropic.claude-sonnet-5-5", "contextTokens": 1000000}),
    )
}

fn feed(events: impl IntoIterator<Item = Envelope>, inputs: &ChatInputs<'_>) -> ChatModel {
    ChatModel::from_progress_feed(&state_of(events), inputs)
}

fn last_chars(text: &str, n: usize) -> String {
    let cs: Vec<char> = text.chars().collect();
    cs[cs.len().saturating_sub(n)..].iter().collect()
}

/// The text the collector holds `cut` characters into a message, and the tail it would send then.
fn long_message() -> String {
    (1..=18)
        .map(|i| format!("Step {i:02}: the parser handles case {i:02} and its test passes."))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_prompt_is_the_first_block_and_comes_from_the_user() {
    let m = feed(
        [session(1)],
        &ChatInputs {
            prompt: Some("Build a todo API with tests"),
            ..ChatInputs::default()
        },
    );
    let b = &m.blocks[0];
    assert_eq!(
        (b.id.as_str(), b.role, b.origin),
        ("prompt", Role::You, Origin::User)
    );
    assert_eq!(b.kind, BlockKind::Message);
    assert_eq!(b.body, "Build a todo API with tests");
    assert_eq!(m.role_word(b), "you");
}

#[test]
fn a_run_with_no_prompt_and_no_events_has_no_blocks() {
    let m = feed([], &ChatInputs::default());
    assert!(m.blocks.is_empty());
    assert_eq!(m.label(), Some(PROGRESS_FEED_LABEL));
}

#[test]
fn the_progress_feed_says_so_and_a_full_history_does_not() {
    assert_eq!(
        PROGRESS_FEED_LABEL,
        "progress feed only: not the full conversation"
    );
    let feed = feed([session(1), tail(2, "hi")], &ChatInputs::default());
    assert_eq!(feed.source, Source::ProgressFeed);
    assert_eq!(feed.label(), Some(PROGRESS_FEED_LABEL));
    let full = ChatModel::from_blocks(Source::FullHistory, "cline", vec![]);
    assert_eq!(full.label(), None);
}

#[test]
fn tails_of_one_message_are_stitched_into_one_engine_block() {
    let full = long_message();
    let cuts = [150usize, 400, 700, 1000, full.chars().count()];
    let mut events = vec![session(1)];
    for (i, cut) in cuts.iter().enumerate() {
        let so_far: String = full.chars().take(*cut).collect();
        events.push(tail(10 + i as u64, &last_chars(&so_far, 500)));
    }
    let m = feed(events, &ChatInputs::default());
    assert_eq!(m.blocks.len(), 1);
    let b = &m.blocks[0];
    assert_eq!(
        (b.role, b.origin, &b.kind),
        (Role::Engine, Origin::Engine, &BlockKind::Message)
    );
    assert_eq!(b.body, full);
    assert_eq!(
        b.id,
        format!("m:{}", ulid(10)),
        "the id is the first tail's event id"
    );
}

#[test]
fn a_repeated_tail_changes_nothing() {
    let m = feed(
        [
            tail(1, "I will read the repo."),
            tail(2, "I will read the repo."),
        ],
        &ChatInputs::default(),
    );
    assert_eq!(m.blocks.len(), 1);
    assert_eq!(m.blocks[0].body, "I will read the repo.");
}

#[test]
fn stitching_overlaps_contains_and_marks_a_gap() {
    assert_eq!(stitch_tail("", "abc"), "abc");
    assert_eq!(stitch_tail("abc", "abc"), "abc");
    assert_eq!(
        stitch_tail("Hello wor", "Hello world, again"),
        "Hello world, again"
    );
    assert_eq!(
        stitch_tail(
            "The quick brown fox jumps over",
            "brown fox jumps over the lazy dog"
        ),
        "The quick brown fox jumps over the lazy dog"
    );
    // The new tail lies inside what is held.
    assert_eq!(
        stitch_tail("one two three four", "two three"),
        "one two three four"
    );
    // Nothing in common: the engine wrote more than a tail between two progress events.
    assert_eq!(
        stitch_tail("The first part.", "A far later part."),
        "The first part. \u{2026} A far later part."
    );
    // A one-character overlap is no evidence.
    assert_eq!(stitch_tail("abcx", "xyz"), "abcx \u{2026} xyz");
}

#[test]
fn a_tool_call_ends_a_message_and_the_next_tail_starts_another() {
    let m = feed(
        [
            session(1),
            tail(2, "I'll start by reading the repo."),
            call(3, "c1", "run_commands: pytest -q", "execute"),
            update(4, "c1", "completed"),
            tail(5, "Tests pass."),
        ],
        &ChatInputs {
            prompt: Some("Build X"),
            ..ChatInputs::default()
        },
    );
    let words: Vec<String> = m.blocks.iter().map(|b| m.role_word(b)).collect();
    assert_eq!(words, ["you", "cline", "tool", "cline"]);
    assert_eq!(m.blocks[1].body, "I'll start by reading the repo.");
    assert_eq!(m.blocks[3].body, "Tests pass.");
}

#[test]
fn a_tool_block_has_the_title_the_kind_and_the_status_and_is_folded() {
    let m = feed(
        [
            call(1, "c1", "run_commands: pytest -q", "execute"),
            update(2, "c1", "in_progress"),
            update(3, "c1", "failed"),
        ],
        &ChatInputs::default(),
    );
    let b = &m.blocks[0];
    assert_eq!(b.kind, BlockKind::ToolCall);
    assert_eq!((b.role, b.origin), (Role::Tool, Origin::Engine));
    assert_eq!(b.head, "run_commands: pytest -q");
    assert_eq!(b.tool_kind.as_deref(), Some("execute"));
    assert_eq!(b.status.as_deref(), Some("failed"));
    assert!(b.folded);
    assert_eq!(b.id, "t:c1");
    assert_eq!(m.blocks.len(), 1, "an update changes the row, it adds none");
}

#[test]
fn the_recorded_tool_events_give_four_tool_blocks_in_order() {
    let mut s = RunState::new(RUN);
    for line in RECORDED.lines() {
        s.apply(serde_json::from_str::<Envelope>(line).unwrap());
    }
    let m = ChatModel::from_progress_feed(&s, &ChatInputs::default());
    let tools: Vec<&Block> = m
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::ToolCall)
        .collect();
    assert_eq!(tools.len(), 4);
    assert!(
        tools
            .iter()
            .all(|b| b.status.as_deref() == Some("completed"))
    );
    assert!(tools[0].head.starts_with("read_files: "));
    // The recording also holds one permission request per call (the earlier line asked); each is a notice before its tool row.
    let others: Vec<&BlockKind> = m
        .blocks
        .iter()
        .map(|b| &b.kind)
        .filter(|k| **k != BlockKind::ToolCall)
        .collect();
    assert_eq!(others.len(), 4);
    assert!(
        others
            .iter()
            .all(|k| **k == BlockKind::Notice(NoticeKind::Permission))
    );
}

#[test]
fn notices_are_notice_blocks_in_event_order() {
    let m = feed(
        [
            tail(1, "Working."),
            agent(
                2,
                EventType::TurnProgress,
                json!({"turn": 1, "kind": "provider-retry", "attempt": 1, "of": 4, "class": "transient", "backoffS": 5, "detail": "Throttled"}),
            ),
            tail(3, "Back at it."),
        ],
        &ChatInputs::default(),
    );
    let words: Vec<String> = m.blocks.iter().map(|b| m.role_word(b)).collect();
    // No session event in this stream: the engine word is the neutral one.
    assert_eq!(words, ["engine", "notice", "engine"]);
    let n = &m.blocks[1];
    assert_eq!(
        (n.origin, &n.kind),
        (
            Origin::Harness,
            &BlockKind::Notice(NoticeKind::ProviderRetry)
        )
    );
    assert_eq!(
        n.body,
        "Provider error (temporary): try 1 of 4 in 5 s. Throttled"
    );
    assert_eq!(n.id, format!("n:{}", ulid(2)));
}

#[test]
fn an_unknown_progress_kind_is_kept_as_a_collapsed_unknown_block() {
    let m = feed(
        [agent(
            1,
            EventType::TurnProgress,
            json!({"turn": 1, "kind": "future-kind", "detail": "something new", "n": 3}),
        )],
        &ChatInputs::default(),
    );
    let b = &m.blocks[0];
    assert_eq!(b.kind, BlockKind::Unknown);
    assert_eq!(b.head, "future-kind");
    assert!(b.folded);
    assert!(b.body.contains("something new"), "{}", b.body);
    assert_eq!(m.role_word(b), "notice");
}

#[test]
fn the_final_message_replaces_the_last_tail_and_ends_the_conversation() {
    let final_text = "All done. I added percent_change() and three tests; the suite passes.";
    let m = feed(
        [
            session(1),
            call(2, "c1", "run_commands: pytest -q", "execute"),
            update(3, "c1", "completed"),
            tail(
                4,
                "I added percent_change() and three tests; the suite passes.",
            ),
        ],
        &ChatInputs {
            final_message: FinalMessage::Text(final_text),
            ..ChatInputs::default()
        },
    );
    let last = m.blocks.last().unwrap();
    assert_eq!(
        (last.id.as_str(), last.role, last.origin),
        ("final", Role::Engine, Origin::Engine)
    );
    assert_eq!(last.body, final_text);
    assert_eq!(m.blocks.len(), 2, "the tail it replaced is gone");
}

#[test]
fn a_final_message_after_a_different_last_block_is_appended() {
    let m = feed(
        [call(1, "c1", "editor: x", "edit")],
        &ChatInputs {
            final_message: FinalMessage::Text("Finished."),
            ..ChatInputs::default()
        },
    );
    let words: Vec<String> = m.blocks.iter().map(|b| m.role_word(b)).collect();
    assert_eq!(words, ["tool", "engine"]);
}

#[test]
fn no_final_message_is_said_for_a_delivered_run_and_only_then() {
    let delivered = [
        session(1),
        harness(
            2,
            EventType::DeliveryReady,
            json!({"artifactId": "ART1", "sha256": "a".repeat(64), "sizeBytes": 1234}),
        ),
        harness(
            3,
            EventType::RunCompleted,
            json!({"status": "complete", "outcome": "reported_done"}),
        ),
    ];
    let m = feed(
        delivered.clone(),
        &ChatInputs {
            final_message: FinalMessage::Unavailable,
            ..ChatInputs::default()
        },
    );
    assert!(
        m.blocks
            .iter()
            .any(|b| b.body == "final message not available"),
        "{:?}",
        m.blocks.iter().map(|b| b.body.as_str()).collect::<Vec<_>>()
    );
    let live = feed(
        [session(1), tail(2, "Working.")],
        &ChatInputs {
            final_message: FinalMessage::Unavailable,
            ..ChatInputs::default()
        },
    );
    assert!(
        live.blocks
            .iter()
            .all(|b| b.body != "final message not available")
    );
    let not_asked = feed(delivered, &ChatInputs::default());
    assert!(
        not_asked
            .blocks
            .iter()
            .all(|b| b.body != "final message not available")
    );
}

#[test]
fn the_delivery_card_is_the_last_block_and_never_holds_a_link() {
    let m = feed(
        [
            session(1),
            tail(2, "Done."),
            harness(
                3,
                EventType::DeliveryPr,
                json!({"url": "https://git-codecommit.us-east-1.amazonaws.com/v1/repos/x/pull-requests/7"}),
            ),
            harness(
                4,
                EventType::DeliveryReady,
                json!({"artifactId": "ART1", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
            ),
            harness(
                5,
                EventType::RunCompleted,
                json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
            ),
        ],
        &ChatInputs {
            zip_path: Some("/Users/me/Downloads/run-ART1.zip"),
            ..ChatInputs::default()
        },
    );
    let d = m.blocks.last().unwrap();
    assert_eq!(
        (d.kind.clone(), d.origin),
        (BlockKind::Delivery, Origin::Harness)
    );
    assert_eq!(d.id, "delivery");
    for want in [
        "Outcome  Cline reported done",
        "Model    Claude Sonnet 5.5",
        "PR       https://git-codecommit.us-east-1.amazonaws.com/v1/repos/x/pull-requests/7",
        "Zip      /Users/me/Downloads/run-ART1.zip (sha256 abababababab, 2048 bytes)",
    ] {
        assert!(d.body.lines().any(|l| l == want), "{want}\n---\n{}", d.body);
    }
    assert!(!d.body.contains("Link"), "{}", d.body);
    assert!(!d.body.contains("X-Amz"));
}

#[test]
fn a_run_that_ended_early_or_is_blocked_says_so_in_a_notice() {
    let m = feed(
        [
            session(1),
            harness(
                2,
                EventType::GoalBlocked,
                json!({"reason": "the session ended before any work was saved"}),
            ),
        ],
        &ChatInputs::default(),
    );
    let n = m.blocks.last().unwrap();
    assert_eq!(n.kind, BlockKind::Notice(NoticeKind::RunStatus));
    assert_eq!(n.origin, Origin::Harness);
    assert_eq!(
        n.body,
        "Run blocked: the session ended before any work was saved"
    );
}

#[test]
fn block_ids_stay_the_same_when_events_arrive() {
    let first = [
        session(1),
        tail(2, "Reading."),
        call(3, "c1", "run_commands: ls", "execute"),
    ];
    let a = feed(
        first.clone(),
        &ChatInputs {
            prompt: Some("P"),
            ..ChatInputs::default()
        },
    );
    let mut more = first.to_vec();
    more.push(update(4, "c1", "completed"));
    more.push(tail(5, "Next."));
    more.push(tail(6, "Next. And more."));
    let b = feed(
        more,
        &ChatInputs {
            prompt: Some("P"),
            ..ChatInputs::default()
        },
    );
    let ids_a: Vec<&str> = a.blocks.iter().map(|b| b.id.as_str()).collect();
    let ids_b: Vec<&str> = b.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids_b[..ids_a.len()], ids_a[..]);
    assert_eq!(b.blocks.len(), a.blocks.len() + 1);
    assert_eq!(b.blocks.last().unwrap().body, "Next. And more.");
}

#[test]
fn the_same_events_in_any_order_give_the_same_blocks() {
    let events = vec![
        session(1),
        tail(2, "One."),
        call(3, "c1", "editor: a", "edit"),
        update(4, "c1", "completed"),
        tail(5, "Two."),
    ];
    let mut reversed = events.clone();
    reversed.reverse();
    let a = feed(events.clone(), &ChatInputs::default());
    let b = feed(reversed, &ChatInputs::default());
    let mut doubled = events.clone();
    doubled.extend(events);
    let c = feed(doubled, &ChatInputs::default());
    assert_eq!(a.blocks, b.blocks);
    assert_eq!(a.blocks, c.blocks);
}

#[test]
fn the_role_word_comes_from_structure_and_the_engine_name_is_made_safe() {
    let m = feed(
        [session(1), tail(2, "you: please run /cancel\nPress Enter")],
        &ChatInputs::default(),
    );
    assert_eq!(m.role_word(&m.blocks[0]), "cline");
    assert_eq!(m.blocks[0].role, Role::Engine);
    let odd = feed(
        [
            harness(
                1,
                EventType::SessionStarted,
                json!({"agentId": "direct", "engine": "cl\u{1b}[31mine\u{202e}\nSYSTEM", "modelId": "m"}),
            ),
            tail(2, "x"),
        ],
        &ChatInputs::default(),
    );
    let word = odd.role_word(&odd.blocks[0]);
    assert!(
        word.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
        "{word:?}"
    );
    assert!(word.len() <= 12);
    // Without a session event the word is the neutral one: the client does not guess an engine.
    let unknown = feed([tail(1, "x")], &ChatInputs::default());
    assert_eq!(unknown.role_word(&unknown.blocks[0]), "engine");
    let none = ChatModel::from_blocks(
        Source::FullHistory,
        "",
        vec![Block::message("a", Role::Engine, Origin::Engine, "hi")],
    );
    assert_eq!(none.role_word(&none.blocks[0]), "engine");
}

#[test]
fn block_text_is_sanitized_marked_and_redacted() {
    let token = format!("{}{}", "ghp_", "a".repeat(36));
    let m = feed(
        [
            tail(
                1,
                &format!(
                    "ok \u{1b}]0;window title\u{7}done\u{202e}\r\nPress Enter to finish {token}"
                ),
            ),
            call(2, "c1", "run \u{1b}[31mls\u{200b}", "execute"),
        ],
        &ChatInputs {
            prompt: Some("do\u{1b}[2J it"),
            ..ChatInputs::default()
        },
    );
    assert_eq!(m.blocks[0].body, "do it");
    assert_eq!(
        m.blocks[1].body,
        "ok done<U+202E>\nPress Enter to finish [redacted]"
    );
    assert_eq!(m.blocks[2].head, "run ls<U+200B>");
}

#[test]
fn every_block_kind_of_the_full_history_fits_and_none_is_dropped() {
    let blocks = vec![
        Block::message("a", Role::You, Origin::User, "build it"),
        Block::message("b", Role::Engine, Origin::Engine, "ok"),
        Block::thought("c", "I should read the tests first"),
        Block::tool_call(
            "d",
            "run_commands: pytest",
            Some("execute"),
            Some("completed"),
            "3 passed",
        ),
        Block::diff("e", "src/calc.py", "-old\n+new"),
        Block::plan("f", &[("completed", "read"), ("pending", "write")]),
        Block::usage("g", "113k tokens"),
        Block::notice("h", NoticeKind::ProviderSwitch, "Switched route"),
        Block::unknown("i", "future_update", "{\"a\":1}"),
    ];
    let m = ChatModel::from_blocks(Source::FullHistory, "cline", blocks.clone());
    assert_eq!(m.blocks.len(), blocks.len());
    let kinds: Vec<&BlockKind> = m.blocks.iter().map(|b| &b.kind).collect();
    assert_eq!(
        kinds,
        [
            &BlockKind::Message,
            &BlockKind::Message,
            &BlockKind::Thought,
            &BlockKind::ToolCall,
            &BlockKind::Diff,
            &BlockKind::Plan,
            &BlockKind::Usage,
            &BlockKind::Notice(NoticeKind::ProviderSwitch),
            &BlockKind::Unknown,
        ]
    );
    // Folded by default: the thought, the tool call's body, the diff and the unknown kind. Messages and notices are open.
    let folded: Vec<bool> = m.blocks.iter().map(|b| b.folded).collect();
    assert_eq!(
        folded,
        [false, false, true, true, true, false, false, false, true]
    );
    assert_eq!(m.blocks[3].head, "run_commands: pytest");
    assert_eq!(m.blocks[3].body, "3 passed");
    assert_eq!(m.blocks[5].body, "[x] read\n[ ] write");
    assert_eq!(m.blocks[8].head, "future_update");
    assert_eq!(m.label(), None);
}

#[test]
fn a_block_revision_changes_with_its_text_only() {
    let a = Block::message("a", Role::Engine, Origin::Engine, "same");
    let b = Block::message("a", Role::Engine, Origin::Engine, "same");
    let c = Block::message("a", Role::Engine, Origin::Engine, "other");
    assert_eq!(a.rev, b.rev);
    assert_ne!(a.rev, c.rev);
}

#[test]
fn the_final_message_is_read_from_the_evidence_artifact() {
    // evidence/direct.json: the delivery decision with agentd's whole summary.json (crates/run-session plan_delivery EvidenceFile).
    let direct = json!({
        "schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "label": "Cline reported done",
        "summary": {"clineMessage": "Added percent_change.\nRan the suite: 3 passed.", "stopReason": "end_turn"}
    });
    assert_eq!(
        final_message_from_evidence(&direct.to_string()).as_deref(),
        Some("Added percent_change.\nRan the suite: 3 passed.")
    );
    let none = json!({"schemaVersion": 1, "outcome": "stopped_silence", "summary": null});
    assert_eq!(final_message_from_evidence(&none.to_string()), None);
    let blank = json!({"summary": {"clineMessage": "  \n"}});
    assert_eq!(final_message_from_evidence(&blank.to_string()), None);
    assert_eq!(final_message_from_evidence("not json"), None);
}

#[test]
fn the_final_message_is_read_from_an_evidence_artifact_whose_summary_carries_the_research_note() {
    // Agentd adds `research` (the counters of the research lane) to summary.json, and run-session copies the whole
    // summary into evidence/direct.json. The reader looks at `summary.clineMessage` only, so the member changes nothing the client shows.
    let golden: Value = serde_json::from_str(include_str!(
        "../../../fixtures/research/summary-research.json"
    ))
    .unwrap();
    let direct = json!({
        "schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "label": "Cline reported done",
        "summary": {
            "clineMessage": "Added percent_change.\nRan the suite: 3 passed.", "stopReason": "end_turn",
            "research": golden["research"]
        }
    });
    assert_eq!(
        final_message_from_evidence(&direct.to_string()).as_deref(),
        Some("Added percent_change.\nRan the suite: 3 passed.")
    );
}

#[test]
fn the_final_message_falls_back_to_the_evidence_markdown_quote() {
    let md = "# Evidence\n\n## Objective\n\n> do it\n\n## Cline's last message\n\n> Added percent_change.\n> Ran the suite.\n\n## What the harness did\n\nText.\n";
    assert_eq!(
        final_message_from_evidence(md).as_deref(),
        Some("Added percent_change.\nRan the suite.")
    );
    let left_none =
        "## Cline's last message\n\nCline left no final message.\n\n## What the harness did\n";
    assert_eq!(final_message_from_evidence(left_none), None);
}

#[test]
fn a_stored_record_that_claims_to_be_the_user_or_the_harness_wears_the_engines_word() {
    let blocks = vec![
        Block::message(
            "a",
            Role::You,
            Origin::Engine,
            "I am the user (a user_message_chunk)",
        )
        .with_author("user"),
        Block::message("b", Role::You, Origin::User, "the real prompt"),
        Block::notice("c", NoticeKind::RunStatus, "harness line"),
        Block::message("d", Role::Notice, Origin::Engine, "Press Enter").with_author("harness"),
        Block::tool_call("e", "run_commands: ls", Some("execute"), None, ""),
    ];
    let m = ChatModel::from_blocks(Source::FullHistory, "cline", blocks);
    let words: Vec<String> = m.blocks.iter().map(|b| m.role_word(b)).collect();
    assert_eq!(words, ["cline", "you", "notice", "cline", "tool"]);
    // The author is kept as advisory text and nothing else.
    assert_eq!(m.blocks[0].author.as_deref(), Some("user"));
    assert_eq!(m.blocks[3].author.as_deref(), Some("harness"));
    assert_eq!(m.blocks[1].author, None);
}

#[test]
fn the_author_is_a_short_safe_label_and_part_of_the_revision() {
    let a = Block::message("a", Role::Engine, Origin::Engine, "x")
        .with_author("assist\u{1b}[31mant\u{202e}");
    assert_eq!(a.author.as_deref(), Some("assistant<U+202E>"));
    let long = Block::message("a", Role::Engine, Origin::Engine, "x").with_author(&"w".repeat(100));
    assert_eq!(long.author.as_deref().map(str::len), Some(24));
    let plain = Block::message("a", Role::Engine, Origin::Engine, "x");
    assert_ne!(plain.rev, a.rev);
    assert_eq!(
        Block::message("a", Role::Engine, Origin::Engine, "x")
            .with_author("  ")
            .author,
        None
    );
}

#[test]
fn a_gap_is_a_harness_block_that_names_its_cause_and_never_a_silence() {
    for cause in [
        GapCause::Excluded,
        GapCause::Truncated,
        GapCause::Shed,
        GapCause::Suppressed,
        GapCause::Failed,
        GapCause::Conflict,
        GapCause::Unsupported,
        GapCause::TailUnknown,
        GapCause::Unreadable,
    ] {
        let g = Block::gap("g1", cause, None);
        assert_eq!(
            (g.kind.clone(), g.origin, g.role),
            (BlockKind::Gap(cause), Origin::Harness, Role::Notice)
        );
        assert!(
            g.body.starts_with('[') && g.body.ends_with(']'),
            "{}",
            g.body
        );
    }
    assert_eq!(
        Block::gap("g", GapCause::Unreadable, None).body,
        "[chunk unreadable]"
    );
    assert_eq!(
        Block::gap("g", GapCause::Truncated, Some("records 40 to 52")).body,
        "[cut: a size limit] records 40 to 52"
    );
}

#[test]
fn two_writers_that_overlapped_never_share_a_block_id() {
    let a = Block::record_id("01M5A2R7K3C9XJ4D8E6FQ0WT1N", 0);
    let b = Block::record_id("01M5A2R9X8B4NCD6Q3H7YT2VKP", 0);
    assert_eq!(a, "r:01M5A2R7K3C9XJ4D8E6FQ0WT1N:0");
    assert_ne!(a, b);
    assert_eq!(Block::record_id("../x\n", 7), "r:x:7");
}

#[test]
fn a_flooded_body_is_cut_at_the_cap_and_says_how_much_is_missing() {
    let big = "y".repeat(MAX_BODY_CHARS + 25);
    let b = Block::message("a", Role::Engine, Origin::Engine, &big);
    assert!(b.truncated);
    assert!(b.body.ends_with("[25 more characters not shown]"));
    assert_eq!(b.body.chars().filter(|c| *c == 'y').count(), MAX_BODY_CHARS);
    let ok = Block::message(
        "a",
        Role::Engine,
        Origin::Engine,
        &"y".repeat(MAX_BODY_CHARS),
    );
    assert!(!ok.truncated);
}

proptest! {
    #[test]
    fn stitched_text_always_ends_with_the_newest_tail(a in ".{0,80}", b in ".{1,80}") {
        let out = stitch_tail(&a, &b);
        prop_assert!(out.ends_with(&b) || out == a);
    }

    #[test]
    fn no_block_text_holds_a_control_character_or_an_escape(raw in ".{0,120}") {
        let m = ChatModel::from_progress_feed(
            &state_of([
                tail(1, &raw),
                call(2, "c1", &raw, "execute"),
                agent(3, EventType::TurnProgress, json!({"turn": 1, "kind": "provider-retry", "attempt": 1, "of": 2, "class": "transient", "backoffS": 1, "detail": raw})),
            ]),
            &ChatInputs { prompt: Some(&raw), final_message: FinalMessage::Text(&raw), zip_path: Some(&raw) },
        );
        for b in &m.blocks {
            for t in [&b.head, &b.body] {
                prop_assert!(!t.chars().any(|c| (c < ' ' && c != '\n' && c != '\t') || ('\u{7f}'..='\u{9f}').contains(&c)), "{:?}", t);
            }
        }
    }
}
