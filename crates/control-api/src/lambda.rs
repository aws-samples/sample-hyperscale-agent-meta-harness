//! API Gateway REST (v1) proxy event <-> handler request and response.

use crate::handler::{Blobs, Deps, Execution, Store, V1Request, V1Response, handle_logged};
use crate::version::{LEGACY_VERSION_HEADER, LabelStore, ReleaseLabel, VERSION, VERSION_HEADER};
use lambda_http::request::RequestContext;
use lambda_http::{Body, Request, RequestExt, Response};
use std::collections::BTreeMap;

/// The path from `/v1` on: a stage or base-path prefix is dropped.
fn v1_path(path: &str) -> String {
    if path == "/v1" || path.starts_with("/v1/") {
        return path.to_string();
    }
    match path.find("/v1/") {
        Some(i) => path[i..].to_string(),
        None if path.ends_with("/v1") => "/v1".into(),
        None => path.to_string(),
    }
}

/// The IAM caller (`requestContext.identity.userArn`), its source IP (`identity.sourceIp`), method,
/// path, headers (lowercase names), query (first value per name) and body of the event.
pub fn to_v1(req: &Request) -> V1Request {
    let (caller_arn, source_ip) = match req.request_context_ref() {
        Some(RequestContext::ApiGatewayV1(ctx)) => (
            ctx.identity.user_arn.clone(),
            ctx.identity.source_ip.clone(),
        ),
        _ => (None, None),
    };
    let body = match req.body() {
        Body::Text(t) => t.clone(),
        Body::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    };
    let mut headers = BTreeMap::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers
                .entry(name.as_str().to_ascii_lowercase())
                .or_insert_with(|| v.to_string());
        }
    }
    let mut query = BTreeMap::new();
    if let Some(q) = req.query_string_parameters_ref() {
        for (name, value) in q.iter() {
            query
                .entry(name.to_string())
                .or_insert_with(|| value.to_string());
        }
    }
    V1Request {
        method: req.method().as_str().to_string(),
        path: v1_path(req.uri().path()),
        caller_arn,
        source_ip,
        headers,
        query,
        body,
    }
}

/// One request of the Lambda, from the API Gateway event to the answer: the handler, the access line and one line per diagnostic, and the answer with the version the stage
/// reports now. `main.rs`
/// builds the dependencies and calls this and nothing else, so the tests drive the path the Lambda runs, with a stand-in store and a scripted parameter store.
pub async fn serve<S: Store, B: Blobs, E: Execution, L: LabelStore>(
    req: &Request,
    deps: &Deps<S, B, E>,
    release: &ReleaseLabel<L>,
) -> Result<Response<Body>, lambda_http::http::Error> {
    let v1 = to_v1(req);
    let line_req = v1.clone();
    let (out, diagnostics) = handle_logged(v1, deps).await;
    for line in log_lines(&line_req, out.status, &diagnostics) {
        println!("{line}");
    }
    let version = release.current().await;
    to_response_labelled(out, &version)
}

/// The answer for a handler's `V1Response` with the compiled-in version. See [`to_response_labelled`], which the Lambda uses.
pub fn to_response(out: V1Response) -> Result<Response<Body>, lambda_http::http::Error> {
    to_response_labelled(out, VERSION)
}

/// The answer for a handler's `V1Response`. This is the one place that builds a Lambda response, so every answer (2xx, 4xx and 5xx) carries the
/// backend's version in `x-hamh-backend-version` and, for the compatibility window, in its earlier name ([`LEGACY_VERSION_HEADER`]). `version` is the
/// version to report: `ReleaseLabel::current`, which is the compiled version unless the stage holds a newer release label for the source tree it runs.
/// An answer with `retry_after_s` carries `Retry-After` too.
pub fn to_response_labelled(
    out: V1Response,
    version: &str,
) -> Result<Response<Body>, lambda_http::http::Error> {
    let mut answer = Response::builder()
        .status(out.status)
        .header("content-type", "application/json")
        // A body may hold a presigned URL, a bearer capability: no cache keeps an answer.
        .header("cache-control", "no-store")
        .header(VERSION_HEADER, version)
        .header(LEGACY_VERSION_HEADER, version);
    // The refusal of a start while the stage is being updated names a pause before the next try.
    if let Some(seconds) = out.retry_after_s {
        answer = answer.header("retry-after", seconds.to_string());
    }
    answer.body(Body::Text(out.body))
}

/// One JSON access line per request: caller, source IP (`ip`, null when the event has none),
/// method, path and status; never the body or query, so no prompt or rule text reaches the logs.
pub fn access_line(req: &V1Request, status: u16) -> String {
    serde_json::json!({
        "caller": req.caller_arn,
        "ip": req.source_ip,
        "method": req.method,
        "path": req.path,
        "status": status,
    })
    .to_string()
}

/// The access line, then one JSON line per handler diagnostic, each wrapped with the request path.
pub fn log_lines(req: &V1Request, status: u16, diagnostics: &[String]) -> Vec<String> {
    let mut out = vec![access_line(req, status)];
    out.extend(
        diagnostics
            .iter()
            .map(|d| serde_json::json!({ "path": req.path, "diagnostic": d }).to_string()),
    );
    out
}

/// The `OPERATOR_PRINCIPALS` setting: comma-separated IAM ARN patterns (`*` wildcards), at least
/// one, each an `arn:aws:` ARN.
pub fn operators_setting(value: &str) -> Result<Vec<String>, String> {
    let v: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if v.is_empty() {
        return Err("OPERATOR_PRINCIPALS names no principal".into());
    }
    if let Some(bad) = v.iter().find(|p| !p.starts_with("arn:aws:")) {
        return Err(format!(
            "OPERATOR_PRINCIPALS: {bad:?} is not an IAM ARN pattern"
        ));
    }
    Ok(v)
}
