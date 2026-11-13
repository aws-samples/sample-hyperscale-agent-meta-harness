//! The fake ACP engine's permission exchange through both drivers.
//! The second recorded turn of `fixtures/engines/fake-acp` has
//! the engine ask permission for `pip install requests`, wait for the client's answer and make a
//! further tool call in the same prompt. The in-process `Agentd` driver and `ProcessEngine` must
//! answer that request themselves (a rejection), or an E2E flow would wait for it forever.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_e2e::acp::{Agentd, ProcessEngine, ReplayEngine, Script};
use harness_core::engine_profile::{fixtures_dir, load_profile, resolve_launch};
use harness_protocol::common::Transport;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FIRST: &str = "Add a percent_change(a, b) function with tests";
const SECOND: &str = "Install the requests package";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let d = base.join(format!("fake-permission-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("ws/src")).unwrap();
    std::fs::create_dir_all(d.join("ws/tests")).unwrap();
    std::fs::write(
        d.join("ws/src/calc.py"),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();
    d
}

#[test]
fn the_in_process_driver_answers_an_agent_permission_request_with_a_rejection() {
    let dir = fixtures_dir(&repo(), "fake-acp").join("transcripts");
    let mut engine = ReplayEngine::new(Script::load_dir(&dir).unwrap()).with_workspace("/work/w");
    let mut agentd = Agentd::default();
    agentd.handshake(&mut engine, "/work/w").unwrap();
    let one = agentd.prompt(&mut engine, FIRST).unwrap();
    assert!(one.permissions.is_empty(), "turn one asks nothing: {one:?}");
    let two = agentd.prompt(&mut engine, SECOND).unwrap();
    assert_eq!(two.stop_reason, "end_turn");
    assert_eq!(two.permissions.len(), 1, "{two:?}");
    let x = &two.permissions[0];
    assert_eq!(
        x.request["toolCall"]["rawInput"]["commands"][0],
        "pip install requests"
    );
    assert_eq!(
        x.answer["outcome"],
        json!({"outcome": "selected", "optionId": "reject_once"})
    );
    assert_eq!(
        two.tool_titles,
        [
            "run_commands: pip install requests",
            "read_files: src/calc.py"
        ],
        "the rejected call and the further call are both reported"
    );
}

#[test]
fn the_process_driver_answers_an_agent_permission_request_instead_of_waiting_for_it() {
    let d = scratch("process");
    let profile = load_profile(&repo().join("engine/profiles/fake-acp.toml")).unwrap();
    let vars: BTreeMap<&str, String> = BTreeMap::from([
        ("workspace", d.join("ws").display().to_string()),
        ("dataDir", d.display().to_string()),
        (
            "fixtures",
            fixtures_dir(&repo(), "fake-acp").display().to_string(),
        ),
    ]);
    let launch = resolve_launch(&profile, Transport::Acp, &vars).unwrap();
    let bin = Path::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut engine = ProcessEngine::spawn(&launch, Some(&bin), &[]).unwrap();
    let mut agentd = Agentd::default();
    agentd
        .handshake(&mut engine, &d.join("ws").display().to_string())
        .unwrap();
    agentd.prompt(&mut engine, FIRST).unwrap();
    let two = agentd.prompt(&mut engine, SECOND).unwrap();
    assert_eq!(two.stop_reason, "end_turn");
    assert_eq!(two.permissions.len(), 1, "{two:?}");
    assert_eq!(
        two.permissions[0].answer["outcome"]["optionId"],
        "reject_once"
    );
    engine.shutdown().unwrap();
    let logged = std::fs::read_to_string(d.join("fake-acp.log")).unwrap();
    assert!(logged.contains("outcome=reject_once"), "{logged}");
}
