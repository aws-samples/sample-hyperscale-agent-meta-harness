//! The function environment of an `auto` stage: the word `auto` is a configuration word of this function and of the stack,
//! not a member of the wire's `ResearchProvider`. It needs the gateway settings and the key secret's name, it is allowed on every stage, and the four
//! older words keep their meaning.

#![allow(clippy::unwrap_used)]

use harness_protocol::research::ResearchProvider;
use research_api::lambda::api_env;
use research_api::ports::StageKind;
use research_api::providers::StageChoice;
use std::collections::HashMap;

const GATEWAY_URL: &str =
    "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp";

const FULL: [(&str, &str); 8] = [
    ("RESEARCH_TABLE_NAME", "hamh-direct-research"),
    ("HARNESS_BUCKET", "hamh-direct-harness-111122223333"),
    ("TAVILY_SECRET_ID", "hamh-direct-research-tavily-api-key"),
    ("RESEARCH_JOB_FUNCTION", "hamh-direct-research-job"),
    ("RESEARCH_PROVIDER", "tavily"),
    ("HAMH_STAGE_KIND", "official"),
    ("RESEARCH_STAGE_RATE_PER_MIN", "80"),
    (
        "WORKER_SESSION_ROLE_NAME",
        "hamh-direct-harness-worker-session",
    ),
];

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

fn auto_pairs() -> Vec<(&'static str, &'static str)> {
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "auto");
    pairs.push(("AGENTCORE_GATEWAY_URL", GATEWAY_URL));
    pairs
}

#[test]
fn auto_reads_the_gateway_settings_and_the_key_secret_name() {
    let e = api_env(env(&auto_pairs())).unwrap();
    assert_eq!(e.choice(), StageChoice::Auto);
    assert_eq!(e.secret_id, "hamh-direct-research-tavily-api-key");
    let g = e.agentcore.expect("the gateway settings");
    assert_eq!(g.gateway_url, GATEWAY_URL);
    assert_eq!(g.region, "us-east-1");
    assert_eq!(g.tool, "web-search-tool___WebSearch");
    // The search tool name is optional configuration, as on an agentcore stage.
    let mut pairs = auto_pairs();
    pairs.push(("AGENTCORE_SEARCH_TOOL", "websearch___WebSearch"));
    assert_eq!(
        api_env(env(&pairs)).unwrap().agentcore.unwrap().tool,
        "websearch___WebSearch"
    );
}

#[test]
fn auto_is_allowed_on_an_official_stage_and_on_a_test_stage() {
    assert_eq!(
        api_env(env(&auto_pairs())).unwrap().stage_kind,
        StageKind::Official
    );
    let mut pairs = auto_pairs();
    pairs[5] = ("HAMH_STAGE_KIND", "test");
    let e = api_env(env(&pairs)).unwrap();
    assert_eq!(
        (e.stage_kind, e.choice()),
        (StageKind::Test, StageChoice::Auto)
    );
}

#[test]
fn an_auto_stage_without_a_valid_gateway_address_stops_at_start_with_the_name_and_not_the_value() {
    let mut pairs = auto_pairs();
    pairs.pop();
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("AGENTCORE_GATEWAY_URL"), "{err}");
    for bad in [
        "http://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://example.org/mcp",
        "gw-abc12345",
    ] {
        let mut pairs = auto_pairs();
        pairs.pop();
        pairs.push(("AGENTCORE_GATEWAY_URL", bad));
        let err = api_env(env(&pairs)).unwrap_err();
        assert!(err.contains("AGENTCORE_GATEWAY_URL"), "{bad}: {err}");
        assert!(!err.contains(bad), "the value is not echoed: {err}");
    }
    let mut pairs = auto_pairs();
    pairs.push(("AWS_REGION", "us-west-2"));
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("Region"), "{err}");
}

#[test]
fn an_auto_stage_still_needs_the_key_secret_name_like_every_stage() {
    let mut pairs = auto_pairs();
    pairs.retain(|(k, _)| *k != "TAVILY_SECRET_ID");
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("TAVILY_SECRET_ID"), "{err}");
}

#[test]
fn the_four_older_words_pin_their_provider_and_are_not_auto() {
    for (word, provider) in [
        ("tavily", ResearchProvider::Tavily),
        ("tavily-keyless", ResearchProvider::TavilyKeyless),
    ] {
        let mut pairs = FULL.to_vec();
        pairs[4] = ("RESEARCH_PROVIDER", word);
        let e = api_env(env(&pairs)).unwrap();
        assert_eq!(e.provider, provider);
        assert_eq!(e.choice(), StageChoice::Pinned(provider), "{word}");
        assert_eq!(e.agentcore, None, "{word}");
    }
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "agentcore");
    pairs.push(("AGENTCORE_GATEWAY_URL", GATEWAY_URL));
    let e = api_env(env(&pairs)).unwrap();
    assert_eq!(e.provider, ResearchProvider::Agentcore);
    assert_eq!(e.choice(), StageChoice::Pinned(ResearchProvider::Agentcore));
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "fixture");
    pairs[5] = ("HAMH_STAGE_KIND", "test");
    let e = api_env(env(&pairs)).unwrap();
    assert_eq!(e.choice(), StageChoice::Pinned(ResearchProvider::Fixture));
}

#[test]
fn the_word_is_exact_and_the_unknown_word_error_lists_the_five() {
    for bad in ["Auto", "AUTO", " auto", "auto ", "automatic", "bing"] {
        let mut pairs = auto_pairs();
        pairs[4] = ("RESEARCH_PROVIDER", bad);
        let err = api_env(env(&pairs)).unwrap_err();
        assert!(
            err.contains("auto, tavily, tavily-keyless, agentcore or fixture"),
            "{bad:?}: {err}"
        );
        assert!(
            !err.contains(&format!("{bad:?}")),
            "the value is not echoed: {err}"
        );
    }
}

#[test]
fn the_fixture_is_still_refused_on_an_official_stage() {
    let mut pairs = auto_pairs();
    pairs[4] = ("RESEARCH_PROVIDER", "fixture");
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("fixture") && err.contains("official"), "{err}");
}
