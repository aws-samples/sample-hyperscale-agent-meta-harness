//! What the client says when the service read a lost session's storage back.
//!
//! The event of a lost session carries these new facts: the cause `end-requested` with the time of the worker's notice, `from: recovered`
//! when the next session starts from the files read back, the cause `restore-failed` for a session that could not start from them, what
//! came of the read (`recovery`, with `kept` when the files read back are stored) and the session whose storage the files the run goes
//! on from were read back from (`readBackSession`). These tests hold the sentences for those facts word for word, in both tenses and
//! on the card. Every sentence about files that were read back and are not used names the command that downloads them. An event of
//! the shape before the read keeps the words it had.
#![allow(clippy::unwrap_used)]

use ham_core::lost_session::{
    CAUSE_END_REQUESTED, CAUSE_NO_HEARTBEAT, CAUSE_RESTORE_FAILED, DELIVERS_COPY, FROM_COPY,
    FROM_RECOVERED, FROM_START, KIND, LostRecovery, SessionLost, THEN_END, THEN_RESTART,
};
use harness_protocol::direct::{MAX_LOST_SLICES, SESSION_LOST_AFTER_S};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use serde_json::{Value, json};

const RUN: &str = "01JABCDEFGHJKMNPQRSTVWXYZ0";

/// Session 1 was asked to end at 14:03:07; the service decided at 14:04:22; session 2 starts from the files read back.
fn read_back() -> SessionLost {
    SessionLost {
        cancelled_at: None,
        kind: KIND.to_string(),
        session: 1,
        decided_at: "2026-11-16T14:04:22Z".into(),
        last_heartbeat_at: Some("2026-11-16T14:02:40Z".into()),
        cause: CAUSE_END_REQUESTED.to_string(),
        silence_s: SESSION_LOST_AFTER_S,
        lost_in_a_row: 1,
        limit: MAX_LOST_SLICES,
        then: THEN_RESTART.to_string(),
        next_session: Some(2),
        from: Some(FROM_RECOVERED.to_string()),
        saved_at: None,
        delivers: None,
        ended_at: Some("2026-11-16T14:03:07Z".into()),
        recovery: Some(LostRecovery {
            tried: true,
            used: true,
            kept: true,
            ..LostRecovery::default()
        }),
        read_back_session: Some(1),
        run_id: RUN.to_string(),
    }
}

/// Session 1 was asked to end; the service read its storage back, stored the files and did not use them for `reason`; session 2
/// starts from the copy saved at 14:02:51.
fn kept_and_not_used(reason: Option<&str>) -> SessionLost {
    SessionLost {
        from: Some(FROM_COPY.to_string()),
        saved_at: Some("2026-11-16T14:02:51Z".into()),
        recovery: Some(LostRecovery {
            tried: true,
            used: false,
            reason: reason.map(str::to_string),
            kept: true,
            ..LostRecovery::default()
        }),
        read_back_session: None,
        ..read_back()
    }
}

/// The sentence that names the command, as every notice of files that are stored and not used ends.
const DOWNLOAD: &str =
    "Download the files read back with `ham pull 01JABCDEFGHJKMNPQRSTVWXYZ0 --read-back 1`.";
const HEAD: &str = "Session 1 was asked to end from outside at 2026-11-16 14:03 UTC.";

fn event(payload: Value) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: "01M3B8X7YK000000000000001W".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 1,
        ts: "2026-11-16T14:04:22Z".into(),
        event_type: EventType::RunWaiting,
        authority: Authority::Harness,
        payload: Payload::Inline { value: payload },
    }
}

#[test]
fn b854_a_session_that_was_asked_to_end_and_read_back_is_told_in_these_words() {
    let lost = read_back();
    assert_eq!(
        lost.sentence(),
        "Session 1 was asked to end from outside at 2026-11-16 14:03 UTC. Session 2 starts from the files the service read back from \
         session 1's storage after it ended. They hold what was written and closed up to about a second before the end; a file that was \
         still being written may lack its last lines."
    );
    assert_eq!(
        lost.sentence_started(),
        "Session 1 was asked to end from outside at 2026-11-16 14:03 UTC. Session 2 started from the files the service read back from \
         session 1's storage after it ended. They hold what was written and closed up to about a second before the end; a file that was \
         still being written may lack its last lines."
    );
    assert_eq!(
        lost.card_row(),
        "Lost      session 1: asked to end 2026-11-16 14:03 UTC; session 2 started from the files read back from session 1's storage"
    );
    // The words claim no more than was measured: about a second, for files that were closed; nothing says the work is whole.
    for claim in [
        "nothing is lost",
        "no work is lost",
        "every file",
        "all files",
        "verified",
    ] {
        assert!(!lost.sentence().contains(claim), "{claim}");
    }
}

#[test]
fn b854_silence_that_was_read_back_keeps_its_head_and_names_the_files_read_back() {
    let mut lost = read_back();
    lost.cause = CAUSE_NO_HEARTBEAT.to_string();
    lost.ended_at = None;
    assert_eq!(
        lost.sentence(),
        "Session 1 was lost: the service received no heartbeat from it for 600 seconds (the last one came at 2026-11-16 14:02 UTC). \
         Session 2 starts from the files the service read back from session 1's storage after it ended. They are the files that storage \
         held; a file that was still being written may lack its last lines."
    );
    assert!(
        lost.card_row()
            .ends_with("session 2 started from the files read back from session 1's storage"),
        "{}",
        lost.card_row()
    );
    assert!(
        lost.card_row()
            .starts_with("Lost      session 1: last heartbeat 2026-11-16 14:02 UTC, called lost "),
        "{}",
        lost.card_row()
    );
}

#[test]
fn b854_a_notice_without_its_time_is_told_without_one() {
    let mut lost = read_back();
    lost.ended_at = None;
    assert!(
        lost.sentence()
            .starts_with("Session 1 was asked to end from outside. Session 2 starts from"),
        "{}",
        lost.sentence()
    );
}

#[test]
fn b854_a_session_that_could_not_start_from_the_files_read_back_is_told_as_that() {
    let lost = SessionLost {
        session: 2,
        cause: CAUSE_RESTORE_FAILED.to_string(),
        next_session: Some(3),
        from: Some(FROM_COPY.to_string()),
        saved_at: Some("2026-11-16T14:02:51Z".into()),
        ended_at: None,
        recovery: None,
        ..read_back()
    };
    assert!(
        lost.sentence().starts_with("Session 2 could not start from the files read back from session 1's storage. Session 3 starts from the copy saved at 2026-11-16 14:02 UTC"),
        "{}",
        lost.sentence()
    );
    assert!(
        lost.sentence_started().starts_with("Session 2 could not start from the files read back from session 1's storage. Session 3 started from the copy saved at 2026-11-16 14:02 UTC"),
        "{}",
        lost.sentence_started()
    );
}

#[test]
fn b854_the_event_is_read_with_its_new_members_made_safe_and_an_older_event_reads_as_before() {
    // The event as the service writes it for a read that was not used.
    let payload = json!({
        "kind": "session-lost", "session": 1, "decidedAt": "2026-11-16T14:04:22Z", "lastHeartbeatAt": "2026-11-16T14:02:40Z",
        "cause": "end-requested", "silenceS": 600, "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 2,
        "from": "copy", "savedAt": "2026-11-16T14:02:51Z", "endedAt": "2026-11-16T14:03:07Z",
        "recovery": {"tried": true, "used": false, "reason": "older-file\u{1b}[31m", "regressed": 2, "regressedPath": "notes/\u{7}log.txt",
                     "newestWriteAt": "not a time"},
        "aMemberOfALaterService": 1
    });
    let lost = SessionLost::parse(&event(payload)).unwrap();
    assert_eq!(lost.cause, "end-requested");
    assert_eq!(lost.ended_at.as_deref(), Some("2026-11-16T14:03:07Z"));
    let r = lost.recovery.as_ref().unwrap();
    assert!((r.tried, r.used) == (true, false));
    assert_eq!(r.regressed, Some(2));
    assert!(
        !r.reason.as_deref().unwrap().contains('\u{1b}'),
        "{:?}",
        r.reason
    );
    assert!(
        !r.regressed_path.as_deref().unwrap().contains('\u{7}'),
        "{:?}",
        r.regressed_path
    );
    assert_eq!(
        r.newest_write_at, None,
        "a time that is no UTC time is dropped"
    );
    // A notice time that is no time is dropped, and the event is still read.
    let odd = json!({
        "kind": "session-lost", "session": 1, "decidedAt": "2026-11-16T14:04:22Z", "cause": "end-requested", "silenceS": 600,
        "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 2, "from": "recovered", "endedAt": "yesterday"
    });
    let lost = SessionLost::parse(&event(odd)).unwrap();
    assert_eq!(lost.ended_at, None);
    assert_eq!(lost.from.as_deref(), Some(FROM_RECOVERED));

    // An event of the shape before the read: no new member, the words it always had.
    let before = json!({
        "kind": "session-lost", "session": 1, "decidedAt": "2026-11-16T14:14:10Z", "lastHeartbeatAt": "2026-11-16T14:04:10Z",
        "cause": "no-heartbeat", "silenceS": 600, "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 2,
        "from": "copy", "savedAt": "2026-11-16T14:03:55Z"
    });
    let lost = SessionLost::parse(&event(before.clone())).unwrap();
    assert_eq!((lost.ended_at.clone(), lost.recovery.clone()), (None, None));
    assert_eq!(
        lost.sentence(),
        "Session 1 was lost: the service received no heartbeat from it for 600 seconds (the last one came at 2026-11-16 14:04 UTC). \
         Session 2 starts from the copy saved at 2026-11-16 14:03 UTC; file changes after that copy are lost."
    );
    assert_eq!(
        serde_json::to_value(&lost).unwrap(),
        before,
        "and it is written back as it was read"
    );
}

/// The third lost session in a row ends the run. The service read its storage back and chose those files, so they are what the run
/// delivers, and the notice says so. A run that ends this way with the copy read back not used keeps the words it had.
#[test]
fn b854_a_run_that_ends_on_its_third_lost_session_says_that_it_delivers_the_files_read_back() {
    let ended = SessionLost {
        lost_in_a_row: MAX_LOST_SLICES,
        then: THEN_END.to_string(),
        next_session: None,
        from: None,
        delivers: Some(DELIVERS_COPY.to_string()),
        ..read_back()
    };
    assert_eq!(
        ended.sentence(),
        "Session 1 was asked to end from outside at 2026-11-16 14:03 UTC. It was lost session 3 in a row, so the run ends and the \
         service delivers the files it read back from session 1's storage."
    );
    assert_eq!(ended.sentence_started(), ended.sentence());
    // The copy read back was not used: the run delivers the last saved copy, as before.
    let not_used = SessionLost {
        recovery: Some(LostRecovery {
            tried: true,
            used: false,
            ..LostRecovery::default()
        }),
        read_back_session: None,
        ..ended.clone()
    };
    assert_eq!(
        not_used.sentence(),
        "Session 1 was asked to end from outside at 2026-11-16 14:03 UTC. It was lost session 3 in a row, so the run ends and the \
         service delivers the last saved copy."
    );
    // An event of the shape before the read has no such member and keeps its words.
    let before = SessionLost {
        recovery: None,
        read_back_session: None,
        ..ended
    };
    assert!(
        before
            .sentence()
            .ends_with("so the run ends and the service delivers the last saved copy."),
        "{}",
        before.sentence()
    );
}

/// The bound of about a second was measured for a session that was stopped from outside. For a session that went silent, or that ended
/// without a result in another way, nothing was measured, so the words name the files and state no bound.
#[test]
fn b854_only_an_end_that_was_asked_for_states_the_bound_of_a_second() {
    for cause in [CAUSE_NO_HEARTBEAT, "no-result"] {
        let mut lost = read_back();
        lost.cause = cause.to_string();
        lost.ended_at = None;
        for said in [lost.sentence(), lost.sentence_started()] {
            assert!(!said.contains("about a second"), "{cause}: {said}");
            assert!(
                said.ends_with(
                    "from session 1's storage after it ended. They are the files that storage held; a file that was still being \
                     written may lack its last lines."
                ),
                "{cause}: {said}"
            );
        }
    }
    // Trigger: the end that was asked for keeps the bound, in both tenses.
    let lost = read_back();
    for said in [lost.sentence(), lost.sentence_started()] {
        assert!(
            said.contains("up to about a second before the end"),
            "TRIGGER PRESENT {said}"
        );
    }
}

/// The files the service read back are stored and the run does not go on from them. The notice no longer says that the file changes
/// after the saved copy are lost: they are in the stage's bucket. It says why they are not used and names the one command that
/// downloads them, in both tenses and on the card. Before this change the notice said the changes were lost and gave no reason.
#[test]
fn b854_files_that_were_read_back_and_not_used_are_told_with_the_reason_and_the_command_that_downloads_them()
 {
    let not_used = "The service read session 1's storage back and did not use it";
    let cases: [(Option<&str>, String); 6] = [
        (
            Some("storage-older"),
            format!("{not_used}: the storage did not prove to be newer than that copy."),
        ),
        (
            Some("superseded"),
            format!("{not_used}: a newer copy was saved while it read."),
        ),
        (
            Some("unstable"),
            "The service did not use session 1's storage: its files were still changing."
                .to_string(),
        ),
        (
            Some("abandoned"),
            "The service could not read session 1's storage back within 120 seconds.".to_string(),
        ),
        (
            Some("not-whole"),
            "The service could not read session 1's storage back whole.".to_string(),
        ),
        // A doubt the service has no word for: files without a readable history, a comparison that was not made, a file that is an
        // earlier version than the saved copy's.
        (None, format!("{not_used}: its checks left a doubt.")),
    ];
    for (reason, why) in cases {
        let lost = kept_and_not_used(reason);
        assert_eq!(
            lost.sentence(),
            format!(
                "{HEAD} Session 2 starts from the copy saved at 2026-11-16 14:02 UTC. {why} {DOWNLOAD}"
            ),
            "{reason:?}"
        );
        assert_eq!(
            lost.sentence_started(),
            format!(
                "{HEAD} Session 2 started from the copy saved at 2026-11-16 14:02 UTC. {why} {DOWNLOAD}"
            ),
            "{reason:?}"
        );
        assert_eq!(
            lost.card_row(),
            "Lost      session 1: asked to end 2026-11-16 14:03 UTC; session 2 started from the copy saved at 2026-11-16 14:02 UTC; the \
             storage was read back and not used; ham pull 01JABCDEFGHJKMNPQRSTVWXYZ0 --read-back 1 downloads the files read back",
            "{reason:?}"
        );
        for said in [lost.sentence(), lost.sentence_started(), lost.card_row()] {
            assert!(
                !said.contains("are lost") && !said.contains("were lost"),
                "files that can be downloaded are not called lost: {said}"
            );
        }
    }

    // A file of the saved copy holds more than the storage did: the count and one path are named.
    let mut lost = kept_and_not_used(Some("older-file"));
    let r = lost.recovery.as_mut().unwrap();
    r.regressed = Some(2);
    r.regressed_path = Some("notes/log.txt".into());
    assert_eq!(
        lost.sentence(),
        format!(
            "{HEAD} Session 2 starts from the copy saved at 2026-11-16 14:02 UTC. {not_used}: 2 file(s) in the saved copy hold more \
             than the storage did, for example `notes/log.txt`. {DOWNLOAD}"
        )
    );

    // The storage held the same tree as the saved copy: nothing newer, and the copy read back can still be downloaded.
    let lost = kept_and_not_used(Some("nothing-newer"));
    assert_eq!(
        lost.sentence(),
        format!(
            "{HEAD} Session 2 starts from the copy saved at 2026-11-16 14:02 UTC. The service read session 1's storage back and found \
             nothing newer. {DOWNLOAD}"
        )
    );

    // No copy had been saved, so the next session starts from the files the run started with; the work of the session is not called
    // lost, because the files read back hold it.
    let lost = SessionLost {
        from: Some(FROM_START.to_string()),
        saved_at: None,
        ..kept_and_not_used(Some("storage-older"))
    };
    assert_eq!(
        lost.sentence(),
        format!(
            "{HEAD} Session 2 starts from the files the run started with: no copy had been saved. {not_used}: the storage did not \
             prove to be newer than the files the run started with. {DOWNLOAD}"
        )
    );

    // The run ends on the lost session and delivers the saved copy: the reason and the command follow the ending.
    let ended = SessionLost {
        lost_in_a_row: MAX_LOST_SLICES,
        then: THEN_END.to_string(),
        next_session: None,
        from: None,
        saved_at: None,
        delivers: Some(DELIVERS_COPY.to_string()),
        ..kept_and_not_used(Some("superseded"))
    };
    assert_eq!(
        ended.sentence(),
        format!(
            "{HEAD} It was lost session 3 in a row, so the run ends and the service delivers the last saved copy. {not_used}: a newer \
             copy was saved while it read. {DOWNLOAD}"
        )
    );
    assert!(
        ended.card_row().ends_with(
            "it was lost session 3 in a row, so the run ended; the storage was read back and not used; ham pull \
             01JABCDEFGHJKMNPQRSTVWXYZ0 --read-back 1 downloads the files read back"
        ),
        "{}",
        ended.card_row()
    );

    // Trigger: the same event without stored files keeps the words of a loss and names no command.
    let mut nothing_kept = kept_and_not_used(Some("worker-refused"));
    nothing_kept.recovery.as_mut().unwrap().kept = false;
    assert_eq!(
        nothing_kept.sentence(),
        format!(
            "{HEAD} Session 2 starts from the copy saved at 2026-11-16 14:02 UTC; file changes after that copy are lost. The service \
             could not read session 1's storage back: the run's worker is of a release that cannot do it."
        )
    );
    assert!(!nothing_kept.card_row().contains("ham pull"));
}

/// What the notice says when no files were read back at all: the reason, in the words of the read, and no command.
#[test]
fn b854_a_read_that_stored_nothing_is_told_with_its_reason_and_no_command() {
    let said = |reason: &str| {
        let mut lost = kept_and_not_used(Some(reason));
        lost.recovery.as_mut().unwrap().kept = false;
        lost.sentence()
    };
    let tail = "Session 2 starts from the copy saved at 2026-11-16 14:02 UTC; file changes after that copy are lost.";
    for (reason, why) in [
        ("not-mounted", "Session 1's storage held no work tree."),
        ("no-tree", "Session 1's storage held no work tree."),
        (
            "timeout",
            "The service could not read session 1's storage back within 120 seconds.",
        ),
        (
            "budget",
            "The service could not read session 1's storage back within 120 seconds.",
        ),
        (
            "session-active",
            "The service could not read session 1's storage back: the stopped session's machine was still answering.",
        ),
        (
            "upload-failed",
            "The service could not read session 1's storage back whole.",
        ),
        (
            "stop-failed",
            "The service could not read session 1's storage back.",
        ),
        (
            "mint-failed",
            "The service could not read session 1's storage back.",
        ),
        (
            "a-word-of-a-later-service",
            "The service could not read session 1's storage back.",
        ),
        (
            "off",
            "Reading a stopped session's storage back is switched off for this run (run.recover_after_stop).",
        ),
        (
            "instances",
            "This run's machine keeps no storage after a session ends, so there was nothing to read back.",
        ),
    ] {
        assert_eq!(said(reason), format!("{HEAD} {tail} {why}"), "{reason}");
    }
}

/// A session that had started from files read back from an earlier session and was lost before a copy of its own: the next session
/// starts from those files again, and the notice names the session whose storage they came from, not the lost one.
#[test]
fn b854_files_that_are_carried_forward_are_told_with_the_session_they_were_read_back_from() {
    let lost = SessionLost {
        session: 2,
        cause: CAUSE_NO_HEARTBEAT.to_string(),
        ended_at: None,
        lost_in_a_row: 2,
        next_session: Some(3),
        recovery: Some(LostRecovery {
            tried: true,
            used: false,
            reason: Some("stop-failed".into()),
            ..LostRecovery::default()
        }),
        read_back_session: Some(1),
        ..read_back()
    };
    assert_eq!(
        lost.sentence(),
        "Session 2 was lost: the service received no heartbeat from it for 600 seconds (the last one came at 2026-11-16 14:02 UTC). \
         Session 3 starts from the files the service read back from session 1's storage, which session 2 had started from; session 2 \
         saved no copy of its own. The service could not read session 2's storage back."
    );
    assert!(
        lost.sentence_started().contains(
            "Session 3 started from the files the service read back from session 1's storage, which session 2 had started from"
        ),
        "{}",
        lost.sentence_started()
    );
    assert!(
        lost.card_row()
            .ends_with("session 3 started from the files read back from session 1's storage"),
        "{}",
        lost.card_row()
    );
    assert!(
        !lost.sentence().contains("could not start"),
        "a start that is not known is not told as a start that failed: {}",
        lost.sentence()
    );

    // The run ends on that session: it delivers the files read back from session 1's storage, and the notice says so, whether the
    // run ends on its limit of lost sessions or for want of time.
    let ended = SessionLost {
        lost_in_a_row: MAX_LOST_SLICES,
        then: THEN_END.to_string(),
        next_session: None,
        from: None,
        delivers: Some(DELIVERS_COPY.to_string()),
        recovery: None,
        ..lost.clone()
    };
    assert!(
        ended.sentence().ends_with(
            "It was lost session 3 in a row, so the run ends and the service delivers the files it read back from session 1's storage."
        ),
        "{}",
        ended.sentence()
    );
    let no_time = SessionLost {
        lost_in_a_row: 1,
        ..ended
    };
    assert!(
        no_time.sentence().ends_with(
            "No new session starts, so the run ends and the service delivers the files it read back from session 1's storage."
        ),
        "{}",
        no_time.sentence()
    );
}

/// The command names the run of the event, which the client reads from the event's envelope; the payload carries no run id. A value
/// that was built without one is told with a placeholder.
#[test]
fn b854_the_command_names_the_run_of_the_event() {
    let payload = json!({
        "kind": "session-lost", "session": 4, "decidedAt": "2026-11-16T14:04:22Z", "cause": "no-heartbeat", "silenceS": 600,
        "lostInARow": 1, "limit": 3, "then": "restart", "nextSession": 5, "from": "copy",
        "recovery": {"tried": true, "used": false, "reason": "superseded", "kept": true}
    });
    let lost = SessionLost::parse(&event(payload.clone())).unwrap();
    assert_eq!(
        lost.read_back_command(),
        format!("ham pull {RUN} --read-back 4")
    );
    assert!(
        lost.sentence().ends_with(&format!(
            "Download the files read back with `ham pull {RUN} --read-back 4`."
        )),
        "{}",
        lost.sentence()
    );
    assert_eq!(
        serde_json::to_value(&lost).unwrap(),
        payload,
        "the run id is no member of the payload"
    );
    let mut bare = lost.clone();
    bare.run_id = String::new();
    assert_eq!(bare.read_back_command(), "ham pull <runId> --read-back 4");
}
