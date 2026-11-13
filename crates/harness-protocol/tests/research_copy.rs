//! The exact-copy rule: which stored third-party files count, how a delivered file is compared
//! with them, and what is exempt. The same pure code runs in the bundle validator (delivery) and in `ham-research check` (agentd), so a
//! file the run is told about before it finishes is the file the validator would block.
#![allow(clippy::unwrap_used)]

use harness_protocol::research_copy::{
    COPY_FLOOR_BYTES, CopyIndex, MatchKind, StoredFile, is_exempt_path, normalise,
    normalised_sha256_hex, sha256_hex,
};
use proptest::prelude::*;

/// A deterministic body of `n` bytes that looks like source text (lines of 40 characters).
fn text(n: usize, tag: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 64);
    let mut i = 0;
    while out.len() < n {
        out.extend_from_slice(format!("// {tag} line {i:05} of third-party text\n").as_bytes());
        i += 1;
    }
    out.truncate(n);
    out
}

fn stored(item: &str, path: &str, bytes: &[u8]) -> StoredFile {
    StoredFile::from_bytes(item, "https://github.com/acme/widgets", "MIT", path, bytes)
}

// ---- normalise ------------------------------------------------------------------------------------------------------------

#[test]
fn normalise_makes_line_endings_trailing_blanks_and_the_final_newline_equal() {
    assert_eq!(normalise(b"a \r\nb\t\r\n\r\n"), b"a\nb\n".to_vec());
    assert_eq!(normalise(b"a\nb"), b"a\nb\n".to_vec());
    assert_eq!(normalise(b"a\nb\n\n\n"), b"a\nb\n".to_vec());
    assert_eq!(normalise(b"a\r\r\nb"), b"a\nb\n".to_vec());
}

#[test]
fn normalise_drops_a_byte_order_mark_and_keeps_inner_blank_lines_and_indentation() {
    assert_eq!(
        normalise(b"\xEF\xBB\xBF  a\n\n    b  \n"),
        b"  a\n\n    b\n".to_vec()
    );
}

#[test]
fn normalise_drops_every_leading_byte_order_mark() {
    assert_eq!(normalise(b"\xEF\xBB\xBF\xEF\xBB\xBFa"), b"a\n".to_vec());
}

#[test]
fn normalise_of_nothing_visible_is_empty() {
    assert_eq!(normalise(b""), b"".to_vec());
    assert_eq!(normalise(b"\n\n"), b"".to_vec());
    assert_eq!(normalise(b" \t\r\n"), b"".to_vec());
}

#[test]
fn normalise_leaves_a_lone_carriage_return_inside_a_line() {
    assert_eq!(normalise(b"a\rb\n"), b"a\rb\n".to_vec());
}

proptest! {
    #[test]
    fn normalise_is_idempotent_and_never_longer_than_the_input_plus_one(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let once = normalise(&bytes);
        let twice = normalise(&once);
        prop_assert_eq!(&once, &twice);
        prop_assert!(once.len() <= bytes.len() + 1);
    }
}

#[test]
fn the_hashes_are_lower_case_sha256_of_the_bytes_and_of_the_normalised_bytes() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(normalised_sha256_hex(b"abc\r\n"), sha256_hex(b"abc\n"));
}

// ---- exempt names ---------------------------------------------------------------------------------------------------------

#[test]
fn licence_texts_notices_and_copying_files_are_exempt_in_any_case_and_directory() {
    for p in [
        "LICENSE",
        "license",
        "LICENSE.md",
        "License.txt",
        "LICENCE",
        "COPYING",
        "COPYING.LESSER",
        "NOTICE",
        "NOTICE.txt",
        "UNLICENSE",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "vendor/acme/LICENSE",
        "a/b/c/notice.md",
    ] {
        assert!(is_exempt_path(p), "{p} should be exempt");
    }
}

#[test]
fn ignore_files_editor_settings_lockfiles_and_minified_files_are_exempt() {
    for p in [
        ".gitignore",
        "sub/.gitignore",
        ".editorconfig",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "poetry.lock",
        "Pipfile.lock",
        "uv.lock",
        "go.sum",
        "composer.lock",
        "Gemfile.lock",
        "web/jquery.min.js",
        "dist/app.min.css",
        "lib/x.min.mjs",
    ] {
        assert!(is_exempt_path(p), "{p} should be exempt");
    }
}

#[test]
fn code_that_only_starts_like_a_licence_name_is_not_exempt() {
    for p in [
        "src/license_check.py",
        "src/notice_board.js",
        "src/licenses.rs",
        "LICENSE-MIT.js",
        "copying_tool.go",
        "src/main.rs",
        "README.md",
        "src/minimal.js",
        "docs/gitignore.md",
        "lockfile.py",
        "go.sum.go",
    ] {
        assert!(!is_exempt_path(p), "{p} should not be exempt");
    }
}

// ---- the index of stored files --------------------------------------------------------------------------------------------

#[test]
fn the_floor_is_one_kibibyte_of_the_stored_file() {
    assert_eq!(COPY_FLOOR_BYTES, 1024);
    let big = text(1200, "big");
    let small = text(900, "small");
    let edge = text(1024, "edge");
    let below = text(1023, "below");
    let ix = CopyIndex::new(vec![
        stored("i-0001", "src/big.js", &big),
        stored("i-0001", "src/small.js", &small),
        stored("i-0001", "src/edge.js", &edge),
        stored("i-0001", "src/below.js", &below),
    ]);
    assert_eq!(ix.len(), 2, "only the files of 1,024 bytes or more count");
    assert!(ix.find("delivered/big.js", &big).is_some());
    assert!(ix.find("delivered/edge.js", &edge).is_some());
    assert!(ix.find("delivered/small.js", &small).is_none());
    assert!(ix.find("delivered/below.js", &below).is_none());
    // The index lists the files that count, in the order given.
    let paths: Vec<&str> = ix.files().iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["src/big.js", "src/edge.js"]);
}

#[test]
fn a_delivered_copy_matches_whatever_its_name_and_names_the_stored_file_and_its_item() {
    let body = text(1500, "copy");
    let ix = CopyIndex::new(vec![stored("i-0007", "lib/util.js", &body)]);
    let m = ix.find("src/vendor/renamed.txt", &body).unwrap();
    assert_eq!(m.kind, MatchKind::Exact);
    assert_eq!(m.stored.item_id, "i-0007");
    assert_eq!(m.stored.path, "lib/util.js");
    assert_eq!(m.stored.origin, "https://github.com/acme/widgets");
    assert_eq!(m.stored.licence, "MIT");
}

#[test]
fn a_crlf_variant_and_a_trailing_blank_variant_match_as_normalised() {
    let body = text(1500, "variant");
    let ix = CopyIndex::new(vec![stored("i-0001", "lib/util.js", &body)]);
    let crlf: Vec<u8> = String::from_utf8(body.clone())
        .unwrap()
        .replace('\n', "\r\n")
        .into_bytes();
    let m = ix.find("src/util.js", &crlf).unwrap();
    assert_eq!(m.kind, MatchKind::Normalised);
    let mut padded = body.clone();
    padded.extend_from_slice(b"\n\n   \n");
    assert_eq!(
        ix.find("src/util.js", &padded).unwrap().kind,
        MatchKind::Normalised
    );
    let mut changed = body.clone();
    changed[10] = b'#';
    assert!(
        ix.find("src/util.js", &changed).is_none(),
        "one changed byte is a different file"
    );
}

#[test]
fn an_exact_match_is_reported_as_exact_even_when_the_normalised_form_also_matches() {
    let body = text(1500, "exactfirst");
    let ix = CopyIndex::new(vec![stored("i-0001", "a.js", &body)]);
    assert_eq!(ix.find("b.js", &body).unwrap().kind, MatchKind::Exact);
}

#[test]
fn an_entry_without_a_normalised_hash_matches_exactly_only() {
    let body = text(1500, "nonorm");
    let mut f = stored("i-0001", "a.js", &body);
    f.norm_sha256 = None;
    let ix = CopyIndex::new(vec![f]);
    let crlf = String::from_utf8(body.clone())
        .unwrap()
        .replace('\n', "\r\n")
        .into_bytes();
    assert!(ix.find("b.js", &body).is_some());
    assert!(ix.find("b.js", &crlf).is_none());
}

#[test]
fn exempt_delivered_names_and_empty_files_never_match() {
    let body = text(1500, "licencelike");
    let ix = CopyIndex::new(vec![stored("i-0001", "lib/util.js", &body)]);
    for name in ["LICENSE", "src/.gitignore", "web/app.min.js", "Cargo.lock"] {
        assert!(
            ix.find(name, &body).is_none(),
            "{name} is exempt on the delivered side"
        );
    }
    assert!(ix.find("src/empty.txt", b"").is_none());
}

#[test]
fn stored_files_with_exempt_names_are_not_in_the_index() {
    let licence = text(3000, "licence text");
    let lock = text(5000, "lock");
    let min = text(5000, "min");
    let ix = CopyIndex::new(vec![
        stored("i-0001", "LICENSE", &licence),
        stored("i-0001", "package-lock.json", &lock),
        stored("i-0001", "dist/x.min.js", &min),
    ]);
    assert!(ix.is_empty());
    assert!(ix.find("docs/my-licence-copy.txt", &licence).is_none());
}

#[test]
fn the_first_stored_file_in_order_names_a_hash_that_several_items_share() {
    let body = text(1500, "shared");
    let ix = CopyIndex::new(vec![
        stored("i-0001", "a/x.js", &body),
        stored("i-0002", "b/y.js", &body),
    ]);
    let m = ix.find("z.js", &body).unwrap();
    assert_eq!(
        (m.stored.item_id.as_str(), m.stored.path.as_str()),
        ("i-0001", "a/x.js")
    );
}
