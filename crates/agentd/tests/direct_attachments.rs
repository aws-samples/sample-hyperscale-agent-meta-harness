// COMPAT-CAH-DATA: this file names the earlier variable spellings that the engine environment carries beside the new ones until VERSION 6.00.
//! The attachments module whole: a start request that carries attachments, the whole agent
//! in process against the fake AWS and the fake ACP engine, and the module registered. The user's own files are placed read-only in
//! `<agent data>/context/attachments/`, outside the work tree (so nothing in them reaches a snapshot, the zip or a pull request), with an index
//! of name, hash and size; the engine learns of them from `HAMH_ATTACHMENTS_DIR` and the index, and the prompt reaches it as typed.
//!
//! A run with no attachments is the run it was: no directory, no variable, no event, no log line. A set the shared validator refuses stops the run
//! before the first model call ("did not start"), with words that name the item and never its content.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::attachments::{AttachmentsHook, place};
use agentd::direct::context::ContextDir;
use agentd::direct::hooks::{DirectHook, Hooks};
use direct_support::{Direct, Opts, PROMPT, RUN, sha};
use harness_protocol::direct::{Attachment, DirectOutcome, s3 as keys};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NOTES: &str = "The API takes a page token.\nSecond line.\n";
const DATA: &str = "a,b\n1,2\n";

/// Puts members into the request the run fetches, and the hash the invocation carries.
fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

fn items() -> Vec<Attachment> {
    vec![
        Attachment::new("notes.md", NOTES),
        Attachment::new("data.csv", DATA),
    ]
}

async fn run(d: &Direct) -> InvocationResponse {
    let hook: Arc<dyn DirectHook> = Arc::new(AttachmentsHook::new());
    let deps = Deps {
        hooks: Some(Hooks::new(vec![hook])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn env_of(d: &Direct, name: &str) -> Option<String> {
    d.engine_probe()
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

fn dir_of(d: &Direct) -> PathBuf {
    d.agent_data().join("context/attachments")
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn events(d: &Direct) -> Vec<Value> {
    d.aws
        .progress("attachments")
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_are_placed_read_only_outside_the_work_tree_with_an_index() {
    let mut d = Direct::start(Opts::default()).await;
    with_members(&mut d, json!({"attachments": items()}));
    let resp = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    let dir = dir_of(&d);
    // The exact bytes, the engine can read them and nothing can write them.
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.md")).unwrap(),
        NOTES
    );
    assert_eq!(std::fs::read_to_string(dir.join("data.csv")).unwrap(), DATA);
    assert_eq!(mode(&dir.join("notes.md")), 0o444);
    assert_eq!(mode(&dir.join("data.csv")), 0o444);
    assert_eq!(mode(&dir.join("INDEX.json")), 0o444);
    assert_eq!(mode(&dir.join("INDEX.md")), 0o444);
    assert_eq!(mode(&dir), 0o755);
    // Outside the work tree: nothing of it can reach a snapshot, the zip or a pull request.
    assert!(!dir.starts_with(d.worktree()));
    assert!(!d.worktree().join("notes.md").exists());
    assert!(!d.worktree().join("context").exists());
    // The index: one row per file, the hash the client computed, the path the engine reads.
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("INDEX.json")).unwrap()).unwrap();
    assert_eq!(index["schema"], "hamh-attachments-index/1");
    assert_eq!(index["run"], RUN);
    let rows = index["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "notes.md");
    assert_eq!(rows[0]["sha256"], sha(NOTES.as_bytes()));
    assert_eq!(rows[0]["bytes"], NOTES.len());
    assert_eq!(rows[0]["path"], dir.join("notes.md").display().to_string());
    assert_eq!(rows[1]["name"], "data.csv");
    // The table in the columns of the lifecycle pack's context index (Origin, Date read, sha256, Licence, Why it matters).
    let md = std::fs::read_to_string(dir.join("INDEX.md")).unwrap();
    let mut lines = md.lines();
    assert_eq!(
        lines.next().unwrap(),
        "| Origin | Date read | sha256 | Licence | Why it matters |"
    );
    let row = md.lines().find(|l| l.contains("notes.md")).unwrap();
    assert!(
        row.contains(&sha(NOTES.as_bytes())) && row.contains("attached by you"),
        "{row}"
    );
    // What the engine is told and what the user's prompt is.
    assert_eq!(
        env_of(&d, "HAMH_ATTACHMENTS_DIR").unwrap(),
        dir.display().to_string()
    );
    // COMPAT-CAH-DATA: rules the user edited before the rename still name the earlier variable: the engine gets both spellings.
    assert_eq!(
        env_of(&d, "CAH_ATTACHMENTS_DIR"),
        env_of(&d, "HAMH_ATTACHMENTS_DIR")
    ); // COMPAT-CAH-DATA
    assert_eq!(d.prompts_received(), [PROMPT.to_string()]);
    println!(
        "ATTACHMENTS TRIGGER PRESENT: 2 files placed 0444 outside the work tree, index rows=2, HAMH_ATTACHMENTS_DIR set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_progress_event_names_the_files_and_holds_no_text() {
    let mut d = Direct::start(Opts::default()).await;
    with_members(&mut d, json!({"attachments": items()}));
    run(&d).await;
    let ev = events(&d);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0]["kind"], "attachments");
    assert_eq!(ev[0]["count"], 2);
    assert_eq!(ev[0]["bytes"], NOTES.len() + DATA.len());
    assert_eq!(ev[0]["names"], json!(["notes.md", "data.csv"]));
    let text = ev[0].to_string();
    assert!(
        !text.contains("page token") && !text.contains("a,b"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_attachments_places_nothing_and_says_nothing() {
    let d = Direct::start(Opts::default()).await;
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(!dir_of(&d).exists());
    assert!(env_of(&d, "HAMH_ATTACHMENTS_DIR").is_none());
    assert!(env_of(&d, "CAH_ATTACHMENTS_DIR").is_none()); // COMPAT-CAH-DATA
    assert!(events(&d).is_empty());
    assert!(
        d.log.lines().iter().all(|l| !l.contains("attachments")),
        "{:?}",
        d.log.lines()
    );
    println!("ATTACHMENTS NEGATIVE PRESENT: no member -> no directory, no variable, no event");
}

#[tokio::test(flavor = "multi_thread")]
async fn research_off_does_not_stop_the_user_s_own_files() {
    let mut d = Direct::start(Opts::default()).await;
    with_members(
        &mut d,
        json!({"attachments": items(), "research": {"enabled": false}}),
    );
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(dir_of(&d).join("notes.md").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_products_own_module_set_places_them_too() {
    // No `Deps.hooks`: the driver registers `Hooks::standard`, the modules of the product, and the attachments module is among them.
    let mut d = Direct::start(Opts::default()).await;
    with_members(&mut d, json!({"attachments": items()}));
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &Deps::default()).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        std::fs::read_to_string(dir_of(&d).join("notes.md")).unwrap(),
        NOTES
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_set_the_validator_refuses_stops_the_run_before_any_model_call_and_names_the_item_not_its_text()
 {
    let mut bad = Attachment::new("notes.md", NOTES);
    bad.size += 3;
    let mut d = Direct::start(Opts::default()).await;
    with_members(&mut d, json!({"attachments": [bad]}));
    let resp = run(&d).await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        d.prompts_received().is_empty(),
        "the engine never got a prompt"
    );
    let detail = s.detail.unwrap();
    assert!(
        detail.contains("notes.md") && detail.contains("size"),
        "{detail}"
    );
    assert!(!detail.contains("page token"), "{detail}");
    assert!(!dir_of(&d).join("notes.md").exists());
}

// ---- placing, without a run ------------------------------------------------------------------------------------------------------------

fn context(tag: &str) -> (ContextDir, PathBuf) {
    let root = std::env::temp_dir().join(format!("attach-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    (ContextDir::create(&root).unwrap(), root)
}

#[test]
fn placing_twice_gives_the_same_files_and_repairs_a_changed_one() {
    let (ctx, _root) = context("twice");
    let first = place::place(&ctx, RUN, &items(), "2026-10-16T22:00:00Z").unwrap();
    assert_eq!(first.items.len(), 2);
    // A second invocation of the same run on the same VM finds its own earlier placement and replaces it with the same bytes.
    let dir = ctx.path().join("attachments");
    let mut perms = std::fs::metadata(dir.join("notes.md"))
        .unwrap()
        .permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(dir.join("notes.md"), perms).unwrap();
    std::fs::write(dir.join("notes.md"), "tampered").unwrap();
    let again = place::place(&ctx, RUN, &items(), "2026-10-16T22:05:00Z").unwrap();
    assert_eq!(again.items.len(), 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.md")).unwrap(),
        NOTES
    );
    assert_eq!(mode(&dir.join("notes.md")), 0o444);
}

#[test]
fn a_directory_agentd_did_not_make_is_never_removed_or_written_into() {
    let (ctx, _root) = context("foreign");
    let dir = ctx.path().join("attachments");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("someone-elses.txt"), "keep me").unwrap();
    let e = place::place(&ctx, RUN, &items(), "2026-10-16T22:00:00Z").unwrap_err();
    assert!(e.contains("already there"), "{e}");
    assert_eq!(
        std::fs::read_to_string(dir.join("someone-elses.txt")).unwrap(),
        "keep me"
    );
    assert!(!dir.join("notes.md").exists());
}

#[test]
fn a_link_where_the_directory_belongs_is_refused_and_not_followed() {
    let (ctx, root) = context("link");
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, ctx.path().join("attachments")).unwrap();
    let e = place::place(&ctx, RUN, &items(), "2026-10-16T22:00:00Z").unwrap_err();
    assert!(e.contains("not a directory") || e.contains("link"), "{e}");
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing was written through the link"
    );
}

#[test]
fn a_name_that_is_not_plain_is_refused_even_if_the_caller_skipped_the_validator() {
    let (ctx, _root) = context("name");
    let mut evil = Attachment::new("../escape.md", "x");
    evil.name = "../escape.md".into();
    let e = place::place(&ctx, RUN, &[evil], "2026-10-16T22:00:00Z").unwrap_err();
    assert!(e.contains("name"), "{e}");
    assert!(!ctx.path().join("escape.md").exists());
}

#[test]
fn a_name_that_collides_with_the_index_is_refused_even_if_the_caller_skipped_the_validator() {
    let (ctx, _root) = context("reserved");
    let e = place::place(
        &ctx,
        RUN,
        &[Attachment::new("INDEX.md", "x")],
        "2026-10-16T22:00:00Z",
    )
    .unwrap_err();
    assert!(e.contains("name"), "{e}");
    assert!(!ctx.path().join("attachments").exists(), "nothing was made");
}
