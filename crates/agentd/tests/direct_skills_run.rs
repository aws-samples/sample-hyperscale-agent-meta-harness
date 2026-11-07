//! The skills module whole: the whole agent runs in process against the fake AWS and the fake ACP engine with
//! the module registered, and a request that carries skills. The acceptance rule of the skills canary is the one the live row applies after
//! a deploy: the skill's marker file is in the work tree AND the engine's `skills` tool was called for that skill. A marker alone passes
//! nothing, because a model that finds the file by searching the disk writes the marker without loading the skill.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks, StartupCtx};
use agentd::direct::skills::{Settings, Skills};
use direct_support::{Direct, Opts, PROMPT, RUN, sha};
use harness_protocol::direct::{DirectOutcome, Skill, SkillFile, s3 as keys, skill_sha256};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

const SKILL: &str = "lifecycle-canary";
const MARKER: &str = "skill-canary.txt";

fn file(path: &str, content: &str) -> SkillFile {
    SkillFile {
        path: path.into(),
        sha256: sha(content.as_bytes()),
        size: content.len() as u64,
        content: content.into(),
    }
}

/// The canary: a skill whose instructions say to create `skill-canary.txt` with a fixed line. The last line is the directive the fake
/// engine follows when its `skills` tool is called for a skill it can find (a real model follows the sentence above it).
fn canary() -> Skill {
    let md = format!(
        "---\nname: {SKILL}\ndescription: Use when asked to run the lifecycle canary.\n---\n# Lifecycle canary\n\nCreate the file {MARKER} in the repository root with the single line `ran by {SKILL}`.\n\nFAKE-ACP-WRITE {MARKER} ran by {SKILL}\n"
    );
    Skill {
        name: SKILL.into(),
        files: vec![file("SKILL.md", &md)],
    }
}

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

fn with_skills(d: &mut Direct, skills: &[Skill]) {
    with_members(d, json!({ "skills": skills }));
}

/// A module that runs before the skills module and names another session directory.
struct NameCwd(&'static str);

impl DirectHook for NameCwd {
    fn name(&self) -> &'static str {
        "name-cwd"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *ctx.session_cwd = Some(ctx.repo.join(self.0).display().to_string());
            Ok(())
        })
    }
}

/// A module that runs before the skills module and leaves a file where a repository or a person's home directory would.
struct Plant {
    repo_skill: Option<&'static str>,
    home_skill: Option<&'static str>,
}

impl DirectHook for Plant {
    fn name(&self) -> &'static str {
        "plant"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let put = |root: &Path, name: &str| {
                let dir = root.join(".agents/skills").join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: planted.\n---\nplanted body\n"),
                )
                .unwrap();
            };
            if let Some(n) = self.repo_skill {
                put(ctx.repo, n);
            }
            if let Some(n) = self.home_skill {
                put(ctx.home, n);
            }
            Ok(())
        })
    }
}

/// A module that runs before the skills module and puts a skill of its own in the skills directory of the configuration directory, as the
/// research module will place its context.
struct PlaceBeside;

impl DirectHook for PlaceBeside {
    fn name(&self) -> &'static str {
        "place-beside"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let dir = ctx.config_dir.join("skills/research-notes");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: research-notes\ndescription: Notes of the research module.\n---\nResearch notes.\n",
            )
            .unwrap();
            Ok(())
        })
    }
}

fn skills_module() -> Arc<dyn DirectHook> {
    Arc::new(Skills::new(Settings::from_lookup(|_| None)))
}

async fn run(d: &Direct, modules: Vec<Arc<dyn DirectHook>>) -> InvocationResponse {
    let deps = Deps {
        hooks: Some(Hooks::new(modules)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn tool_titles(d: &Direct) -> Vec<String> {
    d.events_of(EventType::ToolSummary)
        .iter()
        .filter_map(|e| e["title"].as_str().map(String::from))
        .collect()
}

fn skills_events(d: &Direct) -> Vec<Value> {
    d.events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|e| e["kind"] == "skills")
        .collect()
}

/// The acceptance rule of the canary: the marker is in the tree and the `skills` tool was called for the skill.
fn canary_verdict(d: &Direct) -> Result<(), String> {
    let marker = d.worktree().join(MARKER);
    if !marker.is_file() {
        return Err(format!("{MARKER} was not written"));
    }
    let called = tool_titles(d)
        .iter()
        .any(|t| t.starts_with("skills: ") && t.ends_with(SKILL));
    if called {
        Ok(())
    } else {
        Err("the marker was written without a skills tool call".to_string())
    }
}

/// The status the engine ended the `skills` tool call with (`turn.progress` carries `toolCallId` and `status` of each update).
fn skills_call_status(d: &Direct) -> Option<String> {
    let id = d.events_of(EventType::ToolSummary).into_iter().find(|e| {
        e["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("skills: "))
    })?["toolCallId"]
        .as_str()?
        .to_string();
    d.events_of(EventType::TurnProgress)
        .into_iter()
        .rev()
        .find(|e| e["toolCallId"] == id.as_str())
        .and_then(|e| e["status"].as_str().map(String::from))
}

fn opts(scenario: &'static str) -> Opts {
    Opts {
        scenario,
        ..Opts::default()
    }
}

// ---- the canary -------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_requested_skill_is_placed_listed_called_and_its_marker_comes_back() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let resp = run(&d, vec![skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);

    // The skill is where the engine reads it, read-only, outside the repository.
    let placed = d.agent_data().join("config/skills").join(SKILL);
    assert!(placed.join("SKILL.md").is_file());
    assert!(!d.worktree().join(".agents").exists());

    // The event names the skill and says the preflight ran; it holds no instruction text.
    let evs = skills_events(&d);
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0]["skills"][0]["name"], SKILL);
    assert_eq!(evs[0]["preflight"]["ran"], true);
    assert!(!evs[0].to_string().contains("Create the file"));

    // The engine found the skill where the module put it: its `skills` call completed, and the skill's own instruction wrote the marker.
    assert_eq!(skills_call_status(&d).as_deref(), Some("completed"));
    assert_eq!(
        std::fs::read_to_string(d.worktree().join(MARKER)).unwrap(),
        format!("ran by {SKILL}\n")
    );

    // summary.json lists the skill: name, digest, file count and bytes, never the text.
    let s = d.summary();
    let c = canary();
    assert_eq!(s.skills.len(), 1, "{s:?}");
    assert_eq!(s.skills[0].name, SKILL);
    assert_eq!(s.skills[0].sha256, skill_sha256(&c));
    assert_eq!(s.skills[0].files, 1);
    assert_eq!(s.skills[0].bytes, c.files[0].size);
    assert_eq!(evs[0]["skills"][0]["digest"], s.skills[0].sha256.as_str());

    // The prompt reached the engine as the user typed it.
    assert_eq!(d.prompts_received(), [PROMPT]);

    // The session starts in the git top level of the work tree.
    let want = d.worktree().canonicalize().unwrap().display().to_string();
    let new_session = d
        .fake_engine_log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["method"] == "session/new")
        .unwrap();
    assert_eq!(new_session["params"]["cwd"], want.as_str());

    // TRIGGER PRESENT: the skill was requested, the preflight listed it, and the engine's tool was called for it.
    canary_verdict(&d).unwrap();
    println!(
        "SKILLS-CANARY TRIGGER PRESENT skill={SKILL} preflight=ran tool-call=yes marker={MARKER} cwd=top-level"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_module_the_engine_finds_no_skill_and_the_canary_fails() {
    // The trigger of the canary: the same request, the same scenario, but nothing placed the skill, so the engine's `skills` call
    // fails and no marker is written. The canary is therefore sensitive to the module doing its work.
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let resp = run(&d, vec![]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(skills_call_status(&d).as_deref(), Some("failed"));
    assert!(!d.worktree().join(MARKER).exists());
    let e = canary_verdict(&d).unwrap_err();
    assert!(e.contains("was not written"), "{e}");
    assert!(d.summary().skills.is_empty());
    println!("SKILLS-CANARY NEGATIVE PRESENT module=off skills-call=failed marker=no verdict=fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_products_own_module_set_places_the_skills_too() {
    // No `Deps.hooks`: the driver registers `Hooks::standard`, the modules of the product, and the skills module is among them.
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &Deps::default()).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    canary_verdict(&d).unwrap();
    assert_eq!(d.summary().skills.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_directory_an_earlier_module_named_is_kept() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let first: Arc<dyn DirectHook> = Arc::new(NameCwd("src"));
    let resp = run(&d, vec![first, skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let want = d.worktree().join("src").display().to_string();
    let new_session = d
        .fake_engine_log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["method"] == "session/new")
        .unwrap();
    assert_eq!(new_session["params"]["cwd"], want.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_marker_without_a_skills_call_does_not_pass_the_acceptance_rule() {
    let mut d = Direct::start(opts("skills_marker_no_call")).await;
    with_skills(&mut d, &[canary()]);
    let resp = run(&d, vec![skills_module()]).await;
    // The run itself is fine: the harness does not judge the work (invariant 1).
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.worktree().join(MARKER).is_file(),
        "the trigger: a marker without a call"
    );
    let e = canary_verdict(&d).unwrap_err();
    assert!(e.contains("without a skills tool call"), "{e}");
    println!("SKILLS-CANARY NEGATIVE PRESENT marker=yes tool-call=no verdict=fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_without_skills_places_nothing_and_posts_no_skills_event() {
    let d = Direct::start(Opts::default()).await;
    let resp = run(&d, vec![skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(!d.agent_data().join("config/skills").exists());
    assert!(skills_events(&d).is_empty());
}

// ---- what stops a run before any model call ------------------------------------------------------------------------------------------

async fn stopped_before_the_model(d: &Direct, resp: &InvocationResponse) -> String {
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    assert!(
        d.prompts_received().is_empty(),
        "the engine never got a prompt"
    );
    s.detail.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_with_the_name_of_a_repository_skill_stops_the_run_and_names_the_file() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let plant: Arc<dyn DirectHook> = Arc::new(Plant {
        repo_skill: Some(SKILL),
        home_skill: None,
    });
    let resp = run(&d, vec![plant, skills_module()]).await;
    let detail = stopped_before_the_model(&d, &resp).await;
    assert!(
        detail.contains(SKILL)
            && detail.contains("already there")
            && detail.contains(".agents/skills/lifecycle-canary/SKILL.md"),
        "{detail}"
    );
    assert!(
        !d.agent_data().join("config/skills").exists(),
        "nothing was placed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_in_the_home_directory_is_removed_and_the_run_goes_on() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let plant: Arc<dyn DirectHook> = Arc::new(Plant {
        repo_skill: None,
        home_skill: Some("homeskill"),
    });
    let resp = run(&d, vec![plant, skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(!d.agent_data().join("home/.agents/skills").exists());
    canary_verdict(&d).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_skill_with_another_name_is_left_alone() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let plant: Arc<dyn DirectHook> = Arc::new(Plant {
        repo_skill: Some("theirs"),
        home_skill: None,
    });
    let resp = run(&d, vec![plant, skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.worktree()
            .join(".agents/skills/theirs/SKILL.md")
            .is_file()
    );
    canary_verdict(&d).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_listing_that_misses_changes_or_adds_a_skill_stops_the_run() {
    for (mode, want) in [
        ("missing", "missing from the skills the engine will load"),
        ("changed", "differs from the file placed"),
        ("extra", "outside the repository"),
        ("fail", "skills preflight"),
        ("garbage", "skills listing"),
    ] {
        let mut o = opts("skills_canary");
        o.engine_env = vec![("FAKE_ACP_SKILLS_MODE", mode.to_string())];
        let mut d = Direct::start(o).await;
        with_skills(&mut d, &[canary()]);
        let resp = run(&d, vec![skills_module()]).await;
        let detail = stopped_before_the_model(&d, &resp).await;
        assert!(detail.contains(want), "{mode}: {detail}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn skills_the_shared_validator_refuses_end_the_run_as_did_not_start() {
    let mut d = Direct::start(opts("skills_canary")).await;
    // A skill with no SKILL.md: `harness_protocol::direct::check_skills` names it.
    let broken = Skill {
        name: "no-entry".into(),
        files: vec![file("notes.md", "notes\n")],
    };
    with_skills(&mut d, &[broken]);
    let resp = run(&d, vec![skills_module()]).await;
    let detail = stopped_before_the_model(&d, &resp).await;
    assert!(
        detail.contains("no-entry") && detail.contains("SKILL.md"),
        "{detail}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_whose_document_names_another_folder_ends_the_run_as_did_not_start() {
    let mut d = Direct::start(opts("skills_canary")).await;
    let md = "---\nname: other-name\ndescription: x.\n---\nBody.\n";
    let s = Skill {
        name: "folder-name".into(),
        files: vec![file("SKILL.md", md)],
    };
    with_skills(&mut d, &[s]);
    let resp = run(&d, vec![skills_module()]).await;
    let detail = stopped_before_the_model(&d, &resp).await;
    assert!(detail.contains("frontmatter name"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_that_does_not_finish_in_time_stops_the_run_instead_of_hanging_it() {
    let mut o = opts("skills_canary");
    o.engine_env = vec![("FAKE_ACP_SKILLS_MODE", "hang".to_string())];
    let mut d = Direct::start(o).await;
    with_skills(&mut d, &[canary()]);
    let limited: Arc<dyn DirectHook> = Arc::new(Skills::new(Settings::from_lookup(|k| {
        (k == "AGENTD_SKILLS_LISTING_TIMEOUT_S").then(|| "1".to_string())
    })));
    let started = std::time::Instant::now();
    let resp = run(&d, vec![limited]).await;
    let detail = stopped_before_the_model(&d, &resp).await;
    assert!(detail.contains("did not finish in 1 s"), "{detail}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the run waited for the engine's listing: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_with_no_skills_listing_gets_its_skills_and_the_event_says_the_preflight_did_not_run()
 {
    let mut o = opts("skills_canary");
    o.engine = "fake-acp";
    let mut d = Direct::start(o).await;
    with_skills(&mut d, &[canary()]);
    let resp = run(&d, vec![skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.agent_data()
            .join("config/skills")
            .join(SKILL)
            .join("SKILL.md")
            .is_file()
    );
    let evs = skills_events(&d);
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0]["preflight"]["ran"], false);
    assert_eq!(d.summary().skills.len(), 1);
}

// ---- the switch ------------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn with_the_module_switched_off_a_request_with_skills_is_refused_not_ignored() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let off: Arc<dyn DirectHook> = Arc::new(Skills::new(Settings::from_lookup(|k| {
        (k == "AGENTD_SKILLS_ENABLED").then(|| "off".to_string())
    })));
    let resp = run(&d, vec![off]).await;
    let detail = stopped_before_the_model(&d, &resp).await;
    assert!(detail.contains("switched off"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_module_switched_off_a_request_without_skills_runs_as_it_did() {
    let d = Direct::start(Opts::default()).await;
    let off: Arc<dyn DirectHook> = Arc::new(Skills::new(Settings::from_lookup(|k| {
        (k == "AGENTD_SKILLS_ENABLED").then(|| "off".to_string())
    })));
    let resp = run(&d, vec![off]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
}

// ---- a second invocation of the same run on the same VM -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_second_invocation_of_the_same_run_places_the_skills_again_and_runs() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let first = run(&d, vec![skills_module()]).await;
    assert_eq!(first.outcome, TurnOutcome::Completed, "{first:?}");
    canary_verdict(&d).unwrap();
    // TRIGGER PRESENT: the first attempt's tree is still where the engine reads it, locked, when the second invocation of the run starts.
    let placed = d.agent_data().join("config/skills").join(SKILL);
    assert!(
        placed.join("SKILL.md").is_file(),
        "the first attempt's skill is on disk"
    );
    assert!(
        placed.join(".agentd-placed").is_file(),
        "and it is marked as agentd's"
    );

    // The same invocation again on the same VM (the worktree, the data directory and the configuration directory are the first attempt's).
    let second = run(&d, vec![skills_module()]).await;
    // It started: the skills were placed again (a second `skills` event) and the engine ran (a run the module refuses ends before any
    // prompt, with no stop reason). Its end is the end sequence's conflict: the first attempt's bundle and summary.json are already
    // in the bucket, so agentd records the conflict and writes the second attempt's own objects over them.
    assert_eq!(
        second.stop_reason.as_deref(),
        Some("end_turn"),
        "{second:?}"
    );
    assert_eq!(second.outcome, TurnOutcome::Failed, "{second:?}");
    assert!(
        second
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("already existed"),
        "{second:?}"
    );
    assert_eq!(skills_events(&d).len(), 2, "{:?}", skills_events(&d));
    assert_ne!(d.summary().outcome, DirectOutcome::StoppedSetupFailed);
    assert_eq!(d.summary().skills.len(), 1);
    assert_eq!(skills_call_status(&d).as_deref(), Some("completed"));
    println!(
        "SKILLS-RETRY TRIGGER PRESENT first-attempt-tree=on-disk second-invocation=started skills=1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_another_module_placed_beside_the_run_skills_survives_and_does_not_stop_the_run() {
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[canary()]);
    let beside: Arc<dyn DirectHook> = Arc::new(PlaceBeside);
    let resp = run(&d, vec![beside, skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    canary_verdict(&d).unwrap();
    let other = d.agent_data().join("config/skills/research-notes/SKILL.md");
    assert_eq!(
        std::fs::read_to_string(other).unwrap(),
        "---\nname: research-notes\ndescription: Notes of the research module.\n---\nResearch notes.\n",
        "the other module's skill is as it was written"
    );
    // The summary and the event list the run's own skills only.
    assert_eq!(d.summary().skills.len(), 1);
    assert_eq!(skills_events(&d)[0]["skills"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skill_with_the_description_on_the_next_line_is_placed_listed_and_called() {
    // Valid YAML that agentd's hand-written reader used to refuse after provisioning ("has no description").
    let md = format!(
        "---\nname: {SKILL}\ndescription:\n  Use when asked to run the lifecycle canary.\n---\n# Lifecycle canary\n\nCreate the file {MARKER} in the repository root with the single line `ran by {SKILL}`.\n\nFAKE-ACP-WRITE {MARKER} ran by {SKILL}\n"
    );
    let s = Skill {
        name: SKILL.into(),
        files: vec![file("SKILL.md", &md)],
    };
    let mut d = Direct::start(opts("skills_canary")).await;
    with_skills(&mut d, &[s]);
    let resp = run(&d, vec![skills_module()]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    canary_verdict(&d).unwrap();
}
