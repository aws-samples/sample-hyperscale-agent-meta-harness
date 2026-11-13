//! `ham pull` and the flow a run declared: the harness records `quick`, `full` or `none` in `evidence/direct.json` (`summary.flow`),
//! which rides in the delivered zip. `ham pull` reads it and prints one `Flow` row next to the label (and a `flow` member with `--json`).
//! The row is a fact: the label, the exit code and every other row are the ones they were, and a zip with no record prints nothing more.
//! The deliveries are fakes: zips written here and served by the fake control plane.
#![allow(clippy::unwrap_used)]

mod support;

use ham::flow::{flow_of_zip, flow_row};
use harness_protocol::direct::{DirectFlow, DirectOutcome};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use support::{AKID, Fake, REGION, SECRET, TOKEN};

const RUN: &str = "01M4XFLOWRUNDIRECT00000001";

/// A delivered zip: the entries are `(path under the top directory, body)`.
fn zip_with(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        w.start_file(format!("calc-{RUN}-0123456/{name}"), opts)
            .unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn write_zip(label: &str, entries: &[(&str, Vec<u8>)]) -> PathBuf {
    let dir = support::scratch(label);
    let path = dir.join("delivered.zip");
    std::fs::write(&path, zip_with(entries)).unwrap();
    path
}

/// `evidence/direct.json` as run-session writes it: its own members and agentd's summary under `summary`.
fn evidence(flow: Option<&str>) -> Vec<u8> {
    let mut summary = json!({"schemaVersion": 1, "runId": RUN, "clineMessage": "Done."});
    if let Some(word) = flow {
        summary["flow"] = json!(word);
    }
    serde_json::to_vec(&json!({
        "schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "label": "Cline reported done",
        "summary": summary
    }))
    .unwrap()
}

// ---- the zip reader -----------------------------------------------------------------------------------------------------------

#[test]
fn the_flow_comes_from_the_summary_of_the_evidence_file_in_the_zip() {
    for (word, flow) in [
        ("quick", DirectFlow::Quick),
        ("full", DirectFlow::Full),
        ("none", DirectFlow::Undeclared),
    ] {
        let path = write_zip(
            "pull-flow-ok",
            &[
                ("src/a.py", b"x".to_vec()),
                ("evidence/direct.json", evidence(Some(word))),
            ],
        );
        assert_eq!(flow_of_zip(&path), Some(flow), "{word}");
        assert_eq!(flow_row(flow), format!("Flow      {word}"));
    }
}

#[test]
fn a_zip_without_the_record_gives_nothing() {
    let none = write_zip("pull-flow-none", &[("src/a.py", b"x".to_vec())]);
    assert_eq!(flow_of_zip(&none), None);
    // A run whose summary has no flow member (an older harness, or no commits to read) prints no row.
    let old = write_zip("pull-flow-old", &[("evidence/direct.json", evidence(None))]);
    assert_eq!(flow_of_zip(&old), None);
    // Only `<top>/evidence/direct.json` is the record: a file of the project with that name is not.
    let nested = write_zip(
        "pull-flow-nested",
        &[("src/evidence/direct.json", evidence(Some("quick")))],
    );
    assert_eq!(flow_of_zip(&nested), None);
    assert_eq!(flow_of_zip(std::path::Path::new("/no/such/file.zip")), None);
}

#[test]
fn a_record_that_is_not_json_is_too_large_or_holds_another_word_gives_nothing() {
    let bad = write_zip(
        "pull-flow-bad",
        &[("evidence/direct.json", b"{not json".to_vec())],
    );
    assert_eq!(flow_of_zip(&bad), None);
    let big = write_zip(
        "pull-flow-big",
        &[("evidence/direct.json", vec![b' '; 3_000_000])],
    );
    assert_eq!(flow_of_zip(&big), None);
    let other = write_zip(
        "pull-flow-other",
        &[("evidence/direct.json", evidence(Some("light")))],
    );
    assert_eq!(flow_of_zip(&other), None);
}

// ---- the binary ---------------------------------------------------------------------------------------------------------------

struct Home {
    root: PathBuf,
    downloads: PathBuf,
}

fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        downloads: root.join("downloads"),
        root,
    };
    std::fs::create_dir_all(&h.downloads).unwrap();
    std::fs::create_dir_all(h.root.join("config/ham")).unwrap();
    std::fs::write(
        h.root.join("config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint, REGION
        ),
    )
    .unwrap();
    let script = h.root.join("creds.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        h.root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            REGION
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", h.root.join("config"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_CACHE_HOME", h.root.join("cache"))
        .env("HAMH_DOWNLOAD_DIR", &h.downloads)
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

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    let plane = ham::https::HttpsPlane::new(
        &fake.endpoint,
        ham::sigv4::Signer::new(REGION, support::creds()),
    )
    .unwrap();
    ham::api::Api::new(std::sync::Arc::new(plane))
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_prints_the_flow_row_beside_the_label_and_adds_the_member_to_json() {
    let fake = Fake::start().await;
    let h = home(&fake, "pull-flow-bin");
    let run = new_run(&fake, "flow row").await;
    fake.mem.inner.lock().unwrap().deliverable = Some(zip_with(&[(
        "evidence/direct.json",
        evidence(Some("quick")),
    )]));
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(
        out.lines().any(|l| l == "Outcome   Cline reported done"),
        "{out}"
    );
    assert_eq!(
        out.lines().filter(|l| *l == "Flow      quick").count(),
        1,
        "{out}"
    );
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert_eq!(v["flow"], "quick", "{v}");
    assert_eq!(
        v["label"], "Cline reported done",
        "the flow does not touch the label: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_declared_nothing_shows_none_and_the_exit_code_is_the_same() {
    let fake = Fake::start().await;
    let h = home(&fake, "pull-flow-none-bin");
    let run = new_run(&fake, "flow none").await;
    fake.mem.inner.lock().unwrap().deliverable = Some(zip_with(&[(
        "evidence/direct.json",
        evidence(Some("none")),
    )]));
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.lines().any(|l| l == "Flow      none"), "{out}");
    assert!(
        out.lines().any(|l| l == "Outcome   Cline reported done"),
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_without_the_record_prints_what_it_printed_before() {
    let fake = Fake::start().await;
    let h = home(&fake, "pull-flow-absent");
    let run = new_run(&fake, "flow absent").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        !text(&o.stdout).lines().any(|l| l.starts_with("Flow ")),
        "{}",
        text(&o.stdout)
    );
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert!(v.get("flow").is_none(), "{v}");
}
