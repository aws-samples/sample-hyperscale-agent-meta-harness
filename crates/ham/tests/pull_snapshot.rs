//! `ham pull <runId> --snapshot` and `/pull --snapshot`: the command that downloads the last snapshot of a live run. The
//! library half (`fetch_snapshot`, ranged download, size and commit check, rename after the check) has its own tests in `chat_routes.rs`;
//! here the compiled binary and the in-process runner of the chat box run the command against the stand-in `/v1` API (the real control-api
//! handler over an in-memory store with a snapshot object in it). The two print the same text.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::Api;
use ham::cli::{ExecContext, run_captured};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham_core::command::parse;
use ham_core::config::{Config, parse_config};
use ham_core::snapshot::SNAPSHOT_REF;
use home::{Home, ham, home, text};
use serde_json::{Value, json};
use std::sync::Arc;
use support::{Fake, REGION, creds};

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

/// A copy that holds the whole history. A copy whose header names a commit it needs comes with the repository the run started
/// from.
fn bundle(head: &str, filler: usize) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{head} {SNAPSHOT_REF}\n\nPACK").into_bytes();
    b.extend((0..filler).map(|i| (i % 251) as u8));
    b
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

/// Puts a snapshot (bundle and record) under the run's turn prefix, as agentd does.
fn save_snapshot(fake: &Fake, run: &str, object: &[u8], count: u32) {
    let prefix = harness_protocol::direct::s3::turn_prefix(run);
    let mut g = fake.mem.inner.lock().unwrap();
    g.objects.insert(
        format!("{prefix}snapshot.bundle"),
        (object.to_vec(), "\"etag-b\"".into()),
    );
    g.objects.insert(
        format!("{prefix}snapshot.json"),
        (
            json!({"schemaVersion": 1, "headSha": HEAD, "baseSha": BASE, "at": "2026-10-15T13:10:00Z", "count": count})
                .to_string()
                .into_bytes(),
            "\"etag-j\"".into(),
        ),
    );
}

struct Printed {
    code: i32,
    out: String,
    err: String,
}

async fn shell(h: &Home, args: &[&str]) -> Printed {
    let o = ham(h, args).await;
    Printed {
        code: o.status.code().unwrap(),
        out: text(&o.stdout),
        err: text(&o.stderr),
    }
}

#[tokio::test]
async fn the_command_downloads_the_last_snapshot_and_says_what_it_is() {
    let fake = Fake::start().await;
    let run = start(&fake, "Add a percent_change function").await;
    let object = bundle(HEAD, 40_000);
    save_snapshot(&fake, &run, &object, 3);
    let h = home(&fake, "pull-snapshot");
    let p = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    // The file is named for the run and the snapshot's count, holds the bytes the store holds, and no `.part` file is left.
    let file = h.downloads.join(format!("{run}-snapshot-3.bundle"));
    assert_eq!(std::fs::read(&file).unwrap(), object);
    let left: Vec<String> = std::fs::read_dir(&h.downloads)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec![format!("{run}-snapshot-3.bundle")], "{left:?}");
    // The words: where it is, the commit, and that it is a worker snapshot nobody verified.
    assert!(
        p.out.contains(&file.display().to_string()),
        "the path is printed: {}",
        p.out
    );
    assert!(
        p.out.contains(&HEAD[..12]),
        "the commit is printed: {}",
        p.out
    );
    assert!(
        p.out.contains(ham_core::snapshot::SNAPSHOT_TRUST_NOTE),
        "the trust note is printed: {}",
        p.out
    );
    assert!(!p.out.contains("X-Amz-"), "no link is printed: {}", p.out);
}

#[tokio::test]
async fn json_output_is_one_object_with_the_path_the_commit_and_the_size() {
    let fake = Fake::start().await;
    let run = start(&fake, "Write a markdown parser").await;
    let object = bundle(HEAD, 5_000);
    save_snapshot(&fake, &run, &object, 7);
    let h = home(&fake, "pull-snapshot-json");
    let p = shell(&h, &["pull", &run, "--snapshot", "--json"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    let v: Value = serde_json::from_str(p.out.trim()).unwrap();
    assert_eq!(v["runId"], run.as_str());
    assert_eq!(v["snapshot"]["headSha"], HEAD);
    assert_eq!(v["snapshot"]["count"], 7);
    assert_eq!(v["snapshot"]["sizeBytes"], object.len());
    assert_eq!(
        v["snapshot"]["path"],
        h.downloads
            .join(format!("{run}-snapshot-7.bundle"))
            .display()
            .to_string()
    );
    assert!(!p.out.contains("X-Amz-"), "no link is printed: {}", p.out);
}

#[tokio::test]
async fn a_run_with_no_snapshot_exits_1_with_the_servers_sentence() {
    let fake = Fake::start().await;
    let run = start(&fake, "Another run").await;
    let h = home(&fake, "pull-snapshot-none");
    let p = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(p.code, 1, "{} {}", p.out, p.err);
    assert!(p.err.contains("no copy of this run's work"), "{}", p.err);
    assert_eq!(std::fs::read_dir(&h.downloads).unwrap().count(), 0);
}

#[tokio::test]
async fn typed_in_the_chat_box_the_runner_prints_what_the_shell_prints() {
    let fake = Fake::start().await;
    let run = start(&fake, "Add a percent_change function").await;
    save_snapshot(&fake, &run, &bundle(HEAD, 9_000), 4);
    let h = home(&fake, "pull-snapshot-box");
    let shell_out = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(shell_out.code, 0, "{}", shell_out.err);

    let cfg: Config =
        parse_config(&std::fs::read_to_string(h.config.join("ham/config.toml")).unwrap()).unwrap();
    let api = api(&fake);
    let (config, root, downloads) = (
        h.config.display().to_string(),
        h.root.display().to_string(),
        h.downloads.display().to_string(),
    );
    let env = move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config.clone()),
        "HOME" => Some(root.clone()),
        "HAMH_DOWNLOAD_DIR" => Some(downloads.clone()),
        _ => None,
    };
    let inv = parse(&["pull", &run, "--snapshot"]).unwrap();
    let out = run_captured(
        &inv,
        &ExecContext {
            api: &api,
            cfg: &cfg,
            downloads: &h.downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: None,
        },
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr_text());
    assert_eq!(out.stdout_text(), shell_out.out);
}

// ----: files the service read back from a stopped session's storage ---------------------------------------------------------------

const READ_BACK: &str = "3333333333333333333333333333333333333333";

/// Puts the files the service read back from the storage of session `n` into the bucket, as the worker of the read does.
fn save_read_back(fake: &Fake, run: &str, n: u32, object: &[u8]) {
    fake.mem.inner.lock().unwrap().objects.insert(
        harness_protocol::direct::s3::recovery_bundle_key(run, n),
        (object.to_vec(), "\"etag-r\"".into()),
    );
}

/// `ham pull <runId> --read-back <n>`: the one command that downloads files the service read back and did not use. The Run item
/// names no chosen copy here, and the run's newest saved copy is another commit.
#[tokio::test]
async fn b854_pull_read_back_downloads_the_files_read_back_from_the_named_session_used_or_not() {
    let fake = Fake::start().await;
    let run = start(&fake, "Add a percent_change function for read back").await;
    save_snapshot(&fake, &run, &bundle(HEAD, 4_000), 3);
    let object = bundle(READ_BACK, 30_000);
    save_read_back(&fake, &run, 1, &object);
    let h = home(&fake, "pull-read-back");
    let p = shell(&h, &["pull", &run, "--read-back", "1"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    let file = h.downloads.join(format!("{run}-read-back-1.bundle"));
    assert_eq!(
        std::fs::read(&file).unwrap(),
        object,
        "the file holds the files read back, not the saved copy"
    );
    let left: Vec<String> = std::fs::read_dir(&h.downloads)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec![format!("{run}-read-back-1.bundle")], "{left:?}");
    // The words: what the copy is, without a claim that it is newer than the saved copy or that the run went on from it.
    for said in [
        format!("pulled the files read back from session 1 of run {run}"),
        format!("File      {}", file.display()),
        format!("Commit    {READ_BACK} (read back from the storage of session 1)"),
        "Note      This copy is what the service read back from the storage of session 1 after that session ended. The run may not \
         have gone on from it: the service uses files it read back only when its checks leave no doubt."
            .to_string(),
        format!("Look      git clone -b snapshot {} <dir>", file.display()),
        format!("Warning   {}", ham_core::snapshot::SNAPSHOT_TRUST_NOTE),
    ] {
        assert!(p.out.contains(&said), "missing {said:?} in:\n{}", p.out);
    }
    // No claim that the copy is newer or that the run used it, and the clone names the branch of a copy, not of a delivery.
    for claim in ["newer than", "this copy is its work", "-b harness/"] {
        assert!(!p.out.contains(claim), "{claim:?} in:\n{}", p.out);
    }
    assert!(!p.out.contains("X-Amz-"), "no link is printed: {}", p.out);

    // --json names the session the files were read back from.
    let j = shell(&h, &["pull", &run, "--read-back", "1", "--json"]).await;
    assert_eq!(j.code, 0, "{} {}", j.out, j.err);
    let v: Value = serde_json::from_str(j.out.trim()).unwrap();
    assert_eq!(v["snapshot"]["headSha"], READ_BACK);
    assert_eq!(v["snapshot"]["readBackSession"], 1);
    assert!(v["snapshot"].get("count").is_none(), "{v}");

    // A session whose storage was not read back: exit 1 with the service's sentence, and nothing is written.
    let none = shell(&h, &["pull", &run, "--read-back", "2"]).await;
    assert_eq!(none.code, 1, "{} {}", none.out, none.err);
    assert!(
        none.err
            .contains("no files were read back from the storage of session 2 of this run"),
        "{}",
        none.err
    );
    assert!(
        !h.downloads
            .join(format!("{run}-read-back-2.bundle"))
            .exists()
    );

    // Trigger: the plain snapshot of the same run is the saved copy, another file and another commit.
    let s = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(s.code, 0, "{}", s.err);
    assert!(
        s.out.contains(HEAD) && !s.out.contains(READ_BACK),
        "{}",
        s.out
    );
    assert!(s.out.contains("pulled the snapshot of run"), "{}", s.out);
}

/// `ham pull <runId> --snapshot` follows the copy the service chose: the files read back from session 1 while no later session has
/// saved a copy of its own. The text says what the copy is.
#[tokio::test]
async fn b854_pull_snapshot_says_when_the_newest_copy_is_the_files_read_back() {
    let fake = Fake::start().await;
    let run = start(&fake, "Write a parser whose session is read back").await;
    save_snapshot(&fake, &run, &bundle(HEAD, 4_000), 3);
    let object = bundle(READ_BACK, 12_000);
    save_read_back(&fake, &run, 1, &object);
    {
        let mut g = fake.mem.inner.lock().unwrap();
        let item = g.runs.get_mut(&run).unwrap();
        item.slice = Some(2);
        item.recovered_copy = Some(Box::new(harness_protocol::store::RecoveredCopyRef {
            session: 1,
            key: harness_protocol::direct::s3::recovery_bundle_key(&run, 1),
            version_id: None,
            head_sha: READ_BACK.into(),
            at: "2026-11-16T14:05:00Z".into(),
            newest_write_at: None,
            bundle_bytes: object.len() as u64,
        }));
    }
    let h = home(&fake, "pull-snapshot-read-back");
    let p = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(p.code, 0, "{} {}", p.out, p.err);
    let file = h.downloads.join(format!("{run}-read-back-1.bundle"));
    assert_eq!(std::fs::read(&file).unwrap(), object);
    for said in [
        format!("pulled the snapshot of run {run}"),
        format!("Commit    {READ_BACK} (read back from the storage of session 1)"),
        "Note      This copy is what the service read back from the storage of session 1 after that session ended. It is newer than \
         the last copy saved while the run worked."
            .to_string(),
        format!("Look      git clone -b snapshot {} <dir>", file.display()),
    ] {
        assert!(p.out.contains(&said), "missing {said:?} in:\n{}", p.out);
    }
    assert!(
        !p.out.contains("changes after"),
        "a copy read back is not worded as a copy made while the run worked:\n{}",
        p.out
    );
}

/// `--read-back` stands alone and takes the number of a session.
#[tokio::test]
async fn b854_pull_read_back_takes_a_session_number_and_no_other_selector() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run for the usage of read back").await;
    let h = home(&fake, "pull-read-back-usage");
    for (args, said) in [
        (
            vec!["pull", run.as_str(), "--read-back", "1", "--snapshot"],
            "ham pull takes --read-back <n> alone, without --snapshot or --artifact",
        ),
        (
            vec![
                "pull",
                run.as_str(),
                "--read-back",
                "1",
                "--artifact",
                "zip",
            ],
            "ham pull takes --read-back <n> alone, without --snapshot or --artifact",
        ),
        (
            vec!["pull", run.as_str(), "--read-back", "0"],
            "--read-back takes the number of a session of the run, 1 to 256, got \"0\"",
        ),
        (
            vec!["pull", run.as_str(), "--read-back", "two"],
            "--read-back takes the number of a session of the run, 1 to 256, got \"two\"",
        ),
    ] {
        let p = shell(&h, &args).await;
        assert_ne!(p.code, 0, "{args:?}: {}", p.out);
        assert!(p.err.contains(said), "{args:?}: {}", p.err);
    }
    // The parser gives the command back as it was typed.
    let inv = parse(&["pull", &run, "--read-back", "12"]).unwrap();
    assert_eq!(
        inv.to_argv(),
        vec![
            "pull".to_string(),
            run.clone(),
            "--read-back".to_string(),
            "12".to_string()
        ]
    );
    // The help names the option.
    let help = shell(&h, &["pull", "--help"]).await;
    assert!(
        help.out.contains("--read-back <n>"),
        "the help of ham pull names the option:\n{}",
        help.out
    );
}
