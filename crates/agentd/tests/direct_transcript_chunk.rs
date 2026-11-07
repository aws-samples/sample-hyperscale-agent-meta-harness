//! The chunk object of the stored conversation: a gzip stream whose first line is a header and whose other lines
//! are records. The reader of the read route trusts nothing in it, so the tests read what the writer made with the reader's own rules
//! (`read_like_the_route`, the rules of `ham_core::transcript::decode_chunk`: version, a header seq equal to the key's, a ULID
//! incarnation, a UTC time, short labels, strictly increasing record numbers inside the header's range, a bounded size and depth).
#![allow(clippy::unwrap_used)]

use agentd::direct::transcript::chunk::{
    ChunkError, Header, MAX_OBJECT_BYTES, decode_header, encode,
};
use agentd::direct::transcript::pipeline::{Line, gap_line};
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use std::io::Read;

const ULID: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

fn header(seq: u64, first: u64, last: u64) -> Header {
    Header {
        seq,
        incarnation: ULID.to_string(),
        first_rec: first,
        last_rec: last,
        created_at: "2026-10-16T20:00:00Z".to_string(),
        engine: "Engine".to_string(),
        protocol: "1.38.0".to_string(),
        closed: false,
    }
}

fn line(n: u64, kind: &str, data: Value) -> Line {
    let rec = json!({"n": n, "t": "2026-10-16T20:00:01Z", "dir": "in", "kind": kind, "data": data});
    Line {
        n,
        kind: kind.to_string(),
        bytes: serde_json::to_vec(&rec).unwrap(),
    }
}

fn gunzip(gz: &[u8], limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    GzDecoder::new(gz)
        .take(limit as u64)
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        Value::Object(m) => 1 + m.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.as_bytes()[0] <= b'7'
        && s.bytes()
            .all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c))
}

/// The reader's rules, in the order it applies them.
fn read_like_the_route(gz: &[u8], key_seq: u64) -> Result<(Value, Vec<Value>), String> {
    if gz.len() > 1 << 20 {
        return Err("object over 1 MiB".into());
    }
    let raw = gunzip(gz, 4 << 20);
    if raw.len() >= 4 << 20 {
        return Err("inflates past 4 MiB".into());
    }
    let text = String::from_utf8(raw).map_err(|_| "not UTF-8".to_string())?;
    let mut lines = text.split('\n');
    let head: Value =
        serde_json::from_str(lines.next().filter(|l| !l.is_empty()).ok_or("no header")?)
            .map_err(|e| e.to_string())?;
    if head["v"] != 1 {
        return Err("version".into());
    }
    if head["seq"].as_u64() != Some(key_seq) {
        return Err("seq differs from the key".into());
    }
    let ok_label = |k: &str| {
        head[k]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.chars().count() <= 32)
    };
    let (first, last) = (
        head["firstRec"].as_u64().ok_or("firstRec")?,
        head["lastRec"].as_u64().ok_or("lastRec")?,
    );
    let time = head["createdAt"].as_str().unwrap_or_default();
    if !head["incarnation"].as_str().is_some_and(is_ulid)
        || !time.ends_with('Z')
        || time.len() < 20
        || !ok_label("engine")
        || !ok_label("protocol")
        || first > last
    {
        return Err("bad header".into());
    }
    let mut body: Vec<&str> = lines.collect();
    if body.last() == Some(&"") {
        body.pop();
    }
    if body.len() > 10_000 {
        return Err("too many records".into());
    }
    let mut recs = Vec::new();
    let mut prev: Option<u64> = None;
    for (i, l) in body.iter().enumerate() {
        let rec: Value =
            serde_json::from_str(l).map_err(|_| format!("line {} is not a record", i + 2))?;
        let n = rec["n"].as_u64().ok_or("n")?;
        if depth(&rec["data"]) > 32 {
            return Err("too deep".into());
        }
        if n < first || n > last || prev.is_some_and(|p| n <= p) {
            return Err(format!("record numbers out of order at line {}", i + 2));
        }
        if !matches!(rec["dir"].as_str(), Some("in" | "out"))
            || !rec["kind"]
                .as_str()
                .is_some_and(|k| !k.is_empty() && k.chars().count() <= 64)
        {
            return Err("dir or kind".into());
        }
        prev = Some(n);
        recs.push(rec);
    }
    Ok((head, recs))
}

#[test]
fn a_chunk_is_a_gzip_stream_the_route_accepts() {
    let lines = vec![
        line(0, "prompt", json!({"id": 1})),
        line(
            1,
            "agent_message_chunk",
            json!({"content": {"type": "text", "text": "hi"}}),
        ),
        line(2, "prompt_result", json!({"id": 1})),
    ];
    let gz = encode(&header(7, 0, 2), &lines).unwrap();
    assert_eq!(&gz[..2], &[0x1f, 0x8b], "gzip magic");
    let (head, recs) = read_like_the_route(&gz, 7).unwrap();
    assert_eq!(
        head,
        json!({"v": 1, "seq": 7, "incarnation": ULID, "firstRec": 0, "lastRec": 2, "createdAt": "2026-10-16T20:00:00Z", "engine": "Engine", "protocol": "1.38.0"})
    );
    assert_eq!(
        recs.iter()
            .map(|r| r["n"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(recs[1]["data"]["content"]["text"], "hi");
}

#[test]
fn the_header_says_closed_only_when_it_is_the_last_object_of_a_clean_end() {
    let mut h = header(0, 0, 0);
    let l = vec![line(0, "prompt", json!({}))];
    let open = read_like_the_route(&encode(&h, &l).unwrap(), 0).unwrap().0;
    assert!(open.get("closed").is_none());
    h.closed = true;
    let closed = read_like_the_route(&encode(&h, &l).unwrap(), 0).unwrap().0;
    assert_eq!(closed["closed"], true);
}

#[test]
fn a_leading_gap_record_may_name_numbers_the_chunk_lost() {
    // records 3 to 5 never reached storage: the next chunk starts at 3 with the gap record, then goes on at 6
    let lines = vec![
        gap_line(
            3,
            1_790_000_000,
            json!({"kind": "failed", "count": 3, "reason": "storage"}),
        ),
        line(6, "prompt", json!({})),
        line(7, "prompt_result", json!({})),
    ];
    let gz = encode(&header(2, 3, 7), &lines).unwrap();
    let (head, recs) = read_like_the_route(&gz, 2).unwrap();
    assert_eq!(
        (head["firstRec"].as_u64(), head["lastRec"].as_u64()),
        (Some(3), Some(7))
    );
    assert_eq!(recs[0]["kind"], "gap");
    assert_eq!(recs[0]["data"]["kind"], "failed");
    assert_eq!(
        recs.iter()
            .map(|r| r["n"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [3, 6, 7]
    );
}

#[test]
fn a_chunk_with_the_wrong_key_or_range_is_what_the_route_refuses() {
    let gz = encode(&header(7, 0, 0), &[line(0, "prompt", json!({}))]).unwrap();
    assert!(
        read_like_the_route(&gz, 8).is_err(),
        "the reader checks the seq against the key"
    );
    // the encoder does not repair a header that disagrees with its records
    assert!(matches!(
        encode(&header(1, 0, 0), &[line(5, "prompt", json!({}))]),
        Err(ChunkError::RecordOutsideRange { .. })
    ));
    assert!(matches!(
        encode(
            &header(1, 0, 9),
            &[line(2, "prompt", json!({})), line(2, "prompt", json!({}))]
        ),
        Err(ChunkError::RecordOrder { .. })
    ));
    assert!(matches!(
        encode(&header(1, 4, 2), &[]),
        Err(ChunkError::BadHeader)
    ));
}

#[test]
fn the_encoder_refuses_a_body_the_reader_would_refuse() {
    let big = json!({"text": "x".repeat(300_000)});
    let lines: Vec<Line> = (0..4)
        .map(|i| line(i, "tool_call_update", big.clone()))
        .collect();
    const { assert!(MAX_OBJECT_BYTES <= 1 << 20) };
    let err = encode(&header(0, 0, 3), &lines).unwrap_err();
    assert!(matches!(err, ChunkError::TooLarge { .. }), "{err:?}");
    let few: Vec<Line> = (0..3)
        .map(|i| line(i, "tool_call_update", big.clone()))
        .collect();
    let gz = encode(&header(0, 0, 2), &few).unwrap();
    assert!(read_like_the_route(&gz, 0).is_ok());
}

#[test]
fn the_largest_chunk_the_settings_allow_is_under_the_readers_limits_even_when_it_does_not_compress()
{
    // 512 KiB of records plus one record at the 256 KiB cap: the most one chunk can hold
    let mut x: u64 = 0x2545F4914F6CDD1D;
    let mut noise = |n: usize| -> String {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(b'A' + (x % 52) as u8 % 26 + if x.is_multiple_of(2) { 32 } else { 0 })
            })
            .collect()
    };
    let mut lines = Vec::new();
    let mut total = 0usize;
    let mut n = 0u64;
    while total < 512 * 1024 {
        let l = line(n, "tool_call_update", json!({"rawOutput": noise(60_000)}));
        total += l.bytes.len();
        lines.push(l);
        n += 1;
    }
    lines.push(line(
        n,
        "tool_call_update",
        json!({"rawOutput": noise(250_000)}),
    ));
    let gz = encode(&header(0, 0, n), &lines).unwrap();
    assert!(gz.len() < 1 << 20, "{} bytes", gz.len());
    read_like_the_route(&gz, 0).unwrap();
}

#[test]
fn text_compresses_much_smaller() {
    let text = "the quick brown fox jumps over the lazy dog ".repeat(2000);
    let lines = vec![line(
        0,
        "agent_message_chunk",
        json!({"content": {"type": "text", "text": text.clone()}}),
    )];
    let gz = encode(&header(0, 0, 0), &lines).unwrap();
    assert!(gz.len() * 10 < text.len(), "{} of {}", gz.len(), text.len());
}

#[test]
fn the_header_is_read_back_from_a_prefix_of_the_object() {
    let lines: Vec<Line> = (0..2000)
        .map(|i| {
            line(
                i,
                "agent_message_chunk",
                json!({"text": format!("record number {i} {}", "pad ".repeat(20))}),
            )
        })
        .collect();
    let mut h = header(41, 0, 1999);
    h.closed = true;
    let gz = encode(&h, &lines).unwrap();
    assert!(gz.len() > 4096);
    for cut in [gz.len(), 8192, 4096, 1024] {
        let got = decode_header(&gz[..cut.min(gz.len())])
            .unwrap_or_else(|| panic!("a header from the first {cut} bytes"));
        assert_eq!(got, h, "first {cut} bytes");
    }
}

#[test]
fn what_is_not_a_header_reads_as_none() {
    let lines = vec![line(0, "prompt", json!({}))];
    let gz = encode(&header(3, 0, 0), &lines).unwrap();
    assert!(decode_header(&[]).is_none());
    assert!(decode_header(b"not gzip at all").is_none());
    assert!(decode_header(&gz[..9]).is_none(), "a header cut short");
    // a gzip stream whose first line is not a header
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, b"{\"hello\": 1}\n{\"n\": 0}\n").unwrap();
    assert!(decode_header(&enc.finish().unwrap()).is_none());
    // a header with a field of the wrong type
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, b"{\"v\":\"one\",\"seq\":1}\n").unwrap();
    assert!(decode_header(&enc.finish().unwrap()).is_none());
}

#[test]
fn a_first_line_longer_than_the_header_limit_is_not_a_header() {
    // one line of 1 MiB: the header is read from the first 8 KiB inflated, so this is not one, and the read stays short
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let chunk = vec![b'a'; 1 << 20];
    std::io::Write::write_all(&mut enc, &chunk).unwrap();
    let gz = enc.finish().unwrap();
    let start = std::time::Instant::now();
    assert!(decode_header(&gz).is_none());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn a_header_names_the_writer_and_stays_within_the_labels_the_reader_accepts() {
    let mut h = header(0, 0, 0);
    h.engine = "x".repeat(100);
    h.protocol = String::new();
    let gz = encode(&h, &[line(0, "prompt", json!({}))]).unwrap();
    let (head, _) = read_like_the_route(&gz, 0).unwrap();
    assert!(head["engine"].as_str().unwrap().chars().count() <= 32);
    assert!(!head["protocol"].as_str().unwrap().is_empty());
}
