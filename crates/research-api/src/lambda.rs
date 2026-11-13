//! The Lambda side: the API Gateway REST (v1) proxy event as a [`Request`], a [`Response`] as the proxy answer, the function environment
//! and the one log line per request. The bins are only the Lambda runtime around these and the wiring of the ports.

use crate::agentcore::{DEFAULT_TOOL, GatewayTarget};
use crate::handler::{LogLine, Request, Response};
use crate::metrics::valid_stage;
use crate::ports::StageKind;
use crate::providers::StageChoice;
use harness_protocol::research::ResearchProvider;
use lambda_http::request::RequestContext;
use lambda_http::{Body, Request as HttpRequest, RequestExt, Response as HttpResponse};

/// The account number of a function ARN (`arn:aws:lambda:<region>:<account>:function:<name>`), or `None` for anything else.
fn account_of_function_arn(arn: &str) -> Option<&str> {
    let parts: Vec<&str> = arn.split(':').collect();
    (parts.len() >= 7 && parts[0] == "arn" && parts[2] == "lambda")
        .then(|| parts[4])
        .filter(|a| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit()))
}

fn present(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.is_empty())
}

/// The method, route, IAM identity and body of an event. The path is cut at `/internal/` so a stage prefix does not matter.
///
/// The identity of an event is a caller only when the event looks like one API Gateway made for a worker session: the context is whole
/// (API id, stage, request id, the source address, a temporary access key `ASIA...`), its account is the account of this function (taken from the
/// Lambda context, which the invoker cannot write), and the invocation carries nothing an SDK invoke adds (a client context or a Cognito identity).
/// Anything else has no caller, so the five routes answer `disabled`; the operator's key-state question needs no caller. This is validation and not
/// authentication: a principal that may invoke the function can copy a real event, and who may invoke it is settled by IAM (SECURITY.md).
pub fn to_request(req: &HttpRequest) -> Request {
    let function_account = req
        .lambda_context_ref()
        .and_then(|c| account_of_function_arn(&c.invoked_function_arn));
    let sdk_invoke = req
        .lambda_context_ref()
        .is_some_and(|c| c.client_context.is_some() || c.identity.is_some());
    let (caller_arn, account_id, access_key, request_id) = match req.request_context_ref() {
        Some(RequestContext::ApiGatewayV1(ctx)) => {
            let whole = !sdk_invoke
                && function_account.is_some()
                && ctx.account_id.as_deref() == function_account
                && present(&ctx.apiid)
                && present(&ctx.stage)
                && present(&ctx.request_id)
                && present(&ctx.identity.source_ip)
                && ctx
                    .identity
                    .access_key
                    .as_deref()
                    .is_some_and(|k| k.starts_with("ASIA"));
            (
                ctx.identity.user_arn.clone().filter(|_| whole),
                ctx.account_id.clone(),
                ctx.identity.access_key.clone(),
                ctx.request_id.clone().unwrap_or_default(),
            )
        }
        _ => (None, None, None, String::new()),
    };
    let body = match req.body() {
        Body::Text(t) => t.clone(),
        Body::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    };
    let path = req.uri().path();
    let path = path.find("/internal/").map_or(path, |i| &path[i..]);
    Request {
        method: req.method().as_str().to_string(),
        path: path.to_string(),
        caller_arn,
        account_id,
        access_key,
        request_id,
        body,
    }
}

pub fn to_response(out: Response) -> Result<HttpResponse<Body>, lambda_http::http::Error> {
    HttpResponse::builder()
        .status(out.status)
        .header("content-type", "application/json")
        .body(Body::Text(out.body))
}

/// One JSON line per request: the operation, the run, the status and the word, the time and the request id. Never the body, a header, a
/// key or fetched text.
pub fn log_line(l: &LogLine) -> String {
    serde_json::json!({
        "op": l.op, "run": l.run, "status": l.status, "code": l.code, "ms": l.ms, "requestId": l.request_id,
    })
    .to_string()
}

/// The stage the metrics name when `HAMH_STAGE` is not set.
const DEFAULT_STAGE: &str = "direct";

/// The gateway of the `agentcore` provider, read and checked at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentcoreEnv {
    /// `AGENTCORE_GATEWAY_URL`: `https://<id>.gateway.bedrock-agentcore.<region>.amazonaws.com/mcp`.
    pub gateway_url: String,
    /// The Region of that address, which is also the Region of the signature.
    pub region: String,
    /// `AGENTCORE_SEARCH_TOOL`, `web-search-tool___WebSearch` unless set.
    pub tool: String,
}

/// The environment of `research-api`, read once at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiEnv {
    pub table: String,
    pub bucket: String,
    pub secret_id: String,
    pub job_function: String,
    pub provider: ResearchProvider,
    pub stage_kind: StageKind,
    pub stage_rate_per_min: u32,
    pub worker_role_name: String,
    /// `HAMH_STAGE`: the value of the `Stage` dimension of the Tavily key metrics (`direct` when the variable is not set or empty).
    pub stage: String,
    /// `Some` for the `agentcore` provider and for `auto`; both need the gateway.
    pub agentcore: Option<AgentcoreEnv>,
    /// `RESEARCH_PROVIDER` was `auto`: the provider is chosen for each call. `provider` then holds `agentcore`, the provider that serves while no
    /// key is stored; read the stage with [`ApiEnv::choice`], which does not need that reading.
    pub auto: bool,
}

impl ApiEnv {
    /// How the stage names its provider: `auto`, or the one it pins.
    pub fn choice(&self) -> StageChoice {
        if self.auto {
            StageChoice::Auto
        } else {
            StageChoice::Pinned(self.provider)
        }
    }
}

/// The environment of `research-job`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEnv {
    pub table: String,
    pub bucket: String,
}

fn need(get: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    get(name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("missing environment variable {name}"))
}

/// The gateway settings of an `agentcore` stage: the address (checked, and in the Region the function runs in when `AWS_REGION` is set) and the
/// tool name. No secret is read for this provider. An error names the variable and never its value.
fn agentcore_env(get: &impl Fn(&str) -> Option<String>) -> Result<AgentcoreEnv, String> {
    let url = need(get, "AGENTCORE_GATEWAY_URL")?;
    let target = GatewayTarget::parse(&url).map_err(|e| format!("AGENTCORE_GATEWAY_URL: {e}"))?;
    if let Some(here) = get("AWS_REGION").filter(|r| !r.is_empty())
        && here != target.region
    {
        return Err(
            "AGENTCORE_GATEWAY_URL is in another Region than the one this function runs in".into(),
        );
    }
    let tool = match get("AGENTCORE_SEARCH_TOOL").filter(|t| !t.is_empty()) {
        None => DEFAULT_TOOL.to_string(),
        Some(t) => {
            let plain = t.len() <= 128
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !plain {
                return Err(
                    "AGENTCORE_SEARCH_TOOL is a tool name of letters, digits, - and _".into(),
                );
            }
            t
        }
    };
    Ok(AgentcoreEnv {
        gateway_url: url,
        region: target.region,
        tool,
    })
}

/// Reads the variables through `get`. A function that is misconfigured fails at start with the variable's name and makes no AWS call. The fixture
/// provider is refused unless the stage is a test stage: the lock is checked here, where the provider is chosen. `auto` needs the gateway address too and
/// still reads the key secret named by `TAVILY_SECRET_ID`; it is allowed on every stage.
pub fn api_env(get: impl Fn(&str) -> Option<String>) -> Result<ApiEnv, String> {
    let kind_text = need(&get, "HAMH_STAGE_KIND")?;
    let stage_kind = StageKind::parse(&kind_text)
        .ok_or_else(|| "HAMH_STAGE_KIND is official or test".to_string())?;
    let provider_text = need(&get, "RESEARCH_PROVIDER")?;
    let auto = provider_text == "auto";
    let provider = match provider_text.as_str() {
        "auto" | "agentcore" => ResearchProvider::Agentcore,
        "tavily" => ResearchProvider::Tavily,
        "tavily-keyless" => ResearchProvider::TavilyKeyless,
        "fixture" => ResearchProvider::Fixture,
        _ => {
            return Err(
                "RESEARCH_PROVIDER is auto, tavily, tavily-keyless, agentcore or fixture".into(),
            );
        }
    };
    if provider == ResearchProvider::Fixture && stage_kind != StageKind::Test {
        return Err("RESEARCH_PROVIDER fixture is refused on an official stage".into());
    }
    let rate = need(&get, "RESEARCH_STAGE_RATE_PER_MIN")?
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "RESEARCH_STAGE_RATE_PER_MIN is a positive number".to_string())?;
    // The name of the stage for the metrics' dimension: the stage name the other Lambdas of the stack read from the same variable. Absent or empty means
    // `direct`, the one stage there is, so a function deployed before the variable existed starts as it did. A name CloudWatch would refuse stops the
    // function at start; the error names the variable and never its value.
    let stage = match get("HAMH_STAGE").filter(|s| !s.is_empty()) {
        None => DEFAULT_STAGE.to_string(),
        Some(s) if valid_stage(&s) => s,
        Some(_) => {
            return Err("HAMH_STAGE is 1 to 32 lower-case letters, digits or hyphens".into());
        }
    };
    // `auto` reads as `agentcore` above, so it takes the gateway settings through the same line.
    let agentcore = if provider == ResearchProvider::Agentcore {
        Some(agentcore_env(&get)?)
    } else {
        None
    };
    Ok(ApiEnv {
        table: need(&get, "RESEARCH_TABLE_NAME")?,
        bucket: need(&get, "HARNESS_BUCKET")?,
        secret_id: need(&get, "TAVILY_SECRET_ID")?,
        job_function: need(&get, "RESEARCH_JOB_FUNCTION")?,
        provider,
        stage_kind,
        stage_rate_per_min: rate,
        worker_role_name: need(&get, "WORKER_SESSION_ROLE_NAME")?,
        stage,
        agentcore,
        auto,
    })
}

pub fn job_env(get: impl Fn(&str) -> Option<String>) -> Result<JobEnv, String> {
    Ok(JobEnv {
        table: need(&get, "RESEARCH_TABLE_NAME")?,
        bucket: need(&get, "HARNESS_BUCKET")?,
    })
}

/// One JSON line per `research-job` run: the run, the job and how it ended (the word of a failure, never its message). No key, no body.
pub fn job_log_line(event: &crate::ports::JobEvent, end: &crate::jobs::JobEnd) -> String {
    let (outcome, detail) = match end {
        crate::jobs::JobEnd::Succeeded(item) => ("succeeded", item.id.clone()),
        crate::jobs::JobEnd::Failed(e) => ("failed", e.code.as_str().to_string()),
        crate::jobs::JobEnd::Skipped(why) => ("skipped", (*why).to_string()),
    };
    serde_json::json!({
        "function": "research-job", "run": event.run, "jobId": event.job_id, "outcome": outcome, "detail": detail,
    })
    .to_string()
}
