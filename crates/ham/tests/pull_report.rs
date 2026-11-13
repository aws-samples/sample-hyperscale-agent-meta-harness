//! `ham pull` and the clean-checkout test report: the report rides in the zip as `evidence/test-report.json`; `ham pull`
//! reads it and prints one `Tests` line (and a `testReport` member with `--json`) next to the label. The label is the label of the run
//! and the line never changes it; a zip with no report prints nothing more than before.
#![allow(clippy::unwrap_used)]

mod support;

use ham::report::{card_line, summary_of_zip};
use harness_protocol::direct::{
    DirectOutcome, TestCommandSource, TestCounts, TestReport, TestReportStatus, TestStep,
    TestStepRole, banned_claim_word,
};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{AKID, Fake, REGION, SECRET, TOKEN};

const RUN: &str = "01M4XTESTRUNDIRECT00000001";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn report(exit: i32, counts: (u32, u32, u32)) -> TestReport {
    TestReport {
        schema_version: 1,
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        status: TestReportStatus::Ran,
        reason: None,
        started_at: None,
        duration_ms: 900,
        steps: vec![TestStep {
            role: TestStepRole::Test,
            source: TestCommandSource::Detected,
            command: "python3 -m pytest -q".into(),
            exit_code: Some(exit),
            timed_out: false,
            duration_ms: 800,
            limit_s: None,
            counts: Some(TestCounts {
                passed: counts.0,
                failed: counts.1,
                skipped: counts.2,
            }),
            counts_from: Some("pytest".into()),
            output_tail: "12 passed in 0.31s\n".into(),
            output_truncated: false,
            logged: None,
        }],
        summary: None,
    }
}

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

fn report_bytes(r: &TestReport) -> Vec<u8> {
    serde_json::to_vec(r).unwrap()
}

// ---- the zip reader -----------------------------------------------------------------------------------------------------------

#[test]
fn the_summary_comes_from_the_steps_of_the_report_in_the_zip() {
    let mut r = report(0, (12, 0, 0));
    // A summary the file carries is not read: the steps decide.
    r.summary = Some(report(1, (1, 9, 0)).summary());
    let path = write_zip(
        "pull-report-ok",
        &[
            ("src/a.py", b"x".to_vec()),
            ("evidence/test-report.json", report_bytes(&r)),
        ],
    );
    let s = summary_of_zip(&path).unwrap();
    assert_eq!(s, report(0, (12, 0, 0)).summary());
    assert_eq!(
        s.claim().as_deref(),
        Some("repository tests pass in a clean checkout: 12 of 12")
    );
}

#[test]
fn a_zip_without_the_report_or_a_report_in_another_place_gives_nothing() {
    let none = write_zip(
        "pull-report-none",
        &[("src/a.py", b"x".to_vec()), ("EVIDENCE.md", b"e".to_vec())],
    );
    assert_eq!(summary_of_zip(&none), None);
    // Only `<top>/evidence/test-report.json` is the report: a file of the project with that name is not.
    let nested = write_zip(
        "pull-report-nested",
        &[(
            "src/evidence/test-report.json",
            report_bytes(&report(0, (5, 0, 0))),
        )],
    );
    assert_eq!(summary_of_zip(&nested), None);
    assert_eq!(summary_of_zip(Path::new("/no/such/file.zip")), None);
}

#[test]
fn a_report_that_is_not_json_or_is_too_large_gives_nothing() {
    let bad = write_zip(
        "pull-report-bad",
        &[("evidence/test-report.json", b"{not json".to_vec())],
    );
    assert_eq!(summary_of_zip(&bad), None);
    let big = write_zip(
        "pull-report-big",
        &[("evidence/test-report.json", vec![b' '; 200_000])],
    );
    assert_eq!(summary_of_zip(&big), None);
}

#[test]
fn the_card_line_is_the_one_sentence_after_a_fixed_label() {
    let s = report(0, (12, 0, 0)).summary();
    assert_eq!(
        card_line(&s),
        "Tests     repository tests pass in a clean checkout: 12 of 12."
    );
    let red = report(1, (3, 1, 0)).summary();
    assert_eq!(
        card_line(&red),
        "Tests     `python3 -m pytest -q` exited 1 in a clean checkout; the runner reported 3 passed, 1 failed."
    );
    assert_eq!(banned_claim_word(&card_line(&s)), None);
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
async fn pull_prints_the_tests_line_beside_the_label_and_adds_the_member_to_json() {
    let fake = Fake::start().await;
    let h = home(&fake, "pull-report-bin");
    let run = new_run(&fake, "report line").await;
    let zip = zip_with(&[(
        "evidence/test-report.json",
        report_bytes(&report(0, (12, 0, 0))),
    )]);
    fake.mem.inner.lock().unwrap().deliverable = Some(zip);
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(
        out.lines().any(|l| l == "Outcome   Cline reported done"),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|l| l == "Tests     repository tests pass in a clean checkout: 12 of 12."),
        "{out}"
    );
    assert_eq!(banned_claim_word(&out), None, "{out}");
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert_eq!(v["testReport"]["counts"]["passed"], 12);
    assert_eq!(v["testReport"]["status"], "ran");
    assert_eq!(
        v["label"], "Cline reported done",
        "the report does not touch the label: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_report_changes_neither_the_label_nor_the_exit_code() {
    let fake = Fake::start().await;
    let h = home(&fake, "pull-report-red");
    let run = new_run(&fake, "report red").await;
    fake.mem.inner.lock().unwrap().deliverable = Some(zip_with(&[(
        "evidence/test-report.json",
        report_bytes(&report(1, (3, 1, 0))),
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
    assert!(
        out.contains("Tests     `python3 -m pytest -q` exited 1 in a clean checkout"),
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_without_a_report_prints_what_it_printed_before() {
    let fake = Fake::start().await;
    let h = home(&fake, "pull-report-absent");
    let run = new_run(&fake, "report absent").await;
    fake.mem
        .complete_direct_run(&run, DirectOutcome::ReportedDone, None);
    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        !text(&o.stdout).lines().any(|l| l.starts_with("Tests ")),
        "{}",
        text(&o.stdout)
    );
    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert!(v.get("testReport").is_none(), "{v}");
}
