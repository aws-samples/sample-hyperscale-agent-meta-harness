//! API Gateway REST (v1) proxy event <-> handler request and response.

use crate::handler::{InternalRequest, InternalResponse};
use lambda_http::request::RequestContext;
use lambda_http::{Body, Request, RequestExt, Response};

/// The IAM caller (`requestContext.identity.userArn`), method, path and body of the event.
pub fn to_internal(req: &Request) -> InternalRequest {
    let caller_arn = match req.request_context_ref() {
        Some(RequestContext::ApiGatewayV1(ctx)) => ctx.identity.user_arn.clone(),
        _ => None,
    };
    let body = match req.body() {
        Body::Text(t) => t.clone(),
        Body::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    };
    // The completion key travels in a header; it is read here and printed nowhere.
    let completion_key = req
        .headers()
        .get(harness_protocol::store::COMPLETION_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    InternalRequest {
        method: req.method().as_str().to_string(),
        path: req.uri().path().to_string(),
        caller_arn,
        completion_key,
        body,
    }
}

pub fn to_response(out: InternalResponse) -> Result<Response<Body>, lambda_http::http::Error> {
    Response::builder()
        .status(out.status)
        .header("content-type", "application/json")
        .body(Body::Text(out.body))
}

/// One JSON access line per request: caller, method, path and status; never the body, so no nonce
/// or credential reaches the logs (S3 canary check).
pub fn access_line(req: &InternalRequest, status: u16) -> String {
    serde_json::json!({
        "caller": req.caller_arn,
        "method": req.method,
        "path": req.path,
        "status": status,
    })
    .to_string()
}

/// The access line, then one JSON line per handler diagnostic (for example a control read that
/// failed); the Lambda prints them in order.
pub fn log_lines(req: &InternalRequest, status: u16, diagnostics: &[String]) -> Vec<String> {
    let mut out = vec![access_line(req, status)];
    out.extend(
        diagnostics
            .iter()
            .map(|d| serde_json::json!({ "path": req.path, "diagnostic": d }).to_string()),
    );
    out
}
