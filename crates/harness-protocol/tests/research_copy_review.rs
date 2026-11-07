//! Review fixes of the exact-copy rule: the normalised form of a stored file has a floor of
//! its own, so a stored file padded with blanks past 1 KiB cannot make an ordinary short file look like a copy; and `normalise` is made
//! without a vector of lines, which must not change its output.
#![allow(clippy::unwrap_used)]

use harness_protocol::research_copy::{
    COPY_NORMALISED_FLOOR_BYTES, CopyIndex, StoredFile, normalise,
};
use proptest::prelude::*;

/// The first version of `normalise`, kept as the reference: split into lines, trim each, drop trailing empty lines.
fn reference(bytes: &[u8]) -> Vec<u8> {
    let mut rest = bytes;
    while let Some(after) = rest.strip_prefix(b"\xEF\xBB\xBF") {
        rest = after;
    }
    let mut lines: Vec<&[u8]> = rest
        .split(|b| *b == b'\n')
        .map(|line| {
            let mut end = line.len();
            while end > 0 && matches!(line[end - 1], b' ' | b'\t' | b'\r') {
                end -= 1;
            }
            &line[..end]
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

proptest! {
    #[test]
    fn the_output_of_normalise_is_the_output_of_the_reference(bytes in proptest::collection::vec(prop_oneof![Just(b'\n'), Just(b'\r'), Just(b' '), Just(b'\t'), Just(b'a'), Just(b'b'), any::<u8>()], 0..300)) {
        prop_assert_eq!(normalise(&bytes), reference(&bytes));
    }
}

#[test]
fn a_stored_file_whose_normalised_form_is_short_has_no_normalised_hash() {
    assert_eq!(COPY_NORMALISED_FLOOR_BYTES, 512);
    let mut padded = b"x\n".to_vec();
    padded.extend(std::iter::repeat_n(b' ', 1200));
    padded.extend_from_slice(b"\n\n\n");
    assert!(padded.len() >= 1024);
    let f = StoredFile::from_bytes("i-0001", "https://example.org/a", "MIT", "pad.txt", &padded);
    assert!(f.norm_sha256.is_none());
    let ix = CopyIndex::new(vec![f]);
    assert!(
        ix.find("src/x.txt", b"x\n").is_none(),
        "a two-byte file is not a copy of a padded file"
    );
    assert!(
        ix.find("src/pad.txt", &padded).is_some(),
        "the same bytes still match exactly"
    );
}

#[test]
fn a_stored_file_whose_normalised_form_is_long_enough_keeps_its_normalised_hash() {
    let body: String = (0..60)
        .map(|i| format!("line {i:03} of the text\n"))
        .collect();
    assert!(body.len() >= 1024);
    let f = StoredFile::from_bytes(
        "i-0001",
        "https://example.org/a",
        "MIT",
        "a.txt",
        body.as_bytes(),
    );
    assert!(f.norm_sha256.is_some());
}
