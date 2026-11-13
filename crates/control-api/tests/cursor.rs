//! The opaque position token of the transcript route: bound to the run and the caller, bounded, versioned.
#![allow(clippy::unwrap_used)]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use control_api::cursor::{CursorError, MAX_KEY_CHARS, MAX_TOKEN_CHARS, Position, decode, encode};

const RUN: &str = "01K83D1111111111111111111A";
const OTHER_RUN: &str = "01K83D1111111111111111111B";
const ME: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const YOU: &str = "arn:aws:sts::800000000022:assumed-role/Admin/someone";

fn raw(json: &str) -> String {
    URL_SAFE_NO_PAD.encode(json.as_bytes())
}

#[test]
fn a_position_round_trips_and_the_token_is_url_safe_and_stable() {
    for pos in [
        Position::at(0, 0),
        Position::at(4821, 17),
        Position::at(u64::from(u32::MAX) * 4, u32::MAX),
        Position {
            seq: 9,
            idx: 0,
            after_key: Some("0000000-junk-0049".into()),
            etag: None,
        },
        Position {
            seq: 9,
            idx: 4,
            after_key: None,
            etag: Some("\"0123456789abcdef0123456789abcdef\"".into()),
        },
    ] {
        let t = encode(RUN, ME, &pos);
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{t}"
        );
        assert!(t.len() < MAX_TOKEN_CHARS);
        assert_eq!(decode(RUN, ME, &t).unwrap(), pos);
        assert_eq!(
            t,
            encode(RUN, ME, &pos),
            "the same position always encodes the same way"
        );
    }
    assert_ne!(
        encode(RUN, ME, &Position::at(1, 0)),
        encode(RUN, ME, &Position::at(1, 1))
    );
}

#[test]
fn a_token_is_bound_to_its_run_and_its_caller() {
    let t = encode(RUN, ME, &Position::at(3, 4));
    assert_eq!(decode(OTHER_RUN, ME, &t), Err(CursorError::WrongBinding));
    assert_eq!(decode(RUN, YOU, &t), Err(CursorError::WrongBinding));
    assert_eq!(decode(OTHER_RUN, YOU, &t), Err(CursorError::WrongBinding));
    // The binding does not reveal the run or the caller.
    let text = String::from_utf8(URL_SAFE_NO_PAD.decode(&t).unwrap()).unwrap();
    assert!(
        !text.contains(RUN) && !text.contains("testuser") && !text.contains("800000000022"),
        "{text}"
    );
}

#[test]
fn a_malformed_or_foreign_token_is_refused_with_its_reason() {
    let good = encode(RUN, ME, &Position::at(1, 2));
    assert_eq!(decode(RUN, ME, ""), Err(CursorError::Malformed));
    assert_eq!(
        decode(RUN, ME, "not base64 !!"),
        Err(CursorError::Malformed)
    );
    assert_eq!(
        decode(RUN, ME, &raw("not json")),
        Err(CursorError::Malformed)
    );
    assert_eq!(
        decode(RUN, ME, &raw("[1,2,3]")),
        Err(CursorError::Malformed)
    );
    assert_eq!(
        decode(RUN, ME, &raw("{\"v\":1}")),
        Err(CursorError::Malformed)
    );
    assert_eq!(
        decode(RUN, ME, &raw("{\"v\":1,\"b\":\"x\",\"s\":-1,\"i\":0}")),
        Err(CursorError::Malformed)
    );
    assert_eq!(
        decode(
            RUN,
            ME,
            &raw("{\"v\":1,\"b\":\"x\",\"s\":1,\"i\":4294967296}")
        ),
        Err(CursorError::Malformed)
    );
    // Another version, whatever else it holds.
    let v2 = String::from_utf8(URL_SAFE_NO_PAD.decode(&good).unwrap())
        .unwrap()
        .replace("\"v\":1", "\"v\":2");
    assert_eq!(decode(RUN, ME, &raw(&v2)), Err(CursorError::Version));
    // Padding is not part of the form.
    assert_eq!(
        decode(RUN, ME, &format!("{good}==")),
        Err(CursorError::Malformed)
    );
}

#[test]
fn a_token_is_bounded_and_so_is_the_key_it_carries() {
    assert_eq!(
        decode(RUN, ME, &"A".repeat(MAX_TOKEN_CHARS + 1)),
        Err(CursorError::TooLong)
    );
    // A key longer than the limit is not carried: the position survives, the key does not.
    let long = Position {
        seq: 5,
        idx: 0,
        after_key: Some("k".repeat(MAX_KEY_CHARS + 1)),
        etag: None,
    };
    assert_eq!(
        decode(RUN, ME, &encode(RUN, ME, &long)).unwrap(),
        Position::at(5, 0)
    );
    let edge = Position {
        seq: 5,
        idx: 0,
        after_key: Some("k".repeat(MAX_KEY_CHARS)),
        etag: None,
    };
    assert_eq!(decode(RUN, ME, &encode(RUN, ME, &edge)).unwrap(), edge);
    assert!(encode(RUN, ME, &edge).len() < MAX_TOKEN_CHARS);
    // A token that carries one anyway is refused.
    let b = String::from_utf8(
        URL_SAFE_NO_PAD
            .decode(encode(RUN, ME, &Position::at(5, 0)))
            .unwrap(),
    )
    .unwrap();
    let stuffed = b.replace(
        '}',
        &format!(",\"k\":\"{}\"}}", "k".repeat(MAX_KEY_CHARS + 1)),
    );
    assert_eq!(decode(RUN, ME, &raw(&stuffed)), Err(CursorError::Malformed));
}

#[test]
fn a_key_up_to_the_s3_limit_fits_a_token_and_only_a_plain_key_is_carried() {
    use control_api::cursor::safe_key;
    assert_eq!(MAX_KEY_CHARS, 1024, "the longest key S3 allows, in bytes");
    let longest = Position {
        seq: 1,
        idx: 0,
        after_key: Some("j".repeat(MAX_KEY_CHARS)),
        etag: None,
    };
    let t = encode(RUN, ME, &longest);
    assert!(t.len() <= MAX_TOKEN_CHARS, "{} characters", t.len());
    assert_eq!(decode(RUN, ME, &t).unwrap(), longest);
    // Bytes, not characters: 600 two-byte characters are 1,200 bytes and are not carried.
    let wide = Position {
        seq: 1,
        idx: 0,
        after_key: Some("é".repeat(600)),
        etag: None,
    };
    assert_eq!(
        decode(RUN, ME, &encode(RUN, ME, &wide)).unwrap().after_key,
        None
    );
    // A key that carries something a redactor would hide, or a control character, is never put into a token.
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    assert!(safe_key("0000000-junk-0049"));
    assert!(!safe_key(&format!("0000000-{token}-0001")));
    assert!(!safe_key("junk\u{1b}[31m"));
    assert!(!safe_key("junk\nline"));
    assert!(!safe_key(""));
    assert!(!safe_key(&"k".repeat(MAX_KEY_CHARS + 1)));
    assert_eq!(
        decode(
            RUN,
            ME,
            &encode(
                RUN,
                ME,
                &Position {
                    seq: 1,
                    idx: 0,
                    after_key: Some(format!("0000000-{token}")),
                    etag: None
                }
            )
        )
        .unwrap()
        .after_key,
        None
    );
}

#[test]
fn an_etag_binds_a_position_inside_a_chunk_and_is_bounded() {
    let etag = "\"0123456789abcdef0123456789abcdef\"".to_string();
    let p = Position {
        seq: 3,
        idx: 7,
        after_key: None,
        etag: Some(etag.clone()),
    };
    assert_eq!(
        decode(RUN, ME, &encode(RUN, ME, &p)).unwrap().etag,
        Some(etag)
    );
    // A long or odd ETag is not carried.
    for odd in [
        "x".repeat(65),
        "has space".to_string(),
        "new\nline".to_string(),
        String::new(),
    ] {
        let p = Position {
            seq: 3,
            idx: 7,
            after_key: None,
            etag: Some(odd),
        };
        assert_eq!(decode(RUN, ME, &encode(RUN, ME, &p)).unwrap().etag, None);
    }
}
