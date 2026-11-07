//! `/export` writes a chat as Markdown. `tui::export` builds the text (headings and metadata come from the model's
//! structure, never from block text; a message is quoted line by line so its own `## you` or `---` cannot become structure; tool
//! output and diffs sit in fences longer than anything inside them; raw HTML, images and link definitions in engine text are made
//! literal; presigned links and bearer material are masked) and writes the file: a temp file in the destination directory, a hard
//! link to the final name so an existing file is never replaced, the temp name removed. A name that is taken gets `-2`, `-3`.
#![allow(clippy::unwrap_used)]

use ham::tui::export::{
    Destination, Meta, file_name, neutralize, notice, resolve, to_markdown, write, write_with,
};
use ham_core::chat::{Block, ChatModel, GapCause, Origin, PROGRESS_FEED_LABEL, Role, Source};
use ham_core::notice::NoticeKind;
use ham_core::sanitize::display_text;
use ham_core::stored::{Context, Read as StoredRead, StoredChat, UserItem, compose, uses_stored};
use ham_core::transcript::{
    CHUNK_VERSION, Chunk, ChunkHeader, ChunkRecord, Dir, Origin as RecordOrigin, TranscriptPage,
    TranscriptRecord, TranscriptState, chunk_gzip_stored, decode_chunk, parse_page,
};
use proptest::prelude::*;
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ham-export-{label}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

fn meta() -> Meta<'static> {
    Meta {
        run_id: "01M5287JABCDEF",
        model: Some("Claude Sonnet 5.5"),
        exported_at: "2026-10-16 17:40:02 UTC",
    }
}

fn model(blocks: Vec<Block>) -> ChatModel {
    ChatModel::from_blocks(Source::FullHistory, "cline", blocks)
}

fn conversation() -> ChatModel {
    model(vec![
        Block::message(
            "p",
            Role::You,
            Origin::User,
            "Build a todo API\n\nwith tests first",
        ),
        Block::message(
            "a1",
            Role::Engine,
            Origin::Engine,
            "I will start with the tests.\n\n```python\ndef test_add():\n    assert add(1, 2) == 3\n```\n\n- then the code",
        ),
        Block::tool_call(
            "t1",
            "run_commands: pytest -q",
            Some("execute"),
            Some("completed"),
            "12 passed in 0.31s",
        ),
        Block::diff("d1", "src/todo.py", "@@ -1 +1 @@\n-x = 1\n+x = 2"),
        Block::thought("th", "maybe a dict is enough"),
        Block::gap("g", GapCause::Truncated, None),
        Block::notice("n", NoticeKind::RunStatus, "Run complete"),
        Block::message("a2", Role::Engine, Origin::Engine, "All green."),
    ])
}

// ---- the text ---------------------------------------------------------------------------------------------------------------------

#[test]
fn the_export_has_a_title_the_run_facts_and_every_block_in_order() {
    let md = to_markdown(&conversation(), &meta());
    insta::assert_snapshot!("export_conversation", md);
    let heads: Vec<&str> = md.lines().filter(|l| l.starts_with("## ")).collect();
    assert_eq!(
        heads,
        vec![
            "## you",
            "## cline",
            "## tool",
            "## tool",
            "## cline",
            "## notice",
            "## notice",
            "## cline"
        ]
    );
}

#[test]
fn a_progress_feed_says_so_in_the_header() {
    let m = ChatModel::from_blocks(
        Source::ProgressFeed,
        "cline",
        vec![Block::message("a", Role::Engine, Origin::Engine, "tail")],
    );
    let md = to_markdown(&m, &meta());
    assert!(
        md.contains(&format!("- Source: {PROGRESS_FEED_LABEL}")),
        "{md}"
    );
    let full = to_markdown(&conversation(), &meta());
    assert!(!full.contains("- Source:"), "{full}");
}

#[test]
fn the_same_chat_exports_to_the_same_bytes() {
    assert_eq!(
        to_markdown(&conversation(), &meta()),
        to_markdown(&conversation(), &meta())
    );
}

#[test]
fn an_empty_chat_still_has_its_header() {
    let md = to_markdown(&model(vec![]), &meta());
    assert!(md.starts_with("# ham chat export\n"));
    assert!(!md.contains("\n## "));
}

// ---- reading an export back ---------------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Read {
    word: String,
    head: Option<String>,
    body: String,
}

/// Parses an export the way a Markdown reader sees its structure: top-level `## ` lines start a block, a `> ` line belongs to the quote,
/// a fence runs until a line of at least as many backticks. Nothing else is structure.
fn read_back(md: &str) -> Vec<Read> {
    let mut out: Vec<Read> = Vec::new();
    let mut fence: Option<usize> = None;
    let mut quote: Vec<String> = Vec::new();
    let mut fenced: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<Read>, quote: &mut Vec<String>| {
        if let Some(last) = out.last_mut()
            && !quote.is_empty()
        {
            last.body = quote.join("\n");
            quote.clear();
        }
    };
    for line in md.lines() {
        if let Some(n) = fence {
            let ticks = line.chars().take_while(|c| *c == '`').count();
            if ticks >= n && line.trim_start_matches('`').is_empty() {
                fence = None;
                if let Some(last) = out.last_mut() {
                    last.body = fenced.join("\n");
                }
                fenced.clear();
            } else {
                fenced.push(line.to_string());
            }
            continue;
        }
        if let Some(word) = line.strip_prefix("## ") {
            flush(&mut out, &mut quote);
            out.push(Read {
                word: word.to_string(),
                head: None,
                body: String::new(),
            });
        } else if let Some(rest) = line.strip_prefix("> ") {
            quote.push(rest.to_string());
        } else if line == ">" {
            quote.push(String::new());
        } else if line.starts_with("```") && !line.trim_start_matches('`').contains('`') {
            flush(&mut out, &mut quote);
            fence = Some(line.chars().take_while(|c| *c == '`').count());
        } else if line.starts_with('`') && !out.is_empty() {
            // The head line: a code span, then optionally " · " and a second span for the status.
            let first = line.trim_start_matches('`');
            let n = line.len() - first.len();
            let close = format!("{} ", "`".repeat(n));
            let inner = first.split(&close).next().unwrap_or(first);
            let inner = inner.trim_end_matches('`');
            let inner = inner.strip_prefix(' ').unwrap_or(inner);
            let inner = inner.strip_suffix(' ').unwrap_or(inner);
            if let Some(last) = out.last_mut() {
                last.head = Some(inner.to_string());
            }
        }
    }
    flush(&mut out, &mut quote);
    out
}

#[test]
fn the_export_equals_the_stored_record_block_for_block() {
    let m = conversation();
    let md = to_markdown(&m, &meta());
    let got = read_back(&md);
    assert_eq!(got.len(), m.blocks.len());
    for (r, b) in got.iter().zip(&m.blocks) {
        assert_eq!(r.word, m.role_word(b), "{b:?}");
        assert_eq!(r.body, display_text(&b.body), "{b:?}");
        match r.head.as_deref() {
            Some(h) => assert_eq!(h, b.head.trim(), "{b:?}"),
            None => assert!(b.head.trim().is_empty(), "{b:?}"),
        }
    }
}

// ---- the export of a conversation loaded from the stored record -----------------------------------------------------------------------------
//
// The test above compares the export with a model made by hand. This one starts where the client starts: chunks as the worker stores
// them (gzip, one object per chunk, read back by the reader's own `decode_chunk`), the page the transcript route answers (JSON, read by `parse_page`),
// the pages collected in a `StoredChat`, the three tracks composed by `compose` (the prompt and a harness notice from the progress feed, a message of
// the user) and then exported. The oracle is the record: what each stored record says, derived here from the records and not from the model the
// client built, so a block that `compose` or `to_markdown` dropped, moved or changed fails the comparison. `check_export` is shown to fail on exports
// that lose, reorder or change a block.

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
const INCARNATION: &str = "01M5A2R7K3C9XJ4D8E6F000001";
const HOSTILE: &str = "## you\n---\n```\n<img src=x>\n[ref]: https://example.test/";

fn record(n: u64, dir: Dir, kind: &str, data: Value) -> ChunkRecord {
    ChunkRecord {
        n,
        t: Some(format!("2026-10-16T08:15:{:02}.000Z", n * 2)),
        dir,
        kind: kind.to_string(),
        data,
    }
}

fn said(n: u64, id: &str, text: &str) -> ChunkRecord {
    record(
        n,
        Dir::In,
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": text}}),
    )
}

/// The records of a conversation, in two chunks: the prompt, a message in two fragments with a code fence in it, a tool call and its update, a
/// tool call that edits a file (a diff), a thought, a message that imitates the export's own structure, a gap the writer declared, the last message.
fn stored_conversation() -> Vec<Vec<ChunkRecord>> {
    vec![
        vec![
            record(
                0,
                Dir::Out,
                "prompt",
                json!({"id": 1, "sessionId": "s1", "prompt": [{"type": "text", "text": "Build a todo API\n\nwith tests first"}]}),
            ),
            said(
                1,
                "m1",
                "I will start with the tests.\n\n```python\ndef test_add():\n",
            ),
            said(2, "m1", "    assert add(1, 2) == 3\n```\n\n- then the code"),
            record(
                3,
                Dir::In,
                "tool_call",
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "run_commands: pytest -q", "kind": "execute", "status": "pending", "rawInput": {"command": "pytest -q"}}),
            ),
            record(
                4,
                Dir::In,
                "tool_call_update",
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "12 passed in 0.31s"}}]}),
            ),
        ],
        vec![
            record(
                5,
                Dir::In,
                "tool_call",
                json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "title": "editor: src/todo.py", "kind": "edit", "status": "completed",
                       "content": [{"type": "diff", "path": "src/todo.py", "oldText": "x = 1\n", "newText": "x = 2\n"}]}),
            ),
            record(
                6,
                Dir::In,
                "agent_thought_chunk",
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "maybe a dict is enough"}}),
            ),
            said(7, "m2", HOSTILE),
            record(8, Dir::In, "gap", json!({"kind": "truncated"})),
            said(9, "m3", "All green."),
        ],
    ]
}

/// The page the transcript route answers for these chunks: every chunk made as the writer stores it, decoded by the reader's `decode_chunk`, its
/// records as the route returns them, the page serialized to JSON and read back by `parse_page`.
fn route_page(chunks: Vec<Vec<ChunkRecord>>) -> TranscriptPage {
    let mut records: Vec<TranscriptRecord> = Vec::new();
    for (seq, recs) in (0u64..).zip(chunks) {
        let chunk = Chunk {
            header: ChunkHeader {
                v: CHUNK_VERSION,
                seq,
                incarnation: INCARNATION.into(),
                first_rec: recs.first().map_or(0, |r| r.n),
                last_rec: recs.last().map_or(0, |r| r.n),
                created_at: "2026-10-16T08:15:00.000Z".into(),
                engine: "cline".into(),
                protocol: "1.38.0".into(),
            },
            records: recs,
        };
        let stored = chunk_gzip_stored(&chunk);
        let read =
            decode_chunk(&stored, seq).expect("the reader decodes the chunk the writer stores");
        assert_eq!(read, chunk);
        for r in read.records {
            records.push(TranscriptRecord {
                seq,
                n: r.n,
                incarnation: read.header.incarnation.clone(),
                origin: RecordOrigin::Engine,
                t: r.t,
                dir: r.dir,
                kind: r.kind,
                data: r.data,
            });
        }
    }
    let page = TranscriptPage {
        run_id: RUN.into(),
        state: TranscriptState::Complete,
        records,
        gaps: vec![],
        completeness: vec![],
        published_through: Some(1),
        listed_through: Some(1),
        next: None,
        prev: None,
        more: false,
        skipped_keys: 0,
    };
    parse_page(&serde_json::to_vec(&page).unwrap()).expect("the route's body parses")
}

/// The conversation as the client composes it: the stored page, the run's prompt and one harness notice from the progress feed, and a message the
/// user sent while the run worked, placed by the time the server accepted it (between the stored records 3 and 4).
fn composed_from_the_store() -> (ChatModel, TranscriptPage) {
    let page = route_page(stored_conversation());
    let mut stored = StoredChat::new();
    stored.absorb(&page, StoredRead::First);
    assert!(uses_stored(&stored));
    let ulid = |secs: u64| {
        let base = harness_protocol::common::parse_utc_unix("2026-10-16T08:15:00Z").unwrap() as u64;
        ham_core::messages::ulid_from((base + secs) * 1000, [0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
    };
    let progress = ChatModel::from_blocks(
        Source::ProgressFeed,
        "cline",
        vec![
            Block::message(
                "prompt",
                Role::You,
                Origin::User,
                "Build a todo API\n\nwith tests first",
            ),
            Block::notice(
                &format!("n:{}", ulid(19)),
                NoticeKind::RunStatus,
                "Run complete",
            ),
        ],
    );
    let user = [UserItem {
        at: "2026-10-16T08:15:07Z".into(),
        block: Block::message("u:1", Role::You, Origin::User, "thanks, keep it small"),
    }];
    let cx = Context {
        progress: &progress,
        user: &user,
        live: false,
    };
    (compose(&stored, &cx), page)
}

/// What one stored record (or the records that make one block) must come to in the export.
#[derive(Debug)]
struct Want {
    /// The role word of the block; `None` when the record does not fix it.
    word: Option<&'static str>,
    /// The head line the block has, when the record fixes it.
    head: Option<String>,
    /// The body the block has in full, after the safe-text and quoting rules, or pieces its body must hold.
    body: Body,
}

#[derive(Debug)]
enum Body {
    /// A message: the text of its fragments joined, made safe and made literal as the export does.
    Quoted(String),
    /// Output or a diff in a fence: these lines are in the body.
    Holds(Vec<&'static str>),
}

/// The blocks the export must hold for the stored records, derived from the records alone: fragments of one `messageId` join, a tool call and its
/// update are one block, the diff of a tool call follows it, a thought, a gap and the prompt each make one.
fn wants_of(records: &[TranscriptRecord]) -> Vec<Want> {
    let text_of = |d: &Value| -> String {
        d["content"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let mut out: Vec<Want> = Vec::new();
    let mut last_message: Option<String> = None;
    for r in records {
        let d = &r.data;
        match r.kind.as_str() {
            "prompt" => {
                last_message = None;
                out.push(Want {
                    word: Some("cline"),
                    head: None,
                    body: Body::Holds(vec!["Build a todo API", "with tests first"]),
                });
            }
            "agent_message_chunk" => {
                let id = d["messageId"].as_str().unwrap_or_default().to_string();
                match (&last_message, out.last_mut()) {
                    (
                        Some(prev),
                        Some(Want {
                            body: Body::Quoted(t),
                            ..
                        }),
                    ) if *prev == id => {
                        t.push_str(&text_of(d));
                    }
                    _ => {
                        out.push(Want {
                            word: Some("cline"),
                            head: None,
                            body: Body::Quoted(text_of(d)),
                        });
                    }
                }
                last_message = Some(id);
            }
            "tool_call" => {
                last_message = None;
                out.push(Want {
                    word: Some("tool"),
                    head: d["title"].as_str().map(str::to_string),
                    body: Body::Holds(vec![]),
                });
                if let Some(diff) = d["content"]
                    .as_array()
                    .and_then(|a| a.iter().find(|c| c["type"] == "diff"))
                {
                    out.push(Want {
                        word: Some("tool"),
                        head: diff["path"].as_str().map(str::to_string),
                        body: Body::Holds(vec!["-x = 1", "+x = 2"]),
                    });
                }
            }
            "tool_call_update" => {
                if let Some(Want {
                    body: Body::Holds(lines),
                    ..
                }) = out.iter_mut().rev().find(|w| {
                    w.word == Some("tool") && matches!(&w.body, Body::Holds(l) if l.is_empty())
                }) && d["content"][0]["content"]["text"] == "12 passed in 0.31s"
                {
                    lines.push("12 passed in 0.31s");
                }
            }
            "agent_thought_chunk" => {
                last_message = None;
                out.push(Want {
                    word: None,
                    head: None,
                    body: Body::Holds(vec!["maybe a dict is enough"]),
                });
            }
            _ => {
                // The gap the writer declared: a block whose words are the client's, not the record's.
                last_message = None;
                out.push(Want {
                    word: Some("notice"),
                    head: None,
                    body: Body::Holds(vec![]),
                });
            }
        }
    }
    out
}

/// Whether the export `md` holds, in order, the blocks the stored `records` come to, and the blocks of the other tracks around them.
///
/// 1. The export has as many blocks as the composed model (nothing dropped or added).
/// 2. Each block of the export equals the model's block (the role word, the head, the body after the export's own rules).
/// 3. The wants of the records are found in order, each in a block with its role word, head and text.
fn check_export(md: &str, model: &ChatModel, records: &[TranscriptRecord]) -> Result<(), String> {
    let got = read_back(md);
    if got.len() != model.blocks.len() {
        return Err(format!(
            "the export has {} blocks, the composed conversation {}",
            got.len(),
            model.blocks.len()
        ));
    }
    for (i, (r, b)) in got.iter().zip(&model.blocks).enumerate() {
        if r.word != model.role_word(b) {
            return Err(format!(
                "block {i}: role {:?}, the model's {:?}",
                r.word,
                model.role_word(b)
            ));
        }
        let want = display_text(&b.body);
        let want = if b.head.is_empty() {
            neutralize(&want)
        } else {
            want
        };
        if r.body != want {
            return Err(format!(
                "block {i}: the body differs from the model's: {:?} against {:?}",
                r.body, want
            ));
        }
    }
    let mut from = 0;
    for w in wants_of(records) {
        let found = got.iter().enumerate().skip(from).find(|(_, r)| {
            w.word.is_none_or(|x| r.word == x)
                && w.head
                    .as_deref()
                    .is_none_or(|h| r.head.as_deref() == Some(h))
                && match &w.body {
                    Body::Quoted(t) => r.body == neutralize(&display_text(t)),
                    Body::Holds(lines) => lines.iter().all(|l| r.body.contains(l)),
                }
        });
        match found {
            Some((i, _)) => from = i + 1,
            None => {
                return Err(format!(
                    "no block from position {from} on holds what the record says: {w:?}"
                ));
            }
        }
    }
    Ok(())
}

fn sections(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in md.lines() {
        if l.starts_with("## ") || out.is_empty() {
            out.push(String::new());
        }
        let last = out.len() - 1;
        out[last].push_str(l);
        out[last].push('\n');
    }
    out
}

#[test]
fn an_export_of_the_conversation_loaded_from_the_stored_record_equals_the_record_block_for_block() {
    let (model, page) = composed_from_the_store();
    // The tracks are all there: the prompt first, the harness's notice and the user's message among the engine's blocks.
    assert_eq!(model.blocks.len(), 12, "{:#?}", model.blocks);
    let md = to_markdown(&model, &meta());
    insta::assert_snapshot!("export_of_the_stored_conversation", md);
    check_export(&md, &model, &page.records).unwrap_or_else(|e| panic!("{e}\n{md}"));
    let words: Vec<String> = read_back(&md).into_iter().map(|r| r.word).collect();
    assert_eq!(words.first().map(String::as_str), Some("you"), "{words:?}");
    assert_eq!(words.iter().filter(|w| *w == "you").count(), 2, "{words:?}");
    assert!(
        md.contains("12 passed in 0.31s") && md.contains("+x = 2"),
        "{md}"
    );
    // The message that imitates structure stayed inside its quote: the export has no heading it did not write.
    let headings = md.lines().filter(|l| l.starts_with("## ")).count();
    assert_eq!(headings, model.blocks.len(), "{md}");
    // Exporting twice gives the same bytes.
    assert_eq!(md, to_markdown(&model, &meta()));
}

#[test]
fn the_comparison_with_the_record_fails_on_an_export_that_lost_reordered_or_changed_a_block() {
    let (model, page) = composed_from_the_store();
    let md = to_markdown(&model, &meta());
    assert!(check_export(&md, &model, &page.records).is_ok());
    let secs = sections(&md);
    assert_eq!(
        secs.len(),
        1 + model.blocks.len(),
        "the header and one section a block"
    );
    // The last block is lost.
    let lost = secs[..secs.len() - 1].concat();
    let e = check_export(&lost, &model, &page.records).unwrap_err();
    assert!(e.contains("11 blocks") && e.contains("12"), "{e}");
    // A block of the engine is lost and another of the same kind stands in its place: the count holds, the record does not.
    let tool = secs.iter().position(|s| s.contains("12 passed")).unwrap();
    let mut swapped = secs.clone();
    swapped.swap(tool, tool + 1);
    let e = check_export(&swapped.concat(), &model, &page.records).unwrap_err();
    assert!(e.contains("block"), "{e}");
    // Two blocks of the engine's record are exchanged in the model's own order too: the model agrees with the export, the record does not.
    let mut reordered = model.clone();
    let first = reordered
        .blocks
        .iter()
        .position(|b| b.body.contains("I will start"))
        .unwrap();
    let second = reordered
        .blocks
        .iter()
        .position(|b| b.body.contains("12 passed"))
        .unwrap();
    reordered.blocks.swap(first, second);
    let md2 = to_markdown(&reordered, &meta());
    let e = check_export(&md2, &reordered, &page.records).unwrap_err();
    assert!(e.contains("no block from position"), "{e}");
    // One word of the tool's output is changed.
    let changed = md.replace("12 passed in 0.31s", "13 passed in 0.31s");
    let e = check_export(&changed, &model, &page.records).unwrap_err();
    assert!(e.contains("differs") || e.contains("no block"), "{e}");
    // A message of Cline's turned into the user's: its role word changes.
    let user_said = md.replacen("## cline\n", "## you\n", 1);
    let e = check_export(&user_said, &model, &page.records).unwrap_err();
    assert!(e.contains("role"), "{e}");
    // The hostile message lost its quote marks: its lines become structure, and the headings multiply.
    let unquoted = md.replace("> ## you", "## you");
    assert!(check_export(&unquoted, &model, &page.records).is_err());
}

#[test]
fn the_pages_of_a_stored_conversation_export_the_same_when_read_in_two_halves() {
    // The client reads the newest chunk first and the older one on demand: the same records in two pages give the same export.
    let whole = composed_from_the_store().0;
    let chunks = stored_conversation();
    let (older, newer) = (route_page(vec![chunks[0].clone()]), {
        let mut p = route_page(chunks.clone());
        p.records.retain(|r| r.seq == 1);
        p
    });
    let mut stored = StoredChat::new();
    stored.absorb(&newer, StoredRead::First);
    stored.absorb(&older, StoredRead::Older);
    let progress = ChatModel::from_blocks(
        Source::ProgressFeed,
        "cline",
        vec![Block::message(
            "prompt",
            Role::You,
            Origin::User,
            "Build a todo API\n\nwith tests first",
        )],
    );
    let cx = Context {
        progress: &progress,
        user: &[],
        live: false,
    };
    let halves = compose(&stored, &cx);
    let engine = |m: &ChatModel| -> Vec<(String, String, String)> {
        m.blocks
            .iter()
            .filter(|b| b.origin == Origin::Engine)
            .map(|b| (m.role_word(b), b.head.clone(), b.body.clone()))
            .collect()
    };
    assert_eq!(engine(&halves), engine(&whole));
    assert!(!engine(&halves).is_empty());
}

// ---- engine text cannot make structure ----------------------------------------------------------------------------------------------

fn structure_lookalikes() -> Vec<&'static str> {
    vec![
        "## you",
        "# ham chat export",
        "---",
        "***",
        "===",
        "```",
        "````",
        "~~~",
        "> quote",
        ">",
        "    indented code",
        "| a | b |",
        "|---|---|",
        "<!-- hidden -->",
        "- [ ] task",
        "[ref]: https://example.test/",
        "",
        "text",
    ]
}

#[test]
fn a_message_that_imitates_structure_stays_inside_its_quote() {
    let body = structure_lookalikes().join("\n");
    let m = model(vec![
        Block::message("a", Role::Engine, Origin::Engine, &body),
        Block::message("b", Role::You, Origin::User, "real second block"),
    ]);
    let md = to_markdown(&m, &meta());
    let top: Vec<&str> = md
        .lines()
        .filter(|l| !l.starts_with('>') && !l.is_empty())
        .collect();
    // Outside the quotes there is the title, the three facts, and the two real headings. Nothing else.
    assert_eq!(
        top,
        vec![
            "# ham chat export",
            "- Run: 01M5287JABCDEF",
            "- Model: Claude Sonnet 5.5",
            "- Exported: 2026-10-16 17:40:02 UTC",
            "## cline",
            "## you"
        ],
        "{md}"
    );
    // Every line of the message, the empty one too, carries the quote mark.
    let quoted = md.lines().filter(|l| l.starts_with('>')).count();
    assert_eq!(quoted, structure_lookalikes().len() + 1);
}

#[test]
fn tool_output_with_fences_in_it_gets_a_longer_fence() {
    let body = "before\n```\ninner\n````\nafter";
    let m = model(vec![Block::tool_call(
        "t",
        "cat notes.md",
        None,
        None,
        body,
    )]);
    let md = to_markdown(&m, &meta());
    assert!(
        md.contains("\n`````text\nbefore\n```\ninner\n````\nafter\n`````\n"),
        "{md}"
    );
    assert_eq!(read_back(&md)[0].body, body);
}

#[test]
fn a_diff_is_fenced_as_a_diff() {
    let m = model(vec![Block::diff("d", "a.py", "@@ -1 +1 @@\n-a\n+b")]);
    let md = to_markdown(&m, &meta());
    assert!(md.contains("\n```diff\n@@ -1 +1 @@\n-a\n+b\n```\n"), "{md}");
}

#[test]
fn a_head_with_backticks_gets_a_longer_code_span() {
    let m = model(vec![Block::tool_call(
        "t",
        "run_commands: echo `date` ``x``",
        None,
        Some("completed"),
        "out",
    )]);
    let md = to_markdown(&m, &meta());
    assert!(
        md.contains("\n``` run_commands: echo `date` ``x`` ``` · `completed`\n"),
        "{md}"
    );
    assert_eq!(
        read_back(&md)[0].head.as_deref(),
        Some("run_commands: echo `date` ``x``")
    );
    let edge = model(vec![Block::tool_call("t", "`lead", None, None, "o")]);
    let md = to_markdown(&edge, &meta());
    assert!(md.contains("\n`` `lead ``\n"), "{md}");
}

#[test]
fn neutralize_makes_html_images_and_link_definitions_literal_outside_code() {
    let cases = [
        ("plain text", "plain text"),
        ("a < b", "a \\< b"),
        ("<img src=x>", "\\<img src=x>"),
        ("<!-- hidden -->", "\\<!-- hidden -->"),
        ("<https://example.com>", "\\<https://example.com>"),
        (
            "![](https://example.test/?q=1)",
            "\\![](https://example.test/?q=1)",
        ),
        ("see ![a][ref] now", "see \\![a][ref] now"),
        (
            "[docs]: https://example.test",
            "\\[docs]: https://example.test",
        ),
        ("   [x]: y", "   \\[x]: y"),
        ("[^1]: a note", "\\[^1]: a note"),
        (
            "[a link](https://ok.example) stays",
            "[a link](https://ok.example) stays",
        ),
        ("a <b> and `<i>` and <u>", "a \\<b> and `<i>` and \\<u>"),
        ("use ``a<b`` here", "use ``a<b`` here"),
        ("\\<b> is escaped already", "\\<b> is escaped already"),
        ("\\\\<b> is not", "\\\\\\<b> is not"),
        (
            "```html\n<div>\n[x]: y\n![i](u)\n```",
            "```html\n<div>\n[x]: y\n![i](u)\n```",
        ),
        ("~~~\n<p>\n~~~\n<p>", "~~~\n<p>\n~~~\n\\<p>"),
        (
            "````\n```\n<p>\n```\n````\n<p>",
            "````\n```\n<p>\n```\n````\n\\<p>",
        ),
        ("```\n<p>", "```\n<p>"),
        ("a `unclosed <b>", "a `unclosed \\<b>"),
    ];
    for (input, want) in cases {
        assert_eq!(neutralize(input), want, "{input:?}");
    }
}

#[test]
fn engine_text_with_html_and_images_is_literal_in_the_export() {
    let m = model(vec![Block::message(
        "a",
        Role::Engine,
        Origin::Engine,
        "<img src=\"https://example.test/p?d=x\">\n![t](https://example.test/t.png)\n[k]: https://example.test\n\n```\n<keep>\n```",
    )]);
    let md = to_markdown(&m, &meta());
    for line in md.lines().filter(|l| l.starts_with("> ")) {
        let rest = &line[2..];
        assert!(!rest.starts_with("<img"), "{line}");
        assert!(!rest.starts_with("!["), "{line}");
        assert!(!rest.starts_with("[k]:"), "{line}");
    }
    assert!(md.contains("> \\<img src="), "{md}");
    assert!(md.contains("> \\![t](https://example.test/t.png)"), "{md}");
    assert!(md.contains("> \\[k]: https://example.test"), "{md}");
    assert!(md.contains("> <keep>"), "code stays as it was: {md}");
}

// ---- masking and cleaning -----------------------------------------------------------------------------------------------------------

fn dirty_block(text: &str) -> Block {
    let mut b = Block::tool_call("t", "head", None, None, "body");
    b.body = text.to_string();
    b
}

#[test]
fn presigned_links_and_bearer_material_never_reach_the_file() {
    let sig = "0123456789abcdef".repeat(4);
    let url = format!(
        "https://b.s3.us-east-1.amazonaws.com/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature={sig}"
    );
    let token = format!("ghp_{}", "Z9y8".repeat(9));
    let bearer = format!("Authorization: Bearer {}", "abcDEF0123456789".repeat(3));
    let mut head = Block::tool_call("t2", "curl", None, None, "");
    head.head = format!("curl {url}");
    let m = model(vec![
        dirty_block(&format!("get {url}\n{token}\n{bearer}")),
        head,
        {
            let mut b = Block::message("m", Role::Engine, Origin::Engine, "x");
            b.body = format!("my token is {token} ok");
            b
        },
    ]);
    let md = to_markdown(&m, &meta());
    for secret in [&sig, &token, &"abcDEF0123456789".repeat(3)] {
        assert!(!md.contains(secret.as_str()), "{secret} in {md}");
    }
    assert!(md.contains("[redacted]"));
}

#[test]
fn escape_sequences_and_controls_never_reach_the_file() {
    let mut m = model(vec![
        dirty_block("a\u{1b}]0;title\u{7}b\rc\u{8}d\u{1b}[31mred"),
        Block::message("m", Role::Engine, Origin::Engine, "x"),
    ]);
    m.blocks[1].body = "hello\u{1b}]8;;http://example.test/\u{7}link\u{202e}rtl".to_string();
    m.blocks[0].head = "head\u{1b}[2Jgone".to_string();
    let md = to_markdown(&m, &meta());
    for c in md.chars() {
        assert!(c == '\n' || c == '\t' || !c.is_control(), "{:?}", c);
    }
    assert!(md.contains("<U+202E>"));
    assert!(!md.contains("example.test/"));
}

// ---- file names and destinations ------------------------------------------------------------------------------------------------------

#[test]
fn file_names_keep_only_safe_characters() {
    assert_eq!(
        file_name("01M5287J", "20261016-174002"),
        "ham-chat-01M5287J-20261016-174002.md"
    );
    assert_eq!(
        file_name("dir/sub/name", "2026 10 16"),
        "ham-chat-dirsubname-20261016.md"
    );
    assert_eq!(file_name("", ""), "ham-chat-run.md");
    assert_eq!(
        file_name(&"a".repeat(200), "s").len(),
        "ham-chat-".len() + 40 + "-s.md".len()
    );
    assert_eq!(file_name("名前", "x"), "ham-chat-run-x.md");
}

#[test]
fn the_destination_is_read_from_what_the_user_typed() {
    let d = Path::new("/home/u/Downloads");
    let home = Path::new("/home/u");
    let cases: [(Option<&str>, Destination); 8] = [
        (None, Destination::Dir(d.into())),
        (Some(""), Destination::Dir(d.into())),
        (Some("  "), Destination::Dir(d.into())),
        (Some("/tmp/notes"), Destination::Dir("/tmp/notes".into())),
        (Some("~/notes"), Destination::Dir("/home/u/notes".into())),
        (Some("~"), Destination::Dir("/home/u".into())),
        (
            Some("/tmp/chat.md"),
            Destination::File("/tmp/chat.md".into()),
        ),
        (
            Some("~/x/Chat.MARKDOWN"),
            Destination::File("/home/u/x/Chat.MARKDOWN".into()),
        ),
    ];
    for (arg, want) in cases {
        assert_eq!(resolve(arg, d, Some(home)), want, "{arg:?}");
    }
    assert_eq!(
        resolve(Some("~/x"), d, None),
        Destination::Dir("~/x".into()),
        "no home: the text is kept"
    );
    assert_eq!(
        resolve(Some("rel/dir"), d, Some(home)),
        Destination::Dir("rel/dir".into())
    );
}

// ---- the write ----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_new_file_holds_the_text_is_private_and_leaves_no_temp_file() {
    let dir = scratch("new");
    let path = write(
        &Destination::Dir(dir.clone()),
        "01M5",
        "20261016-174002",
        "hello\n",
    )
    .unwrap();
    assert_eq!(path, dir.join("ham-chat-01M5-20261016-174002.md"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
    assert_eq!(names(&dir), vec!["ham-chat-01M5-20261016-174002.md"]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn a_taken_name_gets_a_number_and_the_old_file_is_never_touched() {
    let dir = scratch("taken");
    let dest = Destination::Dir(dir.clone());
    let first = write(&dest, "r", "s", "one").unwrap();
    let second = write(&dest, "r", "s", "two").unwrap();
    let third = write(&dest, "r", "s", "three").unwrap();
    assert_eq!(first.file_name().unwrap(), "ham-chat-r-s.md");
    assert_eq!(second.file_name().unwrap(), "ham-chat-r-s-2.md");
    assert_eq!(third.file_name().unwrap(), "ham-chat-r-s-3.md");
    assert_eq!(fs::read_to_string(first).unwrap(), "one");
    assert_eq!(fs::read_to_string(second).unwrap(), "two");
    assert_eq!(fs::read_to_string(third).unwrap(), "three");
    assert_eq!(names(&dir).len(), 3, "no temp file is left");
}

#[test]
fn a_file_name_the_user_chose_is_never_replaced() {
    let dir = scratch("explicit");
    let target = dir.join("mine.md");
    fs::write(&target, "precious").unwrap();
    let e = write(&Destination::File(target.clone()), "r", "s", "new").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert!(e.to_string().contains("mine.md"), "{e}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "precious");
    assert_eq!(names(&dir), vec!["mine.md"]);
    // A name that is free is written.
    let other = dir.join("other.md");
    let p = write(&Destination::File(other.clone()), "r", "s", "fresh").unwrap();
    assert_eq!(p, other);
    assert_eq!(fs::read_to_string(&other).unwrap(), "fresh");
}

#[test]
fn a_symbolic_link_at_the_name_is_not_followed_or_replaced() {
    let dir = scratch("symlink");
    let victim = dir.join("victim.txt");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("ham-chat-r-s.md")).unwrap();
    let p = write(&Destination::Dir(dir.clone()), "r", "s", "new").unwrap();
    assert_eq!(p.file_name().unwrap(), "ham-chat-r-s-2.md");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
}

#[test]
fn a_directory_that_is_not_there_is_an_error_and_nothing_is_created() {
    let dir = scratch("missing");
    let gone = dir.join("nope");
    let e = write(&Destination::Dir(gone.clone()), "r", "s", "x").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    assert!(e.to_string().contains("nope"), "{e}");
    assert!(!gone.exists());
    let file = dir.join("afile");
    fs::write(&file, "x").unwrap();
    let e2 = write(&Destination::Dir(file), "r", "s", "x").unwrap_err();
    assert!(
        matches!(
            e2.kind(),
            io::ErrorKind::NotADirectory | io::ErrorKind::NotFound | io::ErrorKind::Other
        ),
        "{e2}"
    );
    assert_eq!(names(&dir), vec!["afile"]);
    let e3 = write(&Destination::File(dir.join("sub/x.md")), "r", "s", "x").unwrap_err();
    assert_eq!(e3.kind(), io::ErrorKind::NotFound);
}

#[test]
fn eight_writers_at_once_make_eight_complete_files() {
    let dir = scratch("race");
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let d = dir.clone();
            std::thread::spawn(move || {
                let body = format!("writer {i}\n").repeat(2000);
                let p = write(&Destination::Dir(d), "r", "s", &body).unwrap();
                (p, body)
            })
        })
        .collect();
    let mut paths = Vec::new();
    for h in handles {
        let (p, body) = h.join().unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), body);
        paths.push(p);
    }
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 8);
    assert_eq!(
        names(&dir).len(),
        8,
        "no temp file is left: {:?}",
        names(&dir)
    );
}

#[test]
fn a_filesystem_without_hard_links_falls_back_to_renaming_the_complete_temp_file() {
    let dir = scratch("nolink");
    let no_links = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied));
    let dest = Destination::Dir(dir.clone());
    let a = write_with(&dest, "r", "s", "one", &no_links).unwrap();
    let b = write_with(&dest, "r", "s", "two", &no_links).unwrap();
    assert_eq!(a.file_name().unwrap(), "ham-chat-r-s.md");
    assert_eq!(b.file_name().unwrap(), "ham-chat-r-s-2.md");
    assert_eq!(fs::read_to_string(a).unwrap(), "one");
    assert_eq!(fs::read_to_string(b).unwrap(), "two");
    assert_eq!(names(&dir).len(), 2);
    // An explicit name that exists is still refused, and the temp file is cleaned up.
    let mine = dir.join("mine.md");
    fs::write(&mine, "precious").unwrap();
    let e = write_with(&Destination::File(mine.clone()), "r", "s", "x", &no_links).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&mine).unwrap(), "precious");
    assert_eq!(names(&dir).len(), 3);
}

#[test]
fn a_link_that_succeeded_but_was_reported_as_existing_counts_as_done() {
    // On a network filesystem a retransmitted link request can answer "exists" after it worked.
    let dir = scratch("retransmit");
    let real_then_exists = |from: &Path, to: &Path| {
        fs::hard_link(from, to)?;
        Err(io::Error::from(io::ErrorKind::AlreadyExists))
    };
    let p = write_with(
        &Destination::Dir(dir.clone()),
        "r",
        "s",
        "body",
        &real_then_exists,
    )
    .unwrap();
    assert_eq!(p.file_name().unwrap(), "ham-chat-r-s.md");
    assert_eq!(fs::read_to_string(&p).unwrap(), "body");
    assert_eq!(names(&dir), vec!["ham-chat-r-s.md"]);
}

#[test]
fn when_every_numbered_name_is_taken_nothing_is_written_and_no_temp_file_stays() {
    let dir = scratch("alltaken");
    let taken = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::AlreadyExists));
    let e = write_with(&Destination::Dir(dir.clone()), "r", "s", "x", &taken).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert!(e.to_string().contains("nothing was written"), "{e}");
    assert!(names(&dir).is_empty(), "{:?}", names(&dir));
}

#[test]
fn the_notice_names_the_file_the_blocks_and_the_size() {
    let p = Path::new("/home/u/Downloads/ham-chat-r-s.md");
    assert_eq!(
        notice(p, 14, 3277),
        "Exported 14 blocks (3.2 KiB) to /home/u/Downloads/ham-chat-r-s.md."
    );
    assert_eq!(
        notice(p, 1, 12),
        "Exported 1 block (12 bytes) to /home/u/Downloads/ham-chat-r-s.md."
    );
}

// ---- properties -------------------------------------------------------------------------------------------------------------------

fn arb_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            3 => prop::sample::select(structure_lookalikes()).prop_map(String::from),
            2 => "[a-z ]{0,12}".prop_map(String::from),
            1 => prop::sample::select(vec![
                "```", "`x`", "<b>", "![i](u)", "\u{1b}[31m", "\u{1b}]0;t\u{7}", "\\", "\t",
                "``", "~~~ rust", "[a]: b", "<!--", "\u{202e}",
            ])
            .prop_map(String::from),
        ],
        0..14,
    )
    .prop_map(|lines| lines.join("\n"))
}

proptest! {
    #[test]
    fn whatever_the_blocks_say_the_structure_is_ours(
        bodies in prop::collection::vec((arb_text(), any::<bool>(), 0u8..4), 1..6),
    ) {
        let blocks: Vec<Block> = bodies
            .iter()
            .enumerate()
            .map(|(i, (t, has_head, kind))| {
                let id = format!("b{i}");
                match (kind, has_head) {
                    (0, _) => Block::message(&id, Role::Engine, Origin::Engine, t),
                    (1, _) => Block::message(&id, Role::You, Origin::User, t),
                    (2, true) => Block::tool_call(&id, "run_commands: x", None, None, t),
                    _ => Block::diff(&id, "f.py", t),
                }
            })
            .collect();
        let m = model(blocks);
        let md = to_markdown(&m, &meta());
        for c in md.chars() {
            prop_assert!(c == '\n' || c == '\t' || !c.is_control(), "{c:?}");
        }
        let got = read_back(&md);
        prop_assert_eq!(got.len(), m.blocks.len());
        for (r, b) in got.iter().zip(&m.blocks) {
            prop_assert_eq!(&r.word, &m.role_word(b));
            // Quoted blocks differ from the record only by the escapes of `neutralize`.
            let want = display_text(&b.body);
            if b.head.is_empty() {
                prop_assert_eq!(&r.body, &neutralize(&want));
            } else {
                prop_assert_eq!(&r.body, &want);
            }
        }
    }

    #[test]
    fn neutralize_leaves_no_raw_angle_bracket_or_image_or_definition_outside_fences(
        lines in prop::collection::vec(
            prop_oneof![
                3 => prop::sample::select(vec![
                    "<b>", "a < b", "\\<b>", "\\\\<b>", "![i](u)", "x ![i][r]", "[a]: b", "  [a]: b", "[^1]: n",
                    "```", "````", "```html", "~~~", "~~~ rust", "text", "", "> q", "- <li>",
                ]).prop_map(String::from),
                1 => "[a-z <>!\\[\\]:]{0,16}".prop_map(String::from),
            ],
            0..16,
        ),
    ) {
        let out = neutralize(&lines.join("\n"));
        prop_assert_eq!(out.split('\n').count(), lines.len().max(1));
        let mut in_fence: Option<(char, usize)> = None;
        for line in out.split('\n') {
            let t = line.trim_start();
            let indent = line.len() - t.len();
            if let Some((c, n)) = in_fence {
                let run = t.chars().take_while(|x| *x == c).count();
                if run >= n && t[run..].trim().is_empty() && indent < 4 {
                    in_fence = None;
                }
                continue;
            }
            if let Some(c) = t.chars().next().filter(|c| *c == '`' || *c == '~') {
                let run = t.chars().take_while(|x| *x == c).count();
                if run >= 3 && indent < 4 && !(c == '`' && t[run..].contains('`')) {
                    in_fence = Some((c, run));
                    continue;
                }
            }
            let chars: Vec<char> = line.chars().collect();
            let escaped = |i: usize| chars[..i].iter().rev().take_while(|c| **c == '\\').count() % 2 == 1;
            for (i, ch) in chars.iter().enumerate() {
                if *ch == '<' {
                    prop_assert!(escaped(i), "raw `<` in {line:?}");
                }
                if *ch == '!' && chars.get(i + 1) == Some(&'[') {
                    prop_assert!(escaped(i), "raw image start in {line:?}");
                }
            }
            let lead: String = line.chars().skip_while(|c| *c == ' ').collect();
            if lead.starts_with('[') {
                prop_assert!(!lead.contains("]:"), "raw definition in {line:?}");
            }
        }
    }
}
