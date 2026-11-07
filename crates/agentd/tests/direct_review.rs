// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! The review round of the stage loop. When the engine ends a turn with a message and the harness counted at
//! least one changed file, the driver sends ONE more prompt in the same session (the review prompt, with `{{base}}` and `{{facts}}` filled in),
//! accepts the turn that follows under the rules of any turn, and records the round. These tests run the real driver over in-memory streams with a
//! scripted engine and a paused clock. They pin: the one extra prompt and its substitutions; each case in which the round is skipped, with its
//! reason; the label, which does not change; a round whose turn fails (the outcome of the turn before it stands); a cancel during the round; the
//! evidence fields; and the pure rules (the gate, the facts, the template).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::acp::{AcpClient, Activity, Watched};
use agentd::direct::classify::{RawEnd, TreeFacts, classify, refine};
use agentd::direct::collector::DirectCollector;
use agentd::direct::context::ContextDir;
use agentd::direct::driver::{
    DriverEnd, InputWiring, NoRespawn, PromptPlan, TreeProbe, run_direct_stages,
};
use agentd::direct::hooks::{DirectHook, Hooks, TurnEnd, TurnVote};
use agentd::direct::input::{ChangedFiles, DigestSource, DigestSources, InputPolicy, TreeSource};
use agentd::direct::retry::RetryPolicy;
use agentd::direct::review::{
    FactsInput, Gate, ReviewPlan, ReviewRepo, asks_the_user, check_member, facts_text, render,
    says_blocked, skip_reason,
};
use agentd::events::EventRecorder;
use harness_protocol::direct::{
    DEFAULT_REVIEW_PROMPT, DirectOutcome, REVIEW_CANCELLED, REVIEW_COMPLETED, REVIEW_FAILED,
    REVIEW_RAN, REVIEW_SKIP_BLOCKED, REVIEW_SKIP_CANCELLED, REVIEW_SKIP_FILES_UNKNOWN,
    REVIEW_SKIP_FORCED_STOP, REVIEW_SKIP_NO_CHANGED_FILE, REVIEW_SKIP_NO_TIME,
    REVIEW_SKIP_NOT_DONE, REVIEW_SKIP_OFF, REVIEW_SKIP_QUESTION, REVIEW_SKIPPED, ReviewMode,
    ReviewOptions,
};
use harness_protocol::envelope::{Envelope, Payload};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::{mpsc, watch};

const MODEL: &str = "us.anthropic.claude-opus-5-5";
const TOP: &str = "/mnt/workspace/run1/direct/repo";
const PROMPT: &str = "Add percent_change\nwith tests\n";
const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD_A: &str = "2222222222222222222222222222222222222222";
const HEAD_B: &str = "3333333333333333333333333333333333333333";

// ---- the scripted engine ------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Step {
    Update(Value),
    Finish(&'static str, Option<Value>),
    ErrorWith(i64, &'static str),
    /// Nothing more, ever (the process stays open).
    Silent,
    /// The process dies: its output closes.
    Die,
}

fn chunk(text: &str) -> Step {
    Step::Update(
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
    )
}

fn end_turn(text: &str) -> Vec<Step> {
    vec![chunk(text), Step::Finish("end_turn", None)]
}

fn frame(v: Value) -> String {
    format!("{v}\n")
}

type Received = Arc<Mutex<Vec<Value>>>;

/// One engine process with one session; `prompts[n]` is what the n-th `session/prompt` is answered with.
fn spawn_engine(prompts: Vec<Vec<Step>>) -> (DuplexStream, DuplexStream, Received) {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut out, client_r) = tokio::io::duplex(1 << 16);
    let received: Received = Arc::default();
    let seen = received.clone();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&raw).unwrap();
            seen.lock().unwrap().push(msg.clone());
            let _ = tx.send(msg);
        }
    });
    tokio::spawn(async move {
        let mut prompt_no = 0usize;
        while let Some(msg) = rx.recv().await {
            let id = msg.get("id").cloned();
            let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
            match msg["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    out.write_all(frame(reply(json!({"protocolVersion": 1}))).as_bytes())
                        .await
                        .unwrap();
                }
                "session/new" => {
                    let options = json!([{"id": "model", "currentValue": MODEL, "options": []}]);
                    out.write_all(
                        frame(reply(json!({"sessionId": "s-1", "configOptions": options})))
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                }
                "session/prompt" => {
                    let steps = prompts.get(prompt_no).cloned().unwrap_or_default();
                    prompt_no += 1;
                    for step in steps {
                        match step {
                            Step::Update(u) => out
                                .write_all(
                                    frame(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": u}}))
                                        .as_bytes(),
                                )
                                .await
                                .unwrap(),
                            Step::Finish(reason, usage) => {
                                let mut result = json!({"stopReason": reason});
                                if let Some(u) = usage {
                                    result["_meta"] = json!({"usage": u});
                                }
                                out.write_all(frame(reply(result)).as_bytes()).await.unwrap();
                            }
                            Step::ErrorWith(code, message) => out
                                .write_all(
                                    frame(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
                                        .as_bytes(),
                                )
                                .await
                                .unwrap(),
                            Step::Silent => std::future::pending::<()>().await,
                            Step::Die => return,
                        }
                        out.flush().await.unwrap();
                    }
                }
                _ => {}
            }
            out.flush().await.unwrap();
        }
    });
    (client_r, client_w, received)
}

// ---- the doubles of the work tree and git -------------------------------------------------------------------------------------

/// The changed files the tree reports, one entry per call; the last repeats. `paths` are listed as far as the limit asked for.
struct Tree {
    totals: Mutex<VecDeque<Option<u32>>>,
    last: Mutex<Option<u32>>,
    paths: Vec<String>,
    limits: Mutex<Vec<usize>>,
}

impl Tree {
    fn new(totals: Vec<Option<u32>>, paths: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            totals: Mutex::new(totals.into()),
            last: Mutex::new(None),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            limits: Mutex::new(Vec::new()),
        })
    }
}

impl TreeSource for Tree {
    fn changed_files(&self, limit: usize) -> BoxFuture<'_, Option<ChangedFiles>> {
        Box::pin(async move {
            self.limits.lock().unwrap().push(limit);
            let mut q = self.totals.lock().unwrap();
            let mut last = self.last.lock().unwrap();
            if let Some(next) = q.pop_front() {
                *last = next;
            }
            (*last).map(|total| ChangedFiles {
                paths: self.paths.iter().take(limit).cloned().collect(),
                total,
            })
        })
    }
}

/// `TreeSource` of an `Arc<Tree>`: the test keeps its own handle to read the limits asked for.
struct SharedTree(Arc<Tree>);

impl TreeSource for SharedTree {
    fn changed_files(&self, limit: usize) -> BoxFuture<'_, Option<ChangedFiles>> {
        self.0.changed_files(limit)
    }
}

/// The git facts: `HEAD` as the round starts and as it ends, the top of the base tree and the untracked count.
struct Repo {
    heads: Mutex<VecDeque<Option<String>>>,
    top: Option<Vec<String>>,
    untracked: Option<u32>,
}

impl Repo {
    fn new(heads: &[Option<&str>]) -> Arc<Self> {
        Arc::new(Self {
            heads: Mutex::new(heads.iter().map(|h| h.map(str::to_string)).collect()),
            top: Some(vec!["src".into(), "README.md".into(), "docs".into()]),
            untracked: Some(2),
        })
    }
}

impl ReviewRepo for Repo {
    fn head(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move { self.heads.lock().unwrap().pop_front().flatten() })
    }
    fn base_top_level(&self) -> BoxFuture<'_, Option<Vec<String>>> {
        Box::pin(async move { self.top.clone() })
    }
    fn untracked(&self) -> BoxFuture<'_, Option<u32>> {
        Box::pin(async move { self.untracked })
    }
}

/// The fingerprint of the work tree, one per call (HEAD, a line feed, the tree): the same string twice means nothing changed.
struct Probe(Mutex<VecDeque<String>>);

impl TreeProbe for Probe {
    fn fingerprint(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>> {
        Box::pin(async move { self.0.lock().unwrap().pop_front().unwrap_or_default() })
    }
}

struct Source;

impl DigestSource for Source {
    fn gather(&self) -> BoxFuture<'_, DigestSources> {
        Box::pin(async {
            DigestSources {
                task: PROMPT.to_string(),
                ..DigestSources::default()
            }
        })
    }
}

/// A module that ends the run at its first turn end, the way the goal loop's brake does.
struct Brake;

impl DirectHook for Brake {
    fn name(&self) -> &'static str {
        "brake"
    }
    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async {
            TurnVote::Stop {
                detail: "The harness ended the run: a test brake.".to_string(),
            }
        })
    }
}

/// A module that counts the turn ends it is asked about and, at its second seal, votes a late message (a user's message that arrived last).
struct LateMessage {
    turn_ends: AtomicU32,
    seals: AtomicU32,
}

impl LateMessage {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            turn_ends: AtomicU32::new(0),
            seals: AtomicU32::new(0),
        })
    }
}

impl DirectHook for LateMessage {
    fn name(&self) -> &'static str {
        "late-message"
    }
    fn on_turn_end<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        self.turn_ends.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { TurnVote::Done })
    }
    fn on_seal<'a>(&'a self, _end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        let n = self.seals.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            if n == 2 {
                TurnVote::Prompt("A late message from the user.".to_string())
            } else {
                TurnVote::Done
            }
        })
    }
}

// ---- running the driver ------------------------------------------------------------------------------------------------------

struct Setup {
    prompts: Vec<Vec<Step>>,
    totals: Vec<Option<u32>>,
    paths: Vec<&'static str>,
    review: Option<ReviewOptions>,
    stage_on: bool,
    guard: Duration,
    timeout_s: u64,
    silence_s: u64,
    heads: Vec<Option<&'static str>>,
    fingerprints: Vec<&'static str>,
    hooks: Vec<Arc<dyn DirectHook>>,
    cancel: Option<watch::Receiver<bool>>,
}

impl Setup {
    fn new(prompts: Vec<Vec<Step>>) -> Self {
        Self {
            prompts,
            totals: vec![Some(3)],
            paths: vec!["src/a.py", "src/b.py", "tests/test_a.py"],
            review: None,
            stage_on: true,
            guard: Duration::from_secs(1200),
            timeout_s: 30_000,
            silence_s: 900,
            heads: vec![Some(HEAD_A), Some(HEAD_B)],
            fingerprints: vec!["a\nt1", "b\nt2"],
            hooks: Vec::new(),
            cancel: None,
        }
    }
}

struct Ran {
    end: DriverEnd,
    received: Received,
    events: Vec<Value>,
    tree: Arc<Tree>,
}

fn plan(timeout_s: u64, silence_s: u64, cancel: Option<watch::Receiver<bool>>) -> PromptPlan {
    PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: PROMPT.into(),
        nudge: None,
        cancel,
        timeout: Duration::from_secs(timeout_s),
        silence: Duration::from_secs(silence_s),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
    }
}

fn policy() -> InputPolicy {
    InputPolicy {
        wait: Duration::ZERO,
        max_requests: 3,
        margin: Duration::from_secs(900),
        min_window: Duration::from_secs(300),
        max_auto_sessions: 0,
        max_user_sessions: 3,
    }
}

fn kind_of(e: &Envelope) -> Option<Value> {
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    value.get("kind").is_some().then(|| value.clone())
}

async fn run(s: Setup) -> Ran {
    let (r, w, received) = spawn_engine(s.prompts);
    let (_keep_cancel, cancel_rx) = match s.cancel {
        Some(rx) => (None, rx),
        None => {
            let (tx, rx) = watch::channel(false);
            (Some(tx), rx)
        }
    };
    let (_instr_tx, instr_rx) = mpsc::channel(4);
    let rec = EventRecorder::new("run1", "direct");
    let activity = Activity::new();
    let hooks = if s.hooks.is_empty() {
        Hooks::none()
    } else {
        Hooks::new(s.hooks)
    };
    let mut client = AcpClient::new(r, w)
        .with_activity(activity.clone())
        .with_cancel(cancel_rx.clone(), Duration::from_secs(10));
    if let Some(tap) = hooks.tap() {
        client = client.with_tap(tap);
    }
    let tree = Tree::new(s.totals, &s.paths);
    let dir = common::scratch("review");
    let wiring = InputWiring {
        policy: policy(),
        instructions: instr_rx,
        cancel: cancel_rx.clone(),
        tree: Arc::new(SharedTree(tree.clone())),
        sources: Arc::new(Source),
        context: ContextDir::create(&dir).unwrap(),
        task: PROMPT.to_string(),
        engine_label: "the engine".to_string(),
        routes: Vec::new(),
        rec: rec.clone(),
        audit: None,
        log: None,
        review: Some(ReviewPlan::new(
            s.review.as_ref(),
            s.stage_on,
            s.guard,
            BASE,
            Repo::new(&s.heads),
        )),
    };
    let mut collector = DirectCollector::new(rec.clone()).with_hooks(hooks);
    let probe = Probe(Mutex::new(
        s.fingerprints.iter().map(|f| f.to_string()).collect(),
    ));
    let mut host = NoRespawn::new();
    let p = plan(s.timeout_s, s.silence_s, Some(cancel_rx));
    let end = run_direct_stages(
        &mut client,
        &mut collector,
        &activity,
        &p,
        &probe,
        Some(wiring),
        &mut host,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events: Vec<Value> = rec.drain().iter().filter_map(kind_of).collect();
    Ran {
        end,
        received,
        events,
        tree,
    }
}

/// The texts of the `session/prompt` requests the engine received, in order.
fn prompts_sent(r: &Received) -> Vec<String> {
    r.lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .map(|m| {
            m["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn review_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|v| v["kind"] == "review-round")
        .collect()
}

fn label(end: &DriverEnd, files: u32) -> DirectOutcome {
    refine(
        classify(&end.raw),
        TreeFacts {
            files_changed: Some(files),
        },
    )
    .outcome
}

// ---- the round ----------------------------------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn one_extra_prompt_is_sent_in_the_same_session_with_the_base_and_the_facts() {
    let mut s = Setup::new(vec![
        end_turn("Added percent_change."),
        end_turn("Reviewed it; fixed one stale line."),
        // A third answer exists, and the round must not ask for it.
        end_turn("never asked for"),
    ]);
    s.paths = vec![
        "src/lib.rs",
        "evidence/run.md",
        ".ham/lifecycle/LOG.md",
        "README.md",
    ];
    s.totals = vec![Some(4)];
    let r = run(s).await;
    let sent = prompts_sent(&r.received);
    assert_eq!(sent.len(), 2, "exactly one extra prompt: {sent:?}");
    assert_eq!(sent[0], PROMPT, "the user's prompt is byte for byte");
    let review = &sent[1];
    assert!(!review.contains("{{"), "every token is replaced: {review}");
    assert!(
        review.contains(&format!("the commit you started from: {BASE}.")),
        "{review}"
    );
    assert!(
        review.contains(
            "- Changed files: 4 (src/lib.rs, evidence/run.md, .ham/lifecycle/LOG.md, README.md)."
        ),
        "{review}"
    );
    assert!(
        review.contains("- New top-level folders: evidence/."),
        "the records folder is not a stray folder: {review}"
    );
    assert!(
        review.contains(
            "- Files under .ham/ (records the harness reads; keep them): .ham/lifecycle/LOG.md."
        ),
        "{review}"
    );
    assert!(review.contains("- Untracked files: 2."), "{review}");
    assert!(review.contains("never git stash"), "{review}");
    // Both turns went to one session, with no session of its own for the round.
    let methods: Vec<String> = r
        .received
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| m["method"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/prompt"
        ]
    );
    for m in r
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/prompt")
    {
        assert_eq!(m["params"]["sessionId"], "s-1");
    }
}

#[tokio::test(start_paused = true)]
async fn the_label_is_unchanged_and_the_last_message_is_the_rounds() {
    let r = run(Setup::new(vec![
        end_turn("Added percent_change."),
        end_turn("Reviewed it; nothing else to fix."),
    ]))
    .await;
    assert_eq!(label(&r.end, 3), DirectOutcome::ReportedDone);
    assert_eq!(r.end.raw.message, "Reviewed it; nothing else to fix.");
    assert!(matches!(r.end.raw.watched, Watched::Done(Ok(_))));
    // A tree the harness could not count keeps the label, and a tree with no changed file never had it.
    assert_eq!(label(&r.end, 0), DirectOutcome::StoppedNoChanges);
}

#[tokio::test(start_paused = true)]
async fn the_round_is_recorded_with_its_commits_files_seconds_and_tokens() {
    let usage1 = json!({"inputTokens": 1000, "outputTokens": 200});
    let usage2 = json!({"inputTokens": 4321, "outputTokens": 876});
    let mut s = Setup::new(vec![
        vec![
            chunk("Added percent_change."),
            Step::Finish("end_turn", Some(usage1)),
        ],
        vec![chunk("Reviewed."), Step::Finish("end_turn", Some(usage2))],
    ]);
    s.heads = vec![Some(HEAD_A), Some(HEAD_B)];
    s.fingerprints = vec!["a\nt1", "b\nt2"];
    let r = run(s).await;
    let rec = r.end.review.as_ref().expect("a record");
    assert_eq!(rec.status, REVIEW_RAN);
    assert_eq!(rec.reason, None);
    assert_eq!(rec.result.as_deref(), Some(REVIEW_COMPLETED));
    assert_eq!(rec.commit_before.as_deref(), Some(HEAD_A));
    assert_eq!(rec.commit_after.as_deref(), Some(HEAD_B));
    assert_eq!(rec.changed_files, Some(true), "the fingerprint changed");
    assert_eq!(rec.input_tokens, Some(4321));
    assert_eq!(rec.output_tokens, Some(876));
    assert!(rec.seconds.is_some());
    assert!(rec.is_valid());
    // The run's usage is the sum of both turns.
    let total = r.end.usage.unwrap();
    assert_eq!((total.input_tokens, total.output_tokens), (5321, 1076));
}

#[tokio::test(start_paused = true)]
async fn a_round_that_changed_nothing_says_so() {
    let mut s = Setup::new(vec![end_turn("Done."), end_turn("Looked again; all fine.")]);
    s.heads = vec![Some(HEAD_A), Some(HEAD_A)];
    s.fingerprints = vec!["same", "same"];
    let r = run(s).await;
    let rec = r.end.review.unwrap();
    assert_eq!(rec.changed_files, Some(false));
    assert_eq!(rec.commit_before, rec.commit_after);
}

#[tokio::test(start_paused = true)]
async fn a_round_that_only_committed_what_was_there_moved_the_head_and_changed_no_file() {
    let mut s = Setup::new(vec![end_turn("Done."), end_turn("Committed it.")]);
    s.heads = vec![Some(HEAD_A), Some(HEAD_B)];
    s.fingerprints = vec!["a\nsame-tree", "b\nsame-tree"];
    let r = run(s).await;
    let rec = r.end.review.unwrap();
    assert_eq!(rec.commit_before.as_deref(), Some(HEAD_A));
    assert_eq!(rec.commit_after.as_deref(), Some(HEAD_B));
    assert_eq!(
        rec.changed_files,
        Some(false),
        "a commit is not a change of a file"
    );
}

#[tokio::test(start_paused = true)]
async fn a_probe_that_could_not_read_the_tree_says_nothing_about_changed_files() {
    let mut s = Setup::new(vec![end_turn("Done."), end_turn("Fine.")]);
    s.fingerprints = vec!["error-0", "error-1"];
    let r = run(s).await;
    assert_eq!(r.end.review.unwrap().changed_files, None);
}

#[tokio::test(start_paused = true)]
async fn what_git_cannot_say_is_left_out_not_made_up() {
    let mut s = Setup::new(vec![end_turn("Done."), end_turn("Fine.")]);
    s.heads = vec![None, None];
    s.fingerprints = vec![];
    let r = run(s).await;
    let rec = r.end.review.unwrap();
    assert_eq!(rec.status, REVIEW_RAN);
    assert_eq!(rec.commit_before, None);
    assert_eq!(rec.commit_after, None);
    assert_eq!(
        rec.changed_files, None,
        "two empty fingerprints say nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn the_round_events_open_and_close_once() {
    let r = run(Setup::new(vec![end_turn("Done."), end_turn("Fine.")])).await;
    let ev = review_events(&r.events);
    let phases: Vec<&str> = ev.iter().map(|e| e["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, ["start", "end"], "{ev:?}");
    assert_eq!(ev[1]["result"], REVIEW_COMPLETED);
    for e in ev {
        assert!(e["text"].as_str().unwrap().len() > 10);
        assert_eq!(e["turn"], 1);
    }
}

#[tokio::test(start_paused = true)]
async fn the_facts_ask_the_tree_for_more_than_a_stop_digest_needs() {
    let r = run(Setup::new(vec![end_turn("Done."), end_turn("Fine.")])).await;
    let limits = r.tree.limits.lock().unwrap().clone();
    assert!(
        limits.contains(&agentd::direct::review::FACTS_PATHS_ASKED),
        "{limits:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_text_of_the_users_is_sent_and_the_default_is_used_without_one() {
    let mut s = Setup::new(vec![end_turn("Done."), end_turn("Fine.")]);
    s.review = Some(ReviewOptions::with_text(
        "Look again.\nBase {{base}}.\n{{facts}}\nBase again {{base}}.\n",
        None,
    ));
    let r = run(s).await;
    let sent = prompts_sent(&r.received);
    assert!(sent[1].starts_with("Look again.\nBase "), "{}", sent[1]);
    assert_eq!(sent[1].matches(BASE).count(), 2, "{}", sent[1]);
    assert!(sent[1].contains("- Changed files: 3"), "{}", sent[1]);
    assert!(!sent[1].contains("never git stash"), "{}", sent[1]);
    // Without a text the built-in default goes out.
    let r = run(Setup::new(vec![end_turn("Done."), end_turn("Fine.")])).await;
    let sent = prompts_sent(&r.received);
    assert!(sent[1].starts_with("Review this delivery as the maintainer"));
    assert_eq!(
        sent[1].lines().count(),
        render(DEFAULT_REVIEW_PROMPT, BASE, &facts_for_three())
            .lines()
            .count()
    );
}

fn facts_for_three() -> String {
    let paths = ["src/a.py", "src/b.py", "tests/test_a.py"].map(String::from);
    let top = ["src", "README.md", "docs"].map(String::from);
    facts_text(&FactsInput {
        paths: &paths,
        total: 3,
        base_top_level: Some(&top),
        untracked: Some(2),
    })
}

/// COMPAT-CAH-DATA: records in the earlier folder are records the harness reads, and the folder is no new top-level folder.
#[test]
fn records_in_the_earlier_folder_are_named_as_records_and_not_as_a_new_folder() {
    let paths = ["src/a.py", ".cah/lifecycle/LOG.md"].map(String::from);
    let top = ["src", "README.md"].map(String::from);
    let t = facts_text(&FactsInput {
        paths: &paths,
        total: 2,
        base_top_level: Some(&top),
        untracked: Some(0),
    });
    assert!(t.contains("- New top-level folders: none."), "{t}");
    assert!(
        t.contains(
            "- Files under .cah/ (records the harness reads; keep them): .cah/lifecycle/LOG.md."
        ),
        "{t}"
    );
}

// ---- the cases in which the round is skipped ---------------------------------------------------------------------------------

async fn skipped(s: Setup, why: &str) -> Ran {
    let r = run(s).await;
    assert_eq!(
        prompts_sent(&r.received).len(),
        1,
        "no extra prompt when the round is skipped ({why})"
    );
    let rec = r.end.review.as_ref().expect("the skip is recorded");
    assert_eq!(rec.status, REVIEW_SKIPPED, "{why}");
    assert_eq!(rec.reason.as_deref(), Some(why), "{rec:?}");
    assert!(rec.commit_before.is_none() && rec.seconds.is_none());
    assert!(
        review_events(&r.events)
            .iter()
            .all(|e| e["phase"] != "start")
    );
    r
}

#[tokio::test(start_paused = true)]
async fn a_run_the_user_switched_the_round_off_for_sends_nothing() {
    let mut s = Setup::new(vec![end_turn("Done.")]);
    s.review = Some(ReviewOptions::off());
    let r = skipped(s, REVIEW_SKIP_OFF).await;
    assert_eq!(label(&r.end, 3), DirectOutcome::ReportedDone);
}

#[tokio::test(start_paused = true)]
async fn the_stage_switch_beats_a_request_that_asks_for_the_round() {
    let mut s = Setup::new(vec![end_turn("Done.")]);
    s.review = Some(ReviewOptions::with_text("Look again {{facts}}", None));
    s.stage_on = false;
    skipped(s, REVIEW_SKIP_OFF).await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_ended_with_a_blocked_line_is_not_reviewed() {
    let s = Setup::new(vec![end_turn(
        "Tried the build.\nBLOCKED: the registry is down and nothing in this checkout can fix it",
    )]);
    skipped(s, REVIEW_SKIP_BLOCKED).await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_ended_with_a_question_for_the_user_is_not_reviewed() {
    let s = Setup::new(vec![end_turn(
        "I added the function. Should it round half up or half even?",
    )]);
    skipped(s, REVIEW_SKIP_QUESTION).await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_after_a_forced_stop_is_not_reviewed() {
    let mut s = Setup::new(vec![end_turn("Done.")]);
    s.hooks = vec![Arc::new(Brake)];
    let r = skipped(s, REVIEW_SKIP_FORCED_STOP).await;
    assert!(r.end.hook_stop.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_run_with_no_changed_file_is_not_reviewed() {
    let mut s = Setup::new(vec![end_turn("Looked around.")]);
    s.totals = vec![Some(0)];
    skipped(s, REVIEW_SKIP_NO_CHANGED_FILE).await;
}

#[tokio::test(start_paused = true)]
async fn a_tree_the_harness_could_not_count_is_not_reviewed_and_the_reason_says_so() {
    let mut s = Setup::new(vec![end_turn("Done.")]);
    s.totals = vec![None];
    skipped(s, REVIEW_SKIP_FILES_UNKNOWN).await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_did_not_end_with_a_message_is_not_reviewed() {
    let s = Setup::new(vec![vec![
        chunk("cut off"),
        Step::Finish("max_tokens", None),
    ]]);
    let r = skipped(s, REVIEW_SKIP_NOT_DONE).await;
    assert_eq!(
        classify(&r.end.raw).outcome,
        DirectOutcome::StoppedMaxTokens
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_with_less_than_the_guard_left_is_not_reviewed() {
    // 19 minutes of budget against a 20-minute guard.
    let mut s = Setup::new(vec![end_turn("Added percent_change.")]);
    s.timeout_s = 19 * 60;
    s.guard = Duration::from_secs(20 * 60);
    skipped(s, REVIEW_SKIP_NO_TIME).await;
}

#[tokio::test(start_paused = true)]
async fn a_guard_of_the_request_replaces_the_stage_guard() {
    // The stage guard is 20 minutes and the run has 10: the request lowers the guard to 2 minutes, and the round runs.
    let mut s = Setup::new(vec![end_turn("Done."), end_turn("Fine.")]);
    s.timeout_s = 10 * 60;
    s.review = Some(ReviewOptions::with_text("Look again {{facts}}", Some(120)));
    let r = run(s).await;
    assert_eq!(prompts_sent(&r.received).len(), 2);
    assert_eq!(r.end.review.unwrap().status, REVIEW_RAN);
}

// ---- a round that fails -------------------------------------------------------------------------------------------------------

async fn failed_round(second: Vec<Step>) -> Ran {
    let r = run(Setup::new(vec![end_turn("Added percent_change."), second])).await;
    let rec = r.end.review.as_ref().expect("a record");
    assert_eq!(rec.status, REVIEW_RAN);
    assert_eq!(rec.result.as_deref(), Some(REVIEW_FAILED), "{rec:?}");
    // The outcome is the one the turn before the round had: reported done, with that turn's message.
    assert_eq!(label(&r.end, 3), DirectOutcome::ReportedDone);
    assert_eq!(r.end.raw.message, "Added percent_change.");
    assert!(matches!(r.end.raw.watched, Watched::Done(Ok(_))));
    assert!(!r.end.raw.cancel_sent);
    assert!(r.end.raw.setup_error.is_none());
    r
}

#[tokio::test(start_paused = true)]
async fn a_round_the_provider_fails_leaves_the_outcome_it_would_have_had() {
    let r = failed_round(vec![Step::ErrorWith(-32603, "Internal error")]).await;
    assert_eq!(prompts_sent(&r.received).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_round_the_engine_dies_in_leaves_the_outcome_it_would_have_had() {
    failed_round(vec![chunk("Let me look"), Step::Die]).await;
}

#[tokio::test(start_paused = true)]
async fn a_round_that_stalls_leaves_the_outcome_it_would_have_had() {
    failed_round(vec![chunk("Let me look"), Step::Silent]).await;
}

#[tokio::test(start_paused = true)]
async fn a_round_that_ends_with_no_message_or_for_length_leaves_the_outcome_it_would_have_had() {
    failed_round(vec![Step::Finish("end_turn", None)]).await;
    failed_round(vec![chunk("cut"), Step::Finish("max_tokens", None)]).await;
    failed_round(vec![chunk("no"), Step::Finish("refusal", None)]).await;
}

#[tokio::test(start_paused = true)]
async fn a_round_that_runs_into_the_deadline_leaves_the_outcome_it_would_have_had() {
    let mut s = Setup::new(vec![
        end_turn("Added percent_change."),
        vec![chunk("hmm"), Step::Silent],
    ]);
    // 40 minutes of budget and a 20-minute guard: the round starts, and the deadline (not the silence limit) ends it.
    s.timeout_s = 40 * 60;
    s.silence_s = 100_000;
    let r = run(s).await;
    let rec = r.end.review.as_ref().unwrap();
    assert_eq!(rec.result.as_deref(), Some(REVIEW_FAILED));
    assert_eq!(label(&r.end, 3), DirectOutcome::ReportedDone);
    assert_eq!(r.end.raw.message, "Added percent_change.");
}

#[tokio::test(start_paused = true)]
async fn the_user_cancelling_during_the_round_ends_the_run_as_the_users_stop() {
    let (tx, rx) = watch::channel(false);
    let mut s = Setup::new(vec![
        end_turn("Added percent_change."),
        vec![chunk("hmm"), Step::Silent],
    ]);
    s.cancel = Some(rx);
    let received_probe = tokio::spawn(async move {
        // Give the round time to start, then cancel.
        tokio::time::sleep(Duration::from_secs(60)).await;
        tx.send(true).unwrap();
        tx
    });
    let r = run(s).await;
    let _tx = received_probe.await.unwrap();
    let rec = r.end.review.as_ref().unwrap();
    assert_eq!(rec.result.as_deref(), Some(REVIEW_CANCELLED), "{rec:?}");
    assert_eq!(classify(&r.end.raw).outcome, DirectOutcome::StoppedByUser);
}

#[tokio::test(start_paused = true)]
async fn after_a_clean_round_the_modules_are_asked_and_a_late_message_is_still_delivered() {
    let late = LateMessage::new();
    let mut s = Setup::new(vec![
        end_turn("Added percent_change."),
        end_turn("Reviewed it."),
        end_turn("Answered the late message."),
    ]);
    s.hooks = vec![late.clone()];
    let r = run(s).await;
    let sent = prompts_sent(&r.received);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[1].starts_with("Review this delivery"), "{}", sent[1]);
    assert_eq!(sent[2], "A late message from the user.");
    // The modules were asked at every turn end, the round's included; the round did not repeat after the late message.
    assert_eq!(late.turn_ends.load(Ordering::SeqCst), 3);
    assert_eq!(late.seals.load(Ordering::SeqCst), 3);
    let rec = r.end.review.unwrap();
    assert_eq!(rec.result.as_deref(), Some(REVIEW_COMPLETED));
    assert_eq!(r.end.raw.message, "Answered the late message.");
}

#[tokio::test(start_paused = true)]
async fn after_a_failed_round_no_module_is_asked_and_nothing_is_sent_into_the_session() {
    let late = LateMessage::new();
    let mut s = Setup::new(vec![
        end_turn("Added percent_change."),
        vec![chunk("Let me look"), Step::Die],
    ]);
    s.hooks = vec![late.clone()];
    let r = run(s).await;
    assert_eq!(
        prompts_sent(&r.received).len(),
        2,
        "no late message after a dead session"
    );
    // Asked at the end of the delivery turn and its seal only: the failed round is not put to the modules.
    assert_eq!(late.turn_ends.load(Ordering::SeqCst), 1);
    assert_eq!(late.seals.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.end.review.as_ref().unwrap().result.as_deref(),
        Some(REVIEW_FAILED)
    );
    assert_eq!(label(&r.end, 3), DirectOutcome::ReportedDone);
}

// ---- the pure rules ---------------------------------------------------------------------------------------------------------

fn done_raw(message: &str) -> RawEnd {
    RawEnd {
        watched: Watched::Done(Ok(agentd::direct::classify::PromptDone {
            stop_reason: "end_turn".into(),
            usage: None,
        })),
        cancel_sent: false,
        message: message.to_string(),
        setup_error: None,
    }
}

fn gate<'a>(raw: &'a RawEnd, files: Option<u32>) -> Gate<'a> {
    Gate {
        mode: ReviewMode::Auto,
        hook_stop: false,
        cancelled: false,
        raw,
        files,
        left: Duration::from_secs(3600),
        guard: Duration::from_secs(1200),
    }
}

#[test]
fn the_gate_lets_a_message_and_a_changed_file_through_and_names_the_first_reason_otherwise() {
    let ok = done_raw("Added the function.");
    assert_eq!(skip_reason(&gate(&ok, Some(1))), None);
    assert_eq!(
        skip_reason(&gate(&ok, Some(0))),
        Some(REVIEW_SKIP_NO_CHANGED_FILE)
    );
    assert_eq!(
        skip_reason(&gate(&ok, None)),
        Some(REVIEW_SKIP_FILES_UNKNOWN)
    );

    let mut off = gate(&ok, Some(1));
    off.mode = ReviewMode::Off;
    assert_eq!(skip_reason(&off), Some(REVIEW_SKIP_OFF));
    // Off wins over everything else.
    let blocked = done_raw("BLOCKED: x");
    let mut all = gate(&blocked, Some(0));
    all.mode = ReviewMode::Off;
    all.hook_stop = true;
    assert_eq!(skip_reason(&all), Some(REVIEW_SKIP_OFF));
    all.mode = ReviewMode::Auto;
    assert_eq!(skip_reason(&all), Some(REVIEW_SKIP_FORCED_STOP));
    all.hook_stop = false;
    assert_eq!(skip_reason(&all), Some(REVIEW_SKIP_BLOCKED));
    let ask = done_raw("Which one?");
    assert_eq!(
        skip_reason(&gate(&ask, Some(0))),
        Some(REVIEW_SKIP_QUESTION)
    );
    let empty = done_raw("   ");
    assert_eq!(
        skip_reason(&gate(&empty, Some(2))),
        Some(REVIEW_SKIP_NOT_DONE)
    );
    let mut late = gate(&ok, Some(2));
    late.left = Duration::from_secs(1199);
    assert_eq!(skip_reason(&late), Some(REVIEW_SKIP_NO_TIME));
    late.left = Duration::from_secs(1200);
    assert_eq!(skip_reason(&late), None, "exactly the guard is enough");
}

#[test]
fn a_cancel_stops_the_round_before_a_prompt_goes_out() {
    let ok = done_raw("Added the function.");
    let mut g = gate(&ok, Some(1));
    g.cancelled = true;
    assert_eq!(skip_reason(&g), Some(REVIEW_SKIP_CANCELLED));
    // A brake of a module still comes first; the other rules come after the cancel.
    g.hook_stop = true;
    assert_eq!(skip_reason(&g), Some(REVIEW_SKIP_FORCED_STOP));
    let blocked = done_raw("BLOCKED: x");
    let mut b = gate(&blocked, Some(1));
    b.cancelled = true;
    assert_eq!(skip_reason(&b), Some(REVIEW_SKIP_CANCELLED));
}

#[test]
fn a_blocked_line_is_found_in_any_line_with_the_marks_a_model_puts_before_it() {
    for yes in [
        "BLOCKED: no registry",
        "Tried.\nBLOCKED: no registry\nMore text after it.",
        "**BLOCKED: no registry**",
        "> BLOCKED: no registry",
        "- BLOCKED: no registry",
        "`BLOCKED: no registry`",
        "# BLOCKED: no registry",
        "**BLOCKED**: no registry",
        "__BLOCKED:__ no registry",
        "**BLOCKED:** no registry",
    ] {
        assert!(says_blocked(yes), "{yes:?}");
    }
    for no in [
        "",
        "Not blocked.",
        "blocked: lower case is not the contract's word",
        "It was BLOCKED: in the middle of a line",
        "BLOCKED without a colon",
        "Unblocked: yes",
    ] {
        assert!(!says_blocked(no), "{no:?}");
    }
}

#[test]
fn a_question_is_a_last_line_that_ends_with_a_question_mark() {
    for yes in [
        "Which one?",
        "Done with the code.\n\nShould it round half up?",
        "Should it round half up?  \n\n",
        "**Should it round half up?**",
        "(Should it round half up?)",
        "Should it round half up?\n```\n",
        "Should it round half up?\n\n---\n",
        "Should it round half up?\n\n***\n\n",
        "Is `x` right?`",
        "Which of the two do you prefer\u{ff1f}",
        "I changed the rounding. Which mode do you want?",
    ] {
        assert!(asks_the_user(yes), "{yes:?}");
    }
    for no in [
        "",
        "Done.",
        "Why? Because the test said so.",
        "Question: what now? I decided to keep going.",
        "Added the function.\nHow it works? See the docs.\nDONE",
        // A closing offer after finished work asks for nothing the delivery needs.
        "Done.\n\nLet me know if you want the docs changed too?",
        "Done.\nWould you like me to also add a changelog entry?",
        "Done.\n**Want me to run the full suite as well?**",
        "Done.\nIf you'd like, I can also rename the helper?",
    ] {
        assert!(!asks_the_user(no), "{no:?}");
    }
}

#[test]
fn the_facts_are_plain_lines_and_say_what_they_do_not_know() {
    let paths: Vec<String> = [
        "src/a.rs",
        "docs/b.md",
        "newdir/c.rs",
        "newdir/d.rs",
        "README.md",
    ]
    .map(String::from)
    .to_vec();
    let top: Vec<String> = ["src", "docs", "README.md"].map(String::from).to_vec();
    let t = facts_text(&FactsInput {
        paths: &paths,
        total: 5,
        base_top_level: Some(&top),
        untracked: Some(0),
    });
    assert_eq!(
        t,
        "- Changed files: 5 (src/a.rs, docs/b.md, newdir/c.rs, newdir/d.rs, README.md).\n\
         - New top-level folders: newdir/.\n\
         - Files under .ham/ (records the harness reads; keep them): none.\n\
         - Untracked files: 0."
    );
    let unknown = facts_text(&FactsInput {
        paths: &[],
        total: 0,
        base_top_level: None,
        untracked: None,
    });
    assert!(unknown.contains("- Changed files: 0."), "{unknown}");
    assert!(
        unknown.contains("New top-level folders: not known."),
        "{unknown}"
    );
    assert!(unknown.contains("Untracked files: not known."), "{unknown}");
}

#[test]
fn the_records_folder_is_not_a_stray_folder_and_a_tree_that_listed_no_paths_says_not_known() {
    let paths: Vec<String> = [".ham/lifecycle/LOG.md", ".ham/test-command", "src/a.rs"]
        .map(String::from)
        .to_vec();
    let top: Vec<String> = vec!["src".into()];
    let t = facts_text(&FactsInput {
        paths: &paths,
        total: 3,
        base_top_level: Some(&top),
        untracked: Some(0),
    });
    assert!(t.contains("- New top-level folders: none."), "{t}");
    assert!(
        t.contains(
            "- Files under .ham/ (records the harness reads; keep them): .ham/lifecycle/LOG.md, .ham/test-command."
        ),
        "{t}"
    );
    // Files differ but the tree listed none of them: nothing is claimed about folders or records.
    let t = facts_text(&FactsInput {
        paths: &[],
        total: 5,
        base_top_level: Some(&top),
        untracked: Some(1),
    });
    assert!(t.contains("- Changed files: 5."), "{t}");
    assert!(t.contains("- New top-level folders: not known."), "{t}");
    assert!(
        t.contains("(records the harness reads; keep them): not known."),
        "{t}"
    );
    assert!(t.contains("- Untracked files: 1."), "{t}");
}

#[test]
fn a_list_of_paths_is_cut_and_a_path_is_one_clean_line() {
    let paths: Vec<String> = (0..30).map(|i| format!("src/f{i}.rs")).collect();
    let top: Vec<String> = vec!["src".into()];
    let t = facts_text(&FactsInput {
        paths: &paths,
        total: 300,
        base_top_level: Some(&top),
        untracked: Some(1),
    });
    assert!(t.contains(", and 18 more)."), "{t}");
    assert!(
        t.contains("(from the first 30 changed paths)"),
        "a cut list says it is cut: {t}"
    );
    let nasty = vec![format!("src/a\nb\u{7}{}", "x".repeat(400))];
    let t = facts_text(&FactsInput {
        paths: &nasty,
        total: 1,
        base_top_level: Some(&top),
        untracked: None,
    });
    assert_eq!(t.lines().count(), 4, "one line per fact: {t:?}");
    assert!(t.contains("src/a?b?"), "{t}");
    assert!(t.lines().next().unwrap().len() < 260, "{t}");
}

#[test]
fn many_records_and_folders_are_listed_up_to_ten() {
    let mut paths: Vec<String> = (0..14).map(|i| format!(".ham/lifecycle/r{i}.md")).collect();
    paths.extend((0..12).map(|i| format!("new{i}/x.rs")));
    let top: Vec<String> = vec![];
    let t = facts_text(&FactsInput {
        paths: &paths,
        total: 26,
        base_top_level: Some(&top),
        untracked: Some(0),
    });
    assert!(t.contains(".ham/lifecycle/r9.md, and 4 more."), "{t}");
    assert!(t.contains("new7/, and 2 more."), "{t}");
}

#[test]
fn the_template_is_filled_in_one_pass_and_what_is_put_in_is_never_read_for_a_token() {
    assert_eq!(render("a {{base}} b {{facts}} c", "B", "F"), "a B b F c");
    assert_eq!(render("{{facts}}{{base}}{{base}}", "B", "F"), "FBB");
    assert_eq!(render("no tokens here", "B", "F"), "no tokens here");
    assert_eq!(render("", "B", "F"), "");
    // A path that looks like a token stays a path.
    assert_eq!(
        render("{{facts}} {{base}}", "B", "x {{base}} y"),
        "x {{base}} y B"
    );
    assert_eq!(render("{{base}}", "{{facts}}", "F"), "{{facts}}");
    // A token that is not whole stays.
    assert_eq!(
        render("{base} {{ base }} {{BASE}}", "B", "F"),
        "{base} {{ base }} {{BASE}}"
    );
}

#[test]
fn the_plan_lays_the_request_over_the_stage() {
    let repo: Arc<dyn ReviewRepo> = Repo::new(&[]);
    let stage = Duration::from_secs(1200);
    let p = ReviewPlan::new(None, true, stage, BASE, repo.clone());
    assert_eq!(
        (p.mode, p.guard, p.text.as_str(), p.base.as_str()),
        (ReviewMode::Auto, stage, DEFAULT_REVIEW_PROMPT, BASE)
    );
    // The run opens the tree probe only for a plan that is on (`is_on`), so a run with the round off pays nothing for it.
    assert!(p.is_on());
    let p = ReviewPlan::new(Some(&ReviewOptions::off()), true, stage, BASE, repo.clone());
    assert_eq!(p.mode, ReviewMode::Off);
    assert!(!p.is_on());
    let p = ReviewPlan::new(None, false, stage, BASE, repo.clone());
    assert_eq!(p.mode, ReviewMode::Off, "the stage switch is final");
    assert!(!p.is_on());
    let mine = ReviewOptions::with_text("Mine {{facts}}", Some(300));
    let p = ReviewPlan::new(Some(&mine), true, stage, BASE, repo.clone());
    assert_eq!(p.mode, ReviewMode::Auto);
    assert_eq!(p.text, "Mine {{facts}}");
    assert_eq!(p.guard, Duration::from_secs(300));
    let p = ReviewPlan::new(Some(&mine), false, stage, BASE, repo);
    assert_eq!(p.mode, ReviewMode::Off);
}

#[test]
fn the_member_of_the_request_is_checked_again_and_the_words_never_hold_the_value() {
    assert_eq!(check_member(None), Ok(()));
    assert_eq!(check_member(Some(&ReviewOptions::off())), Ok(()));
    let mut bad = ReviewOptions::with_text("go", Some(5));
    let e = check_member(Some(&bad)).unwrap_err();
    assert!(e.contains("guardS"), "{e}");
    assert!(!e.contains("go"), "{e}");
    bad.guard_s = None;
    bad.sha256 = Some("0".repeat(64));
    assert!(check_member(Some(&bad)).unwrap_err().contains("sha256"));
}
