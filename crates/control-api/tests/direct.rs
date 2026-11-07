//! `POST /v1/direct/runs`: the prompt unchanged, one model from
//! `models.json`, the user's rules and the optional nudge go to a stored request and one execution. Every refusal
//! happens before a claim, a run or a blob exists.

#![allow(clippy::unwrap_used)]

use control_api::direct_models::builtin;
use control_api::handler::{
    Blobs, DeployedAs, Deps, Execution, OBJECTIVE_ITEM_MAX_BYTES, ObjectPrefix, Store, StoreError,
    V1Request, handle, handle_logged, head_bytes,
};
use harness_protocol::api::v1::{DownloadUrlResponse, StartDirectRunRequest, StartRunResponse};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{
    ArtifactItem, ControlItem, GoalItem, IdempotencyItem, Keyed, RunItem,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const ME: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const BUCKET: &str = "hamh-direct-harness-700000000011";
const MODEL: &str = "global.anthropic.claude-sonnet-5-5";

fn hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

struct Fake {
    idem: Mutex<BTreeMap<String, IdempotencyItem>>,
    runs: Mutex<BTreeMap<String, RunItem>>,
    goals: Mutex<BTreeMap<String, GoalItem>>,
    events: Mutex<Vec<Envelope>>,
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    started: Mutex<BTreeMap<String, Value>>,
    fail_starts: Mutex<u32>,
    fail_reads: Mutex<bool>,
}

impl Fake {
    fn direct() -> Fake {
        Fake {
            idem: Mutex::default(),
            runs: Mutex::default(),
            goals: Mutex::default(),
            events: Mutex::default(),
            blobs: Mutex::default(),
            started: Mutex::default(),
            fail_starts: Mutex::new(0),
            fail_reads: Mutex::new(false),
        }
    }
    fn untouched(&self) -> bool {
        self.idem.lock().unwrap().is_empty()
            && self.runs.lock().unwrap().is_empty()
            && self.events.lock().unwrap().is_empty()
            && self.blobs.lock().unwrap().is_empty()
            && self.started.lock().unwrap().is_empty()
    }
}

impl Store for &Fake {
    async fn claim_idempotency(&self, c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        let mut g = self.idem.lock().unwrap();
        Ok(g.entry(c.key().pk).or_insert_with(|| c.clone()).clone())
    }
    async fn create_run(
        &self,
        run: &RunItem,
        goal: &GoalItem,
        created: &Envelope,
    ) -> Result<(), StoreError> {
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
        Ok(self.goals.lock().unwrap().get(run_id).cloned())
    }
    async fn list_runs(
        &self,
        _owner: &str,
        _next: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        Ok((Vec::new(), None))
    }
    async fn events(
        &self,
        _run_id: &str,
        _shard: u8,
        _after: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        Ok(Vec::new())
    }
    async fn list_artifacts(&self, _run_id: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        Ok(Vec::new())
    }
    async fn get_artifact(&self, _r: &str, _a: &str) -> Result<Option<ArtifactItem>, StoreError> {
        Ok(None)
    }
    async fn put_control(&self, _c: &ControlItem) -> Result<(), StoreError> {
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
    async fn presign_get(&self, _k: &str, _t: Duration) -> Result<DownloadUrlResponse, StoreError> {
        Err(StoreError::Unavailable("not used".into()))
    }
    async fn read_prefix(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<Option<ObjectPrefix>, StoreError> {
        if *self.fail_reads.lock().unwrap() {
            return Err(StoreError::Unavailable("the store is down".into()));
        }
        Ok(self
            .blobs
            .lock()
            .unwrap()
            .get(key)
            .map(|bytes| ObjectPrefix {
                bytes: bytes[..bytes
                    .len()
                    .min(usize::try_from(max_bytes).unwrap_or(usize::MAX))]
                    .to_vec(),
                size: bytes.len() as u64,
                etag: None,
                version_id: None,
                last_modified: None,
            }))
    }
}

impl Execution for &Fake {
    async fn start(&self, name: &str, input: &Value) -> Result<(), StoreError> {
        {
            let mut g = self.fail_starts.lock().unwrap();
            if *g > 0 {
                *g -= 1;
                return Err(StoreError::Unavailable("ThrottlingException".into()));
            }
        }
        self.started
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_insert_with(|| input.clone());
        Ok(())
    }
    async fn deployment(&self) -> Result<DeployedAs, StoreError> {
        Ok(DeployedAs {
            digest: Some("d".repeat(64)),
            source_tree: Some("t".repeat(40)),
        })
    }
}

fn deps(f: &Fake) -> Deps<&Fake, &Fake, &Fake> {
    Deps {
        store: f,
        blobs: f,
        execution: f,
        account: "700000000011".into(),
        operators: vec!["arn:aws:sts::700000000011:assumed-role/Admin/*".into()],
        repo_names: Default::default(),
    }
}

fn req(method: &str, path: &str, caller: Option<&str>, key: Option<&str>, body: &str) -> V1Request {
    V1Request {
        method: method.into(),
        path: path.into(),
        caller_arn: caller.map(str::to_string),
        source_ip: None,
        headers: key
            .map(|k| BTreeMap::from([("idempotency-key".to_string(), k.to_string())]))
            .unwrap_or_default(),
        query: BTreeMap::new(),
        body: body.into(),
    }
}

fn rule(name: &str, content: &str) -> Value {
    json!({"path": name, "sha256": hex(content.as_bytes()), "content": content})
}

fn body() -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "repo", "repo": "cc:hamh-e2e-python-calc"},
        "model": MODEL,
        "rules": [rule("00-my-rules.md", "Write the test first.\n")]
    })
}

async fn post(f: &Fake, key: &str, b: &Value) -> (u16, Value) {
    let out = handle(
        req(
            "POST",
            "/v1/direct/runs",
            Some(ME),
            Some(key),
            &b.to_string(),
        ),
        &deps(f),
    )
    .await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

fn code(v: &Value) -> &str {
    v["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {v}"))
}

fn message(v: &Value) -> &str {
    v["message"].as_str().unwrap()
}

#[tokio::test]
async fn a_valid_request_creates_the_run_stores_the_request_and_starts_one_execution() {
    let f = Fake::direct();
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    let resp: StartRunResponse = serde_json::from_value(v).unwrap();
    assert_eq!(resp.status, GoalStatus::Active);
    assert!(harness_protocol::common::is_ulid(&resp.run_id));
    let id = resp.run_id;

    let run = f.runs.lock().unwrap()[&id].clone();
    assert_eq!(run.owner_principal_arn, ME);
    assert_eq!(
        run.prompt_sha256,
        hex(b"Add percent_change(a, b) with tests")
    );
    assert_eq!(run.status, GoalStatus::Active);
    assert_eq!(run.phase, GoalPhase::Planning);
    assert_eq!(
        run.deployment_digest.as_deref(),
        Some("d".repeat(64).as_str())
    );
    let goal = f.goals.lock().unwrap()[&id].clone();
    assert_eq!(goal.objective, "Add percent_change(a, b) with tests");

    // The request is stored as received (re-serialized), with the rules and the model.
    let blobs = f.blobs.lock().unwrap();
    let stored = blobs
        .iter()
        .find(|(k, _)| k.contains(&id) && k.contains("request"))
        .unwrap_or_else(|| panic!("no request blob in {:?}", blobs.keys().collect::<Vec<_>>()));
    let parsed: StartDirectRunRequest = serde_json::from_slice(stored.1).unwrap();
    assert_eq!(parsed.model, MODEL);
    assert_eq!(parsed.rules.len(), 1);

    // The run.created event says the run is direct and names the stored request.
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::RunCreated);
    let Payload::Inline { value } = &events[0].payload else {
        panic!("inline payload expected");
    };
    assert_eq!(value["mode"], "direct");
    assert_eq!(value["requestSha256"], hex(stored.1));

    // One execution named after the run, carrying the run id only.
    let started = f.started.lock().unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[&id], json!({"runId": id}));
}

#[tokio::test]
async fn a_retry_with_the_same_key_answers_the_same_run_with_one_execution() {
    let f = Fake::direct();
    let (_, a) = post(&f, "k-1", &body()).await;
    let (status, b) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201);
    assert_eq!(a["runId"], b["runId"]);
    assert_eq!(f.runs.lock().unwrap().len(), 1);
    assert_eq!(f.started.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_same_key_with_a_different_request_is_422() {
    let f = Fake::direct();
    post(&f, "k-1", &body()).await;
    let mut other = body();
    other["prompt"] = json!("something else");
    let (status, v) = post(&f, "k-1", &other).await;
    assert_eq!((status, code(&v)), (422, "idempotency-key-reused"), "{v}");
}

#[tokio::test]
async fn a_failed_execution_start_is_503_and_the_retry_finishes_it() {
    let f = Fake::direct();
    *f.fail_starts.lock().unwrap() = 1;
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 503, "{v}");
    assert!(
        !v.to_string().contains("Throttling"),
        "no internals in the reply: {v}"
    );
    assert_eq!(f.runs.lock().unwrap().len(), 1, "the run is recorded");
    assert!(f.started.lock().unwrap().is_empty());
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(f.runs.lock().unwrap().len(), 1);
    assert_eq!(f.started.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unknown_model_is_422_before_anything_is_written() {
    for model in [
        "zai.glm-5",
        "us.zai.glm-5",
        "",
        "claude",
        "us.anthropic.claude-sonnet-5-5",
        "global.anthropic.claude-sonnet-5-5 ",
    ] {
        let f = Fake::direct();
        let mut b = body();
        b["model"] = json!(model);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!((status, code(&v)), (422, "unknown-model"), "{model:?}: {v}");
        assert!(
            message(&v).contains("us.anthropic.claude-opus-5-5"),
            "lists the five: {v}"
        );
        assert!(f.untouched(), "{model:?}");
    }
}

#[tokio::test]
async fn the_five_models_are_all_accepted() {
    for m in builtin().models() {
        let f = Fake::direct();
        let mut b = body();
        b["model"] = json!(m.id);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(status, 201, "{}: {v}", m.id);
    }
}

#[tokio::test]
async fn bad_rules_are_400_before_anything_is_written() {
    let mismatched = json!({"path": "a.md", "sha256": hex(b"other"), "content": "Rule.\n"});
    let cases: Vec<(Vec<Value>, &str)> = vec![
        (vec![rule("a.md", "")], "empty"),
        (vec![rule("a.md", "---\nUse tests.\n")], "---"),
        (vec![rule("AGENTS.md", "Rule.\n")], "AGENTS.md"),
        (vec![rule("../a.md", "Rule.\n")], "../a.md"),
        (vec![rule("règles.md", "Rule.\n")], "règles.md"),
        (
            vec![rule("a.md", "A.\n"), rule("A.md", "B.\n")],
            "duplicate",
        ),
        (vec![mismatched], "sha256"),
    ];
    for (rules, want) in cases {
        let f = Fake::direct();
        let mut b = body();
        b["rules"] = json!(rules);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!((status, code(&v)), (400, "invalid-rules"), "{want}: {v}");
        assert!(message(&v).contains(want), "{want} not in {}", message(&v));
        assert!(f.untouched(), "{want}");
    }
}

#[tokio::test]
async fn a_request_with_no_rules_is_accepted() {
    let f = Fake::direct();
    let mut b = body();
    b.as_object_mut().unwrap().remove("rules");
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
}

#[tokio::test]
async fn the_nudge_is_checked_and_stored() {
    let text = "Keep going.\n";
    let mut b = body();
    b["nudge"] = json!({"text": text, "sha256": hex(text.as_bytes()), "maxNudges": 5});
    let f = Fake::direct();
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
    let stored: StartDirectRunRequest = {
        let blobs = f.blobs.lock().unwrap();
        serde_json::from_slice(blobs.values().next().unwrap()).unwrap()
    };
    assert_eq!(stored.nudge.unwrap().max_nudges, 5);

    for (nudge, want) in [
        (
            json!({"text": text, "sha256": hex(b"other"), "maxNudges": 3}),
            "sha256",
        ),
        (
            json!({"text": " ", "sha256": hex(b" "), "maxNudges": 3}),
            "empty",
        ),
    ] {
        let f = Fake::direct();
        let mut b = body();
        b["nudge"] = nudge;
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!((status, code(&v)), (400, "invalid-nudge"), "{want}: {v}");
        assert!(message(&v).contains(want), "{want}: {v}");
        assert!(f.untouched());
    }
}

#[tokio::test]
async fn the_prompt_travels_unchanged_and_blank_or_oversize_prompts_are_refused() {
    let f = Fake::direct();
    let mut b = body();
    let prompt = "  Fix the bug.\n\n\tThen add tests.  \n";
    b["prompt"] = json!(prompt);
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
    let stored: StartDirectRunRequest =
        serde_json::from_slice(f.blobs.lock().unwrap().values().next().unwrap()).unwrap();
    assert_eq!(stored.prompt, prompt);
    let id = v["runId"].as_str().unwrap();
    assert_eq!(f.goals.lock().unwrap()[id].objective, prompt);
    assert_eq!(
        f.runs.lock().unwrap()[id].prompt_sha256,
        hex(prompt.as_bytes())
    );

    for blank in ["", "   \n\t "] {
        let f = Fake::direct();
        let mut b = body();
        b["prompt"] = json!(blank);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!((status, code(&v)), (400, "bad-request"), "{v}");
        assert!(f.untouched());
    }
    let f = Fake::direct();
    let mut b = body();
    b["prompt"] = json!("x".repeat(harness_protocol::direct::MAX_REQUEST_BYTES + 1));
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!((status, code(&v)), (413, "payload-too-large"), "{v}");
    assert!(f.untouched());
}

#[tokio::test]
async fn members_of_the_old_shape_and_unknown_members_are_400() {
    for (member, value) in [
        ("models", json!([MODEL])),
        ("width", json!({"m": 2})),
        ("plan", json!({})),
        ("budget", json!({"kind": "tokens", "limit": 5})),
        ("note", json!({"case": "x"})),
        ("extra", json!(1)),
    ] {
        let f = Fake::direct();
        let mut b = body();
        b[member] = value;
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!((status, code(&v)), (400, "bad-request"), "{member}: {v}");
        // The answer says a member is unknown and where; it does not print the member's name back (the name is caller text).
        assert!(
            message(&v).starts_with("StartDirectRunRequest: the body has a member this API does not know (line 1, column "),
            "{member}: {v}"
        );
        assert!(!message(&v).contains(member), "{member}: {v}");
        assert!(f.untouched());
    }
}

#[tokio::test]
async fn header_caller_and_method_rules_match_the_old_route() {
    let f = Fake::direct();
    let d = deps(&f);
    let b = body().to_string();
    let out = handle(req("POST", "/v1/direct/runs", Some(ME), None, &b), &d).await;
    assert_eq!(out.status, 400);
    assert!(out.body.contains("idempotency-key-required"));
    let out = handle(
        req("POST", "/v1/direct/runs", Some(ME), Some("bad key"), &b),
        &d,
    )
    .await;
    assert_eq!(out.status, 400);
    let out = handle(req("POST", "/v1/direct/runs", None, Some("k"), &b), &d).await;
    assert_eq!(out.status, 403);
    for method in ["GET", "PUT", "DELETE"] {
        let out = handle(req(method, "/v1/direct/runs", Some(ME), Some("k"), &b), &d).await;
        assert_eq!(out.status, 405, "{method}");
    }
    let out = handle(
        req("POST", "/v1/direct/runs/", Some(ME), Some("k-9"), &b),
        &d,
    )
    .await;
    assert_eq!(out.status, 201, "a trailing slash is the same route");
    let out = handle(req("POST", "/v1/direct", Some(ME), Some("k"), &b), &d).await;
    assert_eq!(out.status, 404);
    assert!(f.runs.lock().unwrap().len() == 1);
}

#[tokio::test]
async fn the_multi_agent_start_route_is_gone() {
    // POST /v1/runs no longer starts a run: GET lists the caller's runs, POST is a 405, and nothing is written.
    let f = Fake::direct();
    let out = handle(
        req(
            "POST",
            "/v1/runs",
            Some(ME),
            Some("k-1"),
            &body().to_string(),
        ),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 405, "{}", out.body);
    assert!(f.untouched());
}

#[tokio::test]
async fn b776_a_prompt_over_the_old_256_kib_cap_is_accepted_and_stored_whole() {
    let prompt = "\u{e9}".repeat(600_000);
    assert!(prompt.len() > 1_000_000);
    let f = Fake::direct();
    let mut b = body();
    b["prompt"] = json!(prompt);
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap();
    let stored: StartDirectRunRequest =
        serde_json::from_slice(f.blobs.lock().unwrap().values().next().unwrap()).unwrap();
    assert_eq!(stored.prompt, prompt, "the worker reads the whole prompt");
    // The Goal item is a DynamoDB item (400 KB): it keeps the head, cut at a character boundary, and the Run item the hash of the whole.
    let goal = f.goals.lock().unwrap()[id].clone();
    assert!(
        goal.objective.len() <= OBJECTIVE_ITEM_MAX_BYTES,
        "{}",
        goal.objective.len()
    );
    assert!(goal.objective.len() > OBJECTIVE_ITEM_MAX_BYTES - 4);
    assert!(prompt.starts_with(&goal.objective));
    assert_eq!(
        f.runs.lock().unwrap()[id].prompt_sha256,
        hex(prompt.as_bytes())
    );
}

async fn get_run(f: &Fake, id: &str) -> (u16, Value) {
    let out = handle(
        req("GET", &format!("/v1/runs/{id}"), Some(ME), None, ""),
        &deps(f),
    )
    .await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

#[tokio::test]
async fn b776_the_run_detail_gives_back_the_whole_prompt() {
    // A prompt the item holds whole is read from the item; a longer one is read from the stored request.
    for n in [1_000, 300_000, 1_500_000] {
        let prompt = format!("{}\n", "word ".repeat(n / 5));
        let f = Fake::direct();
        let mut b = body();
        b["prompt"] = json!(prompt);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(status, 201, "{n}: {v}");
        let (status, detail) = get_run(&f, v["runId"].as_str().unwrap()).await;
        assert_eq!(status, 200, "{n}: {detail}");
        assert_eq!(detail["objective"], json!(prompt), "{n} bytes");
    }
}

#[tokio::test]
async fn b776_a_reply_is_held_to_the_bound_of_one_lambda_reply() {
    // A run detail whose objective would make the reply larger than a Lambda reply keeps the stored head (see `get_run`); the check is this one.
    use harness_protocol::api::v1::RunDetail;
    let f = Fake::direct();
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    let (status, got) = get_run(&f, v["runId"].as_str().unwrap()).await;
    assert_eq!(status, 200, "{got}");
    let mut detail: RunDetail = serde_json::from_value(got).unwrap();
    detail.objective = Some("x".repeat(harness_protocol::direct::MAX_REQUEST_BYTES - 4_000));
    assert!(control_api::handler::detail_fits_a_reply(&detail));
    detail.objective = Some("x".repeat(harness_protocol::direct::MAX_REQUEST_BYTES));
    assert!(!control_api::handler::detail_fits_a_reply(&detail));
    // Quotes are written twice on the way back: half as many bytes of them fill the reply.
    detail.objective = Some("\"".repeat(harness_protocol::direct::MAX_REQUEST_BYTES / 4 + 1));
    assert!(!control_api::handler::detail_fits_a_reply(&detail));
}

#[tokio::test]
async fn b776_a_request_at_the_bound_is_accepted_and_one_byte_over_is_413_naming_the_bound() {
    use harness_protocol::direct::{MAX_REQUEST_BYTES, event_body_bytes};
    let mut b = body();
    b["prompt"] = json!("");
    let base = event_body_bytes(
        &serde_json::to_vec(&serde_json::from_value::<StartDirectRunRequest>(b.clone()).unwrap())
            .unwrap(),
    );
    let f = Fake::direct();
    b["prompt"] = json!("x".repeat(MAX_REQUEST_BYTES - base));
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(
        status,
        201,
        "{}",
        &v.to_string()[..200.min(v.to_string().len())]
    );
    let f = Fake::direct();
    b["prompt"] = json!("x".repeat(MAX_REQUEST_BYTES - base + 1));
    let (status, v) = post(&f, "k-2", &b).await;
    assert_eq!((status, code(&v)), (413, "payload-too-large"), "{v}");
    for want in ["6,225,920 bytes", "6 MiB", "AWS Lambda", "the prompt"] {
        assert!(message(&v).contains(want), "{want} not in {}", message(&v));
    }
    assert!(f.untouched());
}

#[tokio::test]
async fn b776_the_largest_part_is_named_when_files_make_a_request_too_large() {
    let f = Fake::direct();
    let mut b = body();
    let big = "r".repeat(7 * 1024 * 1024);
    b["rules"] = json!([rule("huge-rules.md", &big)]);
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(
        (status, code(&v)),
        (413, "payload-too-large"),
        "{}",
        message(&v)
    );
    assert!(
        message(&v).contains("the rules") && message(&v).contains("huge-rules.md"),
        "{}",
        message(&v)
    );
    assert!(f.untouched());
}

#[tokio::test]
async fn b776_rules_and_a_nudge_over_the_old_caps_are_stored_whole() {
    let f = Fake::direct();
    let mut b = body();
    let files: Vec<Value> = (0..40)
        .map(|i| {
            rule(
                &format!("r{i:02}.md"),
                &format!("Rule {i}.\n{}", "x".repeat(70 * 1024)),
            )
        })
        .collect();
    b["rules"] = json!(files);
    let text = format!("Go on.\n{}", "y".repeat(100 * 1024));
    b["nudge"] = json!({"text": text, "sha256": hex(text.as_bytes()), "maxNudges": 500});
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(
        status,
        201,
        "{}",
        &v.to_string()[..200.min(v.to_string().len())]
    );
    let stored: StartDirectRunRequest =
        serde_json::from_slice(f.blobs.lock().unwrap().values().next().unwrap()).unwrap();
    assert_eq!(stored.rules.len(), 40);
    let nudge = stored.nudge.unwrap();
    assert_eq!((nudge.max_nudges, nudge.text.len()), (500, text.len()));
}

#[tokio::test]
async fn b776_more_rule_files_than_the_summary_can_list_are_refused_with_the_count() {
    let f = Fake::direct();
    let mut b = body();
    let files: Vec<Value> = (0..101)
        .map(|i| rule(&format!("r{i:03}.md"), "Rule.\n"))
        .collect();
    b["rules"] = json!(files);
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!((status, code(&v)), (400, "invalid-rules"), "{v}");
    assert!(
        message(&v).contains("101") && message(&v).contains("100"),
        "{}",
        message(&v)
    );
    assert!(f.untouched());
}

// ---- the default model and the model list ----

async fn get_models(f: &Fake, caller: Option<&str>) -> (u16, Value) {
    let out = handle(req("GET", "/v1/direct/models", caller, None, ""), &deps(f)).await;
    (out.status, serde_json::from_str(&out.body).unwrap())
}

#[tokio::test]
async fn the_model_list_names_the_default_and_marks_what_can_be_chosen() {
    let f = Fake::direct();
    let (status, v) = get_models(&f, Some(ME)).await;
    assert_eq!(status, 200, "{v}");
    let list: ham_core::models::ModelList = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(list.default, MODEL, "the default is Claude Sonnet 5.5");
    assert_eq!(list.models.len(), 5);
    let marked: Vec<&str> = list
        .models
        .iter()
        .filter(|m| m.default)
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(marked, [MODEL], "exactly one row is marked default");
    let sonnet = list.models.iter().find(|m| m.id == MODEL).unwrap();
    assert_eq!(sonnet.alias.as_deref(), Some("sonnet"));
    assert!(sonnet.selectable);
    // The list is exactly the built-in file.
    assert_eq!(list, builtin().list());
    // JSON keys are camelCase for the TUI and scripts.
    assert_eq!(
        v["models"][0]["contextTokens"],
        list.models[0].context_tokens
    );
    assert!(f.untouched(), "reading the list writes nothing");
}

#[tokio::test]
async fn the_model_list_is_served_to_a_known_caller_only() {
    let f = Fake::direct();
    let (status, v) = get_models(&f, None).await;
    assert_eq!((status, code(&v)), (403, "forbidden"), "{v}");
    for method in ["POST", "PUT", "DELETE"] {
        let out = handle(
            req(method, "/v1/direct/models", Some(ME), Some("k"), "{}"),
            &deps(&f),
        )
        .await;
        assert_eq!(out.status, 405, "{method}");
    }
    let out = handle(
        req("GET", "/v1/direct/models/", Some(ME), None, ""),
        &deps(&f),
    )
    .await;
    assert_eq!(out.status, 200, "a trailing slash is the same route");
}

#[tokio::test]
async fn the_default_model_starts_a_run_and_every_listed_model_is_judged_by_its_smoke_record() {
    let f = Fake::direct();
    let mut b = body();
    b["model"] = json!(builtin().default_model().unwrap().id);
    let (status, v) = post(&f, "k-default", &b).await;
    assert_eq!(status, 201, "the default model is accepted: {v}");
    for (i, row) in builtin().list().models.iter().enumerate() {
        let mut b = body();
        b["model"] = json!(row.id);
        let (status, v) = post(&f, &format!("k-row-{i}"), &b).await;
        if row.selectable {
            assert_eq!(status, 201, "{}: {v}", row.id);
        } else {
            assert_eq!(
                (status, code(&v)),
                (422, "model-not-smoked"),
                "{}: {v}",
                row.id
            );
        }
    }
    // An alias is a client convenience: the server takes ids only.
    let mut b = body();
    b["model"] = json!("sonnet");
    let (status, v) = post(&f, "k-alias", &b).await;
    assert_eq!((status, code(&v)), (422, "unknown-model"), "{v}");
}

// ---- inputWaitS ------------------------------------------------------------------------------
//
// An interactive `ham` asks for an answer window (`inputWaitS`, seconds). The server default is no wait, so a request that
// leaves the member out, or sends 0, is stored byte for byte as it was before the member existed: the stored object is what an
// agentd of an older image parses (it refuses unknown members), and the release gate and every script send no member at all.

/// The one request object a fresh fake holds.
fn stored_request_bytes(f: &Fake) -> Vec<u8> {
    let blobs = f.blobs.lock().unwrap();
    assert_eq!(blobs.len(), 1, "one request object: {:?}", blobs.keys());
    blobs.values().next().unwrap().clone()
}

/// The bytes `serde_json` made of the request before `inputWaitS` existed, for `body()`.
fn pre_input_wait_bytes() -> Vec<u8> {
    format!(
        concat!(
            r#"{{"prompt":"Add percent_change(a, b) with tests","#,
            r#""target":{{"mode":"repo","repo":"cc:hamh-e2e-python-calc"}},"#,
            r#""model":"{model}","#,
            r#""rules":[{{"path":"00-my-rules.md","sha256":"{sha}","content":"Write the test first.\n"}}]}}"#
        ),
        model = MODEL,
        sha = hex(b"Write the test first.\n"),
    )
    .into_bytes()
}

#[tokio::test]
async fn a_request_without_input_wait_s_is_stored_byte_for_byte_as_before() {
    let f = Fake::direct();
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(
        String::from_utf8(stored_request_bytes(&f)).unwrap(),
        String::from_utf8(pre_input_wait_bytes()).unwrap()
    );
}

#[tokio::test]
async fn zero_and_null_input_wait_s_are_the_same_request_as_none_at_all() {
    let f = Fake::direct();
    let (status, none) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201, "{none}");
    let want = stored_request_bytes(&f);
    for (i, value) in [json!(0), json!(null)].into_iter().enumerate() {
        let g = Fake::direct();
        let mut b = body();
        b["inputWaitS"] = value.clone();
        let (status, v) = post(&g, "k-1", &b).await;
        assert_eq!(status, 201, "inputWaitS {value}: {v}");
        assert_eq!(
            stored_request_bytes(&g),
            want,
            "inputWaitS {value} must not reach the stored request (case {i})"
        );
    }
    // The same Idempotency-Key with 0 after none is a retry of the same request, not a different one.
    let mut zero = body();
    zero["inputWaitS"] = json!(0);
    let (status, again) = post(&f, "k-1", &zero).await;
    assert_eq!(status, 201, "{again}");
    assert_eq!(again["runId"], none["runId"]);
    assert_eq!(f.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn input_wait_s_in_range_is_stored_as_the_client_sent_it() {
    for s in [1u32, 60, 1800, 14_400] {
        let f = Fake::direct();
        let mut b = body();
        b["inputWaitS"] = json!(s);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(status, 201, "inputWaitS {s}: {v}");
        let stored: Value = serde_json::from_slice(&stored_request_bytes(&f)).unwrap();
        assert_eq!(stored["inputWaitS"], json!(s), "{stored}");
        let parsed: StartDirectRunRequest = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(parsed.input_wait_s, Some(s));
        // The run.created event carries the digest of exactly this object.
        let events = f.events.lock().unwrap();
        let Payload::Inline { value } = &events[0].payload else {
            panic!("inline payload expected");
        };
        assert_eq!(value["requestSha256"], hex(&stored_request_bytes(&f)));
    }
}

#[tokio::test]
async fn input_wait_s_over_the_maximum_is_400_before_anything_is_written() {
    for s in [14_401u64, 86_400, u64::from(u32::MAX)] {
        let f = Fake::direct();
        let mut b = body();
        b["inputWaitS"] = json!(s);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(
            (status, code(&v)),
            (400, "bad-request"),
            "inputWaitS {s}: {v}"
        );
        assert!(message(&v).contains("inputWaitS"), "{v}");
        assert!(message(&v).contains("14400"), "the limit is named: {v}");
        assert!(f.untouched(), "inputWaitS {s} wrote something");
    }
}

#[tokio::test]
async fn input_wait_s_that_is_not_a_whole_number_of_seconds_is_400() {
    for (value, shown) in [
        (json!(-1), "-1"),
        (json!(1.5), "1.5"),
        (json!("30"), "\"30\""),
        (json!(true), "true"),
        (json!(4_294_967_296u64), "4294967296"),
    ] {
        let f = Fake::direct();
        let mut b = body();
        b["inputWaitS"] = value;
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(
            (status, code(&v)),
            (400, "bad-request"),
            "inputWaitS {shown}: {v}"
        );
        assert!(f.untouched(), "inputWaitS {shown} wrote something");
    }
}

#[tokio::test]
async fn a_different_input_wait_s_is_a_different_request_for_the_same_key() {
    let f = Fake::direct();
    let mut a = body();
    a["inputWaitS"] = json!(1800);
    assert_eq!(post(&f, "k-1", &a).await.0, 201);
    let mut b = body();
    b["inputWaitS"] = json!(600);
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!((status, code(&v)), (422, "idempotency-key-reused"), "{v}");
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!((status, code(&v)), (422, "idempotency-key-reused"), "{v}");
}

// ---- reportStepS -------------------------------------------------------------------------------
//
// The user's `run.report_command_minutes` becomes `reportStepS`: the seconds one command of the clean-checkout test report may run. The member is
// left out of the stored request when the client sends none, so the object an agentd of an older image parses is what it was; a value outside
// 1 to 3600 is refused before anything is written.

#[tokio::test]
async fn report_step_s_in_range_is_stored_as_the_client_sent_it() {
    for s in [1u32, 300, 360, 3600] {
        let f = Fake::direct();
        let mut b = body();
        b["reportStepS"] = json!(s);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(status, 201, "reportStepS {s}: {v}");
        let stored: Value = serde_json::from_slice(&stored_request_bytes(&f)).unwrap();
        assert_eq!(stored["reportStepS"], json!(s), "{stored}");
        let parsed: StartDirectRunRequest = serde_json::from_value(stored).unwrap();
        assert_eq!(parsed.report_step_s, Some(s));
    }
}

#[tokio::test]
async fn a_request_without_report_step_s_stores_no_such_member() {
    let f = Fake::direct();
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!(status, 201, "{v}");
    let stored: Value = serde_json::from_slice(&stored_request_bytes(&f)).unwrap();
    assert!(stored.get("reportStepS").is_none(), "{stored}");
}

#[tokio::test]
async fn report_step_s_outside_one_second_to_the_session_bound_is_400_before_anything_is_written() {
    for s in [0u64, 27_901, 86_400, u64::from(u32::MAX)] {
        let f = Fake::direct();
        let mut b = body();
        b["reportStepS"] = json!(s);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(
            (status, code(&v)),
            (400, "bad-request"),
            "reportStepS {s}: {v}"
        );
        assert!(message(&v).contains("reportStepS"), "{v}");
        assert!(message(&v).contains("27900"), "the limit is named: {v}");
        assert!(f.untouched(), "reportStepS {s} wrote something");
    }
}

#[tokio::test]
async fn a_different_report_step_s_is_a_different_request_for_the_same_key() {
    let f = Fake::direct();
    let mut a = body();
    a["reportStepS"] = json!(600);
    assert_eq!(post(&f, "k-1", &a).await.0, 201);
    let (status, v) = post(&f, "k-1", &body()).await;
    assert_eq!((status, code(&v)), (422, "idempotency-key-reused"), "{v}");
}

#[test]
fn b776_head_bytes_cuts_back_to_a_character_boundary() {
    assert_eq!(head_bytes("abc", 10), "abc");
    assert_eq!(head_bytes("abc", 3), "abc");
    // The euro sign takes bytes 1 to 3, and the grinning face four.
    assert_eq!(head_bytes("a\u{20ac}b", 2), "a");
    assert_eq!(head_bytes("a\u{20ac}b", 3), "a");
    assert_eq!(head_bytes("a\u{20ac}b", 4), "a\u{20ac}");
    assert_eq!(head_bytes("\u{1f600}", 3), "");
}

#[tokio::test]
async fn b776_the_goal_item_keeps_a_head_cut_at_a_character_boundary() {
    let prompt = format!("a{}", "\u{1f600}".repeat(100_000));
    assert!(prompt.len() > OBJECTIVE_ITEM_MAX_BYTES);
    let f = Fake::direct();
    let mut b = body();
    b["prompt"] = json!(prompt);
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(status, 201, "{v}");
    let goal = f.goals.lock().unwrap()[v["runId"].as_str().unwrap()].clone();
    assert_eq!(goal.objective.len(), 1 + 4 * 65_535);
    assert!(prompt.starts_with(&goal.objective));
}

async fn get_run_logged(f: &Fake, id: &str) -> (Value, Vec<String>) {
    let (out, logs) = handle_logged(
        req("GET", &format!("/v1/runs/{id}"), Some(ME), None, ""),
        &deps(f),
    )
    .await;
    assert_eq!(out.status, 200, "{}", out.body);
    (serde_json::from_str(&out.body).unwrap(), logs)
}

/// What a case does to the stored request (or to the store) after the run exists.
type Spoil = fn(&Fake, &str);

#[tokio::test]
async fn b776_a_prompt_that_cannot_be_read_back_leaves_the_stored_head_and_a_log_line() {
    use harness_protocol::direct::PLATFORM_REQUEST_BYTES;
    let long = format!("{}\n", "word ".repeat(60_000));
    assert!(long.len() > OBJECTIVE_ITEM_MAX_BYTES);
    let cases: [(&str, Spoil); 5] = [
        ("the stored request is absent", |f, key| {
            f.blobs.lock().unwrap().remove(key);
        }),
        ("the stored request could not be read", |f, _| {
            *f.fail_reads.lock().unwrap() = true;
        }),
        (
            "the stored request is larger than the route reads",
            |f, key| {
                let size = PLATFORM_REQUEST_BYTES + 1024 * 1024 + 10;
                f.blobs
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), vec![b'x'; size]);
            },
        ),
        (
            "the stored prompt is not the prompt of the run",
            |f, key| {
                f.blobs
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), br#"{"prompt":"another prompt"}"#.to_vec());
            },
        ),
        ("the stored request is not a request", |f, key| {
            f.blobs
                .lock()
                .unwrap()
                .insert(key.to_string(), b"not json".to_vec());
        }),
    ];
    for (why, spoil) in cases {
        let f = Fake::direct();
        let mut b = body();
        b["prompt"] = json!(long);
        let (status, v) = post(&f, "k-1", &b).await;
        assert_eq!(status, 201, "{why}: {v}");
        let id = v["runId"].as_str().unwrap();
        // The route reads the whole prompt when nothing is spoilt: the trigger of every case below.
        let (detail, logs) = get_run_logged(&f, id).await;
        assert_eq!(detail["objective"], json!(long), "{why}: trigger present");
        assert!(logs.is_empty(), "{why}: {logs:?}");
        spoil(&f, &harness_protocol::store::s3::request_key(id));
        let head = f.goals.lock().unwrap()[id].objective.clone();
        let (detail, logs) = get_run_logged(&f, id).await;
        assert_eq!(detail["objective"], json!(head), "{why}");
        assert!(
            logs.iter().any(|l| l.contains(&format!("({why})"))),
            "{why}: {logs:?}"
        );
    }
}

#[tokio::test]
async fn b776_a_prompt_that_would_make_the_reply_too_large_leaves_the_stored_head_and_a_log_line() {
    use harness_protocol::direct::{MAX_REQUEST_BYTES, event_body_bytes};
    let mut b = body();
    b["prompt"] = json!("");
    let base = event_body_bytes(
        &serde_json::to_vec(&serde_json::from_value::<StartDirectRunRequest>(b.clone()).unwrap())
            .unwrap(),
    );
    let f = Fake::direct();
    // The largest prompt a request may carry: the reply holds it and the fields of the run detail around it, which is more.
    b["prompt"] = json!("x".repeat(MAX_REQUEST_BYTES - base));
    let (status, v) = post(&f, "k-1", &b).await;
    assert_eq!(
        status,
        201,
        "{}",
        &v.to_string()[..200.min(v.to_string().len())]
    );
    let id = v["runId"].as_str().unwrap();
    let head = f.goals.lock().unwrap()[id].objective.clone();
    let (detail, logs) = get_run_logged(&f, id).await;
    assert_eq!(detail["objective"], json!(head));
    assert!(
        logs.iter().any(|l| l.contains("does not fit one reply")),
        "{logs:?}"
    );
}
