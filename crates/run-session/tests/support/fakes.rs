//! One in-memory world behind every port of the crate. A `Mem` is cheap to clone; clones share the world, so a test
//! reads what the ops wrote.

use dispatcher::endpoints::{EndpointState, Pin};
use ham_core::run_index::{owner_pk, search_sk};
use ham_core::runtag::dynamo::{self, Answered, Av, QuerySpec, UpdateSpec};
use ham_core::runtag::edit::TagState;
use ham_core::runtag::vocab::VocabItem;
use ham_core::utility::{
    KNOWN_ROUTE_IDS, UtilityFault, UtilityModel, UtilityModels, UtilityPurpose, UtilityReply,
    bedrock_rejects_with, parse_reply,
};
use harness_protocol::api::internal::ProcessCredentials;
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{DirectSummary, s3};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{AgentItem, GoalItem, RunItem, TurnItem};
use harness_protocol::target::RunTarget;
use run_session::compute::ComputeStage;
use run_session::error::RunSessionError;
use run_session::name_run::{LogSink, NameBudget, Namer};
use run_session::ops::{Config, Deps, ResearchStage};
use run_session::ports::{Blobs, PackageTokens, Res, Sessions, Store, TagMarks, Tokens, VocabRead};
use run_session::tag_run::TagBudget;
use run_session::utility::{UtilityCall, UtilityClient, UtilityFuture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use worker_api::creds::MintPlan;
use worker_api::handler::{MintError, Minter, TokenContext};

pub const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
pub const BUCKET: &str = "hamh-direct-harness-700000000011";
pub const RUNTIME: &str = "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-AbCdEf";
pub const INSTANCES: &str = "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/hamh_direct_harness_worker_python_instances-XyZaBc";
pub const PROVIDER: &str = "hamh_direct_instances-AbCdEfGhIj";
pub const MACHINE: &str = "arn:aws:states:us-east-1:700000000011:stateMachine:hamh-direct-run";
pub const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
pub const MODEL: &str = "us.anthropic.claude-opus-5-5";
pub const HEAD: &str = "1111111111111111111111111111111111111111";
pub const BASE: &str = "2222222222222222222222222222222222222222";
pub const T0: u64 = 1_790_000_000_000;

pub fn hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// One recorded `touch_run_index` call: (owner, the run's createdAt, run id, outcome, last activity).
pub type IndexTouch = (String, String, String, Option<String>, String);

#[derive(Default)]
pub struct Inner {
    pub runs: Mutex<BTreeMap<String, RunItem>>,
    pub goals: Mutex<BTreeMap<String, GoalItem>>,
    pub agents: Mutex<BTreeMap<String, AgentItem>>,
    pub turns: Mutex<BTreeMap<(String, u32), TurnItem>>,
    pub events: Mutex<BTreeMap<String, Envelope>>,
    pub blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    pub endpoints: Mutex<BTreeMap<String, EndpointState>>,
    pub invocations: Mutex<Vec<Vec<u8>>>,
    /// Every `put_agent` call, kept or not (the item that exists is the first one).
    pub agent_writes: Mutex<Vec<AgentItem>>,
    /// Another writer moves the run to this status just before the next `set_run_status` (a lost race).
    pub race_status: Mutex<Option<GoalStatus>>,
    /// `stop` answers `false`: AgentCore no longer has the session.
    pub stop_gone: Mutex<bool>,
    /// Another writer creates this object just before the next `put_if_absent` of its key (a lost create-only write).
    pub blob_race: Mutex<Option<(String, Vec<u8>)>>,
    /// Keys the role may not read: a read of one answers an error, as S3 does for a role without the grant, and not "absent".
    pub deny_read: Mutex<std::collections::BTreeSet<String>>,
    /// Every version a key has had, by (key, version id). A plain `put_blob` makes none; `put_version` makes one and the current object.
    pub versions: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    pub version_seq: Mutex<u64>,
    /// Every read of a version the service made, as (key, version id); `None` is a read of the current version.
    pub version_reads: Mutex<Vec<(String, Option<String>)>>,
    /// Keep `creds.outputPinsRequired` on the Agent item `start-session` writes. The machine and delivery suites were written for runs that
    /// do not require the version records of the end-of-run objects, so by default the fake store drops the member when it keeps the item;
    /// `tests/b759_pinned_delivery.rs` is the suite for the other mode.
    pub keep_pins_flag: Mutex<bool>,
    pub stops: Mutex<Vec<(String, String)>>,
    /// `Store::close_messages` calls: the run and how many session stops had happened by then (the close comes first).
    pub message_closes: Mutex<Vec<(String, usize)>>,
    pub encrypted: Mutex<Vec<String>>,
    pub minted: Mutex<Vec<MintPlan>>,
    /// Failures to raise, by call name (`invoke`, `stop`, `put_agent`, `abort_run`, `set_run_status`, ...), in order.
    pub failures: Mutex<BTreeMap<String, VecDeque<Option<RunSessionError>>>>,
    /// Calls to `abort_run` that report a lost race.
    pub abort_races: Mutex<u32>,
    pub package_token: Mutex<Option<Result<Option<String>, String>>>,
    pub clock: Mutex<u64>,
    pub invoke_reply: Mutex<Option<Vec<u8>>>,
    /// What `Sessions::instances_runtime_arn` answers; `None` is "the stage has no Instances runtime".
    pub instances_lookup: Mutex<Option<Result<String, RunSessionError>>>,
    /// What `Sessions::capacity_provider_id` answers (the SSM lookup of the real adapter); `None` is "the stage names no parameter".
    pub provider_lookup: Mutex<Option<Result<String, RunSessionError>>>,
    /// How many times the provider lookup was asked.
    pub provider_lookups: Mutex<u32>,
    /// The endpoints of the Instances runtime, by name; empty: it has none and a run uses `DEFAULT`.
    pub instances_endpoints: Mutex<BTreeMap<String, EndpointState>>,
    /// The compute stage of the config `Mem::config` builds; `None` is the default stage.
    pub compute_cfg: Mutex<Option<ComputeStage>>,
    /// Every invoke as (runtime ARN, qualifier, session id).
    pub invoke_targets: Mutex<Vec<(String, String, String)>>,
    /// A discard (or any other writer) moves the run to this status after this many more reads of the Run item: the read that brings the count to
    /// zero still sees the old status, every later read sees the new one.
    pub flip_run_after_reads: Mutex<Option<(u32, GoalStatus)>>,
    /// The same move, made while the next `invoke` runs (also when that invoke then fails or is answered with something unreadable): every read
    /// after it sees the new status.
    pub flip_run_on_invoke: Mutex<Option<GoalStatus>>,
    /// Every `delete_provider_session` as (provider id, session id).
    pub provider_deletes: Mutex<Vec<(String, String)>>,
    /// `delete_provider_session` answers `false`: the session had already ended.
    pub provider_gone: Mutex<bool>,
    /// The run ids that hold an Instances slot, and every claim and release in order (`claim <run> <max>`, `release <run>`).
    pub slots: Mutex<BTreeSet<String>>,
    pub slot_calls: Mutex<Vec<String>>,
    /// The calls that end a session, in order: `stop`, `delete_provider_session`, `release_slot`, `end_agent`.
    pub ending: Mutex<Vec<String>>,
    /// The `runtime_arn` of the config `Mem::config` builds; `None` is the usual [`RUNTIME`].
    pub runtime_cfg: Mutex<Option<String>>,
    /// The `slice_deadline_s` of the config `Mem::config` builds; `None` is the full slice.
    pub slice_deadline_cfg: Mutex<Option<u32>>,
    /// The `provider_silence_s` of the config `Mem::config` builds; `None` leaves agentd's own silence limit.
    pub provider_silence_cfg: Mutex<Option<u32>>,
    /// What `Sessions::runtime_arn` answers (the SSM lookup of the real adapter); `None` is "this adapter has no lookup".
    pub runtime_lookup: Mutex<Option<Result<String, RunSessionError>>>,
    /// How many times the lookup was asked.
    pub runtime_lookups: Mutex<u32>,
    /// The run's Agent item registers `transcript.v1`.
    pub transcript_supported: Mutex<bool>,
    /// The answers the run's user gave.
    pub answers: Mutex<Vec<String>>,
    /// Every search-body write.
    pub search_writes: Mutex<Vec<SearchRecord>>,
    /// Every list call (prefix, start-after, max-keys) and every bounded get (key, max bytes).
    pub lists: Mutex<Vec<(String, Option<String>, usize)>>,
    pub bounded_gets: Mutex<Vec<(String, usize)>>,
    /// Sizes the listing reports for keys whose bytes are different (a hostile object).
    pub listed_sizes: Mutex<BTreeMap<String, u64>>,
    /// Every `touch_run_index` call: (owner, run's createdAt, run id, outcome, last activity), in order.
    pub index_touches: Mutex<Vec<IndexTouch>>,
    /// The research settings of the config `Mem::config` builds; `None` leaves research off.
    pub research_cfg: Mutex<Option<ResearchStage>>,
    /// Every research policy row written, in order.
    pub research_rows: Mutex<Vec<harness_protocol::research::PolicyItem>>,
    /// Every event the store accepted, as `(eventId, type)`, in the order the writes happened (EVT1). `events` is sorted by id and
    /// cannot show this: a reader that follows "ids above the last one seen" sees the writes in this order.
    pub event_writes: Mutex<Vec<(String, String)>>,
    /// The run-name flow in the order it happened: the store calls it makes (`get_run`, `get_root_goal`, `run_name`, `put_run_name`)
    /// and the model call (`converse`), in one log shared with [`ScriptedUtilityClient`].
    pub calls: Arc<Mutex<Vec<String>>>,
    /// The utility model of the config `Mem::config` builds; `None` is the setting `off` (no read, no call).
    pub utility_cfg: Mutex<Option<UtilityModel>>,
    /// The client and the budget `Mem::deps` hands the namer; `None` is a namer with no client.
    pub namer: Mutex<Option<(Arc<ScriptedUtilityClient>, NameBudget, TagBudget)>>,
    /// The purposes of the config `Mem::config` builds; `None` is the run name alone, as a function without `UTILITY_PURPOSES`.
    pub purposes_cfg: Mutex<Option<Vec<UtilityPurpose>>>,
    /// Every tag writer call as the fake applied it, in order.
    pub tag_writes: Mutex<Vec<TagWrite>>,
    /// Every vocabulary read, in order.
    pub tag_queries: Mutex<Vec<TagQuery>>,
    /// When each write reached the store (`put_run_name`, `put_run_tags_inherit`, `put_run_tags_auto`), in order.
    pub write_times: Mutex<Vec<(String, Instant)>>,
    /// Another writer changes the item just before the next write of this kind (`inherit`, `auto`): the person edits between the
    /// branch's read and its write.
    pub before_write: Mutex<Vec<(String, TagHook)>>,
    /// Store calls that never answer (by call name).
    pub stalled: Mutex<std::collections::BTreeSet<String>>,
    /// The per-operation JSON lines the naming op logged, in order.
    pub name_logs: Mutex<Vec<String>>,
    /// The search items as the DynamoDB adapters leave them: `(PK, SK)` to attribute to value. `touch_run_index` and `put_run_name`
    /// write here by the rules of their `UpdateItem` expressions (`aws.rs`), which `tests/aws_run_index.rs` and `tests/aws_run_name.rs`
    /// hold to the wire.
    pub search_items: Mutex<BTreeMap<(String, String), BTreeMap<String, String>>>,
    /// Every `put_run_name` call, kept or not.
    pub name_writes: Mutex<Vec<NameWrite>>,
    /// Another invocation writes this name just before the next `put_run_name` (a lost race for the one name).
    pub name_race: Mutex<Option<String>>,
    /// The next `put_run_name` that stores its name loses its answer: the write happened and the caller sees an error.
    pub lose_name_response: Mutex<bool>,
}

/// A change another writer makes to the tag attributes of the item just before a write of the code under test.
pub type TagHook = Box<dyn FnOnce(&mut TagItem) + Send>;

/// One `put_run_name` call as the fake recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameWrite {
    pub run_id: String,
    pub name: String,
    pub source: String,
    pub model: String,
    /// The name was stored (the condition `attribute_not_exists(runName)` held).
    pub stored: bool,
}

/// One search-body write as the fake recorded it.
#[derive(Clone, Debug)]
pub struct SearchRecord {
    pub pk: String,
    pub sk: String,
    pub run_id: String,
    pub created_at: String,
    pub body: ham_core::runsearch::SearchBody,
    pub indexed_at: String,
}

#[derive(Clone)]
pub struct Mem(pub Arc<Inner>);

impl Default for Mem {
    fn default() -> Self {
        let m = Mem(Arc::new(Inner::default()));
        *m.0.package_token.lock().unwrap() = Some(Ok(Some("tok.en-1234567890".into())));
        *m.0.clock.lock().unwrap() = T0;
        m.0.endpoints.lock().unwrap().insert(
            "live".into(),
            EndpointState {
                name: "live".into(),
                status: "READY".into(),
                live_version: Some("7".into()),
            },
        );
        m.0.endpoints.lock().unwrap().insert(
            "v7".into(),
            EndpointState {
                name: "v7".into(),
                status: "READY".into(),
                live_version: Some("7".into()),
            },
        );
        m
    }
}

static CLOCK: Mutex<u64> = Mutex::new(T0);

fn clock() -> u64 {
    *CLOCK.lock().unwrap()
}

/// Moves the clock every `Mem` config reads (tests that need time to pass run one at a time per binary or set it first).
pub fn set_clock(ms: u64) {
    *CLOCK.lock().unwrap() = ms;
}

impl Mem {
    pub fn config(&self) -> Config {
        Config {
            bucket: BUCKET.into(),
            internal_api_url: "https://abc.execute-api.us-east-1.amazonaws.com/direct".into(),
            region: "us-east-1".into(),
            runtime_arn: self
                .0
                .runtime_cfg
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| RUNTIME.into()),
            tenant: "testuser".into(),
            max_lifetime_s: 28_800,
            run_state_machine_arn: MACHINE.into(),
            research: self.0.research_cfg.lock().unwrap().clone(),
            utility: self.0.utility_cfg.lock().unwrap().clone(),
            utility_purposes: self
                .0
                .purposes_cfg
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| vec![UtilityPurpose::RunName]),
            compute: self
                .0
                .compute_cfg
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default(),
            slice_deadline_s: *self.0.slice_deadline_cfg.lock().unwrap(),
            provider_silence_s: *self.0.provider_silence_cfg.lock().unwrap(),
            clock_ms: clock,
        }
    }

    /// Every slice's prompt deadline is `s` seconds at most (the stage's `HAMH_SLICE_DEADLINE_S`).
    /// Every invocation carries `providerSilenceS = s` (the stage's `HAMH_PROVIDER_SILENCE_S`).
    pub fn set_provider_silence(&self, s: u32) {
        *self.0.provider_silence_cfg.lock().unwrap() = Some(s);
    }

    pub fn set_slice_deadline(&self, s: u32) {
        *self.0.slice_deadline_cfg.lock().unwrap() = Some(s);
    }

    pub fn deps(&self) -> Deps<Mem, Mem, Mem, Mem, Mem, Mem> {
        Deps {
            store: self.clone(),
            tokens: self.clone(),
            minter: self.clone(),
            sessions: self.clone(),
            blobs: self.clone(),
            packages: self.clone(),
            namer: self.namer(),
            cfg: self.config(),
        }
    }

    /// The namer of the deployed function: the client of [`Mem::enable_naming`] (none until then), and a log that collects the lines
    /// the op prints, so a test can read them.
    pub fn namer(&self) -> Namer {
        let inner = self.0.clone();
        let log: LogSink =
            Arc::new(move |line: &str| inner.name_logs.lock().unwrap().push(line.to_string()));
        match self.0.namer.lock().unwrap().as_ref() {
            Some((client, budget, tag_budget)) => Namer {
                client: Some(client.clone() as Arc<dyn UtilityClient>),
                budget: *budget,
                tag_budget: *tag_budget,
                log,
            },
            None => Namer {
                client: None,
                budget: quick_budget(),
                tag_budget: quick_tag_budget(),
                log,
            },
        }
    }

    /// Run names on: the config carries the catalog's default utility model and the namer asks a client that replays `answers`.
    pub fn enable_naming(&self, answers: Vec<Scripted>) -> Arc<ScriptedUtilityClient> {
        self.enable_naming_with(answers, quick_budget())
    }

    pub fn enable_naming_with(
        &self,
        answers: Vec<Scripted>,
        budget: NameBudget,
    ) -> Arc<ScriptedUtilityClient> {
        let client = Arc::new(ScriptedUtilityClient::new(answers, self.0.calls.clone()));
        *self.0.utility_cfg.lock().unwrap() =
            Some(UtilityModels::builtin().default_model().clone());
        *self.0.namer.lock().unwrap() = Some((client.clone(), budget, quick_tag_budget()));
        client
    }

    /// Run names and run tags on: the config carries the catalog's default utility model and both purposes, the namer asks a client that
    /// replays `name_answers` for names and `tag_answers` for tags (the two kinds of request are told apart by their stop sequence), and
    /// the run's search item exists as the start's write leaves it.
    pub fn enable_tagging(
        &self,
        name_answers: Vec<Scripted>,
        tag_answers: Vec<Scripted>,
    ) -> Arc<ScriptedUtilityClient> {
        self.enable_tagging_with(
            name_answers,
            tag_answers,
            quick_budget(),
            quick_tag_budget(),
        )
    }

    pub fn enable_tagging_with(
        &self,
        name_answers: Vec<Scripted>,
        tag_answers: Vec<Scripted>,
        name_budget: NameBudget,
        tag_budget: TagBudget,
    ) -> Arc<ScriptedUtilityClient> {
        let client = Arc::new(
            ScriptedUtilityClient::new(name_answers, self.0.calls.clone())
                .with_tag_answers(tag_answers),
        );
        *self.0.utility_cfg.lock().unwrap() =
            Some(UtilityModels::builtin().default_model().clone());
        *self.0.namer.lock().unwrap() = Some((client.clone(), name_budget, tag_budget));
        *self.0.purposes_cfg.lock().unwrap() =
            Some(vec![UtilityPurpose::RunName, UtilityPurpose::RunTags]);
        self.seed_start_item();
        client
    }

    /// The search item `control-api` writes when the run starts (the head, the model, the repository, the time), when the run exists.
    fn seed_start_item(&self) {
        let Some(run) = self.0.runs.lock().unwrap().get(RUN).cloned() else {
            return;
        };
        let prompt = self
            .0
            .goals
            .lock()
            .unwrap()
            .get(RUN)
            .map(|g| g.objective.clone())
            .unwrap_or_default();
        let mut items = self.0.search_items.lock().unwrap();
        let item = items.entry(search_key(&run)).or_default();
        item.insert("runId".into(), run.run_id.clone());
        item.insert("createdAt".into(), run.created_at.clone());
        item.insert("promptHead".into(), prompt);
        item.insert(
            "repository".into(),
            ham_core::run_index::target_text(&run.target),
        );
        item.insert("lastActivity".into(), run.created_at.clone());
    }

    /// Which purposes the config serves.
    pub fn set_purposes(&self, purposes: &[UtilityPurpose]) {
        *self.0.purposes_cfg.lock().unwrap() = Some(purposes.to_vec());
    }

    /// The catalog row the config carries (`None` is the setting `off`).
    pub fn set_utility_model(&self, model: Option<UtilityModel>) {
        *self.0.utility_cfg.lock().unwrap() = model;
    }

    /// The tag attributes of the run's search item as the tag writers left them, `None` while there is no item.
    pub fn tag_item(&self) -> Option<TagItem> {
        self.0
            .search_items
            .lock()
            .unwrap()
            .get(&search_key(&self.run()))
            .map(TagItem::read)
    }

    /// The tag attributes of another run's search item in the same owner's partition (a parent).
    pub fn tag_item_of(&self, run_id: &str) -> Option<TagItem> {
        let run = self.0.runs.lock().unwrap().get(run_id).cloned()?;
        self.0
            .search_items
            .lock()
            .unwrap()
            .get(&search_key(&run))
            .map(TagItem::read)
    }

    /// Puts tag attributes on the run's search item, as an earlier writer (a person's edit) left them.
    pub fn seed_tag_item(&self, tags: TagItem) {
        let run = self.run();
        let mut items = self.0.search_items.lock().unwrap();
        let item = items.entry(search_key(&run)).or_default();
        item.entry("runId".into())
            .or_insert_with(|| run.run_id.clone());
        item.entry("createdAt".into())
            .or_insert_with(|| run.created_at.clone());
        tags.write_into(item);
    }

    /// A search item of some run of `owner` with these tags, in that owner's partition: the window the vocabulary is read from.
    pub fn seed_other_item(&self, owner: &str, created_at: &str, run_id: &str, tags: TagItem) {
        let mut items = self.0.search_items.lock().unwrap();
        let item = items
            .entry((owner_pk(owner), search_sk(created_at, run_id)))
            .or_default();
        item.insert("runId".into(), run_id.to_string());
        item.insert("createdAt".into(), created_at.to_string());
        tags.write_into(item);
    }

    /// The run this one continues, of the same owner, finished earlier, with its search item holding `tags`.
    pub fn seed_parent(&self, parent_id: &str, tags: TagItem) {
        self.seed_parent_of(OWNER, parent_id, tags);
    }

    /// As [`Mem::seed_parent`], with a parent of another principal.
    pub fn seed_parent_of(&self, owner: &str, parent_id: &str, tags: TagItem) {
        let mut parent = self.run();
        parent.run_id = parent_id.to_string();
        parent.owner_principal_arn = owner.to_string();
        parent.created_at = "2026-10-13T17:00:00Z".to_string();
        parent.continue_from = None;
        parent.status = GoalStatus::Complete;
        self.seed_other_item(owner, &parent.created_at, parent_id, tags);
        self.0
            .runs
            .lock()
            .unwrap()
            .insert(parent_id.to_string(), parent);
        self.set_continue_from(parent_id);
    }

    /// The Run item says it continues `parent_id` (which may not exist).
    pub fn set_continue_from(&self, parent_id: &str) {
        self.0
            .runs
            .lock()
            .unwrap()
            .get_mut(RUN)
            .unwrap()
            .continue_from = Some(parent_id.to_string());
    }

    /// The tag writer calls so far, in order.
    pub fn tag_writes(&self) -> Vec<TagWrite> {
        self.0.tag_writes.lock().unwrap().clone()
    }

    /// The vocabulary reads so far, in order.
    pub fn tag_queries(&self) -> Vec<TagQuery> {
        self.0.tag_queries.lock().unwrap().clone()
    }

    /// When each name or tag write reached the store, in order.
    pub fn write_instants(&self) -> Vec<(String, Instant)> {
        self.0.write_times.lock().unwrap().clone()
    }

    /// Just before the next write of `kind` (`inherit` or `auto`) another writer changes the item with `f`.
    pub fn before_tag_write(&self, kind: &str, f: impl FnOnce(&mut TagItem) + Send + 'static) {
        self.0
            .before_write
            .lock()
            .unwrap()
            .push((kind.to_string(), Box::new(f)));
    }

    /// From now on the call `call` never answers.
    pub fn stall(&self, call: &str) {
        self.0.stalled.lock().unwrap().insert(call.to_string());
    }

    async fn maybe_stall(&self, call: &str) {
        let stalled = self.0.stalled.lock().unwrap().contains(call);
        if stalled {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }

    /// The write path of the two tag writers: the spec is checked against what DynamoDB refuses, a hook may play another writer, and the
    /// spec's own condition and update expression are applied to the item (an upsert, as `UpdateItem` is). `false` when the condition fails.
    fn apply_tag_write(&self, kind: &'static str, run: &RunItem, spec: &UpdateSpec) -> Res<bool> {
        if let Some(why) = dynamo::expr_rejects(spec) {
            panic!("DynamoDB would refuse the {kind} write: {why}\n{spec:#?}");
        }
        let hook = {
            let mut hooks = self.0.before_write.lock().unwrap();
            hooks
                .iter()
                .position(|(k, _)| k == kind)
                .map(|i| hooks.remove(i).1)
        };
        let key = search_key(run);
        let mut items = self.0.search_items.lock().unwrap();
        let existed = items.contains_key(&key);
        let item = items.entry(key.clone()).or_default();
        if let Some(f) = hook {
            let mut tags = TagItem::read(item);
            f(&mut tags);
            tags.write_into(item);
        }
        let applied = apply_spec(item, spec).unwrap_or_else(|e| panic!("{kind}: {e}\n{spec:#?}"));
        if !applied && !existed {
            items.remove(&key);
        }
        self.0.tag_writes.lock().unwrap().push(TagWrite {
            kind,
            run_id: run.run_id.clone(),
            stored: applied,
            expression: spec.expression.clone(),
        });
        self.0
            .write_times
            .lock()
            .unwrap()
            .push((format!("put_run_tags_{kind}"), Instant::now()));
        Ok(applied)
    }

    /// The store and model calls of the naming flow so far, in order.
    pub fn name_calls(&self) -> Vec<String> {
        self.0.calls.lock().unwrap().clone()
    }

    /// The JSON lines the naming op logged.
    pub fn name_logs(&self) -> Vec<String> {
        self.0.name_logs.lock().unwrap().clone()
    }

    pub fn name_writes(&self) -> Vec<NameWrite> {
        self.0.name_writes.lock().unwrap().clone()
    }

    /// The run's search item as the adapters left it, `None` while no adapter wrote it.
    pub fn search_item(&self) -> Option<BTreeMap<String, String>> {
        self.0
            .search_items
            .lock()
            .unwrap()
            .get(&search_key(&self.run()))
            .cloned()
    }

    /// The deployed Lambda: the runtime ARN is not in the config (the function does not read it at start), and
    /// `Sessions::runtime_arn` answers `lookup` on every call, as the SSM read of `AgentCoreSessions` does.
    pub fn runtime_from_lookup(&self, lookup: Result<String, RunSessionError>) {
        *self.0.runtime_cfg.lock().unwrap() = Some(String::new());
        *self.0.runtime_lookup.lock().unwrap() = Some(lookup);
    }

    /// The stage has an Instances worker runtime at `arn`.
    pub fn instances_runtime(&self, arn: &str) {
        *self.0.instances_lookup.lock().unwrap() = Some(Ok(arn.to_string()));
    }

    /// The compute settings of the function.
    pub fn set_compute_stage(&self, stage: ComputeStage) {
        *self.0.compute_cfg.lock().unwrap() = Some(stage);
    }

    /// The stage's capacity provider has this id (the value of its SSM parameter).
    pub fn capacity_provider(&self, id: &str) {
        *self.0.provider_lookup.lock().unwrap() = Some(Ok(id.to_string()));
    }

    /// The lookup of the capacity provider's parameter fails with `e`.
    pub fn capacity_provider_fails(&self, e: RunSessionError) {
        *self.0.provider_lookup.lock().unwrap() = Some(Err(e));
    }

    pub fn provider_lookups(&self) -> u32 {
        *self.0.provider_lookups.lock().unwrap()
    }

    /// A stage with the Instances runtime, its capacity provider and the policy (and the cap of active sessions).
    pub fn instances_stage(
        &self,
        policy: run_session::compute::ComputePolicy,
        max_active: Option<u32>,
    ) {
        self.instances_runtime(INSTANCES);
        self.capacity_provider(PROVIDER);
        self.set_compute_stage(ComputeStage { policy, max_active });
    }

    /// A discard lands after `reads` more reads of the Run item (the read that makes the count zero still sees the old status).
    pub fn flip_run_after_reads(&self, reads: u32, to: GoalStatus) {
        *self.0.flip_run_after_reads.lock().unwrap() = Some((reads, to));
    }

    /// A discard lands while the next invoke runs.
    pub fn flip_run_on_invoke(&self, to: GoalStatus) {
        *self.0.flip_run_on_invoke.lock().unwrap() = Some(to);
    }

    pub fn invoke_targets(&self) -> Vec<(String, String, String)> {
        self.0.invoke_targets.lock().unwrap().clone()
    }

    pub fn provider_deletes(&self) -> Vec<(String, String)> {
        self.0.provider_deletes.lock().unwrap().clone()
    }

    pub fn set_provider_gone(&self, gone: bool) {
        *self.0.provider_gone.lock().unwrap() = gone;
    }

    /// Another run holds an Instances slot.
    pub fn hold_slot(&self, run_id: &str) {
        self.0.slots.lock().unwrap().insert(run_id.to_string());
    }

    pub fn slot_holders(&self) -> Vec<String> {
        self.0.slots.lock().unwrap().iter().cloned().collect()
    }

    pub fn slot_calls(&self) -> Vec<String> {
        self.0.slot_calls.lock().unwrap().clone()
    }

    pub fn ending(&self) -> Vec<String> {
        self.0.ending.lock().unwrap().clone()
    }

    pub fn runtime_lookups(&self) -> u32 {
        *self.0.runtime_lookups.lock().unwrap()
    }

    pub fn fail(&self, call: &str, e: RunSessionError) {
        self.0
            .failures
            .lock()
            .unwrap()
            .entry(call.to_string())
            .or_default()
            .push_back(Some(e));
    }

    /// Lets the next call of `call` through (a later `fail` then hits the call after it).
    pub fn pass(&self, call: &str) {
        self.0
            .failures
            .lock()
            .unwrap()
            .entry(call.to_string())
            .or_default()
            .push_back(None);
    }

    fn failing(&self, call: &str) -> Res<()> {
        match self
            .0
            .failures
            .lock()
            .unwrap()
            .get_mut(call)
            .and_then(VecDeque::pop_front)
        {
            Some(Some(e)) => Err(e),
            _ => Ok(()),
        }
    }

    /// A direct run: the Run and Goal items, the stored request and its `run.created` event.
    pub fn seed(&self, target: RunTarget, prompt: &str, model: &str) -> StartDirectRunRequest {
        self.seed_with(target, prompt, model, None)
    }

    pub fn seed_with(
        &self,
        target: RunTarget,
        prompt: &str,
        model: &str,
        base_ref: Option<&str>,
    ) -> StartDirectRunRequest {
        let rules = vec![harness_protocol::api::v1::RuleFile {
            path: "00-my-rules.md".into(),
            sha256: hex(b"Write the test first.\n"),
            content: "Write the test first.\n".into(),
        }];
        let request = StartDirectRunRequest {
            input_wait_s: None,
            research: None,
            report_tests: None,
            report_step_s: None,
            goal: None,
            third_party: None,
            attachments: vec![],
            review: None,
            prompt: prompt.into(),
            target: target.clone(),
            base_ref: base_ref.map(str::to_string),
            model: model.into(),
            rules,
            nudge: None,
            skills: vec![],
            continue_from: None,
            engine_config: None,
            compute: None,
            build: None,
            max_run_s: None,
            limits: None,
            engine: None,
            provider_sharing: None,
        };
        let body = serde_json::to_vec(&request).unwrap();
        self.0
            .blobs
            .lock()
            .unwrap()
            .insert(s3::request_key(RUN), body.clone());
        let run: RunItem = serde_json::from_value(json!({
            "runId": RUN, "ownerPrincipalArn": OWNER, "promptSha256": hex(prompt.as_bytes()),
            "target": serde_json::to_value(&target).unwrap(), "status": "active", "phase": "planning",
            "enginePolicyVersion": 0, "createdAt": "2026-10-13T18:00:00Z", "version": 1,
            "baseRef": base_ref,
        }))
        .unwrap();
        self.0.runs.lock().unwrap().insert(RUN.into(), run);
        let goal: GoalItem = serde_json::from_value(json!({
            "runId": RUN, "objective": prompt, "requirements": [], "specSha256": "", "acceptanceSha256": "",
            "status": "active", "phase": "planning", "version": 1
        }))
        .unwrap();
        self.0.goals.lock().unwrap().insert(RUN.into(), goal);
        let created = harness_protocol::envelope::Envelope {
            schema_version: harness_protocol::envelope::SCHEMA_VERSION,
            event_id: "01M4X3Q7R9S0V1W2X3Y4Z5A6B0".into(),
            run_id: RUN.into(),
            task_id: None,
            agent_id: None,
            seq: 0,
            ts: "2026-10-13T18:00:00Z".into(),
            event_type: EventType::RunCreated,
            authority: harness_protocol::envelope::Authority::Harness,
            payload: Payload::Inline {
                value: json!({
                    "target": serde_json::to_value(&target).unwrap(),
                    "requestUri": format!("s3://{BUCKET}/{}", s3::request_key(RUN)),
                    "requestSha256": hex(&body), "mode": "direct",
                }),
            },
        };
        self.0
            .events
            .lock()
            .unwrap()
            .insert(created.event_id.clone(), created);
        request
    }

    /// Replaces the stored request object with `body` and makes the `run.created` event name its digest, so `read_request`
    /// accepts it: a request as another version of the control plane stored it.
    pub fn replace_request(&self, body: Vec<u8>) {
        let digest = hex(&body);
        self.put_blob(&s3::request_key(RUN), body);
        for e in self.0.events.lock().unwrap().values_mut() {
            if e.event_type == EventType::RunCreated
                && let Payload::Inline { value } = &mut e.payload
            {
                value["requestSha256"] = json!(digest);
            }
        }
    }

    /// `seed`, then the members of `extra` (a JSON object) are appended to the stored request object as text, after the members
    /// the type writes in its own order, the way a later field of the type would be.
    pub fn seed_with_members(&self, target: RunTarget, prompt: &str, model: &str, extra: Value) {
        self.seed(target, prompt, model);
        let members = extra.as_object().unwrap();
        if members.is_empty() {
            return;
        }
        let mut text = String::from_utf8(self.blob(&s3::request_key(RUN)).unwrap()).unwrap();
        assert_eq!(text.pop(), Some('}'));
        for (k, v) in members {
            text.push_str(&format!(",{}:{v}", serde_json::to_string(k).unwrap()));
        }
        text.push('}');
        self.replace_request(text.into_bytes());
    }

    /// The raw bytes of the n-th invocation body the fake AgentCore received.
    pub fn invocation_bytes(&self, n: usize) -> Vec<u8> {
        self.0.invocations.lock().unwrap()[n].clone()
    }

    pub fn put_blob(&self, key: &str, body: Vec<u8>) {
        self.0.blobs.lock().unwrap().insert(key.into(), body);
    }

    pub fn blob(&self, key: &str) -> Option<Vec<u8>> {
        self.0.blobs.lock().unwrap().get(key).cloned()
    }

    /// A PUT to a versioned bucket. `body` becomes a new version of `key` and the current object; returns the version id.
    pub fn put_version(&self, key: &str, body: Vec<u8>) -> String {
        let mut seq = self.0.version_seq.lock().unwrap();
        *seq += 1;
        let id = format!("version-{seq}");
        self.0
            .versions
            .lock()
            .unwrap()
            .insert((key.to_string(), id.clone()), body.clone());
        self.put_blob(key, body);
        id
    }

    /// Agentd's own write of an end-of-run object: stored as a version, and the record it sends in `turn-complete`.
    pub fn agentd_writes(
        &self,
        key: &str,
        body: Vec<u8>,
    ) -> harness_protocol::output_pins::OutputPin {
        let id = self.put_version(key, body.clone());
        harness_protocol::output_pins::OutputPin::for_body(&body, Some(id))
    }

    /// From now on the fake store keeps the `outputPinsRequired` member of the Agent item `start-session` writes.
    pub fn keep_pins_flag(&self) {
        *self.0.keep_pins_flag.lock().unwrap() = true;
    }

    /// The run's Agent item says the run requires the version records of its end-of-run objects (what `start-session` writes).
    pub fn put_pinned_agent(&self) {
        let agent: AgentItem = serde_json::from_value(json!({
            "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1, "runtimeArn": RUNTIME,
            "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline", "engineVersion": "3.0.71",
            "transport": "acp", "modelId": MODEL, "pool": "direct", "status": "running", "version": 1,
            "creds": {"nonceSha256": "00".repeat(32), "revoked": false, "tenant": "t", "mode": "m", "sessionPolicy": "{}",
                "completionKeySha256": "11".repeat(32), "outputPinsRequired": true}
        }))
        .unwrap();
        self.0.agents.lock().unwrap().insert("direct".into(), agent);
    }

    /// From now on the role is refused a read of `key` (an IAM policy without the grant).
    pub fn deny_read(&self, key: &str) {
        self.0.deny_read.lock().unwrap().insert(key.to_string());
    }

    pub fn put_summary(&self, s: &DirectSummary) {
        self.put_blob(&s3::summary_key(RUN), serde_json::to_vec(s).unwrap());
    }

    /// The final bundle exists at its pinned key.
    pub fn put_bundle(&self) {
        self.put_blob(&s3::bundle_key(RUN), b"BUNDLE".to_vec());
    }

    /// The final bundle of slice `turn` exists at its pinned key.
    pub fn put_bundle_t(&self, turn: u32) {
        self.put_blob(&s3::bundle_key_t(RUN, turn), b"BUNDLE".to_vec());
    }

    /// The summary of slice `turn`.
    pub fn put_summary_t(&self, turn: u32, s: &DirectSummary) {
        self.put_blob(
            &s3::summary_key_t(RUN, turn),
            serde_json::to_vec(s).unwrap(),
        );
    }

    /// The last periodic snapshot of slice `turn`, at `head`.
    pub fn put_snapshot_t(&self, turn: u32, head: &str) {
        self.put_blob(&s3::snapshot_bundle_key_t(RUN, turn), b"SNAP".to_vec());
        self.put_blob(
            &s3::snapshot_json_key_t(RUN, turn),
            serde_json::to_vec(
                &json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE,
                "at": "2026-10-13T18:50:00Z", "count": 5}),
            )
            .unwrap(),
        );
    }

    /// The Turn item of slice `turn`.
    pub fn turn_n(&self, turn: u32) -> Option<TurnItem> {
        self.0
            .turns
            .lock()
            .unwrap()
            .get(&("direct".into(), turn))
            .cloned()
    }

    /// The clock the config reads, set to `ms`.
    pub fn set_now_ms(&self, ms: u64) {
        set_clock(ms);
    }

    pub fn now_ms(&self) -> u64 {
        clock()
    }

    pub fn put_snapshot(&self, head: &str) {
        self.put_blob(&s3::snapshot_bundle_key(RUN), b"SNAP".to_vec());
        self.put_blob(
            &s3::snapshot_json_key(RUN),
            serde_json::to_vec(
                &json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE,
                "at": "2026-10-13T18:50:00Z", "count": 5}),
            )
            .unwrap(),
        );
    }

    pub fn run(&self) -> RunItem {
        self.0.runs.lock().unwrap().get(RUN).cloned().unwrap()
    }

    pub fn goal(&self) -> GoalItem {
        self.0.goals.lock().unwrap().get(RUN).cloned().unwrap()
    }

    pub fn set_run_status_raw(&self, status: GoalStatus) {
        self.0.runs.lock().unwrap().get_mut(RUN).unwrap().status = status;
        self.0.goals.lock().unwrap().get_mut(RUN).unwrap().status = status;
    }

    /// The engine the Run item names (control-api writes it from the request).
    pub fn set_run_engine(&self, engine: harness_protocol::common::Engine) {
        self.0.runs.lock().unwrap().get_mut(RUN).unwrap().engine = Some(engine);
    }

    pub fn agent(&self) -> Option<AgentItem> {
        self.0.agents.lock().unwrap().get("direct").cloned()
    }

    pub fn turn(&self) -> Option<TurnItem> {
        self.0
            .turns
            .lock()
            .unwrap()
            .get(&("direct".into(), 1))
            .cloned()
    }

    /// Every event but `run.created`, in id order, as `(type, payload)`.
    pub fn events(&self) -> Vec<(String, Value)> {
        self.envelopes()
            .into_iter()
            .map(|e| {
                let v = match e.payload {
                    Payload::Inline { value } => value,
                    Payload::ClaimCheck { .. } => Value::Null,
                };
                (e.event_type.as_str().to_string(), v)
            })
            .collect()
    }

    pub fn envelopes(&self) -> Vec<Envelope> {
        self.0
            .events
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.event_type != EventType::RunCreated)
            .cloned()
            .collect()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events().into_iter().map(|(t, _)| t).collect()
    }

    /// Every event write the store accepted, `(eventId, type)`, in write order (EVT1).
    pub fn event_writes(&self) -> Vec<(String, String)> {
        self.0.event_writes.lock().unwrap().clone()
    }

    /// Fails unless the events written so far (those after `skip`) were written in strictly ascending id order: the order a reader
    /// that follows "ids above the last one seen" needs, because an event written with an id below one it has read is never returned.
    pub fn assert_event_writes_ascend(&self, skip: usize, what: &str) {
        let writes = self.event_writes();
        let writes = &writes[skip.min(writes.len())..];
        assert!(writes.len() >= 2, "{what}: only {writes:?} was written");
        for pair in writes.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "{what}: {} ({}) was written before {} ({}), which has the smaller id",
                pair[0].1,
                pair[0].0,
                pair[1].1,
                pair[1].0
            );
        }
    }

    pub fn clear_events(&self) {
        self.0
            .events
            .lock()
            .unwrap()
            .retain(|_, e| e.event_type == EventType::RunCreated);
    }

    pub fn invocations(&self) -> Vec<Value> {
        self.0
            .invocations
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect()
    }

    pub fn stops(&self) -> Vec<(String, String)> {
        self.0.stops.lock().unwrap().clone()
    }

    pub fn message_closes(&self) -> Vec<(String, usize)> {
        self.0.message_closes.lock().unwrap().clone()
    }

    pub fn index_touches(&self) -> Vec<IndexTouch> {
        self.0.index_touches.lock().unwrap().clone()
    }
}

impl Store for Mem {
    async fn get_run(&self, run_id: &str) -> Res<Option<RunItem>> {
        self.0.calls.lock().unwrap().push("get_run".into());
        let mut runs = self.0.runs.lock().unwrap();
        let seen = runs.get(run_id).cloned();
        let mut flip = self.0.flip_run_after_reads.lock().unwrap();
        if let Some((left, to)) = flip.as_mut() {
            if *left <= 1 {
                if let Some(run) = runs.get_mut(run_id) {
                    run.status = *to;
                }
                *flip = None;
            } else {
                *left -= 1;
            }
        }
        Ok(seen)
    }
    async fn get_root_goal(&self, run_id: &str) -> Res<Option<GoalItem>> {
        self.0.calls.lock().unwrap().push("get_root_goal".into());
        Ok(self.0.goals.lock().unwrap().get(run_id).cloned())
    }
    async fn run_created_request_sha256(&self, run_id: &str) -> Res<Option<String>> {
        let events = self.0.events.lock().unwrap();
        Ok(events
            .values()
            .find(|e| e.run_id == run_id && e.event_type == EventType::RunCreated)
            .and_then(|e| match &e.payload {
                Payload::Inline { value } => value
                    .get("requestSha256")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Payload::ClaimCheck { .. } => None,
            }))
    }
    async fn delivery_pr_url(&self, run_id: &str) -> Res<Option<String>> {
        self.failing("delivery_pr_url")?;
        let events = self.0.events.lock().unwrap();
        Ok(events
            .values()
            .rev()
            .find(|e| e.run_id == run_id && e.event_type == EventType::DeliveryPr)
            .and_then(|e| match &e.payload {
                Payload::Inline { value } => {
                    value.get("url").and_then(Value::as_str).map(str::to_string)
                }
                Payload::ClaimCheck { .. } => None,
            }))
    }
    async fn get_agent(&self, _run_id: &str, agent_id: &str) -> Res<Option<AgentItem>> {
        Ok(self.0.agents.lock().unwrap().get(agent_id).cloned())
    }
    async fn put_agent(&self, agent: &AgentItem) -> Res<()> {
        self.failing("put_agent")?;
        // Create-only, as the DynamoDB adapter: an Agent item that exists is left as it is.
        self.0
            .agents
            .lock()
            .unwrap()
            .entry(agent.agent_id.clone())
            .or_insert_with(|| {
                let mut kept = agent.clone();
                if !*self.0.keep_pins_flag.lock().unwrap()
                    && let Some(creds) = kept.creds.as_mut()
                {
                    creds.output_pins_required = None;
                }
                kept
            });
        self.0.agent_writes.lock().unwrap().push(agent.clone());
        Ok(())
    }
    async fn rotate_agent(
        &self,
        next: &AgentItem,
        from_generation: u32,
        from_version: u64,
    ) -> Res<bool> {
        self.failing("rotate_agent")?;
        let mut agents = self.0.agents.lock().unwrap();
        let Some(stored) = agents.get_mut(&next.agent_id) else {
            return Ok(false);
        };
        if stored.session_generation != Some(from_generation) || stored.version != from_version {
            return Ok(false);
        }
        *stored = next.clone();
        self.0.agent_writes.lock().unwrap().push(next.clone());
        Ok(true)
    }
    async fn set_run_slice(&self, run_id: &str, slice: u32) -> Res<()> {
        self.failing("set_run_slice")?;
        if let Some(run) = self.0.runs.lock().unwrap().get_mut(run_id)
            && run.slice.is_none_or(|s| s < slice)
        {
            run.slice = Some(slice);
        }
        Ok(())
    }
    async fn close_messages(&self, run_id: &str) -> Res<()> {
        self.failing("close_messages")?;
        let stops = self.0.stops.lock().unwrap().len();
        self.0
            .message_closes
            .lock()
            .unwrap()
            .push((run_id.to_string(), stops));
        Ok(())
    }
    async fn claim_instances_slot(&self, run_id: &str, max: u32) -> Res<bool> {
        self.failing("claim_instances_slot")?;
        self.0
            .slot_calls
            .lock()
            .unwrap()
            .push(format!("claim {run_id} {max}"));
        // As the DynamoDB condition: the set has room, or already holds this run.
        let mut held = self.0.slots.lock().unwrap();
        if held.contains(run_id) || held.len() < max as usize {
            held.insert(run_id.to_string());
            return Ok(true);
        }
        Ok(false)
    }
    async fn release_instances_slot(&self, run_id: &str) -> Res<()> {
        self.failing("release_instances_slot")?;
        self.0
            .slot_calls
            .lock()
            .unwrap()
            .push(format!("release {run_id}"));
        self.0.slots.lock().unwrap().remove(run_id);
        self.0.ending.lock().unwrap().push("release_slot".into());
        Ok(())
    }
    async fn end_agent(&self, _run_id: &str, agent_id: &str) -> Res<()> {
        self.failing("end_agent")?;
        self.0.ending.lock().unwrap().push("end_agent".into());
        if let Some(a) = self.0.agents.lock().unwrap().get_mut(agent_id) {
            a.status = harness_protocol::store::AgentStatus::Stopped;
            if let Some(c) = a.creds.as_mut() {
                c.revoked = true;
            }
            a.version += 1;
        }
        Ok(())
    }
    async fn get_turn(&self, _run_id: &str, agent_id: &str, n: u32) -> Res<Option<TurnItem>> {
        Ok(self
            .0
            .turns
            .lock()
            .unwrap()
            .get(&(agent_id.to_string(), n))
            .cloned())
    }
    async fn create_turn(&self, turn: &TurnItem) -> Res<bool> {
        self.failing("create_turn")?;
        let mut t = self.0.turns.lock().unwrap();
        let key = (turn.agent_id.clone(), turn.n);
        if t.contains_key(&key) {
            return Ok(false);
        }
        t.insert(key, turn.clone());
        Ok(true)
    }
    async fn replace_turn_token(&self, _run_id: &str, agent_id: &str, n: u32, ct: &str) -> Res<()> {
        self.failing("replace_turn_token")?;
        let mut t = self.0.turns.lock().unwrap();
        let turn = t
            .get_mut(&(agent_id.to_string(), n))
            .ok_or_else(|| RunSessionError::Unavailable("no turn".into()))?;
        turn.task_token_ciphertext = ct.to_string();
        Ok(())
    }
    async fn mark_turn_invoked(&self, _run_id: &str, agent_id: &str, n: u32) -> Res<()> {
        self.failing("mark_turn_invoked")?;
        let mut t = self.0.turns.lock().unwrap();
        let turn = t
            .get_mut(&(agent_id.to_string(), n))
            .ok_or_else(|| RunSessionError::Unavailable("no turn".into()))?;
        turn.invoked_at = Some("2026-10-13T18:00:01Z".into());
        Ok(())
    }
    async fn pin_runtime(&self, run_id: &str, pin: &Pin) -> Res<Pin> {
        let mut runs = self.0.runs.lock().unwrap();
        let run = runs.get_mut(run_id).unwrap();
        if let Some(e) = &run.runtime_endpoint {
            return Ok(Pin {
                version: run.runtime_version.clone().unwrap_or_default(),
                endpoint: e.clone(),
            });
        }
        run.runtime_version = Some(pin.version.clone());
        run.runtime_endpoint = Some(pin.endpoint.clone());
        Ok(pin.clone())
    }
    async fn set_run_phase(&self, run_id: &str, phase: GoalPhase) -> Res<()> {
        self.failing("set_run_phase")?;
        self.0.runs.lock().unwrap().get_mut(run_id).unwrap().phase = phase;
        Ok(())
    }
    async fn set_run_status(&self, run: &RunItem, to: GoalStatus, event: &Envelope) -> Res<bool> {
        self.failing("set_run_status")?;
        let mut runs = self.0.runs.lock().unwrap();
        let stored = runs.get_mut(&run.run_id).unwrap();
        if let Some(other) = self.0.race_status.lock().unwrap().take() {
            stored.status = other;
        }
        let mut events = self.0.events.lock().unwrap();
        if stored.status != run.status || events.contains_key(&event.event_id) {
            return Ok(false);
        }
        stored.status = to;
        stored.version += 1;
        let mut goals = self.0.goals.lock().unwrap();
        let g = goals.get_mut(&run.run_id).unwrap();
        g.status = to;
        g.version += 1;
        events.insert(event.event_id.clone(), event.clone());
        self.0.event_writes.lock().unwrap().push((
            event.event_id.clone(),
            event.event_type.as_str().to_string(),
        ));
        Ok(true)
    }
    async fn set_run_stop(
        &self,
        run: &RunItem,
        to: GoalStatus,
        event: &Envelope,
        reason: &str,
        mode: harness_protocol::goal::ResumeMode,
    ) -> Res<bool> {
        if !self.set_run_status(run, to, event).await? {
            return Ok(false);
        }
        let mut runs = self.0.runs.lock().unwrap();
        let stored = runs.get_mut(&run.run_id).unwrap();
        stored.status_reason = Some(reason.to_string());
        stored.resume_mode = Some(mode);
        Ok(true)
    }
    async fn put_research_policy(&self, item: &harness_protocol::research::PolicyItem) -> Res<()> {
        self.failing("put_research_policy")?;
        let mut rows = self.0.research_rows.lock().unwrap();
        if !rows.iter().any(|r| r.pk == item.pk) {
            rows.push(item.clone());
        }
        Ok(())
    }
    async fn put_event(&self, event: &Envelope) -> Res<()> {
        self.failing("put_event")?;
        self.0
            .events
            .lock()
            .unwrap()
            .insert(event.event_id.clone(), event.clone());
        self.0.event_writes.lock().unwrap().push((
            event.event_id.clone(),
            event.event_type.as_str().to_string(),
        ));
        Ok(())
    }
    /// An upsert of the search item's end fields (`UpdateItem` in the DynamoDB adapter): it makes the item when it is not there.
    async fn touch_run_index(
        &self,
        run: &RunItem,
        outcome: Option<&str>,
        last_activity: &str,
    ) -> Res<()> {
        self.failing("touch_run_index")?;
        self.0.index_touches.lock().unwrap().push((
            run.owner_principal_arn.clone(),
            run.created_at.clone(),
            run.run_id.clone(),
            outcome.map(str::to_string),
            last_activity.to_string(),
        ));
        // The search item as the adapter's UpdateItem leaves it: the key fields where the item is new, `lastActivity` and the outcome
        // when its condition holds (`attribute_not_exists(lastActivity) OR lastActivity < :t OR (lastActivity = :t AND ...)`), and
        // never an attribute of another writer.
        let mut items = self.0.search_items.lock().unwrap();
        let item = items.entry(search_key(run)).or_default();
        let allowed = match item.get("lastActivity") {
            None => true,
            Some(have) if have.as_str() < last_activity => true,
            Some(have) if have == last_activity => match (item.get("outcome"), outcome) {
                (None, _) => true,
                (Some(have), Some(o)) => have == o,
                (Some(_), None) => false,
            },
            Some(_) => false,
        };
        if allowed {
            item.entry("runId".into())
                .or_insert_with(|| run.run_id.clone());
            item.entry("createdAt".into())
                .or_insert_with(|| run.created_at.clone());
            item.insert("lastActivity".into(), last_activity.to_string());
            if let Some(o) = outcome {
                item.insert("outcome".into(), o.to_string());
            }
        }
        Ok(())
    }

    async fn run_name(&self, run: &RunItem) -> Res<Option<String>> {
        self.0.calls.lock().unwrap().push("run_name".into());
        self.failing("run_name")?;
        Ok(self
            .0
            .search_items
            .lock()
            .unwrap()
            .get(&search_key(run))
            .and_then(|item| item.get("runName"))
            .cloned())
    }

    /// The conditional `UpdateItem` of the adapter: `runId` and `createdAt` where the item is new, the three name attributes when the
    /// item holds no `runName`, nothing else.
    async fn put_run_name(
        &self,
        run: &RunItem,
        name: &str,
        source: &str,
        model: &str,
    ) -> Res<bool> {
        self.0.calls.lock().unwrap().push("put_run_name".into());
        self.failing("put_run_name")?;
        self.0
            .write_times
            .lock()
            .unwrap()
            .push(("put_run_name".into(), Instant::now()));
        let mut items = self.0.search_items.lock().unwrap();
        let item = items.entry(search_key(run)).or_default();
        if let Some(rival) = self.0.name_race.lock().unwrap().take() {
            item.entry("runId".into())
                .or_insert_with(|| run.run_id.clone());
            item.entry("createdAt".into())
                .or_insert_with(|| run.created_at.clone());
            item.insert("runName".into(), rival);
            item.insert("runNameSource".into(), "ai".into());
            item.insert("runNameModel".into(), model.to_string());
        }
        let stored = !item.contains_key("runName");
        if stored {
            item.entry("runId".into())
                .or_insert_with(|| run.run_id.clone());
            item.entry("createdAt".into())
                .or_insert_with(|| run.created_at.clone());
            item.insert("runName".into(), name.to_string());
            item.insert("runNameSource".into(), source.to_string());
            item.insert("runNameModel".into(), model.to_string());
        }
        self.0.name_writes.lock().unwrap().push(NameWrite {
            run_id: run.run_id.clone(),
            name: name.to_string(),
            source: source.to_string(),
            model: model.to_string(),
            stored,
        });
        if stored && std::mem::take(&mut *self.0.lose_name_response.lock().unwrap()) {
            return Err(RunSessionError::Unavailable(
                "the answer of the write was lost".into(),
            ));
        }
        Ok(stored)
    }
    async fn tag_state(&self, run: &RunItem) -> Res<Option<TagMarks>> {
        self.0.calls.lock().unwrap().push("tag_state".into());
        self.maybe_stall("tag_state").await;
        self.failing("tag_state")?;
        Ok(self
            .0
            .search_items
            .lock()
            .unwrap()
            .get(&search_key(run))
            .map(|item| {
                let tags = TagItem::read(item);
                TagMarks {
                    at: tags.at,
                    off: tags.off,
                }
            }))
    }

    /// The `Query` of the adapter: the owner's partition of the stored run, `SRCH#` items newest first, at most `limit`, the three tag
    /// lists only. The spec is the one the adapter sends, and it is checked against what DynamoDB refuses.
    async fn tag_vocabulary(&self, run: &RunItem, limit: u32) -> Res<VocabRead> {
        self.0.calls.lock().unwrap().push("tag_vocabulary".into());
        self.maybe_stall("tag_vocabulary").await;
        self.failing("tag_vocabulary")?;
        let mut spec: QuerySpec = dynamo::vocabulary_query(&owner_pk(&run.owner_principal_arn));
        spec.limit = limit.clamp(1, spec.limit);
        if let Some(why) = dynamo::expr_rejects(&spec) {
            panic!("DynamoDB would refuse the vocabulary read: {why}\n{spec:#?}");
        }
        self.0.tag_queries.lock().unwrap().push(TagQuery {
            run_id: run.run_id.clone(),
            limit,
            spec: spec.clone(),
        });
        let partition = owner_pk(&run.owner_principal_arn);
        let items = self.0.search_items.lock().unwrap();
        let window: Vec<VocabItem> = items
            .iter()
            .rev()
            .filter(|((pk, sk), _)| *pk == partition && sk.starts_with("SRCH#"))
            .take(spec.limit as usize)
            .map(|(_, item)| {
                let tags = TagItem::read(item);
                VocabItem {
                    auto: tags.auto,
                    user: tags.user,
                    off: tags.off,
                }
            })
            .collect();
        Ok(VocabRead {
            items: window,
            skipped: 0,
        })
    }

    async fn parent_tags(&self, run: &RunItem) -> Res<Option<TagState>> {
        self.0.calls.lock().unwrap().push("parent_tags".into());
        self.maybe_stall("parent_tags").await;
        self.failing("parent_tags")?;
        let Some(parent_id) = run.continue_from.as_deref() else {
            return Ok(None);
        };
        let Some(parent) = self.0.runs.lock().unwrap().get(parent_id).cloned() else {
            return Ok(None);
        };
        if parent.owner_principal_arn != run.owner_principal_arn {
            return Ok(None);
        }
        Ok(self
            .0
            .search_items
            .lock()
            .unwrap()
            .get(&(
                owner_pk(&run.owner_principal_arn),
                search_sk(&parent.created_at, &parent.run_id),
            ))
            .map(|item| {
                let tags = TagItem::read(item);
                TagState {
                    auto: tags.auto,
                    user: tags.user,
                    off: tags.off,
                }
            }))
    }

    async fn put_run_tags_inherit(
        &self,
        run: &RunItem,
        parent_run_id: &str,
        user: &[String],
        off: &[String],
    ) -> Res<bool> {
        self.0
            .calls
            .lock()
            .unwrap()
            .push("put_run_tags_inherit".into());
        self.failing("put_run_tags_inherit")?;
        let Some(spec) =
            dynamo::inherit_update(&run.run_id, &run.created_at, parent_run_id, user, off)
        else {
            return Ok(false);
        };
        self.apply_tag_write("inherit", run, &spec)
    }

    async fn put_run_tags_auto(
        &self,
        run: &RunItem,
        tags: &[String],
        at: &str,
        answered: Option<Answered<'_>>,
    ) -> Res<bool> {
        self.0
            .calls
            .lock()
            .unwrap()
            .push("put_run_tags_auto".into());
        self.failing("put_run_tags_auto")?;
        let spec = dynamo::auto_update(&run.run_id, &run.created_at, tags, at, answered);
        self.apply_tag_write("auto", run, &spec)
    }

    async fn abort_run(
        &self,
        run: &RunItem,
        goal: Option<&GoalItem>,
        events: &[Envelope],
    ) -> Res<bool> {
        self.failing("abort_run")?;
        {
            let mut races = self.0.abort_races.lock().unwrap();
            if *races > 0 {
                *races -= 1;
                return Ok(false);
            }
        }
        let mut runs = self.0.runs.lock().unwrap();
        let stored = runs.get_mut(&run.run_id).unwrap();
        if stored.version != run.version - 1 {
            return Ok(false);
        }
        *stored = run.clone();
        if let Some(g) = goal {
            self.0
                .goals
                .lock()
                .unwrap()
                .insert(g.run_id.clone(), g.clone());
        }
        for e in events {
            self.0
                .events
                .lock()
                .unwrap()
                .insert(e.event_id.clone(), e.clone());
            self.0
                .event_writes
                .lock()
                .unwrap()
                .push((e.event_id.clone(), e.event_type.as_str().to_string()));
        }
        Ok(true)
    }

    async fn transcript_supported(&self, _run_id: &str) -> Res<bool> {
        self.failing("transcript_supported")?;
        Ok(*self.0.transcript_supported.lock().unwrap())
    }

    async fn user_answers(&self, _run_id: &str) -> Res<Vec<String>> {
        self.failing("user_answers")?;
        Ok(self.0.answers.lock().unwrap().clone())
    }

    async fn put_search_body(&self, w: &run_session::ports::SearchWrite<'_>) -> Res<()> {
        self.failing("put_search_body")?;
        self.0.search_writes.lock().unwrap().push(SearchRecord {
            pk: w.pk.into(),
            sk: w.sk.into(),
            run_id: w.run_id.into(),
            created_at: w.created_at.into(),
            body: w.body.clone(),
            indexed_at: w.indexed_at.into(),
        });
        Ok(())
    }
}

impl Tokens for Mem {
    async fn encrypt(&self, token: &str, ctx: &TokenContext) -> Res<String> {
        self.failing("encrypt")?;
        self.0.encrypted.lock().unwrap().push(token.to_string());
        Ok(format!(
            "ct({token};{};{};{})",
            ctx.run_id, ctx.agent_id, ctx.turn
        ))
    }
    async fn decrypt(&self, ciphertext: &str, ctx: &TokenContext) -> Res<String> {
        self.failing("decrypt")?;
        let suffix = format!(";{};{};{})", ctx.run_id, ctx.agent_id, ctx.turn);
        ciphertext
            .strip_prefix("ct(")
            .and_then(|c| c.strip_suffix(suffix.as_str()))
            .map(str::to_string)
            .ok_or_else(|| {
                RunSessionError::Invalid(format!(
                    "KMS cannot decrypt {ciphertext:?} under another context"
                ))
            })
    }
}

impl PackageTokens for Mem {
    async fn token(&self, _duration_s: u32) -> Result<Option<String>, String> {
        self.0
            .package_token
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Ok(None))
    }
}

impl Minter for Mem {
    async fn mint(&self, plan: &MintPlan) -> Result<ProcessCredentials, MintError> {
        self.0.minted.lock().unwrap().push(plan.clone());
        Ok(ProcessCredentials {
            version: 1,
            access_key_id: ["ASIA-test-", "0123456789"].concat(),
            secret_access_key: "secret".into(),
            session_token: "session".into(),
            expiration: "2026-10-13T19:10:00Z".into(),
        })
    }
}

impl Sessions for Mem {
    async fn runtime_arn(&self) -> Res<Option<String>> {
        *self.0.runtime_lookups.lock().unwrap() += 1;
        match self.0.runtime_lookup.lock().unwrap().clone() {
            None => Ok(None),
            Some(Ok(arn)) => Ok(Some(arn)),
            Some(Err(e)) => Err(e),
        }
    }
    async fn instances_runtime_arn(&self) -> Res<Option<String>> {
        match self.0.instances_lookup.lock().unwrap().clone() {
            None => Ok(None),
            Some(Ok(arn)) => Ok(Some(arn)),
            Some(Err(e)) => Err(e),
        }
    }
    async fn capacity_provider_id(&self) -> Res<Option<String>> {
        *self.0.provider_lookups.lock().unwrap() += 1;
        match self.0.provider_lookup.lock().unwrap().clone() {
            None => Ok(None),
            Some(Ok(id)) => Ok(Some(id)),
            Some(Err(e)) => Err(e),
        }
    }
    async fn endpoint(&self, arn: &str, name: &str) -> Res<Option<EndpointState>> {
        let instances =
            matches!(self.0.instances_lookup.lock().unwrap().as_ref(), Some(Ok(a)) if a == arn);
        if instances {
            return Ok(self
                .0
                .instances_endpoints
                .lock()
                .unwrap()
                .get(name)
                .cloned());
        }
        Ok(self.0.endpoints.lock().unwrap().get(name).cloned())
    }
    async fn invoke(&self, arn: &str, qualifier: &str, session: &str, body: &[u8]) -> Res<Vec<u8>> {
        // The machine can be created and the run discarded while the call is in flight, and the call can still end in an error.
        if let Some(to) = self.0.flip_run_on_invoke.lock().unwrap().take() {
            for run in self.0.runs.lock().unwrap().values_mut() {
                run.status = to;
            }
        }
        self.failing("invoke")?;
        self.0.invocations.lock().unwrap().push(body.to_vec());
        self.0.invoke_targets.lock().unwrap().push((
            arn.to_string(),
            qualifier.to_string(),
            session.to_string(),
        ));
        if let Some(r) = self.0.invoke_reply.lock().unwrap().clone() {
            return Ok(r);
        }
        // As agentd: the acknowledgement names the turn the invocation asked for.
        let turn = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("turn").and_then(Value::as_u64))
            .unwrap_or(1);
        Ok(serde_json::to_vec(
            &json!({"runId": RUN, "agentId": "direct", "turn": turn, "accepted": true}),
        )
        .unwrap())
    }
    async fn stop(&self, _arn: &str, qualifier: &str, session: &str) -> Res<bool> {
        self.failing("stop")?;
        self.0
            .stops
            .lock()
            .unwrap()
            .push((qualifier.to_string(), session.to_string()));
        self.0.ending.lock().unwrap().push("stop".into());
        Ok(!*self.0.stop_gone.lock().unwrap())
    }
    async fn delete_provider_session(&self, provider_id: &str, session_id: &str) -> Res<bool> {
        self.failing("delete_provider_session")?;
        self.0
            .provider_deletes
            .lock()
            .unwrap()
            .push((provider_id.to_string(), session_id.to_string()));
        self.0
            .ending
            .lock()
            .unwrap()
            .push("delete_provider_session".into());
        Ok(!*self.0.provider_gone.lock().unwrap())
    }
}

impl Blobs for Mem {
    async fn get(&self, key: &str) -> Res<Option<Vec<u8>>> {
        if self.0.deny_read.lock().unwrap().contains(key) {
            return Err(RunSessionError::Unavailable(
                "AccessDenied: the role may not read that key".into(),
            ));
        }
        Ok(self.blob(key))
    }
    async fn exists(&self, key: &str) -> Res<bool> {
        Ok(self.0.blobs.lock().unwrap().contains_key(key))
    }
    async fn put_if_absent(&self, key: &str, body: &[u8]) -> Res<bool> {
        self.failing("put_if_absent")?;
        let mut b = self.0.blobs.lock().unwrap();
        {
            let mut race = self.0.blob_race.lock().unwrap();
            if race.as_ref().is_some_and(|(k, _)| k == key) {
                let (k, v) = race.take().unwrap();
                b.insert(k, v);
            }
        }
        if b.contains_key(key) {
            return Ok(false);
        }
        b.insert(key.to_string(), body.to_vec());
        Ok(true)
    }

    async fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Res<run_session::ports::ObjectPage> {
        self.0.lists.lock().unwrap().push((
            prefix.into(),
            start_after.map(str::to_string),
            max_keys,
        ));
        self.failing("list")?;
        // What S3 refuses: max-keys outside 1 to 1,000, a prefix or start-after over 1,024 bytes.
        if !(1..=1000).contains(&max_keys)
            || prefix.len() > 1024
            || start_after.is_some_and(|a| a.len() > 1024)
        {
            return Err(RunSessionError::Unavailable("InvalidArgument".into()));
        }
        let g = self.0.blobs.lock().unwrap();
        let sizes = self.0.listed_sizes.lock().unwrap();
        let mut objects = Vec::new();
        let mut truncated = false;
        for (key, body) in g.iter() {
            if !key.starts_with(prefix) || start_after.is_some_and(|a| key.as_str() <= a) {
                continue;
            }
            if objects.len() == max_keys {
                truncated = true;
                break;
            }
            objects.push(run_session::ports::ObjectInfo {
                key: key.clone(),
                size: sizes.get(key).copied().unwrap_or(body.len() as u64),
            });
        }
        Ok(run_session::ports::ObjectPage { objects, truncated })
    }

    async fn get_bounded(&self, key: &str, max_bytes: usize) -> Res<Option<Vec<u8>>> {
        self.0
            .bounded_gets
            .lock()
            .unwrap()
            .push((key.into(), max_bytes));
        self.failing("get_bounded")?;
        Ok(self.blob(key).filter(|b| b.len() <= max_bytes))
    }

    async fn get_version_bounded(
        &self,
        key: &str,
        version_id: Option<&str>,
        max_bytes: usize,
    ) -> Res<Option<Vec<u8>>> {
        self.0
            .version_reads
            .lock()
            .unwrap()
            .push((key.to_string(), version_id.map(str::to_string)));
        let body = match version_id {
            Some(v) => self
                .0
                .versions
                .lock()
                .unwrap()
                .get(&(key.to_string(), v.to_string()))
                .cloned(),
            None => self.blob(key),
        };
        Ok(body.filter(|b| b.len() <= max_bytes))
    }

    async fn head_version(
        &self,
        key: &str,
        version_id: Option<&str>,
    ) -> Res<Option<run_session::ports::ObjectFacts>> {
        self.0
            .version_reads
            .lock()
            .unwrap()
            .push((key.to_string(), version_id.map(str::to_string)));
        let body = match version_id {
            Some(v) => self
                .0
                .versions
                .lock()
                .unwrap()
                .get(&(key.to_string(), v.to_string()))
                .cloned(),
            None => self.blob(key),
        };
        // S3 stores the SHA-256 of what it received with every object put through a checksum-sending writer.
        Ok(body.map(|b| run_session::ports::ObjectFacts {
            sha256_base64: Some(
                harness_protocol::output_pins::OutputPin::for_body(&b, None).sha256_base64(),
            ),
        }))
    }
}

/// An agentd `summary.json` for `outcome`, with the final bundle uploaded when `bundle` is set.
pub fn summary(
    outcome: harness_protocol::direct::DirectOutcome,
    message: Option<&str>,
    bundle: bool,
) -> DirectSummary {
    let git = if bundle {
        json!({
            "baseSha": BASE, "headSha": HEAD, "committedByHarness": true, "filesChanged": 3,
            "bundleUri": format!("s3://{BUCKET}/{}", s3::bundle_key(RUN)), "bundleBytes": 1234,
        })
    } else {
        json!({"baseSha": BASE, "committedByHarness": false, "filesChanged": 0})
    };
    let v = json!({
        "schemaVersion": 1, "runId": RUN, "agentId": "direct", "turn": 1,
        "outcome": outcome.as_str(), "label": outcome.label(),
        "detail": if outcome.is_reported_done() { Value::Null } else { json!("no output for 900 s") },
        "stopReason": "end_turn", "turnOutcome": "completed",
        "clineMessage": message,
        "error": null,
        "model": {"id": MODEL, "region": "us-east-1", "pinned": MODEL},
        "engine": {"name": "cline", "version": "3.0.71", "binarySha256": "ab".repeat(32)},
        "rules": [{"name": "00-my-rules.md", "sha256": hex(b"Write the test first.\n"), "bytes": 22}],
        "rulesPreflight": {"ran": true, "ms": 812},
        "request": {"sha256": "cd".repeat(32), "promptBytes": 123},
        "nudge": {"requested": false, "honored": false},
        "usage": {"inputTokens": 10, "outputTokens": 20, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0},
        "toolCalls": 41,
        "timing": {"startedAt": "2026-10-13T18:00:00Z", "endedAt": "2026-10-13T19:02:11Z", "wallS": 3731, "promptS": 3690, "maxSilentGapS": 84},
        "git": git,
        "snapshots": {"count": 5, "lastAt": "2026-10-13T18:50:00Z"},
        "packages": {"enabled": true, "requests": 57, "bytes": 123456789, "byPrefix": {"pypi": 40}, "statuses": {"200": 55}},
        "faultHint": null,
        "versions": {"agentd": "0.1.0", "protocol": "1.30.0"},
    });
    serde_json::from_value(v).unwrap()
}

/// The `TurnCompleteRequest` worker-api passes to `SendTaskSuccess`, as the machine stores it in `$.session.turn`.
pub fn turn_output(bundle: bool) -> Value {
    let mut v = json!({
        "summary": "Added pct(a, b).", "runId": RUN, "agentId": "direct", "turn": 1,
        "finishReason": "end_turn", "outcome": "completed",
    });
    if bundle {
        v["bundleUri"] = json!(format!("s3://{BUCKET}/{}", s3::bundle_key(RUN)));
        v["lastCommitSha"] = json!(HEAD);
    }
    v
}

pub fn staged() -> Value {
    json!({"bundleKey": s3::repo_bundle_key(RUN), "bundleUri": format!("s3://{BUCKET}/{}", s3::repo_bundle_key(RUN)),
        "baseSha": BASE, "baseRef": "main", "sizeBytes": 10, "sha256": "ef".repeat(32), "treeKey": "x", "fileCount": 2, "totalBytes": 20,
        "containerCheck": {}})
}

pub fn repo_target() -> RunTarget {
    serde_json::from_value(json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"})).unwrap()
}

/// The key of the run's search item: the owner's partition and the item `control-api` writes at the start.
pub fn search_key(run: &RunItem) -> (String, String) {
    (
        owner_pk(&run.owner_principal_arn),
        search_sk(&run.created_at, &run.run_id),
    )
}

/// A budget that keeps every test of a normal answer far from its limits and makes a retry quick.
pub fn quick_budget() -> NameBudget {
    NameBudget {
        op: Duration::from_secs(20),
        attempt: Duration::from_secs(10),
        backoff_min: Duration::from_millis(1),
        backoff_max: Duration::from_millis(3),
    }
}

/// One answer of the scripted utility client.
#[derive(Clone, Debug)]
pub enum Scripted {
    /// The recorded body of a successful Converse answer (the fixtures of `ham-core`), read by the real `parse_reply`.
    Body(String),
    /// A normal answer: `stop_sequence` and this text (the stop sequence cut the closing tag off, as the live service does).
    Text(String),
    /// A failed HTTP answer: the status and the `x-amzn-errortype` header, mapped by the real `UtilityFault::from_http`.
    Http(u16, Option<&'static str>),
    /// The call never returns; only the attempt's timeout ends it.
    Hang,
    /// A normal answer with another stop reason than the stop sequence (`refusal`, `max_tokens`, ...) and this text.
    Stop(String, String),
}

impl Scripted {
    /// `<title>name` as the live model answers it.
    pub fn title(name: &str) -> Scripted {
        Scripted::Text(format!("<title>{name}"))
    }

    /// A recorded reply of `crates/ham-core/tests/fixtures/runname/replies/<file>.json`.
    pub fn recorded(file: &str) -> Scripted {
        let path = format!(
            "{}/../ham-core/tests/fixtures/runname/replies/{file}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        Scripted::Body(std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
    }

    /// `<tags>["a", "b"]` as the live model answers it when it spaces its list (the stop sequence cut the closing tag off).
    pub fn tags(list: &[&str]) -> Scripted {
        Scripted::Text(format!("<tags>{}", json_list(list, ", ")))
    }

    /// The same list written compact, `["a","b"]`: one blank-separated word of 80 to 110 characters for a full list.
    pub fn tags_compact(list: &[&str]) -> Scripted {
        Scripted::Text(format!("<tags>{}", json_list(list, ",")))
    }

    /// An answer that stopped for `reason` (not the stop sequence) with this text.
    pub fn stop(reason: &str, text: &str) -> Scripted {
        Scripted::Stop(reason.to_string(), text.to_string())
    }

    pub fn throttle() -> Scripted {
        Scripted::Http(429, Some("ThrottlingException"))
    }

    pub fn unavailable() -> Scripted {
        Scripted::Http(503, Some("ServiceUnavailableException"))
    }

    pub fn denied() -> Scripted {
        Scripted::Http(403, Some("AccessDeniedException"))
    }

    pub fn validation() -> Scripted {
        Scripted::Http(
            400,
            Some("ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
        )
    }
}

fn json_list(list: &[&str], separator: &str) -> String {
    let items: Vec<String> = list.iter().map(|t| format!("\"{t}\"")).collect();
    format!("[{}]", items.join(separator))
}

/// A request the scripted client received.
#[derive(Clone, Debug)]
pub struct SentRequest {
    pub route: String,
    pub region: String,
    pub body: Value,
    pub timeout: Duration,
}

/// The utility model with the faults of the service: it replays a queue of answers and refuses a request the live service refuses
/// (`ham_core::utility::bedrock_rejects_with`, the one function every fake of Converse uses). A call with no answer left fails the
/// test, so a call that should not happen is found. Its calls go into the shared log of the store fake.
#[derive(Debug)]
pub struct ScriptedUtilityClient {
    answers: Mutex<VecDeque<Scripted>>,
    /// The answers to the requests of the tag branch (their stop sequence is `</tags>`).
    tag_answers: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<SentRequest>>,
    rejections: Mutex<Vec<String>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl ScriptedUtilityClient {
    pub fn new(answers: Vec<Scripted>, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            tag_answers: Mutex::default(),
            requests: Mutex::default(),
            rejections: Mutex::default(),
            log,
        }
    }

    /// The queue of answers to tag requests.
    pub fn with_tag_answers(self, answers: Vec<Scripted>) -> Self {
        *self.tag_answers.lock().unwrap() = answers.into();
        self
    }

    pub fn requests(&self) -> Vec<SentRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// The requests of the tag branch.
    pub fn tag_requests(&self) -> Vec<SentRequest> {
        self.requests()
            .into_iter()
            .filter(|r| is_tag_request(&r.body))
            .collect()
    }

    /// The requests of the name branch.
    pub fn name_requests(&self) -> Vec<SentRequest> {
        self.requests()
            .into_iter()
            .filter(|r| !is_tag_request(&r.body))
            .collect()
    }

    pub fn tag_answers_left(&self) -> usize {
        self.tag_answers.lock().unwrap().len()
    }

    /// What the fake service refused, in the service's words.
    pub fn rejections(&self) -> Vec<String> {
        self.rejections.lock().unwrap().clone()
    }

    pub fn answers_left(&self) -> usize {
        self.answers.lock().unwrap().len()
    }
}

impl UtilityClient for ScriptedUtilityClient {
    fn converse<'a>(&'a self, call: &'a UtilityCall<'a>) -> UtilityFuture<'a> {
        Box::pin(async move {
            let tags = is_tag_request(call.body);
            self.log
                .lock()
                .unwrap()
                .push(if tags { "converse_tags" } else { "converse" }.into());
            // Two operations of one test interleave at the calls to the model, as they do on the network.
            tokio::task::yield_now().await;
            self.requests.lock().unwrap().push(SentRequest {
                route: call.route.to_string(),
                region: call.region.to_string(),
                body: call.body.clone(),
                timeout: call.timeout,
            });
            let mut request = call.body.clone();
            request["modelId"] = json!(call.route);
            if let Some(reason) = bedrock_rejects_with(&request, &KNOWN_ROUTE_IDS) {
                self.rejections.lock().unwrap().push(reason);
                return Err(UtilityFault::Config);
            }
            let answer = if tags {
                self.tag_answers.lock().unwrap().pop_front()
            } else {
                self.answers.lock().unwrap().pop_front()
            };
            match answer {
                Some(Scripted::Body(text)) => parse_reply(text.as_bytes()),
                Some(Scripted::Text(text)) => Ok(UtilityReply {
                    stop_reason: "stop_sequence".into(),
                    text,
                    input_tokens: 454,
                    output_tokens: 22,
                    latency_ms: Some(743),
                    reasoning_blocks: 0,
                }),
                Some(Scripted::Stop(reason, text)) => Ok(UtilityReply {
                    stop_reason: reason,
                    text,
                    input_tokens: 454,
                    output_tokens: 22,
                    latency_ms: Some(743),
                    reasoning_blocks: 0,
                }),
                Some(Scripted::Http(status, error_type)) => {
                    Err(UtilityFault::from_http(status, error_type))
                }
                Some(Scripted::Hang) => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Err(UtilityFault::Timeout)
                }
                None => panic!("the scripted utility client has no answer left for a call"),
            }
        })
    }
}

/// A budget for the tag branch that keeps a normal answer far from every limit and makes a retry quick.
pub fn quick_tag_budget() -> TagBudget {
    TagBudget {
        vocab: Duration::from_secs(5),
        attempt: Duration::from_secs(10),
        attempts: 2,
        backoff_min: Duration::from_millis(1),
        backoff_max: Duration::from_millis(3),
        total: Duration::from_secs(20),
    }
}

/// The tag request and the name request differ in their stop sequence.
fn is_tag_request(body: &Value) -> bool {
    body["inferenceConfig"]["stopSequences"] == json!(["</tags>"])
}

// ---- the tag attributes of a search item ----------------------------------------------------------------------------------------------

/// The tag attributes of a search item, as the tag writers and a person's edit leave them. The fake stores the item as text per
/// attribute: a list as its JSON, a number in decimal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagItem {
    pub auto: Vec<String>,
    pub user: Vec<String>,
    pub off: Vec<String>,
    pub at: Option<String>,
    pub model: Option<String>,
    pub ver: Option<String>,
    pub rev: Option<u32>,
    pub inh: Option<String>,
}

fn decode_list(text: Option<&String>) -> Vec<String> {
    text.and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default()
}

impl TagItem {
    pub fn read(attrs: &BTreeMap<String, String>) -> TagItem {
        TagItem {
            auto: decode_list(attrs.get("runTags")),
            user: decode_list(attrs.get("runTagsUser")),
            off: decode_list(attrs.get("runTagsOff")),
            at: attrs.get("runTagsAt").cloned(),
            model: attrs.get("runTagsModel").cloned(),
            ver: attrs.get("runTagsVer").cloned(),
            rev: attrs.get("runTagsRev").and_then(|r| r.parse().ok()),
            inh: attrs.get("runTagsInh").cloned(),
        }
    }

    /// Sets the attributes this value holds and removes the ones it does not (an empty list is no attribute).
    pub fn write_into(&self, attrs: &mut BTreeMap<String, String>) {
        let mut put = |name: &str, value: Option<String>| match value {
            Some(v) => {
                attrs.insert(name.to_string(), v);
            }
            None => {
                attrs.remove(name);
            }
        };
        let list = |l: &Vec<String>| (!l.is_empty()).then(|| serde_json::to_string(l).unwrap());
        put("runTags", list(&self.auto));
        put("runTagsUser", list(&self.user));
        put("runTagsOff", list(&self.off));
        put("runTagsAt", self.at.clone());
        put("runTagsModel", self.model.clone());
        put("runTagsVer", self.ver.clone());
        put("runTagsRev", self.rev.map(|r| r.to_string()));
        put("runTagsInh", self.inh.clone());
    }
}

/// One tag writer call as the fake applied it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagWrite {
    /// `inherit` or `auto`.
    pub kind: &'static str,
    pub run_id: String,
    /// The condition held and the item changed.
    pub stored: bool,
    /// The update expression the adapter would send.
    pub expression: String,
}

/// One vocabulary read as the fake received it.
#[derive(Clone, Debug)]
pub struct TagQuery {
    pub run_id: String,
    pub limit: u32,
    pub spec: QuerySpec,
}

// ---- the expressions of the specs, applied -----------------------------------------------------------------------------------------------
//
// Not a DynamoDB: the few shapes the three tag writers use, read from the spec itself, so that a wrong condition or a wrong update in
// `ham_core::runtag::dynamo` shows in the state the tests read. `dynamo::expr_rejects` has already refused what DynamoDB refuses.

fn encode(v: &Av) -> Result<String, String> {
    match v {
        Av::S(s) => Ok(s.clone()),
        Av::N(n) => Ok(n.to_string()),
        Av::L(l) => Ok(serde_json::to_string(l).unwrap()),
        Av::Ss(_) => Err("a set value is not written by the tag writers".into()),
    }
}

fn name_of(spec: &UpdateSpec, token: &str) -> String {
    spec.names
        .get(token)
        .cloned()
        .unwrap_or_else(|| token.to_string())
}

fn value_of(spec: &UpdateSpec, token: &str) -> Result<String, String> {
    spec.values
        .get(token)
        .ok_or_else(|| format!("{token} has no value"))
        .and_then(encode)
}

/// Splits at the commas that are outside parentheses.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(text[start..].trim());
    out
}

fn condition_holds(
    item: &BTreeMap<String, String>,
    spec: &UpdateSpec,
    condition: &str,
) -> Result<bool, String> {
    let atom = |a: &str| -> Result<bool, String> {
        let a = a.trim();
        if let Some(inner) = a
            .strip_prefix("attribute_not_exists(")
            .and_then(|r| r.strip_suffix(')'))
        {
            return Ok(!item.contains_key(&name_of(spec, inner.trim())));
        }
        if let Some((left, right)) = a.split_once(" = ") {
            let have = item.get(&name_of(spec, left.trim()));
            return Ok(have == Some(&value_of(spec, right.trim())?));
        }
        Err(format!("the fake does not know the condition {a:?}"))
    };
    for disjunct in condition.split(" OR ") {
        let mut all = true;
        for a in disjunct.split(" AND ") {
            all &= atom(a)?;
        }
        if all {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Applies the spec to the item when its condition holds: `SET a = :v`, `a = if_not_exists(a, :v)`, `a = if_not_exists(a, :v) + :w` and
/// `REMOVE a, b`, the right sides read from the item as it was before the update. `false` when the condition failed.
fn apply_spec(item: &mut BTreeMap<String, String>, spec: &UpdateSpec) -> Result<bool, String> {
    if let Some(condition) = &spec.condition
        && !condition_holds(item, spec, condition)?
    {
        return Ok(false);
    }
    let (sets, removes) = match spec.expression.split_once(" REMOVE ") {
        Some((s, r)) => (s, Some(r)),
        None => (spec.expression.as_str(), None),
    };
    let sets = sets
        .strip_prefix("SET ")
        .ok_or_else(|| format!("the fake does not know the update {:?}", spec.expression))?;
    let mut changes: Vec<(String, String)> = Vec::new();
    for assignment in split_top_level(sets) {
        let (left, right) = assignment
            .split_once(" = ")
            .ok_or_else(|| format!("not an assignment: {assignment:?}"))?;
        let target = name_of(spec, left.trim());
        let right = right.trim();
        let value = if let Some(rest) = right.strip_prefix("if_not_exists(") {
            let (args, tail) = rest
                .split_once(')')
                .ok_or_else(|| format!("unclosed if_not_exists: {right:?}"))?;
            let (name, default) = args
                .split_once(',')
                .ok_or_else(|| format!("if_not_exists takes two arguments: {right:?}"))?;
            let current = match item.get(&name_of(spec, name.trim())) {
                Some(v) => v.clone(),
                None => value_of(spec, default.trim())?,
            };
            match tail.trim().strip_prefix("+ ") {
                None if tail.trim().is_empty() => current,
                Some(addend) => {
                    let a: i64 = current
                        .parse()
                        .map_err(|_| format!("{current:?} is no number"))?;
                    let b: i64 = value_of(spec, addend.trim())?
                        .parse()
                        .map_err(|_| "the addend is no number".to_string())?;
                    (a + b).to_string()
                }
                None => return Err(format!("the fake does not know {right:?}")),
            }
        } else {
            value_of(spec, right)?
        };
        changes.push((target, value));
    }
    for (target, value) in changes {
        item.insert(target, value);
    }
    if let Some(removes) = removes {
        for token in split_top_level(removes) {
            item.remove(&name_of(spec, token));
        }
    }
    Ok(true)
}
