//! Chat-content search: the keys, the bounded body built from a run's transcript, the matching and the
//! wire types. The transcript pieces come from the Python-made chunk files of `tests/fixtures/transcript/good`.
#![allow(clippy::unwrap_used)]

use ham_core::redact::redact_tokens;
use ham_core::runsearch::{
    ANSWER_MAX_BYTES, ANSWERS_MAX_BYTES, BODY_MAX_BYTES, BodyBuilder, Coverage, GAP_MARKER,
    HEAD_BYTES, HitField, OBJECTIVE_MAX_BYTES, PIECE_MAX_BYTES, Piece, QueryError, SEGS_MAX,
    SearchHit, SearchItem, SearchQuery, SearchResponse, Seg, Source, TAIL_BYTES, chat_ceiling,
    chat_floor, chat_sk, find_match, find_match_redacted, owner_pk, parse_query, parse_response,
    pieces_of_chunk, search_sk, segs_from_str, segs_to_string, sk_floor,
};
use ham_core::transcript::{Chunk, ChunkHeader, ChunkRecord, Dir, decode_chunk};
use proptest::prelude::*;
use serde_json::{Value, json};
use std::path::PathBuf;

const INC: &str = "01K83DA0A0A0A0A0A0A0A0A0A0";

fn good(seq: u64) -> Chunk {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/fixtures/transcript/good/{seq:012}.ndjson.gz.hex"
    ));
    let bytes = ham_core::gzip::unhex(&std::fs::read_to_string(p).unwrap()).unwrap();
    decode_chunk(&bytes, seq).unwrap()
}

fn chunk_of(seq: u64, records: Vec<(&str, Value)>) -> Chunk {
    let recs: Vec<ChunkRecord> = records
        .into_iter()
        .enumerate()
        .map(|(i, (kind, data))| ChunkRecord {
            n: i as u64,
            t: None,
            dir: Dir::In,
            kind: kind.into(),
            data,
        })
        .collect();
    Chunk {
        header: ChunkHeader {
            v: 1,
            seq,
            incarnation: INC.into(),
            first_rec: 0,
            last_rec: recs.len().saturating_sub(1) as u64,
            created_at: "2026-10-16T10:00:00.000Z".into(),
            engine: "cline".into(),
            protocol: "1.38.0".into(),
        },
        records: recs,
    }
}

fn engine(seq: u64, rec: u32, text: &str, message: Option<&str>) -> Piece {
    Piece {
        source: Source::Engine { seq, rec },
        text: text.into(),
        message: message.map(str::to_string),
    }
}

// ---- keys ------------------------------------------------------------------------------------------------------------

#[test]
fn the_owner_partition_is_a_hash_of_the_principal_and_never_the_principal() {
    let a = owner_pk("arn:aws:sts::111122223333:assumed-role/dev/alice");
    let b = owner_pk("arn:aws:sts::111122223333:assumed-role/dev/bob");
    assert_ne!(a, b);
    assert_eq!(
        a,
        owner_pk("arn:aws:sts::111122223333:assumed-role/dev/alice")
    );
    assert!(
        a.starts_with("OWNER#") && a.len() == "OWNER#".len() + 64,
        "{a}"
    );
    assert!(
        a["OWNER#".len()..]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{a}"
    );
    assert!(!a.contains("alice") && !a.contains("111122223333"));
    // SHA-256 of the empty string, so the encoding is the standard one.
    assert_eq!(
        owner_pk(""),
        "OWNER#e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn the_sort_key_orders_runs_by_creation_time() {
    let early = search_sk("2026-10-15T08:00:00.000Z", "01K83D0000000000000000000A");
    let late = search_sk("2026-10-16T08:00:00.000Z", "01K83D0000000000000000000B");
    assert!(early < late);
    assert_eq!(
        early,
        "SRCH#2026-10-15T08:00:00.000Z#01K83D0000000000000000000A"
    );
    assert!(sk_floor("2026-10-16") <= late && sk_floor("2026-10-16") > early);
    assert!(late.as_str() < ham_core::runsearch::SK_CEILING);
}

#[test]
fn the_chat_body_lives_in_its_own_item_that_the_run_index_never_reads() {
    // The run index lists a page of runs by one Query of `SRCH#<oldest>` to `SRCH#<newest>#~` and reads whole items. A 32 KiB body in
    // those items would make every run list read up to 3.2 MB, so the body has a key family of its own, below the run index's.
    let created = "2026-10-15T08:00:00.000Z";
    let run = "01K83D0000000000000000000A";
    let chat = chat_sk(created, run);
    assert_eq!(chat, format!("CHAT#{created}#{run}"));
    let (lo, hi) = (sk_floor("2000-01-01"), ham_core::runsearch::SK_CEILING);
    assert!(
        !(lo.as_str() <= chat.as_str() && chat.as_str() <= hi),
        "{chat} is inside the run index's range {lo} to {hi}"
    );
    for at in [
        "2026-10-15T08:00:00.000Z",
        "2026-12-31T23:59:59.999Z",
        "9999-12-31T23:59:59.999Z",
        "0000-01-01T00:00:00.000Z",
    ] {
        let item = chat_sk(at, run);
        assert!(item.starts_with("CHAT#") && !item.starts_with("SRCH#"));
        assert!(!(format!("SRCH#{at}") <= item && item <= format!("SRCH#{at}#~")));
    }
    // The chat items of a window of runs are one contiguous range.
    let (oldest, newest) = ("2026-10-14T00:00:00.000Z", "2026-10-16T00:00:00.000Z");
    let inside = chat_sk("2026-10-15T08:00:00.000Z", run);
    assert!(chat_floor(oldest) <= inside && inside <= chat_ceiling(newest));
    assert!(chat_sk("2026-10-13T08:00:00.000Z", run) < chat_floor(oldest));
    assert!(
        chat_sk("2026-10-16T00:00:00.000Z", "01K83D0000000000000000000Z") <= chat_ceiling(newest)
    );
    assert!(chat_sk("2026-10-16T00:00:00.001Z", run) > chat_ceiling(newest));
}

#[test]
fn the_run_index_keys_are_the_ones_of_the_run_index() {
    // One definition of the owner partition and of the run index's sort key, in `run_index`: this module re-exports them.
    assert_eq!(owner_pk("arn:x"), ham_core::run_index::owner_pk("arn:x"));
    assert_eq!(
        search_sk("2026-10-15T08:00:00.000Z", "01K83D0000000000000000000A"),
        ham_core::run_index::search_sk("2026-10-15T08:00:00.000Z", "01K83D0000000000000000000A")
    );
    assert_eq!(
        ham_core::runsearch::SK_PREFIX,
        ham_core::run_index::SEARCH_PREFIX
    );
}

// ---- queries ---------------------------------------------------------------------------------------------------------

#[test]
fn a_query_is_words_folded_to_lowercase_and_bounded() {
    let t = parse_query("  Calc   SERVER\tTests \n").unwrap();
    assert_eq!(
        t,
        vec![
            vec!['c', 'a', 'l', 'c'],
            "server".chars().collect::<Vec<_>>(),
            "tests".chars().collect::<Vec<_>>()
        ]
    );
    assert_eq!(parse_query(""), Err(QueryError::Empty));
    assert_eq!(parse_query("   \t\n"), Err(QueryError::Empty));
    assert_eq!(parse_query(&"a".repeat(201)), Err(QueryError::TooLong));
    assert!(parse_query(&"a".repeat(200)).is_ok());
    assert_eq!(parse_query("a b c d e f g h i"), Err(QueryError::TooMany));
    assert!(parse_query("a b c d e f g h").is_ok());
    assert_eq!(parse_query("bad\u{0}word"), Err(QueryError::Control));
    assert_eq!(parse_query("esc\u{1b}[31m"), Err(QueryError::Control));
    // A non-ASCII word folds too.
    assert_eq!(
        parse_query("ÉCOLE").unwrap()[0],
        "école".chars().collect::<Vec<_>>()
    );
}

// ---- what is searchable ----------------------------------------------------------------------------------------------

#[test]
fn only_messages_tool_titles_and_file_names_are_searchable() {
    let c0 = pieces_of_chunk(&good(0));
    let texts: Vec<(&str, Source)> = c0
        .iter()
        .map(|p| (p.text.as_str(), p.source.clone()))
        .collect();
    assert_eq!(
        texts,
        vec![
            (
                "I will start with the tests.",
                Source::Engine { seq: 0, rec: 1 }
            ),
            ("Edit tests/test_calc.py", Source::Engine { seq: 0, rec: 2 }),
        ]
    );
    assert_eq!(c0[0].message.as_deref(), Some("m1"));
    let c1 = pieces_of_chunk(&good(1));
    let texts: Vec<&str> = c1.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "The tests fail as expected. ",
            "Now the code.",
            "Edit src/calc.py"
        ]
    );
    assert_eq!(c1[0].message, c1[1].message);
    let c2 = pieces_of_chunk(&good(2));
    assert_eq!(
        c2.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
        vec!["Run pytest tests"]
    );
}

#[test]
fn thoughts_prompts_echoes_permissions_and_contradicting_records_are_never_searchable() {
    let c = chunk_of(
        0,
        vec![
            (
                "agent_thought_chunk",
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "THOUGHT-TEXT"}}),
            ),
            ("prompt", json!({"text": "PROMPT-TEXT"})),
            (
                "user_message_chunk",
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "ECHO-TEXT"}}),
            ),
            ("permission_request", json!({"title": "PERMISSION-TEXT"})),
            (
                "notice",
                json!({"text": "NOTICE-TEXT", "title": "NOTICE-TITLE"}),
            ),
            ("prompt_result", json!({"stopReason": "end_turn"})),
            // The outer kind says message, the payload says thought: the record contradicts itself.
            (
                "agent_message_chunk",
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "MIXED-TEXT"}}),
            ),
            // An image block is not text.
            (
                "agent_message_chunk",
                json!({"content": {"type": "image", "text": "IMAGE-TEXT"}}),
            ),
            // A message with no text.
            ("agent_message_chunk", json!({"content": {"type": "text"}})),
            // A tool update that only moves the status says nothing.
            (
                "tool_call_update",
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed"}),
            ),
        ],
    );
    assert_eq!(pieces_of_chunk(&c), Vec::<Piece>::new());
}

#[test]
fn a_tool_line_names_the_title_and_each_file_once() {
    let c = chunk_of(
        4,
        vec![
            (
                "tool_call",
                json!({"sessionUpdate": "tool_call", "title": "Edit src/a.rs", "locations": [{"path": "src/a.rs"}, {"path": "src/b.rs"}]}),
            ),
            (
                "tool_call",
                json!({"title": "Write", "rawInput": {"file_path": "docs/x.md", "path": "docs/y.md"}}),
            ),
            (
                "tool_call_update",
                json!({"title": "Edit src/a.rs", "locations": [{"path": "src/a.rs"}]}),
            ),
        ],
    );
    let t: Vec<String> = pieces_of_chunk(&c).into_iter().map(|p| p.text).collect();
    assert_eq!(
        t,
        vec![
            "Edit src/a.rs src/b.rs",
            "Write docs/x.md docs/y.md",
            "Edit src/a.rs"
        ]
    );
}

// ---- the body --------------------------------------------------------------------------------------------------------

#[test]
fn a_short_run_makes_a_full_body_in_order_with_the_place_of_each_piece() {
    let mut b = BodyBuilder::new();
    b.add_objective("Build a calculator   with\ntests");
    b.add_answer("use pytest");
    b.add_answer("  ");
    for seq in 0..=2 {
        b.head_chunk(seq, pieces_of_chunk(&good(seq)));
    }
    let body = b.finish(true);
    let lines: Vec<&str> = body.body.lines().collect();
    assert_eq!(
        lines,
        vec![
            "Build a calculator with tests",
            "use pytest",
            "I will start with the tests.",
            "Edit tests/test_calc.py",
            "The tests fail as expected. Now the code.",
            "Edit src/calc.py",
            "Run pytest tests",
        ]
    );
    assert!(body.body.ends_with('\n'));
    assert_eq!(body.coverage, Coverage::Full);
    assert_eq!(body.through, Some(2));
    let at = |i: usize| (body.segs[i].seq, body.segs[i].rec);
    assert_eq!(body.segs.len(), 7);
    assert_eq!((at(0), at(1)), ((None, 0), (None, 0)));
    assert_eq!((at(2), at(3)), ((Some(0), 1), (Some(0), 2)));
    // The joined message sits where its first fragment was.
    assert_eq!(
        (at(4), at(5), at(6)),
        ((Some(1), 0), (Some(1), 2), (Some(2), 1))
    );
    // Each place starts the piece it names.
    for (seg, line) in body.segs.iter().zip(&lines) {
        assert!(body.body[seg.start as usize..].starts_with(line));
    }
}

#[test]
fn a_long_run_keeps_the_start_and_the_end_marks_the_middle_and_stays_inside_the_limit() {
    let mut b = BodyBuilder::new();
    b.add_objective("the objective");
    let filler = |i: u64| format!("message {i} {}", "word ".repeat(150));
    let chunks = 600u64;
    // The tail first (newest chunks), as the indexer reads them, then the head.
    let mut seq = chunks - 1;
    loop {
        let text = if seq == chunks - 1 {
            "LAST-MARKER final answer".to_string()
        } else {
            filler(seq)
        };
        if b.tail_chunk(seq, vec![engine(seq, 0, &text, Some(&format!("m{seq}")))]) || seq == 0 {
            break;
        }
        seq -= 1;
    }
    for seq in 0..chunks {
        let text = if seq == 0 {
            "FIRST-MARKER begin".to_string()
        } else if seq == 300 {
            "MIDDLE-MARKER".to_string()
        } else {
            filler(seq)
        };
        if b.head_chunk(seq, vec![engine(seq, 0, &text, Some(&format!("m{seq}")))]) {
            break;
        }
    }
    assert!(!b.head_open() && !b.tail_open());
    let body = b.finish(true);
    assert!(
        body.body.len() <= BODY_MAX_BYTES,
        "{} bytes",
        body.body.len()
    );
    assert!(
        body.body.len() > BODY_MAX_BYTES - 1024,
        "{} bytes: the budget should be used",
        body.body.len()
    );
    assert!(
        body.body.contains("the objective")
            && body.body.contains("FIRST-MARKER")
            && body.body.contains("LAST-MARKER")
    );
    assert!(!body.body.contains("MIDDLE-MARKER"));
    assert!(body.body.contains(GAP_MARKER));
    assert_eq!(body.coverage, Coverage::Partial, "the middle is left out");
    assert_eq!(body.through, Some(chunks - 1));
    // The head holds at most its share and the tail at most its share.
    let marker = body.body.find(GAP_MARKER).unwrap();
    assert!(marker <= HEAD_BYTES, "{marker}");
    assert!(
        body.body.len() - marker <= TAIL_BYTES + 8,
        "{}",
        body.body.len() - marker
    );
}

#[test]
fn a_piece_longer_than_the_limit_is_cut_and_the_body_says_it_is_partial() {
    let mut b = BodyBuilder::new();
    b.head_chunk(
        0,
        vec![
            engine(0, 0, &"é".repeat(PIECE_MAX_BYTES), None),
            engine(0, 1, &format!("{}tail", "x".repeat(10_000_000)), None),
        ],
    );
    let body = b.finish(true);
    assert_eq!(body.coverage, Coverage::Partial);
    let lines: Vec<&str> = body.body.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(
        lines.iter().all(|l| l.len() <= PIECE_MAX_BYTES),
        "{:?}",
        lines.iter().map(|l| l.len()).collect::<Vec<_>>()
    );
    assert!(lines[0].chars().all(|c| c == 'é'));
}

#[test]
fn the_users_own_words_have_their_own_limits() {
    let mut b = BodyBuilder::new();
    b.add_objective(&"é".repeat(10_000));
    for i in 0..6 {
        b.add_answer(&format!("{i}{}", "a".repeat(1500)));
    }
    let body = b.finish(true);
    let lines: Vec<&str> = body.body.lines().collect();
    assert!(
        lines[0].len() <= OBJECTIVE_MAX_BYTES && lines[0].chars().all(|c| c == 'é'),
        "{}",
        lines[0].len()
    );
    let answers: usize = lines[1..].iter().map(|l| l.len()).sum();
    assert!(lines[1..].iter().all(|l| l.len() <= ANSWER_MAX_BYTES));
    assert!(answers <= ANSWERS_MAX_BYTES, "{answers}");
    assert_eq!(body.coverage, Coverage::Partial);
    // The objective is first even when it is added last.
    let mut b = BodyBuilder::new();
    b.add_answer("later");
    b.add_objective("first");
    assert!(b.finish(true).body.starts_with("first\nlater\n"));
}

#[test]
fn fragments_of_one_message_join_as_written_in_the_head_and_across_chunks_in_the_tail() {
    // Head, oldest first: "ex" + "pected" must make "expected", and "foo " + "bar" keeps its space.
    let mut b = BodyBuilder::new();
    b.head_chunk(
        0,
        vec![
            engine(0, 0, "ex", Some("m1")),
            engine(0, 1, "pected ", Some("m1")),
        ],
    );
    b.head_chunk(
        1,
        vec![
            engine(1, 0, "foo ", Some("m1")),
            engine(1, 1, "bar", Some("m1")),
            engine(1, 2, "other", Some("m2")),
        ],
    );
    let body = b.finish(true);
    assert_eq!(body.body, "expected foo bar\nother\n");
    assert_eq!(
        (body.segs[0].seq, body.segs[0].rec, body.segs[1].seq),
        (Some(0), 0, Some(1))
    );
    // Tail, newest first: the message started in the older chunk and ends in the newer one.
    let mut b = BodyBuilder::new();
    b.tail_chunk(
        5,
        vec![
            engine(5, 0, "ld of text", Some("m9")),
            engine(5, 1, "done", Some("m10")),
        ],
    );
    b.tail_chunk(4, vec![engine(4, 0, "hello wor", Some("m9"))]);
    let body = b.finish(true);
    assert_eq!(body.body, "hello world of text\ndone\n");
    assert_eq!((body.segs[0].seq, body.segs[0].rec), (Some(4), 0));
}

#[test]
fn a_tool_line_that_repeats_adds_nothing() {
    let mut b = BodyBuilder::new();
    b.head_chunk(
        0,
        vec![
            engine(0, 0, "Edit src/a.rs", None),
            engine(0, 1, "edit SRC/a.rs", None),
            engine(0, 2, "Run tests", None),
        ],
    );
    b.tail_chunk(
        1,
        vec![
            engine(1, 0, "Run   tests", None),
            engine(1, 1, "final", None),
        ],
    );
    assert_eq!(b.finish(true).body, "Edit src/a.rs\nRun tests\nfinal\n");
}

#[test]
fn a_run_with_a_failed_chunk_or_a_gap_between_head_and_tail_is_partial() {
    let mut b = BodyBuilder::new();
    b.head_chunk(0, vec![engine(0, 0, "a", None)]);
    assert_eq!(b.finish(false).coverage, Coverage::Partial);
    let mut b = BodyBuilder::new();
    b.head_chunk(0, vec![engine(0, 0, "a", None)]);
    b.tail_chunk(7, vec![engine(7, 0, "z", None)]);
    let body = b.finish(true);
    assert_eq!(body.coverage, Coverage::Partial);
    assert_eq!(body.body, format!("a\n{GAP_MARKER}\nz\n"));
    assert_eq!(body.segs.len(), 3);
    assert_eq!(body.segs[1].seq, None);
    // Adjacent chunks leave nothing out.
    let mut b = BodyBuilder::new();
    b.head_chunk(0, vec![engine(0, 0, "a", None)]);
    b.tail_chunk(1, vec![engine(1, 0, "z", None)]);
    assert_eq!(b.finish(true).coverage, Coverage::Full);
    // An empty run has an empty body.
    let body = BodyBuilder::new().finish(true);
    assert_eq!(
        (body.body.as_str(), body.through, body.coverage),
        ("", None, Coverage::Full)
    );
}

#[test]
fn only_the_first_places_are_recorded_when_there_are_very_many_pieces() {
    let mut b = BodyBuilder::new();
    let pieces: Vec<Piece> = (0..3000u32)
        .map(|i| engine(0, i, &format!("p{i}"), None))
        .collect();
    b.head_chunk(0, pieces);
    let body = b.finish(true);
    assert_eq!(body.segs.len(), SEGS_MAX);
    assert!(body.body.lines().count() > SEGS_MAX);
    let wire = segs_to_string(&body.segs);
    assert_eq!(segs_from_str(&wire), body.segs);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, ..ProptestConfig::default() })]

    #[test]
    fn whatever_the_text_the_body_is_bounded_valid_and_mapped(
        words in proptest::collection::vec(".{0,300}", 0..120),
        big in 0usize..9000,
        obj in ".{0,200}",
    ) {
        let mut b = BodyBuilder::new();
        b.add_objective(&obj);
        let pieces: Vec<Piece> = words.iter().enumerate().map(|(i, w)| engine(0, i as u32, &format!("{w}{}", "ß".repeat(big % 7 * 300)), if i % 3 == 0 { Some("m") } else { None })).collect();
        let (a, z) = pieces.split_at(pieces.len() / 2);
        b.head_chunk(0, a.to_vec());
        b.tail_chunk(9, z.to_vec());
        let body = b.finish(true);
        prop_assert!(body.body.len() <= BODY_MAX_BYTES);
        prop_assert!(body.segs.len() <= SEGS_MAX);
        let mut last = 0u32;
        for s in &body.segs {
            prop_assert!(s.start >= last);
            prop_assert!(body.body.is_char_boundary(s.start as usize));
            last = s.start;
        }
    }
}

// ---- matching --------------------------------------------------------------------------------------------------------

fn item(body: &str, segs: &[Seg]) -> SearchItem {
    SearchItem {
        sk: "SRCH#2026-10-16T10:00:00.000Z#01K83D0000000000000000000A".into(),
        run_id: "01K83D0000000000000000000A".into(),
        created_at: "2026-10-16T10:00:00.000Z".into(),
        body: Some(body.into()),
        segs: Some(segs_to_string(segs)),
        coverage: Some(Coverage::Full),
        ..SearchItem::default()
    }
}

fn built() -> SearchItem {
    let mut b = BodyBuilder::new();
    b.add_objective("Build a calculator with tests");
    for seq in 0..=2 {
        b.head_chunk(seq, pieces_of_chunk(&good(seq)));
    }
    let body = b.finish(true);
    item(&body.body, &body.segs)
}

#[test]
fn a_phrase_from_the_middle_of_a_chat_finds_the_piece_and_its_place() {
    let it = built();
    let m = find_match(&it, &parse_query("FAIL as EXPECTED").unwrap()).unwrap();
    assert_eq!(m.field, HitField::Chat);
    assert_eq!((m.seq, m.offset), (Some(1), Some(0)));
    assert!(
        m.snippet.contains("The tests fail as expected."),
        "{}",
        m.snippet
    );
    // A file name finds the tool line.
    let m = find_match(&it, &parse_query("src/calc.py").unwrap()).unwrap();
    assert_eq!((m.seq, m.offset), (Some(1), Some(2)));
    assert_eq!(m.snippet, "Edit src/calc.py");
    // The objective is the user's own text: no place in the transcript.
    let m = find_match(&it, &parse_query("calculator").unwrap()).unwrap();
    assert_eq!((m.field, m.seq, m.offset), (HitField::Chat, None, None));
}

#[test]
fn every_word_must_occur_and_the_earliest_chat_occurrence_decides_the_place() {
    let it = built();
    assert!(find_match(&it, &parse_query("calculator zebra").unwrap()).is_none());
    assert!(find_match(&it, &parse_query("zebra").unwrap()).is_none());
    // Words may sit in different pieces: the place is where the earliest of them is.
    let m = find_match(&it, &parse_query("pytest tests/test_calc.py").unwrap()).unwrap();
    assert_eq!((m.seq, m.offset), (Some(0), Some(2)), "{m:?}");
}

#[test]
fn the_other_fields_match_too_and_a_chat_match_wins() {
    let mut it = built();
    it.prompt_head = Some("Please build me a Rust calculator".into());
    it.repository = Some("cc:hamh-e2e-calc".into());
    it.model = Some("Claude Sonnet 5.5".into());
    let m = find_match(&it, &parse_query("rust").unwrap()).unwrap();
    assert_eq!((m.field, m.seq, m.offset), (HitField::Prompt, None, None));
    assert_eq!(m.snippet, "Please build me a Rust calculator");
    assert_eq!(
        find_match(&it, &parse_query("e2e-calc").unwrap())
            .unwrap()
            .field,
        HitField::Repository
    );
    assert_eq!(
        find_match(&it, &parse_query("SONNET").unwrap())
            .unwrap()
            .field,
        HitField::Model
    );
    // Words from different fields still match as a set; the first word's field names the hit.
    let m = find_match(&it, &parse_query("sonnet rust").unwrap()).unwrap();
    assert_eq!(m.field, HitField::Model);
    // "calculator" is in the chat body and the prompt: the chat wins.
    assert_eq!(
        find_match(&it, &parse_query("calculator").unwrap())
            .unwrap()
            .field,
        HitField::Chat
    );
    // A run with no body is searched on its other fields.
    let bare = SearchItem {
        body: None,
        segs: None,
        coverage: None,
        prompt_head: Some("fix the parser".into()),
        ..SearchItem::default()
    };
    assert_eq!(
        find_match(&bare, &parse_query("PARSER").unwrap())
            .unwrap()
            .field,
        HitField::Prompt
    );
    assert!(find_match(&SearchItem::default(), &parse_query("x").unwrap()).is_none());
}

#[test]
fn a_snippet_is_one_short_line_around_the_match_inside_its_piece() {
    let long = format!("{} NEEDLE {}", "left ".repeat(80), "right ".repeat(80));
    let it = item(
        &format!("short first\n{long}\nshort last\n"),
        &[
            Seg {
                start: 0,
                seq: None,
                rec: 0,
            },
            Seg {
                start: 12,
                seq: Some(3),
                rec: 7,
            },
            Seg {
                start: 12 + long.len() as u32 + 1,
                seq: Some(3),
                rec: 8,
            },
        ],
    );
    let m = find_match(&it, &parse_query("needle").unwrap()).unwrap();
    assert!(m.snippet.contains("NEEDLE"), "{}", m.snippet);
    assert!(
        m.snippet.starts_with(GAP_MARKER) && m.snippet.ends_with(GAP_MARKER),
        "{}",
        m.snippet
    );
    assert!(
        m.snippet.chars().count() <= 125,
        "{}",
        m.snippet.chars().count()
    );
    assert!(!m.snippet.contains('\n') && !m.snippet.contains("short"));
    assert_eq!((m.seq, m.offset), (Some(3), Some(7)));
    // Control characters in stored text never reach a snippet.
    let it = item(
        "a\u{1b}[31mneedle\u{7}b\n",
        &[Seg {
            start: 0,
            seq: Some(1),
            rec: 0,
        }],
    );
    let m = find_match(&it, &parse_query("needle").unwrap()).unwrap();
    assert!(!m.snippet.chars().any(char::is_control), "{:?}", m.snippet);
}

#[test]
fn matching_ignores_case_in_any_script_and_survives_text_whose_lowercase_is_longer() {
    let it = item(
        "Die STRASSE und das Café ÉCOLE İstanbul\n",
        &[Seg {
            start: 0,
            seq: Some(2),
            rec: 4,
        }],
    );
    for q in ["strasse", "CAFÉ", "école", "ÉCOLE", "i̇stanbul", "istanbul"] {
        let r = find_match(&it, &parse_query(q).unwrap());
        // The Turkish dotted capital I folds to "i" followed by a combining dot, so its first character "i" matches "istanbul".
        assert!(r.is_some() || q == "i̇stanbul", "{q}");
    }
    assert_eq!(
        find_match(&it, &parse_query("café").unwrap())
            .unwrap()
            .offset,
        Some(4)
    );
}

// ---- the wire --------------------------------------------------------------------------------------------------------

#[test]
fn a_search_response_has_fixed_member_names() {
    let r = SearchResponse {
        hits: vec![SearchHit {
            run_id: "01K83D0000000000000000000A".into(),
            created_at: "2026-10-16T10:00:00.000Z".into(),
            field: HitField::Chat,
            snippet: "…the tests fail…".into(),
            seq: Some(1),
            offset: Some(0),
            open: Some("abc_-1".into()),
            coverage: Some(Coverage::Partial),
        }],
        searched: 40,
        partial: true,
        more: true,
        next: Some("tok".into()),
        partial_coverage: 3,
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v,
        json!({
            "hits": [{"runId": "01K83D0000000000000000000A", "createdAt": "2026-10-16T10:00:00.000Z", "field": "chat", "snippet": "…the tests fail…",
                      "seq": 1, "offset": 0, "open": "abc_-1", "coverage": "partial"}],
            "searched": 40, "partial": true, "more": true, "next": "tok", "partialCoverage": 3
        })
    );
    assert_eq!(parse_response(&serde_json::to_vec(&r).unwrap()).unwrap(), r);
    let bare = parse_response(b"{\"hits\":[],\"newer\":1}").unwrap();
    assert_eq!(bare, SearchResponse::default());
    assert!(parse_response(b"nope").is_err());
}

#[test]
fn the_search_path_encodes_every_value() {
    let q = SearchQuery {
        q: "fix the parser & more".into(),
        ..SearchQuery::default()
    };
    assert_eq!(q.path(), "/v1/search?q=fix%20the%20parser%20%26%20more");
    let q = SearchQuery {
        q: "é".into(),
        since: Some("2026-09-01".into()),
        limit: Some(50),
        after: Some("a_b-c".into()),
    };
    assert_eq!(
        q.path(),
        "/v1/search?q=%C3%A9&since=2026-09-01&limit=50&after=a_b-c"
    );
}

#[test]
fn a_stored_item_reads_back_from_its_attribute_names() {
    let stored = json!({"PK": "OWNER#x", "SK": "SRCH#2026-10-16T10:00:00.000Z#01K83D0000000000000000000A", "runId": "01K83D0000000000000000000A",
                        "createdAt": "2026-10-16T10:00:00.000Z", "promptHead": "p", "model": "m", "repository": "r", "body": "b\n", "segs": "[[0,-1,0]]",
                        "coverage": "full", "bodyThrough": 9, "indexedAt": "2026-10-16T11:00:00.000Z", "outcome": "done", "lastActivity": "x"});
    let it: SearchItem = serde_json::from_value(stored).unwrap();
    assert_eq!(
        (
            it.prompt_head.as_deref(),
            it.model.as_deref(),
            it.repository.as_deref(),
            it.coverage,
            it.body_through
        ),
        (
            Some("p"),
            Some("m"),
            Some("r"),
            Some(Coverage::Full),
            Some(9)
        )
    );
    assert_eq!(
        segs_from_str(it.segs.as_deref().unwrap()),
        vec![Seg {
            start: 0,
            seq: None,
            rec: 0
        }]
    );
    // An item the run index wrote first has no body at all.
    let early: SearchItem = serde_json::from_value(
        json!({"SK": "SRCH#x#y", "runId": "y", "createdAt": "x", "promptHead": "p"}),
    )
    .unwrap();
    assert_eq!((early.body, early.coverage), (None, None));
    // Unreadable places are none, never a panic.
    for bad in ["", "nope", "[1,2]", "[[1,2]]", "[[-1,0,0]]", "[[0,0,-1]]"] {
        assert!(segs_from_str(bad).is_empty(), "{bad}");
    }
}

// ---- findings of the implementation review (2026-10-16) --------------------------------------------------------------

fn piece_in(seq: u64, rec: u32, text: &str) -> Piece {
    engine(seq, rec, text, None)
}

#[test]
fn a_message_id_is_kept_for_joining_only_when_it_is_short() {
    let c = chunk_of(
        0,
        vec![
            msg_with("short", "m-short"),
            msg_with("long", &"i".repeat(2000)),
            msg_with("edge", &"e".repeat(128)),
            msg_with("over", &"o".repeat(129)),
        ],
    );
    let p = pieces_of_chunk(&c);
    assert_eq!(
        p.iter()
            .map(|p| p.message.as_ref().map(String::len))
            .collect::<Vec<_>>(),
        vec![Some(7), None, Some(128), None]
    );
}

fn msg_with(text: &str, id: &str) -> (&'static str, Value) {
    (
        "agent_message_chunk",
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}, "messageId": id}),
    )
}

#[test]
fn a_line_the_budget_dropped_is_still_welcome_where_there_is_room() {
    // The head is full of distinct tool lines; one more is dropped. The same line offered to the tail is not "seen": it is kept.
    let mut b = BodyBuilder::new();
    let filler: Vec<Piece> = (0..40)
        .map(|i| piece_in(0, i, &format!("Edit file-{i:03}.rs {}", "x".repeat(600))))
        .collect();
    assert!(b.head_chunk(0, filler));
    assert!(b.head_chunk(1, vec![piece_in(1, 0, "LATE-LINE")]));
    b.tail_chunk(2, vec![piece_in(2, 0, "LATE-LINE")]);
    let body = b.finish(true);
    assert_eq!(body.body.matches("LATE-LINE").count(), 1);
    assert!(
        body.body.ends_with("LATE-LINE\n"),
        "{}",
        &body.body[body.body.len() - 20..]
    );
}

#[test]
fn a_secret_split_across_the_fragments_of_one_message_is_redacted_after_they_are_joined() {
    let (front, back) = ("Tr0ub", "4dor3xyz");
    let secret = [front, back].concat();
    let build = || {
        let mut b = BodyBuilder::new();
        b.add_objective(&format!("Deploy with password={secret} now"));
        b.head_chunk(
            0,
            vec![
                engine(0, 0, &format!("set password={front}"), Some("m1")),
                engine(0, 1, &format!("{back} in the file"), Some("m1")),
            ],
        );
        b
    };
    let plain = build().finish(true);
    assert!(
        plain.body.contains(&secret),
        "joined text holds what the fragments held: {}",
        plain.body
    );
    let body = build().finish_with(true, &redact_tokens);
    assert!(
        !body.body.contains(&secret) && !body.body.contains("Tr0ub4dor"),
        "{}",
        body.body
    );
    assert!(
        body.body.contains("set password=") && body.body.contains("in the file"),
        "{}",
        body.body
    );
    assert_eq!(
        body.segs.len(),
        2,
        "the places are rebuilt for the redacted text"
    );
}

#[test]
fn redaction_that_makes_text_longer_still_leaves_a_body_inside_the_limit() {
    let mut b = BodyBuilder::new();
    // "password=x" grows to "password=[redacted]": nine more bytes each time.
    let pieces: Vec<Piece> = (0..3000)
        .map(|i| piece_in(0, i, &format!("password=v{i:04}")))
        .collect();
    b.head_chunk(0, pieces.clone());
    b.tail_chunk(1, pieces);
    let body = b.finish_with(true, &redact_tokens);
    assert!(
        body.body.len() <= BODY_MAX_BYTES,
        "{} bytes",
        body.body.len()
    );
    assert!(body.body.contains("[redacted]") && !body.body.contains("v0001"));
    assert!(body.body.is_char_boundary(body.body.len()));
    assert!(
        body.segs
            .iter()
            .all(|s| (s.start as usize) < body.body.len())
    );
}

#[test]
fn one_chunk_larger_than_the_tail_is_shared_by_head_and_tail_without_overlap() {
    // 30 pieces of about 700 bytes: 21 KB, more than the tail's 12 KiB and less than head plus tail.
    let pieces: Vec<Piece> = (0..30)
        .map(|i| piece_in(0, i, &format!("piece-{i:02} {}", "w".repeat(690))))
        .collect();
    let mut b = BodyBuilder::new();
    assert!(b.tail_chunk(0, pieces.clone()), "the tail is full");
    assert_eq!(
        b.tail_cut_seq(),
        Some(0),
        "and it left out the start of this chunk"
    );
    b.head_chunk(0, pieces);
    let body = b.finish(true);
    for i in 0..30 {
        assert_eq!(
            body.body.matches(&format!("piece-{i:02} ")).count(),
            1,
            "piece {i} once: {}",
            body.body.len()
        );
    }
    assert_eq!(body.coverage, Coverage::Full, "nothing was left out");
    let order: Vec<usize> = (0..30)
        .map(|i| body.body.find(&format!("piece-{i:02} ")).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "in order");
    assert_eq!(body.segs.len(), 30);

    // 70 pieces of 700 bytes are more than head and tail hold together: the middle is left out and the body says so.
    let pieces: Vec<Piece> = (0..70)
        .map(|i| piece_in(0, i, &format!("piece-{i:02} {}", "w".repeat(690))))
        .collect();
    let mut b = BodyBuilder::new();
    b.tail_chunk(0, pieces.clone());
    b.head_chunk(0, pieces);
    let body = b.finish(true);
    assert!(
        body.body.contains("piece-00 ")
            && body.body.contains("piece-69 ")
            && !body.body.contains("piece-35 ")
    );
    assert_eq!(body.coverage, Coverage::Partial);
    assert!(body.body.len() <= BODY_MAX_BYTES);
    assert_eq!(body.body.matches("piece-69 ").count(), 1);
}

#[test]
fn a_secret_split_across_the_seam_between_head_and_tail_is_redacted_whole() {
    // The tail is full of other pieces, so the first fragment of one message does not fit it and goes to the head: the message
    // straddles the seam with nothing lost between its two halves.
    let (front, back) = ("Tr0ub", "4dor3xyz");
    let secret = [front, back].concat();
    let mut b = BodyBuilder::new();
    let fillers: Vec<Piece> = (0..17)
        .map(|i| piece_in(1, i, &format!("piece-{i:02} {}", "w".repeat(690))))
        .collect();
    assert!(
        !b.tail_chunk(1, fillers),
        "the tail has room for the last fragment"
    );
    let first = engine(
        0,
        0,
        &format!("{}set password={front}", "x ".repeat(250)),
        Some("m1"),
    );
    let second = engine(0, 1, &format!("{back} in the file"), Some("m1"));
    assert!(
        b.tail_chunk(0, vec![first.clone(), second.clone()]),
        "the first fragment does not fit: the tail stops inside the chunk"
    );
    assert_eq!(b.tail_cut_seq(), Some(0));
    b.head_chunk(0, vec![first, second]);
    let body = b.finish_with(true, &redact_tokens);
    assert!(
        !body.body.contains(&secret) && !body.body.contains("Tr0ub4") && !body.body.contains(back),
        "{}",
        body.body
    );
    assert!(
        body.body.contains("set password=") && body.body.contains("in the file"),
        "{}",
        body.body
    );
    assert_eq!(body.coverage, Coverage::Full, "nothing was left out");
}

#[test]
fn a_chunk_the_tail_took_whole_is_not_offered_to_the_head_again() {
    let mut b = BodyBuilder::new();
    b.tail_chunk(0, vec![piece_in(0, 0, "all of it")]);
    assert_eq!(b.tail_cut_seq(), None);
}

#[test]
fn the_snippet_of_a_stored_secret_is_redacted_before_it_is_clipped() {
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    let body = format!("token {token} needle\n");
    let it = item(
        &body,
        &[Seg {
            start: 0,
            seq: Some(1),
            rec: 2,
        }],
    );
    let m = find_match_redacted(&it, &parse_query("needle").unwrap(), &redact_tokens).unwrap();
    assert!(
        !m.snippet.contains(&token) && !m.snippet.contains("a1B2c3D4"),
        "{}",
        m.snippet
    );
    assert!(
        m.snippet.contains("needle") && m.snippet.contains("[redacted]"),
        "{}",
        m.snippet
    );
    assert_eq!((m.seq, m.offset), (Some(1), Some(2)));
    // A match that exists only inside the secret is no match in the snippet, but the run still matches: the snippet starts the piece.
    let only = find_match_redacted(
        &item(
            &format!("see {token}\n"),
            &[Seg {
                start: 0,
                seq: Some(3),
                rec: 4,
            }],
        ),
        &parse_query(&token).unwrap(),
        &redact_tokens,
    )
    .unwrap();
    assert!(!only.snippet.contains(&token), "{}", only.snippet);
}

#[test]
fn a_cut_never_ends_inside_a_word_when_there_is_a_space_to_fall_back_on() {
    // A credential that straddles the cut would keep its front half, which no redaction pattern matches: the cut falls back to the
    // last space instead.
    let word = "SECRETTOKEN0123456789";
    // Head: the piece is clipped to PIECE_MAX_BYTES inside the token.
    let text = format!("{}{word}", "a ".repeat(PIECE_MAX_BYTES / 2 - 8));
    assert!(text.len() > PIECE_MAX_BYTES && text.len() - word.len() < PIECE_MAX_BYTES);
    let mut b = BodyBuilder::new();
    b.head_chunk(0, vec![engine(0, 0, &text, None)]);
    let body = b.finish(true);
    assert_eq!(body.coverage, Coverage::Partial);
    assert!(
        !body.body.contains("SECRE"),
        "{}",
        &body.body[body.body.len() - 40..]
    );
    assert!(
        body.body.trim_end().ends_with(" a"),
        "the cut falls back to a whole word"
    );
    // Tail: the piece is clipped to its last PIECE_MAX_BYTES, starting inside the token.
    let text = format!("{word}{}", " a".repeat(PIECE_MAX_BYTES / 2 - 8));
    assert!(text.len() > PIECE_MAX_BYTES && text.len() - word.len() < PIECE_MAX_BYTES);
    let mut b = BodyBuilder::new();
    b.tail_chunk(0, vec![engine(0, 0, &text, None)]);
    let body = b.finish(true);
    assert_eq!(body.coverage, Coverage::Partial);
    assert!(
        !body.body.contains("TOKEN0123456789"),
        "{}",
        &body.body[..40]
    );
    assert!(
        body.body.starts_with("a a"),
        "the cut falls back to a whole word"
    );
    // A single word longer than the limit has no space to fall back on and is cut where the limit falls.
    let mut b = BodyBuilder::new();
    b.head_chunk(
        0,
        vec![engine(0, 0, &"x".repeat(PIECE_MAX_BYTES + 10), None)],
    );
    assert_eq!(b.finish(true).body.trim_end().len(), PIECE_MAX_BYTES);
}
