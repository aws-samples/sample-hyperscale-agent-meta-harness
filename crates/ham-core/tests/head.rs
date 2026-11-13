//! The wire type of `GET /v1/runs/{runId}/head`: the commit a finished run delivered, read by control-api from
//! the verified bundle's header, with the state of the run and, when the harness wrote it, Cline's last message. The type lives here, not in
//! `harness-protocol`: a field there would change the worker image key. `control-api` serialises it and `ham` decodes it, and the client
//! checks what it decoded because the answer carries text a worker wrote.
#![allow(clippy::unwrap_used)]

use ham_core::head::{
    ChangeBase, ChangeList, ChangeStatus, ChangedFile, Changes, ChangesRecordError, DeliveredHead,
    HeadError, MAX_CHANGE_FILES, MAX_CHANGES_RECORD_BYTES, changes_key, cline_message_in_evidence,
    path,
};
use harness_protocol::goal::GoalStatus;
use serde_json::json;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";

fn good() -> DeliveredHead {
    DeliveredHead {
        run_id: RUN.into(),
        head_sha: HEAD.into(),
        run_status: GoalStatus::Complete,
        cline_message: None,
        changes: Changes::Unavailable,
    }
}

fn file(p: &str, s: ChangeStatus) -> ChangedFile {
    ChangedFile {
        path: p.into(),
        status: s,
    }
}

fn list(files: Vec<ChangedFile>, omitted: u64) -> ChangeList {
    ChangeList {
        base: ChangeBase::EmptyTree,
        total: files.len() as u64 + omitted,
        omitted,
        files,
    }
}

#[test]
fn the_route_is_the_head_of_a_run() {
    assert_eq!(path(RUN), format!("/v1/runs/{RUN}/head"));
}

#[test]
fn the_wire_names_are_camel_case_and_a_missing_message_is_left_out() {
    let v = serde_json::to_value(good()).unwrap();
    assert_eq!(
        v,
        json!({"runId": RUN, "headSha": HEAD, "runStatus": "complete", "changes": {"state": "unavailable"}})
    );
    let with = DeliveredHead {
        cline_message: Some("done".into()),
        ..good()
    };
    assert_eq!(serde_json::to_value(with).unwrap()["clineMessage"], "done");
}

#[test]
fn an_answer_round_trips_and_a_newer_server_may_add_members() {
    let h = DeliveredHead {
        cline_message: Some("hello.py printed 55.".into()),
        ..good()
    };
    let back: DeliveredHead = serde_json::from_value(serde_json::to_value(&h).unwrap()).unwrap();
    assert_eq!(back, h);
    let newer: DeliveredHead = serde_json::from_value(
        json!({"runId": RUN, "headSha": HEAD, "runStatus": "cleared", "somethingNew": [1, 2]}),
    )
    .unwrap();
    assert_eq!(newer.run_status, GoalStatus::Cleared);
}

#[test]
fn the_check_accepts_the_run_asked_about_and_a_full_object_id() {
    assert_eq!(good().check(RUN), Ok(()));
    // A SHA-256 repository names its objects with 64 digits.
    let long = DeliveredHead {
        head_sha: "ab".repeat(32),
        ..good()
    };
    assert_eq!(long.check(RUN), Ok(()));
}

#[test]
fn the_check_refuses_another_run_and_a_head_that_is_not_a_full_object_id() {
    assert_eq!(
        good().check("01K5ZQ4T8M3N6P7R9S0V1W2X3Z"),
        Err(HeadError::Run)
    );
    for bad in [
        "",
        "9d2f1a7",
        &HEAD.to_uppercase(),
        &format!("{HEAD}0"),
        "g".repeat(40).as_str(),
        "../../etc",
    ] {
        let h = DeliveredHead {
            head_sha: bad.into(),
            ..good()
        };
        assert_eq!(h.check(RUN), Err(HeadError::Head), "{bad:?}");
    }
}

#[test]
fn the_check_refuses_a_message_longer_than_the_evidence_can_hold() {
    let limit = harness_protocol::direct::MAX_CLINE_MESSAGE_CHARS;
    let ok = DeliveredHead {
        cline_message: Some("x".repeat(limit)),
        ..good()
    };
    assert_eq!(ok.check(RUN), Ok(()));
    let over = DeliveredHead {
        cline_message: Some("x".repeat(limit + 1)),
        ..good()
    };
    assert_eq!(over.check(RUN), Err(HeadError::Message));
}

#[test]
fn the_message_the_client_keeps_is_text_without_controls_or_tokens() {
    let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
    let h = DeliveredHead {
        cline_message: Some(format!("a\u{1b}[31mb {secret} \u{202e}c")),
        ..good()
    };
    let m = h.message().unwrap();
    assert!(
        !m.contains('\u{1b}') && !m.contains(&secret) && !m.contains('\u{202e}'),
        "{m:?}"
    );
    assert!(m.starts_with('a'));
    assert_eq!(good().message(), None);
    let blank = DeliveredHead {
        cline_message: Some("  \n ".into()),
        ..good()
    };
    assert_eq!(blank.message(), None, "a blank message is no message");
}

#[test]
fn the_key_of_the_validators_record_sits_beside_the_verified_bundle() {
    assert_eq!(
        changes_key(RUN),
        format!("runs/{RUN}/deliverable/changes.json")
    );
}

#[test]
fn a_change_list_is_a_state_with_its_base_its_total_and_what_it_left_out() {
    let h = DeliveredHead {
        changes: Changes::Recorded(ChangeList {
            base: ChangeBase::Commit {
                sha: "1".repeat(40),
            },
            total: 3,
            omitted: 1,
            files: vec![
                file("hello.py", ChangeStatus::Added),
                file("old.txt", ChangeStatus::Deleted),
            ],
        }),
        ..good()
    };
    let v = serde_json::to_value(&h).unwrap();
    assert_eq!(
        v["changes"],
        json!({"state": "recorded", "base": {"kind": "commit", "sha": "1".repeat(40)}, "total": 3, "omitted": 1,
               "files": [{"path": "hello.py", "status": "A"}, {"path": "old.txt", "status": "D"}]})
    );
    let back: DeliveredHead = serde_json::from_value(v).unwrap();
    assert_eq!(back, h);
    assert_eq!(h.check(RUN), Ok(()));
    let empty = serde_json::to_value(list(vec![], 0)).unwrap();
    assert_eq!(
        empty,
        json!({"base": {"kind": "empty-tree"}, "total": 0, "omitted": 0, "files": []}),
        "an empty list is a delivery that changed nothing, not a missing record"
    );
}

#[test]
fn a_status_letter_this_client_has_no_word_for_is_kept_as_other() {
    let f: ChangedFile = serde_json::from_value(json!({"path": "a", "status": "U"})).unwrap();
    assert_eq!(f.status, ChangeStatus::Other);
    assert_eq!(ChangeStatus::Added.word(), "added");
    assert_eq!(ChangeStatus::Other.word(), "changed");
}

#[test]
fn the_check_refuses_a_change_list_that_does_not_add_up_or_cannot_be_trusted() {
    let with = |l: ChangeList| DeliveredHead {
        changes: Changes::Recorded(l),
        ..good()
    };
    // The numbers must add up: listed plus left out is the total.
    let mut l = list(vec![file("a", ChangeStatus::Added)], 2);
    l.total = 5;
    assert_eq!(with(l).check(RUN), Err(HeadError::Changes));
    // More than the cap.
    let many: Vec<ChangedFile> = (0..=MAX_CHANGE_FILES)
        .map(|i| file(&format!("f{i}"), ChangeStatus::Added))
        .collect();
    assert_eq!(with(list(many, 0)).check(RUN), Err(HeadError::Changes));
    // A path that is empty, holds a NUL, or is longer than a path may be.
    for p in ["", "a\0b", &"x".repeat(1025)] {
        assert_eq!(
            with(list(vec![file(p, ChangeStatus::Added)], 0)).check(RUN),
            Err(HeadError::Changes),
            "{p:?}"
        );
    }
    // A base that is not a commit id.
    let mut l = list(vec![], 0);
    l.base = ChangeBase::Commit { sha: "abc".into() };
    assert_eq!(with(l).check(RUN), Err(HeadError::Changes));
    // The cap itself is fine.
    let at_cap: Vec<ChangedFile> = (0..MAX_CHANGE_FILES)
        .map(|i| file(&format!("f{i}"), ChangeStatus::Modified))
        .collect();
    assert_eq!(with(list(at_cap, 7)).check(RUN), Ok(()));
}

fn fixture() -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/changes/changes-v1.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn the_validators_record_parses_into_the_list_the_route_passes_on() {
    let l = ChangeList::from_record(&fixture(), RUN, HEAD).unwrap();
    assert_eq!(l.base, ChangeBase::EmptyTree);
    assert_eq!((l.total, l.omitted), (3, 1));
    assert_eq!(
        l.files,
        vec![
            file("README.md", ChangeStatus::Added),
            file("hello.py", ChangeStatus::Added)
        ]
    );
}

#[test]
fn a_record_of_another_run_or_another_head_or_version_is_not_used() {
    assert_eq!(
        ChangeList::from_record(&fixture(), "01K5ZQ4T8M3N6P7R9S0V1W2X3Z", HEAD),
        Err(ChangesRecordError::Run)
    );
    assert_eq!(
        ChangeList::from_record(&fixture(), RUN, &"2".repeat(40)),
        Err(ChangesRecordError::Head)
    );
    let v2 = String::from_utf8(fixture())
        .unwrap()
        .replace("\"schemaVersion\":1", "\"schemaVersion\":2");
    assert_eq!(
        ChangeList::from_record(v2.as_bytes(), RUN, HEAD),
        Err(ChangesRecordError::Schema(2))
    );
}

#[test]
fn a_record_that_is_not_json_too_long_or_does_not_add_up_is_not_used() {
    assert_eq!(
        ChangeList::from_record(b"{ not json", RUN, HEAD),
        Err(ChangesRecordError::Shape)
    );
    assert_eq!(
        ChangeList::from_record(b"[1,2]", RUN, HEAD),
        Err(ChangesRecordError::Shape)
    );
    let big = vec![b' '; MAX_CHANGES_RECORD_BYTES + 1];
    assert_eq!(
        ChangeList::from_record(&big, RUN, HEAD),
        Err(ChangesRecordError::TooLarge(MAX_CHANGES_RECORD_BYTES + 1))
    );
    // Listed plus left out must be the total.
    let off = String::from_utf8(fixture())
        .unwrap()
        .replace("\"total\":3", "\"total\":9");
    assert_eq!(
        ChangeList::from_record(off.as_bytes(), RUN, HEAD),
        Err(ChangesRecordError::Shape)
    );
}

#[test]
fn an_error_never_holds_a_byte_of_the_record() {
    let secret = "PRIVATE-PATH-NAME";
    let off = String::from_utf8(fixture())
        .unwrap()
        .replace("hello.py", secret)
        .replace("\"total\":3", "\"total\":9");
    let e = ChangeList::from_record(off.as_bytes(), RUN, HEAD).unwrap_err();
    assert!(!format!("{e} {e:?}").contains(secret));
}

/// `evidence/direct.json` is run-session's `EvidenceFile`: the engine's message is `summary.clineMessage`. A message at the top of the file is
/// not the writer's, a non-string is no message, and no summary is no message.
#[test]
fn the_engines_message_is_read_from_the_summary_of_the_evidence_file() {
    let written = json!({"schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "summary": {"clineMessage": "hello.py printed 55."}});
    assert_eq!(
        cline_message_in_evidence(&written),
        Some("hello.py printed 55.")
    );
    let top =
        json!({"schemaVersion": 1, "runId": RUN, "clineMessage": "not where the writer puts it"});
    assert_eq!(cline_message_in_evidence(&top), None);
    let number = json!({"summary": {"clineMessage": 7}});
    assert_eq!(cline_message_in_evidence(&number), None);
    let null = json!({"summary": {"clineMessage": null}});
    assert_eq!(cline_message_in_evidence(&null), None);
    let none = json!({"summary": null});
    assert_eq!(cline_message_in_evidence(&none), None);
    assert_eq!(cline_message_in_evidence(&json!({})), None);
}
