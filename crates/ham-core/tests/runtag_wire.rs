//! The wire types of `POST /v1/tags/edit`: one batch of at most 40 edits, a status for each, in request order.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::wire::{
    EditStatus, MAX_EDITS, MAX_TAGS_PER_EDIT, TagEdit, TagEditRequest, TagEditResponse,
    TagEditResult, WireError,
};
use serde_json::json;

fn edit(run: &str, add: &[&str], remove: &[&str]) -> TagEdit {
    TagEdit {
        run_id: run.to_string(),
        add: add.iter().map(|t| t.to_string()).collect(),
        remove: remove.iter().map(|t| t.to_string()).collect(),
    }
}

#[test]
fn edit_wire_roundtrips_and_limits_hold() {
    assert_eq!((MAX_EDITS, MAX_TAGS_PER_EDIT), (40, 12));
    let body = json!({"edits": [{"runId": "01J8ZQ0000000000000000AAAA", "add": ["project:x"], "remove": ["tech:s3"]}]});
    let req: TagEditRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(
        req.edits,
        vec![edit(
            "01J8ZQ0000000000000000AAAA",
            &["project:x"],
            &["tech:s3"]
        )]
    );
    assert_eq!(serde_json::to_value(&req).unwrap(), body);
    assert_eq!(req.validate(), Ok(()));

    // `add` and `remove` may be left out.
    let req: TagEditRequest = serde_json::from_value(
        json!({"edits": [{"runId": "R1", "add": ["topic:x"]}, {"runId": "R2"}]}),
    )
    .unwrap();
    assert_eq!(req.edits[0].remove, Vec::<String>::new());
    assert_eq!(req.edits[1].add, Vec::<String>::new());

    // The response: lower-case statuses, `not-found` with a hyphen, `message` only when there is one.
    let resp = TagEditResponse {
        results: vec![
            TagEditResult::ok("R1", vec!["project:x".to_string()]),
            TagEditResult::unchanged("R2", vec![]),
            TagEditResult::invalid("R3", "a tag needs a value after the kind"),
            TagEditResult::not_found("R4"),
            TagEditResult::conflict("R5"),
        ],
    };
    let json = serde_json::to_value(&resp).unwrap();
    let statuses: Vec<&str> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        ["ok", "unchanged", "invalid", "not-found", "conflict"]
    );
    assert_eq!(
        json["results"][0],
        json!({"runId": "R1", "status": "ok", "tags": ["project:x"]})
    );
    assert_eq!(
        json["results"][2]["message"],
        "a tag needs a value after the kind"
    );
    assert!(json["results"][3].get("message").is_none());
    assert_eq!(json["results"][3]["tags"], json!([]));
    let back: TagEditResponse = serde_json::from_value(json).unwrap();
    assert_eq!(back, resp);
    assert_eq!(back.results[3].status, EditStatus::NotFound);

    // 41 edits and 13 tags in one edit are refused, with the limit in the message.
    let many = TagEditRequest {
        edits: (0..41)
            .map(|i| edit(&format!("R{i}"), &["topic:x"], &[]))
            .collect(),
    };
    let err = many.validate().unwrap_err();
    assert_eq!(err, WireError::TooManyEdits { max: 40, got: 41 });
    assert!(err.to_string().contains("40"), "{err}");
    let ok40 = TagEditRequest {
        edits: (0..40)
            .map(|i| edit(&format!("R{i}"), &["topic:x"], &[]))
            .collect(),
    };
    assert_eq!(ok40.validate(), Ok(()));

    let thirteen: Vec<&str> = vec!["topic:t"; 13];
    let err = TagEditRequest {
        edits: vec![edit("R1", &thirteen, &[])],
    }
    .validate()
    .unwrap_err();
    assert_eq!(
        err,
        WireError::TooManyTags {
            run_id: "R1".to_string(),
            max: 12,
            got: 13
        }
    );
    assert!(err.to_string().contains("12"), "{err}");
    // The limit counts `add` and `remove` together.
    let err = TagEditRequest {
        edits: vec![edit("R1", &thirteen[..7], &thirteen[..6])],
    }
    .validate()
    .unwrap_err();
    assert!(matches!(err, WireError::TooManyTags { got: 13, .. }));
    assert_eq!(
        TagEditRequest {
            edits: vec![edit("R1", &thirteen[..12], &[])]
        }
        .validate(),
        Ok(())
    );
}

#[test]
fn run_ids_are_plain_and_not_repeated() {
    let bad = |id: &str| {
        TagEditRequest {
            edits: vec![edit(id, &["topic:x"], &[])],
        }
        .validate()
    };
    assert_eq!(
        bad(""),
        Err(WireError::BadRunId {
            run_id: String::new()
        })
    );
    assert!(matches!(bad("RUN#1"), Err(WireError::BadRunId { .. })));
    assert!(matches!(bad("a b"), Err(WireError::BadRunId { .. })));
    assert!(matches!(
        bad(&"R".repeat(65)),
        Err(WireError::BadRunId { .. })
    ));
    assert_eq!(bad(&"R".repeat(64)), Ok(()));
    assert_eq!(bad("01J8ZQ0000000000000000AAAA"), Ok(()));

    // Two edits for one run in one batch would race each other.
    let err = TagEditRequest {
        edits: vec![
            edit("R1", &["topic:x"], &[]),
            edit("R2", &[], &[]),
            edit("R1", &[], &["topic:y"]),
        ],
    }
    .validate()
    .unwrap_err();
    assert_eq!(
        err,
        WireError::DuplicateRunId {
            run_id: "R1".to_string()
        }
    );
}

#[test]
fn a_malformed_body_does_not_parse() {
    for bad in [
        json!({}),
        json!({"edits": "x"}),
        json!({"edits": [{"add": ["topic:x"]}]}),
        json!({"edits": [{"runId": 5}]}),
        json!({"edits": [{"runId": "R1", "add": "topic:x"}]}),
        json!({"edits": [{"runId": "R1", "add": [5]}]}),
        json!([1, 2]),
    ] {
        assert!(
            serde_json::from_value::<TagEditRequest>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    let unknown_status: Result<TagEditResponse, _> =
        serde_json::from_value(json!({"results": [{"runId": "R1", "status": "OK", "tags": []}]}));
    assert!(unknown_status.is_err());
}
