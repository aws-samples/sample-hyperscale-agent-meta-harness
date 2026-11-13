//! `/v1` and `/internal` API types.

use harness_protocol::api::internal::{
    ApprovalDecision, BatchError, CredsResponse, EventsBatchRequest, EventsBatchResponse,
    PendingControl, ProcessCredentials, TurnCompleteRequest, validate_events_batch,
};
use harness_protocol::api::{INTERNAL_ROUTES, V1_ROUTES};
use harness_protocol::common::Usage;
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::runtime::TurnOutcome;
use serde_json::json;

#[test]
fn route_tables_list_every_documented_route() {
    let v1: Vec<(&str, &str)> = V1_ROUTES.iter().map(|r| (r.method, r.path)).collect();
    for want in [
        ("POST", "/v1/direct/runs"),
        ("GET", "/v1/direct/models"),
        ("GET", "/v1/runs"),
        ("GET", "/v1/runs/{runId}"),
        ("POST", "/v1/runs/{runId}/cancel"),
        ("POST", "/v1/runs/{runId}/input"),
        ("POST", "/v1/runs/{runId}/messages"),
        ("GET", "/v1/runs/{runId}/messages"),
        ("GET", "/v1/runs/{runId}/messages/{messageId}"),
        ("POST", "/v1/runs/{runId}/messages/{messageId}/withdraw"),
        ("GET", "/v1/runs/{runId}/events"),
        ("GET", "/v1/runs/{runId}/artifacts"),
        (
            "POST",
            "/v1/runs/{runId}/artifacts/{artifactId}/download-url",
        ),
        ("GET", "/v1/models"),
        ("GET", "/v1/whoami"),
        ("GET", "/v1/repos"),
        ("POST", "/v1/admin/repos"),
    ] {
        assert!(v1.contains(&want), "missing {want:?}");
    }
    assert_eq!(v1.len(), 17);
    let internal: Vec<&str> = INTERNAL_ROUTES.iter().map(|r| r.path).collect();
    assert_eq!(
        internal,
        [
            "/internal/creds",
            "/internal/heartbeat",
            "/internal/turn-complete",
            "/internal/events",
            "/internal/message-session",
            "/internal/message-claim",
            "/internal/message-seal",
            "/internal/message-outcome",
            "/internal/research/search",
            "/internal/research/fetch",
            "/internal/research/repo",
            "/internal/research/job",
            "/internal/research/limits"
        ]
    );
    assert!(INTERNAL_ROUTES.iter().all(|r| r.method == "POST"));
    let mut ids: Vec<&str> = V1_ROUTES
        .iter()
        .chain(INTERNAL_ROUTES)
        .map(|r| r.operation_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 30, "operation ids are unique");
}

#[test]
fn process_credentials_use_the_credential_process_format_and_redact_debug() {
    let resp = CredsResponse {
        credentials: ProcessCredentials {
            version: 1,
            access_key_id: "ASIAEXAMPLE".into(),
            secret_access_key: "SECRETVALUE".into(),
            session_token: "TOKENVALUE".into(),
            expiration: "2026-09-25T03:00:00Z".into(),
        },
        next_nonce: "n2".into(),
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["credentials"]["Version"], 1);
    assert_eq!(v["credentials"]["AccessKeyId"], "ASIAEXAMPLE");
    assert_eq!(v["credentials"]["SessionToken"], "TOKENVALUE");
    assert_eq!(v["nextNonce"], "n2");
    let dbg = format!("{resp:?}");
    assert!(
        !dbg.contains("SECRETVALUE") && !dbg.contains("TOKENVALUE"),
        "{dbg}"
    );
    assert!(dbg.contains("ASIAEXAMPLE"));
}

#[test]
fn turn_complete_carries_no_verdict() {
    let req = TurnCompleteRequest {
        summary: None,
        run_id: "r".into(),
        agent_id: "a".into(),
        turn: 3,
        finish_reason: "end_turn".into(),
        last_commit_sha: Some("abc1234".into()),
        bundle_uri: None,
        snapshot_uri: None,
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        }),
        outcome: None,
        error: None,
        rules_digest: None,
        rules_manifest: None,
        reverted: vec![],
        reverted_truncated: false,
        refused: vec![],
        refused_truncated: false,
        fault_hint: None,
        max_silent_gap_s: None,
        transcript: None,
        handoff: None,
        output_pins: None,
    };
    let v = serde_json::to_value(&req).unwrap();
    assert!(v.get("verdict").is_none() && v.get("status").is_none());
    assert_eq!(v["usage"]["cacheReadInputTokens"], 0);
    assert!(v.get("outcome").is_none() && v.get("error").is_none());
    // Nothing restored outside the role: the field is left out, and an older agentd's request
    // (without it) reads as none.
    assert!(v.get("reverted").is_none(), "{v}");
    let back: TurnCompleteRequest = serde_json::from_value(v).unwrap();
    assert!(back.reverted.is_empty());
    let scoped = TurnCompleteRequest {
        reverted: vec!["calc/core.py".into()],
        ..req
    };
    let v = serde_json::to_value(&scoped).unwrap();
    assert_eq!(v["reverted"], json!(["calc/core.py"]));
    assert!(v.get("revertedTruncated").is_none(), "{v}");
}

#[test]
fn restored_paths_are_capped_by_count_and_length_with_a_truncated_flag() {
    use harness_protocol::api::internal::{
        MAX_REVERTED_PATH_CHARS, MAX_REVERTED_PATHS, cap_reverted,
    };
    assert_eq!(MAX_REVERTED_PATHS, 20);
    assert_eq!(MAX_REVERTED_PATH_CHARS, 256);
    let few = vec!["a.py".to_string(), "b/c.py".to_string()];
    assert_eq!(cap_reverted(&few), (few.clone(), false));
    let many: Vec<String> = (0..21).map(|n| format!("f{n}.py")).collect();
    let (kept, cut) = cap_reverted(&many);
    assert_eq!(kept, many[..20].to_vec());
    assert!(cut);
    // A long path keeps its first characters, on a character boundary.
    let long = vec![format!("{}.py", "é".repeat(300))];
    let (kept, cut) = cap_reverted(&long);
    assert_eq!(kept[0].chars().count(), MAX_REVERTED_PATH_CHARS);
    assert!(kept[0].starts_with("éé"));
    assert!(cut);
    let body = json!({"runId": "r", "agentId": "a", "turn": 1, "finishReason": "end_turn",
        "reverted": ["x.py"], "revertedTruncated": true});
    let req: TurnCompleteRequest = serde_json::from_value(body).unwrap();
    assert!(req.reverted_truncated);
}

#[test]
fn refused_requests_ride_on_turn_complete_capped_and_omitted_when_none() {
    use harness_protocol::api::internal::{
        MAX_REFUSAL_ACTION_CHARS, MAX_REFUSAL_REASON_CHARS, MAX_REFUSALS, Refusal, cap_refused,
    };
    assert_eq!(MAX_REFUSALS, 10);
    assert_eq!(MAX_REFUSAL_ACTION_CHARS, 240);
    assert_eq!(MAX_REFUSAL_REASON_CHARS, 320);
    let r = |n: u32| Refusal {
        tool: "run_commands".into(),
        action: format!("pip install x{n}"),
        reason: "`pip install` can modify files; commands of role implementer may not".into(),
        count: 1,
    };
    let few = vec![r(1), r(2)];
    assert_eq!(cap_refused(&few), (few.clone(), false));
    let many: Vec<Refusal> = (0..11).map(r).collect();
    let (kept, cut) = cap_refused(&many);
    assert_eq!(kept, many[..10].to_vec());
    assert!(cut);
    // Long fields keep their first characters, on a character boundary.
    let long = vec![Refusal {
        action: "é".repeat(300),
        reason: "x".repeat(400),
        ..r(0)
    }];
    let (kept, cut) = cap_refused(&long);
    assert_eq!(kept[0].action.chars().count(), MAX_REFUSAL_ACTION_CHARS);
    assert_eq!(kept[0].reason.chars().count(), MAX_REFUSAL_REASON_CHARS);
    assert!(kept[0].action.starts_with("éé"));
    assert!(cut);
    let body = json!({"runId": "r", "agentId": "a", "turn": 1, "finishReason": "end_turn",
        "refused": [{"tool": "run_commands", "action": "pip install -e .",
            "reason": "`pip install` can modify files; commands of role implementer may not",
            "count": 23}],
        "refusedTruncated": true});
    let req: TurnCompleteRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(req.refused[0].count, 23);
    assert!(req.refused_truncated);
    assert_eq!(serde_json::to_value(&req).unwrap(), body);
    // An older agentd's request has neither field; none is written when nothing was refused.
    let none: TurnCompleteRequest = serde_json::from_value(
        json!({"runId": "r", "agentId": "a", "turn": 1, "finishReason": "end_turn"}),
    )
    .unwrap();
    assert!(none.refused.is_empty() && !none.refused_truncated);
    let v = serde_json::to_value(&none).unwrap();
    assert!(
        v.get("refused").is_none() && v.get("refusedTruncated").is_none(),
        "{v}"
    );
}

#[test]
fn turn_complete_carries_the_observed_outcome_as_telemetry() {
    let body = serde_json::json!({"runId": "r", "agentId": "a", "turn": 1, "finishReason": "timed_out",
        "usage": {"inputTokens": 0, "outputTokens": 0, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0},
        "outcome": "timed_out", "error": "turn exceeded 60 s"});
    let req: TurnCompleteRequest = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(req.outcome, Some(TurnOutcome::TimedOut));
    assert_eq!(req.error.as_deref(), Some("turn exceeded 60 s"));
    assert_eq!(serde_json::to_value(&req).unwrap(), body);
}

fn agent_event(agent: &str, t: EventType, authority: Authority) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01J8ZQ4Y6X3M2K7N9P5R8T1V0W".into(),
        run_id: "r".into(),
        task_id: None,
        agent_id: Some(agent.into()),
        seq: 1,
        ts: "2026-09-25T01:00:00Z".into(),
        event_type: t,
        authority,
        payload: Payload::Inline { value: json!({}) },
    }
}

#[test]
fn event_batches_hold_only_the_callers_agent_events() {
    let ok = EventsBatchRequest {
        run_id: "r".into(),
        agent_id: "a".into(),
        events: vec![agent_event("a", EventType::TurnProgress, Authority::Agent)],
        take_controls: None,
    };
    validate_events_batch(&ok).unwrap();
    let forged = EventsBatchRequest {
        run_id: "r".into(),
        agent_id: "a".into(),
        events: vec![agent_event("a", EventType::GoalStatus, Authority::Harness)],
        take_controls: None,
    };
    assert!(matches!(
        validate_events_batch(&forged).unwrap_err(),
        BatchError::Authority { index: 0 }
    ));
    let other = EventsBatchRequest {
        run_id: "r".into(),
        agent_id: "a".into(),
        events: vec![agent_event("b", EventType::TurnProgress, Authority::Agent)],
        take_controls: None,
    };
    assert!(matches!(
        validate_events_batch(&other).unwrap_err(),
        BatchError::AgentMismatch { index: 0 }
    ));
    let mut wrong_run = ok.clone();
    wrong_run.events[0].run_id = "other".into();
    assert!(matches!(
        validate_events_batch(&wrong_run).unwrap_err(),
        BatchError::RunMismatch { index: 0 }
    ));
    let mut invalid = ok.clone();
    invalid.events[0].event_id = "nope".into();
    assert!(matches!(
        validate_events_batch(&invalid).unwrap_err(),
        BatchError::Envelope { index: 0, .. }
    ));
    let empty = EventsBatchRequest {
        run_id: "r".into(),
        agent_id: "a".into(),
        events: vec![],
        take_controls: None,
    };
    validate_events_batch(&empty).unwrap();
}

#[test]
fn a_batch_may_leave_controls_queued() {
    // The final flush of a turn sets takeControls false; absent means the response carries them.
    let b: EventsBatchRequest =
        serde_json::from_value(json!({"runId": "r", "agentId": "a", "events": []})).unwrap();
    assert_eq!(b.take_controls, None);
    assert!(!serde_json::to_string(&b).unwrap().contains("takeControls"));
    let last = EventsBatchRequest {
        take_controls: Some(false),
        ..b
    };
    assert_eq!(
        serde_json::to_value(&last).unwrap()["takeControls"],
        json!(false)
    );
}

#[test]
fn pending_controls_are_tagged() {
    let resp = EventsBatchResponse {
        accepted: 2,
        pending: vec![
            PendingControl::Steer {
                text: "t".into(),
                now: false,
            },
            PendingControl::Cancel,
            PendingControl::ApprovalAnswer {
                approval_id: "p1".into(),
                decision: ApprovalDecision::Deny,
            },
        ],
    };
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(
        v["pending"][0],
        json!({"kind": "steer", "text": "t", "now": false})
    );
    assert_eq!(v["pending"][1], json!({"kind": "cancel"}));
    assert_eq!(
        v["pending"][2],
        json!({"kind": "approvalAnswer", "approvalId": "p1", "decision": "deny"})
    );
}

#[test]
fn the_repository_allowlist_admin_and_listing_wire_shapes() {
    // As corrected by C14: ham-admin repos allow|deny through /v1/admin/repos
    // (operator only); ham doctor lists the entries through GET /v1/repos.
    use harness_protocol::api::v1::{
        ListReposResponse, RepoAdminAction, RepoAdminRequest, RepoAdminResponse, RepoAllowView,
    };
    let allow: RepoAdminRequest =
        serde_json::from_value(json!({"action": "allow", "target": "gh:amzn/app"})).unwrap();
    assert_eq!(allow.action, RepoAdminAction::Allow);
    assert_eq!(allow.target.to_string(), "gh:amzn/app");
    let deny: RepoAdminRequest =
        serde_json::from_value(json!({"action": "deny", "target": "cc:hamh-e2e-x"})).unwrap();
    assert_eq!(deny.action, RepoAdminAction::Deny);
    for bad in [
        json!({"action": "merge", "target": "cc:hamh-e2e-x"}),
        json!({"action": "allow", "target": "svn:x"}),
        json!({"action": "allow", "target": "cc:hamh-e2e-x", "owner": "me"}),
    ] {
        assert!(
            serde_json::from_value::<RepoAdminRequest>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    let view = RepoAllowView {
        target: "cc:hamh-e2e-x".parse().unwrap(),
        added_by: "arn:aws:sts::800000000022:assumed-role/Admin/testuser".into(),
        added_at: "2026-09-27T03:00:00Z".into(),
    };
    assert_eq!(
        serde_json::to_value(ListReposResponse {
            repos: vec![view.clone()]
        })
        .unwrap(),
        json!({"repos": [{"target": "cc:hamh-e2e-x",
                          "addedBy": "arn:aws:sts::800000000022:assumed-role/Admin/testuser",
                          "addedAt": "2026-09-27T03:00:00Z"}]})
    );
    assert_eq!(
        serde_json::to_value(RepoAdminResponse {
            action: RepoAdminAction::Deny,
            target: view.target.clone(),
            entry: None
        })
        .unwrap(),
        json!({"action": "deny", "target": "cc:hamh-e2e-x"})
    );
    let routes: Vec<(&str, &str, &str)> = V1_ROUTES
        .iter()
        .map(|r| (r.method, r.path, r.operation_id))
        .collect();
    assert!(
        routes.contains(&("GET", "/v1/repos", "listRepos")),
        "{routes:?}"
    );
    assert!(
        routes.contains(&("POST", "/v1/admin/repos", "adminRepos")),
        "{routes:?}"
    );
}

/// The member `audrecRecordId` is no part of the contract any more. An older `ham-admin` still sends it with a request: it is accepted and ignored. An
/// older backend still stores and lists it: it is ignored on the way in, and nothing writes it.
#[test]
fn the_retired_review_record_member_is_accepted_and_dropped() {
    use harness_protocol::api::v1::{RepoAdminRequest, RepoAllowView};
    use harness_protocol::store::RepoAllowItem;
    for member in [
        json!("AUDREC-123"),
        json!("x".repeat(65)),
        json!(null),
        json!(7),
    ] {
        let r: RepoAdminRequest = serde_json::from_value(
            json!({"action": "allow", "target": "gh:amzn/app", "audrecRecordId": member}),
        )
        .unwrap_or_else(|e| panic!("{member}: {e}"));
        let back = serde_json::to_value(&r).unwrap();
        assert_eq!(
            back,
            json!({"action": "allow", "target": "gh:amzn/app"}),
            "the member is not kept"
        );
    }
    let view: RepoAllowView = serde_json::from_value(json!({
        "target": "cc:hamh-e2e-x", "audrecRecordId": "AUDREC-1",
        "addedBy": "arn:aws:sts::123456789012:assumed-role/Admin/me", "addedAt": "2026-09-27T03:00:00Z"
    }))
    .unwrap();
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .to_lowercase()
            .contains("audrec")
    );
    let item: RepoAllowItem = serde_json::from_value(json!({
        "target": "cc:hamh-e2e-x", "audrecRecordId": "AUDREC-1",
        "addedBy": "arn:aws:sts::123456789012:assumed-role/Admin/me", "addedAt": "2026-09-27T03:00:00Z"
    }))
    .unwrap();
    assert!(
        !serde_json::to_string(&item)
            .unwrap()
            .to_lowercase()
            .contains("audrec")
    );
}

/// The generated contract files carry no review-record member and no Amazon-owned organization rule.
#[test]
fn the_generated_contract_names_no_review_record() {
    for (name, value) in harness_protocol::export::all_schemas() {
        let text = harness_protocol::export::render(&value).to_lowercase();
        assert!(!text.contains("audrec"), "{name}");
        assert!(!text.contains("amazon-owned"), "{name}");
    }
}
