//! A local `/v1` API for the `ham` tests: an axum server on 127.0.0.1 that verifies each request's
//! SigV4 signature (service `execute-api`, recomputed with aws-sigv4 from the bytes received), maps
//! the access key to an IAM caller, strips the stage, and answers through the real
//! `control_api::handler` over an in-memory store (stage `direct` once a test sets `Inner::stage`). Presigned download URLs point
//! at `/objects/<key>` on the same server, which answers
//! ranged GETs like S3. Tests play the harness by appending events, artifacts and objects.
#![allow(dead_code, clippy::unwrap_used)]

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use control_api::handler::{
    Blobs, Deps, Execution, RunIndexItem, RunUsage, Store, StoreError, V1Request, handle,
};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::DownloadUrlResponse;
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    InstructAction, NeedsInputEvidence, NeedsInputModel, NeedsInputPayload, PROGRESS_INPUT_CLOSED,
    PROGRESS_INPUT_IGNORED, PROGRESS_INPUT_RECEIVED, PROGRESS_NEEDS_INPUT, PROGRESS_NEW_SESSION,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::{
    AgentItem, ArtifactItem, ArtifactKind, ControlItem, GoalItem, IdempotencyItem, Keyed,
    RepoAllowItem, RunItem, TurnItem, event_shard, s3,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The fake control plane's Region: the home Region of the `accounts.json` entry the build
/// selected, where the API, the catalog rows and the pools are.
pub const REGION: &str = harness_protocol::target::E2E_REGION;
/// The runtime ARN the fake's Agent items name.
pub const RUNTIME_ARN: &str =
    "arn:aws:bedrock-agentcore:us-west-2:800000000022:runtime/hamh_live_worker-abc123";
pub const STAGE: &str = "live";
pub const BUCKET: &str = "hamh-live-harness-800000000022";
/// Test-only identity: not an AWS key.
pub const AKID: &str = "AKIDTESTONLY";
pub const SECRET: &str = "test-secret-not-a-key";
pub const TOKEN: &str = "test-session-token-value";
pub const ME: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
/// The model of a direct run that sets none: Claude Sonnet 5.5, the default of `engine/catalog/models.json`.
pub const MODEL: &str = "global.anthropic.claude-sonnet-5-5";

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub fn creds() -> Credentials {
    Credentials::new(AKID, SECRET, Some(TOKEN.into()), None, "test")
}

/// A fresh directory under the workspace temp dir (`TMPDIR`).
pub fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ham-test-{label}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One request as the server saw it (names lowercase; no body kept except for `POST /v1/runs`).
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub caller: Option<String>,
    /// SigV4 verification result for `/v1` calls.
    pub signature: Result<(), String>,
    /// When the request reached the server, in Unix milliseconds: a test that waits for the effect of a request can say how long after its key
    /// the request came (FLK1).
    pub at_ms: u64,
}

/// Unix milliseconds now.
fn arrived_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Default)]
pub struct Inner {
    pub idem: BTreeMap<String, IdempotencyItem>,
    pub runs: BTreeMap<String, RunItem>,
    pub goals: BTreeMap<String, GoalItem>,
    pub events: Vec<Envelope>,
    pub blobs: BTreeMap<String, Vec<u8>>,
    pub started: BTreeMap<String, Value>,
    pub artifacts: BTreeMap<(String, String), ArtifactItem>,
    /// S3 objects by key: bytes and ETag.
    pub objects: BTreeMap<String, (Vec<u8>, String)>,
    pub seen: Vec<Seen>,
    pub presigned: Vec<(String, Duration)>,
    /// Object ranges served: (key, start, end).
    pub ranges: Vec<(String, u64, u64)>,
    /// When set, runs finish as soon as they are created (the binary tests' harness).
    pub autopilot: bool,
    /// The deliverable zip `complete_run` stores; `zip_bytes(3000)` when unset.
    pub deliverable: Option<Vec<u8>>,
    /// The controls answers queued.
    pub controls: Vec<ControlItem>,
    /// The repository allowlist by target (`ham doctor`, `ham-admin repos`).
    pub allow: BTreeMap<String, RepoAllowItem>,
    /// Allowlist reads and writes fail.
    pub allow_down: bool,
    /// The repository of the PR link the autopilot announces; `hamh-e2e-python-calc` when unset.
    pub pr_repo: Option<String>,
    /// `complete_run` announces no PR (an `--s3-only` run).
    pub no_pr: bool,
    /// Agent items (steer, cancel).
    pub agent_items: Vec<AgentItem>,
    /// Run executions stopped: (name, cause).
    pub stopped: Vec<(String, String)>,
    /// Presigned URLs expire no later than this Unix second: the signer's session lapses first.
    pub signer_expiry: Option<u64>,
    /// Ranged object GETs other than the probe wait this long (a slow download to kill).
    pub range_delay: Duration,
    /// The engine-reported usage a direct run's one turn recorded, with the model it ran on, by run id
    /// ([`Mem::record_direct_usage`]): what `GET /v1/runs/{id}` prices into `cost`.
    pub direct_usage: BTreeMap<String, (String, Usage)>,
    /// When set, a queued `Instruct` control is taken at once by the simulated agentd: the control is marked delivered and an
    /// `input-received` event of the direct agent follows. Unset, tests append the acknowledgement themselves.
    pub auto_ack_input: bool,
    /// The run index: search items by (owner ARN, sort key).
    pub index: BTreeMap<(String, String), RunIndexItem>,
    /// The chat-content search items of the owners as the store would hand them to the search route: by (owner partition key, sort
    /// key `SRCH#<createdAt>#<runId>`), each the JSON of a `ham_core::runsearch::SearchItem` (body, segments and coverage joined).
    pub search: BTreeMap<(String, String), Value>,
}

/// A query component with its `%XX` escapes decoded, as API Gateway hands it to the function (a client sends a space as `%20`); a sequence that is
/// not an escape stays as it is.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The ETag the stand-in gives a blob: S3 sends it quoted.
fn etag_of(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    format!(
        "\"{}\"",
        d.iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// The in-memory store, blobs and execution behind the real handler.
#[derive(Clone, Default)]
pub struct Mem {
    pub inner: Arc<Mutex<Inner>>,
    /// The message ledger's table: the real ledger over the in-memory table, shared with a simulated agentd.
    pub msgs: message_ledger::mem::MemTable,
    pub base: Arc<Mutex<String>>,
    counter: Arc<AtomicU64>,
    last_ms: Arc<Mutex<u128>>,
}

impl Store for Mem {
    /// What the transcript route asks the store: the run's direct Agent item registered `transcript.v1` (its image writes a stored
    /// conversation). No worker reports a completeness summary here, so none is recorded.
    async fn transcript_facts(
        &self,
        run_id: &str,
    ) -> Result<control_api::transcript::TranscriptFacts, StoreError> {
        let supported = self.inner.lock().unwrap().agent_items.iter().any(|a| {
            a.run_id == run_id
                && a.agent_id == harness_protocol::direct::DIRECT_AGENT_ID
                && a.features
                    .iter()
                    .any(|f| f == harness_protocol::transcript::TRANSCRIPT_FEATURE)
        });
        Ok(control_api::transcript::TranscriptFacts {
            supported,
            purged: false,
            summary: control_api::transcript::SummaryFact::Absent,
        })
    }
    /// One read of an owner's search items, newest first, as the Dynamo store answers it: the sort keys from `sk_floor` up, after
    /// `sk_after` when set, at most `limit` of them, and `next_sk` where a search that goes on goes on.
    async fn search_items(
        &self,
        owner_pk: &str,
        sk_floor: &str,
        sk_after: Option<&str>,
        limit: usize,
    ) -> Result<control_api::search::SearchPage, StoreError> {
        let g = self.inner.lock().unwrap();
        let mut rows: Vec<(&String, &Value)> = g
            .search
            .iter()
            .filter(|((o, sk), _)| {
                o == owner_pk && sk.as_str() >= sk_floor && sk_after.is_none_or(|a| sk.as_str() < a)
            })
            .map(|((_, sk), v)| (sk, v))
            .collect();
        rows.sort_by(|a, b| b.0.cmp(a.0));
        let more = rows.len() > limit;
        rows.truncate(limit);
        let last_sk = rows.last().map(|(sk, _)| (*sk).clone());
        let items = rows
            .iter()
            .filter_map(|(_, v)| serde_json::from_value((*v).clone()).ok())
            .collect();
        Ok(control_api::search::SearchPage {
            items,
            examined: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            next_sk: if more { last_sk.clone() } else { None },
            last_sk,
        })
    }
    async fn message_admit(
        &self,
        run_id: &str,
        principal: &str,
        req: &harness_protocol::messages::SendMessageRequest,
    ) -> Result<message_ledger::Admitted, message_ledger::LedgerError> {
        message_ledger::Ledger::new(self.msgs.clone())
            .admit(run_id, principal, req)
            .await
    }
    async fn message_get(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<Option<harness_protocol::messages::MessageView>, message_ledger::LedgerError> {
        message_ledger::Ledger::new(self.msgs.clone())
            .get_message(run_id, message_id)
            .await
    }
    async fn message_list(
        &self,
        run_id: &str,
    ) -> Result<harness_protocol::messages::MessagesPage, message_ledger::LedgerError> {
        message_ledger::Ledger::new(self.msgs.clone())
            .list(run_id)
            .await
    }
    async fn message_withdraw(
        &self,
        run_id: &str,
        message_id: &str,
    ) -> Result<harness_protocol::messages::MessageView, message_ledger::LedgerError> {
        message_ledger::Ledger::new(self.msgs.clone())
            .withdraw(run_id, message_id)
            .await
    }
    async fn message_close(
        &self,
        run_id: &str,
        reason: message_ledger::CloseReason,
    ) -> Result<message_ledger::Closed, message_ledger::LedgerError> {
        message_ledger::Ledger::new(self.msgs.clone())
            .close(run_id, reason)
            .await
    }
    async fn repo_allow_list(&self) -> Result<Vec<RepoAllowItem>, StoreError> {
        let g = self.inner.lock().unwrap();
        if g.allow_down {
            return Err(StoreError::Unavailable("table unreachable".into()));
        }
        Ok(g.allow.values().cloned().collect())
    }
    async fn repo_allow_put(&self, item: &RepoAllowItem) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.allow_down {
            return Err(StoreError::Unavailable("table unreachable".into()));
        }
        g.allow.insert(item.target.to_string(), item.clone());
        Ok(())
    }
    async fn repo_allow_delete(
        &self,
        target: &harness_protocol::target::RepoTarget,
    ) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.allow_down {
            return Err(StoreError::Unavailable("table unreachable".into()));
        }
        g.allow.remove(&target.to_string());
        Ok(())
    }
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        let mut g = self.inner.lock().unwrap();
        Ok(g.idem
            .entry(c.key().pk)
            .or_insert_with(|| c.clone())
            .clone())
    }
    async fn create_run(
        &self,
        run: &RunItem,
        goal: &GoalItem,
        created: &Envelope,
    ) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        if g.runs.contains_key(&run.run_id) {
            return Err(StoreError::Conflict);
        }
        g.runs.insert(run.run_id.clone(), run.clone());
        g.goals.insert(run.run_id.clone(), goal.clone());
        g.events.push(created.clone());
        Ok(())
    }
    async fn get_run(&self, run_id: &str) -> Result<Option<RunItem>, StoreError> {
        Ok(self.inner.lock().unwrap().runs.get(run_id).cloned())
    }
    async fn get_root_goal(&self, run_id: &str) -> Result<Option<GoalItem>, StoreError> {
        Ok(self.inner.lock().unwrap().goals.get(run_id).cloned())
    }
    async fn list_runs(
        &self,
        owner: &str,
        _next: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        let mut out: Vec<RunItem> = self
            .inner
            .lock()
            .unwrap()
            .runs
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
        let mut out: Vec<Envelope> = self
            .inner
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .filter(|e| after.is_none_or(|a| e.event_id.as_str() > a))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        out.truncate(limit);
        // The journal of what was served, for a failing test to print: only the reads that returned something (one line each).
        if !out.is_empty() {
            let ids: Vec<String> = out
                .iter()
                .map(|e| format!("{}:{:?}", tail_of(&e.event_id), e.event_type))
                .collect();
            SERVED.lock().unwrap().push(format!(
                "{} {run_id} shard={shard} after={:?} -> {}",
                now_millis(),
                after.map(tail_of),
                ids.join(" ")
            ));
        }
        Ok(out)
    }
    async fn list_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .artifacts
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
        Ok(self
            .inner
            .lock()
            .unwrap()
            .artifacts
            .get(&(run_id.to_string(), artifact_id.to_string()))
            .cloned())
    }
    async fn put_control(&self, control: &ControlItem) -> Result<(), StoreError> {
        let ack = {
            let mut g = self.inner.lock().unwrap();
            // The real table puts conditionally (`attribute_not_exists`): one control per id.
            if g.controls.iter().any(|c| {
                c.run_id == control.run_id
                    && c.agent_id == control.agent_id
                    && c.control_id == control.control_id
            }) {
                return Err(StoreError::Conflict);
            }
            let mut item = control.clone();
            let ack = match (&control.control, g.auto_ack_input) {
                (
                    PendingControl::Instruct {
                        request_id,
                        action,
                        text,
                        model,
                        ..
                    },
                    true,
                ) => {
                    item.delivered_at = Some(control_api::ids::rfc3339(unix_now()));
                    Some((
                        request_id.clone(),
                        *action,
                        text.as_deref().map_or(0, str::len),
                        model.clone(),
                    ))
                }
                _ => None,
            };
            g.controls.push(item);
            ack
        };
        if let Some((request_id, action, text_bytes, model)) = ack {
            let mut v = json!({"action": action, "origin": "user", "textBytes": text_bytes, "waitedS": 1,
                               "text": "Your message reached Cline (same session)."});
            if let Some(m) = model {
                v["model"] = json!(m);
            }
            self.input_event(&control.run_id, PROGRESS_INPUT_RECEIVED, &request_id, v);
            // agentd closes a question that is answered with finish; the other answers close it by being received.
            if action == InstructAction::Finish {
                self.input_event(
                    &control.run_id,
                    PROGRESS_INPUT_CLOSED,
                    &request_id,
                    json!({"how": "finish", "waitedS": 1}),
                );
            }
        }
        Ok(())
    }
    async fn get_event(
        &self,
        run_id: &str,
        shard: u8,
        event_id: &str,
    ) -> Result<Option<Envelope>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .events
            .iter()
            .find(|e| {
                e.run_id == run_id
                    && e.event_id == event_id
                    && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .cloned())
    }
    async fn get_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<Option<ControlItem>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .controls
            .iter()
            .find(|c| c.run_id == run_id && c.agent_id == agent_id && c.control_id == control_id)
            .cloned())
    }
    async fn rearm_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().unwrap();
        Ok(g.controls
            .iter_mut()
            .find(|c| c.run_id == run_id && c.agent_id == agent_id && c.control_id == control_id)
            .is_some_and(|c| c.delivered_at.take().is_some()))
    }
    async fn latest_events(
        &self,
        run_id: &str,
        shard: u8,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        let mut out: Vec<Envelope> = self
            .inner
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| b.event_id.cmp(&a.event_id));
        out.truncate(limit);
        out.reverse();
        Ok(out)
    }
    async fn run_usage(&self, run_id: &str) -> Result<RunUsage, StoreError> {
        let Some((model, usage)) = self.inner.lock().unwrap().direct_usage.get(run_id).cloned()
        else {
            return Ok(RunUsage::default());
        };
        let agent: AgentItem = serde_json::from_value(json!({
            "runId": run_id, "agentId": "direct", "taskId": "root", "attempt": 1,
            "runtimeArn": RUNTIME_ARN, "runtimeSessionId": format!("{run_id}.direct.g1-session-0000000"),
            "generation": 1, "engine": "cline", "engineVersion": "3.0.71", "transport": "acp",
            "modelId": model, "pool": format!("{REGION}#{model}"), "status": "stopped", "version": 1
        }))
        .unwrap();
        let turn: TurnItem = serde_json::from_value(json!({
            "runId": run_id, "agentId": "direct", "n": 1, "taskTokenCiphertext": "x",
            "startedAt": "2026-10-14T20:00:00Z", "endedAt": "2026-10-14T20:30:00Z",
            "usage": usage, "rulesDigest": "d", "version": 1
        }))
        .unwrap();
        Ok(RunUsage {
            agents: vec![agent],
            turns: vec![turn],
        })
    }
    async fn list_agents(&self, run_id: &str) -> Result<Vec<AgentItem>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .agent_items
            .iter()
            .filter(|a| a.run_id == run_id)
            .cloned()
            .collect())
    }
    /// A Query of `CTRL#<agent>#` on the run partition: at most `limit` controls in id order and whether more were there.
    async fn list_controls(
        &self,
        run_id: &str,
        agent_id: &str,
        oldest: &str,
        newest: &str,
        limit: usize,
    ) -> Result<(Vec<ControlItem>, bool), StoreError> {
        assert!(limit >= 1, "Query Limit must be at least 1");
        let mut all: Vec<ControlItem> = self
            .inner
            .lock()
            .unwrap()
            .controls
            .iter()
            .filter(|c| c.run_id == run_id && c.agent_id == agent_id)
            .filter(|c| c.control_id.as_str() >= oldest && c.control_id.as_str() <= newest)
            .cloned()
            .collect();
        all.sort_by(|a, b| a.control_id.cmp(&b.control_id));
        let more = all.len() > limit;
        all.truncate(limit);
        Ok((all, more))
    }
    /// The newest `limit` events of a shard below `before`, oldest first. A DynamoDB Query needs `Limit` of at least 1.
    async fn events_before(
        &self,
        run_id: &str,
        shard: u8,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        assert!(limit >= 1, "Query Limit must be at least 1");
        let mut all: Vec<Envelope> = self
            .inner
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .filter(|e| before.is_none_or(|b| e.event_id.as_str() < b))
            .cloned()
            .collect();
        all.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        let skip = all.len().saturating_sub(limit);
        Ok(all.split_off(skip))
    }
    async fn run_index_between(
        &self,
        owner: &str,
        oldest: &str,
        newest: &str,
    ) -> Result<Vec<RunIndexItem>, StoreError> {
        assert!(oldest <= newest, "BETWEEN needs lower <= upper");
        let (lo, hi) = (format!("SRCH#{oldest}"), format!("SRCH#{newest}#~"));
        Ok(self
            .inner
            .lock()
            .unwrap()
            .index
            .iter()
            .filter(|((o, sk), _)| {
                o == owner && sk.as_str() >= lo.as_str() && sk.as_str() <= hi.as_str()
            })
            .map(|(_, v)| v.clone())
            .collect())
    }
    async fn put_run_index(&self, owner: &str, item: &RunIndexItem) -> Result<bool, StoreError> {
        // The sort key of `ham_core::run_index::search_sk`, written out so that this module needs no `ham-core` (it is also built
        // into `ham-admin`'s tests, which do not depend on it); `run_index_between` above relies on the same `SRCH#` layout.
        let key = (
            owner.to_string(),
            format!("SRCH#{}#{}", item.created_at, item.run_id),
        );
        let mut g = self.inner.lock().unwrap();
        if g.index.contains_key(&key) {
            return Ok(false);
        }
        g.index.insert(key, item.clone());
        Ok(true)
    }
    async fn update_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        event: Option<&Envelope>,
    ) -> Result<(), StoreError> {
        let mut g = self.inner.lock().unwrap();
        let old = g.runs.get(&run.run_id).map(|r| r.version);
        if old.map(|v| v + 1) != Some(run.version) {
            return Err(StoreError::Conflict);
        }
        g.runs.insert(run.run_id.clone(), run.clone());
        if let Some(goal) = goal {
            g.goals.insert(goal.run_id.clone(), goal.clone());
        }
        if let Some(e) = event {
            // The handler makes the id of this event from the real clock, but the events the tests play are one second ahead of it
            // (`Mem::ulid`): it would sort before events a client has already read and never be served (the status event of a cancel that
            // clears a run at once). It is numbered like the played ones.
            let mut e = e.clone();
            e.event_id = self.ulid();
            g.events.push(e);
        }
        Ok(())
    }
}

impl Blobs for Mem {
    /// S3 ListObjectsV2 over the objects and the JSON blobs: the keys under `prefix` after `start_after`, in key order, at most `max_keys`.
    async fn list_objects(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<control_api::handler::ObjectPage, StoreError> {
        assert!((1..=1000).contains(&max_keys), "max-keys is 1 to 1000");
        let g = self.inner.lock().unwrap();
        let mut all: BTreeMap<&String, (u64, String)> = BTreeMap::new();
        for (k, b) in &g.blobs {
            all.insert(k, (b.len() as u64, etag_of(b)));
        }
        for (k, (b, e)) in &g.objects {
            all.insert(k, (b.len() as u64, e.clone()));
        }
        let mut objects: Vec<control_api::handler::ObjectInfo> = all
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix) && start_after.is_none_or(|a| k.as_str() > a))
            .map(|(k, (size, etag))| control_api::handler::ObjectInfo {
                key: k.clone(),
                size,
                etag: Some(etag),
            })
            .collect();
        let truncated = objects.len() > max_keys;
        objects.truncate(max_keys);
        Ok(control_api::handler::ObjectPage { objects, truncated })
    }
    /// One whole object: refused above `max_bytes`, and only while its ETag is `if_match` when that is set. `None`: no such key.
    async fn get_object(
        &self,
        key: &str,
        max_bytes: usize,
        if_match: Option<&str>,
    ) -> Result<Option<Vec<u8>>, control_api::handler::ObjectError> {
        let g = self.inner.lock().unwrap();
        let found = g
            .objects
            .get(key)
            .map(|(b, e)| (b.clone(), e.clone()))
            .or_else(|| g.blobs.get(key).map(|b| (b.clone(), etag_of(b))));
        let Some((bytes, etag)) = found else {
            return Ok(None);
        };
        if bytes.len() > max_bytes {
            return Err(control_api::handler::ObjectError::TooLarge);
        }
        if if_match.is_some_and(|m| m != etag) {
            return Err(control_api::handler::ObjectError::Changed);
        }
        Ok(Some(bytes))
    }
    async fn put(&self, key: &str, body: &[u8]) -> Result<String, StoreError> {
        self.inner
            .lock()
            .unwrap()
            .blobs
            .insert(key.to_string(), body.to_vec());
        Ok(format!("s3://{BUCKET}/{key}"))
    }
    fn bucket(&self) -> &str {
        BUCKET
    }
    /// A ranged GET of the first bytes of an object: absent is `None`; a range of zero bytes is not a range (S3 answers 416).
    async fn read_prefix(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<Option<control_api::handler::ObjectPrefix>, StoreError> {
        assert!(max_bytes >= 1, "a ranged GET needs at least one byte");
        let g = self.inner.lock().unwrap();
        Ok(g.objects.get(key).map(|(bytes, etag)| {
            let take = usize::try_from(max_bytes)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            control_api::handler::ObjectPrefix {
                bytes: bytes[..take].to_vec(),
                size: bytes.len() as u64,
                etag: Some(etag.clone()),
                version_id: None,
                last_modified: Some("2020-01-01T00:00:00Z".to_string()),
            }
        }))
    }
    async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
    ) -> Result<DownloadUrlResponse, StoreError> {
        self.inner
            .lock()
            .unwrap()
            .presigned
            .push((key.to_string(), ttl));
        let base = self.base.lock().unwrap().clone();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cap = self.inner.lock().unwrap().signer_expiry;
        let secs = (now + ttl.as_secs()).min(cap.unwrap_or(u64::MAX));
        let d = control_api::ids::rfc3339(now).replace(['-', ':'], "");
        Ok(DownloadUrlResponse {
            url: format!(
                "{base}/objects/{key}?X-Amz-Date={d}&X-Amz-Expires={}&X-Amz-Security-Token=presign-token&X-Amz-Signature=fake",
                ttl.as_secs()
            ),
            expires_at: control_api::ids::rfc3339(secs),
        })
    }
}

impl Execution for Mem {
    async fn stop(&self, name: &str, cause: &str) -> Result<(), StoreError> {
        self.inner
            .lock()
            .unwrap()
            .stopped
            .push((name.to_string(), cause.to_string()));
        Ok(())
    }
    async fn start(&self, name: &str, input: &Value) -> Result<(), StoreError> {
        // Step Functions keeps one execution per name: only the first start runs the harness.
        let (first, autopilot) = {
            let mut g = self.inner.lock().unwrap();
            let first = !g.started.contains_key(name);
            g.started
                .entry(name.to_string())
                .or_insert_with(|| input.clone());
            (first, g.autopilot)
        };
        if first && autopilot {
            self.complete_run(name, true);
        }
        Ok(())
    }
}

/// What the stand-in served to event reads, oldest first: `<unix ms> <run> shard=<n> after=<tail of the cursor> -> <tail:type> ...`. A test
/// that fails on a missing event prints it (`served_journal`).
pub static SERVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn tail_of(id: &str) -> String {
    id.chars()
        .rev()
        .take(9)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The journal of served event reads as lines of text.
pub fn served_journal() -> String {
    SERVED.lock().unwrap().join("\n")
}

impl Mem {
    /// A ULID that sorts after every earlier one, including the handler's `run.created` id: the
    /// time part is one second ahead of now and the random part is a rising counter.
    pub fn ulid(&self) -> String {
        let n = u128::from(self.counter.fetch_add(1, Ordering::SeqCst));
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 1000;
        let ms = {
            let mut last = self.last_ms.lock().unwrap();
            *last = (*last).max(ms);
            *last
        };
        let v = ((ms & ((1 << 48) - 1)) << 80) | n;
        const C: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        (0..26)
            .map(|k| C[((v >> (125 - 5 * k)) & 31) as usize] as char)
            .collect()
    }

    fn envelope(
        &self,
        run_id: &str,
        t: EventType,
        a: Authority,
        agent: Option<&str>,
        v: Value,
    ) -> Envelope {
        Envelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.ulid(),
            run_id: run_id.into(),
            task_id: None,
            agent_id: agent.map(str::to_string),
            seq: 0,
            ts: "2026-09-25T20:00:00Z".into(),
            event_type: t,
            authority: a,
            payload: Payload::Inline { value: v },
        }
    }

    pub fn harness(&self, run_id: &str, t: EventType, v: Value) {
        let e = self.envelope(run_id, t, Authority::Harness, None, v);
        self.inner.lock().unwrap().events.push(e);
    }

    pub fn agent(&self, run_id: &str, agent: &str, t: EventType, v: Value) {
        let e = self.envelope(run_id, t, Authority::Agent, Some(agent), v);
        self.inner.lock().unwrap().events.push(e);
    }

    /// Stores `bytes` as the run's deliverable zip: object, `ART#` item. Returns the artifact id.
    pub fn deliverable(&self, run_id: &str, bytes: Vec<u8>) -> (String, String) {
        let key = s3::deliverable_key(run_id, "python-calc", "abc1234");
        self.object(run_id, &key, ArtifactKind::Zip, bytes)
    }

    /// Stores `bytes` as the held run's partial zip, as the packager does: at the partial key, as a `partial` artifact
    /// whose id is `partial-` and the first 20 hex characters of its SHA-256. Returns the artifact id and the SHA-256.
    pub fn partial(&self, run_id: &str, bytes: Vec<u8>) -> (String, String) {
        let key = s3::partial_deliverable_key(run_id, "python-calc", "abc1234");
        let sha = sha256_hex(&bytes);
        let id = format!("partial-{}", &sha[..20]);
        self.object_as(run_id, &id, &key, ArtifactKind::Partial, bytes)
    }

    pub fn object(
        &self,
        run_id: &str,
        key: &str,
        kind: ArtifactKind,
        bytes: Vec<u8>,
    ) -> (String, String) {
        let id = self.ulid();
        self.object_as(run_id, &id, key, kind, bytes)
    }

    fn object_as(
        &self,
        run_id: &str,
        id: &str,
        key: &str,
        kind: ArtifactKind,
        bytes: Vec<u8>,
    ) -> (String, String) {
        let sha = sha256_hex(&bytes);
        let id = id.to_string();
        let mut g = self.inner.lock().unwrap();
        g.artifacts.insert(
            (run_id.to_string(), id.clone()),
            ArtifactItem {
                run_id: run_id.into(),
                artifact_id: id.clone(),
                kind,
                s3_uri: format!("s3://{BUCKET}/{key}"),
                sha256: sha.clone(),
                size_bytes: bytes.len() as u64,
            },
        );
        g.objects
            .insert(key.to_string(), (bytes, format!("\"{}\"", &sha[..32])));
        (id, sha)
    }

    /// The PR link of run `run_id`: in `pr_repo`, else `hamh-e2e-python-calc`.
    pub fn pr_url(&self, run_id: &str) -> String {
        let _ = run_id;
        let repo = self
            .inner
            .lock()
            .unwrap()
            .pr_repo
            .clone()
            .unwrap_or_else(|| "hamh-e2e-python-calc".into());
        format!(
            "https://{REGION}.console.aws.amazon.com/codesuite/codecommit/repositories/{repo}/pull-requests/7/details"
        )
    }

    /// The harness side of a finished run of the direct line: one `direct` session, the pull request, the zip,
    /// `complete` and `run.completed` with the label "Cline reported done", as run-session's `finalize` writes them. The run's zip is
    /// the one `deliverable` holds, else `zip_bytes(3000)`; `no_pr` leaves the pull request out. Returns the artifact id and SHA-256 of the zip.
    pub fn complete_run(&self, run_id: &str, with_zip: bool) -> Option<(String, String)> {
        self.finish_direct_run(
            run_id,
            MODEL,
            harness_protocol::direct::DirectOutcome::ReportedDone,
            None,
            with_zip,
        )
    }

    /// The harness side of a finished direct run: [`Mem::complete_run`] with the outcome `run.completed` carries
    /// (`outcome`, `label` and `detail`). Returns the artifact id and SHA-256 of the zip.
    pub fn complete_direct_run(
        &self,
        run_id: &str,
        outcome: harness_protocol::direct::DirectOutcome,
        detail: Option<&str>,
    ) -> (String, String) {
        self.complete_direct_run_on(run_id, MODEL, outcome, detail)
    }

    /// [`Mem::complete_direct_run`] for a run whose session used `model_id`.
    pub fn complete_direct_run_on(
        &self,
        run_id: &str,
        model_id: &str,
        outcome: harness_protocol::direct::DirectOutcome,
        detail: Option<&str>,
    ) -> (String, String) {
        self.finish_direct_run(run_id, model_id, outcome, detail, true)
            .unwrap_or_default()
    }

    fn finish_direct_run(
        &self,
        run_id: &str,
        model_id: &str,
        outcome: harness_protocol::direct::DirectOutcome,
        detail: Option<&str>,
        with_zip: bool,
    ) -> Option<(String, String)> {
        let (no_pr, fixed) = {
            let g = self.inner.lock().unwrap();
            (g.no_pr, g.deliverable.clone())
        };
        self.harness(
            run_id,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": model_id}),
        );
        self.harness(
            run_id,
            EventType::GoalStatus,
            json!({"status": "active", "phase": "green"}),
        );
        self.agent(run_id, "direct", EventType::TurnStarted, json!({"turn": 1}));
        self.agent(
            run_id,
            "direct",
            EventType::TurnEnded,
            json!({"turn": 1, "summary": "added percent_change\u{1b}]0;sentinel\u{7} with tests"}),
        );
        let pr = self.pr_url(run_id);
        if !no_pr {
            self.harness(run_id, EventType::DeliveryPr, json!({"url": pr}));
        }
        let bytes = fixed.unwrap_or_else(|| zip_bytes(3000));
        let size = bytes.len();
        let zip = with_zip.then(|| self.deliverable(run_id, bytes));
        self.harness(
            run_id,
            EventType::GoalStatus,
            json!({"status": "complete", "phase": "delivering"}),
        );
        if let Some((id, sha)) = &zip {
            let mut p = json!({"artifactId": id, "sha256": sha, "sizeBytes": size});
            if !no_pr {
                p["prUrl"] = json!(pr);
            }
            self.harness(run_id, EventType::DeliveryReady, p);
        }
        let mut done =
            json!({"status": "complete", "outcome": outcome.as_str(), "label": outcome.label()});
        if let Some(d) = detail {
            done["detail"] = json!(d);
        }
        self.harness(run_id, EventType::RunCompleted, done);
        zip
    }

    /// The `needs-input` question of the simulated agentd for run `run_id`: the typed payload of the golden example, reason
    /// `reason`, the four options and two routes, a 30 minute window from now. The event id is the request id, which is returned.
    pub fn needs_input(&self, run_id: &str, reason: &str) -> String {
        self.needs_input_with(run_id, reason, |_| {})
    }

    /// [`Mem::needs_input`] with `edit` applied to the payload before it is stored.
    pub fn needs_input_with(
        &self,
        run_id: &str,
        reason: &str,
        edit: impl FnOnce(&mut NeedsInputPayload),
    ) -> String {
        let request_id = self.ulid();
        let n = 1 + self
            .inner
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| e.run_id == run_id && payload_kind(e) == Some(PROGRESS_NEEDS_INPUT))
            .count() as u32;
        let mut p = NeedsInputPayload {
            turn: 1,
            kind: PROGRESS_NEEDS_INPUT.into(),
            text: "Needs your input: Cline stopped its own prompt (ham say, ham new-session or ham finish)".into(),
            request_id: request_id.clone(),
            n,
            reason: reason.into(),
            title: "Cline stopped its own prompt".into(),
            stop_outcome: Some("stopped_engine_cancelled".into()),
            evidence: NeedsInputEvidence {
                attempts: 2,
                tool_calls: 41,
                files_changed: 3,
                identical_calls: Some(5),
                failed_turns: None,
                last_error: Some("the engine ended the prompt cancelled".into()),
                run_s: 412,
                sessions: 1,
            },
            last_message: Some(
                "I will keep polling the build until it finishes, then append step 4 to PROGRESS.md."
                    .into(),
            ),
            files: vec!["PROGRESS.md".into(), "src/app.py".into(), "tests/test_app.py".into()],
            files_total: 3,
            blocking: true,
            options: vec![
                InstructAction::ContinueWithPrompt,
                InstructAction::NewSessionWithPrompt,
                InstructAction::TryModel,
                InstructAction::Finish,
            ],
            models: vec![
                NeedsInputModel {
                    id: "us.anthropic.claude-opus-5-5".into(),
                    name: "Claude Opus 5.5".into(),
                    context_tokens: 1_000_000,
                },
                NeedsInputModel {
                    id: "global.anthropic.claude-sonnet-5-5".into(),
                    name: "Claude Sonnet 5.5".into(),
                    context_tokens: 1_000_000,
                },
            ],
            question: None,
            recommended: None,
            can_continue: true,
            wait_s: 1800,
            expires_at: Some(control_api::ids::rfc3339(unix_now() + 1800)),
        };
        edit(&mut p);
        let mut e = self.envelope(
            run_id,
            EventType::TurnProgress,
            Authority::Agent,
            Some("direct"),
            serde_json::to_value(&p).unwrap(),
        );
        e.event_id = request_id.clone();
        self.inner.lock().unwrap().events.push(e);
        request_id
    }

    /// The acknowledgement or closing event of the simulated agentd: `kind` is `input-received`, `input-ignored` or `input-closed`;
    /// `extra` adds members (`action`, `why`, `how`, `textBytes`).
    pub fn input_event(&self, run_id: &str, kind: &str, request_id: &str, extra: Value) {
        debug_assert!(
            [
                PROGRESS_INPUT_RECEIVED,
                PROGRESS_INPUT_IGNORED,
                PROGRESS_INPUT_CLOSED
            ]
            .contains(&kind)
        );
        let mut v = json!({"turn": 1, "kind": kind, "text": kind, "requestId": request_id});
        for (k, x) in extra.as_object().cloned().unwrap_or_default() {
            v[k] = x;
        }
        self.agent(run_id, "direct", EventType::TurnProgress, v);
    }

    /// A fresh Cline session started (`new-session`): `session` is its number, `trigger` is `harness`, `user` or `analyzer`.
    pub fn new_session(&self, run_id: &str, session: u32, trigger: &str, request_id: Option<&str>) {
        self.agent(
            run_id,
            "direct",
            EventType::TurnProgress,
            json!({"turn": 1, "kind": PROGRESS_NEW_SESSION, "text": format!("Restarted from the work tree: fresh Cline session {session}."),
                   "requestId": request_id, "session": session, "trigger": trigger, "reason": "engine_stop",
                   "restartedProcess": false, "digestBytes": 18432, "promptBytes": 21344}),
        );
    }

    /// The controls queued for run `run_id` so far, oldest first.
    pub fn controls_of(&self, run_id: &str) -> Vec<ControlItem> {
        self.inner
            .lock()
            .unwrap()
            .controls
            .iter()
            .filter(|c| c.run_id == run_id)
            .cloned()
            .collect()
    }

    /// The usage the direct run's turn recorded (worker-api's `record_turn_usage`), on `model_id`: `GET /v1/runs/{id}` then answers
    /// a `cost` priced from it, and a run without this call answers none (a turn that ended without engine-reported usage).
    pub fn record_direct_usage(&self, run_id: &str, model_id: &str, usage: Usage) {
        self.inner
            .lock()
            .unwrap()
            .direct_usage
            .insert(run_id.to_string(), (model_id.to_string(), usage));
    }

    /// The harness side of a direct run that delivered nothing: `goal.blocked`, `blocked` and `run.completed` with the label.
    pub fn block_direct_run(
        &self,
        run_id: &str,
        reason: &str,
        outcome: Option<harness_protocol::direct::DirectOutcome>,
    ) {
        self.harness(
            run_id,
            EventType::SessionStarted,
            json!({"agentId": "direct", "engine": "cline", "modelId": MODEL}),
        );
        self.harness(run_id, EventType::GoalBlocked, json!({"reason": reason}));
        self.harness(
            run_id,
            EventType::GoalStatus,
            json!({"status": "blocked", "phase": "delivering"}),
        );
        let mut done = json!({"status": "blocked"});
        if let Some(o) = outcome {
            done["outcome"] = json!(o.as_str());
            done["label"] = json!(o.label());
        }
        self.harness(run_id, EventType::RunCompleted, done);
    }

    /// Agent `agent_id` on task `task_id` (roles test-designer then implementer), as dispatch
    /// records it: the Agent and Task items and the harness `session.started`.
    pub fn add_agent(&self, run_id: &str, agent_id: &str, task_id: &str, status: &str) {
        let engine = "cline";
        let item: AgentItem = serde_json::from_value(json!({
            "runId": run_id, "agentId": agent_id, "taskId": task_id, "attempt": 1,
            "runtimeArn": RUNTIME_ARN, "runtimeSessionId": format!("{run_id}.{agent_id}.g1-session-0000000"),
            "generation": 1, "engine": engine, "engineVersion": "cli-v3.0.65", "transport": "acp",
            "modelId": MODEL, "pool": format!("{REGION}#{MODEL}"), "status": status, "version": 1
        }))
        .unwrap();
        {
            let mut g = self.inner.lock().unwrap();
            g.agent_items
                .retain(|a| !(a.run_id == run_id && a.agent_id == agent_id));
            g.agent_items.push(item);
        }
        let mut e = self.envelope(
            run_id,
            EventType::SessionStarted,
            Authority::Harness,
            None,
            json!({"agentId": agent_id, "engine": engine, "modelId": MODEL}),
        );
        e.task_id = Some(task_id.to_string());
        self.inner.lock().unwrap().events.push(e);
        self.agent(run_id, agent_id, EventType::TurnStarted, json!({"turn": 1}));
    }

    /// The direct agent of `run_id` registers the harness's message controls (`message.next`, `message.now`), as a run on the image that
    /// takes messages does.
    pub fn enable_messages(&self, run_id: &str) {
        let mut g = self.inner.lock().unwrap();
        for a in g
            .agent_items
            .iter_mut()
            .filter(|a| a.run_id == run_id && a.agent_id == "direct")
        {
            a.harness_controls = vec!["message.next".into(), "message.now".into()];
        }
    }

    /// The evidence zip of a delivered run (`ham pull --artifact evidence`), at the packager's evidence key. Returns the artifact id
    /// and the SHA-256.
    pub fn evidence_zip(&self, run_id: &str) -> (String, String) {
        self.object(
            run_id,
            &format!("runs/{run_id}/evidence/evidence.zip"),
            ArtifactKind::Evidence,
            zip_bytes(1500),
        )
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.inner.lock().unwrap().seen.clone()
    }

    pub fn posts(&self, path: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "POST" && s.path == path)
            .collect()
    }

    /// Stores `bytes` as the S3 object `key` (no artifact item): a verified bundle, a snapshot record.
    pub fn put_object(&self, key: &str, bytes: Vec<u8>) {
        let etag = format!("\"{}\"", &sha256_hex(&bytes)[..32]);
        self.inner
            .lock()
            .unwrap()
            .objects
            .insert(key.to_string(), (bytes, etag));
    }

    /// The request object control-api stored for `run_id` (`runs/<run>/input/request.json`), parsed.
    pub fn stored_request_of(&self, run_id: &str) -> Value {
        let g = self.inner.lock().unwrap();
        let bytes = g
            .blobs
            .get(&s3::request_key(run_id))
            .unwrap_or_else(|| panic!("no stored request for run {run_id}"));
        serde_json::from_slice(bytes).unwrap()
    }

    /// Marks the run finished, as run-session does when a run ends: control-api continues only a run that is not active.
    pub fn finish_run(&self, run_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .runs
            .get_mut(run_id)
            .unwrap_or_else(|| panic!("no run {run_id}"))
            .status = harness_protocol::goal::GoalStatus::Complete;
    }

    /// The ids of the runs the control plane holds.
    pub fn run_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().runs.keys().cloned().collect()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The `kind` of a `turn.progress` event's inline payload.
fn payload_kind(e: &Envelope) -> Option<&str> {
    match &e.payload {
        Payload::Inline { value } => value.get("kind")?.as_str(),
        Payload::ClaimCheck { .. } => None,
    }
}

/// Deterministic bytes of the given length (not a real zip; the client only hashes it).
pub fn zip_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

/// What the stand-in puts in `x-hamh-backend-version` on its answers to `/v1` calls, as the control plane's Lambda does on every answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BackendHeader {
    /// This build's own version (the backend and the client share the root `VERSION` file).
    #[default]
    Same,
    /// No header: a backend that predates the tag.
    Absent,
    /// This text, valid or not.
    Value(String),
}

/// Failure injection, consumed one at a time.
#[derive(Default)]
pub struct Faults {
    /// The `x-hamh-backend-version` header of every answer to a `/v1` call (not of an object fetch, which is S3's).
    pub backend_header: Mutex<BackendHeader>,
    /// `POST /v1/runs`: answer 503 before the handler this many times.
    pub post_503: AtomicU32,
    /// `POST /v1/runs`: run the handler, then drop the connection this many times.
    pub post_drop_after_commit: AtomicU32,
    /// Events reads: answer 503 this many times.
    pub events_503: AtomicU32,
    /// Events reads: wait this many milliseconds before the answer (0 = answer at once).
    pub events_delay_ms: AtomicU64,
    /// The list of runs (`GET /v1/runs`, every page): wait this many milliseconds before the answer (0 = answer at once). A slow list
    /// is how a loaded host looks to the Runs screen, which draws its frame before the first list arrives.
    pub runs_delay_ms: AtomicU64,
    /// The transcript route (`GET /v1/runs/{id}/transcript`, every page): wait this many milliseconds before the answer (0 = answer at once). On the
    /// stage the route answers in about 0.65 s, after the run screen has drawn its progress feed.
    pub transcript_delay_ms: AtomicU64,
    /// Object GETs whose range starts at this offset answer 500 once (u64::MAX = none).
    pub fail_range_at: AtomicU64,
    /// Object GETs answer 403 (an expired URL) this many times.
    pub object_403: AtomicU32,
    /// Ranged object GETs other than the `bytes=0-0` probe answer 403 this many times.
    pub range_403: AtomicU32,
    /// Ranged object GETs other than the probe answer 412 (the ETag changed) this many times.
    pub range_412: AtomicU32,
    /// Ranged object GETs other than the probe carry a wrong Content-Range this many times.
    pub bad_content_range: AtomicU32,
    /// The probe is answered 200 with the whole object (a store that ignores Range) this many times.
    pub probe_whole: AtomicU32,
}

fn take(n: &AtomicU32) -> bool {
    n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
        .is_ok()
}

pub struct Fake {
    pub mem: Mem,
    pub faults: Faults,
    /// `http://127.0.0.1:<port>/live`: what `apiEndpoint` is set to.
    pub endpoint: String,
}

impl Fake {
    pub async fn start() -> Arc<Fake> {
        Self::start_with(false).await
    }

    pub async fn start_with(autopilot: bool) -> Arc<Fake> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mem = Mem::default();
        mem.inner.lock().unwrap().autopilot = autopilot;
        *mem.base.lock().unwrap() = format!("http://{addr}");
        let fake = Arc::new(Fake {
            mem,
            faults: Faults {
                fail_range_at: AtomicU64::new(u64::MAX),
                ..Faults::default()
            },
            endpoint: format!("http://{addr}/{STAGE}"),
        });
        let state = fake.clone();
        // The stage's API Gateway takes a body of 10 MB (the control-api Lambda 6 MiB): the stand-in takes the larger, as axum's default of 2 MiB
        // would refuse a request that the platform accepts.
        let app = axum::Router::new()
            .fallback(
                move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                    let state = state.clone();
                    async move { state.serve(method, uri, headers, body).await }
                },
            )
            .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        fake
    }

    fn record(&self, seen: Seen) {
        self.mem.inner.lock().unwrap().seen.push(seen);
    }

    async fn serve(&self, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
        let is_object = uri.path().starts_with("/objects/");
        let mut answer = self.serve_inner(method, uri, headers, body).await;
        if !is_object {
            let header = self.faults.backend_header.lock().unwrap().clone();
            let value = match header {
                BackendHeader::Same => Some(ham::version::CLIENT.to_string()),
                BackendHeader::Absent => None,
                BackendHeader::Value(v) => Some(v),
            };
            if let Some(v) = value.and_then(|v| axum::http::HeaderValue::from_str(&v).ok()) {
                answer.headers_mut().insert(ham::version::BACKEND_HEADER, v);
            }
        }
        answer
    }

    async fn serve_inner(
        &self,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let path = uri.path().to_string();
        let query = uri.query().unwrap_or_default().to_string();
        let lower: BTreeMap<String, String> = headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        if let Some(key) = path.strip_prefix("/objects/") {
            self.record(Seen {
                method: method.to_string(),
                path: path.clone(),
                query: query.clone(),
                headers: lower.clone(),
                body: vec![],
                caller: None,
                signature: Ok(()),
                at_ms: arrived_ms(),
            });
            let delay = self.mem.inner.lock().unwrap().range_delay;
            let probe = lower.get("range").is_none_or(|r| r == "bytes=0-0");
            if !probe && !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            return self.object(key, &query, &lower);
        }
        let signature = verify(method.as_str(), &uri, &lower, &body);
        let caller = match &signature {
            Ok(akid) if akid == AKID => Some(ME.to_string()),
            _ => None,
        };
        let v1_path = path
            .strip_prefix(&format!("/{STAGE}"))
            .unwrap_or(&path)
            .to_string();
        self.record(Seen {
            method: method.to_string(),
            path: v1_path.clone(),
            query: query.clone(),
            headers: lower.clone(),
            body: if v1_path == "/v1/direct/runs" {
                body.to_vec()
            } else {
                vec![]
            },
            caller: caller.clone(),
            signature: signature.clone().map(|_| ()),
            at_ms: arrived_ms(),
        });
        if let Err(e) = signature {
            return (
                StatusCode::FORBIDDEN,
                [("content-type", "application/json")],
                json!({"code": "forbidden", "message": format!("signature: {e}")}).to_string(),
            )
                .into_response();
        }
        if method == Method::POST && v1_path == "/v1/direct/runs" && take(&self.faults.post_503) {
            return api_error(
                503,
                "unavailable",
                "the harness is unavailable; retry with the same Idempotency-Key",
            );
        }
        if v1_path.ends_with("/events") {
            let delay = self.faults.events_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
        if method == Method::GET && v1_path.ends_with("/transcript") {
            let delay = self.faults.transcript_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
        if method == Method::GET && v1_path == "/v1/runs" {
            let delay = self.faults.runs_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
        if v1_path.ends_with("/events") && take(&self.faults.events_503) {
            return api_error(503, "unavailable", "the harness is unavailable; retry");
        }
        let q: BTreeMap<String, String> = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (pct_decode(k), pct_decode(v)),
                None => (pct_decode(kv), String::new()),
            })
            .collect();
        let deps = Deps {
            store: self.mem.clone(),
            blobs: self.mem.clone(),
            execution: self.mem.clone(),
            account: "800000000022".into(),
            operators: vec!["arn:aws:sts::800000000022:assumed-role/Admin/*".into()],
            repo_names: Default::default(),
        };
        let out = handle(
            V1Request {
                method: method.to_string(),
                path: v1_path.clone(),
                caller_arn: caller,
                source_ip: Some("127.0.0.1".into()),
                headers: lower,
                query: q,
                body: String::from_utf8_lossy(&body).into_owned(),
            },
            &deps,
        )
        .await;
        if method == Method::POST
            && v1_path == "/v1/direct/runs"
            && take(&self.faults.post_drop_after_commit)
        {
            // The run is recorded; the answer is lost: the client must retry with its key.
            panic!("fake: dropping the connection after the run was recorded");
        }
        (
            StatusCode::from_u16(out.status).unwrap(),
            [("content-type", "application/json")],
            out.body,
        )
            .into_response()
    }

    fn object(&self, key: &str, query: &str, h: &BTreeMap<String, String>) -> Response {
        if !query.contains("X-Amz-Signature=") {
            return (StatusCode::FORBIDDEN, "unsigned").into_response();
        }
        if take(&self.faults.object_403) {
            return (
                StatusCode::FORBIDDEN,
                "<Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>",
            )
                .into_response();
        }
        let Some((bytes, etag)) = self.mem.inner.lock().unwrap().objects.get(key).cloned() else {
            return (StatusCode::NOT_FOUND, "NoSuchKey").into_response();
        };
        if h.get("if-match").is_some_and(|m| *m != etag) {
            return (StatusCode::PRECONDITION_FAILED, "PreconditionFailed").into_response();
        }
        let size = bytes.len() as u64;
        let Some(range) = h.get("range") else {
            return (StatusCode::OK, [("etag", etag)], bytes).into_response();
        };
        let probe = range == "bytes=0-0";
        if probe && take(&self.faults.probe_whole) {
            return (StatusCode::OK, [("etag", etag)], bytes).into_response();
        }
        if !probe && take(&self.faults.range_403) {
            return (StatusCode::FORBIDDEN, "Request has expired").into_response();
        }
        if !probe && take(&self.faults.range_412) {
            return (StatusCode::PRECONDITION_FAILED, "PreconditionFailed").into_response();
        }
        let Some((s, e)) = range
            .strip_prefix("bytes=")
            .and_then(|r| r.split_once('-'))
            .and_then(|(s, e)| Some((s.parse::<u64>().ok()?, e.parse::<u64>().ok()?)))
        else {
            return (StatusCode::BAD_REQUEST, "bad range").into_response();
        };
        if s > e || s >= size {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [("etag", etag)],
                Vec::new(),
            )
                .into_response();
        }
        if self
            .faults
            .fail_range_at
            .compare_exchange(s, u64::MAX, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "InternalError").into_response();
        }
        let e = e.min(size - 1);
        let content_range = if !probe && take(&self.faults.bad_content_range) {
            format!("bytes {}-{e}/{size}", s + 1)
        } else {
            format!("bytes {s}-{e}/{size}")
        };
        self.mem
            .inner
            .lock()
            .unwrap()
            .ranges
            .push((key.to_string(), s, e));
        (
            StatusCode::PARTIAL_CONTENT,
            [("etag", etag), ("content-range", content_range)],
            bytes[s as usize..=e as usize].to_vec(),
        )
            .into_response()
    }
}

fn api_error(status: u16, code: &str, message: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        json!({"code": code, "message": message}).to_string(),
    )
        .into_response()
}

/// Unix seconds of `YYYYMMDDTHHMMSSZ`.
fn amz_date_secs(d: &str) -> Option<u64> {
    if d.len() != 16 {
        return None;
    }
    let n = |a: usize, b: usize| d.get(a..b)?.parse::<i64>().ok();
    let (y, m, day) = (n(0, 4)?, n(4, 6)?, n(6, 8)?);
    let (hh, mm, ss) = (n(9, 11)?, n(11, 13)?, n(13, 15)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) as u64)
}

/// Checks the `Authorization` header shape, then recomputes the signature from the request as
/// received. Returns the access key id.
pub fn verify(
    method: &str,
    uri: &Uri,
    h: &BTreeMap<String, String>,
    body: &[u8],
) -> Result<String, String> {
    let service_name = "execute-api";
    let auth = h.get("authorization").ok_or("no Authorization header")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 Credential=")
        .ok_or("not AWS4-HMAC-SHA256")?;
    let (cred, rest) = rest
        .split_once(", SignedHeaders=")
        .ok_or("no SignedHeaders")?;
    let (signed, sig) = rest.split_once(", Signature=").ok_or("no Signature")?;
    let parts: Vec<&str> = cred.split('/').collect();
    let [akid, date, region, service, term] = parts.as_slice() else {
        return Err(format!("credential scope {cred}"));
    };
    if *region != REGION || *service != service_name || *term != "aws4_request" {
        return Err(format!("credential scope {cred}"));
    }
    if sig.len() != 64 || !sig.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("signature is not 64 hex characters".into());
    }
    let amz_date = h.get("x-amz-date").ok_or("no X-Amz-Date")?;
    if !amz_date.starts_with(date) {
        return Err("scope date differs from X-Amz-Date".into());
    }
    let names: Vec<&str> = signed.split(';').collect();
    for required in ["host", "x-amz-date"] {
        if !names.contains(&required) {
            return Err(format!("{required} is not signed"));
        }
    }
    if h.contains_key("x-amz-security-token") && !names.contains(&"x-amz-security-token") {
        return Err("x-amz-security-token is not signed".into());
    }
    if method == "POST" && h.contains_key("idempotency-key") && !names.contains(&"idempotency-key")
    {
        return Err("idempotency-key is not signed".into());
    }
    let secs = amz_date_secs(amz_date).ok_or("bad X-Amz-Date")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if secs.abs_diff(now) > 300 {
        return Err("request time is more than 5 minutes off".into());
    }
    let secret = match *akid {
        AKID => SECRET,
        other => return Err(format!("unknown access key {other}")),
    };
    let token = h.get("x-amz-security-token").cloned();
    let identity = Credentials::new(*akid, secret, token, None, "fake").into();
    let host = h.get("host").ok_or("no Host header")?;
    let url = format!(
        "http://{host}{}",
        uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
    );
    let extra: Vec<(&str, &str)> = names
        .iter()
        .filter(|n| !matches!(**n, "host" | "x-amz-date" | "x-amz-security-token"))
        .map(|n| (*n, h.get(*n).map(String::as_str).unwrap_or_default()))
        .collect();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(REGION)
        .name(service_name)
        .time(UNIX_EPOCH + Duration::from_secs(secs))
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| e.to_string())?
        .into();
    let req = SignableRequest::new(method, url, extra.into_iter(), SignableBody::Bytes(body))
        .map_err(|e| e.to_string())?;
    let (instructions, _) = sign(req, &params).map_err(|e| e.to_string())?.into_parts();
    let mut carrier = http::Request::new(());
    instructions.apply_to_request_http1x(&mut carrier);
    let want = carrier
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if want != *auth {
        return Err("signature does not match the request received".into());
    }
    Ok((*akid).to_string())
}
