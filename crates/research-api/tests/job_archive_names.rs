//! The name and entry-type rules of the archive filter that keep the stored list printable and the stored archive plain: an entry whose
//! name has a character the filter does not support (a NUL, a backslash, bytes that are not UTF-8) or whose type it does not keep (a sparse
//! entry) is skipped and counted in the dropped list with its fixed reason, its data is skipped with it, and the files around it are kept.
//! The odd bytes are built here, in memory, from pieces: an old-style file name in another encoding, a path written with the separator of
//! another system, a name cut by a stray zero byte. Ordinary input only.

#![allow(clippy::unwrap_used)]

mod job_support;

use harness_protocol::research::ResearchLimits;
use job_support::*;
use research_api::archive::{Filtered, filter_archive, reason};

fn run(g: &[u8]) -> Filtered {
    filter_archive(g, &[], &ResearchLimits::default()).unwrap()
}

fn drops(f: &Filtered) -> Vec<(&str, &str)> {
    f.dropped
        .iter()
        .map(|d| (d.path.as_str(), d.reason.as_str()))
        .collect()
}

fn kept(f: &Filtered) -> Vec<&str> {
    f.files.iter().map(|m| m.path.as_str()).collect()
}

/// The stored archive holds the kept files only, each with the bytes it had.
fn stored(f: &Filtered) -> Vec<(String, Vec<u8>)> {
    read_output(&f.tar_gz)
        .into_iter()
        .map(|o| (o.path, o.data))
        .collect()
}

/// `<top>/` followed by the pieces, as the bytes of one entry name.
fn name_of(pieces: &[&[u8]]) -> Vec<u8> {
    let mut n = format!("{}/", top()).into_bytes();
    for p in pieces {
        n.extend_from_slice(p);
    }
    n
}

#[test]
fn an_entry_whose_name_is_cut_by_a_zero_byte_is_skipped_and_counted_however_the_name_is_written() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        // The name as a pax path record, and as a GNU long name: both carry the zero byte inside the name.
        let odd = name_of(&[b"notes", &[0u8], b"x.txt"]);
        t.pax_next(&[("path", odd.as_slice())]);
        t.file("short-pax-name", b"skipped one\n");
        t.gnu_long_name_exact(&name_of(&[b"more", &[0u8], b"y.txt"]));
        t.file("short-gnu-name", b"skipped two\n");
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "z-after.txt"]);
    // The list shows the stray byte as a question mark, whole names as they came.
    assert_eq!(
        drops(&f),
        [
            (
                &format!("{}/notes?x.txt", top()) as &str,
                reason::NUL_IN_NAME
            ),
            (
                &format!("{}/more?y.txt", top()) as &str,
                reason::NUL_IN_NAME
            ),
        ]
    );
    assert_eq!(
        stored(&f),
        [
            ("a-before.txt".to_string(), b"before\n".to_vec()),
            ("z-after.txt".to_string(), b"after\n".to_vec()),
        ]
    );
}

#[test]
fn an_entry_written_with_a_backslash_separator_is_skipped_and_counted_and_listed_without_the_backslash()
 {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        t.file_bytes_name(&name_of(&[b"docs", b"\\", b"guide.md"]), b"guide\n");
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "z-after.txt"]);
    assert_eq!(
        drops(&f),
        [(
            &format!("{}/docs?guide.md", top()) as &str,
            reason::BACKSLASH
        )]
    );
    assert_eq!(stored(&f).len(), 2);
}

#[test]
fn an_entry_whose_name_is_in_another_encoding_is_skipped_and_counted() {
    // "café.txt" written in Latin-1: the e-acute is the single byte 0xE9, which is not valid UTF-8.
    let latin1 = name_of(&[b"caf", &[0xE9u8], b".txt"]);
    // The same word in UTF-8 is an ordinary name and is kept.
    let utf8 = format!("{}/café.txt", top());
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        t.file_bytes_name(&latin1, b"latin one\n");
        t.file_bytes_name(utf8.as_bytes(), b"utf eight\n");
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "café.txt", "z-after.txt"]);
    // The list shows the byte that is not text as the replacement character.
    assert_eq!(
        drops(&f),
        [(
            &format!("{}/caf\u{FFFD}.txt", top()) as &str,
            reason::INVALID_UTF8
        )]
    );
    assert_eq!(stored(&f).len(), 3);
}

#[test]
fn a_sparse_entry_is_skipped_and_counted_whether_it_is_marked_in_the_header_or_in_a_pax_record() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        // The old GNU form: the type letter S, with a little data after the header.
        t.raw(
            format!("{top}/holes.bin").as_bytes(),
            b'S',
            600,
            b"",
            &[b'x'; 600],
        );
        // The pax form: a record that names a sparse map, then an entry that looks like a plain file.
        t.pax_next(&[("GNU.sparse.major", b"1"), ("GNU.sparse.minor", b"0")]);
        t.file(&format!("{top}/map.bin"), b"some bytes\n");
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "z-after.txt"]);
    assert_eq!(
        drops(&f),
        [("holes.bin", reason::SPARSE), ("map.bin", reason::SPARSE),]
    );
    assert_eq!(stored(&f).len(), 2);
}

#[test]
fn a_text_file_in_another_encoding_is_kept_byte_for_byte() {
    // Latin-1 text has bytes that are not UTF-8 and no zero byte: it is text, not binary.
    let latin1: Vec<u8> = [b"caf".as_slice(), &[0xE9u8], b" au lait\n"].concat();
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/menu.txt"), &latin1);
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["menu.txt"]);
    assert!(f.dropped.is_empty(), "{:?}", f.dropped);
    assert_eq!(stored(&f), [("menu.txt".to_string(), latin1.clone())]);
    assert_eq!(f.files[0].sha256, sha256_hex(&latin1));
}

#[test]
fn an_entry_with_a_control_character_in_its_name_is_skipped_and_counted() {
    // A tab, a bell, the delete character and a C1 control (next line): ordinary input of a careless tool, not text a file name keeps.
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        t.file_bytes_name(&name_of(&[b"tab", b"\t", b"one.txt"]), b"one\n");
        t.file_bytes_name(&name_of(&[b"bell", &[7u8], b"two.txt"]), b"two\n");
        t.file_bytes_name(&name_of(&[b"del", &[0x7Fu8], b"three.txt"]), b"three\n");
        t.file_bytes_name(
            &name_of(&[b"next", "\u{85}".as_bytes(), b"four.txt"]),
            b"four\n",
        );
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "z-after.txt"]);
    let top = top();
    assert_eq!(
        drops(&f),
        [
            (
                &format!("{top}/tab?one.txt") as &str,
                reason::UNSUPPORTED_CHARACTER
            ),
            (
                &format!("{top}/bell?two.txt") as &str,
                reason::UNSUPPORTED_CHARACTER
            ),
            (
                &format!("{top}/del?three.txt") as &str,
                reason::UNSUPPORTED_CHARACTER
            ),
            (
                &format!("{top}/next?four.txt") as &str,
                reason::UNSUPPORTED_CHARACTER
            ),
        ]
    );
    assert_eq!(stored(&f).len(), 2);
}

#[test]
fn an_entry_with_a_direction_or_zero_width_character_in_its_name_is_skipped_and_counted() {
    // Characters that change how a name is drawn without being drawn themselves: a right-to-left override, a zero-width space, a byte-order
    // mark, a direction isolate and a left-to-right mark. A name that reads one way and means another is not stored.
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        for (i, c) in ["\u{202e}", "\u{200b}", "\u{feff}", "\u{2066}", "\u{200e}"]
            .iter()
            .enumerate()
        {
            let name = format!("{top}/n{i}{c}x.txt");
            t.file_bytes_name(name.as_bytes(), b"odd\n");
        }
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "z-after.txt"]);
    let top = top();
    let want: Vec<(String, &str)> = (0..5)
        .map(|i| (format!("{top}/n{i}?x.txt"), reason::UNSUPPORTED_CHARACTER))
        .collect();
    let got: Vec<(String, &str)> = drops(&f)
        .into_iter()
        .map(|(p, r)| (p.to_string(), r))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn an_entry_whose_path_is_longer_than_the_manifest_allows_is_skipped_and_counted() {
    let at_limit = "d".repeat(100) + "/" + &"e".repeat(100) + "/" + &"f".repeat(53) + "x"; // 256 bytes
    assert_eq!(at_limit.len(), 256);
    let over = format!("{at_limit}y"); // 257 bytes
    // 129 two-byte letters make 258 bytes in 129 characters: the limit counts bytes.
    let wide = "é".repeat(129);
    let very_long = "g".repeat(5000);
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        for (i, name) in [&at_limit, &over, &wide, &very_long].iter().enumerate() {
            t.pax_next(&[("path", format!("{top}/{name}").as_bytes())]);
            t.file(&format!("short-{i}"), format!("body {i}\n").as_bytes());
        }
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", at_limit.as_str(), "z-after.txt"]);
    let rows = drops(&f);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(
        rows.iter().all(|(_, r)| *r == reason::NAME_TOO_LONG),
        "{rows:?}"
    );
    // The list shows each name cut at 256 characters.
    assert!(
        rows.iter().all(|(p, _)| p.chars().count() <= 256),
        "{rows:?}"
    );
    assert_eq!(stored(&f).len(), 3);
}

#[test]
fn an_entry_with_an_empty_part_in_its_name_is_skipped_and_counted() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        t.file_bytes_name(&name_of(&[b"docs//guide.md"]), b"guide\n");
        t.pax_next(&[("path", b"")]);
        t.file("short-name", b"nameless\n");
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["a-before.txt", "z-after.txt"]);
    let rows = drops(&f);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter().all(|(_, r)| *r == reason::EMPTY_COMPONENT),
        "{rows:?}"
    );
    assert_eq!(rows[0].0, format!("{}/docs//guide.md", top()));
    // A name that is empty is still listed with something to read: a dropped entry needs a path in the manifest.
    assert!(!rows[1].0.is_empty());
}
