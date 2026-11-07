//! What the client shows of a run is not cut by numbers nobody chose. A plan and a tool call are listed whole, a record that was
//! written whole is shown whole, the list of changed files reaches 2,000 paths, and a search reaches back to the first run.
#![allow(clippy::unwrap_used, clippy::assertions_on_constants)] // the tests pin the values of constants on purpose

use ham_core::head::{ChangeList, MAX_CHANGE_FILES, MAX_CHANGES_RECORD_BYTES};
use ham_core::stored::{Read, StoredChat};
use ham_core::transcript::{
    Dir, MAX_RECORD_RESPONSE_BYTES, Origin, TranscriptPage, TranscriptRecord, TranscriptState,
};
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const INC: &str = "01M5A2R7K3C9XJ4D8E6F000001";

fn rec(n: u64, kind: &str, data: Value) -> TranscriptRecord {
    TranscriptRecord {
        seq: 0,
        n,
        incarnation: INC.to_string(),
        origin: Origin::Engine,
        t: Some("2026-10-16T08:15:00Z".to_string()),
        dir: Dir::In,
        kind: kind.to_string(),
        data,
    }
}

fn chat_of(records: Vec<TranscriptRecord>) -> StoredChat {
    let page = TranscriptPage {
        run_id: RUN.into(),
        state: TranscriptState::Complete,
        records,
        gaps: vec![],
        completeness: vec![],
        published_through: None,
        listed_through: None,
        next: None,
        prev: None,
        more: false,
        skipped_keys: 0,
    };
    let mut c = StoredChat::new();
    c.absorb(&page, Read::First);
    c
}

#[test]
fn b776_a_plan_of_150_entries_is_listed_whole() {
    let entries: Vec<Value> = (0..150)
        .map(|i| json!({"content": format!("step number {i}"), "status": "pending", "priority": "medium"}))
        .collect();
    let c = chat_of(vec![rec(
        0,
        "plan",
        json!({"sessionUpdate": "plan", "entries": entries}),
    )]);
    let blocks = c.blocks();
    let body = &blocks[0].body;
    assert!(body.contains("step number 99"), "{body}");
    assert!(
        body.contains("step number 100"),
        "entry 101 was left out: {body}"
    );
    assert!(
        body.contains("step number 149"),
        "the last entry was left out"
    );
}

#[test]
fn b776_a_tool_call_with_80_edits_shows_all_of_them() {
    let items: Vec<Value> = (0..80)
        .map(|i| json!({"type": "diff", "path": format!("src/file_{i}.txt"), "oldText": "a", "newText": "b"}))
        .collect();
    let c = chat_of(vec![
        rec(
            0,
            "tool_call",
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit many files", "kind": "edit", "status": "pending"}),
        ),
        rec(
            1,
            "tool_call_update",
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed", "content": items}),
        ),
    ]);
    let all: String = c
        .blocks()
        .iter()
        .map(|b| b.body.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("src/file_49.txt"), "{all}");
    assert!(
        all.contains("src/file_50.txt"),
        "the 51st edit was left out"
    );
    assert!(
        all.contains("src/file_79.txt"),
        "the last edit was left out"
    );
}

#[test]
fn b776_a_block_as_long_as_the_longest_final_message_is_shown_whole() {
    assert_eq!(ham_core::chat::MAX_BODY_CHARS, 200_000);
    let text = "w".repeat(190_000);
    let c = chat_of(vec![rec(
        0,
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "text", "text": text}}),
    )]);
    let blocks = c.blocks();
    assert_eq!(
        blocks[0].body.chars().filter(|c| *c == 'w').count(),
        190_000
    );
    assert!(!blocks[0].truncated);
}

#[test]
fn b776_a_record_is_shown_whole_up_to_the_page_budget_of_the_route() {
    assert_eq!(MAX_RECORD_RESPONSE_BYTES, 3_000_000 - 64 * 1024);
}

fn changes_record(files: usize) -> Vec<u8> {
    let list: Vec<Value> = (0..files)
        .map(|i| json!({"path": format!("src/module_{i}/file.rs"), "status": "M"}))
        .collect();
    serde_json::to_vec(&json!({
        "schemaVersion": 1, "runId": RUN, "headSha": HEAD, "base": {"kind": "empty-tree"},
        "total": files, "omitted": 0, "files": list
    }))
    .unwrap()
}

#[test]
fn b776_the_list_of_changed_files_reaches_2000_paths() {
    assert_eq!(MAX_CHANGE_FILES, 2_000);
    assert!(MAX_CHANGES_RECORD_BYTES >= 4 * 1024 * 1024);
    let l = ChangeList::from_record(&changes_record(2_000), RUN, HEAD).unwrap();
    assert_eq!(l.files.len(), 2_000);
    assert!(ChangeList::from_record(&changes_record(2_001), RUN, HEAD).is_err());
}
