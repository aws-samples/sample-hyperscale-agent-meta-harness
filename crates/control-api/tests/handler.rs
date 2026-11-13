//! The public `/v1` API: the IAM caller comes from the
//! request context, and every run route checks the owner (another caller's run is a 404). A run is
//! started by `POST /v1/direct/runs` (`tests/direct.rs`); here the runs are read: the list, one run with
//! its cost, the event cursor, the artifacts and a download URL, an S3 presigned GET of one artifact
//! under the run's prefix, valid for 15 minutes, with its true expiry; and the repository
//! allowlist the operator edits and `ham doctor` lists.
#![allow(clippy::unwrap_used)]

use control_api::handler::{
    Blobs, DOWNLOAD_URL_TTL, DeployedAs, Deps, Execution, RunUsage, Store, StoreError, V1Request,
    handle, handle_logged,
};
use control_api::repo_names::{EARLIER_DEFAULT_PREFIX, RepoNames};
use harness_protocol::api::v1::{
    DownloadUrlResponse, EventsPage, ListArtifactsResponse, ListRunsResponse, RunDetail,
    WhoAmIResponse,
};
use harness_protocol::common::Engine;
use harness_protocol::envelope::Envelope;
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{
    ArtifactItem, ArtifactKind, ControlItem, GoalItem, IdempotencyItem, Keyed, RepoAllowItem,
    RunItem, event_shard, s3,
};
use harness_protocol::target::RepoTarget;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const ME: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const OTHER: &str = "arn:aws:sts::800000000022:assumed-role/Admin/someone";
const PROMPT: &str = "Add a percent_change(a, b) function with tests";
const BUCKET: &str = "hamh-live-harness-800000000022";

fn hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[derive(Default)]
struct Fake {
    idem: Mutex<BTreeMap<String, IdempotencyItem>>,
    runs: Mutex<BTreeMap<String, RunItem>>,
    goals: Mutex<BTreeMap<String, GoalItem>>,
    events: Mutex<Vec<Envelope>>,
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    /// `ART#` items by (runId, artifactId).
    artifacts: Mutex<BTreeMap<(String, String), ArtifactItem>>,
    /// Every presign call: key and lifetime.
    presigned: Mutex<Vec<(String, Duration)>>,
    /// Step Functions semantics: one execution per name; a repeat start of a name is a no-op.
    started: Mutex<BTreeMap<String, Value>>,
    /// Fail this many `start` calls, then succeed.
    fail_starts: Mutex<u32>,
    /// Fail this many `create_run` calls, then succeed.
    fail_creates: Mutex<u32>,
    /// Every read fails.
    down: bool,
    /// Artifact reads fail (the run reads still work).
    artifacts_down: bool,
    /// Presigning fails.
    presign_down: bool,
    /// What the deployment read returns.
    deployment: DeployedAs,
    /// The deployment read fails.
    deployment_down: bool,
    /// Queued controls.
    controls: Mutex<Vec<ControlItem>>,
    /// Queuing a control fails.
    controls_down: bool,
    /// The repository allowlist by target.
    allow: Mutex<BTreeMap<String, RepoAllowItem>>,
    /// The runs' Agent and Turn items (W10 cost).
    usage: Mutex<BTreeMap<String, RunUsage>>,
    /// Reading them fails.
    usage_down: bool,
}

fn take(n: &Mutex<u32>) -> bool {
    let mut g = n.lock().unwrap();
    let fail = *g > 0;
    *g = g.saturating_sub(1);
    fail
}

impl Fake {
    fn up(&self) -> Result<(), StoreError> {
        if self.down {
            return Err(StoreError::Unavailable("table unreachable".into()));
        }
        Ok(())
    }
}

impl Store for &Fake {
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        self.up()?;
        let mut g = self.idem.lock().unwrap();
        Ok(g.entry(c.key().pk).or_insert_with(|| c.clone()).clone())
    }
    async fn create_run(
        &self,
        run: &RunItem,
        goal: &GoalItem,
        created: &Envelope,
    ) -> Result<(), StoreError> {
        if take(&self.fail_creates) {
            return Err(StoreError::Unavailable("throttled".into()));
        }
        let mut runs = self.runs.lock().unwrap();
        if runs.contains_key(&run.run_id) {
            return Err(StoreError::Conflict);
        }
        runs.insert(run.run_id.clone(), run.clone());
        self.goals
            .lock()
            .unwrap()
            .insert(run.run_id.clone(), goal.clone());
        self.events.lock().unwrap().push(created.clone());
        Ok(())
    }
    async fn get_run(&self, run_id: &str) -> Result<Option<RunItem>, StoreError> {
        self.up()?;
        Ok(self.runs.lock().unwrap().get(run_id).cloned())
    }
    async fn get_root_goal(&self, run_id: &str) -> Result<Option<GoalItem>, StoreError> {
        self.up()?;
        Ok(self.goals.lock().unwrap().get(run_id).cloned())
    }
    async fn list_runs(
        &self,
        owner: &str,
        next: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        self.up()?;
        assert_eq!(next, None);
        let mut out: Vec<RunItem> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.owner_principal_arn == owner)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.run_id.cmp(&a.run_id));
        Ok((out, None))
    }
    async fn events(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        self.up()?;
        let mut out: Vec<Envelope> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .filter(|e| after.is_none_or(|a| e.event_id.as_str() > a))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        out.truncate(limit);
        Ok(out)
    }
    async fn list_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        self.up()?;
        if self.artifacts_down {
            return Err(StoreError::Unavailable("artifact query throttled".into()));
        }
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .values()
            .filter(|a| a.run_id == run_id)
            .cloned()
            .collect())
    }
    async fn get_artifact(
        &self,
        run_id: &str,
        artifact_id: &str,
    ) -> Result<Option<ArtifactItem>, StoreError> {
        self.up()?;
        if self.artifacts_down {
            return Err(StoreError::Unavailable("artifact query throttled".into()));
        }
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .get(&(run_id.to_string(), artifact_id.to_string()))
            .cloned())
    }
    async fn repo_allow_list(&self) -> Result<Vec<RepoAllowItem>, StoreError> {
        self.up()?;
        Ok(self.allow.lock().unwrap().values().cloned().collect())
    }
    async fn repo_allow_put(&self, item: &RepoAllowItem) -> Result<(), StoreError> {
        self.up()?;
        self.allow
            .lock()
            .unwrap()
            .insert(item.target.to_string(), item.clone());
        Ok(())
    }
    async fn repo_allow_delete(&self, target: &RepoTarget) -> Result<(), StoreError> {
        self.up()?;
        self.allow.lock().unwrap().remove(&target.to_string());
        Ok(())
    }
    async fn run_usage(&self, run_id: &str) -> Result<RunUsage, StoreError> {
        self.up()?;
        if self.usage_down {
            return Err(StoreError::Unavailable("run usage unreadable".into()));
        }
        Ok(self
            .usage
            .lock()
            .unwrap()
            .get(run_id)
            .cloned()
            .unwrap_or_default())
    }
    async fn put_control(&self, control: &ControlItem) -> Result<(), StoreError> {
        if self.controls_down {
            return Err(StoreError::Unavailable("controls write throttled".into()));
        }
        self.controls.lock().unwrap().push(control.clone());
        Ok(())
    }
}

impl Blobs for &Fake {
    async fn put(&self, key: &str, body: &[u8]) -> Result<String, StoreError> {
        self.blobs
            .lock()
            .unwrap()
            .insert(key.to_string(), body.to_vec());
        Ok(format!("s3://{BUCKET}/{key}"))
    }
    fn bucket(&self) -> &str {
        BUCKET
    }
    async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
    ) -> Result<DownloadUrlResponse, StoreError> {
        if self.presign_down {
            return Err(StoreError::Unavailable(
                "no credentials to sign with".into(),
            ));
        }
        self.presigned.lock().unwrap().push((key.to_string(), ttl));
        Ok(DownloadUrlResponse {
            url: format!("https://{BUCKET}.s3.us-west-2.amazonaws.com/{key}?X-Amz-Expires=900"),
            expires_at: "2026-09-25T20:15:00Z".into(),
        })
    }
}

impl Execution for &Fake {
    async fn start(&self, name: &str, input: &Value) -> Result<(), StoreError> {
        if take(&self.fail_starts) {
            return Err(StoreError::Unavailable("ThrottlingException".into()));
        }
        self.started
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| input.clone());
        Ok(())
    }

    async fn deployment(&self) -> Result<DeployedAs, StoreError> {
        if self.deployment_down {
            return Err(StoreError::Unavailable("ParameterNotFound".into()));
        }
        Ok(self.deployment.clone())
    }
}

fn deps(f: &Fake) -> Deps<&Fake, &Fake, &Fake> {
    Deps {
        store: f,
        blobs: f,
        execution: f,
        account: "800000000022".into(),
        operators: vec!["arn:aws:sts::800000000022:assumed-role/Admin/*".into()],
        repo_names: Default::default(),
    }
}

fn req(
    method: &str,
    path: &str,
    caller: Option<&str>,
    headers: &[(&str, &str)],
    query: &[(&str, &str)],
    body: &str,
) -> V1Request {
    V1Request {
        method: method.into(),
        path: path.into(),
        caller_arn: caller.map(str::to_string),
        source_ip: None,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.to_string()))
            .collect(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.into(),
    }
}

fn start_body_with(prompt: &str) -> String {
    let content = "Prefer small pure functions.\n";
    json!({
        "prompt": prompt,
        "target": {"mode": "repo", "repo": "cc:hamh-e2e-python-calc"},
        "model": "global.anthropic.claude-sonnet-5-5",
        "rules": [{"path": "style.md", "sha256": hex(content.as_bytes()), "content": content}]
    })
    .to_string()
}

fn start_body() -> String {
    start_body_with(PROMPT)
}

/// Starts a run through `POST /v1/direct/runs`, the one route that makes one.
async fn start(f: &Fake, key: &str) -> (u16, Value) {
    let out = handle(
        req(
            "POST",
            "/v1/direct/runs",
            Some(ME),
            &[("Idempotency-Key", key)],
            &[],
            &start_body(),
        ),
        &deps(f),
    )
    .await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

#[tokio::test]
async fn run_reads_are_owner_only_and_another_callers_run_is_not_found() {
    let f = Fake::default();
    let (_, v) = start(&f, "k-7").await;
    let id = v["runId"].as_str().unwrap().to_string();
    let d = deps(&f);
    let out = handle(
        req("GET", &format!("/v1/runs/{id}"), Some(ME), &[], &[], ""),
        &d,
    )
    .await;
    assert_eq!(out.status, 200, "{}", out.body);
    let detail: RunDetail = serde_json::from_str(&out.body).unwrap();
    assert_eq!(detail.summary.run_id, id);
    assert_eq!(detail.summary.owner_principal_arn, ME);
    assert_eq!(
        (detail.summary.status, detail.summary.phase),
        (GoalStatus::Active, GoalPhase::Planning)
    );
    assert_eq!(detail.objective.as_deref(), Some(PROMPT));
    assert_eq!(detail.tokens_used, 0);
    assert_eq!(detail.links, Default::default());
    for path in [format!("/v1/runs/{id}"), format!("/v1/runs/{id}/events")] {
        let out = handle(
            req("GET", &path, Some(OTHER), &[], &[("shard", "0")], ""),
            &d,
        )
        .await;
        assert_eq!(out.status, 404, "{path}");
        assert!(!out.body.contains(PROMPT));
    }
    for missing in ["01K5ZQ4T8M3N6P7R9S0V1W2X3Y", "not-a-run"] {
        let out = handle(
            req(
                "GET",
                &format!("/v1/runs/{missing}"),
                Some(ME),
                &[],
                &[],
                "",
            ),
            &d,
        )
        .await;
        assert_eq!(out.status, 404, "{missing}");
    }
    start(&f, "k-8").await;
    let list: ListRunsResponse = serde_json::from_str(
        &handle(req("GET", "/v1/runs", Some(ME), &[], &[], ""), &d)
            .await
            .body,
    )
    .unwrap();
    assert_eq!(list.runs.len(), 2);
    assert!(list.runs[0].run_id > list.runs[1].run_id, "newest first");
    assert_eq!(list.next_token, None);
    let theirs: ListRunsResponse = serde_json::from_str(
        &handle(req("GET", "/v1/runs", Some(OTHER), &[], &[], ""), &d)
            .await
            .body,
    )
    .unwrap();
    assert!(theirs.runs.is_empty());
}

#[tokio::test]
async fn a_run_whose_root_execution_ended_early_lists_and_reads_why() {
    // The reaper writes the root execution's end on the Run item, so `ham runs` and
    // the run detail show why a cleared run ended.
    let f = Fake::default();
    let (_, v) = start(&f, "k-abort").await;
    let id = v["runId"].as_str().unwrap().to_string();
    let why = harness_protocol::orchestration::RunAbortedPayload::new(
        harness_protocol::orchestration::ExecutionEnd::Aborted,
        Some("HarnessOperatorStop"),
        Some("operator: orphan check"),
        "2026-09-29T21:30:26Z",
    );
    {
        let mut runs = f.runs.lock().unwrap();
        let r = runs.get_mut(&id).unwrap();
        r.status = GoalStatus::Cleared;
        r.aborted = Some(Box::new(why.clone()));
    }
    let d = deps(&f);
    let list: ListRunsResponse = serde_json::from_str(
        &handle(req("GET", "/v1/runs", Some(ME), &[], &[], ""), &d)
            .await
            .body,
    )
    .unwrap();
    assert_eq!(list.runs[0].aborted.as_deref(), Some(&why));
    let out = handle(
        req("GET", &format!("/v1/runs/{id}"), Some(ME), &[], &[], ""),
        &d,
    )
    .await;
    let detail: RunDetail = serde_json::from_str(&out.body).unwrap();
    assert_eq!(detail.summary.status, GoalStatus::Cleared);
    assert_eq!(detail.summary.aborted.as_deref(), Some(&why));
    // A run that did not end early carries no field on the wire.
    let (_, v) = start(&f, "k-plain").await;
    let plain = v["runId"].as_str().unwrap().to_string();
    let out = handle(
        req("GET", &format!("/v1/runs/{plain}"), Some(ME), &[], &[], ""),
        &d,
    )
    .await;
    let raw: Value = serde_json::from_str(&out.body).unwrap();
    assert!(raw["summary"].get("aborted").is_none(), "{raw}");
}

#[tokio::test]
async fn events_are_read_by_shard_after_a_cursor() {
    let f = Fake::default();
    let (_, v) = start(&f, "k-9").await;
    let id = v["runId"].as_str().unwrap().to_string();
    let d = deps(&f);
    let path = format!("/v1/runs/{id}/events");
    let page: EventsPage = serde_json::from_str(
        &handle(req("GET", &path, Some(ME), &[], &[("shard", "00")], ""), &d)
            .await
            .body,
    )
    .unwrap();
    assert_eq!(page.events.len(), 1);
    let cursor = page.next_after.clone().unwrap();
    assert_eq!(cursor, page.events[0].event_id);
    let next: EventsPage = serde_json::from_str(
        &handle(
            req(
                "GET",
                &path,
                Some(ME),
                &[],
                &[("shard", "00"), ("after", &cursor)],
                "",
            ),
            &d,
        )
        .await
        .body,
    )
    .unwrap();
    assert!(next.events.is_empty());
    assert_eq!(next.next_after.as_deref(), Some(cursor.as_str()));
    let other: EventsPage = serde_json::from_str(
        &handle(req("GET", &path, Some(ME), &[], &[("shard", "31")], ""), &d)
            .await
            .body,
    )
    .unwrap();
    assert!(other.events.is_empty());
    assert_eq!(other.next_after, None);
    for bad in [
        vec![],
        vec![("shard", "32")],
        vec![("shard", "x")],
        vec![("shard", "-1")],
        vec![("shard", "0"), ("after", "not-a-ulid")],
    ] {
        let out = handle(req("GET", &path, Some(ME), &[], &bad, ""), &d).await;
        assert_eq!(out.status, 400, "{bad:?}");
    }
}

#[tokio::test]
async fn whoami_names_the_caller_and_other_routes_are_refused() {
    let f = Fake::default();
    let d = deps(&f);
    let w: WhoAmIResponse = serde_json::from_str(
        &handle(req("GET", "/v1/whoami", Some(ME), &[], &[], ""), &d)
            .await
            .body,
    )
    .unwrap();
    assert_eq!(
        (w.principal_arn.as_str(), w.account.as_str()),
        (ME, "800000000022")
    );
    for (method, path, caller, want) in [
        ("GET", "/v1/whoami", None, 403),
        ("GET", "/v1/runs", None, 403),
        ("GET", "/v1/nope", Some(ME), 404),
        (
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events/extra",
            Some(ME),
            404,
        ),
        ("DELETE", "/v1/runs", Some(ME), 405),
        ("POST", "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y", Some(ME), 405),
        ("POST", "/v1/whoami", Some(ME), 405),
    ] {
        let out = handle(req(method, path, caller, &[], &[], ""), &d).await;
        assert_eq!(out.status, want, "{method} {path}");
        let v: Value = serde_json::from_str(&out.body).unwrap();
        assert!(v["code"].is_string(), "{v}");
    }
}

#[tokio::test]
async fn a_store_outage_is_a_503_and_its_reason_goes_to_the_log_only() {
    let f = Fake {
        down: true,
        ..Fake::default()
    };
    let d = deps(&f);
    for (method, path, headers, query, body) in [
        ("GET", "/v1/runs".to_string(), vec![], vec![], String::new()),
        (
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y".to_string(),
            vec![],
            vec![],
            String::new(),
        ),
        (
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events".to_string(),
            vec![],
            vec![("shard", "0")],
            String::new(),
        ),
        (
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/artifacts".to_string(),
            vec![],
            vec![],
            String::new(),
        ),
        (
            "POST",
            "/v1/direct/runs".to_string(),
            vec![("Idempotency-Key", "k")],
            vec![],
            start_body(),
        ),
        (
            "POST",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X41/download-url"
                .to_string(),
            vec![],
            vec![],
            String::new(),
        ),
    ] {
        let (out, log) =
            handle_logged(req(method, &path, Some(ME), &headers, &query, &body), &d).await;
        assert_eq!(out.status, 503, "{method} {path}: {}", out.body);
        let v: Value = serde_json::from_str(&out.body).unwrap();
        assert_eq!(v["code"], "unavailable");
        assert!(!out.body.contains("table unreachable"), "{}", out.body);
        assert!(
            log.iter().any(|l| l.contains("table unreachable")),
            "{log:?}"
        );
    }
}

fn artifact(run_id: &str, artifact_id: &str, kind: ArtifactKind, key: &str) -> ArtifactItem {
    ArtifactItem {
        run_id: run_id.into(),
        artifact_id: artifact_id.into(),
        kind,
        s3_uri: format!("s3://{BUCKET}/{key}"),
        sha256: "ab".repeat(32),
        size_bytes: 2048,
    }
}

fn add_artifact(f: &Fake, a: ArtifactItem) {
    f.artifacts
        .lock()
        .unwrap()
        .insert((a.run_id.clone(), a.artifact_id.clone()), a);
}

async fn started_run(f: &Fake, key: &str) -> String {
    let (_, v) = start(f, key).await;
    v["runId"].as_str().unwrap().to_string()
}

async fn detail_of(f: &Fake, id: &str) -> control_api::handler::V1Response {
    handle(
        req("GET", &format!("/v1/runs/{id}"), Some(ME), &[], &[], ""),
        &deps(f),
    )
    .await
}

#[tokio::test]
async fn a_runs_detail_carries_its_cost_by_model_and_engine_once_a_turn_reports_usage() {
    let f = Fake::default();
    let id = started_run(&f, "k-cost").await;
    let out = detail_of(&f, &id).await;
    let v: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(v["costUsd"], 0.0);
    assert!(v.get("cost").is_none(), "{v}");
    let agent: harness_protocol::store::AgentItem = serde_json::from_value(json!({
        "runId": id, "agentId": "direct", "taskId": "root", "attempt": 1, "runtimeArn": "arn:aws:bedrock-agentcore:x",
        "runtimeSessionId": "s", "generation": 1, "engine": "cline", "engineVersion": "1", "transport": "acp",
        "modelId": "global.anthropic.claude-sonnet-5-5", "pool": "p", "status": "running", "version": 1})).unwrap();
    let turn = |n: u32, usage: Option<Value>| -> harness_protocol::store::TurnItem {
        let mut t = json!({"runId": id, "agentId": "direct", "n": n, "taskTokenCiphertext": "c", "startedAt": "2026-09-27T00:00:00Z",
            "rulesDigest": "d", "version": 1});
        if let Some(u) = usage {
            t["usage"] = u;
        }
        serde_json::from_value(t).unwrap()
    };
    f.usage.lock().unwrap().insert(id.clone(), RunUsage { agents: vec![agent], turns: vec![
        turn(1, Some(json!({"inputTokens": 1_000_000, "outputTokens": 100_000, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0}))),
        turn(2, None),
    ] });
    let out = detail_of(&f, &id).await;
    assert_eq!(out.status, 200, "{}", out.body);
    let detail: RunDetail = serde_json::from_str(&out.body).unwrap();
    // 1M input at 2.00 plus 100K output at 10.00 per 1M tokens.
    assert_eq!(detail.cost_usd, 3.0);
    let cost = detail.cost.unwrap();
    assert_eq!(cost.prices_version, "bedrock-us-west-2-2026-10-05");
    assert_eq!(cost.turns_without_usage, 1);
    assert_eq!(
        (
            cost.lines[0].model_id.as_str(),
            cost.lines[0].engine,
            cost.lines[0].usd
        ),
        ("global.anthropic.claude-sonnet-5-5", Engine::CLINE, 3.0)
    );
    let down = Fake {
        usage_down: true,
        ..Default::default()
    };
    let id2 = started_run(&down, "k-cost-2").await;
    let out = handle(
        req("GET", &format!("/v1/runs/{id2}"), Some(ME), &[], &[], ""),
        &deps(&down),
    )
    .await;
    assert_eq!(out.status, 503);
}

#[tokio::test]
async fn a_runs_detail_reports_the_tokens_its_turns_used() {
    // A direct run never moves the root goal's counter, so `tokensUsed` is the sum of what its turns reported: the four kinds of token, the
    // same sum the cost is priced from and the delivery card prints.
    let f = Fake::default();
    let id = started_run(&f, "k-tokens").await;
    let detail: RunDetail = serde_json::from_str(&detail_of(&f, &id).await.body).unwrap();
    assert_eq!(detail.tokens_used, 0, "no turn has reported usage yet");
    let agent = |agent_id: &str, model: &str| -> harness_protocol::store::AgentItem {
        serde_json::from_value(json!({
            "runId": id, "agentId": agent_id, "taskId": "root", "attempt": 1, "runtimeArn": "arn:aws:bedrock-agentcore:x",
            "runtimeSessionId": "s", "generation": 1, "engine": "cline", "engineVersion": "1", "transport": "acp",
            "modelId": model, "pool": "p", "status": "running", "version": 1}))
        .unwrap()
    };
    let turn = |agent_id: &str,
                n: u32,
                usage: Option<Value>|
     -> harness_protocol::store::TurnItem {
        let mut t = json!({"runId": id, "agentId": agent_id, "n": n, "taskTokenCiphertext": "c", "startedAt": "2026-09-27T00:00:00Z",
            "rulesDigest": "d", "version": 1});
        if let Some(u) = usage {
            t["usage"] = u;
        }
        serde_json::from_value(t).unwrap()
    };
    let usage = |i: u64, o: u64, r: u64, w: u64| json!({"inputTokens": i, "outputTokens": o, "cacheReadInputTokens": r, "cacheWriteInputTokens": w});
    f.usage.lock().unwrap().insert(
        id.clone(),
        RunUsage {
            agents: vec![
                agent("direct", "global.anthropic.claude-sonnet-5-5"),
                agent("direct-2", "global.anthropic.claude-opus-5-5"),
            ],
            turns: vec![
                turn("direct", 1, Some(usage(1_000, 200, 3_000, 40))),
                turn("direct", 2, Some(usage(500, 100, 0, 0))),
                turn("direct-2", 1, Some(usage(7, 8, 9, 10))),
                // A turn that reported nothing adds nothing.
                turn("direct", 3, None),
            ],
        },
    );
    let detail: RunDetail = serde_json::from_str(&detail_of(&f, &id).await.body).unwrap();
    assert_eq!(
        detail.tokens_used,
        1_000 + 200 + 3_000 + 40 + 500 + 100 + 7 + 8 + 9 + 10
    );
    // It is the sum of the lines of `cost`, which the client prints on the card.
    let from_lines: u64 = detail
        .cost
        .unwrap()
        .lines
        .iter()
        .map(|l| {
            l.usage.input_tokens
                + l.usage.output_tokens
                + l.usage.cache_read_input_tokens
                + l.usage.cache_write_input_tokens
        })
        .sum();
    assert_eq!(detail.tokens_used, from_lines);
}

#[tokio::test]
async fn artifacts_are_listed_in_id_order_for_the_owner_only() {
    let f = Fake::default();
    let id = started_run(&f, "k-a1").await;
    let other = started_run(&f, "k-a2").await;
    let zip = s3::deliverable_key(&id, "python-calc", "abc1234");
    add_artifact(
        &f,
        artifact(&id, "01K5ZQ4T8M3N6P7R9S0V1W2X42", ArtifactKind::Zip, &zip),
    );
    add_artifact(
        &f,
        artifact(
            &id,
            "01K5ZQ4T8M3N6P7R9S0V1W2X41",
            ArtifactKind::Evidence,
            &format!("{}EVIDENCE.md", s3::evidence_prefix(&id)),
        ),
    );
    add_artifact(
        &f,
        artifact(
            &other,
            "01K5ZQ4T8M3N6P7R9S0V1W2X43",
            ArtifactKind::Zip,
            "runs/x/y.zip",
        ),
    );
    let d = deps(&f);
    let path = format!("/v1/runs/{id}/artifacts");
    let out = handle(req("GET", &path, Some(ME), &[], &[], ""), &d).await;
    assert_eq!(out.status, 200, "{}", out.body);
    let list: ListArtifactsResponse = serde_json::from_str(&out.body).unwrap();
    let ids: Vec<&str> = list
        .artifacts
        .iter()
        .map(|a| a.artifact_id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["01K5ZQ4T8M3N6P7R9S0V1W2X41", "01K5ZQ4T8M3N6P7R9S0V1W2X42"]
    );
    let z = &list.artifacts[1];
    assert_eq!(z.kind, ArtifactKind::Zip);
    assert_eq!(z.s3_uri, format!("s3://{BUCKET}/{zip}"));
    assert_eq!(z.sha256, "ab".repeat(32));
    assert_eq!(z.size_bytes, 2048);

    let out = handle(req("GET", &path, Some(OTHER), &[], &[], ""), &d).await;
    assert_eq!(out.status, 404, "{}", out.body);
    assert!(!out.body.contains("deliverable"), "{}", out.body);
    let out = handle(
        req(
            "GET",
            "/v1/runs/not-a-run/artifacts",
            Some(ME),
            &[],
            &[],
            "",
        ),
        &d,
    )
    .await;
    assert_eq!(out.status, 404);
    let out = handle(req("GET", &path, None, &[], &[], ""), &d).await;
    assert_eq!(out.status, 403);

    // A run with no artifacts yet lists none.
    let fresh = started_run(&f, "k-a3").await;
    let out = handle(
        req(
            "GET",
            &format!("/v1/runs/{fresh}/artifacts"),
            Some(ME),
            &[],
            &[],
            "",
        ),
        &d,
    )
    .await;
    assert_eq!(out.status, 200);
    let list: ListArtifactsResponse = serde_json::from_str(&out.body).unwrap();
    assert!(list.artifacts.is_empty());
}

#[tokio::test]
async fn a_download_url_is_a_15_minute_presigned_get_of_the_artifact_key() {
    assert_eq!(DOWNLOAD_URL_TTL, Duration::from_secs(15 * 60));
    let f = Fake::default();
    let id = started_run(&f, "k-d1").await;
    let zip = s3::deliverable_key(&id, "python-calc", "abc1234");
    add_artifact(
        &f,
        artifact(&id, "01K5ZQ4T8M3N6P7R9S0V1W2X42", ArtifactKind::Zip, &zip),
    );
    let d = deps(&f);
    let path = format!("/v1/runs/{id}/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X42/download-url");
    // The URL writes nothing, so the call needs no Idempotency-Key; one is accepted.
    for headers in [vec![], vec![("Idempotency-Key", "dl-1")]] {
        let out = handle(req("POST", &path, Some(ME), &headers, &[], ""), &d).await;
        assert_eq!(out.status, 200, "{}", out.body);
        let url: DownloadUrlResponse = serde_json::from_str(&out.body).unwrap();
        assert!(url.url.contains(&zip), "{}", url.url);
        assert_eq!(url.expires_at, "2026-09-25T20:15:00Z");
    }
    assert_eq!(
        f.presigned.lock().unwrap().clone(),
        vec![(zip.clone(), DOWNLOAD_URL_TTL), (zip, DOWNLOAD_URL_TTL)]
    );

    for (caller, p) in [
        (Some(OTHER), path.clone()),
        (
            Some(ME),
            format!("/v1/runs/{id}/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X49/download-url"),
        ),
        (
            Some(ME),
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X42/download-url"
                .to_string(),
        ),
        (
            Some(ME),
            format!("/v1/runs/{id}/artifacts/a%2Fb/download-url"),
        ),
    ] {
        let out = handle(req("POST", &p, caller, &[], &[], ""), &d).await;
        assert_eq!(out.status, 404, "{caller:?} {p}: {}", out.body);
    }
    assert_eq!(f.presigned.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_artifact_outside_the_runs_prefix_is_never_presigned() {
    let f = Fake::default();
    let id = started_run(&f, "k-d2").await;
    let other = started_run(&f, "k-d3").await;
    let theirs = s3::deliverable_key(&other, "python-calc", "abc1234");
    let cases = [
        (
            "01K5ZQ4T8M3N6P7R9S0V1W2X51",
            format!("s3://{BUCKET}/{theirs}"),
        ),
        (
            "01K5ZQ4T8M3N6P7R9S0V1W2X52",
            format!(
                "s3://another-bucket/{}",
                s3::deliverable_key(&id, "python-calc", "abc1234")
            ),
        ),
        (
            "01K5ZQ4T8M3N6P7R9S0V1W2X53",
            "https://example.com/x.zip".to_string(),
        ),
        ("01K5ZQ4T8M3N6P7R9S0V1W2X54", format!("s3://{BUCKET}/")),
    ];
    for (aid, uri) in &cases {
        let mut a = artifact(&id, aid, ArtifactKind::Zip, "unused");
        a.s3_uri = uri.clone();
        add_artifact(&f, a);
    }
    let d = deps(&f);
    for (aid, uri) in &cases {
        let (out, log) = handle_logged(
            req(
                "POST",
                &format!("/v1/runs/{id}/artifacts/{aid}/download-url"),
                Some(ME),
                &[],
                &[],
                "",
            ),
            &d,
        )
        .await;
        assert_eq!(out.status, 404, "{uri}: {}", out.body);
        assert!(
            log.iter().any(|l| l.contains(aid)),
            "{uri}: the refusal is logged: {log:?}"
        );
    }
    assert!(f.presigned.lock().unwrap().is_empty());
}

#[tokio::test]
async fn artifact_routes_refuse_other_methods() {
    let f = Fake::default();
    let id = started_run(&f, "k-d4").await;
    let d = deps(&f);
    for (method, path) in [
        ("POST", format!("/v1/runs/{id}/artifacts")),
        ("DELETE", format!("/v1/runs/{id}/artifacts")),
        (
            "GET",
            format!("/v1/runs/{id}/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X42/download-url"),
        ),
    ] {
        let out = handle(req(method, &path, Some(ME), &[], &[], ""), &d).await;
        assert_eq!(out.status, 405, "{method} {path}");
    }
    let out = handle(
        req(
            "GET",
            &format!("/v1/runs/{id}/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X42"),
            Some(ME),
            &[],
            &[],
            "",
        ),
        &d,
    )
    .await;
    assert_eq!(out.status, 404);
}

#[tokio::test]
async fn artifact_read_and_presign_failures_are_503s_logged_with_their_reason() {
    let f = Fake {
        artifacts_down: true,
        ..Fake::default()
    };
    let id = started_run(&f, "k-d5").await;
    let d = deps(&f);
    for (method, path) in [
        ("GET", format!("/v1/runs/{id}/artifacts")),
        (
            "POST",
            format!("/v1/runs/{id}/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X42/download-url"),
        ),
    ] {
        let (out, log) = handle_logged(req(method, &path, Some(ME), &[], &[], ""), &d).await;
        assert_eq!(out.status, 503, "{method} {path}");
        assert!(!out.body.contains("throttled"));
        assert!(
            log.iter().any(|l| l.contains("artifact query throttled")),
            "{log:?}"
        );
    }

    let f = Fake {
        presign_down: true,
        ..Fake::default()
    };
    let id = started_run(&f, "k-d6").await;
    add_artifact(
        &f,
        artifact(
            &id,
            "01K5ZQ4T8M3N6P7R9S0V1W2X42",
            ArtifactKind::Zip,
            &s3::deliverable_key(&id, "python-calc", "abc1234"),
        ),
    );
    let (out, log) = handle_logged(
        req(
            "POST",
            &format!("/v1/runs/{id}/artifacts/01K5ZQ4T8M3N6P7R9S0V1W2X42/download-url"),
            Some(ME),
            &[],
            &[],
            "",
        ),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 503);
    assert!(
        log.iter()
            .any(|l| l.contains("no credentials to sign with")),
        "{log:?}"
    );
}

// ---- the repository allowlist -------------

const NOT_OPERATOR: &str = "arn:aws:sts::800000000022:assumed-role/hamh-live-e2e-other-principal/x";

fn admin(caller: &str, body: Value) -> V1Request {
    req(
        "POST",
        "/v1/admin/repos",
        Some(caller),
        &[],
        &[],
        &body.to_string(),
    )
}

#[tokio::test]
async fn an_operator_allows_and_denies_a_repository_and_ham_doctor_lists_the_entries() {
    let f = Fake::default();
    let d = deps(&f);
    let r = handle(
        admin(ME, json!({"action": "allow", "target": "cc:hamh-e2e-foo"})),
        &d,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["action"], "allow");
    assert_eq!(v["target"], "cc:hamh-e2e-foo");
    assert_eq!(v["entry"]["addedBy"], ME);
    assert!(v["entry"]["addedAt"].as_str().unwrap().ends_with('Z'));
    let r = handle(
        admin(
            ME,
            json!({"action": "allow", "target": "gh:ex-sample1/app"}),
        ),
        &d,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // Any caller of the account may read the list (ham doctor).
    let r = handle(req("GET", "/v1/repos", Some(OTHER), &[], &[], ""), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let v: Value = serde_json::from_str(&r.body).unwrap();
    let targets: Vec<&str> = v["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["cc:hamh-e2e-foo", "gh:ex-sample1/app"]);
    let r = handle(
        admin(ME, json!({"action": "deny", "target": "cc:hamh-e2e-foo"})),
        &d,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        serde_json::from_str::<Value>(&r.body).unwrap(),
        json!({"action": "deny", "target": "cc:hamh-e2e-foo"})
    );
    assert_eq!(
        f.allow.lock().unwrap().keys().cloned().collect::<Vec<_>>(),
        ["gh:ex-sample1/app"]
    );
}

#[tokio::test]
async fn a_configured_repository_name_rule_lets_an_operator_allow_the_names_it_covers() {
    // The stage's rule (HAMH_REPO_NAMES, from the `repoNames` of the account file) replaces the default prefix.
    let f = Fake::default();
    let mut d = deps(&f);
    d.repo_names = RepoNames::parse("my-app,team-*").unwrap();
    for target in ["cc:my-app", "cc:team-b"] {
        let r = handle(admin(ME, json!({"action": "allow", "target": target})), &d).await;
        assert_eq!(r.status, 200, "{target}: {}", r.body);
    }
    let allowed: Vec<String> = f.allow.lock().unwrap().keys().cloned().collect();
    assert_eq!(allowed, ["cc:my-app", "cc:team-b"]);
}

#[tokio::test]
async fn a_configured_repository_name_rule_refuses_the_other_names_with_the_rule_in_the_text() {
    let f = Fake::default();
    let mut d = deps(&f);
    d.repo_names = RepoNames::parse("my-app,team-*").unwrap();
    for target in ["cc:hamh-e2e-foo", "cc:my-app-2", "cc:other"] {
        let r = handle(admin(ME, json!({"action": "allow", "target": target})), &d).await;
        assert_eq!(r.status, 422, "{target}: {}", r.body);
        let e: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(e["code"], "refused", "{target}");
        assert_eq!(
            e["message"],
            format!(
                "refused: {target} is outside the repository names this stage allows (my-app, team-*)"
            ),
            "{target}"
        );
    }
    assert!(f.allow.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_rule_that_covers_every_name_lets_an_operator_allow_any_codecommit_repository() {
    let f = Fake::default();
    let mut d = deps(&f);
    d.repo_names = RepoNames::parse("*").unwrap();
    let r = handle(
        admin(ME, json!({"action": "allow", "target": "cc:python-calc"})),
        &d,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn the_default_rule_refuses_a_name_outside_the_prefix_with_the_text_of_the_fixed_prefix() {
    let f = Fake::default();
    let d = deps(&f);
    assert!(d.repo_names.is_default());
    let r = handle(
        admin(ME, json!({"action": "allow", "target": "cc:my-app"})),
        &d,
    )
    .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let e: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        e["message"],
        "refused: cc:my-app is outside the hamh-e2e- prefix"
    );
}

#[tokio::test]
async fn the_default_rule_lets_an_operator_allow_an_earlier_repository() {
    // A repository the earlier release made stays usable after the operator allows it on the new stage's table.
    let f = Fake::default();
    let d = deps(&f);
    assert!(d.repo_names.is_default());
    let target = format!("cc:{EARLIER_DEFAULT_PREFIX}foo");
    let r = handle(admin(ME, json!({"action": "allow", "target": target})), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let allowed: Vec<String> = f.allow.lock().unwrap().keys().cloned().collect();
    assert_eq!(allowed, [target]);
}

#[tokio::test]
async fn a_rule_with_the_default_entry_lets_an_operator_allow_an_earlier_repository() {
    let f = Fake::default();
    let mut d = deps(&f);
    d.repo_names = RepoNames::parse("team-*,hamh-e2e-*").unwrap();
    let target = format!("cc:{EARLIER_DEFAULT_PREFIX}foo");
    let r = handle(admin(ME, json!({"action": "allow", "target": target})), &d).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn a_configured_rule_without_the_default_entry_does_not_gain_the_earlier_prefix() {
    let f = Fake::default();
    let mut d = deps(&f);
    d.repo_names = RepoNames::parse("my-app,team-*").unwrap();
    let target = format!("cc:{EARLIER_DEFAULT_PREFIX}foo");
    let r = handle(admin(ME, json!({"action": "allow", "target": target})), &d).await;
    assert_eq!(r.status, 422, "{}", r.body);
    let e: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        e["message"],
        format!(
            "refused: {target} is outside the repository names this stage allows (my-app, team-*)"
        )
    );
    assert!(f.allow.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_earlier_prefix_look_alike_is_refused_with_the_text_of_the_fixed_prefix() {
    let f = Fake::default();
    let d = deps(&f);
    let name = format!("x{EARLIER_DEFAULT_PREFIX}foo");
    let r = handle(
        admin(
            ME,
            json!({"action": "allow", "target": format!("cc:{name}")}),
        ),
        &d,
    )
    .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let e: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        e["message"],
        format!("refused: cc:{name} is outside the hamh-e2e- prefix")
    );
}

#[tokio::test]
async fn only_an_operator_principal_changes_the_allowlist() {
    let f = Fake::default();
    let r = handle(
        admin(
            NOT_OPERATOR,
            json!({"action": "allow", "target": "cc:hamh-e2e-foo"}),
        ),
        &deps(&f),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
    let e: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(e["code"], "not-an-operator");
    assert_eq!(
        e["message"],
        format!(
            "ham-admin repos needs an operator principal (arn:aws:sts::800000000022:assumed-role/Admin/*); the caller is {NOT_OPERATOR}"
        )
    );
    assert!(f.allow.lock().unwrap().is_empty());
}

#[tokio::test]
async fn operator_patterns_match_exactly_or_across_every_star_in_order() {
    let f = Fake::default();
    let mut d = deps(&f);
    d.operators = vec![
        "arn:aws:sts::800000000022:assumed-role/Ops/alice".into(),
        "arn:aws:sts::800000000022:assumed-role/*/break-glass-*".into(),
    ];
    for (i, (caller, status)) in [
        // A pattern without `*` is the whole ARN.
        ("arn:aws:sts::800000000022:assumed-role/Ops/alice", 200),
        ("arn:aws:sts::800000000022:assumed-role/Ops/alice2", 403),
        // Every literal between stars appears, in order, after the first.
        (
            "arn:aws:sts::800000000022:assumed-role/Admin/break-glass-testuser",
            200,
        ),
        ("arn:aws:sts::800000000022:assumed-role/break-glass-x", 403),
        ("arn:aws:sts::800000000022:assumed-role/Admin/testuser", 403),
        (
            "arn:aws:sts::111111111111:assumed-role/Admin/break-glass-x",
            403,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let target = format!("cc:hamh-e2e-op{i}");
        let r = handle(
            admin(caller, json!({"action": "allow", "target": target})),
            &d,
        )
        .await;
        assert_eq!(r.status, status, "{caller}: {}", r.body);
    }
    let allowed: Vec<String> = f.allow.lock().unwrap().keys().cloned().collect();
    assert_eq!(allowed, ["cc:hamh-e2e-op0", "cc:hamh-e2e-op2"]);
}

#[tokio::test]
async fn allow_refuses_what_repo_gateway_would_refuse_with_the_exact_reason() {
    let f = Fake::default();
    let d = deps(&f);
    for (body, reason) in [(
        json!({"action": "allow", "target": "cc:python-calc"}),
        "refused: cc:python-calc is outside the hamh-e2e- prefix",
    )] {
        let r = handle(admin(ME, body.clone()), &d).await;
        assert_eq!(r.status, 422, "{body}: {}", r.body);
        let e: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(e["code"], "refused", "{body}");
        assert_eq!(e["message"], reason, "{body}");
    }
    assert!(f.allow.lock().unwrap().is_empty());
}

#[tokio::test]
async fn allow_singles_out_no_organization_and_ignores_a_retired_review_record_field() {
    // The operator decides which repositories are allowed: an organization is not refused for its name, and a request that still carries the
    // retired `audrecRecordId` member is accepted without reading it: it is neither checked nor kept.
    let f = Fake::default();
    let d = deps(&f);
    for target in ["gh:amzn/app", "gh:aws-samples/demo", "gh:someone/else"] {
        let r = handle(admin(ME, json!({"action": "allow", "target": target})), &d).await;
        assert_eq!(r.status, 200, "{target}: {}", r.body);
    }
    let r = handle(
        admin(
            ME,
            json!({"action": "allow", "target": "cc:hamh-e2e-foo", "audrecRecordId": "x".repeat(65)}),
        ),
        &d,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    {
        let kept = f.allow.lock().unwrap();
        assert_eq!(kept.len(), 4);
    }
    let listed = handle(req("GET", "/v1/repos", Some(ME), &[], &[], ""), &d).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(
        !listed.body.contains("audrec"),
        "the list answers no review record: {}",
        listed.body
    );
}

#[tokio::test]
async fn admin_requests_that_are_not_valid_are_400_and_other_methods_405() {
    let f = Fake::default();
    let d = deps(&f);
    for bad in [
        "{",
        r#"{"action": "merge", "target": "cc:hamh-e2e-x"}"#,
        r#"{"action": "allow", "target": "cc:hamh-e2e-x", "extra": 1}"#,
    ] {
        let r = handle(req("POST", "/v1/admin/repos", Some(ME), &[], &[], bad), &d).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    let r = handle(req("PUT", "/v1/admin/repos", Some(ME), &[], &[], "{}"), &d).await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn an_unavailable_allowlist_is_a_503() {
    let f = Fake {
        down: true,
        ..Default::default()
    };
    let d = deps(&f);
    assert_eq!(
        handle(req("GET", "/v1/repos", Some(ME), &[], &[], ""), &d)
            .await
            .status,
        503
    );
    assert_eq!(
        handle(
            admin(ME, json!({"action": "deny", "target": "cc:hamh-e2e-foo"})),
            &d
        )
        .await
        .status,
        503
    );
}

// ---- the deployed engine set at StartRun -------------------------------
//
// `ENGINE_SET` is the engines a stage ships. StartRun refuses, before anything is written, a request whose explicit models
// would start a session on another engine: the newest policy's route for a default roster role, else the run's engine.
// Default models and custom modes are resolved by run-tasks, which refuses them again at admission (orchestrator tests).

// ---- message control on a store that keeps no ledger (the defaults of the Store trait) ---------------------------------------

#[tokio::test]
async fn a_store_that_keeps_no_ledger_answers_the_message_reads_and_withdrawals_with_503() {
    let f = Fake::default();
    let id = started_run(&f, "k-msg").await;
    let message = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
    for (method, path) in [
        ("GET", format!("/v1/runs/{id}/messages")),
        ("GET", format!("/v1/runs/{id}/messages/{message}")),
        ("POST", format!("/v1/runs/{id}/messages/{message}/withdraw")),
    ] {
        let out = handle(req(method, &path, Some(ME), &[], &[], ""), &deps(&f)).await;
        assert_eq!(out.status, 503, "{method} {path}: {}", out.body);
    }
    // the same routes for somebody else's run are 404, as for every run route
    for (method, path) in [
        ("GET", format!("/v1/runs/{id}/messages")),
        ("POST", format!("/v1/runs/{id}/messages/{message}/withdraw")),
    ] {
        let out = handle(req(method, &path, Some(OTHER), &[], &[], ""), &deps(&f)).await;
        assert_eq!(out.status, 404, "{method} {path}: {}", out.body);
    }
}

// ----: every run-scoped route refuses a caller who does not own the run ----------------------------------------------------------------

/// One request of the walk. `route` is the route of the handler's `match` it exercises, spelled `METHOD /path` with `{id}`, `{art}` and `{message}` for
/// the path variables; `label` also says the query or the body when they matter.
struct WalkRow {
    route: &'static str,
    label: &'static str,
    method: &'static str,
    path: String,
    headers: Vec<(&'static str, &'static str)>,
    query: Vec<(&'static str, &'static str)>,
    body: String,
}

fn walk_rows(id: &str, art: &str, msg: &str) -> Vec<WalkRow> {
    let row = |route: &'static str,
               label: &'static str,
               method: &'static str,
               path: String,
               headers: Vec<(&'static str, &'static str)>,
               query: Vec<(&'static str, &'static str)>,
               body: String| WalkRow {
        route,
        label,
        method,
        path,
        headers,
        query,
        body,
    };
    let mut cont: Value = serde_json::from_str(&start_body()).unwrap();
    cont["continueFrom"] = json!({ "runId": id });
    let get = |route: &'static str, path: String, query: Vec<(&'static str, &'static str)>| {
        row(route, route, "GET", path, vec![], query, String::new())
    };
    vec![
        get("GET /v1/runs/{id}", format!("/v1/runs/{id}"), vec![]),
        get("GET /v1/runs/{id}/events", format!("/v1/runs/{id}/events"), vec![("shard", "00")]),
        get("GET /v1/runs/{id}/transcript", format!("/v1/runs/{id}/transcript"), vec![]),
        get("GET /v1/runs/{id}/head", format!("/v1/runs/{id}/head"), vec![]),
        row(
            "POST /v1/runs/{id}/retry",
            "POST /v1/runs/{id}/retry",
            "POST",
            format!("/v1/runs/{id}/retry"),
            vec![("Idempotency-Key", "walk-retry-1")],
            vec![],
            String::new(),
        ),
        row(
            "POST /v1/runs/{id}/resume",
            "POST /v1/runs/{id}/resume",
            "POST",
            format!("/v1/runs/{id}/resume"),
            vec![("Idempotency-Key", "walk-resume-1")],
            vec![],
            String::new(),
        ),
        row(
            "POST /v1/runs/{id}/snapshot/download-url",
            "POST /v1/runs/{id}/snapshot/download-url",
            "POST",
            format!("/v1/runs/{id}/snapshot/download-url"),
            vec![],
            vec![],
            String::new(),
        ),
        get("GET /v1/runs/{id}/artifacts", format!("/v1/runs/{id}/artifacts"), vec![]),
        row(
            "POST /v1/runs/{id}/artifacts/{art}/download-url",
            "POST /v1/runs/{id}/artifacts/{art}/download-url",
            "POST",
            format!("/v1/runs/{id}/artifacts/{art}/download-url"),
            vec![],
            vec![],
            String::new(),
        ),
        row(
            "POST /v1/runs/{id}/cancel",
            "POST /v1/runs/{id}/cancel",
            "POST",
            format!("/v1/runs/{id}/cancel"),
            vec![],
            vec![],
            String::new(),
        ),
        row(
            "POST /v1/runs/{id}/cancel",
            "POST /v1/runs/{id}/cancel?discard=true",
            "POST",
            format!("/v1/runs/{id}/cancel"),
            vec![],
            vec![("discard", "true")],
            String::new(),
        ),
        row(
            "POST /v1/runs/{id}/input",
            "POST /v1/runs/{id}/input",
            "POST",
            format!("/v1/runs/{id}/input"),
            vec![],
            vec![],
            json!({"requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT1P", "action": "continue_with_prompt", "text": "go on"}).to_string(),
        ),
        row(
            "POST /v1/runs/{id}/messages",
            "POST /v1/runs/{id}/messages",
            "POST",
            format!("/v1/runs/{id}/messages"),
            vec![],
            vec![],
            json!({"messageId": msg, "text": "please also add docs"}).to_string(),
        ),
        get("GET /v1/runs/{id}/messages", format!("/v1/runs/{id}/messages"), vec![]),
        get("GET /v1/runs/{id}/messages/{message}", format!("/v1/runs/{id}/messages/{msg}"), vec![]),
        row(
            "POST /v1/runs/{id}/messages/{message}/withdraw",
            "POST /v1/runs/{id}/messages/{message}/withdraw",
            "POST",
            format!("/v1/runs/{id}/messages/{msg}/withdraw"),
            vec![],
            vec![],
            String::new(),
        ),
        row(
            "POST /v1/tags/edit",
            "POST /v1/tags/edit",
            "POST",
            "/v1/tags/edit".to_string(),
            vec![],
            vec![],
            json!({"edits": [{"runId": id, "add": ["topic:walk"]}]}).to_string(),
        ),
        row(
            "POST /v1/direct/runs",
            "POST /v1/direct/runs (continueFrom the run)",
            "POST",
            "/v1/direct/runs".to_string(),
            vec![("Idempotency-Key", "walk-cont-1")],
            vec![],
            cont.to_string(),
        ),
        get("GET /v1/runs", "/v1/runs".to_string(), vec![]),
        get("GET /v1/search", "/v1/search".to_string(), vec![("q", "percent_change")]),
    ]
}

/// The routes of the handler's `match` as `METHOD /path` strings, read from the source text of `src/handler.rs`.
fn route_arms(source: &str) -> Vec<String> {
    let start = source
        .find("match (segments.as_slice(), method) {")
        .expect("the handler has the route match");
    let body = &source[start..];
    let end = body.find("not_allowed(method)").unwrap_or(body.len());
    let mut out = Vec::new();
    for line in body[..end].lines() {
        let Some(rest) = line.trim_start().strip_prefix("([") else {
            continue;
        };
        let Some((segments, tail)) = rest.split_once("], \"") else {
            continue;
        };
        let Some((method, _)) = tail.split_once('"') else {
            continue;
        };
        let path = segments
            .split(',')
            .map(str::trim)
            .map(
                |s| match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                    Some(literal) => literal.to_string(),
                    None => format!("{{{s}}}"),
                },
            )
            .collect::<Vec<_>>()
            .join("/");
        out.push(format!("{method} /{path}"));
    }
    out
}

/// Routes that name no run and read nothing of anyone's: they have no ownership to walk.
const NOT_RUN_SCOPED: [(&str, &str); 6] = [
    (
        "GET /v1/direct/models",
        "the model list is the same for every caller",
    ),
    (
        "GET /v1/models",
        "the model list is the same for every caller",
    ),
    (
        "GET /v1/repos",
        "the repository allowlist is the stage's, the same for every caller",
    ),
    (
        "POST /v1/admin/repos",
        "an operator edits the allowlist; tests/handler.rs holds the operator tests",
    ),
    (
        "GET /v1/whoami",
        "it answers with the caller's own principal",
    ),
    ("POST /v1/runs/index", "it fills the caller's own run index"),
];

/// The routes of `source` that the walk has no row for and that are not listed as run-less.
fn routes_without_a_row(source: &str, rows: &[WalkRow]) -> Vec<String> {
    route_arms(source)
        .into_iter()
        .filter(|a| !NOT_RUN_SCOPED.iter().any(|(r, _)| r == a))
        .filter(|a| !rows.iter().any(|r| r.route == a))
        .collect()
}

async fn walk_send(d: &Deps<&Fake, &Fake, &Fake>, caller: &str, r: &WalkRow) -> (u16, String) {
    let (out, _logs) = handle_logged(
        req(
            r.method,
            &r.path,
            Some(caller),
            &r.headers,
            &r.query,
            &r.body,
        ),
        d,
    )
    .await;
    (out.status, out.body)
}

/// The short word a person would read from an answer: the error `code`, the first tag edit status, or the number of runs in a list.
fn walk_word(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if let Some(c) = v.get("code").and_then(Value::as_str) {
        return format!("{status} {c}");
    }
    if let Some(s) = v["results"][0].get("status").and_then(Value::as_str) {
        return format!("{status} result={s}");
    }
    if let Some(runs) = v.get("runs").and_then(Value::as_array) {
        return format!("{status} runs={}", runs.len());
    }
    if let Some(h) = v.get("hits").and_then(Value::as_array) {
        return format!("{status} hits={}", h.len());
    }
    format!("{status}")
}

#[tokio::test]
async fn b633_every_run_scoped_route_refuses_a_caller_who_does_not_own_the_run() {
    let f = Fake::default();
    let id = started_run(&f, "k-walk").await;
    let missing = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";
    let art = "01K5ZQ4T8M3N6P7R9S0V1W2X42";
    let msg = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
    let zip = s3::deliverable_key(&id, "python-calc", "abc1234");
    add_artifact(&f, artifact(&id, art, ArtifactKind::Zip, &zip));
    let d = deps(&f);
    let mine = walk_rows(&id, art, msg);
    let gone = walk_rows(missing, art, msg);

    // Pass 1: somebody else asks for my run. Pass 2: I ask for a run that does not exist. Pass 3: I ask for my run. The owner's pass comes last because
    // some routes write (cancel, messages, input).
    let mut other = Vec::new();
    for r in &mine {
        other.push(walk_send(&d, OTHER, r).await);
    }
    let mut absent = Vec::new();
    for r in &gone {
        absent.push(walk_send(&d, ME, r).await);
    }
    let mut me = Vec::new();
    for r in &mine {
        me.push(walk_send(&d, ME, r).await);
    }

    let secrets = [
        PROMPT,
        ME,
        zip.as_str(),
        "s3://",
        "abababab",
        "python-calc",
        "percent_change",
        "style.md",
    ];
    let mut failures: Vec<String> = Vec::new();
    for (i, r) in mine.iter().enumerate() {
        let (os, ob) = &other[i];
        let (ms, mb) = &me[i];
        let (gs, gb) = &absent[i];
        let leaked: Vec<&str> = secrets.iter().copied().filter(|s| ob.contains(s)).collect();
        // 1. What the stranger is told.
        let v: Value = serde_json::from_str(ob).unwrap_or(Value::Null);
        let refused = match r.route {
            // documented: the batch is a 200 and the edit's own status is not-found
            "POST /v1/tags/edit" => {
                *os == 200
                    && v["results"].as_array().map(Vec::len) == Some(1)
                    && v["results"][0]["status"] == "not-found"
                    && v["results"][0]["tags"].as_array().is_none_or(Vec::is_empty)
            }
            "GET /v1/runs" => *os == 200 && v["runs"].as_array().is_some_and(Vec::is_empty),
            // A search is partitioned by the caller, not by run id: the stranger gets no hit; a store with no search table answers 503 to both callers.
            "GET /v1/search" => {
                (*os == 200 && v["hits"].as_array().is_some_and(Vec::is_empty)) || *os == 503
            }
            _ => *os == 404 && v["code"] == "not-found",
        };
        if !refused {
            failures.push(format!(
                "OTHER was not refused on {}: {}",
                r.label,
                walk_word(*os, ob)
            ));
        }
        if !leaked.is_empty() {
            failures.push(format!("OTHER body leaks {leaked:?} on {}", r.label));
        }
        // 2. The stranger cannot tell my run from a run that does not exist (same status, same body once the id is masked).
        let mask = |b: &str, run: &str| b.replace(run, "<ID>");
        let run_scoped = !matches!(r.route, "GET /v1/search" | "GET /v1/runs");
        if run_scoped && (os != gs || mask(ob, &id) != mask(gb, missing)) {
            failures.push(format!(
                "OTHER on my run differs from ME on a missing run on {}: {} vs {}",
                r.label,
                walk_word(*os, ob),
                walk_word(*gs, gb)
            ));
        }
        // 3. The trigger: the maintainers' own request on the same route is not the "no run for this caller" refusal.
        if mb.contains("for this caller")
            || (*ms == 404
                && mb.contains("\"not-found\"")
                && r.route != "GET /v1/runs/{id}/messages/{message}")
        {
            failures.push(format!(
                "TRIGGER ABSENT: ME is refused like a stranger on {}: {}",
                r.label,
                walk_word(*ms, mb)
            ));
        }
        if (os, ob) == (ms, mb) && r.route != "GET /v1/search" {
            failures.push(format!(
                "TRIGGER ABSENT: ME and OTHER got the same answer on {}",
                r.label
            ));
        }
    }
    // The data exists for the owner, so an empty answer to the stranger means something.
    let at = |route: &str| mine.iter().position(|r| r.route == route).unwrap();
    let get_run_me = &me[at("GET /v1/runs/{id}")].1;
    assert!(
        get_run_me.contains(PROMPT),
        "the owner reads the objective: {get_run_me}"
    );
    let arts_me = &me[at("GET /v1/runs/{id}/artifacts")].1;
    assert!(
        arts_me.contains(&zip),
        "the owner lists the artifact: {arts_me}"
    );
    let dl = at("POST /v1/runs/{id}/artifacts/{art}/download-url");
    assert!(
        me[dl].1.contains(&zip),
        "the owner gets a presigned URL for the artifact: {}",
        me[dl].1
    );
    assert_eq!(me[dl].0, 200);
    assert_eq!(
        f.presigned.lock().unwrap().len(),
        1,
        "only the owner's download-url was presigned"
    );
    assert!(
        me[at("GET /v1/runs")].1.contains(&id),
        "the owner's list holds the run"
    );
    // Negative control: the leak detector fires on the maintainers' own answers, so no leak for the stranger is a real absence.
    let flagged = |body: &str| {
        secrets
            .iter()
            .copied()
            .filter(|s| body.contains(s))
            .collect::<Vec<_>>()
    };
    let nc_run = flagged(get_run_me);
    let nc_art = flagged(arts_me);
    assert!(
        nc_run.contains(&PROMPT) && nc_run.contains(&ME),
        "{nc_run:?}"
    );
    assert!(
        nc_art.contains(&"s3://") && nc_art.contains(&"abababab"),
        "{nc_art:?}"
    );
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn b633_the_walk_has_a_row_for_every_route_of_the_handler_that_names_a_run() {
    let source = include_str!("../src/handler.rs");
    let rows = walk_rows("01K5ZQ4T8M3N6P7R9S0V1W2X4Y", "art", "msg");
    // TRIGGER PRESENT: the reader finds the routes at all.
    let arms = route_arms(source);
    assert!(arms.len() >= 20, "{arms:?}");
    assert!(
        arms.contains(&"GET /v1/runs/{id}/events".to_string()),
        "{arms:?}"
    );
    assert_eq!(routes_without_a_row(source, &rows), Vec::<String>::new());
    // No row for a route that no longer exists.
    for r in &rows {
        assert!(
            arms.iter().any(|a| a == r.route),
            "a row for a route the handler does not have: {}",
            r.route
        );
    }
    for (route, reason) in NOT_RUN_SCOPED {
        assert!(
            arms.iter().any(|a| a == route),
            "{route} ({reason}) is no route of the handler any more"
        );
    }
}

#[test]
fn b633_a_route_added_to_the_handler_without_a_row_fails_the_walk() {
    let source = include_str!("../src/handler.rs");
    let rows = walk_rows("01K5ZQ4T8M3N6P7R9S0V1W2X4Y", "art", "msg");
    // A scratch copy of the handler with one more run-scoped route in its match.
    let marker = "        ([\"v1\", \"models\"], \"GET\") => models(),";
    assert!(
        source.contains(marker),
        "the marker arm moved; update this test"
    );
    let scratch = source.replacen(
        marker,
        &format!("        ([\"v1\", \"runs\", id, \"export\"], \"GET\") => export(id),\n{marker}"),
        1,
    );
    assert_eq!(
        routes_without_a_row(&scratch, &rows),
        vec!["GET /v1/runs/{id}/export".to_string()]
    );
    // And one with no run in it is a decision written into the list, not a silent pass.
    let runless = source.replacen(
        marker,
        &format!("        ([\"v1\", \"ping\"], \"GET\") => ping(),\n{marker}"),
        1,
    );
    assert_eq!(
        routes_without_a_row(&runless, &rows),
        vec!["GET /v1/ping".to_string()]
    );
}
