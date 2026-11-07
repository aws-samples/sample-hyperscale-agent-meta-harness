//! `packager`:
//! from the verified bundle it builds `<repo>-<runId>-<sha7>.zip` holding
//! `src/` (the head tree through `git archive`), `repo.bundle` (full history), `EVIDENCE.md`
//! (rendered from what Cline reported), `evidence/` and `MANIFEST.sha256`,
//! uploads it to `runs/<run>/deliverable/` with its SHA-256 in the object metadata, writes the
//! `ART#<artifactId>` item and returns the `delivery.ready` payload.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::error::DeliveryError;
use delivery::files::sha256_hex;
use delivery::git::Git;
use delivery::packager::{PackageRequest, Packager, artifact_id, top_dir};
use harness_protocol::direct::{DirectEvidence, DirectOutcome, DirectRuleRef, HeadSource};
use harness_protocol::store::s3::verified_bundle_key;
use harness_protocol::store::{ArtifactKind, s3};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::PathBuf;

const REPO: &str = "hamh-e2e-python-calc-20260925";
const PR: &str = "https://us-west-2.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc-20260925/pull-requests/1/details";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    base: String,
    head: String,
    p: Packager<MemBlobs, FakeArtifacts>,
}

fn world_with(files: &[(&str, &str)]) -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let (work, shas) = work_repo(&root, "work", &[files]);
    let head = commit(
        &work,
        &[(
            "src/calc.py",
            Some(
                "def add(a, b):\n    return a + b\n\ndef percent_change(a, b):\n    return (b - a) / a * 100\n",
            ),
        )],
        "percent_change",
    );
    world_over(tmp, work, shas[0].clone(), head)
}

/// The world over a work repository whose last commit is `head`: the verified bundle in the fake bucket and a packager.
fn world_over(tmp: tempfile::TempDir, work: PathBuf, base: String, head: String) -> World {
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let f = root.join("verified.bundle");
    verified_bundle(&work, RUN, &f);
    let p = Packager {
        blobs: MemBlobs::default(),
        artifacts: FakeArtifacts::default(),
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: root.join("scratch"),
    };
    p.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    World {
        _tmp: tmp,
        root,
        work,
        base,
        head,
        p,
    }
}

fn world() -> World {
    world_with(&[
        ("src/calc.py", "def add(a, b):\n    return a + b\n"),
        ("tests/test_calc.py", "from src.calc import add\n"),
        ("buildspec.yml", "version: 0.2\n"),
        ("bin/run.sh", "#!/bin/sh\necho run\n"),
    ])
}

fn evidence() -> DirectEvidence {
    DirectEvidence {
        outcome: DirectOutcome::ReportedDone,
        label: DirectOutcome::ReportedDone.label().to_string(),
        detail: None,
        stop_reason: Some("end_turn".into()),
        cline_message: Some("Added percent_change in src/calc.py.".into()),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        engine_version: Some("3.0.71".into()),
        rules: vec![DirectRuleRef {
            name: "00-my-rules.md".into(),
            sha256: "ab".repeat(32),
            bytes: Some(2943),
        }],
        usage: None,
        wall_s: Some(3731),
        head_source: HeadSource::Final,
        final_model_id: None,
        model_switches: 0,
        test_report: None,
        review: None,
        flow: None,
    }
}

fn req(w: &World) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: REPO.into(),
        base_sha: Some(w.base.clone()),
        head_sha: w.head.clone(),
        objective: "Add percent_change(a, b) with tests".into(),
        pr_url: Some(PR.into()),
        direct: evidence(),
    }
}

/// (name, bytes) of every file entry, in zip order.
fn entries(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    let mut out = Vec::new();
    for i in 0..a.len() {
        let mut f = a.by_index(i).unwrap();
        if f.is_dir() {
            continue;
        }
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.push((f.name().unwrap().to_string(), b));
    }
    out
}

fn deliverable(w: &World) -> (String, Vec<u8>) {
    let key = s3::deliverable_key(RUN, REPO, &w.head[..7]);
    (key.clone(), w.p.blobs.get(&key).unwrap())
}

#[tokio::test]
async fn the_zip_holds_src_the_bundle_evidence_md_evidence_and_the_manifest_under_one_top_dir() {
    let w = world();
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/junit/report.xml"),
        b"<testsuite tests=\"4\"/>".to_vec(),
    );
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/tdd/red-green.json"),
        b"{}".to_vec(),
    );
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/goal-log.jsonl"),
        b"{\"turn\":1}\n".to_vec(),
    );
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let top = format!("{REPO}-{RUN}-{}/", &w.head[..7]);
    assert_eq!(top_dir(REPO, RUN, &w.head), top.trim_end_matches('/'));
    let names: Vec<String> = entries(&zip).into_iter().map(|(n, _)| n).collect();
    assert!(names.iter().all(|n| n.starts_with(&top)), "{names:?}");
    let rel: Vec<&str> = names.iter().map(|n| &n[top.len()..]).collect();
    let src: Vec<&str> = rel.iter().filter_map(|r| r.strip_prefix("src/")).collect();
    let tree: Vec<String> = git(&w.work, &["ls-tree", "-r", "--name-only", &w.head])
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(src, tree.iter().map(String::as_str).collect::<Vec<_>>());
    for want in [
        "repo.bundle",
        "EVIDENCE.md",
        "evidence/junit/report.xml",
        "evidence/tdd/red-green.json",
        "evidence/goal-log.jsonl",
    ] {
        assert!(rel.contains(&want), "{want} missing from {rel:?}");
    }
    assert_eq!(rel.last(), Some(&"MANIFEST.sha256"));
}

#[tokio::test]
async fn the_manifest_lists_every_other_file_with_its_sha256_and_src_matches_the_head_blobs() {
    let w = world();
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let files: BTreeMap<String, Vec<u8>> = entries(&zip)
        .into_iter()
        .map(|(n, b)| (n[top.len()..].to_string(), b))
        .collect();
    let manifest = String::from_utf8(files["MANIFEST.sha256"].clone()).unwrap();
    let listed: BTreeMap<String, String> = manifest
        .lines()
        .map(|l| {
            let (h, p) = l.split_once("  ").unwrap();
            (p.to_string(), h.to_string())
        })
        .collect();
    let expected: BTreeMap<String, String> = files
        .iter()
        .filter(|(p, _)| p.as_str() != "MANIFEST.sha256")
        .map(|(p, b)| (p.clone(), sha256_hex(b)))
        .collect();
    assert_eq!(listed, expected);
    // Sorted by path, `sha256sum -c` format.
    let order: Vec<&str> = manifest
        .lines()
        .map(|l| l.split_once("  ").unwrap().1)
        .collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
    for (p, h) in &listed {
        if let Some(path) = p.strip_prefix("src/") {
            let blob = std::process::Command::new("git")
                .current_dir(&w.work)
                .args(["cat-file", "blob", &format!("{}:{path}", w.head)])
                .output()
                .unwrap()
                .stdout;
            assert_eq!(h, &sha256_hex(&blob), "{p}");
        }
    }
}

#[tokio::test]
async fn src_is_the_head_tree_verbatim_even_with_export_attributes() {
    let w = world_with(&[
        ("src/calc.py", "x = 1\n"),
        (
            ".gitattributes",
            "src/hidden.py export-ignore\nsrc/subst.py export-subst\n",
        ),
        ("src/hidden.py", "hidden = True\n"),
        ("src/subst.py", "VERSION = '$Format:%H$'\n"),
    ]);
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let files: BTreeMap<String, Vec<u8>> = entries(&zip)
        .into_iter()
        .map(|(n, b)| (n[top.len()..].to_string(), b))
        .collect();
    let get = |k: &str| files.get(k).map(|b| String::from_utf8_lossy(b).to_string());
    assert_eq!(
        get("src/src/hidden.py").as_deref(),
        Some("hidden = True\n"),
        "{:?}",
        files.keys()
    );
    assert_eq!(
        get("src/src/subst.py").as_deref(),
        Some("VERSION = '$Format:%H$'\n"),
        "{:?}",
        files.keys()
    );
}

/// A head tree that a plain `git archive` does not copy verbatim: a symlink to a file and one to a directory, an executable, and
/// files that in-tree attributes convert on export (CRLF line endings, `$Id$` expansion, a UTF-16 working-tree encoding).
fn world_converting() -> World {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let (work, shas) = work_repo(&root, "work", &[&[("README.md", "# fixture\n")]]);
    write_files(
        &work,
        &[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            ("bin/run.sh", "#!/bin/sh\necho run\n"),
            ("run.cmd", "@echo off\necho run\n"),
            ("version.dat", "$Id$\n"),
            (
                ".gitattributes",
                "*.cmd text eol=crlf\n*.dat ident\n*.utf16 working-tree-encoding=UTF-16LE\n",
            ),
        ],
    );
    let wide: Vec<u8> = "caf\u{e9}\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    std::fs::write(work.join("wide.utf16"), wide).unwrap();
    std::fs::set_permissions(
        work.join("bin/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("src/calc.py", work.join("link.py")).unwrap();
    symlink("src", work.join("srclink")).unwrap();
    symlink("README.md", work.join("lisez-moi-caf\u{e9}")).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "converting tree"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    world_over(tmp, work, shas[0].clone(), head)
}

/// What a zip says of one file entry under `<top>/src/`: its bytes, Unix mode and modification time.
type SrcEntry = (Vec<u8>, Option<u32>, Option<(u16, u8, u8, u8, u8, u8)>);

/// Every file entry under `<top>/src/` of the zip, by path below `src/`.
fn src_entries(zip: &[u8], w: &World) -> BTreeMap<String, SrcEntry> {
    let prefix = top_dir(REPO, RUN, &w.head) + "/src/";
    let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..a.len() {
        let mut f = a.by_index(i).unwrap();
        if f.is_dir() {
            continue;
        }
        let Some(rel) = f.name().unwrap().strip_prefix(&prefix).map(str::to_string) else {
            continue;
        };
        let mode = f.unix_mode();
        let time = f.last_modified().map(|t| {
            (
                t.year(),
                t.month(),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
            )
        });
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.insert(rel, (b, mode, time));
    }
    out
}

/// Git's blob id of `bytes`, from git itself (no filters).
fn blob_id(w: &World, bytes: &[u8]) -> String {
    use std::io::Write;
    let mut c = std::process::Command::new("git")
        .current_dir(&w.work)
        .args(["hash-object", "--no-filters", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(bytes).unwrap();
    let out = c.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// The zip's `src/` equals `git ls-tree -r` of the head: the same paths, each with the tree's blob id and mode.
fn assert_src_is_the_head_tree(w: &World, zip: &[u8]) {
    let in_zip = src_entries(zip, w);
    let mut in_tree = BTreeMap::new();
    for line in git(&w.work, &["ls-tree", "-r", "-z", &w.head])
        .split('\0')
        .filter(|l| !l.is_empty())
    {
        let (meta, path) = line.split_once('\t').unwrap();
        let mut m = meta.split(' ');
        let (mode, kind, id) = (m.next().unwrap(), m.next().unwrap(), m.next().unwrap());
        if kind == "blob" {
            in_tree.insert(path.to_string(), (mode.to_string(), id.to_string()));
        }
    }
    let mut problems = Vec::new();
    for (path, (bytes, unix, _)) in &in_zip {
        let mode = match unix.unwrap_or(0o100_644) {
            m if m & 0o170_000 == 0o120_000 => "120000",
            m if m & 0o111 != 0 => "100755",
            _ => "100644",
        };
        match in_tree.get(path) {
            None => problems.push(format!("{path}: in the zip, not in the tree")),
            Some((tree_mode, id)) => {
                if tree_mode != mode {
                    problems.push(format!(
                        "{path}: mode {mode} in the zip, {tree_mode} in the tree"
                    ));
                }
                let zip_id = blob_id(w, bytes);
                if &zip_id != id {
                    problems.push(format!(
                        "{path}: blob {zip_id} in the zip, {id} in the tree"
                    ));
                }
            }
        }
    }
    for path in in_tree.keys().filter(|p| !in_zip.contains_key(*p)) {
        problems.push(format!("{path}: in the tree, not in the zip"));
    }
    assert!(
        problems.is_empty(),
        "src/ is not the head tree: {problems:#?}"
    );
}

#[tokio::test]
async fn src_holds_every_path_of_the_head_with_its_blob_and_mode_symlinks_and_conversions_included()
{
    let w = world_converting();
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    assert_src_is_the_head_tree(&w, &zip);
}

#[tokio::test]
async fn a_symlink_stays_a_symlink_entry_that_holds_its_target() {
    let w = world_converting();
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let src = src_entries(&zip, &w);
    let commit_time = src["src/calc.py"].2;
    assert!(commit_time.is_some());
    for (path, target) in [
        ("link.py", "src/calc.py"),
        ("srclink", "src"),
        ("lisez-moi-caf\u{e9}", "README.md"),
    ] {
        let (bytes, mode, time) = &src[path];
        assert_eq!(bytes, target.as_bytes(), "{path}");
        assert_eq!(
            mode.map(|m| m & 0o170_000),
            Some(0o120_000),
            "{path}: {mode:?}"
        );
        assert_eq!(
            *time, commit_time,
            "{path}: a link carries the commit time like every entry"
        );
    }
    // A regular file keeps being a regular file, an executable stays executable. `git archive` writes no Unix attributes for a plain
    // file and the zip crate (9.x) copies an entry's attributes as they are, so a plain file has no mode here; a missing mode reads as
    // 100644, as `ham clone` reads it.
    assert_eq!(
        src["src/calc.py"].1.unwrap_or(0o100_644) & 0o170_000,
        0o100_000
    );
    assert_eq!(src["bin/run.sh"].1.map(|m| m & 0o777), Some(0o755));
    // The manifest covers the link text, as it covers the bytes of every other entry.
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let files: BTreeMap<String, Vec<u8>> = entries(&zip)
        .into_iter()
        .map(|(n, b)| (n[top.len()..].to_string(), b))
        .collect();
    let manifest = String::from_utf8(files["MANIFEST.sha256"].clone()).unwrap();
    assert!(
        manifest.contains(&format!("{}  src/link.py\n", sha256_hex(b"src/calc.py"))),
        "{manifest}"
    );
}

#[tokio::test]
async fn in_tree_eol_ident_and_encoding_attributes_do_not_change_the_bytes_of_src() {
    let w = world_converting();
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let src = src_entries(&zip, &w);
    assert_eq!(src["run.cmd"].0, b"@echo off\necho run\n", "eol=crlf");
    assert_eq!(src["version.dat"].0, b"$Id$\n", "ident");
    assert_eq!(
        src["wide.utf16"].0,
        "caf\u{e9}\n".as_bytes(),
        "working-tree-encoding"
    );
}

/// For a tree with no symlink and no attribute, `src/` keeps what a plain `git archive` writes: the entry names, bytes, times
/// and execute bits, in the same order. (The mode type bits are not compared here; the symbolic link case above pins those of
/// links, and the head-tree case pins the git mode of every path.)
#[tokio::test]
async fn src_of_an_ordinary_tree_keeps_git_archives_names_bytes_times_and_execute_bits() {
    let w = world();
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let top = top_dir(REPO, RUN, &w.head);
    let plain_path = w.root.join("plain.zip");
    git(
        &w.work,
        &[
            "archive",
            "--format=zip",
            &format!("--prefix={top}/src/"),
            &format!("--output={}", plain_path.display()),
            &w.head,
        ],
    );
    let facts = |bytes: &[u8]| {
        let mut a = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        let mut v = Vec::new();
        for i in 0..a.len() {
            let mut f = a.by_index(i).unwrap();
            if !f.name().unwrap().starts_with(&format!("{top}/src/")) {
                continue;
            }
            let t = f.last_modified().map(|t| {
                (
                    t.year(),
                    t.month(),
                    t.day(),
                    t.hour(),
                    t.minute(),
                    t.second(),
                )
            });
            let exec = !f.is_dir() && f.unix_mode().is_some_and(|m| m & 0o111 != 0);
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            v.push((f.name().unwrap().to_string(), b, exec, t));
        }
        v
    };
    let plain = facts(&std::fs::read(&plain_path).unwrap());
    assert!(plain.len() > 3, "{plain:?}");
    assert_eq!(facts(&zip), plain);
}

/// A zip entry holds a link target as text, so a target that is not UTF-8 cannot be written verbatim: the package is refused with the
/// link's name (a flattened regular file would make `src/` differ from the head without saying so).
#[tokio::test]
async fn a_symlink_whose_target_is_not_utf_8_is_refused_by_name() {
    use std::os::unix::ffi::OsStrExt;
    let w = world();
    std::os::unix::fs::symlink(
        std::ffi::OsStr::from_bytes(b"caf\xe9.txt"),
        w.work.join("odd-link"),
    )
    .unwrap();
    let head = commit(&w.work, &[("src/more.py", Some("x = 1\n"))], "an odd link");
    let f = w.root.join("verified-odd.bundle");
    verified_bundle(&w.work, RUN, &f);
    w.p.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    let mut r = req(&w);
    r.head_sha = head.clone();
    let err = w.p.package(&r).await.unwrap_err();
    assert!(
        matches!(err, DeliveryError::Invalid(_))
            && err.to_string().contains("odd-link")
            && err.to_string().contains("not UTF-8"),
        "{err:?}"
    );
    assert!(
        w.p.blobs
            .keys()
            .iter()
            .all(|k| !k.contains("/deliverable/") || k.ends_with("verified.bundle")),
        "nothing is published: {:?}",
        w.p.blobs.keys()
    );
}

#[tokio::test]
async fn the_repo_bundle_has_the_full_history_of_the_harness_branch() {
    let w = world();
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let bundle = entries(&zip)
        .into_iter()
        .find(|(n, _)| n == &format!("{top}repo.bundle"))
        .unwrap()
        .1;
    let f = w.root.join("out.bundle");
    std::fs::write(&f, bundle).unwrap();
    let empty = w.root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    let verify = std::process::Command::new("git")
        .current_dir(&empty)
        .args(["bundle", "verify", f.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(verify.status.success());
    let said = String::from_utf8_lossy(&verify.stdout).to_string()
        + &String::from_utf8_lossy(&verify.stderr);
    assert!(said.contains("complete history"), "{said}");
    let heads = git(&empty, &["bundle", "list-heads", f.to_str().unwrap()]);
    assert!(
        heads.contains(&format!("{} refs/heads/harness/{RUN}", w.head)),
        "{heads}"
    );
    assert!(heads.contains(&format!("{} HEAD", w.head)), "{heads}");
}

#[tokio::test]
async fn the_object_metadata_carries_the_zip_sha256_and_the_artifact_item_records_it() {
    let w = world();
    let out = w.p.package(&req(&w)).await.unwrap();
    let (key, zip) = deliverable(&w);
    let meta = w.p.blobs.meta(&key).unwrap();
    assert_eq!(meta.content_type, "application/zip");
    assert_eq!(meta.sha256_hex, sha256_hex(&zip));
    assert_eq!(out.sha256, sha256_hex(&zip));
    assert_eq!(out.size_bytes, zip.len() as u64);
    assert_eq!(out.pr_url.as_deref(), Some(PR));
    assert_eq!(out.artifact_id, artifact_id(&out.sha256));
    assert_eq!(out.artifact_id, format!("zip-{}", &out.sha256[..20]));
    let items = w.p.artifacts.items.lock().unwrap().clone();
    let kinds: Vec<ArtifactKind> = items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, [ArtifactKind::Zip, ArtifactKind::Evidence]);
    let a = &items[0];
    assert_eq!(a.run_id, RUN);
    assert_eq!(a.artifact_id, out.artifact_id);
    assert_eq!(a.kind, ArtifactKind::Zip);
    assert_eq!(a.s3_uri, format!("s3://{BUCKET}/{key}"));
    assert_eq!(a.sha256, out.sha256);
    assert_eq!(a.size_bytes, out.size_bytes);
    assert_eq!(
        key,
        format!("runs/{RUN}/deliverable/{REPO}-{RUN}-{}.zip", &w.head[..7])
    );
}

#[tokio::test]
async fn a_rerun_builds_the_same_zip_and_writes_no_second_artifact() {
    let w = world();
    let first = w.p.package(&req(&w)).await.unwrap();
    let second = w.p.package(&req(&w)).await.unwrap();
    assert_eq!(first, second);
    // The zip and the evidence archive, once each.
    assert_eq!(w.p.artifacts.items.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn inputs_that_do_not_describe_the_bundle_are_invalid() {
    let w = world();
    let mut bad_repo = req(&w);
    bad_repo.repo = "../x".into();
    assert!(matches!(
        w.p.package(&bad_repo).await,
        Err(DeliveryError::Invalid(_))
    ));
    let mut wrong = req(&w);
    wrong.head_sha = w.base.clone();
    assert!(
        matches!(w.p.package(&wrong).await, Err(DeliveryError::Invalid(m)) if m.contains("verified bundle"))
    );
    let mut bad_pr = req(&w);
    bad_pr.pr_url = Some("javascript:alert(1)".into());
    assert!(matches!(
        w.p.package(&bad_pr).await,
        Err(DeliveryError::Invalid(_))
    ));
    assert!(w.p.artifacts.items.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_state_machine_input_parses_with_defaults() {
    let w = world();
    let parsed: PackageRequest = serde_json::from_value(json!({
        "runId": RUN, "repo": REPO, "headSha": w.head, "objective": "x",
        "direct": serde_json::to_value(evidence()).unwrap()
    }))
    .unwrap();
    assert!(parsed.pr_url.is_none() && parsed.base_sha.is_none());
    let out = w.p.package(&parsed).await.unwrap();
    assert!(out.pr_url.is_none());
    // A package without what Cline reported is not a package.
    assert!(
        serde_json::from_value::<PackageRequest>(json!({
            "runId": RUN, "repo": REPO, "headSha": w.head, "objective": "x"
        }))
        .is_err()
    );
}

#[tokio::test]
async fn unsafe_evidence_object_names_are_skipped() {
    let w = world();
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/../../escape.txt"),
        b"x".to_vec(),
    );
    w.p.blobs
        .insert(&format!("runs/{RUN}/evidence/dir/"), Vec::new());
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/direct.json"),
        br#"{"ok":true}"#.to_vec(),
    );
    w.p.package(&req(&w)).await.unwrap();
    let (_, zip) = deliverable(&w);
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let files: BTreeMap<String, Vec<u8>> = entries(&zip)
        .into_iter()
        .map(|(n, b)| (n[top.len()..].to_string(), b))
        .collect();
    assert_eq!(files["evidence/direct.json"], br#"{"ok":true}"#);
    assert!(
        files
            .keys()
            .all(|k| !k.contains("..") && !k.ends_with("escape.txt")),
        "{:?}",
        files.keys()
    );
}

#[tokio::test]
async fn the_evidence_archive_is_the_zip_without_src_and_the_bundle() {
    let w = world();
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/junit/report.xml"),
        b"<testsuite tests=\"4\"/>".to_vec(),
    );
    w.p.package(&req(&w)).await.unwrap();
    let key = s3::evidence_archive_key(RUN, REPO, &w.head[..7]);
    let archive = w.p.blobs.get(&key).expect("the evidence archive");
    let top = format!("{REPO}-{RUN}-{}-evidence/", &w.head[..7]);
    let names: Vec<String> = entries(&archive).into_iter().map(|(n, _)| n).collect();
    assert!(names.iter().all(|n| n.starts_with(&top)), "{names:?}");
    let rel: Vec<&str> = names.iter().map(|n| &n[top.len()..]).collect();
    assert!(
        rel.contains(&"EVIDENCE.md") && rel.contains(&"evidence/junit/report.xml"),
        "{rel:?}"
    );
    assert!(
        !rel.iter()
            .any(|r| r.starts_with("src/") || *r == "repo.bundle"),
        "{rel:?}"
    );
    assert_eq!(rel.last(), Some(&"MANIFEST.sha256"));
    assert_eq!(
        w.p.blobs.meta(&key).unwrap().sha256_hex,
        sha256_hex(&archive)
    );
    let items = w.p.artifacts.items.lock().unwrap().clone();
    let ev = items
        .iter()
        .find(|i| i.kind == ArtifactKind::Evidence)
        .unwrap();
    assert_eq!(ev.s3_uri, format!("s3://{BUCKET}/{key}"));
    assert_eq!(ev.sha256, sha256_hex(&archive));
    assert_eq!(ev.artifact_id, format!("evidence-{}", &ev.sha256[..20]));
}
