//! The session policy of a run with failover routes. The credentials of a direct run were narrowed
//! to ONE model's ARNs; a run that may switch to another route or model needs every route's ARNs in the same inline policy, which STS
//! limits to 2,048 characters. The foundation-model ARNs carry the Region wildcard `us-*` to fit:
//! effective permissions are the intersection of this policy and the
//! role's identity policy, and the role lists the foundation models and profiles of the three inference Regions only, so the wildcard
//! cannot reach a Region the role does not allow.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use dispatcher::ids::runtime_session_id;
use dispatcher::items::{Dispatch, agent_item, mint_plan};
use dispatcher::policy::{
    MAX_SESSION_POLICY_CHARS, PolicyError, session_policy_for_routes, session_policy_in,
};
use dispatcher::stage::{ACCOUNT, REGION};
use harness_protocol::common::Engine;
use serde_json::Value;
use std::collections::BTreeSet;

/// The nine routes of the five supported models (`engine/catalog/models.json`, primary and alternate routes).
const NINE: [&str; 9] = [
    "us.anthropic.claude-opus-5-5",
    "global.anthropic.claude-opus-5-5",
    "global.anthropic.claude-sonnet-5-5",
    "us.anthropic.claude-sonnet-5-5",
    "us.openai.gpt-6.1-sol",
    "global.openai.gpt-6.1-sol",
    "us.openai.gpt-6-astra",
    "global.openai.gpt-6-astra",
    "us.zai.glm-5.3",
];

fn routes(ids: &[&str]) -> Vec<(String, String)> {
    ids.iter()
        .map(|i| (i.to_string(), REGION.to_string()))
        .collect()
}

fn as_refs(r: &[(String, String)]) -> Vec<(&str, &str)> {
    r.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
}

fn policy(ids: &[&str]) -> Result<String, PolicyError> {
    let r = routes(ids);
    session_policy_for_routes(&as_refs(&r))
}

fn statements(policy: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(policy).unwrap();
    assert_eq!(v["Version"], "2012-10-17");
    v["Statement"].as_array().unwrap().clone()
}

fn leased(policy: &str) -> BTreeSet<String> {
    let st = statements(policy);
    let m = st.iter().find(|s| s["Sid"] == "LeasedModel").unwrap();
    m["Resource"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect()
}

fn base(id: &str) -> &str {
    id.split_once('.').map_or(id, |(_, rest)| rest)
}

#[test]
fn the_nine_routes_of_the_five_models_fit_the_sts_limit_with_room_to_spare() {
    let p = policy(&NINE).unwrap();
    let n = p.chars().count();
    assert!(n <= MAX_SESSION_POLICY_CHARS, "{n} characters");
    assert!(
        n <= MAX_SESSION_POLICY_CHARS - 100,
        "{n} characters leaves no room for the next route"
    );
}

#[test]
fn the_resources_are_exactly_the_profiles_and_the_foundation_models_of_their_bases() {
    let got = leased(&policy(&NINE).unwrap());
    let mut want = BTreeSet::new();
    for id in NINE {
        want.insert(format!(
            "arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/{id}"
        ));
        want.insert(format!(
            "arn:aws:bedrock:us-*::foundation-model/{}",
            base(id)
        ));
        if id.starts_with("global.") {
            // The foundation model of a global profile is also addressed with no Region.
            want.insert(format!("arn:aws:bedrock:::foundation-model/{}", base(id)));
        }
    }
    assert_eq!(got, want);
}

#[test]
fn only_the_us_region_segment_of_a_foundation_model_arn_is_a_wildcard() {
    for r in leased(&policy(&NINE).unwrap()) {
        let stars = r.matches('*').count();
        if stars == 0 {
            continue;
        }
        assert_eq!(stars, 1, "{r}");
        assert!(
            r.starts_with("arn:aws:bedrock:us-*::foundation-model/"),
            "{r}"
        );
        assert!(!r.ends_with('*') && !r.ends_with("/*"), "{r}");
    }
}

#[test]
fn nothing_but_the_listed_models_is_named() {
    let got = leased(
        &policy(&[
            "us.anthropic.claude-opus-5-5",
            "global.anthropic.claude-opus-5-5",
        ])
        .unwrap(),
    );
    assert!(got.iter().all(|r| r.contains("claude-opus-5-5")), "{got:?}");
    assert!(
        !got.iter()
            .any(|r| r.contains("sonnet") || r.contains("gpt"))
    );
}

#[test]
fn one_route_is_the_old_single_model_policy_byte_for_byte() {
    for id in NINE {
        let r = routes(&[id]);
        assert_eq!(
            session_policy_for_routes(&as_refs(&r)).unwrap(),
            session_policy_in(id, REGION).unwrap(),
            "{id}"
        );
    }
}

#[test]
fn a_route_listed_twice_is_granted_once() {
    assert_eq!(
        policy(&[
            "us.openai.gpt-6.1-sol",
            "global.openai.gpt-6.1-sol",
            "us.openai.gpt-6.1-sol"
        ])
        .unwrap(),
        policy(&["us.openai.gpt-6.1-sol", "global.openai.gpt-6.1-sol"]).unwrap()
    );
}

#[test]
fn the_roles_other_grants_stay_as_they_were() {
    let r = routes(&[
        "us.anthropic.claude-opus-5-5",
        "global.anthropic.claude-opus-5-5",
    ]);
    let st = statements(&session_policy_for_routes(&as_refs(&r)).unwrap());
    assert_eq!(st.len(), 2);
    assert!(
        st.iter()
            .any(|s| s["Sid"] == "RoleGrants" && s["Resource"] == "*")
    );
    let single = statements(&session_policy_in("us.anthropic.claude-opus-5-5", REGION).unwrap());
    let grants = |v: &[Value]| v.iter().find(|s| s["Sid"] == "RoleGrants").unwrap().clone();
    assert_eq!(grants(&st), grants(&single));
    assert!(st.iter().all(|s| s["Effect"] == "Allow"));
}

#[test]
fn a_mantle_route_still_brings_the_mantle_statement() {
    let st =
        statements(&policy(&["us.anthropic.claude-opus-5-5", "aws-mantle/zai.glm-5"]).unwrap());
    assert!(st.iter().any(|s| s["Sid"] == "LeasedMantle"));
}

#[test]
fn a_bad_route_refuses_the_whole_policy() {
    for bad in [
        "",
        "us.anthropic.claude-opus-5-5 ",
        "us.anthropic.*",
        "*",
        "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-opus-5-5",
        "us.vendor/model",
    ] {
        match policy(&["us.openai.gpt-6.1-sol", bad]) {
            Err(PolicyError::BadModel(_)) => {}
            other => panic!("{bad:?}: {other:?}"),
        }
    }
}

#[test]
fn a_route_in_a_region_that_is_not_an_inference_region_is_refused() {
    let r = vec![
        (
            "us.anthropic.claude-opus-5-5".to_string(),
            REGION.to_string(),
        ),
        (
            "global.anthropic.claude-opus-5-5".to_string(),
            "eu-west-1".to_string(),
        ),
    ];
    assert!(matches!(
        session_policy_for_routes(&as_refs(&r)),
        Err(PolicyError::BadRegion(_))
    ));
}

#[test]
fn no_routes_is_not_a_policy() {
    assert!(matches!(
        session_policy_for_routes(&[]),
        Err(PolicyError::BadModel(_))
    ));
}

#[test]
fn too_many_routes_for_the_limit_are_refused_with_the_size() {
    let long = (0..40)
        .map(|i| format!("global.vendor{i}.model-{i}-abcdefghijklmnopqrstuvwxyz"))
        .collect::<Vec<_>>();
    let refs: Vec<&str> = long.iter().map(String::as_str).collect();
    assert!(matches!(policy(&refs), Err(PolicyError::TooLong(n)) if n > MAX_SESSION_POLICY_CHARS));
}

// ----: the wildcard must not reach the Region-less ARN of a model with no global profile ------------------------------------

/// IAM's case-insensitive glob (`*` and `?`) of `pattern` over `text`.
fn iam_glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    // ok[i][j]: pattern[..i] matches text[..j]
    let mut ok = vec![vec![false; t.len() + 1]; p.len() + 1];
    ok[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            ok[i][0] = ok[i - 1][0];
        }
        for j in 1..=t.len() {
            ok[i][j] = match p[i - 1] {
                '*' => ok[i - 1][j] || ok[i][j - 1],
                '?' => ok[i - 1][j - 1],
                c => ok[i - 1][j - 1] && c == t[j - 1],
            };
        }
    }
    ok[p.len()][t.len()]
}

/// True when some resource of the leased statement matches `arn`.
fn reaches(resources: &BTreeSet<String>, arn: &str) -> bool {
    resources.iter().any(|r| iam_glob(r, arn))
}

fn global_bases() -> BTreeSet<&'static str> {
    NINE.iter()
        .filter(|i| i.starts_with("global."))
        .map(|i| base(i))
        .collect()
}

fn all_bases() -> BTreeSet<&'static str> {
    NINE.iter().map(|i| base(i)).collect()
}

#[test]
fn no_resource_reaches_the_region_less_arn_of_a_model_that_has_no_global_profile() {
    let got = leased(&policy(&NINE).unwrap());
    assert!(
        !reaches(&got, "arn:aws:bedrock:::foundation-model/zai.glm-5.3"),
        "glm has only a us. profile; a Region-less ARN is what a global. profile call uses: {got:?}"
    );
    for b in all_bases().difference(&global_bases()) {
        assert!(
            !reaches(&got, &format!("arn:aws:bedrock:::foundation-model/{b}")),
            "{b}"
        );
    }
}

#[test]
fn the_region_less_arn_is_granted_for_every_model_with_a_global_profile_and_for_no_other() {
    let got = leased(&policy(&NINE).unwrap());
    for b in global_bases() {
        assert!(
            got.contains(&format!("arn:aws:bedrock:::foundation-model/{b}")),
            "{b}: a global. profile call needs the Region-less ARN"
        );
    }
    let region_less: Vec<&String> = got
        .iter()
        .filter(|r| r.starts_with("arn:aws:bedrock:::"))
        .collect();
    assert_eq!(region_less.len(), global_bases().len(), "{region_less:?}");
}

#[test]
fn the_foundation_models_stay_reachable_in_the_us_regions_of_the_role_ceiling_and_nowhere_else() {
    let got = leased(&policy(&NINE).unwrap());
    for b in all_bases() {
        for region in ["us-west-2", "us-east-1", "us-east-2"] {
            assert!(
                reaches(
                    &got,
                    &format!("arn:aws:bedrock:{region}::foundation-model/{b}")
                ),
                "{b} in {region}: a us. profile call lands there"
            );
        }
        for region in [
            "eu-west-1",
            "ap-southeast-2",
            "ca-central-1",
            "sa-east-1",
            "af-south-1",
        ] {
            assert!(
                !reaches(
                    &got,
                    &format!("arn:aws:bedrock:{region}::foundation-model/{b}")
                ),
                "{b} in {region}: outside the US the session policy names nothing, whatever the role says"
            );
        }
        assert!(
            !reaches(
                &got,
                &format!("arn:aws:bedrock:us-east-1:{ACCOUNT}:foundation-model/{b}")
            ),
            "{b}: a foundation model has no account"
        );
    }
}

#[test]
fn the_policy_with_the_real_session_tags_stays_under_the_sts_limit_with_room_to_spare() {
    // STS packs the session policy and the session tags together (PackedPolicySize). Measured 2026-10-15 against the stage's
    // harness-worker-session role: this policy with the five run tags packs to 36 percent, and a 2,048-character policy to 37 percent,
    // so the 2,048-character limit is the one that binds. The tags are the ones the dispatcher mints with (`worker_api::creds::decide`).
    let policy = policy(&NINE).unwrap();
    let d = Dispatch {
        run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
        agent_id: "direct".into(),
        task_id: "direct".into(),
        engine: Engine::CLINE,
        engine_version: "3.0.71".into(),
        model_id: NINE[2].into(),
        runtime_arn: format!("arn:aws:bedrock-agentcore:{REGION}:{ACCOUNT}:runtime/w-abc"),
        runtime_session_id: runtime_session_id("01M4ZHFBV5SDX7J92E6YRH4QTW", "direct", 1),
        tenant: "testuser".into(),
        mode: "s3-only".into(),
    };
    let nonce = "n".repeat(43);
    let plan = mint_plan(&agent_item(&d, &nonce, &policy), &nonce).unwrap();
    assert_eq!(plan.tags.len(), 5, "{:?}", plan.tags);
    let tag_chars: usize = plan
        .tags
        .iter()
        .map(|(k, v)| k.chars().count() + v.chars().count())
        .sum();
    let total = plan.session_policy.chars().count() + tag_chars;
    assert_eq!(plan.session_policy, policy);
    assert!(
        total <= MAX_SESSION_POLICY_CHARS - 100,
        "policy plus tags is {total} characters"
    );
}

#[test]
fn the_wildcard_form_is_the_one_the_a7_judge_is_tested_on() {
    // `ham-e2e` keeps the policy of a Sonnet run as a literal (the judge must not read the code it judges); this keeps the literal honest.
    let text = include_str!("../../ham-e2e/tests/live_direct_support/a7_rc3_session_policy.json");
    // The file is a recording: its profile ARNs name the Region and account of the stage it was recorded on. They are read from the file itself and moved to
    // this build's stage, so the test does not depend on which `accounts.json` entry the build selected.
    let recorded: Value = serde_json::from_str(text).unwrap();
    let first = recorded["Statement"][1]["Resource"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|r| r.contains(":inference-profile/"))
        .unwrap();
    let parts: Vec<&str> = first.splitn(6, ':').collect();
    let was = format!(
        "arn:aws:bedrock:{}:{}:inference-profile/",
        parts[3], parts[4]
    );
    let prefix = format!("arn:aws:bedrock:{REGION}:{ACCOUNT}:inference-profile/");
    let fixture: Value = serde_json::from_str(&text.replace(&was, &prefix)).unwrap();
    let resources = fixture["Statement"][1]["Resource"].as_array().unwrap();
    let ids: Vec<&str> = resources
        .iter()
        .filter_map(|r| r.as_str().unwrap().strip_prefix(prefix.as_str()))
        .collect();
    assert_eq!(ids.len(), 9, "{ids:?}");
    let want: Value = serde_json::from_str(&policy(&ids).unwrap()).unwrap();
    assert_eq!(fixture, want);
}
