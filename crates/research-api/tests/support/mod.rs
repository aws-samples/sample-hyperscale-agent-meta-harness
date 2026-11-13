//! Shared set-up of the route tests: a handler over the in-memory fakes, a run with a policy row, requests as API Gateway would hand them.
#![allow(dead_code, clippy::unwrap_used)]

use harness_protocol::research::{
    PolicyItem, ResearchLimits, ResearchProvider, SearchResult, route,
};
use research_api::fakes::{FixedClock, MemBlobs, MemTable, RecordingInvoker, ScriptedProvider};
use research_api::handler::{Config, Deps, Request, Response, handle};
use research_api::ports::{ExtractOutcome, ExtractedPage, SearchOutcome, StageKind};
use serde_json::{Value, json};

pub const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
pub const OTHER_RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWY";
pub const ACCOUNT: &str = "111122223333";
pub const ROLE: &str = "hamh-direct-harness-worker-session";
/// 2026-10-16 15:00:00 UTC.
pub const NOW_MS: u64 = 1_792_162_800_000;

pub type TestDeps = Deps<MemTable, MemBlobs, ScriptedProvider, RecordingInvoker, FixedClock>;

pub fn caller_arn(run: &str) -> String {
    format!("arn:aws:sts::{ACCOUNT}:assumed-role/{ROLE}/{run}.direct")
}

pub fn policy(run: &str) -> PolicyItem {
    PolicyItem::new(
        run,
        true,
        ResearchProvider::Fixture,
        ResearchLimits::default(),
        NOW_MS / 1000 + 8 * 3600,
    )
}

pub fn deps(provider: ScriptedProvider) -> TestDeps {
    let d = Deps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        provider,
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(NOW_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: 80,
            stage_kind: StageKind::Test,
        },
    };
    d.table.put_policy(policy(RUN));
    d
}

pub fn fixture_provider() -> ScriptedProvider {
    ScriptedProvider::new(ResearchProvider::Fixture)
}

pub fn request(path: &str, body: &Value) -> Request {
    Request {
        method: "POST".into(),
        path: path.into(),
        caller_arn: Some(caller_arn(RUN)),
        account_id: Some(ACCOUNT.into()),
        access_key: Some("ASIA-test-session-key-id".into()),
        request_id: format!(
            "req-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ),
        body: body.to_string(),
    }
}

static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub async fn call(d: &TestDeps, path: &str, body: Value) -> (u16, Value, Response) {
    let resp = handle(d, &request(path, &body)).await;
    let v: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
    (resp.status, v, resp)
}

pub fn result(title: &str, url: &str, snippet: &str) -> SearchResult {
    SearchResult {
        title: title.into(),
        url: url.into(),
        snippet: snippet.into(),
        published: None,
        score: None,
    }
}

pub fn outcome(results: Vec<SearchResult>) -> SearchOutcome {
    SearchOutcome {
        results,
        credits: Some(1),
        upstream_request_id: Some("tvly-req-1".into()),
    }
}

pub fn page(url: &str, text: &str) -> ExtractedPage {
    ExtractedPage {
        url: url.into(),
        text: text.into(),
    }
}

pub fn extracted(pages: Vec<ExtractedPage>) -> ExtractOutcome {
    ExtractOutcome {
        pages,
        failed: vec![],
        credits: Some(1),
        upstream_request_id: None,
    }
}

pub fn search_path() -> &'static str {
    route::SEARCH
}

/// The audit objects of a run, parsed, in key order.
pub fn audits(d: &TestDeps, run: &str) -> Vec<Value> {
    let prefix = format!("runs/{run}/agents/direct/research/audit/");
    d.blobs
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(&prefix))
        .map(|k| serde_json::from_slice(&d.blobs.bytes(&k).unwrap()).unwrap())
        .collect()
}

pub fn json_body(v: Value) -> Value {
    v
}

pub fn q(text: &str) -> Value {
    json!({ "query": text })
}
