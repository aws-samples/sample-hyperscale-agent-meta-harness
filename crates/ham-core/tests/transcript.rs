//! The stored conversation's chunk format and the transcript route's wire types. The
//! chunk files in `tests/fixtures/transcript/` come from Python's zlib (`gen.py`), so the reader is checked against objects the Rust
//! writer helpers did not make.
#![allow(clippy::unwrap_used)]

use ham_core::chat::GapCause;
use ham_core::gzip::gzip_stored;
use ham_core::redact::redact_tokens;
use ham_core::transcript::{
    Before, CHUNK_VERSION, Chunk, ChunkError, ChunkHeader, ChunkRecord, CompletenessEntry, Dir,
    GapCode, GapKind, MAX_RECORD_DEPTH, MAX_RECORDS_PER_CHUNK, Origin, TranscriptGap,
    TranscriptPage, TranscriptQuery, TranscriptRecord, TranscriptState, TranscriptSummary,
    chunk_gzip_stored, chunk_key, chunk_ndjson, decode_chunk, parse_page, seq_of_key, seq_of_name,
    transcript_prefix,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

const RUN: &str = "01K83D0RUN0RUN0RUN0RUN0RUN";
const INC_A: &str = "01K83DA0A0A0A0A0A0A0A0A0A0";
const INC_B: &str = "01K83DB0B0B0B0B0B0B0B0B0B0";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcript")
}

fn fixture_bytes(path: &std::path::Path) -> Vec<u8> {
    let hex = std::fs::read_to_string(format!("{}.hex", path.display())).unwrap();
    ham_core::gzip::unhex(&hex).unwrap()
}

fn header(seq: u64, inc: &str, first: u64, last: u64) -> ChunkHeader {
    ChunkHeader {
        v: CHUNK_VERSION,
        seq,
        incarnation: inc.into(),
        first_rec: first,
        last_rec: last,
        created_at: "2026-10-16T10:00:00.000Z".into(),
        engine: "cline".into(),
        protocol: "1.38.0".into(),
    }
}

fn rec(n: u64, kind: &str, data: Value) -> ChunkRecord {
    ChunkRecord {
        n,
        t: Some("2026-10-16T10:00:00.000Z".into()),
        dir: Dir::In,
        kind: kind.into(),
        data,
    }
}

fn chunk(seq: u64, inc: &str, records: Vec<ChunkRecord>) -> Chunk {
    let first = records.first().map_or(0, |r| r.n);
    let last = records.last().map_or(0, |r| r.n);
    Chunk {
        header: header(seq, inc, first, last),
        records,
    }
}

/// A gzip object around raw NDJSON text.
fn object(text: &str) -> Vec<u8> {
    gzip_stored(text.as_bytes())
}

fn head_line(h: &ChunkHeader) -> String {
    serde_json::to_string(h).unwrap()
}

// ---- keys ------------------------------------------------------------------------------------------------------------

#[test]
fn keys_are_the_zero_padded_names_under_the_runs_agent_prefix() {
    assert_eq!(
        transcript_prefix(RUN),
        format!("runs/{RUN}/agents/direct/turns/1/transcript/")
    );
    assert_eq!(
        chunk_key(RUN, 0),
        format!("runs/{RUN}/agents/direct/turns/1/transcript/000000000000.ndjson.gz")
    );
    assert_eq!(
        chunk_key(RUN, 4821),
        format!("runs/{RUN}/agents/direct/turns/1/transcript/000000004821.ndjson.gz")
    );
    assert_eq!(seq_of_key(RUN, &chunk_key(RUN, 4821)), Some(4821));
    assert_eq!(seq_of_key(RUN, &chunk_key(RUN, 0)), Some(0));
}

#[test]
fn only_a_twelve_digit_name_directly_under_the_prefix_is_a_chunk() {
    let p = transcript_prefix(RUN);
    for (name, want) in [
        ("000000000007.ndjson.gz", Some(7)),
        ("00000000007.ndjson.gz", None),   // eleven digits
        ("0000000000007.ndjson.gz", None), // thirteen
        ("00000000000a.ndjson.gz", None),  // not a digit
        ("000000000007.ndjson", None),     // not gzip
        ("000000000007.ndjson.gz.tmp", None),
        ("000000000007.NDJSON.GZ", None),
        ("-00000000007.ndjson.gz", None),
        ("+00000000007.ndjson.gz", None),
        ("", None),
        ("seal", None),
        ("manifest.json", None),
        ("sub/000000000007.ndjson.gz", None),
        ("000000000007.ndjson.gz/", None),
    ] {
        assert_eq!(seq_of_key(RUN, &format!("{p}{name}")), want, "{name}");
        assert_eq!(seq_of_name(name), want, "{name}");
    }
    // Another run's prefix and the bare prefix are not this run's chunks.
    assert_eq!(
        seq_of_key(RUN, &chunk_key("01K83D0RUN0RUN0RUN0RUN0RUM", 3)),
        None
    );
    assert_eq!(seq_of_key(RUN, &p), None);
    assert_eq!(
        seq_of_key(
            RUN,
            &format!("runs/{RUN}/agents/direct/turns/1/000000000007.ndjson.gz")
        ),
        None
    );
    // Every number a u64 can hold fits twelve digits only below 10^12: larger names are refused.
    assert_eq!(seq_of_name("999999999999.ndjson.gz"), Some(999_999_999_999));
}

// ---- the format round trip -------------------------------------------------------------------------------------------

#[test]
fn a_chunk_round_trips_through_its_text_and_a_stored_gzip_object() {
    let c = chunk(
        3,
        INC_A,
        vec![
            rec(
                10,
                "agent_message_chunk",
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}, "messageId": "m1"}),
            ),
            rec(
                11,
                "tool_call",
                json!({"sessionUpdate": "tool_call", "kind": "edit", "title": "Edit a.rs", "toolCallId": "c1"}),
            ),
        ],
    );
    let text = String::from_utf8(chunk_ndjson(&c)).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert!(text.ends_with('\n'));
    assert!(
        text.lines().next().unwrap().contains("\"firstRec\":10"),
        "{text}"
    );
    assert_eq!(decode_chunk(&chunk_gzip_stored(&c), 3).unwrap(), c);
    // The payload keeps ACP's own `kind` next to the record's `kind`.
    let back = decode_chunk(&chunk_gzip_stored(&c), 3).unwrap();
    assert_eq!(back.records[1].kind, "tool_call");
    assert_eq!(back.records[1].data["kind"], "edit");
}

#[test]
fn a_chunk_with_a_header_and_no_records_is_valid_and_an_empty_one_is_not() {
    let h = header(0, INC_A, 0, 0);
    let ok = object(&format!("{}\n", head_line(&h)));
    assert_eq!(decode_chunk(&ok, 0).unwrap().records.len(), 0);
    // No trailing newline is fine as well.
    assert!(decode_chunk(&object(&head_line(&h)), 0).is_ok());
    assert_eq!(decode_chunk(&object(""), 0), Err(ChunkError::NoHeader));
    assert_eq!(decode_chunk(&object("\n"), 0), Err(ChunkError::NoHeader));
}

#[test]
fn unknown_members_of_the_header_and_of_a_record_are_dropped_not_kept() {
    let mut h: Value = serde_json::to_value(header(2, INC_A, 0, 0)).unwrap();
    h["futureField"] = json!(1);
    let line = json!({"n": 0, "t": "2026-10-16T10:00:00.000Z", "dir": "in", "kind": "notice", "data": {"text": "x"},
                      "author": "user", "origin": "harness", "role": "you", "actions": ["approve"]});
    let c = decode_chunk(&object(&format!("{h}\n{line}\n")), 2).unwrap();
    let back = serde_json::to_value(&c.records[0]).unwrap();
    assert_eq!(
        back,
        json!({"n": 0, "t": "2026-10-16T10:00:00.000Z", "dir": "in", "kind": "notice", "data": {"text": "x"}})
    );
}

#[test]
fn the_stored_form_has_fixed_member_names_so_it_can_move_to_the_protocol_crate_unchanged() {
    // The writer (agentd) cannot depend on this crate, so these bytes are the contract it is built against.
    let h = header(12, INC_A, 40, 52);
    assert_eq!(
        serde_json::to_string(&h).unwrap(),
        format!(
            "{{\"v\":1,\"seq\":12,\"incarnation\":\"{INC_A}\",\"firstRec\":40,\"lastRec\":52,\"createdAt\":\"2026-10-16T10:00:00.000Z\",\"engine\":\"cline\",\"protocol\":\"1.38.0\"}}"
        )
    );
    let r = rec(
        40,
        "tool_call",
        json!({"sessionUpdate": "tool_call", "kind": "edit", "title": "t"}),
    );
    // The record's own members come in this order; the payload's member order is the writer's (it depends on whether serde_json keeps
    // insertion order in the build, which cargo decides for the whole workspace), so the payload is compared as a value.
    let line = serde_json::to_string(&r).unwrap();
    let prefix = "{\"n\":40,\"t\":\"2026-10-16T10:00:00.000Z\",\"dir\":\"in\",\"kind\":\"tool_call\",\"data\":";
    assert!(line.starts_with(prefix) && line.ends_with('}'), "{line}");
    let data: Value = serde_json::from_str(&line[prefix.len()..line.len() - 1]).unwrap();
    assert_eq!(
        data,
        json!({"kind": "edit", "sessionUpdate": "tool_call", "title": "t"})
    );
    let out = ChunkRecord {
        dir: Dir::Out,
        t: None,
        ..rec(41, "prompt", json!({"text": "go"}))
    };
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        "{\"n\":41,\"dir\":\"out\",\"kind\":\"prompt\",\"data\":{\"text\":\"go\"}}"
    );
}

// ---- what makes a chunk unreadable -----------------------------------------------------------------------------------

#[test]
fn the_header_must_agree_with_the_key_and_be_well_formed() {
    let good = |h: &ChunkHeader| object(&format!("{}\n", head_line(h)));
    let h = header(5, INC_A, 0, 0);
    assert_eq!(
        decode_chunk(&good(&h), 6),
        Err(ChunkError::SeqMismatch { key: 6, header: 5 })
    );
    let mut v2 = h.clone();
    v2.v = 2;
    assert_eq!(decode_chunk(&good(&v2), 5), Err(ChunkError::Version(2)));
    for bad in [
        ChunkHeader {
            incarnation: "not-a-ulid".into(),
            ..h.clone()
        },
        ChunkHeader {
            incarnation: INC_A.to_lowercase(),
            ..h.clone()
        },
        ChunkHeader {
            created_at: "yesterday".into(),
            ..h.clone()
        },
        ChunkHeader {
            engine: String::new(),
            ..h.clone()
        },
        ChunkHeader {
            engine: "e".repeat(33),
            ..h.clone()
        },
        ChunkHeader {
            protocol: "p".repeat(33),
            ..h.clone()
        },
        ChunkHeader {
            first_rec: 5,
            last_rec: 4,
            ..h.clone()
        },
    ] {
        assert_eq!(
            decode_chunk(&good(&bad), 5),
            Err(ChunkError::BadHeader),
            "{bad:?}"
        );
    }
    // A header that is not JSON, and one that lacks members.
    assert_eq!(
        decode_chunk(&object("{not json}\n"), 5),
        Err(ChunkError::BadHeader)
    );
    assert_eq!(
        decode_chunk(&object("{\"v\":1,\"seq\":5}\n"), 5),
        Err(ChunkError::BadHeader)
    );
}

#[test]
fn record_numbers_must_increase_inside_the_headers_range() {
    let h = header(0, INC_A, 3, 5);
    let line = |n: u64| json!({"n": n, "dir": "in", "kind": "notice", "data": {}}).to_string();
    let ok = object(&format!(
        "{}\n{}\n{}\n{}\n",
        head_line(&h),
        line(3),
        line(4),
        line(5)
    ));
    assert_eq!(decode_chunk(&ok, 0).unwrap().records.len(), 3);
    let skip_ok = object(&format!("{}\n{}\n{}\n", head_line(&h), line(3), line(5)));
    assert_eq!(
        decode_chunk(&skip_ok, 0).unwrap().records.len(),
        2,
        "numbers the writer shed leave holes, which is allowed"
    );
    for (body, at) in [
        (format!("{}\n{}\n{}\n", head_line(&h), line(4), line(4)), 3), // repeated
        (format!("{}\n{}\n{}\n", head_line(&h), line(5), line(4)), 3), // decreasing
        (format!("{}\n{}\n", head_line(&h), line(2)), 2),              // below the range
        (format!("{}\n{}\n", head_line(&h), line(6)), 2),              // above the range
    ] {
        assert_eq!(
            decode_chunk(&object(&body), 0),
            Err(ChunkError::RecordOrder { line: at }),
            "{body}"
        );
    }
}

#[test]
fn a_line_that_is_not_a_record_makes_the_whole_chunk_unreadable() {
    let h = head_line(&header(0, INC_A, 0, 3));
    let rec_line = json!({"n": 0, "dir": "in", "kind": "notice", "data": {}}).to_string();
    for (body, line) in [
        (format!("{h}\n{rec_line}\n{{broken\n"), 3),
        (format!("{h}\n{rec_line}\n\n{rec_line}\n"), 3), // a blank line in the middle
        (format!("{h}\n[1,2]\n"), 2),
        (
            format!("{h}\n{{\"n\":-1,\"dir\":\"in\",\"kind\":\"x\",\"data\":null}}\n"),
            2,
        ),
        (
            format!("{h}\n{{\"n\":0,\"dir\":\"in\",\"data\":null}}\n"),
            2,
        ), // no kind
        (
            format!("{h}\n{{\"n\":0,\"dir\":\"in\",\"kind\":7,\"data\":null}}\n"),
            2,
        ),
    ] {
        assert_eq!(
            decode_chunk(&object(&body), 0),
            Err(ChunkError::BadJson { line }),
            "{body}"
        );
    }
    // Text that is not UTF-8.
    let mut bytes = format!("{h}\n").into_bytes();
    bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
    assert_eq!(
        decode_chunk(&gzip_stored(&bytes), 0),
        Err(ChunkError::NotUtf8)
    );
}

#[test]
fn nesting_a_record_may_not_exceed_the_depth_limit() {
    let h = head_line(&header(0, INC_A, 0, 0));
    let nest = |depth: usize| {
        let mut v = json!(1);
        for _ in 0..depth {
            v = json!({ "a": v });
        }
        v
    };
    let line =
        |depth: usize| json!({"n": 0, "dir": "in", "kind": "x", "data": nest(depth)}).to_string();
    assert!(decode_chunk(&object(&format!("{h}\n{}\n", line(MAX_RECORD_DEPTH))), 0).is_ok());
    assert_eq!(
        decode_chunk(
            &object(&format!("{h}\n{}\n", line(MAX_RECORD_DEPTH + 1))),
            0
        ),
        Err(ChunkError::Depth { line: 2 })
    );
    // Far deeper than the JSON parser's own limit is an error too, not a crash.
    let deep = format!(
        "{h}\n{}{}{}\n",
        "{\"n\":0,\"dir\":\"in\",\"kind\":\"x\",\"data\":",
        "[".repeat(5000) + &"]".repeat(5000),
        "}"
    );
    assert!(matches!(
        decode_chunk(&object(&deep), 0),
        Err(ChunkError::BadJson { line: 2 })
    ));
}

#[test]
fn a_chunk_may_not_hold_more_than_the_record_limit() {
    let n = MAX_RECORDS_PER_CHUNK as u64;
    let h = head_line(&header(0, INC_A, 0, n));
    let mut body = format!("{h}\n");
    for i in 0..n {
        body.push_str(&format!(
            "{{\"n\":{i},\"dir\":\"in\",\"kind\":\"x\",\"data\":null}}\n"
        ));
    }
    assert_eq!(
        decode_chunk(&object(&body), 0).unwrap().records.len(),
        MAX_RECORDS_PER_CHUNK
    );
    body.push_str(&format!(
        "{{\"n\":{n},\"dir\":\"in\",\"kind\":\"x\",\"data\":null}}\n"
    ));
    assert_eq!(
        decode_chunk(&object(&body), 0),
        Err(ChunkError::TooManyRecords)
    );
}

#[test]
fn the_object_and_its_expansion_are_bounded() {
    // An object over 1 MiB is refused unread.
    assert_eq!(
        decode_chunk(&vec![0u8; (1 << 20) + 1], 0),
        Err(ChunkError::TooLarge)
    );
    // A stream that expands past 4 MiB is refused as it grows.
    let bomb = ham_core::gzip::gzip_run_of(b' ', 6 * 1024 * 1024);
    assert!(bomb.len() < 100 * 1024);
    assert_eq!(decode_chunk(&bomb, 0), Err(ChunkError::TooLarge));
}

#[test]
fn gzip_faults_are_named_without_the_chunks_own_text() {
    let good = chunk_gzip_stored(&chunk(
        0,
        INC_A,
        vec![rec(0, "notice", json!({"text": "SECRET-IN-THE-CHUNK"}))],
    ));
    let mut crc = good.clone();
    let n = crc.len();
    crc[n - 8] ^= 1;
    for bad in [
        crc,
        good[..good.len() / 2].to_vec(),
        b"plain text".to_vec(),
        Vec::new(),
    ] {
        let e = decode_chunk(&bad, 0).unwrap_err();
        assert_eq!(e.code(), GapCode::BadGzip, "{e}");
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }
}

#[test]
fn every_error_has_a_gap_code_and_a_kind() {
    let cases: Vec<(ChunkError, GapCode, GapKind)> = vec![
        (
            ChunkError::Gzip(ham_core::gzip::GzipError::BadCrc),
            GapCode::BadGzip,
            GapKind::Unreadable,
        ),
        (ChunkError::TooLarge, GapCode::TooLarge, GapKind::Unreadable),
        (ChunkError::NotUtf8, GapCode::BadJson, GapKind::Unreadable),
        (ChunkError::NoHeader, GapCode::BadJson, GapKind::Unreadable),
        (
            ChunkError::BadJson { line: 3 },
            GapCode::BadJson,
            GapKind::Unreadable,
        ),
        (
            ChunkError::BadHeader,
            GapCode::BadHeader,
            GapKind::Unreadable,
        ),
        (
            ChunkError::Version(9),
            GapCode::Version,
            GapKind::Unreadable,
        ),
        (
            ChunkError::SeqMismatch { key: 1, header: 2 },
            GapCode::SeqMismatch,
            GapKind::Unreadable,
        ),
        (
            ChunkError::TooManyRecords,
            GapCode::TooManyRecords,
            GapKind::Unreadable,
        ),
        (
            ChunkError::RecordOrder { line: 4 },
            GapCode::RecordOrder,
            GapKind::Unreadable,
        ),
        (
            ChunkError::Depth { line: 5 },
            GapCode::Depth,
            GapKind::Unreadable,
        ),
    ];
    for (e, code, kind) in cases {
        assert_eq!(e.code(), code, "{e}");
        assert_eq!(e.code().kind(), kind, "{e}");
    }
    assert_eq!(GapCode::MissingChunk.kind(), GapKind::Missing);
    assert_eq!(GapCode::TailMissing.kind(), GapKind::Missing);
    assert_eq!(GapCode::ListLimit.kind(), GapKind::Truncated);
    assert_eq!(GapCode::Interleaved.kind(), GapKind::Conflict);
    assert_eq!(GapCode::ChangedWhileReading.kind(), GapKind::Conflict);
}

// ---- the Python-made chunk files -------------------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct Expect {
    ok: bool,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    incarnation: Option<String>,
    #[serde(default)]
    records: Option<usize>,
    #[serde(default)]
    first: Option<u64>,
    #[serde(default)]
    last: Option<u64>,
}

fn index() -> BTreeMap<String, BTreeMap<String, Expect>> {
    serde_json::from_slice(&std::fs::read(fixtures().join("index.json")).unwrap()).unwrap()
}

#[test]
fn every_chunk_file_reads_as_the_generator_says_it_must() {
    let index = index();
    assert!(index.len() >= 8, "{} scenarios", index.len());
    let mut files = 0;
    for (scenario, chunks) in &index {
        for (name, want) in chunks {
            let seq = seq_of_name(name).unwrap();
            let bytes = fixture_bytes(&fixtures().join(scenario).join(name));
            let got = decode_chunk(&bytes, seq);
            files += 1;
            if want.ok {
                let c = got.unwrap_or_else(|e| panic!("{scenario}/{name}: {e}"));
                assert_eq!(
                    Some(c.header.incarnation.as_str()),
                    want.incarnation.as_deref(),
                    "{scenario}/{name}"
                );
                assert_eq!(Some(c.records.len()), want.records, "{scenario}/{name}");
                assert_eq!(
                    c.records.first().map(|r| r.n),
                    want.first,
                    "{scenario}/{name}"
                );
                assert_eq!(
                    c.records.last().map(|r| r.n),
                    want.last,
                    "{scenario}/{name}"
                );
            } else {
                let e = got.expect_err(&format!("{scenario}/{name} must be unreadable"));
                let code = serde_json::to_value(e.code()).unwrap();
                assert_eq!(
                    code.as_str(),
                    want.code.as_deref(),
                    "{scenario}/{name}: {e}"
                );
            }
        }
    }
    assert!(files >= 24, "{files} files");
}

#[test]
fn the_forged_chunk_keeps_its_claims_out_of_the_records() {
    let bytes = fixture_bytes(&fixtures().join("forged/000000000000.ndjson.gz"));
    let c = decode_chunk(&bytes, 0).unwrap();
    assert_eq!(c.records.len(), 4);
    for r in &c.records {
        let text = serde_json::to_string(r).unwrap();
        assert!(!text.contains("\"author\""), "{text}");
        assert!(!text.contains("\"origin\""), "{text}");
        assert!(!text.contains("\"role\""), "{text}");
        let projected = TranscriptRecord::project(&c.header, r.clone(), &redact_tokens);
        assert_eq!(projected.origin, Origin::Engine);
    }
    // The ACP user_message_chunk is an engine record too.
    assert_eq!(c.records[2].kind, "user_message_chunk");
    assert_eq!(
        TranscriptRecord::project(&c.header, c.records[2].clone(), &redact_tokens).origin,
        Origin::Engine
    );
}

// ---- projection ------------------------------------------------------------------------------------------------------

#[test]
fn projection_is_engine_origin_with_a_clean_kind_no_odd_time_and_redacted_text() {
    let h = header(7, INC_B, 0, 9);
    // A token-shaped fake built at run time so no secret value sits in the source.
    let fake_token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    let sig = "0123456789abcdef".repeat(2);
    let url = format!(
        "https://bucket.s3.amazonaws.com/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature={sig}"
    );
    let r = ChunkRecord {
        n: 4,
        t: Some("2026-10-16T10:00:00.000Z".to_string() + &"z".repeat(100)),
        dir: Dir::Out,
        kind: "tool_call".into(),
        data: json!({"text": format!("token {fake_token} and {url}"), "nested": [{"deep": format!("Bearer {}", "a1".repeat(15))}], format!("key {fake_token}"): 1}),
    };
    let p = TranscriptRecord::project(&h, r, &redact_tokens);
    assert_eq!(
        (p.seq, p.n, p.incarnation.as_str(), p.origin, p.dir),
        (7, 4, INC_B, Origin::Engine, Dir::Out)
    );
    assert_eq!(p.t, None, "a time that is not a UTC timestamp is not sent");
    let text = serde_json::to_string(&p).unwrap();
    assert!(!text.contains(&fake_token), "{text}");
    assert!(!text.contains(&sig), "{text}");
    assert!(!text.contains(&"a1".repeat(8)), "{text}");
    assert_eq!(p.id(), (INC_B, 4));
    for (kind, want) in [
        ("agent_message_chunk", "agent_message_chunk"),
        ("a b", "unknown"),
        ("", "unknown"),
        ("x\u{1b}[31m", "unknown"),
        (&"k".repeat(65), "unknown"),
        ("tool_call_update", "tool_call_update"),
        ("a.b:c-d", "a.b:c-d"),
    ] {
        let r = ChunkRecord {
            n: 0,
            t: None,
            dir: Dir::In,
            kind: kind.into(),
            data: Value::Null,
        };
        assert_eq!(
            TranscriptRecord::project(&h, r, &redact_tokens).kind,
            want,
            "{kind:?}"
        );
    }
}

#[test]
fn the_omission_record_keeps_the_place_and_says_why() {
    let h = header(2, INC_A, 0, 0);
    let big = TranscriptRecord::project(
        &h,
        rec(0, "tool_call", json!({"text": "x"})),
        &redact_tokens,
    );
    let o = big.omitted(300_000);
    assert_eq!(
        (
            o.seq,
            o.n,
            o.incarnation.as_str(),
            o.origin,
            o.kind.as_str()
        ),
        (2, 0, INC_A, Origin::Engine, "omitted")
    );
    assert_eq!(
        o.data,
        json!({"reason": "oversized", "kind": "tool_call", "bytes": 300_000})
    );
}

// ---- the wire --------------------------------------------------------------------------------------------------------

#[test]
fn a_page_serializes_with_the_documented_member_names() {
    let page = TranscriptPage {
        run_id: RUN.into(),
        state: TranscriptState::Live,
        records: vec![TranscriptRecord {
            seq: 1,
            n: 2,
            incarnation: INC_A.into(),
            origin: Origin::Engine,
            t: Some("2026-10-16T10:00:00.000Z".into()),
            dir: Dir::In,
            kind: "agent_message_chunk".into(),
            data: json!({"content": {"type": "text", "text": "hi"}}),
        }],
        gaps: vec![
            TranscriptGap::new(GapCode::MissingChunk, 3, 4),
            TranscriptGap::new(GapCode::Interleaved, 6, 6),
        ],
        completeness: vec![CompletenessEntry {
            kind: GapKind::TailUnknown,
            from_seq: None,
            to_seq: None,
            count: None,
        }],
        published_through: Some(2),
        listed_through: Some(9),
        next: Some("abc_-123".into()),
        prev: Some(1),
        more: true,
        skipped_keys: 2,
    };
    let v = serde_json::to_value(&page).unwrap();
    assert_eq!(
        v,
        json!({
            "runId": RUN,
            "state": "live",
            "records": [{"seq": 1, "n": 2, "incarnation": INC_A, "origin": "engine", "t": "2026-10-16T10:00:00.000Z", "dir": "in",
                         "kind": "agent_message_chunk", "data": {"content": {"type": "text", "text": "hi"}}}],
            "gaps": [{"kind": "missing", "code": "missing-chunk", "fromSeq": 3, "toSeq": 4},
                     {"kind": "conflict", "code": "interleaved", "fromSeq": 6, "toSeq": 6}],
            "completeness": [{"kind": "tail-unknown"}],
            "publishedThrough": 2,
            "listedThrough": 9,
            "next": "abc_-123",
            "prev": 1,
            "more": true,
            "skippedKeys": 2
        })
    );
    assert_eq!(
        parse_page(&serde_json::to_vec(&page).unwrap()).unwrap(),
        page
    );
}

#[test]
fn a_page_from_a_newer_server_still_parses_and_a_bad_one_does_not() {
    let body = json!({"runId": RUN, "state": "unsupported", "somethingNew": {"a": 1}, "records": [], "gaps": [], "extra": true});
    let p = parse_page(body.to_string().as_bytes()).unwrap();
    assert_eq!(
        (p.state, p.more, p.records.len(), p.skipped_keys),
        (TranscriptState::Unsupported, false, 0, 0)
    );
    for bad in [
        "",
        "{}",
        "[]",
        "{\"runId\":\"x\",\"state\":\"sideways\"}",
        "not json",
    ] {
        assert!(parse_page(bad.as_bytes()).is_err(), "{bad}");
    }
    // Every state word.
    for (word, state) in [
        ("unsupported", TranscriptState::Unsupported),
        ("empty", TranscriptState::Empty),
        ("live", TranscriptState::Live),
        ("incomplete", TranscriptState::Incomplete),
        ("complete", TranscriptState::Complete),
        ("purged", TranscriptState::Purged),
    ] {
        assert_eq!(
            parse_page(json!({"runId": RUN, "state": word}).to_string().as_bytes())
                .unwrap()
                .state,
            state
        );
    }
}

#[test]
fn the_query_builds_the_path_the_route_reads() {
    let q = TranscriptQuery::default();
    assert_eq!(q.path(RUN), format!("/v1/runs/{RUN}/transcript"));
    let q = TranscriptQuery {
        after: Some("eyJ2IjoxfQ".into()),
        before: None,
        limit: Some(16),
    };
    assert_eq!(
        q.path(RUN),
        format!("/v1/runs/{RUN}/transcript?after=eyJ2IjoxfQ&limit=16")
    );
    let q = TranscriptQuery {
        after: None,
        before: Some(Before::End),
        limit: None,
    };
    assert_eq!(q.path(RUN), format!("/v1/runs/{RUN}/transcript?before=end"));
    let q = TranscriptQuery {
        after: None,
        before: Some(Before::Seq(41)),
        limit: Some(64),
    };
    assert_eq!(
        q.path(RUN),
        format!("/v1/runs/{RUN}/transcript?before=41&limit=64")
    );
    // A value is encoded, so a cursor can never add a member of its own.
    let q = TranscriptQuery {
        after: Some("a&before=end b".into()),
        before: None,
        limit: None,
    };
    assert_eq!(
        q.path(RUN),
        format!("/v1/runs/{RUN}/transcript?after=a%26before%3Dend%20b")
    );
}

#[test]
fn gap_kinds_map_to_the_clients_words() {
    for (kind, cause) in [
        (GapKind::Excluded, GapCause::Excluded),
        (GapKind::Truncated, GapCause::Truncated),
        (GapKind::Shed, GapCause::Shed),
        (GapKind::Suppressed, GapCause::Suppressed),
        (GapKind::Failed, GapCause::Failed),
        (GapKind::Conflict, GapCause::Conflict),
        (GapKind::Unsupported, GapCause::Unsupported),
        (GapKind::TailUnknown, GapCause::TailUnknown),
        (GapKind::Unreadable, GapCause::Unreadable),
        (GapKind::Missing, GapCause::Failed),
    ] {
        assert_eq!(kind.cause(), cause);
    }
    for (kind, word) in [
        (GapKind::TailUnknown, "tail-unknown"),
        (GapKind::Missing, "missing"),
        (GapKind::Excluded, "excluded"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(word));
    }
    assert_eq!(
        ham_core::chat::Origin::from(Origin::User),
        ham_core::chat::Origin::User
    );
    assert_eq!(
        ham_core::chat::Origin::from(Origin::Harness),
        ham_core::chat::Origin::Harness
    );
    assert_eq!(
        ham_core::chat::Origin::from(Origin::Engine),
        ham_core::chat::Origin::Engine
    );
}

#[test]
fn a_summary_is_lossless_only_when_it_is_valid_and_declares_nothing_but_exclusions() {
    let entry = |kind, a: Option<u64>, b: Option<u64>| CompletenessEntry {
        kind,
        from_seq: a,
        to_seq: b,
        count: None,
    };
    let none = TranscriptSummary::default();
    assert!(none.validate() && none.is_lossless());
    let excluded = TranscriptSummary {
        last_transcript_seq: Some(9),
        entries: vec![entry(GapKind::Excluded, Some(0), Some(9))],
    };
    assert!(excluded.is_lossless());
    for kind in [
        GapKind::Truncated,
        GapKind::Shed,
        GapKind::Suppressed,
        GapKind::Failed,
        GapKind::Conflict,
        GapKind::Unsupported,
        GapKind::TailUnknown,
    ] {
        let s = TranscriptSummary {
            last_transcript_seq: Some(1),
            entries: vec![entry(kind, None, None)],
        };
        assert!(s.validate() && !s.is_lossless(), "{kind:?}");
    }
    // The reader's own words are not a writer's to declare; ranges must be ordered and whole; the list is bounded.
    for bad in [
        TranscriptSummary {
            last_transcript_seq: None,
            entries: vec![entry(GapKind::Unreadable, None, None)],
        },
        TranscriptSummary {
            last_transcript_seq: None,
            entries: vec![entry(GapKind::Missing, None, None)],
        },
        TranscriptSummary {
            last_transcript_seq: None,
            entries: vec![entry(GapKind::Shed, Some(5), Some(4))],
        },
        TranscriptSummary {
            last_transcript_seq: None,
            entries: vec![entry(GapKind::Shed, Some(5), None)],
        },
        TranscriptSummary {
            last_transcript_seq: None,
            entries: (0..257)
                .map(|_| entry(GapKind::Excluded, None, None))
                .collect(),
        },
    ] {
        assert!(!bad.validate() && !bad.is_lossless(), "{bad:?}");
    }
    let ok256 = TranscriptSummary {
        last_transcript_seq: None,
        entries: (0..256)
            .map(|_| entry(GapKind::Excluded, None, None))
            .collect(),
    };
    assert!(ok256.validate());
    // As stored: camelCase members.
    let v = serde_json::to_value(&excluded).unwrap();
    assert_eq!(
        v,
        json!({"lastTranscriptSeq": 9, "entries": [{"kind": "excluded", "fromSeq": 0, "toSeq": 9}]})
    );
}

// ---- findings of the implementation review (2026-10-16) --------------------------------------------------------------

#[test]
fn a_time_that_is_not_a_utc_timestamp_and_a_kind_that_redacts_differently_are_not_sent() {
    let h = header(7, INC_B, 0, 9);
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    for t in [
        token.clone(),
        "yesterday".to_string(),
        "2026-10-16 10:00:00".to_string(),
        "x".repeat(100),
    ] {
        let r = ChunkRecord {
            n: 0,
            t: Some(t),
            dir: Dir::In,
            kind: "notice".into(),
            data: Value::Null,
        };
        assert_eq!(TranscriptRecord::project(&h, r, &redact_tokens).t, None);
    }
    let good = ChunkRecord {
        n: 0,
        t: Some("2026-10-16T10:00:00.123Z".into()),
        dir: Dir::In,
        kind: "notice".into(),
        data: Value::Null,
    };
    assert_eq!(
        TranscriptRecord::project(&h, good, &redact_tokens)
            .t
            .as_deref(),
        Some("2026-10-16T10:00:00.123Z")
    );
    let r = ChunkRecord {
        n: 0,
        t: None,
        dir: Dir::In,
        kind: token.clone(),
        data: Value::Null,
    };
    let p = TranscriptRecord::project(&h, r, &redact_tokens);
    assert_eq!(p.kind, "unknown");
    assert!(
        !serde_json::to_string(&p.omitted(300_000))
            .unwrap()
            .contains(&token)
    );
}

#[test]
fn the_reader_has_two_more_gap_words_and_the_first_chunk_is_numbered_zero() {
    assert_eq!(ham_core::transcript::FIRST_SEQ, 0);
    for (code, word, kind) in [
        (GapCode::RecordHole, "record-hole", GapKind::Missing),
        (
            GapCode::DuplicateRecord,
            "duplicate-record",
            GapKind::Conflict,
        ),
    ] {
        assert_eq!(serde_json::to_value(code).unwrap(), json!(word));
        assert_eq!(code.kind(), kind);
    }
}

#[test]
fn a_hole_in_the_record_numbers_is_found_unless_a_gap_record_explains_it() {
    use ham_core::transcript::record_hole_before;
    let r = |n: u64, kind: &str| ChunkRecord {
        n,
        t: None,
        dir: Dir::In,
        kind: kind.into(),
        data: Value::Null,
    };
    // Inside a chunk: 0, 1, 3 has a hole before 3.
    let c = chunk(
        0,
        INC_A,
        vec![r(0, "notice"), r(1, "notice"), r(3, "notice")],
    );
    assert_eq!(record_hole_before(&c, None), Some(3));
    // The writer says what it shed: a `gap` record explains the numbers up to it.
    let c = chunk(
        0,
        INC_A,
        vec![r(0, "notice"), r(1, "notice"), r(3, "gap"), r(4, "notice")],
    );
    assert_eq!(record_hole_before(&c, None), None);
    let c = chunk(0, INC_A, vec![r(0, "notice"), r(3, "gap")]);
    assert_eq!(
        record_hole_before(&c, None),
        None,
        "the gap record itself may follow a shed range"
    );
    // Across chunks of one writer: the next chunk must start at the number after the last one seen.
    let c = chunk(1, INC_A, vec![r(5, "notice"), r(6, "notice")]);
    assert_eq!(record_hole_before(&c, Some((INC_A.to_string(), 4))), None);
    assert_eq!(
        record_hole_before(&c, Some((INC_A.to_string(), 2))),
        Some(5)
    );
    // A writer that continues under a new incarnation restarts at zero: nothing is missing.
    let c = chunk(2, INC_B, vec![r(0, "notice"), r(1, "notice")]);
    assert_eq!(record_hole_before(&c, Some((INC_A.to_string(), 99))), None);
    let c = chunk(2, INC_B, vec![r(1, "notice")]);
    assert_eq!(
        record_hole_before(&c, Some((INC_A.to_string(), 99))),
        Some(1),
        "a new writer starts at 0"
    );
    // The first chunk of a writer starts at 0; with no earlier chunk in view only a hole inside the chunk counts.
    let c = chunk(0, INC_A, vec![r(4, "notice"), r(5, "notice")]);
    assert_eq!(record_hole_before(&c, None), None);
    // A header that claims records the chunk does not hold.
    let mut c = chunk(0, INC_A, vec![r(0, "notice"), r(1, "notice")]);
    c.header.last_rec = 5;
    assert_eq!(record_hole_before(&c, None), Some(2));
    let empty = Chunk {
        header: header(0, INC_A, 0, 0),
        records: vec![],
    };
    assert_eq!(record_hole_before(&empty, None), Some(0));
}

#[test]
fn a_gap_record_just_before_the_position_explains_the_skip_after_it() {
    use ham_core::transcript::record_hole_in;
    let h = header(0, INC_A, 0, 7);
    let after = vec![rec(6, "notice", json!({})), rec(7, "notice", json!({}))];
    // The record the read resumed after was a gap record: numbers 3 to 5 were shed and said so.
    assert_eq!(record_hole_in(&h, &after, Some(3), true), None);
    assert_eq!(record_hole_in(&h, &after, Some(3), false), Some(6));
    // The numbers that follow must still run on.
    let skipping = vec![rec(6, "notice", json!({})), rec(8, "notice", json!({}))];
    let h = header(0, INC_A, 0, 8);
    assert_eq!(record_hole_in(&h, &skipping, Some(3), true), Some(8));
}

#[test]
fn the_edges_of_neighbouring_chunks_show_a_skip_between_them() {
    use ham_core::transcript::{boundary_hole, chunk_edge};
    let c = |seq: u64, inc: &str, nums: &[(u64, &str)]| Chunk {
        header: header(seq, inc, nums[0].0, nums[nums.len() - 1].0),
        records: nums.iter().map(|(n, k)| rec(*n, k, json!({}))).collect(),
    };
    let empty = Chunk {
        header: header(5, INC_A, 0, 0),
        records: vec![],
    };
    assert_eq!(chunk_edge(&empty), None);
    let a = chunk_edge(&c(0, INC_A, &[(0, "notice"), (1, "notice"), (2, "gap")])).unwrap();
    assert_eq!(
        (
            a.seq,
            a.incarnation.as_str(),
            a.first_n,
            a.last_n,
            a.first_is_gap,
            a.last_is_gap
        ),
        (0, INC_A, 0, 2, false, true)
    );
    let next = |inc: &str, nums: &[(u64, &str)]| chunk_edge(&c(1, inc, nums)).unwrap();
    let plain = chunk_edge(&c(0, INC_A, &[(0, "notice"), (1, "notice")])).unwrap();
    // The same writer goes on.
    assert!(!boundary_hole(&plain, &next(INC_A, &[(2, "notice")])));
    assert!(boundary_hole(&plain, &next(INC_A, &[(4, "notice")])));
    // A gap record explains it, whether it ends the first chunk or starts the next.
    assert!(!boundary_hole(
        &plain,
        &next(INC_A, &[(4, "gap"), (5, "notice")])
    ));
    assert!(!boundary_hole(&a, &next(INC_A, &[(5, "notice")])));
    // A new writer starts at 0.
    assert!(!boundary_hole(&plain, &next(INC_B, &[(0, "notice")])));
    assert!(boundary_hole(&plain, &next(INC_B, &[(3, "notice")])));
    assert!(!boundary_hole(&plain, &next(INC_B, &[(3, "gap")])));
}

/// The reader and agentd's writer share one set of words: the feature, the cap on a summary, the file-name rule and the object keys
/// have one definition in `harness_protocol`, and the reader re-exports them. A divergence of the two ends would fail here.
#[test]
fn the_reader_uses_the_writers_constants_and_key_helpers() {
    use harness_protocol::direct::s3;
    use harness_protocol::transcript as wire;
    assert_eq!(
        ham_core::transcript::TRANSCRIPT_FEATURE,
        wire::TRANSCRIPT_FEATURE
    );
    assert_eq!(
        ham_core::transcript::MAX_COMPLETENESS_ENTRIES,
        wire::MAX_COMPLETENESS_ENTRIES
    );
    assert_eq!(transcript_prefix(RUN), s3::transcript_prefix(RUN));
    for seq in [0u64, 7, 123_456_789_012] {
        assert_eq!(chunk_key(RUN, seq), s3::transcript_chunk_key(RUN, seq));
    }
    for name in [
        "000000000007.ndjson.gz",
        "7.ndjson.gz",
        "000000000007.ndjson",
        "00000000000x.ndjson.gz",
        "",
    ] {
        assert_eq!(seq_of_name(name), wire::seq_of_name(name), "{name}");
    }
}
