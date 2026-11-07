//! The API Gateway REST (v1) proxy event as a request, the response, the log line and the function environment.

#![allow(clippy::unwrap_used)]

use harness_protocol::research::ResearchProvider;
use lambda_http::RequestExt;
use research_api::handler::{LogLine, Response};
use research_api::lambda::{api_env, job_env, log_line, to_request, to_response};
use research_api::ports::StageKind;
use std::collections::HashMap;

const EVENT: &str = r#"{
  "resource": "/internal/research/search", "path": "/direct/internal/research/search", "httpMethod": "POST",
  "headers": {"content-type": "application/json"}, "multiValueHeaders": {"content-type": ["application/json"]},
  "queryStringParameters": null, "multiValueQueryStringParameters": null, "pathParameters": null,
  "stageVariables": null, "isBase64Encoded": false, "body": "{\"query\":\"is-plain-obj\"}",
  "requestContext": {
    "accountId": "111122223333", "resourceId": "abc", "stage": "direct", "requestId": "req-1",
    "resourcePath": "/internal/research/search", "httpMethod": "POST", "apiId": "api123", "path": "/direct/internal/research/search",
    "identity": {"userArn": "arn:aws:sts::111122223333:assumed-role/hamh-direct-harness-worker-session/01M4XAB0CDEFGHJKMNPQRSTVWX.direct",
                 "accessKey": "ASIA-test-session-key-id", "sourceIp": "10.50.0.10", "vpceId": "vpce-0123"}
  }
}"#;

#[test]
fn the_caller_account_key_route_and_body_come_from_the_event() {
    // The invocation context names this function's account: an event is a worker's only when its account is the function's.
    let mut context = lambda_runtime::Context::default();
    context.invoked_function_arn =
        "arn:aws:lambda:us-west-2:111122223333:function:hamh-direct-research-api".to_string();
    let event = lambda_http::request::from_str(EVENT)
        .unwrap()
        .with_lambda_context(context);
    let req = to_request(&event);
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("POST", "/internal/research/search")
    );
    assert_eq!(req.account_id.as_deref(), Some("111122223333"));
    assert_eq!(req.access_key.as_deref(), Some("ASIA-test-session-key-id"));
    assert_eq!(req.request_id, "req-1");
    assert!(
        req.caller_arn
            .unwrap()
            .ends_with("01M4XAB0CDEFGHJKMNPQRSTVWX.direct")
    );
    assert_eq!(req.body, "{\"query\":\"is-plain-obj\"}");
}

#[test]
fn an_event_without_identity_has_no_caller() {
    let event = EVENT.replace(r#""userArn": "arn:aws:sts::111122223333:assumed-role/hamh-direct-harness-worker-session/01M4XAB0CDEFGHJKMNPQRSTVWX.direct","#, "");
    let req = to_request(&lambda_http::request::from_str(&event).unwrap());
    assert_eq!(req.caller_arn, None);
}

#[test]
fn responses_are_json_with_the_handler_status_and_the_log_line_never_has_the_body() {
    let resp = to_response(Response {
        status: 429,
        body: "{\"code\":\"limit_rate\"}".into(),
        log: LogLine::default(),
    })
    .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let line = log_line(&LogLine {
        op: "search".into(),
        run: Some("01M4XAB0CDEFGHJKMNPQRSTVWX".into()),
        status: 200,
        code: None,
        ms: 41,
        request_id: "req-1".into(),
    });
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        (
            v["op"].as_str(),
            v["status"].as_u64(),
            v["requestId"].as_str()
        ),
        (Some("search"), Some(200), Some("req-1"))
    );
    assert!(v.get("body").is_none() && v.get("query").is_none());
}

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

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

#[test]
fn the_api_environment_is_read_whole_or_the_missing_name_is_reported() {
    let e = api_env(env(&FULL)).unwrap();
    assert_eq!(
        (e.provider, e.stage_kind, e.stage_rate_per_min),
        (ResearchProvider::Tavily, StageKind::Official, 80)
    );
    assert_eq!(e.secret_id, "hamh-direct-research-tavily-api-key");
    for (i, (name, _)) in FULL.iter().enumerate() {
        let mut pairs: Vec<(&str, &str)> = FULL.to_vec();
        pairs.remove(i);
        let err = api_env(env(&pairs)).unwrap_err();
        assert!(err.contains(name), "{name}: {err}");
    }
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "bing");
    assert!(api_env(env(&pairs)).is_err());
    pairs[4] = ("RESEARCH_PROVIDER", "tavily");
    pairs[6] = ("RESEARCH_STAGE_RATE_PER_MIN", "0");
    assert!(api_env(env(&pairs)).is_err());
}

#[test]
fn the_stage_name_of_the_metrics_is_read_from_ham_stage_and_is_direct_without_it() {
    assert_eq!(api_env(env(&FULL)).unwrap().stage, "direct");
    let mut pairs = FULL.to_vec();
    pairs.push(("HAMH_STAGE", "direct"));
    assert_eq!(api_env(env(&pairs)).unwrap().stage, "direct");
    pairs[FULL.len()] = ("HAMH_STAGE", "test-2");
    assert_eq!(api_env(env(&pairs)).unwrap().stage, "test-2");
    // An empty value is no value; a name CloudWatch would refuse stops the function at start with the variable's name and not its value.
    pairs[FULL.len()] = ("HAMH_STAGE", "");
    assert_eq!(api_env(env(&pairs)).unwrap().stage, "direct");
    let too_long = "x".repeat(33);
    for bad in ["Direct", "a b", "stage_1", "x\"y", too_long.as_str()] {
        pairs[FULL.len()] = ("HAMH_STAGE", bad);
        let err = api_env(env(&pairs)).unwrap_err();
        assert!(err.contains("HAMH_STAGE"), "{err}");
        assert!(!err.contains(bad), "{err}");
    }
}

#[test]
fn the_fixture_provider_starts_only_on_a_test_stage() {
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "fixture");
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("fixture") && err.contains("official"), "{err}");
    pairs[5] = ("HAMH_STAGE_KIND", "test");
    assert_eq!(
        api_env(env(&pairs)).unwrap().provider,
        ResearchProvider::Fixture
    );
    pairs[5] = ("HAMH_STAGE_KIND", "staging");
    assert!(api_env(env(&pairs)).is_err());
}

#[test]
fn the_job_environment_has_a_table_and_a_bucket() {
    let e = job_env(env(&FULL)).unwrap();
    assert_eq!(
        (e.table.as_str(), e.bucket.as_str()),
        ("hamh-direct-research", "hamh-direct-harness-111122223333")
    );
    assert!(
        job_env(env(&[("HARNESS_BUCKET", "b")]))
            .unwrap_err()
            .contains("RESEARCH_TABLE_NAME")
    );
}

#[test]
fn the_job_log_line_names_the_job_and_the_word_of_a_failure_and_never_its_message() {
    use harness_protocol::research::{ResearchError, ResearchErrorCode};
    use research_api::jobs::JobEnd;
    use research_api::lambda::job_log_line;
    use research_api::ports::JobEvent;
    let ev = JobEvent {
        run: "01M4XAB0CDEFGHJKMNPQRSTVWX".into(),
        agent: "direct".into(),
        job_id: "j-1".into(),
    };
    let failed = job_log_line(
        &ev,
        &JobEnd::Failed(ResearchError::new(
            ResearchErrorCode::NotFound,
            "GitHub has no such repository",
        )),
    );
    let v: serde_json::Value = serde_json::from_str(&failed).unwrap();
    assert_eq!(
        (
            v["outcome"].as_str(),
            v["detail"].as_str(),
            v["jobId"].as_str()
        ),
        (Some("failed"), Some("not_found"), Some("j-1"))
    );
    assert!(!failed.contains("GitHub has no such"));
    let skipped = job_log_line(&ev, &JobEnd::Skipped("the job is not queued"));
    assert!(skipped.contains("skipped") && skipped.contains("the job is not queued"));
}

// ---- the agentcore provider --------------------------------------------------------------------------------------

const GATEWAY_URL: &str =
    "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp";

fn agentcore_pairs() -> Vec<(&'static str, &'static str)> {
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "agentcore");
    pairs.push(("AGENTCORE_GATEWAY_URL", GATEWAY_URL));
    pairs
}

#[test]
fn the_agentcore_provider_reads_the_gateway_address_and_the_default_tool() {
    let e = api_env(env(&agentcore_pairs())).unwrap();
    assert_eq!(e.provider, ResearchProvider::Agentcore);
    let g = e.agentcore.expect("the gateway settings");
    assert_eq!(g.gateway_url, GATEWAY_URL);
    assert_eq!(g.region, "us-east-1");
    assert_eq!(g.tool, "web-search-tool___WebSearch");
    // The tool name is optional configuration.
    let mut pairs = agentcore_pairs();
    pairs.push(("AGENTCORE_SEARCH_TOOL", "websearch___WebSearch"));
    assert_eq!(
        api_env(env(&pairs)).unwrap().agentcore.unwrap().tool,
        "websearch___WebSearch"
    );
}

#[test]
fn the_official_stage_may_name_agentcore_and_still_refuses_the_fixture() {
    let e = api_env(env(&agentcore_pairs())).unwrap();
    assert_eq!(e.stage_kind, StageKind::Official);
    // No secret is read for this provider, but the stack still passes the (empty) Tavily secret name like it does for every provider.
    assert_eq!(e.secret_id, "hamh-direct-research-tavily-api-key");
    let mut pairs = agentcore_pairs();
    pairs[4] = ("RESEARCH_PROVIDER", "fixture");
    assert!(api_env(env(&pairs)).is_err());
}

#[test]
fn the_other_providers_need_no_gateway_and_report_none() {
    for provider in ["tavily", "tavily-keyless"] {
        let mut pairs = FULL.to_vec();
        pairs[4] = ("RESEARCH_PROVIDER", provider);
        assert_eq!(api_env(env(&pairs)).unwrap().agentcore, None, "{provider}");
    }
    // A gateway address that is set for a stage on another provider is not read, so a bad one does not stop the function.
    let mut pairs = FULL.to_vec();
    pairs.push(("AGENTCORE_GATEWAY_URL", "not a url"));
    assert_eq!(api_env(env(&pairs)).unwrap().agentcore, None);
}

#[test]
fn the_unknown_provider_error_lists_the_four_words() {
    let mut pairs = FULL.to_vec();
    pairs[4] = ("RESEARCH_PROVIDER", "bing");
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(
        err.contains("tavily, tavily-keyless, agentcore or fixture"),
        "{err}"
    );
}

#[test]
fn an_agentcore_stage_without_a_valid_gateway_address_stops_at_start_with_the_names_and_not_the_value()
 {
    let mut pairs = agentcore_pairs();
    pairs.pop();
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("AGENTCORE_GATEWAY_URL"), "{err}");

    for bad in [
        "http://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/other",
        "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com:8443/mcp",
        "https://example.org/mcp",
        "gw-abc12345",
    ] {
        let mut pairs = agentcore_pairs();
        pairs.pop();
        pairs.push(("AGENTCORE_GATEWAY_URL", bad));
        let err = api_env(env(&pairs)).unwrap_err();
        assert!(err.contains("AGENTCORE_GATEWAY_URL"), "{bad}: {err}");
        assert!(!err.contains(bad), "the value is not echoed: {err}");
    }
    let mut pairs = agentcore_pairs();
    pairs.push(("AGENTCORE_SEARCH_TOOL", "a tool; with spaces"));
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(err.contains("AGENTCORE_SEARCH_TOOL"), "{err}");
}

#[test]
fn the_gateway_must_be_in_the_region_the_function_runs_in() {
    let mut pairs = agentcore_pairs();
    pairs.push(("AWS_REGION", "us-east-1"));
    assert!(api_env(env(&pairs)).is_ok());
    let mut pairs = agentcore_pairs();
    pairs.push(("AWS_REGION", "eu-west-1"));
    let err = api_env(env(&pairs)).unwrap_err();
    assert!(
        err.contains("AGENTCORE_GATEWAY_URL") && err.contains("Region"),
        "{err}"
    );
}
