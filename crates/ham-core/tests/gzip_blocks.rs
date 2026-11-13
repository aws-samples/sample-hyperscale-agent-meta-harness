//! Hand-built deflate blocks for the cases a compressor never writes: the malformed trees, the references that reach too far and the
//! limits at their edges (the review of 2026-10-16 asked for each of them by name). `tests/gzip.rs` holds the streams real
//! compressors made; these are the other half.
#![allow(clippy::unwrap_used)]

use ham_core::gzip::{GzipError, crc32, gunzip_bounded};

/// Deflate bits, least significant first; Huffman codes go most significant bit first.
#[derive(Default)]
struct W {
    bytes: Vec<u8>,
    acc: u32,
    n: u32,
}

impl W {
    fn bits(&mut self, value: u32, count: u32) -> &mut Self {
        self.acc |= value << self.n;
        self.n += count;
        while self.n >= 8 {
            self.bytes.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
        self
    }
    fn code(&mut self, code: u32, len: u32) -> &mut Self {
        let mut rev = 0;
        for i in 0..len {
            rev |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.bits(rev, len)
    }
    /// A fixed-Huffman block header; `last` is BFINAL.
    fn fixed(&mut self, last: bool) -> &mut Self {
        self.bits(u32::from(last), 1).bits(1, 2)
    }
    fn lit(&mut self, b: u8) -> &mut Self {
        let b = u32::from(b);
        if b < 144 {
            self.code(0x30 + b, 8)
        } else {
            self.code(0x190 + b - 144, 9)
        }
    }
    /// A length symbol 257..=279 (no extra bits for 257..=264) of the fixed code.
    fn len_sym(&mut self, sym: u32) -> &mut Self {
        if sym < 280 {
            self.code(sym - 256, 7)
        } else {
            self.code(0xC0 + sym - 280, 8)
        }
    }
    fn dist_sym(&mut self, sym: u32) -> &mut Self {
        self.code(sym, 5)
    }
    fn eob(&mut self) -> &mut Self {
        self.code(0, 7)
    }
    fn finish(&mut self) -> Vec<u8> {
        if self.n > 0 {
            self.bytes.push((self.acc & 0xFF) as u8);
            self.acc = 0;
            self.n = 0;
        }
        std::mem::take(&mut self.bytes)
    }
}

/// A gzip member around a raw deflate `body` that must inflate to `plain`.
fn member(body: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut v = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    v.extend_from_slice(body);
    v.extend_from_slice(&crc32(plain).to_le_bytes());
    v.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    v
}

#[test]
fn a_match_may_reach_back_exactly_to_the_first_byte_and_no_further() {
    // 'a' 'b' then a length-3 match at distance 2 ("bab" continues the pattern) and at distance 3 (one past the start).
    let ok = W::default()
        .fixed(true)
        .lit(b'a')
        .lit(b'b')
        .len_sym(257)
        .dist_sym(1)
        .eob()
        .finish();
    assert_eq!(
        gunzip_bounded(&member(&ok, b"ababa"), 100).unwrap(),
        b"ababa"
    );
    // Distance code 2 is distance 3: only two bytes exist.
    let far = W::default()
        .fixed(true)
        .lit(b'a')
        .lit(b'b')
        .len_sym(257)
        .dist_sym(2)
        .eob()
        .finish();
    assert_eq!(
        gunzip_bounded(&member(&far, b""), 100),
        Err(GzipError::BadDistance)
    );
    // A match before any byte.
    let first = W::default()
        .fixed(true)
        .len_sym(257)
        .dist_sym(0)
        .eob()
        .finish();
    assert_eq!(
        gunzip_bounded(&member(&first, b""), 100),
        Err(GzipError::BadDistance)
    );
}

#[test]
fn matches_that_overlap_their_own_output_repeat_it() {
    // Distance 1, length 10: "a" then ten more.
    let one = W::default()
        .fixed(true)
        .lit(b'a')
        .len_sym(264)
        .dist_sym(0)
        .eob()
        .finish();
    assert_eq!(
        gunzip_bounded(&member(&one, &[b'a'; 11]), 100).unwrap(),
        vec![b'a'; 11]
    );
    // Distance 2, length 10: "ab" repeated.
    let two = W::default()
        .fixed(true)
        .lit(b'a')
        .lit(b'b')
        .len_sym(264)
        .dist_sym(1)
        .eob()
        .finish();
    assert_eq!(
        gunzip_bounded(&member(&two, b"abababababab"), 100).unwrap(),
        b"abababababab"
    );
}

#[test]
fn a_distance_of_32768_is_reachable_after_enough_output() {
    // 40,000 literals, then a match of length 3 at the largest distance symbol's top: symbol 29 base 24577, 13 extra bits.
    let mut w = W::default();
    w.fixed(true);
    let plain: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    for &b in &plain {
        w.lit(b);
    }
    // Distance 32768 = 24577 + 8191.
    w.len_sym(257).dist_sym(29).bits(8191, 13).eob();
    let body = w.finish();
    let mut want = plain.clone();
    let start = want.len() - 32768;
    for i in 0..3 {
        want.push(want[start + i]);
    }
    assert_eq!(
        gunzip_bounded(&member(&body, &want), 1 << 20).unwrap(),
        want
    );
}

#[test]
fn reserved_symbols_of_the_fixed_code_are_refused() {
    // Length symbol 286 (8-bit code 11000110) and 287.
    for sym in [286u32, 287] {
        let body = W::default()
            .fixed(true)
            .lit(b'a')
            .code(0xC0 + sym - 280, 8)
            .eob()
            .finish();
        assert_eq!(
            gunzip_bounded(&member(&body, b""), 100),
            Err(GzipError::BadCode),
            "length symbol {sym}"
        );
    }
    // Distance symbols 30 and 31 do not exist.
    for sym in [30u32, 31] {
        let body = W::default()
            .fixed(true)
            .lit(b'a')
            .len_sym(257)
            .dist_sym(sym)
            .eob()
            .finish();
        assert_eq!(
            gunzip_bounded(&member(&body, b""), 100),
            Err(GzipError::BadCode),
            "distance symbol {sym}"
        );
    }
}

#[test]
fn a_stored_block_after_an_unaligned_block_starts_on_the_next_byte() {
    // A non-final fixed block with one literal leaves five unused bits; the stored block that follows begins on a byte boundary.
    let mut w = W::default();
    // The stored block's own header (BFINAL 1, BTYPE 00) follows the end-of-block code in the same byte; the rest of that byte is padding.
    w.fixed(false).lit(b'x').eob().bits(1, 1).bits(0, 2);
    let mut body = w.finish();
    // LEN 2, its complement, and the bytes of the final stored block "yz".
    body.extend_from_slice(&[0x02, 0x00, 0xfd, 0xff, b'y', b'z']);
    assert_eq!(gunzip_bounded(&member(&body, b"xyz"), 100).unwrap(), b"xyz");
    // A zero-length stored block between two others.
    let body = [
        0x00, 0, 0, 0xff, 0xff, 0x00, 1, 0, 0xfe, 0xff, b'q', 0x01, 0, 0, 0xff, 0xff,
    ];
    assert_eq!(gunzip_bounded(&member(&body, b"q"), 100).unwrap(), b"q");
}

#[test]
fn the_output_limit_holds_at_cap_minus_one_cap_and_cap_plus_one_for_each_kind_of_output() {
    let cap = 300usize;
    // Literals.
    let mut lits = W::default();
    lits.fixed(true);
    for _ in 0..cap + 1 {
        lits.lit(b'k');
    }
    let body = lits.eob().finish();
    assert_eq!(
        gunzip_bounded(&member(&body, &vec![b'k'; cap + 1]), cap),
        Err(GzipError::OutputLimit)
    );
    assert_eq!(
        gunzip_bounded(&member(&body, &vec![b'k'; cap + 1]), cap + 1)
            .unwrap()
            .len(),
        cap + 1
    );
    assert_eq!(
        gunzip_bounded(&member(&body, &vec![b'k'; cap + 1]), cap + 2)
            .unwrap()
            .len(),
        cap + 1
    );
    // A stored block of cap + 1 bytes.
    let n = (cap + 1) as u16;
    let mut stored = vec![0x01];
    stored.extend_from_slice(&n.to_le_bytes());
    stored.extend_from_slice(&(!n).to_le_bytes());
    stored.extend_from_slice(&vec![b's'; cap + 1]);
    assert_eq!(
        gunzip_bounded(&member(&stored, &vec![b's'; cap + 1]), cap),
        Err(GzipError::OutputLimit)
    );
    assert_eq!(
        gunzip_bounded(&member(&stored, &vec![b's'; cap + 1]), cap + 1)
            .unwrap()
            .len(),
        cap + 1
    );
    // A literal and a match of 258 (symbol 285): 259 bytes.
    let body = W::default()
        .fixed(true)
        .lit(b'r')
        .len_sym(285)
        .dist_sym(0)
        .eob()
        .finish();
    assert_eq!(
        gunzip_bounded(&member(&body, &[b'r'; 259]), 258),
        Err(GzipError::OutputLimit)
    );
    assert_eq!(
        gunzip_bounded(&member(&body, &[b'r'; 259]), 259)
            .unwrap()
            .len(),
        259
    );
}

/// A dynamic block header whose code-length code is the two symbols `a` and `b` of length 1 each (a complete code, `a` < `b` gets 0).
/// `symbols` are then written as codes of that code: 0 is the first symbol, 1 the second.
fn dynamic_with(
    a: u32,
    b: u32,
    hlit: u32,
    hdist: u32,
    steps: &[(u32, Option<(u32, u32)>)],
) -> Vec<u8> {
    let order = [
        16u32, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut w = W::default();
    w.bits(1, 1)
        .bits(2, 2)
        .bits(hlit, 5)
        .bits(hdist, 5)
        .bits(0, 4); // HCLEN 0: four code-length codes (16, 17, 18, 0)
    for &sym in &order[..4] {
        w.bits(u32::from(sym == a || sym == b), 3);
    }
    for &(which, extra) in steps {
        w.code(which, 1);
        if let Some((value, count)) = extra {
            w.bits(value, count);
        }
    }
    w.finish()
}

#[test]
fn malformed_dynamic_headers_are_refused_with_the_code_error() {
    // Symbol 16 (repeat the previous length) as the very first length: nothing precedes it. Codes: 0 -> symbol 0, 1 -> symbol 16.
    let body = dynamic_with(0, 16, 0, 0, &[(1, Some((0, 2)))]);
    assert_eq!(
        gunzip_bounded(&member(&body, b""), 100),
        Err(GzipError::BadCode)
    );
    // Repeats that run past the declared table: 258 lengths, three runs of 138 zeros (symbol 18, 7 extra bits = 127).
    let body = dynamic_with(
        0,
        18,
        0,
        0,
        &[
            (1, Some((127, 7))),
            (1, Some((127, 7))),
            (1, Some((127, 7))),
        ],
    );
    assert_eq!(
        gunzip_bounded(&member(&body, b""), 100),
        Err(GzipError::BadCode)
    );
    // Exactly 258 lengths, all zero: the table fits, but the end-of-block symbol has no code.
    let body = dynamic_with(0, 18, 0, 0, &[(1, Some((127, 7))), (1, Some((109, 7)))]);
    assert_eq!(
        gunzip_bounded(&member(&body, b""), 100),
        Err(GzipError::BadCode)
    );
    // HLIT 29 would declare 286 + ... literal codes plus HDIST 31 distances: past the 286 and 30 limits.
    let body = dynamic_with(0, 18, 31, 31, &[]);
    assert_eq!(
        gunzip_bounded(&member(&body, b""), 100),
        Err(GzipError::BadCode)
    );
    // An incomplete code-length code (one symbol of length 1 only).
    let body = dynamic_with(0, 0, 0, 0, &[]);
    assert_eq!(
        gunzip_bounded(&member(&body, b""), 100),
        Err(GzipError::BadCode)
    );
}

#[test]
fn the_degenerate_distance_tree_zlib_writes_for_literal_only_data_is_accepted() {
    // zlib's Z_HUFFMAN_ONLY output has a dynamic block with a single distance code of length one: an incomplete code that the
    // format allows. The fixture holds one; it must decode.
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gzip");
    let gz = ham_core::gzip::unhex(
        &std::fs::read_to_string(dir.join("text-huffman-only.gz.hex")).unwrap(),
    )
    .unwrap();
    assert_eq!(gunzip_bounded(&gz, 1 << 20).unwrap().len(), 40_000);
}
