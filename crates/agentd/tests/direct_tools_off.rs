//! What the tool hook leaves alone.
//!
//! The layer is an addition to a working run, and the standing rule is that an optional addition never costs the required path. So the module
//! has three ways to be absent, and in each a run is the run it was, byte for byte:
//!
//! - the kill switch `AGENTD_TOOLS_MODE=off` removes the module from the list (`tools::hook` answers `None`);
//! - a profile with no `[mcp]` table (or `via = "none"`) is an engine the layer has not been wired to: the module is in the list and does
//!   nothing at all, not a file, not a variable, not an event, not a log line, not a summary member;
//! - `AGENTD_TOOLS_DISABLE` removes single tools, and a module left with no tool does not start a server.
//!
//! The 13 golden streams of `direct_golden.rs` run the product's own list with a test profile that has no table, so they pin the second case;
//! the first is pinned here twice: by the list (the nine modules that were there before this one) and by a whole run with the switch off and
//! the table present, compared with the control run of a profile without the table.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod hook_support;
mod tools_support;

use agentd::direct::Deps;
use agentd::direct::goal::settings::GoalSettings;
use agentd::direct::hooks::{DirectHook, HookEnv, Hooks};
use agentd::direct::tools::settings::{Mode, Settings};
use agentd::direct::tools::{self, ToolsHook};
use agentd::events::{EventRecorder, TurnControls};
use common::{plugin_folder, repo_root};
use direct_support::{Direct, Opts, RUN};
use hook_support::{FILE, Rig, exists};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use tools_support::echo::Echo;
use tools_support::{refuses, request, rpc};

/// The nine modules of the product before the tool layer, in the order they were asked.
const BEFORE: [&str; 9] = [
    "transcript",
    "goal",
    "research",
    "attachments",
    "engine_config",
    "skills",
    "continue_from",
    "report",
    "messages",
];

fn off() -> Settings {
    Settings::from_lookup(&|k| (k == "AGENTD_TOOLS_MODE").then(|| "off".to_string()))
}

fn goal_with_plugins() -> GoalSettings {
    let dir = plugin_folder().display().to_string();
    GoalSettings::from_lookup(&move |k| (k == "AGENTD_GOAL_PLUGIN_DIR").then(|| dir.clone()))
}

// ---- the list ------------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn with_the_module_off_the_product_list_is_the_nine_it_was() {
    let d = Direct::start(Opts::default()).await;
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
    assert_eq!(off().mode, Mode::Off);
    assert!(!off().enabled);
    let hooks = Hooks::standard_with_tools(&env, goal_with_plugins(), off());
    assert_eq!(
        hooks.names(),
        BEFORE,
        "the kill switch leaves the list as it was"
    );
    // Switched on, the module sits right after research.
    let on = Hooks::standard_with_tools(&env, goal_with_plugins(), Settings::default());
    let names = on.names();
    assert_eq!(names.len(), 10, "{names:?}");
    let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert_eq!(at("tools"), at("research") + 1, "{names:?}");
    // The word `off` is not the only one that reads as off: a typo in the switch is off too, and the list agrees.
    let typo = Settings::from_lookup(&|k| (k == "AGENTD_TOOLS_MODE").then(|| "of".to_string()));
    assert_eq!(
        Hooks::standard_with_tools(&env, goal_with_plugins(), typo).names(),
        BEFORE
    );
    // ... and the log says why the module is missing, once.
    let said = d
        .log
        .lines()
        .iter()
        .filter(|l| {
            l.contains("direct tools off") && l.contains("neither an on word nor an off word")
        })
        .count();
    assert_eq!(said, 1, "{:?}", d.log.lines());
}

#[test]
fn the_constructors_answer_none_when_off_and_a_module_when_on() {
    assert!(tools::hook_with(off()).is_none());
    assert!(tools::hook_with(Settings::default()).is_some());
    let h = tools::hook_with(Settings::default()).unwrap();
    assert_eq!(h.name(), "tools");
}

// ---- a whole run with the switch off -------------------------------------------------------------------------------------------------

/// What differs between two runs of the same code: scratch paths, ids, times, ports, durations and sizes.
struct Norm {
    dir: String,
    ulid: Regex,
    time: Regex,
    sha: Regex,
    port: Regex,
}

impl Norm {
    fn new(dir: &std::path::Path) -> Self {
        Self {
            dir: dir.display().to_string(),
            ulid: Regex::new(r"\b[0-9A-HJKMNP-TV-Z]{26}\b").unwrap(),
            time: Regex::new(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z").unwrap(),
            sha: Regex::new(r"\b[0-9a-f]{40,64}\b").unwrap(),
            port: Regex::new(r"127\.0\.0\.1:\d+").unwrap(),
        }
    }

    fn text(&self, s: &str) -> String {
        let s = s.replace(&self.dir, "<dir>");
        let s = self.time.replace_all(&s, "<time>");
        let s = self.sha.replace_all(&s, "<sha>");
        let s = self.port.replace_all(&s, "<addr>");
        self.ulid
            .replace_all(&s, |c: &regex::Captures| {
                if &c[0] == RUN { "<run>" } else { "<ulid>" }.to_string()
            })
            .to_string()
    }

    fn value(&self, key: &str, v: &Value) -> Value {
        let variable = key.ends_with('S')
            || key.ends_with("Bytes")
            || key.ends_with("Ms")
            || key.ends_with("_s")
            || key.ends_with("_ms")
            || matches!(key, "ms" | "seq" | "bytes" | "pid");
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Number(_) if variable => json!("<n>"),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(key, x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (self.text(k), self.value(k, x)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// The observable record of a run served by the real `agentd serve` process: the engine's wire, the events, the `turn-complete`, the summary,
/// the prompts and agentd's own log lines.
fn record(d: &Direct) -> Value {
    let n = Norm::new(&d.dir);
    let wire: Vec<Value> = d
        .fake_engine_log()
        .lines()
        .map(|l| {
            for dir in ["in ", "out "] {
                if let Some(j) = l.strip_prefix(dir)
                    && let Ok(v) = serde_json::from_str::<Value>(j)
                {
                    return json!({ dir.trim(): n.value("", &v) });
                }
            }
            json!(n.text(l))
        })
        .collect();
    let events: Vec<Value> = d
        .aws
        .events()
        .into_iter()
        .map(|(t, v)| json!({"type": serde_json::to_value(t).unwrap(), "payload": n.value("", &v)}))
        .collect();
    let complete: Vec<Value> = d
        .aws
        .complete()
        .iter()
        .map(|c| n.value("", &serde_json::to_value(c).unwrap()))
        .collect();
    let summary = d
        .aws
        .get(&harness_protocol::direct::s3::summary_key(RUN))
        .map(|b| n.value("", &serde_json::from_slice::<Value>(&b).unwrap()));
    let log: Vec<String> = std::fs::read_to_string(d.process.as_ref().unwrap().log_file.clone())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("agentd: "))
        .map(|l| n.text(l))
        // the numbers of durations and byte counts differ between runs
        .map(|l| {
            Regex::new(r"\b\d+(\.\d+)? ?(s|ms)\b")
                .unwrap()
                .replace_all(&l, "<n>s")
                .to_string()
        })
        .collect();
    json!({
        "wire": wire,
        "events": events,
        "complete": complete,
        "summary": summary,
        "prompts": d.prompts_received().iter().map(|p| n.text(p)).collect::<Vec<_>>(),
        "agentd_log": log,
    })
}

fn sorted(v: Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut e: Vec<(String, Value)> = o.into_iter().collect();
            e.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(e.into_iter().map(|(k, x)| (k, sorted(x))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// One run of the real `agentd serve`, with the copies while the engine works off (`snapshotEveryS` 0) as the golden streams run them:
/// a copy that a tool call or the timer starts during a short scenario is stored or not by timing, and would make
/// two runs of the same product name a different number of copies. The copy of the end is in every record.
async fn served_run(opts: Opts) -> Direct {
    let d = Direct::start_process(Opts {
        snapshot_every_s: Some(0),
        ..opts
    })
    .await;
    d.invoke().await;
    d.wait_healthy().await;
    d
}

/// The golden streams of `direct_golden.rs` run the product's list on a profile with no `[mcp]` table, and this test runs the real `agentd serve`
/// process on three configurations and compares the full record of each with the normalizer of the golden tests (wire, events, `turn-complete`,
/// summary, prompts, agentd's log): the control (no table: what the golden streams run), the kill switch off with the table present (must be
/// the control, byte for byte), and the switch unset with the table present (must differ by the module's note and log lines and nothing else).
/// It compares runs with each other, not with the stored files, because the stored files are checked by `direct_golden.rs` on the control
/// configuration, and the three configurations must be the same product on the same day.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_module_off_the_golden_streams_are_unchanged() {
    // The control: the product's list, a profile with no table (what the 13 golden streams run).
    let control = served_run(Opts::default()).await;
    // The kill switch in the environment of the real `agentd serve` process, with the table present in the profile.
    let switched_off = served_run(Opts {
        mcp: true,
        agentd_env: vec![("AGENTD_TOOLS_MODE", "off".to_string())],
        ..Opts::default()
    })
    .await;
    // The trigger is present: the same table, the switch not set, and the run does differ (the module is in the list and says what it did).
    let switched_on = served_run(Opts {
        mcp: true,
        ..Opts::default()
    })
    .await;

    let (a, b, c) = (
        record(&control),
        record(&switched_off),
        record(&switched_on),
    );
    let pretty = |v: &Value| serde_json::to_string_pretty(&sorted(v.clone())).unwrap();
    assert_eq!(
        pretty(&a),
        pretty(&b),
        "AGENTD_TOOLS_MODE=off must leave the record of a run as the control run has it"
    );
    // The trigger is present: with the switch unset the module is in the run and says what it did, in a note that is a valid one.
    let note: harness_protocol::direct::DirectToolsNote = serde_json::from_value(
        c["summary"]
            .get("tools")
            .unwrap_or_else(|| panic!("TRIGGER ABSENT: no tools member in {}", c["summary"]))
            .clone(),
    )
    .unwrap();
    assert!(note.is_valid(), "{note:?}");
    assert!(
        c["agentd_log"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap_or("").starts_with("direct tools")),
        "TRIGGER ABSENT: the module said nothing"
    );
    println!("TRIGGER PRESENT tools-module-in-run");
    // And that is all it does to a run: its note and its own log lines. Whatever else differs from the control run would be a change of the
    // run's record that the switch is there to prevent.
    let mut only_the_module = c.clone();
    only_the_module["summary"]
        .as_object_mut()
        .unwrap()
        .remove("tools");
    only_the_module["agentd_log"]
        .as_array_mut()
        .unwrap()
        .retain(|l| !l.as_str().unwrap_or("").starts_with("direct tools"));
    assert_eq!(
        pretty(&a),
        pretty(&only_the_module),
        "with the switch unset the module adds its note and its log lines to a run, and nothing else"
    );
    // And nothing of the module is on the disk of the switched-off run.
    let agent_data = switched_off.agent_data();
    assert!(!exists(&agent_data.join("data/settings").join(FILE)));
}

#[test]
fn the_stripped_test_profile_is_the_real_profile_less_exactly_the_mcp_table() {
    use harness_core::engine_profile::load_profile;
    let real = load_profile(&repo_root().join("engine/profiles/cline.toml")).unwrap();
    assert!(real.mcp.is_some(), "the real profile has the table");
    for (mcp, want) in [(false, None), (true, real.mcp.clone())] {
        let dir = common::scratch("profile");
        let path = dir.join("cline.toml");
        std::fs::write(&path, direct_support::test_cline_profile(mcp)).unwrap();
        let p = load_profile(&path).unwrap();
        assert_eq!(p.mcp, want, "mcp={mcp}");
        assert_eq!(p.name, real.name);
        assert_eq!(p.hidden_paths, real.hidden_paths);
        assert_eq!(p.env_remove, real.env_remove);
        assert_eq!(p.rows, real.rows);
        assert_eq!(p.gate, real.gate);
        // The environment is the real one and the fake engine's own variables.
        let mut env = p.env.clone();
        env.remove("FAKE_ACP_LOG");
        env.remove("HAMH_FAKE_MCP_SETTINGS");
        assert_eq!(env, real.env, "mcp={mcp}");
        assert_eq!(
            p.env.contains_key("HAMH_FAKE_MCP_SETTINGS"),
            mcp,
            "the fake engine is told where the registration file is only when the table is there"
        );
    }
}

// ---- a profile with no table, a profile without a server, and the disable list -----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_without_an_mcp_table_adds_nothing() {
    // The `fake-acp` profile has no `[mcp]` table. The module is in the list and the run goes through the whole driver.
    let d = Direct::start(Opts {
        engine: "fake-acp",
        ..Opts::default()
    })
    .await;
    let hook = Arc::new(ToolsHook::with_factory(Settings::default(), |_, _| {
        vec![Echo::new().0]
    }));
    let deps = Deps {
        hooks: Some(Hooks::new(vec![hook.clone() as Arc<dyn DirectHook>])),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(
        resp.outcome,
        harness_protocol::runtime::TurnOutcome::Completed,
        "{resp:?}"
    );
    // No file under the engine's directories, no variable in the engine's environment, no event, no log line, no summary member.
    let data = d.agent_data().join("data");
    let mut found = Vec::new();
    for entry in walk(&data) {
        if entry.to_string_lossy().contains("mcp") {
            found.push(entry);
        }
    }
    assert!(found.is_empty(), "files: {found:?}");
    assert!(
        !d.engine_probe().contains("HAMH_TOOLS_URL"),
        "{}",
        d.engine_probe()
    );
    assert!(
        !d.log.lines().iter().any(|l| l.contains("direct tools")),
        "{:?}",
        d.log.lines()
    );
    assert!(
        d.aws
            .events()
            .iter()
            .all(|(_, v)| !v.to_string().contains("\"tools\"") && !v.to_string().contains("mcp")),
        "an event names the module"
    );
    assert!(d.summary().tools.is_none(), "{:?}", d.summary().tools);
    assert!(hook.note().is_none(), "the module decided nothing");
    assert_eq!(hook.port(), None);
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            out.push(p.clone());
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.extend(walk(&p));
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_that_takes_no_server_adds_nothing_either() {
    let mut rig = Rig::new().await;
    rig.mcp = serde_json::from_value(json!({"via": "none"})).unwrap();
    let hook = ToolsHook::new(Settings::default());
    let (echo, _) = Echo::new();
    rig.start(&hook, vec![echo]).await;
    assert!(hook.note().is_none());
    assert!(!rig.engine_env.contains_key("HAMH_TOOLS_URL"));
    assert!(!exists(&rig.file()));
    assert!(rig.tools_log().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn disable_names_remove_single_tools() {
    // A name the table does not have yet leaves the tools that are there.
    let mut rig = Rig::new().await;
    let hook = ToolsHook::new(Settings {
        port: 0,
        disabled: BTreeSet::from(["semantic_search".to_string()]),
        ..Settings::default()
    });
    let (echo, _) = Echo::new();
    rig.start(&hook, vec![echo]).await;
    let list = rpc(rig.addr(), &request(json!(1), "tools/list", json!({})))
        .await
        .json();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["code_search"], "the others stay");
    assert_eq!(hook.note().unwrap().list, ["code_search"]);
    hook.stop_for_tests().await;

    // The tool of the table, disabled: nothing is left to offer, so the module starts no server, writes no file and sets no variable, and
    // the summary says why.
    let mut rig = Rig::new().await;
    let busy = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let hook = ToolsHook::new(Settings {
        port: busy,
        disabled: BTreeSet::from(["code_search".to_string()]),
        ..Settings::default()
    });
    let (echo, _) = Echo::new();
    rig.start(&hook, vec![echo]).await;
    let note = hook.note().unwrap();
    assert!(!note.enabled);
    // Every tool that was built was switched off by name: a choice, not the empty list of a stage that builds none.
    assert_eq!(note.reason.as_deref(), Some("all-disabled"));
    assert!(note.list.is_empty() && note.counts.is_none());
    assert!(!exists(&rig.file()));
    assert!(!rig.engine_env.contains_key("HAMH_TOOLS_URL"));
    assert!(refuses(format!("127.0.0.1:{busy}").parse().unwrap()).await);
    assert_eq!(hook.port(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_with_no_backend_at_all_is_off_for_the_same_reason() {
    let mut rig = Rig::new().await;
    let hook = ToolsHook::new(Settings::default());
    rig.start(&hook, Vec::new()).await;
    let note = hook.note().unwrap();
    assert!(!note.enabled);
    assert_eq!(note.reason.as_deref(), Some("no-tools"));
    assert!(!exists(&rig.file()));
}
