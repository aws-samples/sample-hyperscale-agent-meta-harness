//! The tool names and the title convention of each engine live in one table under `src/engine/`. The recorder, the goal watch
//! and the helper notice ask it by engine name, so no tool of one engine is the yardstick of another.
#![allow(clippy::unwrap_used)]

use agentd::acp::AcpHandler;
use agentd::direct::collector::DirectCollector;
use agentd::direct::goal::helper_notice;
use agentd::direct::goal::watch::{HelperRule, HelperStall};
use agentd::direct::recorder::{Recorder, StopContext};
use agentd::engine::goal::goal_engine;
use agentd::engine::tools::{TitleStyle, calls_tool, engine_tools, known_tool, tool_of};
use agentd::events::EventRecorder;
use serde_json::{Value, json};
use std::time::Duration;

fn call(id: &str, title: &str, input: Value) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "other",
           "status": "pending", "rawInput": input})
}

#[test]
fn b789_the_table_of_the_qualified_engine_names_its_seven_tools_and_its_helper_tool() {
    let t = engine_tools("cline");
    let names: Vec<&str> = t.known.iter().map(|k| k.name).collect();
    assert_eq!(
        names,
        [
            "apply_patch",
            "editor",
            "fetch_web_content",
            "read_files",
            "run_commands",
            "search_codebase",
            "spawn_agent"
        ]
    );
    assert_eq!(t.delegate, Some("spawn_agent"));
    assert_eq!(t.title, TitleStyle::NameColonArgs);
    let run = known_tool("cline", "run_commands").unwrap();
    assert_eq!(
        (run.allowed, run.required),
        (&["commands"][..], &["commands"][..])
    );
    assert!(known_tool("cline", "frobnicate").is_none());
}

#[test]
fn b789_no_tool_of_the_qualified_engine_is_the_yardstick_of_another_engine() {
    for engine in ["opencode", "an-engine-with-no-table"] {
        let t = engine_tools(engine);
        assert!(t.known.is_empty(), "{engine}: {:?}", t.known);
        assert!(known_tool(engine, "run_commands").is_none(), "{engine}");
    }
    // The generic table names no helper tool; OpenCode's is `subagent`, from recorded sessions of v2.2.6.
    assert_eq!(engine_tools("an-engine-with-no-table").delegate, None);
    assert_eq!(engine_tools("opencode").delegate, Some("subagent"));
    // The fake ACP engine replays recordings of the qualified engine, and a recorder never told its engine reads the same table.
    assert_eq!(engine_tools("fake-acp"), engine_tools("cline"));
    assert_eq!(engine_tools(""), engine_tools("cline"));
}

#[test]
fn b789_the_name_of_a_tool_is_read_from_the_title_in_the_engines_style() {
    assert_eq!(tool_of("cline", "run_commands: ls -la"), "run_commands");
    assert_eq!(tool_of("an-engine-with-no-table", "bash: ls -la"), "bash");
    // OpenCode titles a pending call by the tool's name and a running shell call by its command: a command is no tool name.
    assert_eq!(tool_of("opencode", "shell"), "shell");
    assert_eq!(tool_of("opencode", "ls -la"), "");
    // No prefix, an upper-case start, a name that is too long and a path-like prefix are not tool names.
    assert_eq!(tool_of("cline", "Reading files"), "");
    assert_eq!(tool_of("cline", "Run: ls"), "");
    assert_eq!(tool_of("cline", &format!("{}: x", "a".repeat(41))), "");
    assert_eq!(tool_of("cline", "src/main.rs: line 3"), "");
    assert_eq!(tool_of("cline", ": x"), "");
}

#[test]
fn b789_a_title_names_a_tool_when_it_is_the_name_alone_or_the_name_before_a_colon() {
    for engine in ["cline", "an-engine-with-no-table"] {
        assert!(calls_tool(
            engine,
            "spawn_agent: review the tests",
            "spawn_agent"
        ));
        assert!(calls_tool(engine, "spawn_agent", "spawn_agent"));
        assert!(
            !calls_tool(engine, "spawn_agents: x", "spawn_agent"),
            "a longer name is another tool"
        );
        assert!(
            !calls_tool(engine, "read: spawn_agent", "spawn_agent"),
            "the name must lead"
        );
        assert!(!calls_tool(engine, "", "spawn_agent"));
    }
}

#[test]
fn b789_the_recorder_checks_the_input_keys_of_the_tools_of_the_engine_it_serves() {
    let bad = call("t1", "spawn_agent: x", json!({"task": "t", "extra": 1}));
    // Served engine unnamed or Cline: the unknown key and the missing key are anomalies.
    for r in [
        Recorder::new(vec![]),
        Recorder::new(vec![]).with_engine("cline"),
    ] {
        let mut r = r;
        r.on_tool_call(&bad);
        let lines: Vec<String> = r.anomalies().into_iter().map(|a| a.line).collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("unknown key \"extra\"")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("missing required key \"systemPrompt\"")),
            "{lines:?}"
        );
    }
    // An engine with no table has no tool of that name checked: the same call is judged by blank keys only.
    let mut r = Recorder::new(vec![]).with_engine("an-engine-with-no-table");
    r.on_tool_call(&bad);
    assert!(r.anomalies().is_empty(), "{:?}", r.anomalies());
    assert_eq!(
        r.calls()[0].tool,
        "spawn_agent",
        "the name is still read from the title"
    );
    r.on_tool_call(&call("t2", "bash: ls", json!({"": 1})));
    assert_eq!(
        r.anomalies().len(),
        1,
        "a blank key is an anomaly for every engine"
    );
}

#[test]
fn b789_the_collector_hands_its_engine_to_the_recorder() {
    let bad = call("t1", "spawn_agent: x", json!({"task": "t", "extra": 1}));
    let update = json!({"sessionId": "s-1", "update": bad});
    let anomalies = |engine: Option<&str>| {
        let rec = EventRecorder::new("run1", "direct");
        let mut c = DirectCollector::new(rec);
        if let Some(e) = engine {
            c = c.with_engine(e);
        }
        c.on_notification("session/update", &update);
        c.stop_digest(1, StopContext::default()).anomalies.len()
    };
    assert_eq!(
        anomalies(None),
        2,
        "an unnamed engine reads the qualified table"
    );
    assert_eq!(anomalies(Some("cline")), 2);
    assert_eq!(
        anomalies(Some("opencode")),
        0,
        "OpenCode is not judged by another engine's tools"
    );
}

fn stall() -> HelperStall {
    HelperStall {
        rule: HelperRule::Silence,
        open: 2,
        silent: Duration::from_secs(1500),
        oldest: Duration::from_secs(1700),
        limit: Duration::from_secs(1500),
    }
}

#[test]
fn b789_the_helper_notice_names_the_helper_tool_of_the_engine_and_says_nothing_of_one_it_lacks() {
    let cline = goal_engine("cline").unwrap();
    let with = helper_notice(&stall(), 1, cline.delegate_tool);
    assert!(
        with.contains("do not start helper agents (spawn_agent) again in this run."),
        "{with}"
    );
    let without = helper_notice(&stall(), 1, None);
    assert!(
        without.contains("do not start helper agents again in this run."),
        "{without}"
    );
    assert!(!without.contains("spawn_agent"), "{without}");
    // Everything but the parenthesis is the same text.
    assert_eq!(with.replace(" (spawn_agent)", ""), without);
}
