//! The confined extractor of the research module: a stored repository archive is unpacked into a fresh
//! directory only if it is exactly what its manifest lists. Entry types other than files and directories, paths that leave the directory,
//! members the manifest does not list, hashes that differ and archives that expand past a cap all end the extraction, and a failed
//! extraction leaves nothing behind. Every archive here is made by hand (`raw`), because the `tar` crate's builder refuses the paths
//! these tests need to try.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::research::extract::{ExtractCaps, ExtractError, WantedFile, extract_targz};
use common::scratch;
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// One archive member as it is written, whatever a builder would allow.
struct Raw {
    name: Vec<u8>,
    kind: u8,
    body: Vec<u8>,
    link: Vec<u8>,
    mode: u32,
}

fn raw(name: &str, kind: u8, body: &[u8]) -> Raw {
    Raw {
        name: name.as_bytes().to_vec(),
        kind,
        body: body.to_vec(),
        link: Vec::new(),
        mode: 0o644,
    }
}

fn file(name: &str, body: &[u8]) -> Raw {
    raw(name, b'0', body)
}

fn dir(name: &str) -> Raw {
    raw(name, b'5', b"")
}

fn header_block(name: &[u8], kind: u8, size: usize, link: &[u8], mode: u32) -> Vec<u8> {
    // A sparse member only exists in the GNU format.
    let mut h = if kind == b'S' {
        tar::Header::new_gnu()
    } else {
        tar::Header::new_ustar()
    };
    {
        let old = h.as_old_mut();
        old.name[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
        old.linkname[..link.len()].copy_from_slice(link);
    }
    h.set_entry_type(tar::EntryType::new(kind));
    h.set_size(size as u64);
    h.set_mode(mode);
    h.set_mtime(0);
    h.set_cksum();
    h.as_bytes().to_vec()
}

fn pad(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(512) {
        out.push(0);
    }
}

fn tar_bytes(entries: &[Raw]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        if e.name.len() > 100 {
            // A GNU long name: its own member, then the real one with a cut name.
            let mut long = e.name.clone();
            long.push(0);
            out.extend(header_block(b"././@LongLink", b'L', long.len(), b"", 0o644));
            out.extend(&long);
            pad(&mut out);
        }
        out.extend(header_block(&e.name, e.kind, e.body.len(), &e.link, e.mode));
        out.extend(&e.body);
        pad(&mut out);
    }
    out.extend(vec![0u8; 1024]);
    out
}

fn gz(tar: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(tar).unwrap();
    enc.finish().unwrap()
}

fn archive(entries: &[Raw]) -> Vec<u8> {
    gz(&tar_bytes(entries))
}

fn hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn want(files: &[(&str, &[u8])]) -> BTreeMap<String, WantedFile> {
    files
        .iter()
        .map(|(p, b)| {
            (
                (*p).to_string(),
                WantedFile {
                    sha256: hex(b),
                    bytes: b.len() as u64,
                },
            )
        })
        .collect()
}

fn text(n: usize, tag: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < n {
        out.extend_from_slice(format!("{tag} line {i:04}\n").as_bytes());
        i += 1;
    }
    out.truncate(n);
    out
}

fn caps() -> ExtractCaps {
    ExtractCaps {
        max_unpacked_bytes: 1024 * 1024,
        max_files: 100,
        max_file_bytes: 256 * 1024,
        min_free_bytes: 0,
    }
}

/// Makes a tree writable again so a test can remove it.
fn tidy(root: &Path) {
    fn walk(p: &Path) {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return;
        };
        if m.is_dir() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path());
                }
            }
        }
    }
    walk(root);
    let _ = std::fs::remove_dir_all(root);
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// A fresh parent and the destination in it (the destination does not exist yet).
fn fresh(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let parent = scratch(label);
    let dest = parent.join("repo");
    (parent, dest)
}

// ---- the good archive -----------------------------------------------------------------------------------------------------

#[test]
fn a_listed_archive_is_extracted_with_nested_directories_and_every_hash_checked() {
    let readme = text(1500, "readme");
    let lib = text(3000, "lib");
    let deep = text(700, "deep");
    let mut exec = file("bin/run.sh", &text(200, "run"));
    exec.mode = 0o4755;
    let archive = archive(&[
        dir("docs/"),
        file("README.md", &readme),
        file("src/lib.rs", &lib),
        file("src/deep/er/mod.rs", &deep),
        exec,
    ]);
    let run_sh = text(200, "run");
    let wanted = want(&[
        ("README.md", &readme),
        ("src/lib.rs", &lib),
        ("src/deep/er/mod.rs", &deep),
        ("bin/run.sh", &run_sh),
    ]);
    let (parent, dest) = fresh("ex-good");
    let got = extract_targz(&archive, &dest, &wanted, &caps()).unwrap();

    let paths: Vec<&str> = got.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "README.md",
            "bin/run.sh",
            "src/deep/er/mod.rs",
            "src/lib.rs"
        ]
    );
    assert_eq!(got.unpacked_bytes, (1500 + 3000 + 700 + 200) as u64);
    assert_eq!(std::fs::read(dest.join("src/lib.rs")).unwrap(), lib);
    assert_eq!(
        std::fs::read(dest.join("src/deep/er/mod.rs")).unwrap(),
        deep
    );
    assert!(dest.join("docs").is_dir(), "a directory member is made");
    let readme_entry = got.files.iter().find(|f| f.path == "README.md").unwrap();
    assert_eq!(readme_entry.sha256, hex(&readme));
    assert_eq!(readme_entry.bytes, 1500);

    // Read-only for everyone, no execute bit, no set-id bit, whatever the archive said; directories read-only too.
    for f in &got.files {
        assert_eq!(mode(&dest.join(&f.path)), 0o444, "{}", f.path);
    }
    for d in ["", "src", "src/deep", "src/deep/er", "docs", "bin"] {
        assert_eq!(mode(&dest.join(d)), 0o555, "directory {d:?}");
    }
    tidy(&parent);
}

#[test]
fn the_normalised_hash_of_a_crlf_file_is_that_of_the_lf_file() {
    // Whole lines only: the text builder cuts mid-line, and a trailing blank is what normalising removes.
    let mut lf = text(1400, "crlf");
    let end = lf.iter().rposition(|b| *b == b'\n').unwrap();
    lf.truncate(end + 1);
    let crlf: Vec<u8> = String::from_utf8(lf.clone())
        .unwrap()
        .replace('\n', "\r\n")
        .into_bytes();
    let (parent, dest) = fresh("ex-norm");
    let got = extract_targz(
        &archive(&[file("a.txt", &crlf)]),
        &dest,
        &want(&[("a.txt", &crlf)]),
        &caps(),
    )
    .unwrap();
    assert_eq!(got.files[0].sha256, hex(&crlf));
    assert_eq!(got.files[0].norm_sha256.as_deref(), Some(hex(&lf).as_str()));
    tidy(&parent);
}

#[test]
fn a_pax_global_header_and_a_gnu_long_name_are_read() {
    let long_path = format!("{}/file.txt", "d".repeat(60) + "/" + &"e".repeat(60));
    assert!(long_path.len() > 100);
    let body = text(1100, "long");
    let comment = format!("comment={}", "a".repeat(40));
    let mut record = format!(" {comment}\n");
    let mut n = record.len() + 1;
    while (n.to_string() + &record).len() != n {
        n = (n.to_string() + &record).len();
    }
    record = format!("{n}{record}");
    let archive = archive(&[
        raw("pax_global_header", b'g', record.as_bytes()),
        file(&long_path, &body),
    ]);
    let (parent, dest) = fresh("ex-pax");
    let got = extract_targz(&archive, &dest, &want(&[(&long_path, &body)]), &caps()).unwrap();
    assert_eq!(got.files.len(), 1);
    assert_eq!(std::fs::read(dest.join(&long_path)).unwrap(), body);
    tidy(&parent);
}

#[test]
fn an_empty_manifest_and_an_empty_archive_give_an_empty_tree() {
    let (parent, dest) = fresh("ex-empty");
    let got = extract_targz(&archive(&[]), &dest, &BTreeMap::new(), &caps()).unwrap();
    assert!(got.files.is_empty());
    assert_eq!(got.unpacked_bytes, 0);
    assert!(dest.is_dir());
    tidy(&parent);
}

// ---- what the manifest does not say ---------------------------------------------------------------------------------------

#[test]
fn a_member_the_manifest_does_not_list_ends_the_extraction_and_leaves_nothing() {
    let a = text(1200, "a");
    let b = text(1200, "b");
    let (parent, dest) = fresh("ex-unlisted");
    let err = extract_targz(
        &archive(&[file("a.txt", &a), file("extra.txt", &b)]),
        &dest,
        &want(&[("a.txt", &a)]),
        &caps(),
    )
    .unwrap_err();
    assert!(
        matches!(err, ExtractError::Unlisted(ref p) if p == "extra.txt"),
        "{err:?}"
    );
    assert!(!dest.exists(), "a failed extraction leaves no directory");
    tidy(&parent);
}

#[test]
fn a_listed_file_that_is_not_in_the_archive_is_an_error() {
    let a = text(1200, "a");
    let b = text(1200, "b");
    let (parent, dest) = fresh("ex-missing");
    let err = extract_targz(
        &archive(&[file("a.txt", &a)]),
        &dest,
        &want(&[("a.txt", &a), ("b.txt", &b)]),
        &caps(),
    )
    .unwrap_err();
    assert!(
        matches!(err, ExtractError::Missing(ref p) if p == "b.txt"),
        "{err:?}"
    );
    assert!(!dest.exists());
    tidy(&parent);
}

#[test]
fn a_file_whose_bytes_differ_from_the_manifest_hash_is_an_integrity_error() {
    let a = text(1200, "a");
    let mut changed = a.clone();
    changed[5] = b'#';
    let (parent, dest) = fresh("ex-hash");
    let err = extract_targz(
        &archive(&[file("a.txt", &changed)]),
        &dest,
        &want(&[("a.txt", &a)]),
        &caps(),
    )
    .unwrap_err();
    assert!(
        matches!(err, ExtractError::Integrity(ref p) if p == "a.txt"),
        "{err:?}"
    );
    assert_eq!(err.code(), "integrity");
    assert!(!dest.exists());
    tidy(&parent);
}

#[test]
fn a_file_longer_than_the_manifest_size_is_an_integrity_error_without_reading_it_all() {
    let a = text(1200, "a");
    let longer = text(200_000, "a");
    let mut wanted = want(&[("a.txt", &a)]);
    wanted.get_mut("a.txt").unwrap().sha256 = hex(&longer);
    let (parent, dest) = fresh("ex-size");
    let err =
        extract_targz(&archive(&[file("a.txt", &longer)]), &dest, &wanted, &caps()).unwrap_err();
    assert!(matches!(err, ExtractError::Integrity(_)), "{err:?}");
    tidy(&parent);
}

#[test]
fn a_member_listed_once_but_present_twice_is_refused() {
    let a = text(1200, "a");
    let (parent, dest) = fresh("ex-dup");
    let err = extract_targz(
        &archive(&[file("a.txt", &a), file("a.txt", &a)]),
        &dest,
        &want(&[("a.txt", &a)]),
        &caps(),
    )
    .unwrap_err();
    assert!(
        matches!(err, ExtractError::Duplicate(ref p) if p == "a.txt"),
        "{err:?}"
    );
    assert!(!dest.exists());
    tidy(&parent);
}

// ---- paths ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_path_that_is_not_a_plain_relative_name_is_refused_even_when_the_manifest_lists_it() {
    let body = text(1200, "p");
    // Names the extractor does not take: an empty segment, a dot segment, a backslash, a control character, nothing at all.
    for bad in ["./a", "a/./b", "a//b", "a\\b", "a/b\u{1}c", ""] {
        let (parent, dest) = fresh("ex-path");
        let err = extract_targz(
            &archive(&[file(bad, &body)]),
            &dest,
            &want(&[(bad, &body)]),
            &caps(),
        )
        .unwrap_err();
        assert!(matches!(err, ExtractError::BadPath(_)), "{bad:?}: {err:?}");
        assert!(!dest.exists(), "{bad:?}");
        tidy(&parent);
    }
}

#[test]
fn a_path_that_is_not_utf8_is_refused() {
    let body = text(1200, "p");
    let mut bad = file("x", &body);
    bad.name = vec![b'a', 0xFF, b'b'];
    let (parent, dest) = fresh("ex-utf8");
    let err = extract_targz(&archive(&[bad]), &dest, &BTreeMap::new(), &caps()).unwrap_err();
    assert!(matches!(err, ExtractError::BadPath(_)), "{err:?}");
    tidy(&parent);
}

// ---- caps -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_unpacked_total_is_capped_across_files() {
    let a = text(60_000, "a");
    let b = text(60_000, "b");
    let c = text(60_000, "c");
    let (parent, dest) = fresh("ex-total");
    let small = ExtractCaps {
        max_unpacked_bytes: 150_000,
        ..caps()
    };
    let err = extract_targz(
        &archive(&[file("a", &a), file("b", &b), file("c", &c)]),
        &dest,
        &want(&[("a", &a), ("b", &b), ("c", &c)]),
        &small,
    )
    .unwrap_err();
    assert!(matches!(err, ExtractError::TooLarge(_)), "{err:?}");
    tidy(&parent);
}

#[test]
fn a_file_over_the_single_file_cap_is_refused() {
    let big = text(300_000, "big");
    let (parent, dest) = fresh("ex-file");
    let err = extract_targz(
        &archive(&[file("big.txt", &big)]),
        &dest,
        &want(&[("big.txt", &big)]),
        &caps(),
    )
    .unwrap_err();
    assert!(matches!(err, ExtractError::TooLarge(_)), "{err:?}");
    tidy(&parent);
}

#[test]
fn more_members_than_the_cap_or_more_listed_files_than_the_cap_are_refused() {
    let bodies: Vec<(String, Vec<u8>)> = (0..30)
        .map(|i| (format!("f{i:02}.txt"), text(1100, &format!("f{i}"))))
        .collect();
    let entries: Vec<Raw> = bodies.iter().map(|(n, b)| file(n, b)).collect();
    let refs: Vec<(&str, &[u8])> = bodies
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let few = ExtractCaps {
        max_files: 20,
        ..caps()
    };
    let (parent, dest) = fresh("ex-many");
    let err = extract_targz(&archive(&entries), &dest, &want(&refs), &few).unwrap_err();
    assert!(matches!(err, ExtractError::TooMany), "{err:?}");
    assert!(!dest.exists());
    tidy(&parent);
}

#[test]
fn too_little_free_space_stops_before_anything_is_written() {
    let a = text(1200, "a");
    let (parent, dest) = fresh("ex-space");
    let tight = ExtractCaps {
        min_free_bytes: u64::MAX / 2,
        ..caps()
    };
    let err = extract_targz(
        &archive(&[file("a.txt", &a)]),
        &dest,
        &want(&[("a.txt", &a)]),
        &tight,
    )
    .unwrap_err();
    assert!(matches!(err, ExtractError::NoSpace), "{err:?}");
    assert_eq!(err.code(), "no_space");
    assert!(!dest.exists());
    tidy(&parent);
}

// ---- the destination and the archive itself -------------------------------------------------------------------------------

#[test]
fn an_existing_destination_is_refused_and_left_alone() {
    let a = text(1200, "a");
    let (parent, dest) = fresh("ex-exists");
    std::fs::create_dir(&dest).unwrap();
    std::fs::write(dest.join("keep.txt"), b"mine").unwrap();
    let err = extract_targz(
        &archive(&[file("a.txt", &a)]),
        &dest,
        &want(&[("a.txt", &a)]),
        &caps(),
    )
    .unwrap_err();
    assert!(matches!(err, ExtractError::Exists), "{err:?}");
    assert_eq!(std::fs::read(dest.join("keep.txt")).unwrap(), b"mine");
    tidy(&parent);
}

#[test]
fn bytes_that_are_not_a_gzip_tar_are_a_bad_archive() {
    let (parent, dest) = fresh("ex-bad");
    for junk in [
        b"not a gzip at all".to_vec(),
        Vec::new(),
        gz(b"this is gzip but not a tar"),
    ] {
        let a = text(1200, "a");
        let err = extract_targz(&junk, &dest, &want(&[("a.txt", &a)]), &caps()).unwrap_err();
        assert!(
            matches!(err, ExtractError::BadArchive(_) | ExtractError::Missing(_)),
            "{err:?}"
        );
        assert!(!dest.exists());
    }
    let a = text(1200, "a");
    let mut cut = archive(&[file("a.txt", &a)]);
    cut.truncate(cut.len() / 2);
    let err = extract_targz(&cut, &dest, &want(&[("a.txt", &a)]), &caps()).unwrap_err();
    assert!(matches!(err, ExtractError::BadArchive(_)), "{err:?}");
    tidy(&parent);
}
