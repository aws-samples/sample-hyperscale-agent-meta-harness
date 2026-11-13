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

fn bundle(head: &str, filler: usize) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n-{BASE} base subject\n{head} {SNAPSHOT_REF}\n\nPACK")
        .into_bytes();
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
    assert!(p.err.contains("no snapshot"), "{}", p.err);
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
