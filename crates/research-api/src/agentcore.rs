//! The `agentcore` search provider: web search from AWS's own index through an AgentCore Gateway with the
//! Web Search Tool connector, with no third-party key.
//!
//! The call is MCP over JSON-RPC 2.0, one POST per message to the gateway's `/mcp` address, signed with SigV4 (service `bedrock-agentcore`,
//! the Region of the gateway) by the function's own role. The role needs `bedrock-agentcore:InvokeGateway` on the gateway. The gateway speaks
//! only MCP protocol version `2025-03-26`, needs no session id and no `initialized` notification, and answers
//! `tools/call` of `<target>___WebSearch` with a JSON string `{id, results:[{title, url, publishedDate, text}]}` in `content[0].text`. The
//! provider does one `initialize` per cold start (the fact is cached, not a session id), then one `tools/call` per search.
//!
//! The query is at most 200 characters (the gateway's cap, kept here as a limit of this provider and not of the route), `maxResults` is 1 to
//! 25, a published-date window and a domain list are the gateway's `filters`. Failures map to the route's closed error words: a throttle (HTTP
//! 429, or a tool error that says so; the gateway sends it as HTTP 200 with `isError`) is `upstream_rate_limited`; a refusal (403, JSON-RPC
//! -32002, an `AccessDenied` text) is an `upstream_error` that names the missing IAM action and nothing of the gateway; every other failure
//! is an `upstream_error` whose text is a fixed sentence or, for a tool error, an excerpt of at most 200 characters without addresses.
//!
//! The service terms bar extracting, storing or reproducing results in bulk. This module only reads what the gateway answers; what the route
//! keeps of it is decided in `crate::ops` (titles and URLs only). The module prints nothing: a query, an excerpt or a credential must not reach
//! a log line (a test pins that).

use crate::audit::rfc3339;
use crate::http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use crate::ports::{Clock, ExtractOutcome, Fail, Provider, SearchOutcome, SystemClock};
use crate::scan::{clean_text, excerpt};
use aws_credential_types::Credentials;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use harness_protocol::research::{
    ResearchErrorCode, ResearchProvider, SearchRequest, SearchResult, TimeRange,
};
use serde_json::{Value, json};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;

/// The tool of the target the stack creates (`web-search-tool`, three underscores, `WebSearch`).
pub const DEFAULT_TOOL: &str = "web-search-tool___WebSearch";
/// The gateway's cap on a query, in characters.
pub const MAX_QUERY_CHARS: usize = 200;
/// The only MCP protocol version the gateway speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
/// The SigV4 service name of the gateway.
pub const SERVICE: &str = "bedrock-agentcore";

/// Results when the request names no count (the route's default).
const DEFAULT_RESULTS: u8 = 5;
/// The gateway takes 1 to 25.
const MAX_GATEWAY_RESULTS: u8 = 25;
/// An excerpt is cut to 1 KiB, like every snippet of the route.
const SNIPPET_CHARS: usize = 1024;
/// The largest answer that is read: 25 results of 2,400 characters is about 70 KB.
const RESPONSE_CAP_BYTES: usize = 512 * 1024;
/// The longest part of a tool error that a message carries.
const ERROR_EXCERPT_CHARS: usize = 200;
/// The longest `retry-after` that is passed on.
const MAX_RETRY_AFTER_S: u64 = 3600;
/// The hint for a throttle that arrives as a tool error, which carries no `retry-after`.
const THROTTLE_RETRY_S: u32 = 60;
const MS_PER_DAY: u64 = 86_400_000;

// ---- the gateway address -------------------------------------------------------------------------------------------------------------

/// The address of a gateway is not `https://<id>.gateway.bedrock-agentcore.<region>.amazonaws.com/mcp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatewayUrlError;

impl fmt::Display for GatewayUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the gateway address is https://<id>.gateway.bedrock-agentcore.<region>.amazonaws.com/mcp",
        )
    }
}

impl std::error::Error for GatewayUrlError {}

/// A gateway address that passed the rules: https, one host of the service, no port, no user information, no query, no fragment, and the
/// path `/mcp`. The Region is read from the host and is the Region of the signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayTarget {
    pub url: String,
    pub host: String,
    pub region: String,
}

fn is_gateway_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 100
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_region(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    let word = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase());
    parts.len() >= 3
        && parts[0].len() == 2
        && word(parts[0])
        && parts[1..parts.len() - 1].iter().all(|p| word(p))
        && parts
            .last()
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

impl GatewayTarget {
    pub fn parse(url: &str) -> Result<Self, GatewayUrlError> {
        let rest = url.strip_prefix("https://").ok_or(GatewayUrlError)?;
        let (host, path) = rest.split_once('/').ok_or(GatewayUrlError)?;
        if path != "mcp" {
            return Err(GatewayUrlError);
        }
        let (id, tail) = host
            .split_once(".gateway.bedrock-agentcore.")
            .ok_or(GatewayUrlError)?;
        let region = tail.strip_suffix(".amazonaws.com").ok_or(GatewayUrlError)?;
        if !is_gateway_id(id) || !is_region(region) {
            return Err(GatewayUrlError);
        }
        Ok(Self {
            url: url.to_string(),
            host: host.to_string(),
            region: region.to_string(),
        })
    }
}

// ---- the signature -------------------------------------------------------------------------------------------------------------------

/// The headers SigV4 adds to a POST of `body` to `url`: `authorization`, `x-amz-date` and, with a session, `x-amz-security-token`. The
/// request's own `headers` and the host are signed; the caller sends exactly those headers, that body and these added headers.
pub fn sign_headers(
    url: &str,
    region: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    credentials: &Credentials,
    time: SystemTime,
) -> Result<Vec<(String, String)>, Fail> {
    let unsigned = || Fail::internal("the gateway request could not be signed");
    let identity = credentials.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SERVICE)
        .time(time)
        .settings(SigningSettings::default())
        .build()
        .map_err(|_| unsigned())?
        .into();
    let request = SignableRequest::new(
        "POST",
        url,
        headers.iter().copied(),
        SignableBody::Bytes(body),
    )
    .map_err(|_| unsigned())?;
    let (instructions, _signature) = sign(request, &params).map_err(|_| unsigned())?.into_parts();
    let mut carrier = http::Request::new(());
    instructions.apply_to_request_http1x(&mut carrier);
    Ok(carrier
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

// ---- the provider --------------------------------------------------------------------------------------------------------------------

/// The provider over one [`HttpClient`] (its timeouts are the caller's). The handshake runs once per instance: concurrent first searches
/// share it, and a failed one is tried again by the next search.
pub struct AgentcoreProvider<C = SystemClock> {
    http: HttpClient,
    target: GatewayTarget,
    tool: String,
    credentials: SharedCredentialsProvider,
    clock: C,
    ready: OnceCell<()>,
    next_id: AtomicU64,
}

impl<C> fmt::Debug for AgentcoreProvider<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentcoreProvider")
            .field("region", &self.target.region)
            .field("tool", &self.tool)
            .finish_non_exhaustive()
    }
}

impl AgentcoreProvider<SystemClock> {
    pub fn new(
        http: HttpClient,
        target: GatewayTarget,
        tool: impl Into<String>,
        credentials: SharedCredentialsProvider,
    ) -> Self {
        Self::with_clock(http, target, tool, credentials, SystemClock)
    }
}

impl<C> AgentcoreProvider<C> {
    /// With the clock that dates the signature and the published-date window (a test fixes it).
    pub fn with_clock(
        http: HttpClient,
        target: GatewayTarget,
        tool: impl Into<String>,
        credentials: SharedCredentialsProvider,
        clock: C,
    ) -> Self {
        Self {
            http,
            target,
            tool: tool.into(),
            credentials,
            clock,
            ready: OnceCell::new(),
            next_id: AtomicU64::new(1),
        }
    }
}

fn unreadable() -> Fail {
    Fail::upstream("the gateway's answer could not be read")
}

fn denied() -> Fail {
    Fail::upstream(
        "the gateway refused the research-api role; check that the role may call bedrock-agentcore:InvokeGateway on the gateway",
    )
}

fn map_http(e: HttpError) -> Fail {
    match e {
        HttpError::Timeout => Fail::upstream("the gateway did not answer in time"),
        HttpError::TooLarge => Fail::upstream("the gateway's answer was too large"),
        HttpError::InvalidRequest(_) => Fail::internal("the gateway request could not be built"),
        other => Fail::upstream(format!(
            "the gateway could not be reached ({})",
            other.class()
        )),
    }
}

fn rate_limited(response: &HttpResponse) -> Fail {
    let fail = Fail::new(
        ResearchErrorCode::UpstreamRateLimited,
        "the gateway is limiting the rate of searches",
    );
    match response
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(seconds) => {
            fail.with_retry_after(u32::try_from(seconds.min(MAX_RETRY_AFTER_S)).unwrap_or(3600))
        }
        None => fail,
    }
}

/// Whether `lower` (lower case) holds `marker` not followed by another digit (`error code 429` is a throttle, `error code 4290` is not).
fn has_marker(lower: &str, marker: &str) -> bool {
    lower.match_indices(marker).any(|(i, _)| {
        !lower[i + marker.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

/// A tool error that says the gateway throttled. It is recognised by the phrases the service uses, never by a bare number, and never in a
/// validation message (which echoes what the caller sent).
fn is_throttle(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("validation error") {
        return false;
    }
    has_marker(&lower, "error code 429")
        || has_marker(&lower, "status code 429")
        || lower.contains("throttlingexception")
        || lower.contains("too many requests")
        || lower.contains("rate exceeded")
}

/// A text from the gateway that says the caller is not allowed.
fn is_denied(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("accessdenied")
        || lower.contains("access denied")
        || lower.contains("not authorized")
}

/// A token of a tool error without an address, an ARN or an account number.
fn redact(token: &str) -> &str {
    let bare = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let lower = bare.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("arn:")
        || (bare.len() >= 12 && bare.chars().all(|c| c.is_ascii_digit()))
    {
        "<redacted>"
    } else {
        token
    }
}

/// One line of at most 200 characters from a tool error: controls removed, addresses and ARNs replaced.
fn tool_error_excerpt(text: &str) -> String {
    let cleaned = clean_text(text);
    let one_line = cleaned
        .split_whitespace()
        .map(redact)
        .collect::<Vec<_>>()
        .join(" ");
    excerpt(&one_line, ERROR_EXCERPT_CHARS)
}

fn tool_error(text: &str) -> Fail {
    if is_denied(text) {
        return denied();
    }
    if is_throttle(text) {
        return Fail::new(
            ResearchErrorCode::UpstreamRateLimited,
            "the gateway is limiting the rate of searches",
        )
        .with_retry_after(THROTTLE_RETRY_S);
    }
    Fail::upstream(format!(
        "the gateway's search tool failed: {}",
        tool_error_excerpt(text)
    ))
}

/// A JSON-RPC `error` member: the code, never the message (it can hold ARNs).
fn rpc_error(error: &Value) -> Fail {
    let code = error["code"].as_i64();
    let message = error["message"].as_str().unwrap_or("");
    if code == Some(-32002) || is_denied(message) {
        return denied();
    }
    if code == Some(-32600) {
        return Fail::upstream(format!(
            "the gateway refused the request as an unsupported MCP protocol version; this provider speaks {MCP_PROTOCOL_VERSION}"
        ));
    }
    match code {
        Some(c) => Fail::upstream(format!("the gateway answered a JSON-RPC error ({c})")),
        None => unreadable(),
    }
}

/// The JSON message of an answer: the body, or the last `data:` event of an event stream.
fn read_message(response: &HttpResponse) -> Result<Value, Fail> {
    let streamed = response
        .header("content-type")
        .is_some_and(|t| t.to_ascii_lowercase().contains("text/event-stream"));
    if !streamed {
        return serde_json::from_slice(&response.body).map_err(|_| unreadable());
    }
    String::from_utf8_lossy(&response.body)
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .next_back()
        .ok_or_else(unreadable)
}

impl<C: Clock> AgentcoreProvider<C> {
    /// One JSON-RPC request: signed, sent, checked. Returns the `result` object.
    async fn call(&self, method: &str, params: Value) -> Result<Value, Fail> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = serde_json::to_vec(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .map_err(|_| Fail::internal("the gateway request could not be encoded"))?;
        let credentials =
            self.credentials.provide_credentials().await.map_err(|_| {
                Fail::upstream("the function's AWS credentials could not be loaded")
            })?;
        let own = [
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
        ];
        let signed = sign_headers(
            &self.target.url,
            &self.target.region,
            &own,
            &body,
            &credentials,
            UNIX_EPOCH + Duration::from_millis(self.clock.now_ms()),
        )?;
        let mut headers: Vec<(String, String)> = own
            .iter()
            .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
            .collect();
        headers.extend(signed);
        let response = self
            .http
            .send_limited(
                HttpRequest {
                    method: http::Method::POST,
                    url: self.target.url.clone(),
                    headers,
                    body,
                },
                RESPONSE_CAP_BYTES,
                self.http.options().total_timeout,
            )
            .await
            .map_err(map_http)?;
        match response.status {
            200..=299 => {}
            400 => {
                return Err(match read_message(&response) {
                    Ok(v) if v["error"]["code"].as_i64() == Some(-32600) => rpc_error(&v["error"]),
                    _ => Fail::upstream("the gateway answered 400"),
                });
            }
            401 | 403 => return Err(denied()),
            429 => return Err(rate_limited(&response)),
            status => return Err(Fail::upstream(format!("the gateway answered {status}"))),
        }
        let message = read_message(&response)?;
        if message["id"] != json!(id) {
            return Err(unreadable());
        }
        if message.get("error").is_some_and(|e| !e.is_null()) {
            return Err(rpc_error(&message["error"]));
        }
        match message.get("result") {
            Some(result) if result.is_object() => Ok(result.clone()),
            _ => Err(unreadable()),
        }
    }

    /// The handshake, once. A failure leaves the cell empty, so the next search tries again.
    async fn ensure_ready(&self) -> Result<(), Fail> {
        self.ready
            .get_or_try_init(|| async {
                let result = self
                    .call(
                        "initialize",
                        json!({"protocolVersion": MCP_PROTOCOL_VERSION, "capabilities": {},
                               "clientInfo": {"name": "ham-research", "version": "1"}}),
                    )
                    .await?;
                if result["protocolVersion"] == MCP_PROTOCOL_VERSION {
                    Ok(())
                } else {
                    Err(Fail::upstream(format!(
                        "the gateway does not speak MCP protocol version {MCP_PROTOCOL_VERSION}"
                    )))
                }
            })
            .await
            .map(|_| ())
    }

    /// The gateway's `filters` for a request, or `None` when it names none.
    fn filters(&self, req: &SearchRequest) -> Option<Value> {
        let mut filters = serde_json::Map::new();
        if !req.include_domains.is_empty() {
            filters.insert(
                "domainFilter".into(),
                json!({"include": req.include_domains}),
            );
        }
        if let Some(range) = req.time_range {
            let days: u64 = match range {
                TimeRange::Day => 1,
                TimeRange::Week => 7,
                TimeRange::Month => 30,
                TimeRange::Year => 365,
            };
            let from = self.clock.now_ms().saturating_sub(days * MS_PER_DAY);
            filters.insert("publishedDateFilter".into(), json!({"from": rfc3339(from)}));
        }
        (!filters.is_empty()).then_some(Value::Object(filters))
    }
}

/// The results of the inner answer, in order, at most `wanted`. A result without a URL is skipped.
fn read_results(answer: &Value, wanted: usize) -> Vec<SearchResult> {
    answer["results"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    Some(SearchResult {
                        title: r["title"].as_str().unwrap_or("").to_string(),
                        url: r["url"].as_str()?.to_string(),
                        snippet: excerpt(r["text"].as_str().unwrap_or(""), SNIPPET_CHARS),
                        published: r["publishedDate"]
                            .as_str()
                            .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("unknown"))
                            .map(str::to_string),
                        score: None,
                    })
                })
                .take(wanted)
                .collect()
        })
        .unwrap_or_default()
}

impl<C: Clock> Provider for AgentcoreProvider<C> {
    fn id(&self) -> ResearchProvider {
        ResearchProvider::Agentcore
    }

    fn max_query_chars(&self) -> Option<usize> {
        Some(MAX_QUERY_CHARS)
    }

    fn can_extract(&self) -> bool {
        false
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchOutcome, Fail> {
        if req.query.chars().count() > MAX_QUERY_CHARS {
            return Err(Fail::new(
                ResearchErrorCode::Usage,
                format!(
                    "query is longer than {MAX_QUERY_CHARS} characters for this stage's search provider"
                ),
            ));
        }
        self.ensure_ready().await?;
        let wanted = req
            .max_results
            .unwrap_or(DEFAULT_RESULTS)
            .clamp(1, MAX_GATEWAY_RESULTS);
        let mut arguments = json!({"query": req.query, "maxResults": wanted});
        if let Some(filters) = self.filters(req) {
            arguments["filters"] = filters;
        }
        let result = self
            .call(
                "tools/call",
                json!({"name": self.tool, "arguments": arguments}),
            )
            .await?;
        let text = result["content"]
            .get(0)
            .and_then(|c| c["text"].as_str())
            .ok_or_else(unreadable)?;
        if result["isError"].as_bool() == Some(true) {
            return Err(tool_error(text));
        }
        let answer: Value = serde_json::from_str(text).map_err(|_| unreadable())?;
        if !answer.is_object() {
            return Err(unreadable());
        }
        Ok(SearchOutcome {
            results: read_results(&answer, usize::from(wanted)),
            credits: None,
            upstream_request_id: answer["id"].as_str().map(str::to_string),
        })
    }

    async fn extract(
        &self,
        _urls: &[String],
        _query: Option<&str>,
    ) -> Result<ExtractOutcome, Fail> {
        Err(Fail::new(
            ResearchErrorCode::Disabled,
            "this stage's search provider is agentcore, which cannot fetch pages; fetch needs a stage on the tavily provider",
        ))
    }
}
