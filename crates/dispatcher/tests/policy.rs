//! The inline STS session policy: Bedrock narrowed to the
//! leased model, the role's other grants passed through, within the 2,048-character limit.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use dispatcher::policy::{
    MAX_SESSION_POLICY_CHARS, PolicyError, bedrock_resources, session_policy,
};
use dispatcher::stage::{ACCOUNT, REGION};
use proptest::prelude::*;
use serde_json::Value;

const SONNET: &str = "anthropic.claude-sonnet-4-5-20250929-v1:0";

/// A pinned inference Region other than the home Region.
fn other_region() -> &'static str {
    if REGION == "us-east-1" {
        "us-west-2"
    } else {
        "us-east-1"
    }
}

/// A 12-digit account number other than the stage account, whichever `accounts.json` entry the build selected (`ACCOUNT` is the account the build embeds).
fn other_account() -> &'static str {
    if ACCOUNT == "111122223333" {
        "123456789012"
    } else {
        "111122223333"
    }
}

fn statements(policy: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(policy).unwrap();
    assert_eq!(v["Version"], "2012-10-17");
    v["Statement"].as_array().unwrap().clone()
}

fn by_sid<'a>(st: &'a [Value], sid: &str) -> Option<&'a Value> {
    st.iter().find(|s| s["Sid"] == sid)
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
        other => panic!("not a string or list: {other}"),
    }
}

#[test]
fn a_geo_profile_gets_its_profile_arn_and_the_foundation_model_in_each_inference_region() {
    let r = bedrock_resources(&format!("us.{SONNET}")).unwrap();
    assert_eq!(
        r,
        vec![
            format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/us.{SONNET}"),
            format!("arn:aws:bedrock:us-west-2::foundation-model/{SONNET}"),
            format!("arn:aws:bedrock:us-east-1::foundation-model/{SONNET}"),
            format!("arn:aws:bedrock:us-east-2::foundation-model/{SONNET}"),
        ]
    );
}

#[test]
fn a_global_profile_also_gets_the_regionless_foundation_model_arn() {
    let r = bedrock_resources(&format!("global.{SONNET}")).unwrap();
    assert_eq!(r.len(), 5);
    assert_eq!(
        r[0],
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/global.{SONNET}")
    );
    assert_eq!(r[4], format!("arn:aws:bedrock:::foundation-model/{SONNET}"));
}

#[test]
fn a_plain_model_id_gets_foundation_model_arns_only() {
    assert_eq!(
        bedrock_resources("minimax.minimax-m2.5").unwrap(),
        vec![
            "arn:aws:bedrock:us-west-2::foundation-model/minimax.minimax-m2.5",
            "arn:aws:bedrock:us-east-1::foundation-model/minimax.minimax-m2.5",
            "arn:aws:bedrock:us-east-2::foundation-model/minimax.minimax-m2.5",
        ]
    );
}

#[test]
fn a_mantle_model_gets_its_base_foundation_models_and_the_mantle_statement() {
    let st = statements(&session_policy("aws-mantle/qwen.qwen3-coder-next").unwrap());
    assert_eq!(st.len(), 3);
    let mantle = by_sid(&st, "LeasedMantle").unwrap();
    assert_eq!(mantle["Effect"], "Allow");
    assert_eq!(
        strings(&mantle["Action"]),
        ["bedrock-mantle:CreateInference"]
    );
    assert_eq!(mantle["Resource"], "*");
    let model = by_sid(&st, "LeasedModel").unwrap();
    assert_eq!(
        strings(&model["Resource"]),
        bedrock_resources("qwen.qwen3-coder-next").unwrap()
    );
}

#[test]
fn a_converse_model_gets_no_mantle_statement() {
    let st = statements(&session_policy(&format!("us.{SONNET}")).unwrap());
    assert!(by_sid(&st, "LeasedMantle").is_none());
    assert!(
        st.iter()
            .flat_map(|s| strings(&s["Action"]))
            .all(|a| !a.starts_with("bedrock-mantle:"))
    );
}

#[test]
fn the_policy_passes_the_roles_other_grants_through_and_names_only_the_leased_model() {
    let model_id = format!("us.{SONNET}");
    let st = statements(&session_policy(&model_id).unwrap());
    assert_eq!(st.len(), 2);
    let rest = by_sid(&st, "RoleGrants").unwrap();
    assert_eq!(rest["Effect"], "Allow");
    assert_eq!(rest["Resource"], "*");
    let actions = strings(&rest["Action"]);
    for a in [
        "s3:GetObject",
        "s3:PutObject",
        "s3:AbortMultipartUpload",
        "s3:ListBucket",
        "kms:Decrypt",
        "kms:GenerateDataKey",
        "execute-api:Invoke",
    ] {
        assert!(
            actions.contains(&a.to_string()),
            "{a} is not passed through"
        );
    }
    // Only the leased-model statement grants Bedrock, and nothing is a wildcard action.
    assert!(
        actions
            .iter()
            .all(|a| !a.starts_with("bedrock") && !a.contains('*'))
    );
    let model = by_sid(&st, "LeasedModel").unwrap();
    assert_eq!(model["Effect"], "Allow");
    assert_eq!(
        strings(&model["Action"]),
        [
            "bedrock:InvokeModel",
            "bedrock:InvokeModelWithResponseStream"
        ]
    );
    assert_eq!(
        strings(&model["Resource"]),
        bedrock_resources(&model_id).unwrap()
    );
    assert!(st.iter().all(|s| s["Effect"] == "Allow"));
}

#[test]
fn catalog_shaped_models_stay_well_under_the_sts_limit() {
    for m in [
        format!("global.{SONNET}"),
        "us.anthropic.claude-opus-4-1-20250805-v1:0".to_string(),
        "apac.amazon.nova-premier-v1:0".to_string(),
        "aws-mantle/google.gemma-4-26b-a4b".to_string(),
        "us.meta.llama4-maverick-17b-instruct-v1:0".to_string(),
        "openai.gpt-oss-120b-1:0".to_string(),
    ] {
        let p = session_policy(&m).unwrap();
        assert!(
            p.chars().count() <= MAX_SESSION_POLICY_CHARS / 2,
            "{m}: {} characters",
            p.chars().count()
        );
    }
}

#[test]
fn a_model_id_long_enough_to_break_the_limit_is_refused() {
    let m = format!("global.{}", "a".repeat(400));
    match session_policy(&m) {
        Err(PolicyError::TooLong(n)) => assert!(n > MAX_SESSION_POLICY_CHARS),
        other => panic!("expected TooLong, got {other:?}"),
    }
}

#[test]
fn an_inference_profile_arn_in_the_home_account_and_region_leases_what_its_id_leases() {
    for id in [
        format!("us.{SONNET}"),
        "global.anthropic.claude-opus-5-5".to_string(),
    ] {
        let arn = format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/{id}");
        assert_eq!(
            bedrock_resources(&arn).unwrap(),
            bedrock_resources(&id).unwrap()
        );
        assert!(session_policy(&arn).is_ok(), "{arn}");
    }
}

#[test]
fn a_foundation_model_arn_leases_what_its_model_id_leases() {
    let id = "qwen.qwen3-coder-480b-a35b-v1:0";
    for region in ["us-west-2", "us-east-1", "us-east-2"] {
        let arn = format!("arn:aws:bedrock:{region}::foundation-model/{id}");
        assert_eq!(
            bedrock_resources(&arn).unwrap(),
            bedrock_resources(id).unwrap()
        );
    }
}

#[test]
fn arns_outside_the_home_account_region_or_known_resource_types_are_refused() {
    for m in [
        format!(
            "arn:aws:bedrock:{REGION}:{}:inference-profile/us.{SONNET}",
            other_account()
        ),
        format!(
            "arn:aws:bedrock:{}:{ACCOUNT}:inference-profile/us.{SONNET}",
            other_region()
        ),
        format!("arn:aws:bedrock:eu-west-1::foundation-model/{SONNET}"),
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:foundation-model/{SONNET}"),
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/"),
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/us.*"),
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:provisioned-model/{SONNET}"),
        format!("arn:aws-cn:bedrock:{REGION}:{ACCOUNT}:inference-profile/us.{SONNET}"),
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/aws-mantle/{SONNET}"),
    ] {
        assert!(
            matches!(session_policy(&m), Err(PolicyError::BadModel(_))),
            "{m:?} was accepted"
        );
    }
}

#[test]
fn wildcards_arns_and_odd_characters_are_refused_so_the_policy_never_widens() {
    for m in [
        "",
        "*",
        "anthropic.*",
        "us.anthropic.claude?",
        format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:application-inference-profile/x").as_str(),
        "arn:aws:bedrock:us-west-2::foundation-model",
        "a b",
        "aws-mantle/",
        "aws-mantle/x/y",
        "a\"b",
        "../x",
        "us.",
    ] {
        assert!(
            matches!(session_policy(m), Err(PolicyError::BadModel(_))),
            "{m:?} was accepted"
        );
        assert!(bedrock_resources(m).is_err(), "{m:?} was accepted");
    }
}

proptest! {
    #[test]
    fn any_well_formed_model_id_yields_a_bounded_policy_without_wildcard_resources(
        prefix in prop::sample::select(vec!["", "us.", "eu.", "apac.", "global.", "aws-mantle/"]),
        base in "[a-z][a-z0-9-]{0,20}\\.[a-z0-9.:_-]{1,40}",
    ) {
        let m = format!("{prefix}{base}");
        let p = session_policy(&m).unwrap();
        prop_assert!(p.chars().count() <= MAX_SESSION_POLICY_CHARS);
        let st = statements(&p);
        let model = by_sid(&st, "LeasedModel").unwrap();
        for r in strings(&model["Resource"]) {
            prop_assert!(!r.contains('*') && !r.contains('?'), "{}", r);
            prop_assert!(r.starts_with("arn:aws:bedrock:"), "{}", r);
        }
    }
}

#[test]
fn a_model_region_moves_the_profile_arn_and_keeps_the_foundation_models() {
    use dispatcher::policy::{bedrock_resources_in, session_policy_in};
    let id = format!("us.{SONNET}");
    let other = other_region();
    let r = bedrock_resources_in(&id, other).unwrap();
    assert_eq!(
        r[0],
        format!("arn:aws:bedrock:{other}:{ACCOUNT}:inference-profile/{id}")
    );
    assert_eq!(r[1..], bedrock_resources(&id).unwrap()[1..]);
    assert_eq!(
        bedrock_resources_in(&id, REGION).unwrap(),
        bedrock_resources(&id).unwrap()
    );
    assert!(
        session_policy_in(&id, other)
            .unwrap()
            .contains(&format!("{other}:{ACCOUNT}:inference-profile"))
    );
    for bad in ["eu-west-1", "us-west-1", ""] {
        assert!(bedrock_resources_in(&id, bad).is_err(), "{bad}");
    }
}

#[test]
fn the_session_policy_grants_no_package_registry_token() {
    // Root invariant 2: the engine gets these credentials through credential_process, so
    // they must not mint a CodeArtifact token. The dispatcher
    // mints the token with its own role and hands it to agentd in the invocation.
    for model in [format!("us.{SONNET}"), format!("aws-mantle/{SONNET}")] {
        let text = session_policy(&model).unwrap();
        for a in [
            "sts:GetServiceBearerToken",
            "codeartifact:GetAuthorizationToken",
            "codeartifact:GetRepositoryEndpoint",
            "codeartifact:ReadFromRepository",
            "codeartifact:",
        ] {
            assert!(
                !text.contains(a),
                "{model}: the session policy names {a}: {text}"
            );
        }
    }
}
