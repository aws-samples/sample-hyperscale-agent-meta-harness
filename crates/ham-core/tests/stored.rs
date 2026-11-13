//! The stored conversation as the chat view shows it: pages of
//! records from the transcript route become blocks, gaps become visible lines, and the three tracks (the engine's records, the user's
//! own items, the harness's notices) are merged for display without changing anyone's origin. Every record is engine origin whatever it
//! says; nothing in a record becomes a role, a card, a key or a command.

#![allow(clippy::unwrap_used)]

use ham_core::chat::{
    Block, BlockKind, ChatModel, GapCause, Origin, PROGRESS_FEED_LABEL, Role, Source,
};
use ham_core::notice::NoticeKind;
use ham_core::stored::{Context, Read, StoredChat, UserItem, compose, uses_stored};
use ham_core::transcript::{
    CompletenessEntry, Dir, GapCode, GapKind, TranscriptGap, TranscriptPage, TranscriptRecord,
    TranscriptState,
};
use serde_json::{Value, json};

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const INC: &str = "01M5A2R7K3C9XJ4D8E6F000001";
const INC2: &str = "01M5A2R7K3C9XJ4D8E6F000002";

fn rec_in(
    seq: u64,
    inc: &str,
    n: u64,
    t: &str,
    dir: Dir,
    kind: &str,
    data: Value,
) -> TranscriptRecord {
    TranscriptRecord {
        seq,
        n,
        incarnation: inc.to_string(),
        origin: ham_core::transcript::Origin::Engine,
        t: Some(t.to_string()),
        dir,
        kind: kind.to_string(),
        data,
    }
}

fn rec(seq: u64, n: u64, kind: &str, data: Value) -> TranscriptRecord {
    let dir = if matches!(kind, "prompt" | "cancel") {
        Dir::Out
    } else {
        Dir::In
    };
    rec_in(
        seq,
        INC,
        n,
        &format!("2026-10-16T08:15:{:02}Z", n % 60),
        dir,
        kind,
        data,
    )
}

fn msg(seq: u64, n: u64, id: &str, text: &str) -> TranscriptRecord {
    rec(
        seq,
        n,
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}}),
    )
}

fn page(state: TranscriptState, records: Vec<TranscriptRecord>) -> TranscriptPage {
    TranscriptPage {
        run_id: RUN.into(),
        state,
        records,
        gaps: vec![],
        completeness: vec![],
        published_through: None,
        listed_through: None,
        next: None,
        prev: None,
        more: false,
        skipped_keys: 0,
    }
}

fn chat_of(pages: &[TranscriptPage]) -> StoredChat {
    let mut c = StoredChat::new();
    for p in pages {
        c.absorb(p, Read::First);
    }
    c
}

fn texts(blocks: &[Block]) -> Vec<String> {
    blocks.iter().map(|b| b.body.clone()).collect()
}

fn progress(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::ProgressFeed, "cline", blocks)
}

/// A ULID made `secs` seconds after 2026-10-16T08:15:00Z, the base time of the records below (a ULID's first ten characters are its
/// millisecond time).
fn ulid_at(secs: u64, tail: u32) -> String {
    let base = harness_protocol::common::parse_utc_unix("2026-10-16T08:15:00Z").unwrap() as u64;
    ham_core::messages::ulid_from(
        (base + secs) * 1000,
        [0, 0, 0, 0, 0, 0, 0, 0, 0, tail as u8],
    )
}

// ---- projection -------------------------------------------------------------------------------------------------------------

#[test]
fn fragments_of_one_message_join_into_one_block_in_order() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![
            msg(0, 0, "m1", "Hello, "),
            msg(0, 1, "m1", "world."),
            msg(0, 2, "m2", "Second message."),
        ],
    )]);
    let blocks = c.blocks();
    assert_eq!(texts(&blocks), ["Hello, world.", "Second message."]);
    assert!(blocks.iter().all(|b| b.kind == BlockKind::Message));
    assert_eq!(blocks[0].id, Block::record_id(INC, 0));
}

#[test]
fn a_tool_call_and_its_updates_are_one_block_with_the_final_status_and_the_output() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![
            rec(
                0,
                0,
                "tool_call",
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "run_commands: pytest -q", "kind": "execute", "status": "pending", "rawInput": {"command": "pytest -q"}}),
            ),
            msg(0, 1, "m1", "Running the tests."),
            rec(
                0,
                2,
                "tool_call_update",
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "3 passed in 0.12s"}}]}),
            ),
        ],
    )]);
    let blocks = c.blocks();
    assert_eq!(blocks.len(), 2, "{blocks:#?}");
    let tool = &blocks[0];
    assert_eq!(tool.kind, BlockKind::ToolCall);
    assert_eq!(tool.head, "run_commands: pytest -q");
    assert_eq!(tool.tool_kind.as_deref(), Some("execute"));
    assert_eq!(tool.status.as_deref(), Some("completed"));
    assert!(tool.folded);
    assert!(tool.body.contains("pytest -q"), "{}", tool.body);
    assert!(tool.body.contains("3 passed in 0.12s"), "{}", tool.body);
    // The message between the call and its update keeps its place.
    assert_eq!(blocks[1].body, "Running the tests.");
}

#[test]
fn a_diff_in_a_tool_call_becomes_a_diff_block_after_it() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![rec(
            0,
            0,
            "tool_call",
            json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "title": "editor: src/app.py", "kind": "edit", "status": "completed",
            "content": [{"type": "diff", "path": "src/app.py", "oldText": "a\nb\nc\n", "newText": "a\nB\nc\n"}]}),
        )],
    )]);
    let blocks = c.blocks();
    let d = blocks
        .iter()
        .find(|b| b.kind == BlockKind::Diff)
        .expect("a diff block");
    assert_eq!(d.head, "src/app.py");
    assert!(d.body.contains("-b") && d.body.contains("+B"), "{}", d.body);
}

#[test]
fn plan_usage_prompt_result_and_cancel_have_blocks() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![
            rec(
                0,
                0,
                "plan",
                json!({"sessionUpdate": "plan", "entries": [{"content": "write tests", "status": "completed", "priority": "high"}, {"content": "write code", "status": "in_progress", "priority": "high"}]}),
            ),
            rec(
                0,
                1,
                "usage_update",
                json!({"sessionUpdate": "usage_update", "used": 12345, "size": 200000}),
            ),
            rec(
                0,
                2,
                "prompt_result",
                json!({"id": 3, "result": {"stopReason": "end_turn"}}),
            ),
            rec(0, 3, "cancel", json!({"sessionId": "s-1"})),
            rec(0, 4, "end", json!({"reason": "final"})),
        ],
    )]);
    let blocks = c.blocks();
    let kinds: Vec<&BlockKind> = blocks.iter().map(|b| &b.kind).collect();
    assert_eq!(kinds[0], &BlockKind::Plan);
    assert!(
        blocks[0].body.contains("[x] write tests") && blocks[0].body.contains("[~] write code")
    );
    assert!(
        blocks[1].body.contains("12,345") && blocks[1].body.contains("200,000"),
        "{}",
        blocks[1].body
    );
    assert!(blocks[2].body.contains("end_turn"), "{}", blocks[2].body);
    assert!(
        blocks[3].body.to_lowercase().contains("cancel"),
        "{}",
        blocks[3].body
    );
    assert_eq!(blocks.len(), 5);
}

#[test]
fn a_prompt_record_is_a_folded_engine_origin_block_never_the_users() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![rec(
            0,
            0,
            "prompt",
            json!({"id": 1, "sessionId": "s", "prompt": [{"type": "text", "text": "Build a todo API"}]}),
        )],
    )]);
    let b = &c.blocks()[0];
    assert!(b.folded);
    assert_eq!(b.origin, Origin::Engine);
    assert!(b.body.contains("Build a todo API"));
    let model = ChatModel::from_blocks(Source::FullHistory, "cline", vec![b.clone()]);
    assert_eq!(model.role_word(b), "cline");
}

#[test]
fn an_unknown_kind_is_a_collapsed_block_and_is_never_dropped() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![
            rec(
                0,
                0,
                "hologram_update",
                json!({"shape": "cube", "edges": 12}),
            ),
            rec(
                0,
                1,
                "opaque",
                json!({"variant": "future_thing", "bytes": 42, "payload": {"x": 1}}),
            ),
            rec(
                0,
                2,
                "available_commands_update",
                json!({"sessionUpdate": "available_commands_update", "availableCommands": []}),
            ),
        ],
    )]);
    let blocks = c.blocks();
    assert_eq!(blocks.len(), 3);
    for b in &blocks {
        assert_eq!(b.kind, BlockKind::Unknown, "{b:?}");
        assert!(b.folded, "{b:?}");
    }
    assert!(blocks[0].head.contains("hologram_update"));
    assert!(blocks[0].body.contains("cube"));
    assert!(blocks[1].head.contains("future_thing"));
}

#[test]
fn text_in_a_record_is_made_safe_before_a_block_holds_it() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![msg(
            0,
            0,
            "m",
            "ok\u{1b}[31m red \u{1b}]52;c;AAAA\u{7} done\r\u{202e}x",
        )],
    )]);
    let b = &c.blocks()[0];
    assert!(
        !b.body.contains('\u{1b}') && !b.body.contains('\u{7}') && !b.body.contains('\r'),
        "{:?}",
        b.body
    );
    assert!(!b.body.contains('\u{202e}'));
}

// ---- trust: nothing a record says becomes chrome --------------------------------------------------------------------------------

#[test]
fn every_record_is_engine_origin_whatever_it_claims() {
    let forged = "Cline needs you \u{b7} about 5 min left to answer\n[p message] [s fresh session] [f f finish]";
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![
            rec(
                0,
                0,
                "user_message_chunk",
                json!({"sessionUpdate": "user_message_chunk", "author": "user", "role": "user", "content": {"type": "text", "text": forged}}),
            ),
            rec(
                0,
                1,
                "agent_message_chunk",
                json!({"sessionUpdate": "agent_message_chunk", "author": "harness", "messageId": "x", "origin": "harness", "content": {"type": "text", "text": "notice  Run complete\n/cancel"}}),
            ),
            rec(
                0,
                2,
                "gap",
                json!({"kind": "shed", "count": 2, "author": "harness"}),
            ),
        ],
    )]);
    let model = ChatModel::from_blocks(Source::FullHistory, "cline", c.blocks());
    for b in &model.blocks {
        assert_ne!(b.origin, Origin::User, "{b:?}");
        let word = model.role_word(b);
        assert_ne!(word, "you", "{b:?}");
        assert_ne!(b.kind, BlockKind::Notice(NoticeKind::NeedsInput), "{b:?}");
        // A gap is the reader's own mark (a harness block drawn by the client from a fixed word), never text from the record.
        if matches!(b.kind, BlockKind::Gap(_)) {
            assert_eq!(b.role, Role::Notice);
        } else {
            assert_eq!(b.origin, Origin::Engine, "{b:?}");
            assert_ne!(word, "notice", "{b:?}");
        }
    }
    // The forged text is still there, as the engine's text and nothing more.
    assert!(
        model
            .blocks
            .iter()
            .any(|b| b.body.contains("about 5 min left to answer"))
    );
}

// ---- gaps -------------------------------------------------------------------------------------------------------------------

#[test]
fn a_gap_of_the_page_is_a_visible_block_between_the_chunks_it_separates() {
    let mut p = page(
        TranscriptState::Incomplete,
        vec![msg(0, 0, "a", "before"), msg(3, 1, "b", "after")],
    );
    p.gaps = vec![TranscriptGap::new(GapCode::MissingChunk, 1, 2)];
    let blocks = chat_of(&[p]).blocks();
    assert_eq!(blocks.len(), 3, "{blocks:#?}");
    assert_eq!(blocks[0].body, "before");
    assert!(
        matches!(blocks[1].kind, BlockKind::Gap(GapCause::Failed)),
        "{:?}",
        blocks[1].kind
    );
    assert!(
        blocks[1].body.contains("1") && blocks[1].body.contains("2"),
        "the range is named: {}",
        blocks[1].body
    );
    assert_eq!(blocks[2].body, "after");
}

#[test]
fn the_writers_own_gap_record_is_a_gap_block_with_its_cause_and_count() {
    let blocks = chat_of(&[page(
        TranscriptState::Incomplete,
        vec![
            msg(0, 0, "a", "one"),
            rec(0, 1, "gap", json!({"kind": "shed", "count": 7, "reason": "queue"})),
            rec(0, 2, "gap", json!({"kind": "tail-unknown", "afterSeq": 0, "reason": "a previous writer ended without a final flush"})),
            rec(0, 3, "gap", json!({"kind": "something-new", "count": 1})),
        ],
    )])
    .blocks();
    assert!(matches!(blocks[1].kind, BlockKind::Gap(GapCause::Shed)));
    assert!(blocks[1].body.contains('7'), "{}", blocks[1].body);
    assert!(matches!(
        blocks[2].kind,
        BlockKind::Gap(GapCause::TailUnknown)
    ));
    // A kind this client has no name for is kept collapsed under its label.
    assert_eq!(blocks[3].kind, BlockKind::Unknown);
}

#[test]
fn a_declared_loss_shows_and_a_declared_exclusion_stays_quiet() {
    let mut p = page(TranscriptState::Incomplete, vec![msg(0, 0, "a", "one")]);
    p.completeness = vec![
        CompletenessEntry {
            kind: GapKind::Excluded,
            from_seq: None,
            to_seq: None,
            count: Some(4),
        },
        CompletenessEntry {
            kind: GapKind::Shed,
            from_seq: Some(1),
            to_seq: Some(1),
            count: Some(3),
        },
        CompletenessEntry {
            kind: GapKind::TailUnknown,
            from_seq: None,
            to_seq: None,
            count: None,
        },
    ];
    let blocks = chat_of(&[p]).blocks();
    let causes: Vec<GapCause> = blocks
        .iter()
        .filter_map(|b| match b.kind {
            BlockKind::Gap(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(
        causes,
        [GapCause::Shed, GapCause::TailUnknown],
        "{blocks:#?}"
    );
    assert!(
        matches!(
            blocks.last().unwrap().kind,
            BlockKind::Gap(GapCause::TailUnknown)
        ),
        "an abnormal end is the last line"
    );
}

#[test]
fn a_gap_seen_on_two_pages_is_shown_once() {
    let mut a = page(TranscriptState::Live, vec![msg(0, 0, "a", "one")]);
    a.gaps = vec![TranscriptGap::new(GapCode::BadGzip, 1, 1)];
    let b = a.clone();
    let blocks = chat_of(&[a, b]).blocks();
    assert_eq!(
        blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Gap(_)))
            .count(),
        1
    );
}

// ---- paging -----------------------------------------------------------------------------------------------------------------

#[test]
fn pages_that_overlap_join_without_a_repeat_or_a_hole() {
    let all: Vec<TranscriptRecord> = (0..6)
        .map(|n| msg(n / 2, n, &format!("m{n}"), &format!("text {n}")))
        .collect();
    let one = chat_of(&[page(TranscriptState::Complete, all.clone())]).blocks();
    let mut c = StoredChat::new();
    c.absorb(
        &page(TranscriptState::Complete, all[2..].to_vec()),
        Read::First,
    );
    c.absorb(
        &page(TranscriptState::Complete, all[..4].to_vec()),
        Read::Older,
    );
    assert_eq!(c.blocks(), one);
}

#[test]
fn the_view_does_not_depend_on_the_order_the_pages_arrived_in() {
    let all: Vec<TranscriptRecord> = (0..8)
        .map(|n| msg(n / 2, n, &format!("m{n}"), &format!("text {n}")))
        .collect();
    let (a, b, c) = (all[0..3].to_vec(), all[3..6].to_vec(), all[6..].to_vec());
    let order = |ps: [&Vec<TranscriptRecord>; 3]| {
        let mut s = StoredChat::new();
        for (i, p) in ps.iter().enumerate() {
            s.absorb(
                &page(TranscriptState::Complete, (*p).clone()),
                if i == 0 { Read::First } else { Read::Older },
            );
        }
        s.blocks()
    };
    assert_eq!(order([&a, &b, &c]), order([&c, &b, &a]));
    assert_eq!(order([&a, &b, &c]), order([&b, &a, &c]));
}

#[test]
fn the_cursors_follow_the_pages() {
    let mut first = page(TranscriptState::Live, vec![msg(5, 10, "a", "x")]);
    first.prev = Some(5);
    first.next = Some("cursor-1".into());
    let mut older = page(TranscriptState::Live, vec![msg(2, 11, "b", "y")]);
    older.prev = Some(2);
    older.next = Some("ignored".into());
    let mut newer = page(TranscriptState::Live, vec![msg(6, 12, "c", "z")]);
    newer.next = Some("cursor-2".into());
    newer.prev = Some(6);
    let mut c = StoredChat::new();
    c.absorb(&first, Read::First);
    assert_eq!(
        (c.older(), c.newer().as_deref()),
        (Some(5), Some("cursor-1"))
    );
    c.absorb(&older, Read::Older);
    assert_eq!(
        (c.older(), c.newer().as_deref()),
        (Some(2), Some("cursor-1"))
    );
    c.absorb(&newer, Read::Newer);
    assert_eq!(
        (c.older(), c.newer().as_deref()),
        (Some(2), Some("cursor-2"))
    );
    assert_eq!(c.state(), Some(TranscriptState::Live));
}

#[test]
fn two_writers_with_the_same_record_numbers_never_share_an_identity() {
    let a = rec_in(
        0,
        INC,
        0,
        "2026-10-16T08:15:01Z",
        Dir::In,
        "agent_message_chunk",
        json!({"messageId": "a", "content": {"type": "text", "text": "from the first writer"}}),
    );
    let b = rec_in(
        1,
        INC2,
        0,
        "2026-10-16T08:15:09Z",
        Dir::In,
        "agent_message_chunk",
        json!({"messageId": "b", "content": {"type": "text", "text": "from the second writer"}}),
    );
    let blocks = chat_of(&[page(TranscriptState::Complete, vec![a, b])]).blocks();
    assert_eq!(blocks.len(), 2);
    assert_ne!(blocks[0].id, blocks[1].id);
}

// ---- which source the view uses -----------------------------------------------------------------------------------------------

#[test]
fn an_older_run_keeps_the_progress_feed_and_its_label() {
    let unsupported = chat_of(&[page(TranscriptState::Unsupported, vec![])]);
    assert!(!uses_stored(&unsupported));
    let feed = progress(vec![Block::message(
        "prompt",
        Role::You,
        Origin::User,
        "Build it",
    )]);
    let cx = Context {
        progress: &feed,
        user: &[],
        live: false,
    };
    let model = compose(&unsupported, &cx);
    assert_eq!(model.source, Source::ProgressFeed);
    assert_eq!(model.label(), Some(PROGRESS_FEED_LABEL));
    assert_eq!(model.blocks, feed.blocks);
    // Nothing read yet, a purged run and an empty store of a run that ended are the progress feed as well.
    assert!(!uses_stored(&StoredChat::new()));
    assert!(!uses_stored(&chat_of(&[page(
        TranscriptState::Purged,
        vec![]
    )])));
    assert!(!uses_stored(&chat_of(&[page(
        TranscriptState::Empty,
        vec![]
    )])));
}

#[test]
fn a_stored_conversation_drops_the_progress_label() {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![msg(0, 0, "a", "Done.")],
    )]);
    assert!(uses_stored(&c));
    let feed = progress(vec![Block::message(
        "prompt",
        Role::You,
        Origin::User,
        "Build it",
    )]);
    let model = compose(
        &c,
        &Context {
            progress: &feed,
            user: &[],
            live: false,
        },
    );
    assert_eq!(model.source, Source::FullHistory);
    assert_eq!(model.label(), None);
}

// ---- the three tracks -------------------------------------------------------------------------------------------------------

#[test]
fn the_prompt_comes_first_notices_and_user_items_are_placed_by_time_and_the_final_cards_are_left_out()
 {
    let c = chat_of(&[page(
        TranscriptState::Complete,
        vec![
            rec_in(
                0,
                INC,
                0,
                "2026-10-16T08:15:01Z",
                Dir::In,
                "agent_message_chunk",
                json!({"messageId": "a", "content": {"type": "text", "text": "first answer"}}),
            ),
            rec_in(
                0,
                INC,
                1,
                "2026-10-16T08:15:20Z",
                Dir::In,
                "agent_message_chunk",
                json!({"messageId": "b", "content": {"type": "text", "text": "second answer"}}),
            ),
            rec_in(
                0,
                INC,
                2,
                "2026-10-16T08:15:40Z",
                Dir::In,
                "agent_message_chunk",
                json!({"messageId": "c", "content": {"type": "text", "text": "third answer"}}),
            ),
        ],
    )]);
    let feed = progress(vec![
        Block::message("prompt", Role::You, Origin::User, "Build a todo API"),
        Block::message(
            &format!("m:{}", ulid_at(1, 1)),
            Role::Engine,
            Origin::Engine,
            "progress-feed copy of the first answer",
        ),
        Block::tool_call(
            "t:abc",
            "run_commands",
            Some("execute"),
            Some("completed"),
            "",
        ),
        Block::notice(
            &format!("n:{}", ulid_at(30, 2)),
            NoticeKind::ProviderRetry,
            "Provider error (temporary): try 1 of 4 in 12 s.",
        ),
        Block::message(
            "final",
            Role::Engine,
            Origin::Engine,
            "the final message again",
        ),
        Block::notice("status", NoticeKind::RunStatus, "Run blocked: x"),
    ]);
    let user = vec![UserItem {
        at: "2026-10-16T08:15:10Z".into(),
        block: Block::message("msg:1", Role::You, Origin::User, "please add tests"),
    }];
    let model = compose(
        &c,
        &Context {
            progress: &feed,
            user: &user,
            live: false,
        },
    );
    let order: Vec<String> = model
        .blocks
        .iter()
        .map(|b| match &b.kind {
            BlockKind::Notice(_) => format!("notice:{}", b.body),
            _ => b.body.clone(),
        })
        .collect();
    assert_eq!(
        order,
        [
            "Build a todo API",
            "first answer",
            "please add tests",
            "second answer",
            "notice:Provider error (temporary): try 1 of 4 in 12 s.",
            "third answer",
        ],
        "{order:#?}"
    );
    // Origins are fixed by the route that served the block.
    assert_eq!(model.blocks[0].origin, Origin::User);
    assert_eq!(model.blocks[2].origin, Origin::User);
    assert_eq!(model.blocks[4].origin, Origin::Harness);
    assert_eq!(model.blocks[1].origin, Origin::Engine);
}

#[test]
fn a_tail_the_store_does_not_hold_yet_is_shown_as_a_labelled_live_line_and_dropped_when_covered() {
    let tail = "I am writing the handler now and then the test for";
    let feed = progress(vec![
        Block::message("prompt", Role::You, Origin::User, "go"),
        Block::message(
            &format!("m:{}", ulid_at(5, 1)),
            Role::Engine,
            Origin::Engine,
            tail,
        ),
    ]);
    // Not stored yet: the line is shown, labelled, and it is the engine's.
    let lag = chat_of(&[page(
        TranscriptState::Live,
        vec![msg(0, 0, "a", "Earlier message.")],
    )]);
    let model = compose(
        &lag,
        &Context {
            progress: &feed,
            user: &[],
            live: true,
        },
    );
    let live = model.blocks.last().unwrap();
    assert_eq!(live.id, "live");
    assert!(live.head.contains("not stored yet"), "{live:?}");
    assert_eq!(live.body, tail);
    assert_eq!(live.origin, Origin::Engine);
    assert!(!live.folded);
    // Stored (inside a longer message, as the last 500 characters of it): dropped.
    let caught_up = chat_of(&[page(
        TranscriptState::Live,
        vec![msg(
            0,
            0,
            "a",
            &format!("Earlier text. {tail} the other module."),
        )],
    )]);
    let model = compose(
        &caught_up,
        &Context {
            progress: &feed,
            user: &[],
            live: true,
        },
    );
    assert!(
        model.blocks.iter().all(|b| b.id != "live"),
        "{:#?}",
        model.blocks
    );
    // A run that ended has no live line.
    let model = compose(
        &lag,
        &Context {
            progress: &feed,
            user: &[],
            live: false,
        },
    );
    assert!(model.blocks.iter().all(|b| b.id != "live"));
}

#[test]
fn the_view_is_a_function_of_the_pages_and_the_events_so_a_reopen_shows_the_same() {
    let records = [
        rec(
            0,
            0,
            "prompt",
            json!({"id": 1, "prompt": [{"type": "text", "text": "go"}]}),
        ),
        msg(0, 1, "m1", "Working on it."),
        rec(
            1,
            2,
            "tool_call",
            json!({"sessionUpdate": "tool_call", "toolCallId": "c", "title": "t", "kind": "read", "status": "completed"}),
        ),
        msg(1, 3, "m2", "Done."),
    ];
    let feed = progress(vec![Block::message(
        "prompt",
        Role::You,
        Origin::User,
        "go",
    )]);
    let build = |split: usize| {
        let mut c = StoredChat::new();
        c.absorb(
            &page(TranscriptState::Complete, records[split..].to_vec()),
            Read::First,
        );
        c.absorb(
            &page(TranscriptState::Complete, records[..split + 1].to_vec()),
            Read::Older,
        );
        compose(
            &c,
            &Context {
                progress: &feed,
                user: &[],
                live: false,
            },
        )
    };
    let whole = build(0);
    for split in 1..4 {
        assert_eq!(build(split), whole, "split at {split}");
    }
}

#[test]
fn what_is_shown_equals_what_is_stored() {
    let records = vec![
        msg(0, 0, "m1", "alpha "),
        msg(0, 1, "m1", "beta"),
        rec(
            0,
            2,
            "tool_call",
            json!({"toolCallId": "c1", "title": "one", "status": "pending"}),
        ),
        rec(
            0,
            3,
            "tool_call_update",
            json!({"toolCallId": "c1", "status": "completed"}),
        ),
        rec(
            0,
            4,
            "tool_call",
            json!({"toolCallId": "c2", "title": "two", "status": "completed"}),
        ),
        msg(1, 5, "m2", "gamma"),
    ];
    let blocks = chat_of(&[page(TranscriptState::Complete, records)]).blocks();
    let said: Vec<&str> = blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Message)
        .map(|b| b.body.as_str())
        .collect();
    assert_eq!(said, ["alpha beta", "gamma"]);
    assert_eq!(
        blocks
            .iter()
            .filter(|b| b.kind == BlockKind::ToolCall)
            .count(),
        2
    );
}
