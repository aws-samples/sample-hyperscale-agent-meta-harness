//! `ham cancel` through the binary against the local `/v1` API (the real control-api handler).
//!
//! A cancel that keeps the work is recorded by the service, and the client says so at once: after the status line it prints that the
//! cancel is recorded, what follows for the run's session, and the command that downloads the newest saved copy. `ham runs` marks the run
//! `cancel recorded` while it is active. `ham cancel <runId> --discard` is the explicit clear: it sends `?discard=true`, the run is
//! cleared and its execution stopped. A cancel without the option never clears a run that has a session record.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use harness_protocol::api::v1::RunSummary;
use harness_protocol::goal::GoalStatus;
use home::{ham, home, json_lines, new_run, text};
use serde_json::json;
use support::Fake;

fn status_of(fake: &Fake, run: &str) -> GoalStatus {
    fake.mem.inner.lock().unwrap().runs[run].status
}

fn recorded_at(fake: &Fake, run: &str) -> Option<String> {
    fake.mem.inner.lock().unwrap().runs[run]
        .cancel_requested_at
        .clone()
}

#[tokio::test]
async fn b853_ham_cancel_prints_the_record() {
    let fake = Fake::start().await;
    // A session that the service has not ended: it is asked to stop.
    let run = new_run(&fake, "b853-live").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    let h = home(&fake, "b853-live");
    let o = ham(&h, &["cancel", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], format!("run {run} active"), "{out}");
    assert!(
        lines[1].starts_with("The cancel is recorded (")
            && lines[1].ends_with(
                "UTC). The run's session is asked to stop and the work so far is kept: the run delivers what exists."
            ),
        "{out}"
    );
    assert!(
        lines[2].starts_with("If its session does not answer, the service calls it lost")
            && lines[2].ends_with(
                "starts no further session and delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
            ),
        "{out}"
    );
    assert_eq!(
        lines[3],
        format!(
            "ham pull {run} --snapshot downloads the newest saved copy of the work at any time, if the run saved one."
        ),
        "{out}"
    );
    assert_eq!(lines.len(), 4, "{out}");
    let at = recorded_at(&fake, &run).expect("the service recorded the cancel");
    assert_eq!(status_of(&fake, &run), GoalStatus::Active);

    // `--json` carries the record, with the time of the first cancel.
    let o = ham(&h, &["cancel", &run, "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let lines = json_lines(&o);
    assert_eq!(lines.len(), 1, "{}", text(&o.stdout));
    assert_eq!(lines[0]["status"], json!("active"));
    assert_eq!(lines[0]["cancel"]["requestedAt"], json!(at));
    assert_eq!(lines[0]["cancel"]["sessionLive"], json!(true));

    // The run list says it while the run is active.
    let o = ham(&h, &["runs"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let listed = text(&o.stdout);
    let row = listed
        .lines()
        .find(|l| l.starts_with(run.as_str()))
        .expect("the run is listed");
    assert!(row.contains(") \u{b7} cancel recorded  "), "{row}");
    let o = ham(&h, &["runs", "--json"]).await;
    let listed = json_lines(&o);
    let mine = listed[0]["runs"]
        .as_array()
        .expect("one line with the runs")
        .iter()
        .find(|r| r["runId"] == json!(run))
        .expect("the run is listed");
    assert_eq!(mine["cancelRequestedAt"], json!(at), "{mine}");

    // No session that answers: the explicit clear is named, and nothing is cleared.
    let gone = new_run(&fake, "b853-gone").await;
    fake.mem.add_agent(&gone, "direct", "root", "stopped");
    let o = ham(&h, &["cancel", &gone]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], format!("run {gone} active"), "{out}");
    assert!(
        lines[1].ends_with(
            "The run has no session that answers: the service starts no further session and delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one."
        ),
        "{out}"
    );
    assert_eq!(
        lines[2],
        format!(
            "ham pull {gone} --snapshot downloads the newest saved copy of the work at any time, if the run saved one. ham cancel {gone} --discard ends the run now and delivers nothing."
        ),
        "{out}"
    );
    assert_eq!(
        status_of(&fake, &gone),
        GoalStatus::Active,
        "nothing is cleared"
    );
    assert!(fake.mem.inner.lock().unwrap().stopped.is_empty());

    // A run with no session record is cleared as before, and the answer carries no record: today's lines.
    let fresh = new_run(&fake, "b853-fresh").await;
    let o = ham(&h, &["cancel", &fresh]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.starts_with(&format!("run {fresh} cleared")), "{out}");
    assert!(!out.contains("The cancel is recorded"), "{out}");
}

#[tokio::test]
async fn b853_discard_is_the_explicit_clear() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "b853-discard").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    let h = home(&fake, "b853-discard");
    // TRIGGER PRESENT: without the option the run is not cleared.
    let o = ham(&h, &["cancel", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(status_of(&fake, &run), GoalStatus::Active);
    let at = recorded_at(&fake, &run);
    assert!(at.is_some());

    let o = ham(&h, &["cancel", &run, "--discard"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.starts_with(&format!("run {run} cleared")), "{out}");
    assert!(
        !out.contains("The cancel is recorded"),
        "a discard prints the lines of a cleared run: {out}"
    );
    assert_eq!(status_of(&fake, &run), GoalStatus::Cleared);
    {
        let g = fake.mem.inner.lock().unwrap();
        assert_eq!(g.stopped.len(), 1, "the run's execution is stopped");
        assert_eq!(g.stopped[0].0, run);
    }
    assert_eq!(recorded_at(&fake, &run), at, "the record stays as history");
    // The option before the run id, and its JSON form.
    let other = new_run(&fake, "b853-discard-2").await;
    fake.mem.add_agent(&other, "direct", "root", "stopped");
    let o = ham(&h, &["cancel", "--discard", &other, "--json"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        json_lines(&o),
        vec![json!({"runId": other, "status": "cleared"})]
    );
    // The help names the option.
    let o = ham(&h, &["cancel", "--help"]).await;
    assert!(text(&o.stdout).contains("--discard"), "{}", text(&o.stdout));
}

fn row(extra: serde_json::Value) -> RunSummary {
    let mut v = json!({"runId": "01M6C3D78J6WJM6ADMEPMRMJ38", "status": "active", "phase": "green",
        "target": {"mode": "s3-only"}, "createdAt": "2026-11-16T07:00:00Z",
        "ownerPrincipalArn": "arn:aws:sts::111122223333:assumed-role/Admin/tester"});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

/// The words after the phase come from the service's members alone: no client clock is involved.
#[test]
fn b853_the_runs_line_says_what_the_service_says_about_the_session() {
    let plain = ham::cli::runs_line(&row(json!({})));
    assert!(
        plain.starts_with(
            "01M6C3D78J6WJM6ADMEPMRMJ38  active (green)  s3-only  2026-11-16T07:00:00Z"
        ),
        "today's row: {plain}"
    );
    let cancelled = ham::cli::runs_line(&row(json!({"cancelRequestedAt": "2026-11-16T07:05:22Z"})));
    assert!(
        cancelled.starts_with(
            "01M6C3D78J6WJM6ADMEPMRMJ38  active (green) \u{b7} cancel recorded  s3-only  2026-11-16T07:00:00Z"
        ),
        "{cancelled}"
    );
    let silent = ham::cli::runs_line(&row(json!({"sessionSilentSince": "2026-11-16T07:05:22Z"})));
    assert!(
        silent.starts_with(
            "01M6C3D78J6WJM6ADMEPMRMJ38  active (green) \u{b7} session silent since 07:05 UTC  s3-only  2026-11-16T07:00:00Z"
        ),
        "{silent}"
    );
    // A recorded cancel wins over the silent word.
    let both = ham::cli::runs_line(&row(
        json!({"cancelRequestedAt": "2026-11-16T07:05:22Z", "sessionSilentSince": "2026-11-16T07:05:22Z"}),
    ));
    assert!(
        both.contains("\u{b7} cancel recorded") && !both.contains("silent"),
        "{both}"
    );
    // A run that has ended says neither, whatever the row carries; a time that is no UTC time is not printed.
    let ended = ham::cli::runs_line(&row(
        json!({"status": "complete", "cancelRequestedAt": "2026-11-16T07:05:22Z", "sessionSilentSince": "2026-11-16T07:05:22Z"}),
    ));
    assert!(!ended.contains('\u{b7}'), "{ended}");
    let odd = ham::cli::runs_line(&row(json!({"sessionSilentSince": "\u{1b}[31msoon"})));
    assert!(!odd.contains('\u{b7}') && !odd.contains('\u{1b}'), "{odd}");
}

/// The one line that `ham watch` prints for a recorded cancel ends with the command that downloads the
/// newest saved copy of the work, and the line is printed once.
#[test]
fn b853_ham_watch_says_the_recorded_cancel_once_with_the_command_of_the_newest_copy() {
    use ham::watch::InputMemo;
    use ham_core::state::RunState;
    use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
    const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";
    let ev = |n: u64, ts: &str, t: EventType, authority: Authority, payload: serde_json::Value| {
        Envelope {
            schema_version: 1,
            event_id: format!("01J9ZZZZZZ00000000000000{n:02}"),
            run_id: RUN.into(),
            task_id: None,
            agent_id: (authority == Authority::Agent).then(|| "direct".to_string()),
            seq: n,
            ts: ts.into(),
            event_type: t,
            authority,
            payload: Payload::Inline { value: payload },
        }
    };
    let working = vec![
        ev(
            1,
            "2026-11-16T07:03:00Z",
            EventType::RunCreated,
            Authority::Harness,
            json!({"target": "s3-only"}),
        ),
        ev(
            2,
            "2026-11-16T07:03:01Z",
            EventType::GoalStatus,
            Authority::Harness,
            json!({"status": "active", "phase": "green"}),
        ),
        ev(
            3,
            "2026-11-16T07:03:10Z",
            EventType::TurnStarted,
            Authority::Agent,
            json!({"turn": 1, "engine": "cline", "model": "us.anthropic.claude-opus-5-5", "role": "direct", "session": 1, "beatS": 60}),
        ),
    ];
    let mut state = RunState::new(RUN);
    for e in working {
        state.apply(e);
    }
    let mut memo = InputMemo::new();
    assert!(
        !memo
            .lines(&state.view())
            .iter()
            .any(|l| l.contains("Cancel recorded")),
        "TRIGGER PRESENT: a run without the event prints no such line"
    );
    state.apply(ev(
        4,
        "2026-11-16T07:05:22Z",
        EventType::RunWaiting,
        Authority::Harness,
        json!({
            "kind": "cancel-recorded", "requestedAt": "2026-11-16T07:05:22Z", "session": 1, "sessionLive": true,
            "lastHeartbeatAt": "2026-11-16T07:05:20Z", "lostAfter": "2026-11-16T07:15:20Z"
        }),
    ));
    let lines = memo.lines(&state.view());
    let said: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("Cancel recorded at"))
        .collect();
    assert_eq!(said.len(), 1, "{lines:?}");
    assert!(
        said[0].ends_with(&format!(
            "delivers the files it read back from the session's storage when its checks leave no doubt, and otherwise the last saved copy, if the run saved one. ham pull {RUN} --snapshot downloads the newest saved copy of the work at any time, if the run saved one."
        )),
        "{lines:?}"
    );
    // The next read of the same view says nothing again.
    let again = memo.lines(&state.view());
    assert!(
        !again.iter().any(|l| l.contains("Cancel recorded")),
        "{again:?}"
    );
}
