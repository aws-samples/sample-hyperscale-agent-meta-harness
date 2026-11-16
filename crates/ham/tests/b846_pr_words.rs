//! `ham pr <runId>` for a run with no pull request says what is true for the state the run is in.
//! A run that ended blocked and a paused zip-only run both read "has no PR yet", as if one would come:
//!
//! - a run that delivered a zip and no pull request has none by design, whatever status it ended with (`complete`, `paused`, `stalled`);
//! - a run that ended blocked or was cleared delivered nothing and opens none, and the line says where its saved work is;
//! - a run that still works may get one: "no PR yet".
//!
//! The binary runs against the local `/v1` API, as in `commands.rs`. The exit code stays 1 and nothing goes to standard output.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use harness_protocol::envelope::EventType;
use home::{ham, home, new_run, text};
use serde_json::json;
use support::Fake;

/// The line `ham pr` writes to standard error for a run with no pull request: plain and with `--json`, exit code 1 both times.
async fn no_pr_lines(h: &home::Home, run: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for args in [vec!["pr", run], vec!["pr", run, "--json"]] {
        let o = ham(h, &args).await;
        assert_eq!(o.status.code(), Some(1), "{args:?}: exit 1 stays");
        assert!(text(&o.stdout).is_empty(), "{args:?}: nothing on stdout");
        lines.push(text(&o.stderr));
    }
    lines
}

/// What the service writes when a zip-only run stops with `status` and delivers its zip: no pull request link.
fn deliver_stopped(fake: &Fake, run: &str, status: &str, label: &str) {
    fake.mem.harness(
        run,
        EventType::GoalStatus,
        json!({"status": status, "phase": "delivering"}),
    );
    fake.mem.harness(
        run,
        EventType::DeliveryReady,
        json!({"artifactId": "01M5A2R7K3C9XJ4D8E6F0000AA", "sha256": "ab".repeat(32), "sizeBytes": 1234}),
    );
    fake.mem.harness(
        run,
        EventType::RunCompleted,
        json!({"status": status, "outcome": "stopped_timeout", "label": label}),
    );
}

#[tokio::test]
async fn b846_pr_of_a_run_that_ended_blocked_says_it_opens_none_and_where_its_work_is() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "pr-blocked").await;
    fake.mem.block_direct_run(
        &run,
        "bundle validation failed: buildspec buildspec.yml",
        None,
    );
    let h = home(&fake, "pr-blocked");
    for err in no_pr_lines(&h, &run).await {
        assert!(
            err.contains(&format!(
                "run {run} has no PR and opens none (status blocked): the run ended without a delivery."
            )),
            "{err}"
        );
        assert!(
            err.contains(&format!("ham pull {run} --snapshot")),
            "the line says where the saved work is: {err}"
        );
        assert!(!err.contains("no PR yet"), "{err}");
        assert!(!err.contains("by design"), "{err}");
    }
}

#[tokio::test]
async fn b846_pr_of_a_paused_or_stalled_zip_only_run_says_it_has_none_by_design() {
    let fake = Fake::start().await;
    let h = home(&fake, "pr-stopped");
    for (status, label) in [
        ("paused", "Cline stopped: the run reached its time limit"),
        ("stalled", "Cline stopped: the session was lost"),
    ] {
        let run = new_run(&fake, &format!("pr-{status}")).await;
        deliver_stopped(&fake, &run, status, label);
        for err in no_pr_lines(&h, &run).await {
            assert!(
                err.contains(&format!("run {run} has no PR by design (status {status})")),
                "{err}"
            );
            assert!(err.contains(&format!("ham pull {run}")), "{err}");
            assert!(err.contains(&format!("ham clone {run}")), "{err}");
            assert!(!err.contains("no PR yet"), "{err}");
        }
    }
}

#[tokio::test]
async fn b846_pr_of_a_cleared_run_says_it_opens_none() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "pr-cleared").await;
    let h = home(&fake, "pr-cleared");
    // A cancel of a run with no live session clears it.
    let o = ham(&h, &["cancel", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    for err in no_pr_lines(&h, &run).await {
        assert!(
            err.contains(&format!(
                "run {run} has no PR and opens none (status cleared): the run ended without a delivery."
            )),
            "{err}"
        );
        assert!(!err.contains("no PR yet"), "{err}");
    }
}

#[tokio::test]
async fn b846_pr_of_a_run_that_still_works_says_no_pr_yet() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "pr-working").await;
    let h = home(&fake, "pr-working");
    for err in no_pr_lines(&h, &run).await {
        assert!(
            err.contains(&format!("run {run} has no PR yet (status active)")),
            "{err}"
        );
    }
}
