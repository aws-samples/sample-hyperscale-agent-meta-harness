//! `ham clone` of the zip the real packager makes: the zip comes out of
//! `delivery::packager::Packager::package` over a real git repository, never out of a helper that lays out entries by hand. A
//! hand-built zip once hid that the packager's `git archive` applied `.gitattributes` conversions (line endings, `ident`, a
//! working-tree encoding) and that its raw copy turned symbolic links into regular files, so `ham clone` refused the zips of
//! repositories with a symlink or an `eol=crlf` rule (Spring Initializr's `*.cmd text eol=crlf`).
#![allow(clippy::unwrap_used)]

mod clonefix;
mod packagerfx;
mod support;

use clonefix::{Item, first_commit, git, item};
use ham::clone::{Git, Request, clone_zip};
use packagerfx::{RUN, deliver};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn req<'a>(zip: &'a Path, dest: &'a Path, cwd: &'a Path, partial: bool) -> Request<'a> {
    Request {
        run_id: RUN,
        zip,
        dest: Some(dest),
        cwd,
        partial,
    }
}

/// UTF-16LE bytes of `s`, as a working tree with `working-tree-encoding=UTF-16LE` holds them.
fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// A second commit with an executable, a symlink to a file, one to a directory, and files that `.gitattributes` converts on
/// export and on checkout.
fn tricky_commit() -> Vec<Item> {
    vec![
        item("run.sh", "#!/bin/sh\npython3 -m pytest -q\n", 0o100_755),
        item("link.py", "calc.py", 0o120_000),
        item("testlink", "tests", 0o120_000),
        item("docs/caf\u{e9}.md", "# Caf\u{e9}\n", 0o100_644),
        item("gradlew.cmd", "@echo off\necho gradle\n", 0o100_644),
        item("notes.txt", "line one\nline two\n", 0o100_644),
        item("version.dat", "$Id$\n", 0o100_644),
        Item {
            path: "wide.utf16".into(),
            content: utf16le("caf\u{e9}\n"),
            mode: 0o100_644,
        },
        item(
            ".gitattributes",
            "*.cmd text eol=crlf\n*.txt text eol=crlf\n*.dat ident\n*.utf16 working-tree-encoding=UTF-16LE\n",
            0o100_644,
        ),
    ]
}

/// The reviewer's finding F1: the repository has a symlink and `eol`, `ident` and encoding rules; the packager's zip has to clone,
/// and the clone is the repository the run built.
#[tokio::test]
async fn the_packagers_zip_of_a_repository_with_symlinks_and_conversion_rules_clones() {
    let d = deliver("pk-tricky", &first_commit(), &tricky_commit()).await;
    let cwd = support::scratch("pk-tricky-cwd");
    let out = clone_zip(&Git::system(), &req(&d.zip, Path::new("out"), &cwd, false))
        .unwrap_or_else(|e| panic!("ham clone refused the packager's zip: {e}"));

    assert_eq!(out.head, d.head);
    assert_eq!(out.top, d.top);
    assert_eq!(out.files, 11);
    let c = &out.dir;
    // The clone is the repository the run built: the same commit, so the same tree, and git calls it clean.
    assert_eq!(git(c, &["rev-parse", "HEAD"]), d.head);
    assert_eq!(
        git(c, &["rev-parse", "HEAD^{tree}"]),
        git(&d.repo, &["rev-parse", "HEAD^{tree}"])
    );
    assert_eq!(git(c, &["status", "--porcelain"]), "");
    assert_eq!(git(c, &["remote"]), "");
    // Symbolic links are symbolic links, the executable is executable.
    assert_eq!(
        std::fs::read_link(c.join("link.py")).unwrap(),
        Path::new("calc.py")
    );
    assert_eq!(
        std::fs::read_link(c.join("testlink")).unwrap(),
        Path::new("tests")
    );
    let mode = std::fs::metadata(c.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "run.sh is executable");
    // The committed blobs are what the zip holds; a checkout converts them as the repository's rules say, and says so.
    assert_eq!(
        std::fs::read(c.join("gradlew.cmd")).unwrap(),
        b"@echo off\r\necho gradle\r\n"
    );
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(
        out.warnings[0].contains(".gitattributes") && out.warnings[0].contains("gradlew.cmd"),
        "{:?}",
        out.warnings
    );
}

/// The control: a repository with no symlink and no rule clones from the packager's zip, with nothing to warn about.
#[tokio::test]
async fn the_packagers_zip_of_an_ordinary_repository_clones_without_a_warning() {
    let second = vec![
        item("run.sh", "#!/bin/sh\npython3 -m pytest -q\n", 0o100_755),
        item("README.md", "# python-calc\n", 0o100_644),
    ];
    let d = deliver("pk-plain", &first_commit(), &second).await;
    let cwd = support::scratch("pk-plain-cwd");
    let out = clone_zip(&Git::system(), &req(&d.zip, Path::new("out"), &cwd, false)).unwrap();
    assert_eq!(out.head, d.head);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(git(&out.dir, &["status", "--porcelain"]), "");
}
