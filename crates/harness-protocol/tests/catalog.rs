//! Catalog rows.

use std::collections::BTreeMap;

use harness_protocol::catalog::{CatalogRow, ExcludedModel, Lifecycle, RouteFamily};
use harness_protocol::common::{Engine, Protocol};

fn row() -> CatalogRow {
    CatalogRow {
        profile_id: "us.anthropic.claude-sonnet-5".into(),
        base_model_id: "anthropic.claude-sonnet-5".into(),
        regions: vec!["us-west-2".into(), "us-east-1".into()],
        lifecycle: Lifecycle::Active,
        protocol: Protocol::Converse,
        region: "us-west-2".into(),
        engine: Engine::CLINE,
        route_family: None,
        tool_use: true,
        tool_streaming: true,
        parallel_tools: true,
        integer_args_ok: true,
        context_tokens: 200_000,
        max_output_tokens: 64_000,
        burndown_class: "5x".into(),
        cache_support: true,
        lease_class: "cached".into(),
        iam_resource_arns: vec![
            "arn:aws:bedrock:us-west-2:800000000022:inference-profile/us.anthropic.claude-sonnet-5"
                .into(),
        ],
        retention_allowed_modes: vec!["default".into()],
        measured_tdd_pass_rate: Some(0.9),
        gate_cheat_rate: Some(0.0),
        usd_per_agent_hour: Some(3.5),
        smoke_record_id: "smoke-20260925-001".into(),
        smoked_at: "2026-09-25T02:00:00Z".into(),
        role_max_tokens: BTreeMap::new(),
        tdd_qualification: None,
    }
}

#[test]
fn wire_names_and_enum_strings() {
    let v = serde_json::to_value(row()).unwrap();
    for key in [
        "profileId",
        "baseModelId",
        "toolUse",
        "integerArgsOk",
        "maxOutputTokens",
        "iamResourceArns",
        "smokeRecordId",
    ] {
        assert!(v.get(key).is_some(), "{key}");
    }
    assert_eq!(
        serde_json::to_value(Protocol::ConverseRelay).unwrap(),
        "converse-relay"
    );
    assert_eq!(
        serde_json::to_value(Protocol::MantleResponses).unwrap(),
        "mantle-responses"
    );
    assert_eq!(
        serde_json::to_value(RouteFamily::OpenaiV1).unwrap(),
        "/openai/v1"
    );
    assert_eq!(serde_json::to_value(RouteFamily::V1).unwrap(), "/v1");
    assert_eq!(serde_json::to_value(Lifecycle::Legacy).unwrap(), "LEGACY");
}

#[test]
fn exclusions_keep_the_exact_reason() {
    let e = ExcludedModel {
        profile_id: "minimax.minimax-m2.5".into(),
        region: "us-west-2".into(),
        protocol: Some(Protocol::Converse),
        reason: "ValidationException: The model returned the following errors: temperature".into(),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(
        v["reason"],
        "ValidationException: The model returned the following errors: temperature"
    );
}
