//! `ham pull <runId> --snapshot` for a copy of the work that needs the repository the run started from.
//!
//! A copy may hold what the run changed and leave out what the bundle the run started from holds. The client says that it takes such
//! a copy in two parts (`?parts=1`), the route then names both, and `ham` keeps both files: the copy under its usual name and the
//! base as `<runId>-base.bundle`. It runs no git and builds nothing; its text names both files and the line that puts them together.
//! What this file pins:
//!
//! * through the compiled binary and the stand-in `/v1` API (the real control-api handler): both files are on disk, the text and the
//!   `--json` object say what each is, the printed line gives a checkout at the copy's commit, and `ham` itself started no git;
//! * a base that cannot be downloaded or is not what the route said leaves the copy on disk, and the command says where it is.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::{Api, Retry};
use ham::chat_routes::{SnapshotFetchError, fetch_snapshot_with};
use ham::http::{ControlPlane, Response, TransportError};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use home::{Home, home, text};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Fake, REGION, creds};

/// Runs the host's git in `dir` with a fixed identity and no configuration of the machine; returns its trimmed output.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with a base commit and one commit of work on it, as two bundles: the one a run starts from (`HEAD` and its branch,
/// as the service stages it) and a copy of the work that leaves out what the base holds.
struct Work {
    base_bytes: Vec<u8>,
    copy_bytes: Vec<u8>,
    base_commit: String,
    copy_commit: String,
}

fn work(label: &str) -> Work {
    work_on(label, "main")
}

/// [`work`] in a repository whose own branch is `branch`.
fn work_on(label: &str, branch: &str) -> Work {
    let root = support::scratch(label);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", branch]);
    std::fs::write(repo.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base_commit = git(&repo, &["rev-parse", "HEAD"]);
    let base = root.join("base.bundle");
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            base.to_str().unwrap(),
            "HEAD",
            &format!("refs/heads/{branch}"),
        ],
    );
    std::fs::write(
        repo.join("calc.py"),
        "def add(a, b):\n    return a + b\n\n\ndef sub(a, b):\n    return a - b\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "harness: snapshot 1"]);
    let copy_commit = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["update-ref", "refs/heads/snapshot", &copy_commit]);
    let copy = root.join("copy.bundle");
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            "--version=2",
            copy.to_str().unwrap(),
            "refs/heads/snapshot",
            &format!("^{base_commit}"),
        ],
    );
    Work {
        base_bytes: std::fs::read(&base).unwrap(),
        copy_bytes: std::fs::read(&copy).unwrap(),
        base_commit,
        copy_commit,
    }
}

/// Runs a printed look line the way a person pastes it: as one command line of the shell, with `<dir>` named.
fn paste(look: &str, dir: &Path, cwd: &Path) {
    let line = look
        .trim_start_matches("Look")
        .trim()
        .replace("<dir>", dir.to_str().unwrap());
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&line)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{line}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
}

async fn start(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    let key: String = prompt.chars().filter(char::is_ascii_alphanumeric).collect();
    api(fake)
        .start_direct_run(&req, &format!("k-{key}"))
        .await
        .unwrap()
        .run_id
}

/// Puts the copy and its record under the run's turn prefix, as the worker does, and the base where the service stages it.
fn save(fake: &Fake, run: &str, w: &Work, base: Option<&[u8]>, count: u32) {
    let prefix = harness_protocol::direct::s3::turn_prefix(run);
    fake.mem
        .put_object(&format!("{prefix}snapshot.bundle"), w.copy_bytes.clone());
    fake.mem.put_object(
        &format!("{prefix}snapshot.json"),
        json!({"schemaVersion": 1, "headSha": w.copy_commit, "baseSha": w.base_commit, "at": "2026-11-17T13:10:00Z", "count": count})
            .to_string()
            .into_bytes(),
    );
    if let Some(bytes) = base {
        fake.mem.put_object(
            &harness_protocol::direct::s3::repo_bundle_key(run),
            bytes.to_vec(),
        );
    }
}

struct Printed {
    code: i32,
    out: String,
    err: String,
}

/// Runs the compiled `ham` with a `git` stand-in first on its `PATH` that leaves a mark when it is started: `ham` must not start it.
async fn shell(h: &Home, args: &[&str]) -> (Printed, bool) {
    use std::os::unix::fs::PermissionsExt;
    let bin = h.root.join("stand-in-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mark = h.root.join("git-was-started");
    let script = bin.join("git");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho started >> {}\nexit 1\n", mark.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = home::command(h, args, &[]);
    cmd.env(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    let o = cmd.output().await.unwrap();
    (
        Printed {
            code: o.status.code().unwrap(),
            out: text(&o.stdout),
            err: text(&o.stderr),
        },
        mark.exists(),
    )
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn b860_pull_keeps_the_copy_and_the_base() {
    let w = work("b860-pull-parts-work");
    println!(
        "B860 TRIGGER PRESENT: the copy's header names the base commit as a commit it needs: {}",
        String::from_utf8_lossy(&w.copy_bytes[..200]).contains(&format!("-{}", w.base_commit))
    );
    assert!(String::from_utf8_lossy(&w.copy_bytes[..200]).contains(&format!("-{}", w.base_commit)));
    let fake = Fake::start().await;
    let run = start(&fake, "Add a sub function").await;
    save(&fake, &run, &w, Some(&w.base_bytes), 4);
    let h = home(&fake, "b860-pull-parts");
    let (p, git_started) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    // Both files are on disk under their names, each the bytes the store holds, and no part file is left.
    let copy_path = h.downloads.join(format!("{run}-snapshot-4.bundle"));
    let base_path = h.downloads.join(format!("{run}-base.bundle"));
    assert_eq!(
        files_in(&h.downloads),
        vec![
            format!("{run}-base.bundle"),
            format!("{run}-snapshot-4.bundle")
        ]
    );
    assert_eq!(std::fs::read(&copy_path).unwrap(), w.copy_bytes);
    assert_eq!(std::fs::read(&base_path).unwrap(), w.base_bytes);
    // The request said that this client takes a copy in two parts.
    let asked = fake
        .mem
        .posts(&format!("/v1/runs/{run}/snapshot/download-url"));
    assert!(
        !asked.is_empty() && asked.iter().all(|s| s.query == "parts=1"),
        "{:?}",
        asked.iter().map(|s| s.query.clone()).collect::<Vec<_>>()
    );
    // The text: the two files, the two sizes and the line that puts them together, word for word.
    let lines: Vec<&str> = p.out.lines().collect();
    let file_at = lines
        .iter()
        .position(|l| *l == format!("File      {}", copy_path.display()))
        .unwrap_or_else(|| panic!("no File line: {}", p.out));
    assert_eq!(
        lines[file_at + 1],
        format!(
            "Needs     {} (the repository the run started from)",
            base_path.display()
        ),
        "{}",
        p.out
    );
    assert!(
        lines.contains(
            &format!(
                "Size      {} bytes, and {} bytes for the repository the run started from",
                w.copy_bytes.len(),
                w.base_bytes.len()
            )
            .as_str()
        ),
        "{}",
        p.out
    );
    let look = format!(
        "Look      git clone {base} <dir> && git -C <dir> fetch {copy} snapshot && git -C <dir> checkout -B ham-snapshot FETCH_HEAD",
        base = base_path.display(),
        copy = copy_path.display()
    );
    assert!(lines.contains(&look.as_str()), "{}", p.out);
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("Look ")).count(),
        1,
        "{}",
        p.out
    );
    assert!(
        p.out.contains(ham_core::snapshot::SNAPSHOT_TRUST_NOTE),
        "{}",
        p.out
    );
    assert!(!p.out.contains("X-Amz-"), "no link is printed: {}", p.out);
    // `ham` ran no git: the two files are kept as they came.
    assert!(!git_started, "ham started git");

    // The printed line, run with git, gives a checkout at the copy's commit with the work in it.
    let dir = h.root.join("looked");
    paste(&look, &dir, &h.root);
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), w.copy_commit);
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
    assert!(
        std::fs::read_to_string(dir.join("calc.py"))
            .unwrap()
            .contains("def sub")
    );

    // `--json`: the copy as before, and where the base is and its size.
    let (p, git_started) = shell(&h, &["pull", &run, "--snapshot", "--json"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    let v: Value = serde_json::from_str(p.out.trim()).unwrap();
    assert_eq!(v["snapshot"]["path"], copy_path.display().to_string());
    assert_eq!(v["snapshot"]["headSha"], w.copy_commit.as_str());
    assert_eq!(v["snapshot"]["sizeBytes"], w.copy_bytes.len());
    assert_eq!(
        v["snapshot"]["base"],
        json!({"path": base_path.display().to_string(), "sizeBytes": w.base_bytes.len()})
    );
    assert!(v["snapshot"].get("needsBase").is_none(), "{v}");
    assert!(!git_started, "ham started git");
    // A second pull replaces the base file and leaves no part file.
    assert_eq!(
        files_in(&h.downloads),
        vec![
            format!("{run}-base.bundle"),
            format!("{run}-snapshot-4.bundle")
        ]
    );
}

// ---- a scripted plane with two objects --------------------------------------------------------------------------------

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";

/// Answers the download route with one JSON object and serves the copy and the base by the key in the URL, with S3's range rules.
struct Two {
    answer: Value,
    copy: Vec<u8>,
    base: Vec<u8>,
    /// The status every fetch of the base answers with instead of its bytes.
    base_status: Option<u16>,
    calls: Mutex<Vec<String>>,
    /// How many fetches of the base still answer 403, as S3 does for a link that expired, before the bytes come.
    base_expired: Mutex<u32>,
    /// What the route answers from its second call on; `None`: the same answer every time.
    later: Option<Value>,
}

impl ControlPlane for Two {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(path.to_string());
        let answer = match &self.later {
            Some(later) if calls.len() > 1 => later,
            _ => &self.answer,
        };
        Ok(Response {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(answer).unwrap(),
        })
    }
    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        let is_base = url.contains("input/repo.bundle");
        if let (true, Some(status)) = (is_base, self.base_status) {
            return Ok(Response {
                status,
                headers: BTreeMap::new(),
                body: b"<Error/>".to_vec(),
            });
        }
        if is_base {
            let mut expired = self.base_expired.lock().unwrap();
            if *expired > 0 {
                *expired -= 1;
                return Ok(Response {
                    status: 403,
                    headers: BTreeMap::new(),
                    body: b"<Error><Code>AccessDenied</Code></Error>".to_vec(),
                });
            }
        }
        let object = if is_base { &self.base } else { &self.copy };
        let size = object.len() as u64;
        let (s, e) = range.unwrap_or((0, size - 1));
        let e = e.min(size - 1);
        Ok(Response {
            status: 206,
            headers: BTreeMap::from([(
                "content-range".to_string(),
                format!("bytes {s}-{e}/{size}"),
            )]),
            body: object[usize::try_from(s).unwrap()..=usize::try_from(e).unwrap()].to_vec(),
        })
    }
}

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

fn thin_copy() -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base\n{HEAD} refs/heads/snapshot\n\nPACK").into_bytes();
    b.extend((0..3_000).map(|i| (i % 251) as u8));
    b
}

fn whole_base() -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n{BASE} HEAD\n{BASE} refs/heads/main\n\nPACK").into_bytes();
    b.extend((0..9_000).map(|i| (i % 241) as u8));
    b
}

fn answer(copy: &[u8], base_size: u64) -> Value {
    json!({
        "url": "https://bucket.s3.us-east-1.amazonaws.com/runs/x/agents/direct/turns/1/snapshot.bundle?X-Amz-Signature=deadbeef&X-Amz-Security-Token=SECRETTOKEN",
        "expiresAt": "2099-10-15T13:25:00Z", "headSha": HEAD, "baseSha": BASE, "takenAt": "2026-11-17T13:10:00Z",
        "count": 3, "sizeBytes": copy.len(), "etag": "\"abc\"", "runStatus": "active",
        "base": {
            "url": "https://bucket.s3.us-east-1.amazonaws.com/runs/x/input/repo.bundle?X-Amz-Signature=cafecafe&X-Amz-Security-Token=SECRETTOKEN",
            "sizeBytes": base_size, "etag": "\"def\""
        }
    })
}

fn scripted(plane: Two) -> Api<Two> {
    Api::new(Arc::new(plane)).with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(5),
    })
}

/// The copy is on disk, the base is not, no part file is left, and the error is the sentence that says where the copy is.
fn assert_copy_kept(dir: &Path, e: &SnapshotFetchError, reason_holds: &str) -> PathBuf {
    let copy_path = dir.join(format!("{RUN}-snapshot-3.bundle"));
    assert_eq!(files_in(dir), vec![format!("{RUN}-snapshot-3.bundle")]);
    assert_eq!(std::fs::read(&copy_path).unwrap(), thin_copy());
    let shown = e.to_string();
    let head = format!(
        "the copy is at {}; the repository the run started from could not be downloaded (",
        copy_path.display()
    );
    assert!(shown.starts_with(&head), "{shown}");
    assert!(shown.ends_with("); run the command again"), "{shown}");
    assert!(shown.contains(reason_holds), "{shown}");
    assert!(
        !shown.contains("X-Amz-") && !shown.contains("SECRETTOKEN") && !shown.contains("cafecafe"),
        "no link in an error: {shown}"
    );
    copy_path
}

#[tokio::test]
async fn b860_pull_keeps_the_copy_when_the_base_fails() {
    // The download of the base answers 500, every time.
    let dir = support::scratch("b860-base-500");
    let api = scripted(Two {
        answer: answer(&thin_copy(), whole_base().len() as u64),
        copy: thin_copy(),
        base: whole_base(),
        base_status: Some(500),
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(0),
        later: None,
    });
    let e = fetch_snapshot_with(&api, RUN, &dir, 1024)
        .await
        .unwrap_err();
    println!("B860 TRIGGER PRESENT: the answer names a base, and its download answers 500: {e}");
    assert_copy_kept(&dir, &e, "HTTP 500");

    // The base has another size than the route said.
    let dir = support::scratch("b860-base-size");
    let api = scripted(Two {
        answer: answer(&thin_copy(), whole_base().len() as u64 + 7),
        copy: thin_copy(),
        base: whole_base(),
        base_status: None,
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(0),
        later: None,
    });
    let e = fetch_snapshot_with(&api, RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert_copy_kept(&dir, &e, "bytes");

    // The base is a bundle that itself needs commits: it would not clone alone, so it is not kept as the base.
    let dir = support::scratch("b860-base-thin");
    let api = scripted(Two {
        answer: answer(&thin_copy(), thin_copy().len() as u64),
        copy: thin_copy(),
        base: thin_copy(),
        base_status: None,
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(0),
        later: None,
    });
    let e = fetch_snapshot_with(&api, RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert_copy_kept(&dir, &e, "not a bundle that clones alone");

    // A base address that is no S3 address is never fetched.
    let dir = support::scratch("b860-base-host");
    let mut a = answer(&thin_copy(), whole_base().len() as u64);
    a["base"]["url"] =
        json!("https://other.example.com/runs/x/input/repo.bundle?X-Amz-Signature=cafecafe");
    let api = scripted(Two {
        answer: a,
        copy: thin_copy(),
        base: whole_base(),
        base_status: None,
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(0),
        later: None,
    });
    let e = fetch_snapshot_with(&api, RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert_copy_kept(&dir, &e, "not an S3 address");

    // Through the compiled binary: the stage holds a base that is no bundle. The copy's file is on disk, the exit code is 1 and the
    // sentence is on standard error.
    let w = work("b860-pull-base-fails-work");
    let fake = Fake::start().await;
    let run = start(&fake, "Add a mul function").await;
    save(&fake, &run, &w, Some(b"PK\x03\x04 this is no bundle"), 2);
    let h = home(&fake, "b860-pull-base-fails");
    let (p, _) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(p.code, 1, "{} {}", p.out, p.err);
    let copy_path = h.downloads.join(format!("{run}-snapshot-2.bundle"));
    assert_eq!(
        files_in(&h.downloads),
        vec![format!("{run}-snapshot-2.bundle")]
    );
    assert_eq!(std::fs::read(&copy_path).unwrap(), w.copy_bytes);
    let said = p.err.trim();
    assert!(
        said.starts_with(&format!(
            "ham: the copy is at {}; the repository the run started from could not be downloaded (",
            copy_path.display()
        )) && said.ends_with("); run the command again"),
        "{said}"
    );
    assert!(p.out.trim().is_empty(), "{}", p.out);
}

/// The look line of a copy in two files, pasted as it is printed, for the two cases a plain line fails in: a repository whose own
/// branch is named like the copy's ref, and a download folder with a blank in its path.
#[test]
fn b860_the_look_line_works_for_a_branch_named_snapshot_and_a_folder_with_a_blank() {
    let w = work_on("b860-look-line-work", "snapshot");
    let root = support::scratch("b860-look-line");
    let downloads = root.join("my downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let (copy_path, base_path) = (
        downloads.join(format!("{RUN}-snapshot-1.bundle")),
        downloads.join(format!("{RUN}-base.bundle")),
    );
    std::fs::write(&copy_path, &w.copy_bytes).unwrap();
    std::fs::write(&base_path, &w.base_bytes).unwrap();
    println!(
        "B860 TRIGGER PRESENT: the base's own branch is named snapshot, and the folder of the two files has a blank in its path: {}",
        git(
            &root,
            &["bundle", "list-heads", base_path.to_str().unwrap()]
        )
    );
    assert!(
        git(
            &root,
            &["bundle", "list-heads", base_path.to_str().unwrap()]
        )
        .contains("refs/heads/snapshot")
    );
    let fetched = ham::chat_routes::SnapshotFetched {
        path: copy_path.clone(),
        head_sha: w.copy_commit.clone(),
        base_sha: Some(w.base_commit.clone()),
        taken_at: "2026-11-17T13:10:00Z".to_string(),
        count: Some(1),
        size_bytes: w.copy_bytes.len() as u64,
        run_status: harness_protocol::goal::GoalStatus::Active,
        record_stale: false,
        final_copy: false,
        engine: None,
        read_back_from: None,
        asked_read_back: false,
        base: Some((base_path.clone(), w.base_bytes.len() as u64)),
        needs_base: false,
    };
    let text = ham::snapshot_cmd::text(RUN, &fetched);
    let look = text
        .lines()
        .find(|l| l.starts_with("Look "))
        .unwrap_or_else(|| panic!("no Look line: {text}"));
    // A path with a blank is one word of the line: it is printed in single quotes.
    assert_eq!(
        look,
        format!(
            "Look      git clone '{base}' <dir> && git -C <dir> fetch '{copy}' snapshot && git -C <dir> checkout -B ham-snapshot FETCH_HEAD",
            base = base_path.display(),
            copy = copy_path.display()
        )
    );
    let dir = root.join("looked");
    paste(look, &dir, &root);
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), w.copy_commit);
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
    assert!(
        std::fs::read_to_string(dir.join("calc.py"))
            .unwrap()
            .contains("def sub")
    );

    // A copy that clones alone, in the same folder: its one clone line names the file as one word too.
    let whole = ham::chat_routes::SnapshotFetched {
        base: None,
        ..fetched.clone()
    };
    let text = ham::snapshot_cmd::text(RUN, &whole);
    assert!(
        text.lines().any(|l| l
            == format!(
                "Look      git clone -b snapshot '{}' <dir>",
                copy_path.display()
            )),
        "{text}"
    );
    // A single quote inside a path ends the quoted word, is written on its own and opens the word again.
    let odd = ham::chat_routes::SnapshotFetched {
        path: PathBuf::from("/srv/it's here/copy.bundle"),
        base: None,
        ..fetched
    };
    let text = ham::snapshot_cmd::text(RUN, &odd);
    assert!(
        text.lines()
            .any(|l| l == r"Look      git clone -b snapshot '/srv/it'\''s here/copy.bundle' <dir>"),
        "{text}"
    );
}

/// A download folder that is given as a relative path (`HAMH_DOWNLOAD_DIR=out`, or `downloadDir` in `config.toml`): the look line of
/// a copy in two files changes folder before it fetches the copy (`git -C <dir>`), where a relative path no longer points at the
/// file. So the line names both files by their whole paths, and it works when a person pastes it in the folder `ham` ran in. The
/// `File` and `Needs` lines keep the paths as the folder was given.
#[tokio::test]
async fn b860_the_look_line_works_for_a_relative_download_folder() {
    let w = work("b860-look-relative-work");
    let fake = Fake::start().await;
    let run = start(&fake, "Add a sub function from a relative folder").await;
    save(&fake, &run, &w, Some(&w.base_bytes), 2);
    let h = home(&fake, "b860-look-relative");
    // The folder `ham` runs in, as the system names it, and the download folder `out` inside it.
    let ran_in = std::fs::canonicalize(&h.root).unwrap();
    let out_dir = ran_in.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let o = home::command(&h, &["pull", &run, "--snapshot"], &[])
        .env("HAMH_DOWNLOAD_DIR", "out")
        .current_dir(&ran_in)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    let printed = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{printed} {}", text(&o.stderr));
    let (copy_name, base_name) = (
        format!("{run}-snapshot-2.bundle"),
        format!("{run}-base.bundle"),
    );
    println!(
        "B860 TRIGGER PRESENT: the download folder was given as the relative path out, and both files are in it: {:?}",
        files_in(&out_dir)
    );
    assert_eq!(
        files_in(&out_dir),
        vec![base_name.clone(), copy_name.clone()]
    );
    let lines: Vec<&str> = printed.lines().collect();
    assert!(
        lines.contains(&format!("File      out/{copy_name}").as_str()),
        "{printed}"
    );
    assert!(
        lines.contains(
            &format!("Needs     out/{base_name} (the repository the run started from)").as_str()
        ),
        "{printed}"
    );
    let look = lines
        .iter()
        .find(|l| l.starts_with("Look "))
        .unwrap_or_else(|| panic!("no Look line: {printed}"));
    // Pasted in the folder `ham` ran in, the line gives a clean checkout at the copy's commit.
    let dir = ran_in.join("looked");
    paste(look, &dir, &ran_in);
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), w.copy_commit);
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
    // Word for word: both files by their whole paths.
    assert_eq!(
        *look,
        format!(
            "Look      git clone {base} <dir> && git -C <dir> fetch {copy} snapshot && git -C <dir> checkout -B ham-snapshot FETCH_HEAD",
            base = out_dir.join(&base_name).display(),
            copy = out_dir.join(&copy_name).display()
        )
    );
}

/// The link of the base can expire while a large base downloads: S3 then answers 403, and the client asks the route once more. A
/// fresh link for the same object goes on where the download stood. An answer that no longer names a base (the run's newest copy
/// is a whole one by then) and an answer that names another object end the download with the reason that is true, and the copy
/// stays on disk.
#[tokio::test]
async fn b860_a_base_link_that_expired_is_asked_for_once_more() {
    // The same object under a fresh link: both files are on disk, and the route was asked twice.
    let dir = support::scratch("b860-base-link-fresh");
    let api = scripted(Two {
        answer: answer(&thin_copy(), whole_base().len() as u64),
        copy: thin_copy(),
        base: whole_base(),
        base_status: None,
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(1),
        later: None,
    });
    let got = fetch_snapshot_with(&api, RUN, &dir, 1024).await.unwrap();
    println!(
        "B860 TRIGGER PRESENT: the first fetch of the base answered 403 and the route was asked again"
    );
    assert_eq!(
        got.base,
        Some((
            dir.join(format!("{RUN}-base.bundle")),
            whole_base().len() as u64
        ))
    );
    assert_eq!(
        std::fs::read(dir.join(format!("{RUN}-base.bundle"))).unwrap(),
        whole_base()
    );
    assert_eq!(
        files_in(&dir),
        vec![
            format!("{RUN}-base.bundle"),
            format!("{RUN}-snapshot-3.bundle")
        ]
    );

    // The route no longer names a base with the newest copy: the reason says that, and nothing says that the base changed.
    let dir = support::scratch("b860-base-link-no-base");
    let mut without = answer(&thin_copy(), whole_base().len() as u64);
    without.as_object_mut().unwrap().remove("base");
    let api = scripted(Two {
        answer: answer(&thin_copy(), whole_base().len() as u64),
        copy: thin_copy(),
        base: whole_base(),
        base_status: None,
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(1),
        later: Some(without),
    });
    let e = fetch_snapshot_with(&api, RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert_copy_kept(
        &dir,
        &e,
        "the stage no longer sends the repository the run started from with the newest copy",
    );
    assert!(!e.to_string().contains("changed while"), "{e}");

    // The route names another object as the base: that is a base that changed.
    let dir = support::scratch("b860-base-link-other");
    let mut other = answer(&thin_copy(), whole_base().len() as u64);
    other["base"]["etag"] = json!("\"another\"");
    let api = scripted(Two {
        answer: answer(&thin_copy(), whole_base().len() as u64),
        copy: thin_copy(),
        base: whole_base(),
        base_status: None,
        calls: Mutex::new(Vec::new()),
        base_expired: Mutex::new(1),
        later: Some(other),
    });
    let e = fetch_snapshot_with(&api, RUN, &dir, 1024)
        .await
        .unwrap_err();
    assert_copy_kept(
        &dir,
        &e,
        "the repository the run started from changed while it was downloading",
    );
}

/// A stage whose bucket holds no bundle the run started from (no rule removes it; a hand did). The route serves the copy alone, and
/// `ham` keeps it and says what it needs: the latest copy still downloads. Through the compiled binary and the real handler.
#[tokio::test]
async fn b860_pull_keeps_the_copy_when_the_stage_holds_no_base() {
    let w = work("b860-pull-no-base-work");
    let fake = Fake::start().await;
    let run = start(&fake, "Add a div function").await;
    save(&fake, &run, &w, None, 5);
    let h = home(&fake, "b860-pull-no-base");
    let (p, git_started) = shell(&h, &["pull", &run, "--snapshot"]).await;
    println!(
        "B860 TRIGGER PRESENT: the copy needs the base and the stage's bucket holds none; ham said: {} {}",
        p.out, p.err
    );
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    let copy_path = h.downloads.join(format!("{run}-snapshot-5.bundle"));
    assert_eq!(
        files_in(&h.downloads),
        vec![format!("{run}-snapshot-5.bundle")]
    );
    assert_eq!(std::fs::read(&copy_path).unwrap(), w.copy_bytes);
    let lines: Vec<&str> = p.out.lines().collect();
    assert!(
        lines.contains(&format!("Note      {}", ham::snapshot_cmd::NEEDS_BASE_NOTE).as_str()),
        "{}",
        p.out
    );
    assert!(!lines.iter().any(|l| l.starts_with("Look ")), "{}", p.out);
    assert!(!lines.iter().any(|l| l.starts_with("Needs ")), "{}", p.out);
    assert!(!git_started, "ham started git");
    let (p, _) = shell(&h, &["pull", &run, "--snapshot", "--json"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    let v: Value = serde_json::from_str(p.out.trim()).unwrap();
    assert_eq!(v["snapshot"]["needsBase"], true, "{v}");
    assert!(v["snapshot"].get("base").is_none(), "{v}");
}
