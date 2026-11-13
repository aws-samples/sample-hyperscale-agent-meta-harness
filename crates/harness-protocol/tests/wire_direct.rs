//! Protocol 1.29.0, the direct-run request shape:
//! `POST /v1/direct/runs` carries the user's prompt unchanged, the target, ONE model, the user's rules and the
//! optional nudge file. The old `StartRunRequest` keeps its bytes: nothing here edits it.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::V1_ROUTES;
use harness_protocol::api::v1::{
    ContinueSettings, ModelList, ModelRow, RuleFile, RunTarget, StartDirectRunRequest,
};
use harness_protocol::common::Protocol;
use harness_protocol::export::all_schemas;
use serde_json::{Value, json};

fn v1() -> Value {
    all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "openapi-v1.json")
        .unwrap()
        .1
}

#[test]
fn the_minimal_direct_request_is_a_prompt_a_target_and_a_model() {
    let r: StartDirectRunRequest = serde_json::from_value(json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": "us.anthropic.claude-sonnet-5-5"
    }))
    .unwrap();
    assert_eq!(r.prompt, "Add percent_change(a, b) with tests");
    assert_eq!(r.target, RunTarget::S3Only);
    assert_eq!(r.model, "us.anthropic.claude-sonnet-5-5");
    assert!(r.rules.is_empty() && r.nudge.is_none() && r.base_ref.is_none());
    // Optional members are left out of the wire form, so the idempotency hash of a small request is stable.
    let back = serde_json::to_value(&r).unwrap();
    assert_eq!(
        back,
        json!({
            "prompt": "Add percent_change(a, b) with tests",
            "target": {"mode": "s3-only"},
            "model": "us.anthropic.claude-sonnet-5-5",
            "rules": []
        })
    );
}

#[test]
fn the_direct_request_round_trips_with_rules_nudge_and_base_ref() {
    let rules = vec![RuleFile {
        path: "00-my-rules.md".into(),
        sha256: "a".repeat(64),
        content: "Write the test first.\n".into(),
    }];
    let nudge = ContinueSettings {
        text: "Re-read my request.\n".into(),
        sha256: "b".repeat(64),
        max_nudges: 3,
    };
    let r = StartDirectRunRequest {
        // Leading and trailing whitespace and newlines are the user's: the prompt travels unchanged.
        prompt: "  Fix the bug.\n\nThen add tests.  \n".into(),
        target: RunTarget::Repo {
            repo: "cc:hamh-e2e-python-calc".parse().unwrap(),
        },
        base_ref: Some("main".into()),
        model: "us.openai.gpt-6.1-sol".into(),
        rules,
        nudge: Some(nudge),
        input_wait_s: None,
        skills: vec![],
        continue_from: None,
        research: None,
        report_tests: None,
        report_step_s: None,
        goal: None,
        third_party: None,
        attachments: vec![],
        review: None,
        engine_config: None,
        compute: None,
        build: None,
        max_run_s: None,
        limits: None,
        engine: None,
        provider_sharing: None,
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["baseRef"], "main");
    assert_eq!(v["nudge"]["maxNudges"], 3);
    assert_eq!(v["rules"][0]["path"], "00-my-rules.md");
    let back: StartDirectRunRequest = serde_json::from_value(v).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.prompt, "  Fix the bug.\n\nThen add tests.  \n");
}

#[test]
fn members_of_the_old_multi_agent_shape_are_refused_not_ignored() {
    // A user (or a stale client) who sends the old planner, width or modes fields learns at once that this
    // route passes the prompt to Cline and nothing else.
    for member in [
        ("models", json!(["us.anthropic.claude-sonnet-5-5"])),
        ("width", json!({"m": 2})),
        ("modes", json!({"modes": []})),
        ("plan", json!({})),
        ("budget", json!({"kind": "tokens", "limit": 1})),
        ("approvals", json!("relay")),
        ("qualification", json!({})),
        ("hiddenInject", json!({"fixture": "x"})),
        ("cheatPlant", json!({"case": "x"})),
        ("extra", json!(1)),
    ] {
        let mut v = json!({
            "prompt": "p", "target": {"mode": "s3-only"}, "model": "m"
        });
        v[member.0] = member.1.clone();
        let e = serde_json::from_value::<StartDirectRunRequest>(v).unwrap_err();
        assert!(e.to_string().contains(member.0), "{}: {e}", member.0);
    }
}

#[test]
fn prompt_target_and_model_are_required() {
    for drop in ["prompt", "target", "model"] {
        let mut v = json!({"prompt": "p", "target": {"mode": "s3-only"}, "model": "m"});
        v.as_object_mut().unwrap().remove(drop);
        assert!(
            serde_json::from_value::<StartDirectRunRequest>(v).is_err(),
            "{drop} is required"
        );
    }
}

#[test]
fn a_continue_file_must_carry_all_three_members() {
    for drop in ["text", "sha256", "maxNudges"] {
        let mut v = json!({"text": "t", "sha256": "a", "maxNudges": 3});
        v.as_object_mut().unwrap().remove(drop);
        assert!(
            serde_json::from_value::<ContinueSettings>(v).is_err(),
            "{drop}"
        );
    }
    assert!(
        serde_json::from_value::<ContinueSettings>(
            json!({"text": "t", "sha256": "a", "maxNudges": -1})
        )
        .is_err(),
        "a negative count"
    );
}

#[test]
fn the_route_is_listed_and_documented() {
    assert!(
        V1_ROUTES
            .iter()
            .any(|r| (r.method, r.path, r.operation_id)
                == ("POST", "/v1/direct/runs", "startDirectRun")),
        "route table"
    );
    let doc = v1();
    let op = &doc["paths"]["/v1/direct/runs"]["post"];
    assert_eq!(op["operationId"], "startDirectRun");
    assert!(
        op["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Idempotency-Key" && p["required"] == true),
        "the same Idempotency-Key header as POST /v1/runs"
    );
    let body = op["requestBody"]["content"]["application/json"]["schema"].to_string();
    assert!(body.contains("StartDirectRunRequest"), "{body}");
    let ok = op["responses"]["200"]["content"]["application/json"]["schema"].to_string();
    assert!(
        ok.contains("StartRunResponse"),
        "the answer is the existing one: {ok}"
    );
    let def = &doc["components"]["schemas"]["StartDirectRunRequest"];
    let mut required: Vec<&str> = def["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["model", "prompt", "target"]);
    assert_eq!(def["additionalProperties"], false);
}

// ---- protocol 1.31.0: GET /v1/direct/models ----------------------------------------------------------------------------
// The merge review of the model-choice branch found the route served and used by `ham models --server` while the route
// table, openapi-v1.json and the snapshots did not know it, and its ModelList type lived in ham-core.

fn row(id: &str, alias: Option<&str>, note: Option<&str>) -> ModelRow {
    ModelRow {
        id: id.into(),
        name: "Name".into(),
        vendor: "v".into(),
        alias: alias.map(str::to_string),
        protocol: Protocol::Converse,
        region: "us-east-1".into(),
        context_tokens: 200_000,
        max_output_tokens: 8192,
        selectable: true,
        default: alias == Some("one"),
        note: note.map(str::to_string),
        engine: None,
        requires_provider_sharing: false,
    }
}

#[test]
fn the_models_route_is_listed_and_documented() {
    assert!(
        V1_ROUTES.iter().any(|r| (r.method, r.path, r.operation_id)
            == ("GET", "/v1/direct/models", "listDirectModels")),
        "route table"
    );
    let doc = v1();
    let op = &doc["paths"]["/v1/direct/models"]["get"];
    assert_eq!(op["operationId"], "listDirectModels");
    assert!(op.get("requestBody").is_none(), "a GET has no body: {op}");
    let ok = op["responses"]["200"]["content"]["application/json"]["schema"].to_string();
    assert!(ok.contains("ModelList"), "{ok}");
    let required = |name: &str| -> Vec<String> {
        let mut r: Vec<String> = doc["components"]["schemas"][name]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} is a schema with required members"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        r.sort_unstable();
        r
    };
    assert_eq!(required("ModelList"), ["default", "models"]);
    assert_eq!(
        required("ModelRow"),
        [
            "contextTokens",
            "default",
            "id",
            "maxOutputTokens",
            "name",
            "protocol",
            "region",
            "selectable",
            "vendor"
        ],
        "alias and note are optional members"
    );
    let props = &doc["components"]["schemas"]["ModelRow"]["properties"];
    assert!(props.get("alias").is_some() && props.get("note").is_some());
}

#[test]
fn a_model_row_leaves_out_an_absent_alias_and_note_and_the_list_round_trips() {
    let list = ModelList {
        default: "a.one".into(),
        models: vec![
            row("a.one", Some("one"), Some("Read this first.")),
            row("a.two", None, None),
        ],
    };
    let v = serde_json::to_value(&list).unwrap();
    assert_eq!(v["models"][0]["note"], "Read this first.");
    assert_eq!(v["models"][0]["contextTokens"], 200_000);
    assert!(v["models"][1].get("alias").is_none() && v["models"][1].get("note").is_none());
    let back: ModelList = serde_json::from_value(v).unwrap();
    assert_eq!(back, list);
    // A row written before the note existed still reads.
    let old: ModelRow = serde_json::from_value(json!({
        "id": "a.two", "name": "n", "vendor": "v", "protocol": "converse", "region": "us-east-1",
        "contextTokens": 1, "maxOutputTokens": 1, "selectable": false, "default": false
    }))
    .unwrap();
    assert_eq!(old.note, None);
}
