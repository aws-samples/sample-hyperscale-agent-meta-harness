//! One in-memory world behind `Store`, `Blobs` and `Execution` for the chat-client routes.
//! A fake rejects what the real API rejects, with the API reference beside the check:
//!
//! - DynamoDB `Query` needs `Limit` of at least 1, and `BETWEEN` needs a lower bound that is not above the upper one
//!   (<https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_Query.html>).
//! - A GSI1 page holds at most 100 rows and ends with a `LastEvaluatedKey` the next call must hand back; a key from another
//!   caller is refused (`DynamoStore` binds its `nextToken` to the caller).
//! - An index item is addressed by `OWNER#<sha256 of the caller>` and `SRCH#<createdAt>#<runId>`; the write is an `UpdateItem` whose
//!   `if_not_exists` leaves every field that is already there alone, and it reports whether it made or completed the item.
//! - An S3 ranged GET needs a first byte below the object size (HTTP 416 otherwise) and the key must exist (404 `NoSuchKey`)
//!   (<https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html>).
//! - A presigned URL lives at most 7 days (SigV4, <https://docs.aws.amazon.com/AmazonS3/latest/userguide/using-presigned-url.html>).
//! - The tag writers' `UpdateItem` goes through `ham_core::runtag::dynamo::expr_rejects` first and the fake panics on
//!   a refusal (a path set and removed in one expression, an empty set, a reserved word as a bare name, an unused or undefined placeholder,
//!   <https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Expressions.UpdateExpressions.html>). It then reads the spec's own
//!   text: the condition (`attribute_not_exists`, `attribute_exists`, `path = :value`, joined by AND and OR) is judged against the stored
//!   item, and SET and REMOVE act on a JSON image of the item, every right-hand side read from the item as it was before the update. A form
//!   it cannot read is a panic, so a changed spec fails a test instead of passing against a permissive fake. A condition that fails is
//!   `StoreError::Conflict` and changes nothing; an item that is absent is an empty image, so `if_not_exists` creates it, and it must hold
//!   `runId` and `createdAt` afterwards or the fake panics.
#![allow(dead_code, clippy::unwrap_used)]

use control_api::handler::{
    Blobs, DeployedAs, Deps, Execution, ObjectPrefix, RunUsage, Store, StoreError, TagRead,
    V1Request,
};
use ham_core::run_index::{RunIndexItem, owner_pk, search_sk};
use ham_core::runtag::dynamo::{Av, UpdateSpec, expr_rejects};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::api::v1::DownloadUrlResponse;
use harness_protocol::direct_input::{InstructAction, InstructOrigin};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::{
    AgentItem, ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RunItem, event_shard,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

pub const ME: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
pub const OTHER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/someone";
pub const BUCKET: &str = "hamh-direct-harness-700000000011";
/// A GSI1 page.
pub const PAGE: usize = 100;
/// Seven days, the longest a SigV4 URL can live.
const MAX_PRESIGN: Duration = Duration::from_secs(7 * 24 * 3600);
pub const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

/// A ULID built from a counter: sorts by `n`.
pub fn ulid(n: u64) -> String {
    const C: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut s = String::from("01M5A2R7K3");
    let mut rest = Vec::new();
    let mut v = n;
    for _ in 0..16 {
        rest.push(C[(v % 32) as usize] as char);
        v /= 32;
    }
    rest.reverse();
    s.extend(rest);
    s
}

/// `2026-10-15T13:00:00Z` plus `n` seconds (within the day range the tests use).
pub fn at(n: u64) -> String {
    control_api::ids::rfc3339(1_791_982_800 + n)
}

pub fn run_item(id: &str, owner: &str, status: &str, created: &str, target: Value) -> RunItem {
    serde_json::from_value(json!({
        "runId": id, "ownerPrincipalArn": owner, "promptSha256": "cd".repeat(32),
        "target": target, "status": status, "phase": "green",
        "enginePolicyVersion": 0, "createdAt": created, "version": 3
    }))
    .unwrap()
}

pub fn goal_item(run: &str, objective: &str) -> GoalItem {
    serde_json::from_value(json!({
        "runId": run, "objective": objective, "requirements": [], "specSha256": "", "acceptanceSha256": "",
        "status": "active", "phase": "green", "lastBlockerFingerprint": null, "sameBlockerStreak": 0,
        "emptyTurnStreak": 0, "execFailureStreak": 0, "ladderRung": 0, "evaluatorVerdicts": [],
        "tokensUsed": 0, "timeUsedS": 0, "turnCount": 0, "version": 1
    }))
    .unwrap()
}

pub fn agent_item(run: &str, model: &str, status: &str) -> AgentItem {
    serde_json::from_value(json!({
        "runId": run, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{run}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": model,
        "pool": "direct", "status": status, "version": 1
    }))
    .unwrap()
}

/// An agent progress event of the direct agent (shard 28).
pub fn progress(run: &str, id: &str, ts: &str, value: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: id.into(),
        run_id: run.into(),
        task_id: None,
        agent_id: Some("direct".into()),
        seq: 1,
        ts: ts.into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline { value },
    }
}

/// A blocking `needs-input` question whose request id is its own event id.
pub fn question(run: &str, id: &str, ts: &str) -> Envelope {
    progress(
        run,
        id,
        ts,
        json!({
            "turn": 1, "kind": "needs-input", "text": "Needs your input", "requestId": id, "n": 1,
            "reason": "engine_stop", "title": "Cline stopped its own prompt", "stopOutcome": "stopped_engine_cancelled",
            "evidence": {"attempts": 2, "toolCalls": 41, "filesChanged": 3, "identicalCalls": 5, "failedTurns": null,
                         "lastError": null, "runS": 412, "sessions": 1},
            "lastMessage": "waiting", "files": [], "filesTotal": 0, "blocking": true,
            "options": ["continue_with_prompt", "new_session_with_prompt", "try_model", "finish"],
            "models": [], "recommended": null, "canContinue": true, "waitS": 1800, "expiresAt": "2026-10-15T14:11:07Z"
        }),
    )
}

pub fn message(run: &str, id: &str, ts: &str, text: &str) -> Envelope {
    progress(
        run,
        id,
        ts,
        json!({"turn": 1, "kind": "message", "text": text}),
    )
}

pub fn answer_control(
    run: &str,
    request: &str,
    text: Option<&str>,
    origin: InstructOrigin,
) -> ControlItem {
    ControlItem {
        run_id: run.into(),
        agent_id: "direct".into(),
        control_id: request.into(),
        control: PendingControl::Instruct {
            request_id: request.into(),
            action: InstructAction::ContinueWithPrompt,
            text: text.map(Into::into),
            model: None,
            origin,
        },
        created_at: "2026-10-15T13:42:00Z".into(),
        delivered_at: None,
    }
}

#[derive(Debug, Default)]
pub struct World {
    pub runs: Mutex<BTreeMap<String, RunItem>>,
    pub goals: Mutex<BTreeMap<String, GoalItem>>,
    pub agents: Mutex<Vec<AgentItem>>,
    pub events: Mutex<Vec<Envelope>>,
    pub controls: Mutex<Vec<ControlItem>>,
    /// Index items by (owner ARN, sort key).
    pub index: Mutex<BTreeMap<(String, String), RunIndexItem>>,
    pub objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// `Last-Modified` of an object as RFC 3339; an object not listed was written long ago.
    pub modified: Mutex<BTreeMap<String, String>>,
    pub presigned: Mutex<Vec<(String, Duration)>>,
    /// The version each presign was pinned to, in the order of `presigned`.
    pub presigned_versions: Mutex<Vec<Option<String>>>,
    /// Every store, blob and execution call, by name and its main arguments.
    pub calls: Mutex<Vec<String>>,
    pub idem: Mutex<BTreeMap<String, IdempotencyItem>>,
    pub started: Mutex<Vec<String>>,
    /// The tail read sleeps this long (a slow table).
    pub tail_sleep: Mutex<Duration>,
    pub index_read_down: AtomicBool,
    pub index_write_down: AtomicBool,
    /// Another writer completed every item first: `put_run_index` finds it whole.
    pub index_races: AtomicBool,
    pub control_read_down: AtomicBool,
    pub objects_down: AtomicBool,
    pub presign_down: AtomicBool,
    /// The role may not list the bucket (no `s3:ListBucket`): S3 answers a GET of an absent key 403 AccessDenied, not 404, and the S3 adapter
    /// maps that to `Unavailable`. A listing by `exists` (granted for one key pattern) still answers.
    pub no_list_permission: AtomicBool,
    /// Writers that land between a tag edit's read and its write: each `put_run_tags` takes the oldest first, applies it to the
    /// item (its own condition must hold, or the fake panics: a hook that lost its own race is a test mistake) and only then judges the
    /// edit's write.
    pub tag_gap: Mutex<VecDeque<UpdateSpec>>,
    /// Every `UpdateSpec` `put_run_tags` received, as sent, with the owner it was sent for and the run.
    pub tag_writes: Mutex<Vec<(String, String, UpdateSpec)>>,
    /// The tag state read and the tag write fail like a throttled table.
    pub tag_read_down: AtomicBool,
    pub tag_write_down: AtomicBool,
    /// The tag write fails for the one run named here and works for every other.
    pub tag_write_fail_run: Mutex<Option<String>>,
    /// The tag write is applied and then reports a lost race, as an SDK retry of an acknowledged write would (the retry meets its own
    /// revision bump).
    pub tag_ack_lost_as_conflict: AtomicBool,
    /// Tag attributes the item holds in a form the reader cannot take, by run id (what the adapter finds in the raw item).
    pub tag_unreadable: Mutex<BTreeMap<String, Vec<String>>>,
    /// Tag-state reads in flight now, and the most there ever were at once.
    pub tag_reading: AtomicUsize,
    pub tag_reading_max: AtomicUsize,
    /// The stage has an Instances worker runtime: what `Execution::instances_available` answers.
    pub instances: AtomicBool,
    /// The Instances lookup fails like a throttled parameter read.
    pub instances_down: AtomicBool,
    /// The stage's deploy lock: what `Execution::deploy_lock` answers. `None` is a stage without the parameter.
    pub deploy_lock: Mutex<Option<Result<control_api::handler::DeployLock, StoreError>>>,
}

impl World {
    pub fn new() -> World {
        World::default()
    }
    pub fn log(&self, line: impl Into<String>) {
        self.calls.lock().unwrap().push(line.into());
    }
    pub fn count(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
    pub fn add_run(&self, run: RunItem) {
        self.runs.lock().unwrap().insert(run.run_id.clone(), run);
    }
    pub fn add_events(&self, events: impl IntoIterator<Item = Envelope>) {
        self.events.lock().unwrap().extend(events);
    }
    /// An object written long ago.
    pub fn put_object(&self, key: &str, bytes: Vec<u8>) {
        self.objects.lock().unwrap().insert(key.into(), bytes);
        self.modified.lock().unwrap().remove(key);
    }
    pub fn put_object_at(&self, key: &str, bytes: Vec<u8>, modified: &str) {
        self.objects.lock().unwrap().insert(key.into(), bytes);
        self.modified
            .lock()
            .unwrap()
            .insert(key.into(), modified.into());
    }
    pub fn add_index(&self, owner: &str, item: RunIndexItem) {
        let sk = search_sk(&item.created_at, &item.run_id);
        self.index.lock().unwrap().insert((owner.into(), sk), item);
    }
    pub fn indexed(&self, owner: &str) -> Vec<RunIndexItem> {
        self.index
            .lock()
            .unwrap()
            .iter()
            .filter(|((o, _), _)| o == owner)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

pub fn deps(w: &World) -> Deps<&World, &World, &World> {
    Deps {
        store: w,
        blobs: w,
        execution: w,
        account: "700000000011".into(),
        operators: vec![],
        repo_names: Default::default(),
    }
}

pub fn request(
    caller: &str,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    body: &str,
) -> V1Request {
    V1Request {
        method: method.into(),
        path: path.into(),
        caller_arn: Some(caller.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: query
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        body: body.into(),
    }
}

pub async fn get(w: &World, caller: &str, path: &str, query: &[(&str, &str)]) -> (u16, Value) {
    let out = control_api::handler::handle(request(caller, "GET", path, query, ""), &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

pub async fn get_logged(
    w: &World,
    caller: &str,
    path: &str,
    query: &[(&str, &str)],
) -> (u16, Value, Vec<String>) {
    let (out, logs) =
        control_api::handler::handle_logged(request(caller, "GET", path, query, ""), &deps(w))
            .await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
        logs,
    )
}

pub async fn post(
    w: &World,
    caller: &str,
    path: &str,
    query: &[(&str, &str)],
    body: &str,
) -> (u16, Value, Vec<String>) {
    let (out, logs) =
        control_api::handler::handle_logged(request(caller, "POST", path, query, body), &deps(w))
            .await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
        logs,
    )
}

fn unavailable(what: &str) -> StoreError {
    StoreError::Unavailable(format!("{what} throttled"))
}

impl Store for &World {
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        self.log("claim_idempotency");
        let key = format!("{}#{}", c.principal_arn, c.idempotency_key);
        Ok(self
            .idem
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| c.clone())
            .clone())
    }
    async fn create_run(
        &self,
        run: &RunItem,
        goal: &GoalItem,
        created: &Envelope,
    ) -> Result<(), StoreError> {
        self.log("create_run");
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
        Ok(self.runs.lock().unwrap().get(run_id).cloned())
    }
    async fn get_root_goal(&self, run_id: &str) -> Result<Option<GoalItem>, StoreError> {
        self.log(format!("get_root_goal {run_id}"));
        Ok(self.goals.lock().unwrap().get(run_id).cloned())
    }
    /// GSI1: the owner's Run items newest first, `PAGE` per call, the token bound to the caller.
    async fn list_runs(
        &self,
        owner: &str,
        next: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        self.log("list_runs");
        let mut rows: Vec<RunItem> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.owner_principal_arn == owner)
            .cloned()
            .collect();
        rows.sort_by(|a, b| (&b.created_at, &b.run_id).cmp(&(&a.created_at, &a.run_id)));
        let start = match next {
            None => 0,
            Some(t) => {
                let (o, n) = t.rsplit_once('|').ok_or(StoreError::BadCursor)?;
                if o != owner {
                    return Err(StoreError::BadCursor);
                }
                n.parse::<usize>().map_err(|_| StoreError::BadCursor)?
            }
        };
        let page: Vec<RunItem> = rows.iter().skip(start).take(PAGE).cloned().collect();
        let end = start + page.len();
        let token = (end < rows.len()).then(|| format!("{owner}|{end}"));
        Ok((page, token))
    }
    async fn events(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        self.log(format!("events {run_id} {shard}"));
        let mut out = self.shard_events(run_id, shard);
        out.retain(|e| after.is_none_or(|a| e.event_id.as_str() > a));
        out.truncate(limit);
        Ok(out)
    }
    async fn list_artifacts(&self, _: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        Ok(vec![])
    }
    async fn get_artifact(&self, _: &str, _: &str) -> Result<Option<ArtifactItem>, StoreError> {
        Ok(None)
    }
    async fn run_usage(&self, _: &str) -> Result<RunUsage, StoreError> {
        Ok(RunUsage::default())
    }
    async fn list_agents(&self, run_id: &str) -> Result<Vec<AgentItem>, StoreError> {
        self.log(format!("list_agents {run_id}"));
        Ok(self
            .agents
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.run_id == run_id)
            .cloned()
            .collect())
    }
    async fn put_control(&self, c: &ControlItem) -> Result<(), StoreError> {
        self.controls.lock().unwrap().push(c.clone());
        Ok(())
    }
    async fn get_control(
        &self,
        run_id: &str,
        agent_id: &str,
        control_id: &str,
    ) -> Result<Option<ControlItem>, StoreError> {
        self.log(format!("get_control {control_id}"));
        if self.control_read_down.load(Ordering::SeqCst) {
            return Err(unavailable("control read"));
        }
        Ok(self
            .controls
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.run_id == run_id && c.agent_id == agent_id && c.control_id == control_id)
            .cloned())
    }
    /// A Query of `SK BETWEEN CTRL#<agent>#<oldest> AND CTRL#<agent>#<newest>` on the run partition (consistent read): at most `limit`
    /// controls in id order and whether more were in the window.
    async fn list_controls(
        &self,
        run_id: &str,
        agent_id: &str,
        oldest: &str,
        newest: &str,
        limit: usize,
    ) -> Result<(Vec<ControlItem>, bool), StoreError> {
        self.log(format!("list_controls {run_id} {oldest} {newest}"));
        assert!(limit >= 1, "Query Limit must be at least 1");
        // DynamoDB BETWEEN: the lower bound may not be above the upper one.
        assert!(
            oldest <= newest,
            "BETWEEN needs lower <= upper: {oldest} {newest}"
        );
        if self.control_read_down.load(Ordering::SeqCst) {
            return Err(unavailable("control read"));
        }
        let mut all: Vec<ControlItem> = self
            .controls
            .lock()
            .unwrap()
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
    async fn latest_events(
        &self,
        run_id: &str,
        shard: u8,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        self.log(format!("latest_events {run_id}"));
        let nap = *self.tail_sleep.lock().unwrap();
        if !nap.is_zero() {
            tokio::time::sleep(nap).await;
        }
        let mut all = self.shard_events(run_id, shard);
        let skip = all.len().saturating_sub(limit);
        Ok(all.split_off(skip))
    }
    async fn events_before(
        &self,
        run_id: &str,
        shard: u8,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        self.log(format!(
            "events_before {run_id} {shard} {}",
            before.unwrap_or("-")
        ));
        // DynamoDB Query: Limit must be at least 1.
        assert!(limit >= 1, "Query Limit must be at least 1");
        let mut all = self.shard_events(run_id, shard);
        all.retain(|e| before.is_none_or(|b| e.event_id.as_str() < b));
        let skip = all.len().saturating_sub(limit);
        Ok(all.split_off(skip))
    }
    async fn run_index_between(
        &self,
        owner: &str,
        oldest: &str,
        newest: &str,
    ) -> Result<Vec<RunIndexItem>, StoreError> {
        self.log(format!("run_index_between {oldest} {newest}"));
        // DynamoDB BETWEEN: the lower bound may not be above the upper one.
        assert!(
            oldest <= newest,
            "BETWEEN needs lower <= upper: {oldest} {newest}"
        );
        if self.index_read_down.load(Ordering::SeqCst) {
            return Err(unavailable("index read"));
        }
        let lo = format!("SRCH#{oldest}");
        let hi = format!("SRCH#{newest}#~");
        Ok(self
            .index
            .lock()
            .unwrap()
            .iter()
            .filter(|((o, sk), _)| {
                o == owner && sk.as_str() >= lo.as_str() && sk.as_str() <= hi.as_str()
            })
            .map(|(_, v)| v.clone())
            .collect())
    }
    /// `UpdateItem ... SET f = if_not_exists(f, :v)` for each start field: a field already there is kept. `true` when the item was
    /// absent or lacked its prompt head (a run's end wrote it first), `false` when it was complete.
    async fn put_run_index(&self, owner: &str, item: &RunIndexItem) -> Result<bool, StoreError> {
        self.log(format!("put_run_index {}", item.run_id));
        if self.index_write_down.load(Ordering::SeqCst) {
            return Err(unavailable("index write"));
        }
        if self.index_races.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let key = (owner.to_string(), search_sk(&item.created_at, &item.run_id));
        let mut g = self.index.lock().unwrap();
        match g.get_mut(&key) {
            None => {
                g.insert(key, item.clone());
                Ok(true)
            }
            Some(old) if old.is_partial() => {
                old.prompt_head = item.prompt_head.clone();
                old.prompt_truncated = item.prompt_truncated;
                old.repository = item.repository.clone();
                if old.model.is_none() {
                    old.model = item.model.clone();
                }
                Ok(true)
            }
            Some(_) => Ok(false),
        }
    }
    /// A consistent `GetItem` of the search item that projects what an edit needs and nothing else: the ids, the repository, the three
    /// lists an edit replaces or reads and the revision. The fake narrows the stored item the same way, so a route that reads another
    /// attribute sees its default.
    async fn get_run_tag_state(
        &self,
        owner: &str,
        run: &harness_protocol::store::RunItem,
    ) -> Result<Option<TagRead>, StoreError> {
        self.log(format!("get_run_tag_state {}", run.run_id));
        let now = self.tag_reading.fetch_add(1, Ordering::SeqCst) + 1;
        self.tag_reading_max.fetch_max(now, Ordering::SeqCst);
        tokio::task::yield_now().await;
        self.tag_reading.fetch_sub(1, Ordering::SeqCst);
        if self.tag_read_down.load(Ordering::SeqCst) {
            return Err(unavailable("tag read"));
        }
        let key = (owner.to_string(), search_sk(&run.created_at, &run.run_id));
        let unreadable = self
            .tag_unreadable
            .lock()
            .unwrap()
            .get(&run.run_id)
            .cloned()
            .unwrap_or_default();
        Ok(self.index.lock().unwrap().get(&key).map(|i| TagRead {
            item: RunIndexItem {
                run_id: i.run_id.clone(),
                created_at: i.created_at.clone(),
                repository: i.repository.clone(),
                run_tags: i.run_tags.clone(),
                run_tags_user: i.run_tags_user.clone(),
                run_tags_off: i.run_tags_off.clone(),
                run_tags_rev: i.run_tags_rev,
                ..RunIndexItem::default()
            },
            unreadable,
        }))
    }
    /// One `UpdateItem` of the tag writers. See the header of this file for what the fake refuses and how it reads the spec.
    async fn put_run_tags(
        &self,
        owner: &str,
        run: &harness_protocol::store::RunItem,
        spec: &UpdateSpec,
    ) -> Result<(), StoreError> {
        self.log(format!("put_run_tags {}", run.run_id));
        self.tag_writes
            .lock()
            .unwrap()
            .push((owner.to_string(), run.run_id.clone(), spec.clone()));
        if let Some(reason) = expr_rejects(spec) {
            panic!("DynamoDB would refuse this UpdateItem: {reason}\n{spec:?}");
        }
        if self.tag_write_down.load(Ordering::SeqCst)
            || self.tag_write_fail_run.lock().unwrap().as_deref() == Some(run.run_id.as_str())
        {
            return Err(unavailable("tag write"));
        }
        let key = (owner.to_string(), search_sk(&run.created_at, &run.run_id));
        let mut index = self.index.lock().unwrap();
        if let Some(other) = self.tag_gap.lock().unwrap().pop_front() {
            assert!(
                apply_spec(&mut index, &key, &other),
                "the writer that lands in the gap lost its own race: {other:?}"
            );
        }
        if !apply_spec(&mut index, &key, spec) {
            return Err(StoreError::Conflict);
        }
        if self.tag_ack_lost_as_conflict.load(Ordering::SeqCst) {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
}

// ---- the tag writers' UpdateItem, read from its own text ----------------------------------------------------------------------

type Image = serde_json::Map<String, Value>;

fn av_json(v: &Av) -> Value {
    match v {
        Av::S(s) => Value::String(s.clone()),
        Av::N(n) => json!(n),
        Av::L(items) | Av::Ss(items) => {
            Value::Array(items.iter().cloned().map(Value::String).collect())
        }
    }
}

fn attribute_of(spec: &UpdateSpec, alias: &str) -> String {
    spec.names
        .get(alias)
        .cloned()
        .unwrap_or_else(|| alias.to_string())
}

fn value_of(spec: &UpdateSpec, alias: &str) -> Value {
    av_json(
        spec.values
            .get(alias)
            .unwrap_or_else(|| panic!("the placeholder {alias} has no value")),
    )
}

/// `text` cut at the commas that are not inside parentheses.
fn split_top(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts
}

fn atom_holds(spec: &UpdateSpec, image: &Image, atom: &str) -> bool {
    let call = |name: &str| {
        atom.strip_prefix(name)
            .and_then(|r| r.strip_prefix('('))
            .and_then(|r| r.strip_suffix(')'))
    };
    if let Some(path) = call("attribute_not_exists") {
        return !image.contains_key(&attribute_of(spec, path.trim()));
    }
    if let Some(path) = call("attribute_exists") {
        return image.contains_key(&attribute_of(spec, path.trim()));
    }
    if let Some((path, value)) = atom.split_once(" = ") {
        return image.get(&attribute_of(spec, path.trim())) == Some(&value_of(spec, value.trim()));
    }
    panic!("this fake cannot judge the condition {atom:?}")
}

/// OR binds looser than AND.
fn condition_holds(spec: &UpdateSpec, image: &Image) -> bool {
    let Some(condition) = spec.condition.as_deref() else {
        return true;
    };
    condition.split(" OR ").any(|alternative| {
        alternative
            .split(" AND ")
            .all(|atom| atom_holds(spec, image, atom.trim()))
    })
}

/// The value of a SET right-hand side against the item as it was: `:v`, `if_not_exists(path, :v)`, and either plus `:n` for a counter.
fn right_side(spec: &UpdateSpec, image: &Image, text: &str) -> Value {
    if let Some((left, right)) = text.rsplit_once(" + ") {
        let number = |v: Value| v.as_i64().unwrap_or_else(|| panic!("{v} is not a number"));
        return json!(
            number(right_side(spec, image, left)) + number(right_side(spec, image, right))
        );
    }
    if let Some(inner) = text
        .strip_prefix("if_not_exists(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts = split_top(inner);
        assert_eq!(parts.len(), 2, "if_not_exists takes a path and a value");
        return image
            .get(&attribute_of(spec, parts[0]))
            .cloned()
            .unwrap_or_else(|| value_of(spec, parts[1]));
    }
    value_of(spec, text.trim())
}

/// Applies `spec` to the item at `key` when its condition holds; `false` when it does not (nothing changes).
fn apply_spec(
    index: &mut BTreeMap<(String, String), RunIndexItem>,
    key: &(String, String),
    spec: &UpdateSpec,
) -> bool {
    let mut image: Image = match index.get(key).map(serde_json::to_value) {
        Some(Ok(Value::Object(map))) => map,
        Some(other) => panic!("an item is not an object: {other:?}"),
        None => Image::new(),
    };
    if !condition_holds(spec, &image) {
        return false;
    }
    let body = spec.expression.strip_prefix("SET ").unwrap_or_else(|| {
        panic!(
            "this fake reads SET [REMOVE] expressions: {}",
            spec.expression
        )
    });
    let (sets, removes) = body.split_once(" REMOVE ").unwrap_or((body, ""));
    let mut writes: Vec<(String, Value)> = Vec::new();
    for action in split_top(sets) {
        let (path, right) = action
            .split_once(" = ")
            .unwrap_or_else(|| panic!("a SET action is path = value: {action}"));
        writes.push((
            attribute_of(spec, path.trim()),
            right_side(spec, &image, right.trim()),
        ));
    }
    for path in split_top(removes).into_iter().filter(|p| !p.is_empty()) {
        image.remove(&attribute_of(spec, path));
    }
    image.extend(writes);
    let item: RunIndexItem = serde_json::from_value(Value::Object(image))
        .unwrap_or_else(|e| panic!("the item cannot be read after the update: {e}"));
    index.insert(key.clone(), item);
    true
}

impl World {
    fn shard_events(&self, run_id: &str, shard: u8) -> Vec<Envelope> {
        let mut v: Vec<Envelope> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.run_id == run_id && event_shard(e.authority, e.agent_id.as_deref()) == shard
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        v
    }
}

impl Blobs for &World {
    async fn put(&self, key: &str, body: &[u8]) -> Result<String, StoreError> {
        self.log(format!("blob_put {key}"));
        self.objects
            .lock()
            .unwrap()
            .insert(key.into(), body.to_vec());
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
        self.presign_get_version(key, None, ttl).await
    }
    async fn presign_get_version(
        &self,
        key: &str,
        version_id: Option<&str>,
        ttl: Duration,
    ) -> Result<DownloadUrlResponse, StoreError> {
        self.log(format!("presign {key}"));
        self.presigned_versions
            .lock()
            .unwrap()
            .push(version_id.map(str::to_string));
        if self.presign_down.load(Ordering::SeqCst) {
            return Err(unavailable("presign"));
        }
        assert!(ttl <= MAX_PRESIGN, "a SigV4 URL lives at most 7 days");
        self.presigned.lock().unwrap().push((key.into(), ttl));
        Ok(DownloadUrlResponse {
            url: format!(
                "https://{BUCKET}.s3.us-east-1.amazonaws.com/{key}?X-Amz-Signature=0123456789abcdef&X-Amz-Security-Token=SESSIONTOKENVALUE"
            ),
            expires_at: "2026-10-15T13:25:00Z".into(),
        })
    }
    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        self.log(format!("exists {key}"));
        if self.objects_down.load(Ordering::SeqCst) {
            return Err(unavailable("object list"));
        }
        Ok(self.objects.lock().unwrap().contains_key(key))
    }
    async fn read_prefix(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<Option<ObjectPrefix>, StoreError> {
        self.log(format!("read_prefix {key} {max_bytes}"));
        if self.objects_down.load(Ordering::SeqCst) {
            return Err(unavailable("object read"));
        }
        // S3 answers a ranged GET with a first byte past the end by 416, and a range of zero bytes is not a range.
        assert!(max_bytes >= 1, "a ranged GET needs at least one byte");
        let g = self.objects.lock().unwrap();
        let Some(bytes) = g.get(key) else {
            if self.no_list_permission.load(Ordering::SeqCst) {
                return Err(unavailable("s3 GET returned 403 AccessDenied"));
            }
            return Ok(None);
        };
        let take = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        Ok(Some(ObjectPrefix {
            bytes: bytes[..take].to_vec(),
            size: bytes.len() as u64,
            etag: Some(etag_of(bytes)),
            version_id: Some(version_of(bytes)),
            last_modified: Some(
                self.modified
                    .lock()
                    .unwrap()
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| "2020-01-01T00:00:00Z".to_string()),
            ),
        }))
    }
}

/// The S3 version id of an object holding `bytes` in this world.
pub fn version_of(bytes: &[u8]) -> String {
    format!("v-{}", &control_api::ids::sha256_hex(bytes)[..16])
}

/// The ETag a single-part upload of `bytes` has in this world (quoted, as S3 sends it).
pub fn etag_of(bytes: &[u8]) -> String {
    format!("\"{}\"", &control_api::ids::sha256_hex(bytes)[..32])
}

impl Execution for &World {
    async fn start(&self, name: &str, _: &Value) -> Result<(), StoreError> {
        self.started.lock().unwrap().push(name.into());
        Ok(())
    }
    async fn deployment(&self) -> Result<DeployedAs, StoreError> {
        Ok(DeployedAs::default())
    }
    async fn instances_available(&self) -> Result<bool, StoreError> {
        self.log("instances_available");
        if self.instances_down.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable(
                "the parameter read is throttled".into(),
            ));
        }
        Ok(self.instances.load(Ordering::SeqCst))
    }
    async fn deploy_lock(&self) -> Result<Option<control_api::handler::DeployLock>, StoreError> {
        self.log("deploy_lock");
        self.deploy_lock.lock().unwrap().clone().transpose()
    }
}

/// `owner_pk` for assertions about where an item sits.
pub fn pk(owner: &str) -> String {
    owner_pk(owner)
}
