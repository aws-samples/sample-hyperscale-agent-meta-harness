//! audit: `rules_cmd::with_attachments` has two call sites, and each one is exercised here through its real path, not through the file
//! reader alone:
//!
//! - the command line: the real `ham` binary runs `ham "<prompt>" --direct --model <id> --attach <file>` against the local `/v1` API, which runs
//!   the real control-api handler as the `direct` stage (`cli.rs`, the start of a direct run);
//! - the composer: the real `submit_direct` of the dashboard, reached through `tui::perform(Effect::SubmitDirect)` against the same API
//!   (`tui/mod.rs`).
//!
//! Each success test names its trigger: the `attachments` member must be in the POST body and in the request control-api stored, so a call site that
//! stops calling `with_attachments` (or calls it with the wrong list) fails the test. A file that cannot be sent stops the start before any POST.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{Effect, Msg, perform};
use ham_core::attachments::read_attachments;
use ham_core::command::{Command, parse};
use harness_protocol::direct::MAX_REQUEST_BYTES;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;
use support::{AKID, Fake, REGION, SECRET, TOKEN, creds};

const MODEL: &str = "global.anthropic.claude-sonnet-5-5";
const PROMPT: &str = "Add a percent_change(a, b) function with tests";
const ENDPOINT: &str = "/v1/direct/runs";
const NOTES: &str = "Use two-space indents.\nKeep functions short.\n";
const TABLE: &str = "a,b\n1,2\n3,4\n";

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

struct Home {
    root: PathBuf,
}

/// A temp HOME with `~/.config/ham/config.toml` naming the fake, and a `credential_process` profile.
fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    std::fs::create_dir_all(root.join(".config/ham")).unwrap();
    std::fs::write(
        root.join(".config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint, REGION
        ),
    )
    .unwrap();
    let script = root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            REGION
        ),
    )
    .unwrap();
    std::fs::write(root.join("aws-credentials"), "").unwrap();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    Home { root }
}

async fn ham(h: &Home, args: &[String]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", &h.root)
        .env("HOME", &h.root)
        .env("HAMH_DOWNLOAD_DIR", h.root.join("downloads"))
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

/// The command line of a direct start that attaches `files`, in the order given; `--no-rules` keeps the rules directory out of the test.
fn start_line(files: &[String]) -> Vec<String> {
    let mut a = words(&[
        PROMPT,
        "--s3-only",
        "--direct",
        "--model",
        MODEL,
        "--json",
        "--no-rules",
    ]);
    for f in files {
        a.push("--attach".into());
        a.push(f.clone());
    }
    a
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p.display().to_string()
}

fn body_of(fake: &Fake, n: usize) -> Value {
    serde_json::from_slice(&fake.mem.posts(ENDPOINT)[n].body).unwrap()
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

/// The composer's submit: the words the box holds, parsed like a command line, then the real `submit_direct` through `perform`.
async fn composer_submit(a: &Api<HttpsPlane>, files: &[String], nonce: &str) -> Msg {
    let mut line = words(&[
        "--prompt",
        PROMPT,
        "--s3-only",
        "--no-rules",
        "--direct",
        "--model",
        MODEL,
    ]);
    for f in files {
        line.push("--attach".into());
        line.push(f.clone());
    }
    let Command::StartDirect(args) = parse(&line).unwrap().command else {
        panic!("the words do not make a direct start");
    };
    assert_eq!(&args.attach, files, "the parser keeps the files in order");
    let dir = support::scratch("attach-sites-tui");
    perform(
        a,
        &dir,
        Effect::SubmitDirect {
            args,
            nonce: nonce.into(),
        },
    )
    .await
    .unwrap()
}

/// What the two files must look like on the wire: name, size, lowercase sha256 and content, in the order attached.
fn expected_items(notes: &str, table: &str) -> Value {
    serde_json::json!([
        {"name": file_name(notes), "sha256": sha(NOTES.as_bytes()), "size": NOTES.len(), "content": NOTES},
        {"name": file_name(table), "sha256": sha(TABLE.as_bytes()), "size": TABLE.len(), "content": TABLE},
    ])
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Files the start must refuse, with a name for the case. Each is judged by the reader, and the expected words come from it.
fn bad_cases(dir: &Path) -> Vec<(&'static str, Vec<String>)> {
    let a = write(dir, "one/notes.md", b"first\n");
    let b = write(dir, "two/notes.md", b"second\n");
    vec![
        (
            "a file that does not exist",
            vec![dir.join("missing.md").display().to_string()],
        ),
        ("a folder", vec![dir.display().to_string()]),
        (
            "a reserved name",
            vec![write(dir, "INDEX.md", b"index text\n")],
        ),
        (
            "bytes that are not text",
            vec![write(dir, "blob.txt", &[0xff, 0xfe, 0xfd])],
        ),
        ("an empty file", vec![write(dir, "empty.md", b"")]),
        (
            // Longer than a whole request can be: refused before it is read, so a file of holes is as good as one of text.
            "a file longer than a request",
            vec![{
                let p = dir.join("big.txt");
                std::fs::File::create(&p)
                    .unwrap()
                    .set_len(MAX_REQUEST_BYTES as u64 + 1)
                    .unwrap();
                p.display().to_string()
            }],
        ),
        ("a name used twice", vec![a, b]),
    ]
}

// ---- the command line ---------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_line_start_sends_every_attached_file_and_control_api_stores_them() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "attach-cli");
    let notes = write(&h.root, "docs/notes.md", NOTES.as_bytes());
    let table = write(&h.root, "docs/table.csv", TABLE.as_bytes());
    let o = ham(&h, &start_line(&[notes.clone(), table.clone()])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let first: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();

    assert_eq!(fake.mem.posts(ENDPOINT).len(), 1);
    let sent = body_of(&fake, 0);
    let want = expected_items(&notes, &table);
    assert_eq!(sent["attachments"], want, "{sent}");
    assert_eq!(sent["rules"].as_array().unwrap().len(), 0);
    // The service accepted the request (201) and kept the same list as the stored start request.
    let stored = fake.mem.stored_request_of(first["runId"].as_str().unwrap());
    assert_eq!(stored["attachments"], want, "{stored}");
    println!("ATTACH-CALL-SITE TRIGGER PRESENT: site=command-line attachments=2 stored=equal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_line_start_without_attach_has_no_attachments_member() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "attach-cli-none");
    let o = ham(&h, &start_line(&[])).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let sent = body_of(&fake, 0);
    assert!(sent.get("attachments").is_none(), "{sent}");
    let first: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
    let stored = fake.mem.stored_request_of(first["runId"].as_str().unwrap());
    assert!(stored.get("attachments").is_none(), "{stored}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_line_refuses_a_file_it_cannot_send_before_anything_is_sent() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "attach-cli-bad");
    for (what, files) in bad_cases(&h.root) {
        let expected = read_attachments(&files).unwrap_err().to_string();
        let o = ham(&h, &start_line(&files)).await;
        assert_eq!(o.status.code(), Some(1), "{what}: {}\n{}", out(&o), err(&o));
        assert!(
            err(&o).contains(&expected),
            "{what}: want {expected:?} in {:?}",
            err(&o)
        );
        assert!(!err(&o).contains("index text") && !err(&o).contains("first\n"));
        assert!(
            fake.mem.posts(ENDPOINT).is_empty(),
            "{what}: nothing was sent"
        );
    }
}

// ---- the composer -------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_composer_submit_sends_every_attached_file_and_control_api_stores_them() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("attach-sites-files");
    let notes = write(&dir, "docs/notes.md", NOTES.as_bytes());
    let table = write(&dir, "docs/table.csv", TABLE.as_bytes());
    let msg = composer_submit(&a, &[notes.clone(), table.clone()], "attach-1").await;
    let Msg::Started(Ok(started)) = msg else {
        panic!("the run did not start: {msg:?}");
    };

    assert_eq!(fake.mem.posts(ENDPOINT).len(), 1);
    let sent = body_of(&fake, 0);
    let want = expected_items(&notes, &table);
    assert_eq!(sent["attachments"], want, "{sent}");
    let stored = fake.mem.stored_request_of(&started.run_id);
    assert_eq!(stored["attachments"], want, "{stored}");
    println!("ATTACH-CALL-SITE TRIGGER PRESENT: site=composer attachments=2 stored=equal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_composer_submit_without_attach_has_no_attachments_member() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let msg = composer_submit(&a, &[], "attach-none").await;
    let Msg::Started(Ok(_)) = msg else {
        panic!("the run did not start: {msg:?}");
    };
    let sent = body_of(&fake, 0);
    assert!(sent.get("attachments").is_none(), "{sent}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_composer_refuses_a_file_it_cannot_send_before_anything_is_sent() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("attach-sites-bad");
    for (n, (what, files)) in bad_cases(&dir).into_iter().enumerate() {
        let expected = read_attachments(&files).unwrap_err().to_string();
        let msg = composer_submit(&a, &files, &format!("attach-bad-{n}")).await;
        let Msg::Started(Err(shown)) = msg else {
            panic!("{what}: the start was not refused: {msg:?}");
        };
        assert!(
            shown.contains(&expected),
            "{what}: want {expected:?} in {shown:?}"
        );
        assert!(!shown.contains("index text") && !shown.contains("first\n"));
        assert!(
            fake.mem.posts(ENDPOINT).is_empty(),
            "{what}: nothing was sent"
        );
    }
}

// ---- both ------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_line_and_the_composer_send_the_same_attachments_for_the_same_files() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "attach-both");
    let notes = write(&h.root, "docs/notes.md", NOTES.as_bytes());
    let table = write(&h.root, "docs/table.csv", TABLE.as_bytes());
    let files = [notes, table];
    let o = ham(&h, &start_line(&files)).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let msg = composer_submit(&api(&fake), &files, "attach-both").await;
    assert!(matches!(msg, Msg::Started(Ok(_))), "{msg:?}");
    assert_eq!(fake.mem.posts(ENDPOINT).len(), 2);
    let (cli, composer) = (body_of(&fake, 0), body_of(&fake, 1));
    assert_eq!(
        cli["attachments"].as_array().map(Vec::len),
        Some(2),
        "the command line sent both files: {cli}"
    );
    assert_eq!(cli["attachments"], composer["attachments"]);
}
