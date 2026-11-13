//! The research provider `auto` in run-session: the stack gives run-session the same `RESEARCH_PROVIDER` word as research-api,
//! and the default word is now `auto`. run-session writes the policy row of each run, and a row names a search provider of the wire (`tavily`,
//! `tavily-keyless`, `agentcore` or `fixture`); `auto` is not one of them. It reads as the provider that serves while no key is stored, `agentcore`. The
//! row's provider is informational: research-api chooses the provider of each request itself, and every answer, record and audit object names the provider
//! that served it. The four older words keep their meaning.
#![allow(clippy::unwrap_used)]

use harness_protocol::research::{ResearchLimits, ResearchProvider};
use run_session::lambda::research_from_lookup;
use run_session::ops::ResearchStage;
use std::collections::HashMap;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

fn read(provider: &str) -> Result<Option<ResearchStage>, String> {
    research_from_lookup(&env(&[
        ("RESEARCH_TABLE_NAME", "t"),
        ("RESEARCH_PROVIDER", provider),
    ]))
}

#[test]
fn a_stage_on_the_auto_provider_is_read_and_its_rows_name_agentcore() {
    let on = read("auto").unwrap().unwrap();
    assert_eq!(
        (on.provider, on.limits.clone()),
        (ResearchProvider::Agentcore, ResearchLimits::default())
    );
    // The row a run gets passes the wire's own check, so research-api can read it.
    harness_protocol::research::PolicyItem::new(
        "01M4XAB0CDEFGHJKMNPQRSTVWX",
        true,
        on.provider,
        on.limits,
        1_792_200_000,
    )
    .validate()
    .unwrap();
}

#[test]
fn auto_still_needs_the_table_like_every_word_and_otherwise_research_is_off() {
    assert_eq!(
        research_from_lookup(&env(&[("RESEARCH_PROVIDER", "auto")])).unwrap(),
        None
    );
    assert_eq!(
        research_from_lookup(&env(&[
            ("RESEARCH_TABLE_NAME", "t"),
            ("RESEARCH_PROVIDER", "")
        ]))
        .unwrap(),
        None
    );
}

#[test]
fn the_limits_override_applies_to_auto_as_to_the_others() {
    let limits = ResearchLimits {
        searches: 50,
        ..ResearchLimits::default()
    };
    let json = serde_json::to_string(&limits).unwrap();
    let got = research_from_lookup(&env(&[
        ("RESEARCH_TABLE_NAME", "t"),
        ("RESEARCH_PROVIDER", "auto"),
        ("RESEARCH_LIMITS_JSON", &json),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        (got.provider, got.limits.searches),
        (ResearchProvider::Agentcore, 50)
    );
}

#[test]
fn the_four_older_words_keep_their_meaning_and_other_spellings_of_auto_are_refused() {
    for (word, provider) in [
        ("tavily", ResearchProvider::Tavily),
        ("tavily-keyless", ResearchProvider::TavilyKeyless),
        ("agentcore", ResearchProvider::Agentcore),
        ("fixture", ResearchProvider::Fixture),
    ] {
        assert_eq!(read(word).unwrap().unwrap().provider, provider, "{word}");
    }
    for bad in ["Auto", "AUTO", " auto", "auto ", "automatic", "bing"] {
        let err = read(bad).unwrap_err();
        assert!(err.contains("RESEARCH_PROVIDER"), "{bad:?}: {err}");
        assert!(err.contains("unknown provider"), "{bad:?}: {err}");
    }
}
