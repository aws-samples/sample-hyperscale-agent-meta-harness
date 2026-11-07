// COMPAT-CAH-DATA: this file names the earlier variable spellings that the engine environment carries beside the new ones until VERSION 6.00.
//! The tool hook in whole runs: the real driver, the fake AWS, the fake engine with its MCP client
//! (`crates/ham-e2e/src/fake_mcp.rs`) and the echo backend behind agentd's real tool server. A transcript tool call `mcp: ham/code_search {..}`
//! makes the fake engine read the registration file the hook wrote, connect to the server it names and call it, the way the pinned engine does
//! at its first prompt; `mcp-connect: none|handshake-only|full` picks how far the connect goes and `mcp-settings: edit|delete` makes the engine
//! damage its file after it connected. What the tests read: the engine's own log (what it did), the summary (what the module counted) and the
//! disk (what is left).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod hook_support;
mod tools_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks, StartupCtx};
use agentd::direct::tools::ToolsHook;
use agentd::direct::tools::connect::Connect;
use agentd::direct::tools::settings::Settings;
use direct_support::{Answer, Direct, Opts};
use harness_protocol::direct::DirectOutcome;
use harness_protocol::runtime::TurnOutcome;
use hook_support::FILE;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tools_support::echo::Echo;
use tools_support::refuses;

fn any_port() -> Settings {
    Settings {
        port: 0,
        ..Settings::default()
    }
}

/// The module with the echo backend and a short connect window, as the product's list would hold it with a tool behind it.
fn hook(window: Duration) -> Arc<ToolsHook> {
    Arc::new(
        ToolsHook::with_factory(any_port(), |_, _| vec![Echo::new().0]).with_connect_window(window),
    )
}

async fn run_with(
    d: &Direct,
    hooks: Vec<Arc<dyn DirectHook>>,
) -> harness_protocol::runtime::InvocationResponse {
    let deps = Deps {
        hooks: Some(Hooks::new(hooks)),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn count(text: &str, needle: &str) -> usize {
    text.lines().filter(|l| l.contains(needle)).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_reaches_the_server_and_completes_the_tool_update() {
    let d = Direct::start(Opts {
        scenario: "mcp_call",
        mcp: true,
        ..Opts::default()
    })
    .await;
    let h = hook(Duration::from_secs(30));
    let resp = run_with(&d, vec![h.clone() as Arc<dyn DirectHook>]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The scenario edits nothing, so the run ends as a plain run that changed no file; the outcome is not what this test is about.
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedNoChanges);

    // The trigger: the engine connected and called through the registration file the hook wrote, in this run.
    let engine = d.fake_engine_log();
    let connected = count(&engine, "mcp connect full url=http://127.0.0.1:");
    let called = count(&engine, "mcp call code_search completed");
    assert!(
        connected == 1 && called == 1,
        "TRIGGER ABSENT mcp-call: connect lines {connected}, call lines {called}\n{engine}"
    );
    println!("TRIGGER PRESENT mcp-call");
    // The update of the call reached the client completed, with the server's text.
    let update = engine
        .lines()
        .filter_map(|l| l.strip_prefix("out "))
        .filter_map(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        .find(|v| {
            v.pointer("/params/update/toolCallId") == Some(&serde_json::json!("mcp-call-1"))
                && v.pointer("/params/update/sessionUpdate")
                    == Some(&serde_json::json!("tool_call_update"))
        })
        .expect("the update of the call");
    assert_eq!(
        update.pointer("/params/update/status"),
        Some(&serde_json::json!("completed"))
    );
    assert_eq!(
        update.pointer("/params/update/content/0/content/text"),
        Some(&serde_json::json!("echo: hello from the fake engine"))
    );

    // The startup order: the variable was in the engine's environment at launch, naming the port the module bound.
    let port = h.port().expect("the module bound a port");
    assert!(
        d.engine_probe()
            .contains(&format!("HAMH_TOOLS_URL=http://127.0.0.1:{port}/mcp")),
        "{}",
        d.engine_probe()
    );
    // COMPAT-CAH-DATA: rules the user edited before the rename still name the earlier variable: the engine gets both spellings.
    assert!(
        d.engine_probe()
            .contains(&format!("CAH_TOOLS_URL=http://127.0.0.1:{port}/mcp")), // COMPAT-CAH-DATA
        "{}",
        d.engine_probe()
    );
    // The summary: the module's counts, validated inside a whole summary and stored with the run.
    let note = d.summary().tools.expect("summary.tools");
    assert!(note.enabled && note.reason.is_none(), "{note:?}");
    assert_eq!(note.port, Some(port));
    assert_eq!(note.list, ["code_search"]);
    assert_eq!(
        note.counts.as_ref().unwrap().calls.get("code_search"),
        Some(&1)
    );
    assert_eq!(
        note.connect,
        Some("ok".to_string()),
        "the engine listed the tools; a run shorter than the window is judged at its end"
    );
    // The shutdown: the server is gone, the file is still the hook's.
    assert!(refuses(format!("127.0.0.1:{port}").parse().unwrap()).await);
    let lines = d.log.lines();
    assert!(
        lines.iter().any(|l| l.starts_with("direct tools")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("echo:")),
        "a log line holds the text of a result: {lines:?}"
    );
    // The engine's tool call is one of the tool calls the driver counts (it is an ordinary tool call to it).
    assert!(
        d.aws
            .events()
            .iter()
            .any(|(_, v)| v.to_string().contains("mcp: ham/code_search"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_connect_word_of_a_whole_run_follows_how_far_the_engine_went() {
    // The prompt stays open for 2 s (the fake engine's silence) and the window is 300 ms: the verdict is made inside the run, by the clock.
    for (scenario, want, tool_call) in [
        ("mcp_connect_none", Some(Connect::None), "not-connected"),
        (
            "mcp_connect_handshake_only",
            Some(Connect::HandshakeOnly),
            "not-connected",
        ),
        ("mcp_connect_full", Some(Connect::Ok), "completed"),
    ] {
        let d = Direct::start(Opts {
            scenario,
            mcp: true,
            engine_env: vec![("FAKE_ACP_SILENCE_S", "2".to_string())],
            timeout_s: 60,
            ..Opts::default()
        })
        .await;
        let h = hook(Duration::from_millis(300));
        let resp = run_with(&d, vec![h.clone() as Arc<dyn DirectHook>]).await;
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{scenario}: {resp:?}");
        let engine = d.fake_engine_log();
        assert_eq!(
            count(&engine, &format!("mcp call code_search {tool_call}")),
            1,
            "{scenario}: {engine}"
        );
        let note = d.summary().tools.expect("summary.tools");
        assert_eq!(
            note.connect.as_deref(),
            want.map(|c| c.as_str()),
            "{scenario}: {note:?}"
        );
        // The class is the module's reading of what its server saw, not of what the engine says: the server's own counts agree.
        let calls = note
            .counts
            .unwrap()
            .calls
            .get("code_search")
            .copied()
            .unwrap_or(0);
        assert_eq!(calls, u64::from(tool_call == "completed"), "{scenario}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_session_connects_again_through_a_file_the_engine_damaged() {
    // Session 1 connects, then rewrites its registration file (as the engine does after every connect). The user asks for a fresh session:
    // the old session ends, the engine's process group is killed, a new engine starts, and its first prompt reads the file. The hook must have
    // put the file back by then, or the second session connects to nothing.
    let d = Direct::start(Opts {
        scenario: "mcp_stuck_then_fresh",
        mcp: true,
        engine_env: vec![
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "mcp_fresh_done".into()),
            ("FAKE_ACP_RESTART_SCENARIO", "mcp_fresh_done".into()),
        ],
        input_wait_s: Some(30),
        edit_cfg: Some(Box::new(|c: &mut agentd::config::Config| {
            c.direct.provider_retries = 3;
            c.direct.engine_stop_resumes = 1;
            c.direct.provider_retry_pause = Duration::from_millis(20);
            c.direct.failover_wait = Duration::from_millis(100);
            c.direct.failover_wait_cap = Duration::from_millis(200);
            c.direct.failover_probe = false;
            c.direct.input_margin = Duration::ZERO;
            c.direct.input_min_window = Duration::from_secs(1);
        })),
        ..Opts::default()
    })
    .await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(harness_protocol::direct::InstructAction::NewSessionWithPrompt)
            .text("Poll the log file instead."),
    );
    let h = hook(Duration::from_millis(300));
    let resp = run_with(&d, vec![h.clone() as Arc<dyn DirectHook>]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let engine = d.fake_engine_log();
    // The log of the fake engine is one file for both processes (it appends).
    assert_eq!(count(&engine, "mcp settings edited"), 1, "{engine}");
    assert_eq!(
        count(&engine, "mcp initialize 200"),
        2,
        "two sessions, two handshakes:\n{engine}"
    );
    assert_eq!(count(&engine, "mcp tools/list 200 tools=1"), 2, "{engine}");
    assert_eq!(
        count(&engine, "mcp call code_search completed"),
        2,
        "{engine}"
    );
    assert_eq!(count(&engine, "unreadable-settings"), 0, "{engine}");
    let note = d.summary().tools.unwrap();
    assert_eq!(note.counts.unwrap().calls.get("code_search"), Some(&2));
    assert_eq!(note.connect.as_deref(), Some("ok"));
    // At the end of the run the file is the hook's again, and nothing of the damage is left.
    let file = d.agent_data().join("data/settings").join(FILE);
    let text = std::fs::read_to_string(file).unwrap();
    assert!(
        text.contains("streamableHttp") && text.contains("\"ham\""),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_the_engine_deleted_is_back_at_the_end_of_the_run() {
    let d = Direct::start(Opts {
        scenario: "mcp_delete_settings",
        mcp: true,
        ..Opts::default()
    })
    .await;
    let h = hook(Duration::from_secs(30));
    let resp = run_with(&d, vec![h.clone() as Arc<dyn DirectHook>]).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let engine = d.fake_engine_log();
    assert_eq!(
        count(&engine, "mcp settings deleted"),
        1,
        "TRIGGER: the engine deleted its file\n{engine}"
    );
    let file = d.agent_data().join("data/settings").join(FILE);
    assert!(file.exists(), "the file is back");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("registration restored")),
        "{:?}",
        d.log.lines()
    );
}

/// A module that refuses the run in its startup, placed after the tool hook, and remembers what the tool hook had put in the environment.
struct Refuses {
    seen: Mutex<Option<String>>,
}

impl DirectHook for Refuses {
    fn name(&self) -> &'static str {
        "refuses"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            *self.seen.lock().unwrap() = ctx.engine_env.get("HAMH_TOOLS_URL").cloned();
            Err("a later module refuses the run".to_string())
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_after_the_tool_hook_that_refuses_the_run_leaves_no_server_behind() {
    // The product's list puts the tool hook before attachments and engine_config, so their refusal of a run (a settings read-back that does
    // not match) comes after the port is bound and the file is written; `on_shutdown` runs for every ending, and the server goes with it.
    let d = Direct::start(Opts {
        mcp: true,
        scenario: "mcp_call",
        ..Opts::default()
    })
    .await;
    let h = hook(Duration::from_secs(30));
    let later = Arc::new(Refuses {
        seen: Mutex::new(None),
    });
    let resp = run_with(
        &d,
        vec![
            h.clone() as Arc<dyn DirectHook>,
            later.clone() as Arc<dyn DirectHook>,
        ],
    )
    .await;
    assert_ne!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let url = later
        .seen
        .lock()
        .unwrap()
        .clone()
        .expect("the tool hook ran first");
    let addr: std::net::SocketAddr = url
        .strip_prefix("http://")
        .and_then(|u| u.strip_suffix("/mcp"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        refuses(addr).await,
        "the server is stopped when the run ends before it started"
    );
    // No engine ran, so no tool call was made; the note says the module started and counted nothing.
    let note = d
        .summary()
        .tools
        .expect("a failed start still tells what the module did");
    assert!(note.enabled);
    assert_eq!(note.counts.unwrap().calls.len(), 0);
}
