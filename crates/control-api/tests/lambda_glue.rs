//! Mapping between the API Gateway REST (v1) proxy event and the handler's request and response.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use control_api::handler::V1Response;
use control_api::lambda::{access_line, to_response, to_v1};

const EVENT: &str = r#"{
  "resource": "/v1/runs/{runId}/events", "path": "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events", "httpMethod": "GET",
  "headers": {"Idempotency-Key": "k-1", "Content-Type": "application/json"},
  "multiValueHeaders": {"Idempotency-Key": ["k-1"], "Content-Type": ["application/json"]},
  "queryStringParameters": {"shard": "00", "after": "01K5ZQ4T8M3N6P7R9S0V1W2X3Y"},
  "multiValueQueryStringParameters": {"shard": ["00"], "after": ["01K5ZQ4T8M3N6P7R9S0V1W2X3Y"]},
  "pathParameters": {"runId": "01K5ZQ4T8M3N6P7R9S0V1W2X3Y"},
  "stageVariables": null, "isBase64Encoded": false, "body": "{\"prompt\":\"secret prompt text\"}",
  "requestContext": {
    "accountId": "800000000022", "resourceId": "abc", "stage": "live", "requestId": "req-1",
    "resourcePath": "/v1/runs/{runId}/events", "httpMethod": "GET", "apiId": "api123",
    "path": "/live/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events",
    "identity": {"userArn": "arn:aws:sts::800000000022:assumed-role/Admin/testuser", "sourceIp": "203.0.113.9"}
  }
}"#;

#[test]
fn caller_method_path_headers_query_and_body_come_from_the_event() {
    let req = lambda_http::request::from_str(EVENT).unwrap();
    let v1 = to_v1(&req);
    assert_eq!(v1.method, "GET");
    assert_eq!(v1.path, "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events");
    assert_eq!(
        v1.caller_arn.as_deref(),
        Some("arn:aws:sts::800000000022:assumed-role/Admin/testuser")
    );
    assert_eq!(v1.headers["idempotency-key"], "k-1");
    assert_eq!(v1.query["shard"], "00");
    assert_eq!(v1.query["after"], "01K5ZQ4T8M3N6P7R9S0V1W2X3Y");
    assert_eq!(v1.body, "{\"prompt\":\"secret prompt text\"}");
}

#[test]
fn a_stage_prefix_on_the_path_is_dropped() {
    let event = EVENT.replace(
        r#""path": "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events", "httpMethod""#,
        r#""path": "/live/v1/whoami", "httpMethod""#,
    );
    let req = lambda_http::request::from_str(&event).unwrap();
    assert_eq!(to_v1(&req).path, "/v1/whoami");
}

#[test]
fn an_event_without_identity_has_no_caller() {
    let event = EVENT.replace(
        r#""userArn": "arn:aws:sts::800000000022:assumed-role/Admin/testuser", "#,
        "",
    );
    let req = lambda_http::request::from_str(&event).unwrap();
    assert_eq!(to_v1(&req).caller_arn, None);
}

#[test]
fn responses_are_json_with_the_handler_status() {
    let resp = to_response(V1Response {
        status: 201,
        body: "{\"runId\":\"x\"}".into(),
        retry_after_s: None,
    })
    .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.headers()["content-type"], "application/json");
    // A presigned URL in a body is a bearer capability: no cache keeps it.
    assert_eq!(resp.headers()["cache-control"], "no-store");
}

/// The backend's version as the root `VERSION` file holds it (the one source of the `ham vN.NN | backend vN.NN` tag).
fn root_version() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../VERSION");
    std::fs::read_to_string(&path).unwrap().trim().to_string()
}

#[test]
fn every_answer_names_the_backend_version_whatever_its_status() {
    // The client reads `x-hamh-backend-version` off any answer: a success, a refusal, a missing run and a failure alike.
    let version = root_version();
    for status in [200, 201, 400, 403, 404, 409, 422, 500, 503] {
        let resp = to_response(V1Response {
            status,
            body: "{}".into(),
            retry_after_s: None,
        })
        .unwrap();
        assert_eq!(resp.status(), status);
        assert_eq!(
            resp.headers()["x-hamh-backend-version"],
            version.as_str(),
            "status {status}"
        );
        // The version header does not displace the headers every answer already carried.
        assert_eq!(
            resp.headers()["cache-control"],
            "no-store",
            "status {status}"
        );
        assert_eq!(
            resp.headers()["content-type"],
            "application/json",
            "status {status}"
        );
    }
}

/// The earlier name of the version header, sent next to the new one until the compatibility window ends.
const EARLIER_VERSION_HEADER: &str = "x-cah-backend-version"; // COMPAT-CAH

#[test]
fn every_answer_also_names_the_backend_version_under_the_earlier_header() {
    // A client of the earlier release reads the earlier name: the same value rides on a success, a refusal, a missing run and a failure alike.
    let version = root_version();
    for status in [200, 201, 400, 403, 404, 409, 422, 500, 503] {
        let resp = to_response(V1Response {
            status,
            body: "{}".into(),
            retry_after_s: None,
        })
        .unwrap();
        assert_eq!(
            resp.headers()[EARLIER_VERSION_HEADER],
            version.as_str(),
            "status {status}"
        );
        assert_eq!(
            resp.headers()["x-hamh-backend-version"],
            resp.headers()[EARLIER_VERSION_HEADER],
            "both names carry one value, status {status}"
        );
    }
}

#[test]
fn the_access_line_names_caller_route_and_status_and_never_the_body() {
    let req = lambda_http::request::from_str(EVENT).unwrap();
    let line = access_line(&to_v1(&req), 200);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        v["caller"],
        "arn:aws:sts::800000000022:assumed-role/Admin/testuser"
    );
    assert_eq!(v["method"], "GET");
    assert_eq!(v["path"], "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events");
    assert_eq!(v["status"], 200);
    assert!(!line.contains("secret prompt text"), "{line}");
    assert!(v.get("body").is_none());
}

#[test]
fn the_access_line_names_the_callers_source_ip() {
    // The dead-window proof counts `/v1` requests by the
    // client host's egress IP, and the /v1 API has no API Gateway access log, so the
    // access line carries `requestContext.identity.sourceIp`.
    let req = lambda_http::request::from_str(EVENT).unwrap();
    let v: serde_json::Value = serde_json::from_str(&access_line(&to_v1(&req), 200)).unwrap();
    assert_eq!(v["ip"], "203.0.113.9");
    let bare = EVENT.replace(r#", "sourceIp": "203.0.113.9""#, "");
    let req = lambda_http::request::from_str(&bare).unwrap();
    let v: serde_json::Value = serde_json::from_str(&access_line(&to_v1(&req), 200)).unwrap();
    assert!(v["ip"].is_null(), "{v}");
    assert!(v.as_object().unwrap().contains_key("ip"), "{v}");
}

#[test]
fn the_operator_principals_setting_is_a_list_of_arn_patterns() {
    use control_api::lambda::operators_setting;
    assert_eq!(
        operators_setting("arn:aws:sts::800000000022:assumed-role/Admin/*").unwrap(),
        ["arn:aws:sts::800000000022:assumed-role/Admin/*"]
    );
    assert_eq!(
        operators_setting(
            " arn:aws:iam::800000000022:role/a , arn:aws:sts::800000000022:assumed-role/b/* "
        )
        .unwrap(),
        [
            "arn:aws:iam::800000000022:role/a",
            "arn:aws:sts::800000000022:assumed-role/b/*"
        ]
    );
    assert!(
        operators_setting("")
            .unwrap_err()
            .contains("names no principal")
    );
    assert!(
        operators_setting("Admin")
            .unwrap_err()
            .contains("not an IAM ARN pattern")
    );
}

// The account file may name the operator of the stage (an IAM role, an IAM Identity Center role or an IAM user), and the CDK app turns it into the patterns below
// (infra/lib/operator.ts `callerArnPatterns`, the table infra/test/fixtures/operator-identity.json). control-api reads any list of ARN patterns, so the setting for each form
// is accepted and admits that identity and no other.
#[test]
fn the_operator_setting_of_each_identity_form_admits_that_identity_and_no_other() {
    use control_api::handler::principal_matches;
    use control_api::lambda::operators_setting;
    let acct = "123456789012";
    let any = |setting: &str, arn: &str| {
        operators_setting(setting)
            .unwrap()
            .iter()
            .any(|p| principal_matches(p, arn))
    };

    // an IAM role: the sessions of that role
    let role = format!("arn:aws:sts::{acct}:assumed-role/StageOperator/*");
    assert!(any(
        &role,
        &format!("arn:aws:sts::{acct}:assumed-role/StageOperator/alice")
    ));
    assert!(!any(
        &role,
        &format!("arn:aws:sts::{acct}:assumed-role/StageOperator2/alice")
    ));
    assert!(!any(
        &role,
        &format!("arn:aws:sts::{acct}:assumed-role/Other/StageOperator")
    ));
    assert!(!any(
        &role,
        &format!("arn:aws:iam::{acct}:user/StageOperator")
    ));

    // an IAM Identity Center role: the role is AWSReservedSSO_<permission set>_<hash>, the session is the user
    let sso = format!(
        "arn:aws:sts::{acct}:assumed-role/AWSReservedSSO_AdministratorAccess_0123456789abcdef/*"
    );
    assert!(any(
        &sso,
        &format!(
            "arn:aws:sts::{acct}:assumed-role/AWSReservedSSO_AdministratorAccess_0123456789abcdef/alice@example.com"
        )
    ));
    assert!(!any(
        &sso,
        &format!(
            "arn:aws:sts::{acct}:assumed-role/AWSReservedSSO_ReadOnly_0123456789abcdef/alice@example.com"
        )
    ));

    // an IAM user: the user with and without a path, two patterns in one comma separated setting
    let user = format!("arn:aws:iam::{acct}:user/alice,arn:aws:iam::{acct}:user/*/alice");
    assert_eq!(operators_setting(&user).unwrap().len(), 2);
    for ok in ["user/alice", "user/team/alice", "user/a/b/alice"] {
        assert!(any(&user, &format!("arn:aws:iam::{acct}:{ok}")), "{ok}");
    }
    for no in ["user/alicia", "user/team/alicia", "user/malice", "root"] {
        assert!(!any(&user, &format!("arn:aws:iam::{acct}:{no}")), "{no}");
    }
    assert!(!any(
        &user,
        &format!("arn:aws:sts::{acct}:assumed-role/alice/session")
    ));
    assert!(!any(&user, "arn:aws:iam::444455556666:user/alice"));
}

#[test]
fn every_diagnostic_is_wrapped_with_the_request_path() {
    use control_api::lambda::log_lines;
    let req = lambda_http::request::from_str(EVENT).unwrap();
    let v1 = to_v1(&req);
    let lines = log_lines(&v1, 201, &["store: ThrottlingException".into()]);
    assert_eq!(lines.len(), 2);
    let wrapped: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(wrapped["diagnostic"], "store: ThrottlingException");
    assert_eq!(
        wrapped["path"],
        "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events"
    );
    // A line that looks like an embedded metric is wrapped too: the handler writes none.
    let lines = log_lines(&v1, 200, &["{\"_aws\": {}}".to_string()]);
    assert!(lines[1].contains("diagnostic"));
}

#[test]
fn b804_admit_the_refusal_of_a_start_carries_retry_after_and_no_other_answer_does() {
    let refused = to_response(V1Response {
        status: 409,
        body: "{}".into(),
        retry_after_s: Some(120),
    })
    .unwrap();
    assert_eq!(refused.status(), 409);
    assert_eq!(refused.headers()["retry-after"], "120");
    // the headers every answer carries are still there
    assert_eq!(refused.headers()["content-type"], "application/json");
    assert_eq!(refused.headers()["cache-control"], "no-store");
    assert_eq!(
        refused.headers()["x-hamh-backend-version"],
        root_version().as_str()
    );
    for status in [200, 201, 409, 503] {
        let plain = to_response(V1Response {
            status,
            body: "{}".into(),
            retry_after_s: None,
        })
        .unwrap();
        assert!(
            !plain.headers().contains_key("retry-after"),
            "status {status}"
        );
    }
}
