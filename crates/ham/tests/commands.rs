//! The `ham` commands beyond start, watch, pull and link, end to end through the binary
//! against the local `/v1` API (the real control-api handler as the `direct` stage): `cancel`, `pr` and `doctor`; plus `link`
//! near the signer's expiry and `pull` killed mid-download then resumed, and `pull --artifact evidence`.
//! Every command has a `--json` form; exit codes: 0 done, 1 error, 2 usage.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use harness_protocol::api::internal::PendingControl;
use home::{assert_no_secrets, ham, ham_env, home, home_with, json_lines, new_run, text};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::Fake;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn one_json(o: &std::process::Output) -> Value {
    let lines = json_lines(o);
    assert_eq!(lines.len(), 1, "one JSON line: {}", text(&o.stdout));
    lines[0].clone()
}

#[tokio::test]
async fn cancel_stops_a_run_without_a_live_agent_and_a_finished_run_cannot_be_cancelled() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "control").await;
    let h = home(&fake, "control");
    let o = ham(&h, &["cancel", &run, "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(one_json(&o), json!({"runId": run, "status": "cleared"}));
    {
        let g = fake.mem.inner.lock().unwrap();
        assert_eq!(g.stopped.len(), 1, "the run's execution is stopped");
        assert_eq!(g.stopped[0].0, run);
    }
    // The watch of a cleared run ends with its exit code (15).
    let o = ham(&h, &["watch", &run, "--json"]).await;
    assert_eq!(o.status.code(), Some(15), "{}", text(&o.stderr));
    // A run that is complete cannot change: the cancel answers 409 with the reason.
    let done = new_run(&fake, "control-done").await;
    fake.mem
        .inner
        .lock()
        .unwrap()
        .runs
        .get_mut(&done)
        .unwrap()
        .status = harness_protocol::goal::GoalStatus::Complete;
    let o = ham(&h, &["cancel", &done]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("run-finished"),
        "{}",
        text(&o.stderr)
    );
    let o = ham(&h, &["cancel", "01K5ZQ4T8M3N6P7R9S0V1W2X3Y"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("no run"), "{}", text(&o.stderr));
}

/// On the direct stage a cancel with a live agent asks Cline to stop and keeps the work: the run stays active until agentd ends the
/// prompt and the run machine delivers what exists (control-api `controls.rs`).
#[tokio::test]
async fn cancel_asks_the_live_agent_to_stop_and_keeps_the_run() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "control-live").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    let h = home(&fake, "control-live");
    let o = ham(&h, &["cancel", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout).trim(), format!("run {run} active"));
    let g = fake.mem.inner.lock().unwrap();
    assert!(g.stopped.is_empty(), "the run is not stopped");
    assert!(
        g.controls
            .iter()
            .any(|c| c.agent_id == "direct" && c.control == PendingControl::Cancel)
    );
}

#[tokio::test]
async fn pr_shows_the_harness_pr_link_or_says_there_is_none() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "pr").await;
    let h = home(&fake, "pr");
    let o = ham(&h, &["pr", &run]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("no PR yet"), "{}", text(&o.stderr));
    // A PR link written by an agent is not shown (invariant 4).
    fake.mem.agent(
        &run,
        "A1",
        harness_protocol::envelope::EventType::DeliveryPr,
        json!({"url": "https://example.test/other-pr/1"}),
    );
    assert_eq!(ham(&h, &["pr", &run]).await.status.code(), Some(1));
    fake.mem.complete_run(&run, true);
    let o = ham(&h, &["pr", &run, "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let j = one_json(&o);
    assert_eq!(j["prUrl"], fake.mem.pr_url(&run));
    assert_eq!(j["status"], "complete");
    let o = ham(&h, &["pr", &run]).await;
    assert!(text(&o.stdout).contains(&fake.mem.pr_url(&run)));
}

/// A complete `--s3-only` run has no PR by design, so `ham pr`
/// says that and names the two commands that get the code; a run that has not delivered yet still says "no PR yet".
#[tokio::test]
async fn pr_on_an_s3_only_run_says_there_is_no_pr_by_design() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "pr-s3").await;
    let h = home(&fake, "pr-s3");
    let o = ham(&h, &["pr", &run]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("no PR yet"), "{}", text(&o.stderr));
    fake.mem.inner.lock().unwrap().no_pr = true;
    fake.mem.complete_run(&run, true);
    for args in [vec!["pr", run.as_str()], vec!["pr", run.as_str(), "--json"]] {
        let o = ham(&h, &args).await;
        assert_eq!(o.status.code(), Some(1), "{args:?}: exit 1 stays");
        let err = text(&o.stderr);
        assert!(err.contains("no PR by design"), "{err}");
        assert!(err.contains("status complete"), "{err}");
        assert!(err.contains(&format!("ham pull {run}")), "{err}");
        assert!(err.contains(&format!("ham clone {run}")), "{err}");
        assert!(!err.contains("no PR yet"), "{err}");
        assert!(text(&o.stdout).is_empty(), "nothing on stdout");
    }
    // A complete --s3-only run whose delivery.ready has not been announced is still a run with no PR by design: the target says so.
    let run = new_run(&fake, "pr-s3-nozip").await;
    fake.mem.complete_run(&run, false);
    let o = ham(&h, &["pr", &run]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("no PR by design"),
        "{}",
        text(&o.stderr)
    );
}

#[tokio::test]
async fn doctor_prints_one_pass_line_per_check_and_fails_with_a_wrong_profile() {
    // The catalog check on the direct stage reads the five models of `engine/catalog/models.json`; the age rule is
    // `tests/doctor_catalog.rs`.
    let fake = Fake::start().await;
    let h = home(&fake, "doctor");
    let o = ham(&h, &["doctor"]).await;
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    let out = text(&o.stdout);
    let lines: Vec<&str> = out.lines().collect();
    for (i, check) in ["identity", "api", "versions", "catalog", "git-host"]
        .iter()
        .enumerate()
    {
        assert!(
            lines[i].starts_with(&format!("PASS {check} "))
                || (*check == "catalog" && lines[i].starts_with("WARN catalog ")),
            "line {i}: {out}"
        );
    }
    assert!(
        lines[0].contains(support::ME) && lines[0].contains("800000000022"),
        "{out}"
    );
    assert!(lines[3].contains("11 selectable rows"), "{out}");
    let o = ham(&h, &["doctor", "--json"]).await;
    let j = one_json(&o);
    assert!(
        j["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["pass"] == true),
        "{j}"
    );

    // A wrong profile: every check that needs the API fails, and nothing leaks.
    let wrong = home_with(&fake.endpoint, "doctor-wrong", "ham-no-such-profile");
    let o = ham(&wrong, &["doctor"]).await;
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("FAIL identity "), "{out}");
    assert!(out.contains("ham-no-such-profile"), "{out}");
    assert!(
        out.contains("FAIL api ") && out.contains("FAIL catalog "),
        "{out}"
    );
    assert!(out.contains("PASS git-host "), "{out}");
    assert_no_secrets(&o);
    let o = ham(&wrong, &["doctor", "--json"]).await;
    let j = one_json(&o);
    assert_eq!(j["checks"][0]["name"], "identity");
    assert_eq!(j["checks"][0]["pass"], false);
}

#[tokio::test]
async fn link_prints_the_shorter_expiry_of_a_signer_about_to_lapse() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "link").await;
    fake.mem.deliverable(&run, support::zip_bytes(2048));
    let lapse = now_secs() + 120;
    fake.mem.inner.lock().unwrap().signer_expiry = Some(lapse);
    let h = home(&fake, "link");
    let o = ham(&h, &["link", &run, "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let j = one_json(&o);
    assert_eq!(j["expiresAt"], control_api::ids::rfc3339(lapse));
    let o = ham(&h, &["link", &run]).await;
    assert!(
        text(&o.stdout).contains(&format!("(expires {})", control_api::ids::rfc3339(lapse))),
        "{}",
        text(&o.stdout)
    );
}

fn part_len(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_killed_mid_download_resumes_with_only_the_missing_ranges() {
    const MIB: u64 = 1024 * 1024;
    let fake = Fake::start().await;
    let run = new_run(&fake, "kill").await;
    let (_, sha) = fake
        .mem
        .deliverable(&run, support::zip_bytes((10 * MIB) as usize));
    fake.mem.inner.lock().unwrap().range_delay = Duration::from_millis(300);
    let h = home(&fake, "kill");
    let chunk = MIB.to_string();
    let env = [("HAMH_RANGE_BYTES", chunk.as_str())];
    let mut child = home::command(&h, &["pull", &run, "--json"], &env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Kill once the first window of ranges is written, before the download ends. The bound is wall
    // time: under a loaded full coverage run the first MiB took over 20 s of sleep ticks.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while part_len(&h.downloads) < MIB {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if std::time::Instant::now() >= deadline {
            let exited = child.try_wait();
            let served = fake.mem.inner.lock().unwrap().ranges.len();
            panic!(
                "no bytes written in 120 s: child exited={exited:?} ranges served={served} part bytes={}",
                part_len(&h.downloads)
            );
        }
    }
    child.kill().await.unwrap();
    let kept = part_len(&h.downloads);
    assert!(kept > 0 && kept < 10 * MIB, "kept {kept}");
    fake.mem.inner.lock().unwrap().ranges.clear();
    fake.mem.inner.lock().unwrap().range_delay = Duration::ZERO;
    let o = ham_env(&h, &["pull", &run, "--json"], &env).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let j = one_json(&o);
    assert_eq!(j["sha256"], sha);
    assert_eq!(j["resumedFrom"], kept);
    let mut fetched: Vec<(u64, u64)> = fake
        .mem
        .inner
        .lock()
        .unwrap()
        .ranges
        .iter()
        .filter(|(_, s, e)| !(*s == 0 && *e == 0))
        .map(|(_, s, e)| (*s, *e))
        .collect();
    fetched.sort_unstable();
    assert_eq!(fetched.first().map(|r| r.0), Some(kept), "{fetched:?}");
    assert_eq!(fetched.len() as u64, (10 * MIB - kept).div_ceil(MIB));
    // A range size outside 1 KiB-16 MiB is refused before any call.
    let o = ham_env(&h, &["pull", &run], &[("HAMH_RANGE_BYTES", "10")]).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
}

#[tokio::test]
async fn pull_fetches_the_evidence_zip() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "artifacts").await;
    fake.mem.deliverable(&run, support::zip_bytes(3000));
    let (_, ev_sha) = fake.mem.evidence_zip(&run);
    let h = home(&fake, "artifacts");
    let o = ham(&h, &["pull", &run, "--artifact", "evidence", "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let j = one_json(&o);
    assert_eq!(j["sha256"], ev_sha);
    assert!(j["path"].as_str().unwrap().ends_with("evidence.zip"));
    // The selectors of the multi-agent line's bundles and logs are gone.
    let o = ham(&h, &["pull", &run, "--artifact", "bundle:T1"]).await;
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).starts_with("usage:"), "{}", text(&o.stderr));
    let o = ham(&h, &["pull", &run, "--artifact", "partial"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("no matching artifact"),
        "{}",
        text(&o.stderr)
    );
}

#[tokio::test]
async fn runs_and_watch_say_why_a_run_ended_early() {
    // The run-ended reaper clears a run whose root execution was stopped, failed or
    // timed out, writes the execution's end on the Run item and publishes `run.aborted`.
    use harness_protocol::envelope::EventType;
    use harness_protocol::goal::GoalStatus;
    use harness_protocol::orchestration::{ExecutionEnd, RunAbortedPayload};
    let fake = Fake::start().await;
    let run = new_run(&fake, "aborted").await;
    let why = RunAbortedPayload::new(
        ExecutionEnd::Aborted,
        Some("HarnessOperatorStop"),
        Some("operator: orphan check"),
        "2026-09-29T21:30:26Z",
    );
    {
        let mut g = fake.mem.inner.lock().unwrap();
        let r = g.runs.get_mut(&run).unwrap();
        r.status = GoalStatus::Cleared;
        r.aborted = Some(Box::new(why.clone()));
    }
    fake.mem.harness(
        &run,
        EventType::GoalStatus,
        json!({"status": "cleared", "phase": "planning"}),
    );
    fake.mem.harness(
        &run,
        EventType::RunAborted,
        serde_json::to_value(&why).unwrap(),
    );
    let h = home(&fake, "runs-aborted");
    let o = ham(&h, &["runs"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    let line = out.lines().next().unwrap();
    assert!(
        line.starts_with(&format!("{run}  cleared (planning)  s3-only  ")),
        "{out}"
    );
    assert!(
        line.ends_with("  ended early: ABORTED HarnessOperatorStop: operator: orphan check"),
        "{out}"
    );
    let v = one_json(&ham(&h, &["runs", "--json"]).await);
    assert_eq!(v["runs"][0]["aborted"]["status"], "ABORTED");
    assert_eq!(v["runs"][0]["aborted"]["cause"], "operator: orphan check");

    let o = ham(&h, &["watch", &run]).await;
    assert_eq!(o.status.code(), Some(15), "cleared: {}", text(&o.stderr));
    let out = text(&o.stdout);
    assert_eq!(
        out.matches("ended early: ABORTED HarnessOperatorStop: operator: orphan check")
            .count(),
        1,
        "{out}"
    );
    let o = ham(&h, &["watch", &run, "--json"]).await;
    let last = json_lines(&o)
        .into_iter()
        .rfind(|l| l.get("run").is_some())
        .unwrap();
    assert_eq!(last["run"]["status"], "cleared");
    assert_eq!(last["run"]["aborted"]["error"], "HarnessOperatorStop");
}
