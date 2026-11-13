//! A scripted fake AgentCore Gateway on 127.0.0.1 (MCP over JSON-RPC, protocol 2025-03-26) for the provider tests and the route tests of the
//! `agentcore` search provider. It answers `initialize`, hands every `tools/call` to the test, and records every request. The credentials and the
//! secret are stand-ins built at run time. No test reaches the network or AWS.

#![allow(dead_code, clippy::unwrap_used)]

use super::net_fake::{FakeServer, Recorded, Reply};
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use harness_protocol::research::SearchRequest;
use research_api::agentcore::{AgentcoreProvider, DEFAULT_TOOL, GatewayTarget};
use research_api::fakes::FixedClock;
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::ports::{Fail, Provider};
use research_api::providers::AgentcoreDeps;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

pub const GATEWAY_HOST: &str = "gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com";
pub const GATEWAY_URL: &str =
    "https://gw-abc12345.gateway.bedrock-agentcore.us-east-1.amazonaws.com/mcp";
/// 2026-10-25 12:00:00 UTC.
pub const NOW_S: u64 = 1_792_929_600;
pub const KEY_ID: &str = "ASIA-test-vector";

pub fn secret() -> String {
    ["stand-in-", "sec", "ret-", "0123456789abcdef"].concat()
}

pub fn token() -> String {
    ["stand-in-", "tok", "en-", "0123456789"].concat()
}

pub fn credentials() -> Credentials {
    Credentials::new(KEY_ID, secret(), Some(token()), None, "test")
}

pub fn http_for(addr: SocketAddr, total: Duration) -> HttpClient {
    let mut hosts = BTreeMap::new();
    hosts.insert(GATEWAY_HOST.to_string(), addr);
    HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: total,
            max_response_bytes: 4 * 1024 * 1024,
        },
        TestNet { hosts },
    )
    .unwrap()
}

pub fn provider_with(server: &FakeServer, total: Duration) -> AgentcoreProvider<FixedClock> {
    AgentcoreProvider::with_clock(
        http_for(server.addr, total),
        GatewayTarget::parse(GATEWAY_URL).unwrap(),
        DEFAULT_TOOL,
        SharedCredentialsProvider::new(credentials()),
        FixedClock::at_ms(NOW_S * 1000),
    )
}

pub fn provider(server: &FakeServer) -> AgentcoreProvider<FixedClock> {
    provider_with(server, Duration::from_secs(3))
}

pub fn search(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        ..Default::default()
    }
}

// ---- the fake gateway ------------------------------------------------------------------------------------------------------------------

pub fn method_of(rec: &Recorded) -> String {
    rec.json()["method"].as_str().unwrap_or("").to_string()
}

pub fn rpc(rec: &Recorded, result: Value) -> Reply {
    Reply::json(
        200,
        &json!({"jsonrpc": "2.0", "id": rec.json()["id"].clone(), "result": result}).to_string(),
    )
}

pub fn init_reply(rec: &Recorded) -> Reply {
    rpc(
        rec,
        json!({"protocolVersion": "2025-03-26", "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
               "serverInfo": {"name": "hamh-direct-websearch", "version": "1.0.0"}}),
    )
}

pub fn tool_ok(rec: &Recorded, text: &str) -> Reply {
    rpc(
        rec,
        json!({"isError": false, "content": [{"type": "text", "text": text}]}),
    )
}

pub fn tool_err(rec: &Recorded, text: &str) -> Reply {
    rpc(
        rec,
        json!({"isError": true, "content": [{"type": "text", "text": text}]}),
    )
}

/// The inner JSON string of a success: `{id, results:[{title,url,publishedDate,text}]}`. Every excerpt carries `marker`.
pub fn results_text(n: usize, marker: &str) -> String {
    let results: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "title": format!("Result {i}"),
                "url": format!("https://docs.example.org/page-{i}"),
                "publishedDate": if i % 2 == 0 { "2025-03-01" } else { "unknown" },
                "text": format!("{marker} excerpt number {i}. ").repeat(100),
            })
        })
        .collect();
    json!({"id": "31ff0adc", "results": results}).to_string()
}

/// A gateway that answers `initialize` and hands every `tools/call` to `call`.
pub async fn gateway(call: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> FakeServer {
    FakeServer::start(move |rec| match method_of(rec).as_str() {
        "initialize" => init_reply(rec),
        "tools/call" => call(rec),
        _ => Reply::json(404, "{}"),
    })
    .await
}

pub async fn ok_gateway(n: usize) -> FakeServer {
    gateway(move |rec| tool_ok(rec, &results_text(n, "MARK"))).await
}

pub fn calls(server: &FakeServer) -> Vec<Recorded> {
    server
        .requests()
        .into_iter()
        .filter(|r| method_of(r) == "tools/call")
        .collect()
}

pub fn initializes(server: &FakeServer) -> usize {
    server
        .requests()
        .iter()
        .filter(|r| method_of(r) == "initialize")
        .count()
}

pub async fn fail_with(reply: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Fail {
    let server = gateway(reply).await;
    provider(&server).search(&search("q")).await.unwrap_err()
}

/// The client for tests that build the provider through `build_provider`: the gateway host maps to `addr`.
pub fn provider_http(addr: SocketAddr) -> HttpClient {
    http_for(addr, Duration::from_secs(3))
}

/// The settings `build_provider` takes for the provider: the address, the tool and stand-in credentials.
pub fn agentcore_deps() -> AgentcoreDeps {
    AgentcoreDeps {
        target: GatewayTarget::parse(GATEWAY_URL).unwrap(),
        tool: DEFAULT_TOOL.to_string(),
        credentials: SharedCredentialsProvider::new(credentials()),
    }
}
