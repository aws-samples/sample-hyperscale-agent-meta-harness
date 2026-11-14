//! What `ham` says about the saved copy of a run's work. However a run ended, `ham pull <runId> --snapshot` downloads its latest
//! work and says what the copy is; the commands that need a zip (`ham pull`, `ham clone`, `ham link`) name that command for a run without
//! one, in words that are true for the run's status. The text tests are pure; the command tests run the compiled binary against the
//! stand-in `/v1` API (the real control-api handler over an in-memory store).
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::Api;
use ham::chat_routes::SnapshotFetched;
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::snapshot_cmd::{json_of, text};
use ham_core::snapshot::SNAPSHOT_REF;
use harness_protocol::goal::GoalStatus;
use home::{Home, ham, home};
use serde_json::{Value, json};
use std::sync::Arc;
use support::{Fake, REGION, creds};

const RUN: &str = "01M76EJVX4BSZ814X2DVVN834F";
const BASE: &str = "f6dc8e0f72e20000000000000000000000000000";
const FINAL: &str = "ad2645e679230000000000000000000000000000";
const EARLIER: &str = "cced2ef89ca70000000000000000000000000000";

const ALL: [GoalStatus; 8] = [
    GoalStatus::Active,
    GoalStatus::Paused,
    GoalStatus::Blocked,
    GoalStatus::UsageLimited,
    GoalStatus::BudgetLimited,
    GoalStatus::Complete,
    GoalStatus::Cleared,
    GoalStatus::Stalled,
];

fn fetched(
    status: GoalStatus,
    head: &str,
    final_copy: bool,
    engine: Option<&str>,
) -> SnapshotFetched {
    SnapshotFetched {
        path: "/downloads/x-snapshot-9.bundle".into(),
        head_sha: head.into(),
        base_sha: Some(BASE.into()),
        taken_at: "2026-11-12T00:47:40Z".into(),
        count: Some(9),
        size_bytes: 186_834,
        run_status: status,
        record_stale: false,
        final_copy,
        engine: engine.map(str::to_string),
    }
}

#[test]
fn b823_the_copy_of_the_end_is_said_to_be_the_work_as_the_engine_left_it() {
    let t = text(
        RUN,
        &fetched(GoalStatus::Blocked, FINAL, true, Some("cline")),
    );
    assert!(
        t.contains("Saved     the work as Cline left it (saved when Cline stopped)"),
        "{t}"
    );
    assert!(!t.contains("are not in it"), "{t}");
    // The engine that ran is the one named.
    let t = text(
        RUN,
        &fetched(GoalStatus::Blocked, FINAL, true, Some("opencode")),
    );
    assert!(
        t.contains("Saved     the work as OpenCode left it (saved when OpenCode stopped)"),
        "{t}"
    );
    // The JSON form carries the fact.
    let v = json_of(
        RUN,
        &fetched(GoalStatus::Blocked, FINAL, true, Some("cline")),
    );
    assert_eq!(v["snapshot"]["finalCopy"], true);
}

#[test]
fn b823_a_copy_made_while_the_run_worked_says_what_is_not_in_it() {
    for status in ALL {
        let t = text(RUN, &fetched(status, EARLIER, false, Some("cline")));
        assert!(
            t.contains(
                "Saved     while the run worked: changes after 2026-11-12T00:47:40Z are not in it"
            ),
            "{status:?}: {t}"
        );
        assert!(!t.contains("left it"), "{status:?}: {t}");
    }
    assert_eq!(
        json_of(RUN, &fetched(GoalStatus::Active, EARLIER, false, None))["snapshot"]["finalCopy"],
        false
    );
}

#[test]
fn b823_the_note_about_the_run_is_true_for_every_status() {
    let note = |status: GoalStatus| -> Vec<String> {
        text(RUN, &fetched(status, FINAL, true, Some("cline")))
            .lines()
            .filter(|l| l.starts_with("Note"))
            .map(str::to_string)
            .collect()
    };
    assert!(
        note(GoalStatus::Active).is_empty(),
        "a run that works needs no note"
    );
    for delivered in [
        GoalStatus::Complete,
        GoalStatus::Paused,
        GoalStatus::Stalled,
    ] {
        assert_eq!(
            note(delivered),
            vec![format!(
                "Note      the run delivered a zip: ham pull {RUN} downloads it"
            )],
            "{delivered:?}"
        );
    }
    assert_eq!(
        note(GoalStatus::Blocked),
        vec![
            "Note      the run is blocked and delivered no zip: this copy is its work".to_string()
        ]
    );
    assert_eq!(
        note(GoalStatus::Cleared),
        vec![
            "Note      the run was cleared and delivered nothing: this copy is its work"
                .to_string()
        ]
    );
    for limited in [GoalStatus::UsageLimited, GoalStatus::BudgetLimited] {
        assert_eq!(
            note(limited),
            vec![
                "Note      the run stopped at a limit and delivered no zip: this copy is its work"
                    .to_string()
            ],
            "{limited:?}"
        );
    }
    // The sentence of the earlier releases, which was wrong for a run that delivered nothing, is gone.
    for status in ALL {
        let t = text(RUN, &fetched(status, FINAL, true, Some("cline")));
        assert!(!t.contains("its delivered result is"), "{status:?}: {t}");
    }
}

#[test]
fn b823_a_copy_of_the_end_that_equals_the_start_says_that_no_file_changed() {
    let t = text(
        RUN,
        &fetched(GoalStatus::Blocked, BASE, true, Some("cline")),
    );
    assert!(
        t.contains("Note      no file differs from the run's start"),
        "{t}"
    );
    let t = text(
        RUN,
        &fetched(GoalStatus::Blocked, FINAL, true, Some("cline")),
    );
    assert!(!t.contains("no file differs"), "{t}");
}

// ---- the commands, against the stand-in service ------------------------------------------------------------------------------------

fn bundle(head: &str) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{head} {SNAPSHOT_REF}\n\nPACK").into_bytes();
    b.extend((0..20_000).map(|i| (i % 251) as u8));
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

fn set_status(fake: &Fake, run: &str, status: GoalStatus) {
    fake.mem
        .inner
        .lock()
        .unwrap()
        .runs
        .get_mut(run)
        .unwrap()
        .status = status;
}

/// The copy agentd stores: the bundle, then the record; the copy of the end carries `final`.
fn save_copy(fake: &Fake, run: &str, head: &str, count: u32, last: bool) {
    let prefix = harness_protocol::direct::s3::turn_prefix(run);
    let mut record = json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE, "at": "2026-11-12T00:47:40Z", "count": count});
    if last {
        record["final"] = json!(true);
    }
    let mut g = fake.mem.inner.lock().unwrap();
    g.objects.insert(
        format!("{prefix}snapshot.bundle"),
        (bundle(head), "\"etag-b\"".into()),
    );
    g.objects.insert(
        format!("{prefix}snapshot.json"),
        (record.to_string().into_bytes(), "\"etag-j\"".into()),
    );
}

async fn shell(h: &Home, args: &[&str]) -> (i32, String, String) {
    let o = ham(h, args).await;
    (
        o.status.code().unwrap(),
        home::text(&o.stdout),
        home::text(&o.stderr),
    )
}

/// The run of 2026-11-11: blocked at delivery. One command returns the final work, and the commands that need a zip name it.
#[tokio::test]
async fn b823_a_blocked_run_gives_its_final_work_with_one_command() {
    let fake = Fake::start().await;
    let run = start(&fake, "Build a market terminal").await;
    save_copy(&fake, &run, FINAL, 9, true);
    set_status(&fake, &run, GoalStatus::Blocked);
    let h = home(&fake, "b823-blocked");

    let (code, out, err) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(code, 0, "{out} {err}");
    let file = h.downloads.join(format!("{run}-snapshot-9.bundle"));
    assert_eq!(
        std::fs::read(&file).unwrap(),
        bundle(FINAL),
        "the final head's bundle is on disk"
    );
    assert!(out.contains(&format!("Commit    {FINAL}")), "{out}");
    assert!(
        out.contains("Saved     the work as Cline left it (saved when Cline stopped)"),
        "{out}"
    );
    assert!(
        out.contains("Note      the run is blocked and delivered no zip: this copy is its work"),
        "{out}"
    );
    assert!(out.contains("Look      git clone -b snapshot"), "{out}");

    // The commands that need a zip say where the work is, in words for a run that has ended.
    let want = format!(
        "ham: run {run} ended blocked and delivered no zip. ham pull {run} --snapshot downloads the latest saved copy of its work."
    );
    for args in [
        vec!["pull", run.as_str()],
        vec!["clone", run.as_str()],
        vec!["link", run.as_str()],
    ] {
        let (code, out, err) = shell(&h, &args).await;
        assert_eq!(code, 1, "{args:?}: {out} {err}");
        assert_eq!(err.trim(), want, "{args:?}");
        assert!(
            !err.contains("yet") && !err.contains("ham watch"),
            "{args:?}: {err}"
        );
    }
}

#[tokio::test]
async fn b823_every_ended_status_downloads_its_copy_with_the_same_command() {
    let fake = Fake::start().await;
    for (n, status) in ALL.into_iter().enumerate() {
        let run = start(&fake, &format!("Run number {n}")).await;
        save_copy(&fake, &run, FINAL, 3, true);
        set_status(&fake, &run, status);
        let h = home(&fake, &format!("b823-status-{n}"));
        let (code, out, err) = shell(&h, &["pull", &run, "--snapshot", "--json"]).await;
        assert_eq!(code, 0, "{status:?}: {out} {err}");
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["snapshot"]["headSha"], FINAL, "{status:?}");
        assert_eq!(v["snapshot"]["finalCopy"], true, "{status:?}");
    }
}

#[tokio::test]
async fn b823_a_run_that_works_and_has_no_zip_is_told_to_follow_it_and_where_its_copy_is() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run that works").await;
    let h = home(&fake, "b823-active");
    let (code, _, err) = shell(&h, &["pull", &run]).await;
    assert_eq!(code, 1);
    assert_eq!(
        err.trim(),
        format!(
            "ham: run {run} is still working and has no zip yet. Follow it with ham watch {run}; ham pull {run} --snapshot downloads the latest saved copy of its work."
        )
    );
}

#[tokio::test]
async fn b823_a_run_without_a_copy_says_so_in_words_for_its_status() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run with nothing saved").await;
    let h = home(&fake, "b823-none");
    let (code, _, err) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(code, 1);
    assert!(
        err.contains(
            "no copy of this run's work is saved yet; ask again when the run has changed a file"
        ),
        "{err}"
    );
    set_status(&fake, &run, GoalStatus::Blocked);
    let (code, _, err) = shell(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("no copy of this run's work was saved"),
        "{err}"
    );
    assert!(!err.contains("yet") && !err.contains("agentd"), "{err}");
}

#[tokio::test]
async fn b823_cancelling_a_run_into_cleared_names_the_command_for_its_copy() {
    let fake = Fake::start().await;
    let run = start(&fake, "A run to clear").await;
    let h = home(&fake, "b823-clear");
    // No live session in the stand-in: a cancel clears the run.
    let (code, out, err) = shell(&h, &["cancel", &run]).await;
    assert_eq!(code, 0, "{out} {err}");
    assert!(out.contains(&format!("run {run} cleared")), "{out}");
    assert!(
        out.contains(&format!(
            "ham pull {run} --snapshot downloads the latest saved copy of its work."
        )),
        "{out}"
    );
}
