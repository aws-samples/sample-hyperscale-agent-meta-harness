//! Wave 2 (backend lambdas): the stage-wide search rate is a setting of the function environment. Any positive number is taken, so a stage with a
//! production key (800 searches a minute) uses its whole plan with no code edit. This pins the reader; the stage configuration that sets the
//! variable belongs to the CDK app.
#![allow(clippy::unwrap_used)]

use research_api::lambda::api_env;
use std::collections::HashMap;

fn full(rate: &str) -> HashMap<String, String> {
    [
        ("RESEARCH_TABLE_NAME", "hamh-direct-research"),
        ("HARNESS_BUCKET", "hamh-direct-harness-111122223333"),
        ("TAVILY_SECRET_ID", "hamh-direct-research-tavily-api-key"),
        ("RESEARCH_JOB_FUNCTION", "hamh-direct-research-job"),
        ("RESEARCH_PROVIDER", "tavily"),
        ("HAMH_STAGE_KIND", "official"),
        ("RESEARCH_STAGE_RATE_PER_MIN", rate),
        (
            "WORKER_SESSION_ROLE_NAME",
            "hamh-direct-harness-worker-session",
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[test]
fn b776w_the_stage_rate_is_whatever_positive_number_the_environment_holds() {
    for (word, want) in [("1", 1), ("80", 80), ("800", 800), ("100000", 100_000)] {
        let m = full(word);
        let e = api_env(move |k| m.get(k).cloned()).unwrap();
        assert_eq!(e.stage_rate_per_min, want, "{word}");
    }
    for bad in ["0", "-5", "many", "", "80.5"] {
        let m = full(bad);
        let e = api_env(move |k| m.get(k).cloned());
        assert!(e.is_err(), "{bad:?} is no rate");
    }
}
