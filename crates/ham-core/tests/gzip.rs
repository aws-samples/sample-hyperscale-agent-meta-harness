//! The bounded gzip reader of the stored conversation. The streams in `tests/fixtures/gzip/` come from
//! Python's zlib (`gen.py`), so the decoder is checked against another implementation; the rest is built here from the pieces of
//! RFC 1951 and RFC 1952.
#![allow(clippy::unwrap_used)]

use ham_core::gzip::{GzipError, crc32, gunzip_bounded, gzip_run_of, gzip_stored, unhex};
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gzip")
}

fn sha(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(serde::Deserialize)]
struct Entry {
    sha256: String,
    len: usize,
}

fn index() -> BTreeMap<String, Entry> {
    serde_json::from_slice(&std::fs::read(dir().join("index.json")).unwrap()).unwrap()
}

fn fixture(name: &str) -> Vec<u8> {
    unhex(&std::fs::read_to_string(dir().join(format!("{name}.hex"))).unwrap()).unwrap()
}

const LIMIT: usize = 4 * 1024 * 1024;

#[test]
fn crc32_matches_the_known_check_value() {
    // The CRC-32 check value of "123456789" (RFC 1952 / zlib).
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
}

#[test]
fn every_python_stream_decodes_to_its_recorded_bytes() {
    let index = index();
    assert!(
        index.len() >= 20,
        "the fixture index lists {} streams",
        index.len()
    );
    for (name, want) in &index {
        if name.starts_with("bomb") {
            continue;
        }
        let got = gunzip_bounded(&fixture(name), LIMIT).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got.len(), want.len, "{name}: length");
        assert_eq!(sha(&got), want.sha256, "{name}: content");
    }
}

#[test]
fn the_fixtures_cover_stored_fixed_and_dynamic_blocks_and_every_header_field() {
    let index = index();
    for name in [
        "text-l0.gz",         // stored blocks
        "text-fixed.gz",      // fixed Huffman
        "text-l9.gz",         // dynamic Huffman
        "random-l6.gz",       // incompressible: stored
        "fname.gz",           // FNAME
        "fextra.gz",          // FEXTRA
        "fcomment.gz",        // FCOMMENT
        "fhcrc.gz",           // FHCRC with FNAME
        "all-flags.gz",       // all four
        "python-gzipfile.gz", // Python's own writer
        "text-300k.gz",       // a length that crosses the 64 KiB block sizes
    ] {
        assert!(index.contains_key(name), "{name} is missing from the index");
    }
}

#[test]
fn a_stream_that_expands_to_exactly_the_limit_is_accepted_and_one_byte_less_is_refused() {
    let want = &index()["zeros-100k.gz"];
    let gz = fixture("zeros-100k.gz");
    assert_eq!(gunzip_bounded(&gz, want.len).unwrap().len(), want.len);
    assert_eq!(
        gunzip_bounded(&gz, want.len - 1),
        Err(GzipError::OutputLimit)
    );
    assert_eq!(gunzip_bounded(&gz, 0), Err(GzipError::OutputLimit));
    // An empty stream needs no room.
    assert_eq!(
        gunzip_bounded(&fixture("empty.gz"), 0).unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn a_python_bomb_stops_at_the_limit_without_growing_past_it() {
    let gz = fixture("bomb-16mib.gz");
    assert!(gz.len() < 20_000, "the bomb is {} bytes", gz.len());
    for limit in [1usize, 1000, LIMIT] {
        assert_eq!(
            gunzip_bounded(&gz, limit),
            Err(GzipError::OutputLimit),
            "limit {limit}"
        );
    }
    // Given room it is a valid stream.
    assert_eq!(
        gunzip_bounded(&gz, 16 * 1024 * 1024).unwrap().len(),
        16 * 1024 * 1024
    );
}

#[test]
fn the_run_encoder_makes_a_hundred_to_one_bomb() {
    let gz = gzip_run_of(0, 50 * 1024 * 1024);
    assert!(gz.len() < 512 * 1024, "{} bytes for 50 MiB", gz.len());
    assert_eq!(gunzip_bounded(&gz, LIMIT), Err(GzipError::OutputLimit));
    let small = gzip_run_of(b'a', 1000);
    assert_eq!(gunzip_bounded(&small, 1000).unwrap(), vec![b'a'; 1000]);
    // Run lengths around the 258-byte match size and its remainders.
    for n in [
        1usize, 2, 3, 4, 257, 258, 259, 260, 261, 262, 515, 516, 517, 518, 519,
    ] {
        assert_eq!(
            gunzip_bounded(&gzip_run_of(7, n), 1 << 20).unwrap(),
            vec![7u8; n],
            "run of {n}"
        );
    }
}

#[test]
fn stored_streams_round_trip_across_the_block_sizes() {
    for n in [
        0usize, 1, 5, 255, 256, 65_534, 65_535, 65_536, 65_537, 131_070, 131_071, 200_000,
    ] {
        let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
        let gz = gzip_stored(&data);
        assert_eq!(gunzip_bounded(&gz, n).unwrap(), data, "{n} bytes");
    }
}

#[test]
fn every_prefix_of_a_stream_is_an_error() {
    let gz = fixture("fhcrc.gz");
    for cut in 0..gz.len() {
        assert!(
            gunzip_bounded(&gz[..cut], LIMIT).is_err(),
            "prefix of {cut} bytes was accepted"
        );
    }
    assert!(gunzip_bounded(&gz, LIMIT).is_ok());
    let gz = fixture("ndjson-l6.gz");
    for cut in (0..gz.len()).step_by(3) {
        assert!(
            gunzip_bounded(&gz[..cut], LIMIT).is_err(),
            "prefix of {cut} bytes was accepted"
        );
    }
}

#[test]
fn a_flipped_bit_is_an_error_or_the_same_bytes() {
    // The header's time, extra flags and OS bytes are not checked, so a flip there decodes to the same bytes; anything else must
    // fail (the CRC and the length are the net under the block syntax).
    let gz = fixture("one-byte.gz");
    let want = gunzip_bounded(&gz, LIMIT).unwrap();
    for byte in 0..gz.len() {
        for bit in 0..8 {
            let mut bad = gz.clone();
            bad[byte] ^= 1 << bit;
            if let Ok(got) = gunzip_bounded(&bad, LIMIT) {
                assert_eq!(
                    got, want,
                    "flip of bit {bit} of byte {byte} changed the output"
                );
            }
        }
    }
    let gz = fixture("ndjson-l6.gz");
    let want = gunzip_bounded(&gz, LIMIT).unwrap();
    for byte in (0..gz.len()).step_by(7) {
        let mut bad = gz.clone();
        bad[byte] ^= 0x10;
        if let Ok(got) = gunzip_bounded(&bad, LIMIT) {
            assert_eq!(got, want, "flip in byte {byte} changed the output");
        }
    }
}

#[test]
fn header_faults_have_their_own_errors() {
    let good = gzip_stored(b"hello");
    let mut bad = good.clone();
    bad[0] = 0x1e;
    assert_eq!(gunzip_bounded(&bad, 100), Err(GzipError::BadMagic));
    let mut bad = good.clone();
    bad[1] = 0x8c;
    assert_eq!(gunzip_bounded(&bad, 100), Err(GzipError::BadMagic));
    let mut bad = good.clone();
    bad[2] = 7;
    assert_eq!(gunzip_bounded(&bad, 100), Err(GzipError::UnsupportedMethod));
    for reserved in [0x20u8, 0x40, 0x80] {
        let mut bad = good.clone();
        bad[3] |= reserved;
        assert_eq!(
            gunzip_bounded(&bad, 100),
            Err(GzipError::ReservedFlags),
            "flag {reserved:#x}"
        );
    }
    assert_eq!(gunzip_bounded(&[], 100), Err(GzipError::Truncated));
    assert_eq!(gunzip_bounded(&good[..9], 100), Err(GzipError::Truncated));
}

#[test]
fn a_wrong_header_crc_is_refused() {
    let mut gz = fixture("fhcrc.gz");
    // Header: 10 fixed bytes, then the name "x\0", then the two-byte header CRC.
    assert_eq!(&gz[10..12], b"x\0");
    gz[12] ^= 0xff;
    assert_eq!(gunzip_bounded(&gz, LIMIT), Err(GzipError::BadHeaderCrc));
}

#[test]
fn an_extra_field_longer_than_the_input_is_truncated_not_a_panic() {
    let mut gz = gzip_stored(b"hello");
    gz[3] |= 4;
    let mut with_extra = gz[..10].to_vec();
    with_extra.extend_from_slice(&u16::MAX.to_le_bytes());
    with_extra.extend_from_slice(&gz[10..]);
    assert_eq!(gunzip_bounded(&with_extra, 100), Err(GzipError::Truncated));
    // An unterminated name runs off the end.
    let mut gz = gzip_stored(b"hello");
    gz[3] |= 8;
    assert!(gunzip_bounded(&gz[..12], 100).is_err());
}

#[test]
fn the_trailer_is_checked() {
    let good = gzip_stored(b"hello world");
    let n = good.len();
    let mut bad = good.clone();
    bad[n - 8] ^= 1; // CRC
    assert_eq!(gunzip_bounded(&bad, 100), Err(GzipError::BadCrc));
    let mut bad = good.clone();
    bad[n - 4] ^= 1; // ISIZE
    assert_eq!(gunzip_bounded(&bad, 100), Err(GzipError::BadSize));
    assert_eq!(
        gunzip_bounded(&good[..n - 1], 100),
        Err(GzipError::Truncated)
    );
}

#[test]
fn bytes_after_the_member_are_refused_including_a_second_member() {
    let one = gzip_stored(b"first");
    let mut two = one.clone();
    two.extend_from_slice(&gzip_stored(b"second"));
    assert_eq!(gunzip_bounded(&two, 100), Err(GzipError::TrailingData));
    let mut junk = one.clone();
    junk.push(0);
    assert_eq!(gunzip_bounded(&junk, 100), Err(GzipError::TrailingData));
    assert_eq!(gunzip_bounded(&one, 100).unwrap(), b"first");
}

/// A gzip member around a hand-made deflate body: `body` is the raw deflate bytes of `data`.
fn member(body: &[u8], data: &[u8]) -> Vec<u8> {
    let mut v = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    v.extend_from_slice(body);
    v.extend_from_slice(&crc32(data).to_le_bytes());
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v
}

#[test]
fn block_syntax_faults_are_named() {
    // BTYPE 3 is reserved.
    assert_eq!(
        gunzip_bounded(&member(&[0b0000_0111], b""), 10),
        Err(GzipError::BadBlockType)
    );
    // A stored block whose length is not the complement of its check.
    assert_eq!(
        gunzip_bounded(
            &member(
                &[0x01, 0x05, 0x00, 0x05, 0x00, b'h', b'e', b'l', b'l', b'o'],
                b"hello"
            ),
            10
        ),
        Err(GzipError::BadStoredLength)
    );
    // A stored block that claims more than the input holds.
    assert_eq!(
        gunzip_bounded(&member(&[0x01, 0xff, 0x00, 0x00, 0xff, b'h'], b"h"), 10),
        Err(GzipError::Truncated)
    );
    // A fixed block whose first symbol is a match with nothing behind it: literal/length 257 (7-bit code 0000001), distance 1.
    // Bits (LSB first): BFINAL 1, BTYPE 01, code 0000001 (MSB first), distance 00000.
    let body = [0b0000_0011, 0b0000_0010, 0b0000_0000];
    assert_eq!(
        gunzip_bounded(&member(&body, b""), 10),
        Err(GzipError::BadDistance)
    );
}

#[test]
fn a_dynamic_block_with_an_oversubscribed_or_missing_end_code_is_refused() {
    // BFINAL 1, BTYPE 10, HLIT 0 (257), HDIST 0 (1), HCLEN 0 (4 code-length codes: 16, 17, 18, 0), all four set to length 1:
    // four codes of length 1 are over-subscribed.
    let mut bits: Vec<u8> = vec![1, 0, 1];
    bits.extend([0; 5]); // HLIT
    bits.extend([0; 5]); // HDIST
    bits.extend([0; 4]); // HCLEN
    for _ in 0..4 {
        bits.extend([1, 0, 0]); // length 1, three bits LSB first
    }
    let mut body = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        body[i / 8] |= b << (i % 8);
    }
    let r = gunzip_bounded(&member(&body, b""), 10);
    assert!(
        matches!(r, Err(GzipError::BadCode)),
        "an over-subscribed code is {r:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic_and_never_exceed_the_limit(data in proptest::collection::vec(any::<u8>(), 0..600), limit in 0usize..5000) {
        if let Ok(out) = gunzip_bounded(&data, limit) {
            prop_assert!(out.len() <= limit);
        }
    }

    #[test]
    fn a_gzip_header_followed_by_arbitrary_bytes_never_panics(body in proptest::collection::vec(any::<u8>(), 0..400), limit in 0usize..70_000) {
        let mut v = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        v.extend_from_slice(&body);
        if let Ok(out) = gunzip_bounded(&v, limit) {
            prop_assert!(out.len() <= limit);
        }
    }

    #[test]
    fn stored_encoding_round_trips(data in proptest::collection::vec(any::<u8>(), 0..70_000)) {
        let gz = gzip_stored(&data);
        prop_assert_eq!(gunzip_bounded(&gz, data.len()).unwrap(), data.clone());
        if !data.is_empty() {
            prop_assert_eq!(gunzip_bounded(&gz, data.len() - 1), Err(GzipError::OutputLimit));
        }
    }
}

#[test]
fn hex_text_reads_back_as_the_bytes_it_spells() {
    assert_eq!(unhex("").unwrap(), Vec::<u8>::new());
    assert_eq!(unhex("00ff10Ab").unwrap(), vec![0x00, 0xff, 0x10, 0xab]);
    // Line breaks and spaces between the digits are ignored; a pair may not be split by one.
    assert_eq!(
        unhex("1f8b\n0800 ff\r\n").unwrap(),
        vec![0x1f, 0x8b, 0x08, 0x00, 0xff]
    );
    assert!(unhex("1f 8").is_err(), "an odd number of digits");
    assert!(unhex("abc").is_err(), "an odd number of digits");
    assert!(unhex("1g").is_err(), "not a hex digit");
    assert!(unhex("1\u{e9}").is_err(), "not ASCII");
    // Every byte value survives a round trip.
    let all: Vec<u8> = (0..=255).collect();
    let text: String = all.iter().map(|b| format!("{b:02x}\n")).collect();
    assert_eq!(unhex(&text).unwrap(), all);
}
