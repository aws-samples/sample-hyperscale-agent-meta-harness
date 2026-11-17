//! reopened: the command time limit of the second engine where the pinned engine reads it.
//!
//! OpenCode 2.2.6 reads no variable and no configuration key for the limit of its shell tool, so the variable an earlier build set did nothing
//! and a command that named no limit was cut after 120 seconds. The limit now travels in the options of a small plugin that agentd writes beside
//! the session document and names in both layers of the configuration. These tests prove where agentd places the limit, what it reads back and
//! what it says.
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::direct::collector::DirectCollector;
use agentd::direct::recorder::{CallVerdict, Recorder};
use agentd::emit::EmitContext;
use agentd::engine::config::{Confirm, confirm_launch, finalize_env};
use agentd::engine::direct::{
    LIMIT_REASON, WAIT_ENV, launch_env, run_limit, wait_env, wait_seconds, wait_seconds_of_ms,
};
use agentd::engine::direct_words::{limit_not_applied, limit_not_confirmed, limit_not_placed};
use agentd::engine::faults::{EngineCut, cline_cut, opencode_cut};
use agentd::engine::launch::{Standalone, standalone};
use agentd::engine::opencode::{
    ENGINE_DEFAULT_LIMIT_MS, Line, READY_PREFIX, SHELL_LIMIT_DIR, SHELL_LIMIT_ID,
    SHELL_LIMIT_PLUGIN, check_limit, document, emit, ready_line, shell_limit_dir,
};
use agentd::engine::user_settings::{CommandSeconds, UserSettings, check_command_limit};
use agentd::engine::without_lines_starting;
use agentd::events::EventRecorder;
use b790_support::{MODEL, ctx, read_json, repo_root, scratch};
use harness_protocol::envelope::{EventType, Payload};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const NOT_HELD: &str =
    "engine settings: the engine's configuration does not hold the command time limit";

fn limited(ms: u64) -> EmitContext {
    EmitContext {
        command_limit_ms: Some(ms),
        ..ctx(MODEL)
    }
}

fn settings(seconds: Option<u32>) -> UserSettings {
    UserSettings {
        command_timeout: seconds.map(|s| CommandSeconds::new(s).unwrap()),
        ..UserSettings::default()
    }
}

/// The harness's root of the engine for a context: the folder beside its config directory.
fn root_of(c: &EmitContext) -> PathBuf {
    c.config_dir.with_file_name("harness-config")
}

fn inline(env: &BTreeMap<String, String>) -> Value {
    serde_json::from_str(&env["OPENCODE_CONFIG_CONTENT"]).unwrap()
}

// ---- the placement ----------------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_launch_of_the_pinned_line_names_the_limit_plugin_in_both_layers() {
    for ms in [600_000u64, 900_000] {
        let c = limited(ms);
        let out = emit(&c, Line::V2).unwrap();
        let want = json!([{
            "package": root_of(&c).join("plugins/hamh-shell-limit").display().to_string(),
            "options": {"timeoutMs": ms},
        }]);
        assert_eq!(read_json(&out.files[0])["plugins"], want, "the file layer");
        assert_eq!(inline(&out.env)["plugins"], want, "the inline layer");
        assert_eq!(document(&c, Line::V2)["plugins"], want);
    }
    assert_eq!(SHELL_LIMIT_DIR, "plugins/hamh-shell-limit");
    assert_eq!(SHELL_LIMIT_ID, "hamh.shell-limit");
}

#[test]
fn b800_the_plugin_file_is_the_compiled_text_under_the_harness_root() {
    let c = limited(600_000);
    let out = emit(&c, Line::V2).unwrap();
    let plugin = shell_limit_dir(&root_of(&c)).join("index.js");
    assert!(out.files.contains(&plugin), "{:?}", out.files);
    assert_eq!(
        std::fs::read_to_string(&plugin).unwrap(),
        SHELL_LIMIT_PLUGIN
    );
    assert!(
        std::fs::symlink_metadata(&plugin)
            .unwrap()
            .file_type()
            .is_file()
    );
    // A symbolic link where the plugins folder belongs is refused: root would otherwise write behind it.
    let c2 = limited(600_000);
    let elsewhere = scratch("elsewhere");
    std::fs::create_dir_all(root_of(&c2)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root_of(&c2).join("plugins")).unwrap();
    let err = emit(&c2, Line::V2).unwrap_err();
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

#[test]
fn b800_the_plugin_text_changes_no_text_the_model_reads() {
    // One hook, on the input of a call before it runs. No hook on a result, none on a session, no description.
    assert_eq!(SHELL_LIMIT_PLUGIN.matches(".hook(").count(), 1);
    assert!(SHELL_LIMIT_PLUGIN.contains(r#"ctx.tool.hook("execute.before""#));
    for absent in ["execute.after", "session.hook", "description"] {
        assert!(!SHELL_LIMIT_PLUGIN.contains(absent), "{absent}");
    }
    assert!(SHELL_LIMIT_PLUGIN.contains(r#"id: "hamh.shell-limit""#));
    assert!(SHELL_LIMIT_PLUGIN.contains("hamh.shell-limit: ready ${ms}"));
    // The range of the option is the range of the setting, in milliseconds.
    assert!(SHELL_LIMIT_PLUGIN.contains(&format!(
        "ms < {} || ms > {}",
        CommandSeconds::MIN * 1000,
        CommandSeconds::MAX * 1000
    )));
    // What the scans of the repository ask of a script file: plain ASCII, short lines, a first line that is a sentence.
    assert!(SHELL_LIMIT_PLUGIN.is_ascii());
    assert!(SHELL_LIMIT_PLUGIN.lines().all(|l| l.len() < 1000));
    assert!(SHELL_LIMIT_PLUGIN.starts_with("// The run's command time limit"));
}

#[test]
fn b800_the_launch_of_the_pinned_line_sets_no_variable_the_engine_does_not_read() {
    let out = emit(&limited(600_000), Line::V2).unwrap();
    assert!(
        out.env.keys().all(|k| !k.contains("EXPERIMENTAL")),
        "{:?}",
        out.env.keys()
    );
    for s in [settings(None), settings(Some(900))] {
        assert!(
            launch_env("opencode", &s).is_empty(),
            "no variable carries the limit"
        );
        assert_eq!(
            run_limit("opencode", &s).map(CommandSeconds::get),
            Some(s.command_timeout.map_or(600, CommandSeconds::get))
        );
    }
    assert!(run_limit("cline", &settings(Some(900))).is_none());
    assert!(run_limit("fake-acp", &settings(None)).is_none());
}

fn stand_alone(seconds: Option<u32>) -> agentd::engine::launch::Launch {
    let root = scratch("standalone");
    standalone(&Standalone {
        engine: "opencode",
        profiles_dir: &repo_root().join("engine/profiles"),
        root: &root,
        model: MODEL,
        region: "us-west-2",
        command_timeout_s: seconds,
        endpoint: None,
        idle_timeout: None,
    })
    .unwrap()
}

#[test]
fn b800_the_launch_of_the_pinned_line_sets_exactly_these_engine_variables() {
    let launch = stand_alone(None);
    let names: Vec<&str> = launch
        .env
        .keys()
        .map(String::as_str)
        .filter(|k| k.starts_with("OPENCODE_"))
        .collect();
    assert_eq!(
        names,
        [
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_DISABLE_AUTOUPDATE",
            "OPENCODE_DISABLE_CLAUDE_CODE",
            "OPENCODE_DISABLE_MODELS_FETCH",
            "OPENCODE_DISABLE_PROJECT_CONFIG",
            "OPENCODE_LOG_LEVEL",
            "OPENCODE_PRINT_LOGS",
        ]
    );
    assert_eq!(launch.env["OPENCODE_PRINT_LOGS"], "1");
    assert_eq!(launch.env["OPENCODE_LOG_LEVEL"], "WARN");
    assert_eq!(launch.env[WAIT_ENV], "240");
    assert_eq!(
        launch.ready_line.as_deref(),
        Some("hamh.shell-limit: ready 600000")
    );
    assert_eq!(launch.argv, ["/opt/harness/bin/opencode", "acp"]);
    // A user's limit travels the same way.
    let set = stand_alone(Some(60));
    assert_eq!(
        set.ready_line.as_deref(),
        Some("hamh.shell-limit: ready 60000")
    );
    assert_eq!(set.env[WAIT_ENV], "54");
    // The two stand-in knobs reach both layers and leave the plugin entry alone.
    let root = scratch("standalone-knobs");
    let knobs = standalone(&Standalone {
        engine: "opencode",
        profiles_dir: &repo_root().join("engine/profiles"),
        root: &root,
        model: MODEL,
        region: "us-west-2",
        command_timeout_s: Some(5),
        endpoint: Some("http://127.0.0.1:9"),
        idle_timeout: Some("20 seconds"),
    })
    .unwrap();
    for doc in [
        inline(&knobs.env),
        read_json(Path::new(&knobs.env["OPENCODE_CONFIG"])),
    ] {
        assert_eq!(
            doc["provider"]["amazon-bedrock"]["options"]["baseURL"],
            "http://127.0.0.1:9"
        );
        assert_eq!(doc["experimental"]["location_idle_timeout"], "20 seconds");
        assert_eq!(doc["plugins"][0]["options"]["timeoutMs"], 5000);
    }
}

#[test]
fn b800_no_other_source_file_names_an_engine_variable() {
    // It holds every engine variable a run can set only while no other source file adds one: the emitter and the kill switch table are the
    // two places that name them.
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    let mut naming: Vec<String> = files
        .iter()
        .filter(|p| {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains("\"OPENCODE_"))
        })
        .map(|p| p.strip_prefix(&src).unwrap().display().to_string())
        .collect();
    naming.sort();
    assert_eq!(naming, ["engine/layout.rs", "engine/opencode.rs"]);
}

#[test]
fn b800_no_limit_leaves_the_document_and_the_environment_as_they_were() {
    let c = ctx(MODEL);
    assert!(c.command_limit_ms.is_none());
    let out = emit(&c, Line::V2).unwrap();
    assert_eq!(out.files.len(), 1, "the session document and nothing else");
    assert!(read_json(&out.files[0]).get("plugins").is_none());
    assert!(inline(&out.env).get("plugins").is_none());
    let names: Vec<&str> = out.env.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_DISABLE_PROJECT_CONFIG"
        ]
    );
    assert!(!root_of(&c).join("plugins").exists());
    assert!(ready_line(&out.env).is_none());
}

#[test]
fn b800_the_first_line_gets_nothing_for_the_limit() {
    let c = limited(600_000);
    let out = emit(&c, Line::V1).unwrap();
    assert!(inline(&out.env).get("plugins").is_none());
    assert!(read_json(&out.files[0]).get("plugins").is_none());
    assert!(
        out.env
            .keys()
            .all(|k| !k.contains("TIMEOUT") && !k.contains("LOG")),
        "{:?}",
        out.env.keys()
    );
    assert!(!root_of(&c).join("plugins").exists());
    assert_eq!(
        check_limit(&c.config_dir, &out.env, 600_000).unwrap_err(),
        NOT_HELD
    );
    assert_eq!(Line::of_version("1.18.32"), Line::V1);
    assert_eq!(Line::of_version("2.2.6"), Line::V2);
}

#[test]
fn b800_finalize_env_keeps_the_plugins_member() {
    let c = limited(900_000);
    let mut env = emit(&c, Line::V2).unwrap().env;
    let before = inline(&env)["plugins"].clone();
    env.insert("HAMH_TOOLS_URL".into(), "http://127.0.0.1:4711/mcp".into());
    let profile = harness_core::engine_profile::load_profile(
        &repo_root().join("engine/profiles/opencode.toml"),
    )
    .unwrap();
    finalize_env(
        "opencode",
        &profile.version,
        profile.mcp.as_ref(),
        &repo_root(),
        &["spawn_agent".to_string()],
        &mut env,
    )
    .unwrap();
    let after = inline(&env);
    assert!(
        after.get("mcp").is_some(),
        "the tool server was added: {after}"
    );
    assert_eq!(after["permission"]["task"], "deny");
    assert_eq!(after["plugins"], before);
    check_limit(&c.config_dir, &env, 900_000).unwrap();
}

// ---- the read-back ----------------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_read_back_accepts_what_the_emitter_wrote() {
    for seconds in [None, Some(1), Some(900), Some(1800)] {
        let s = settings(seconds);
        let ms = run_limit("opencode", &s).unwrap().millis();
        let c = limited(ms);
        let env = emit(&c, Line::V2).unwrap().env;
        check_command_limit("opencode", &c.config_dir, &env, &s).unwrap();
        check_limit(&c.config_dir, &env, ms).unwrap();
    }
}

#[test]
fn b800_the_read_back_refuses_each_way_the_placement_can_be_wrong() {
    type Change = fn(&EmitContext, &mut BTreeMap<String, String>);
    fn set_inline(env: &mut BTreeMap<String, String>, edit: impl FnOnce(&mut Value)) {
        let mut doc = inline(env);
        edit(&mut doc);
        env.insert("OPENCODE_CONFIG_CONTENT".into(), doc.to_string());
    }
    fn set_file(env: &BTreeMap<String, String>, edit: impl FnOnce(&mut Value)) {
        let path = Path::new(&env["OPENCODE_CONFIG"]);
        let mut doc = read_json(path);
        edit(&mut doc);
        std::fs::write(path, doc.to_string()).unwrap();
    }
    fn plugin(c: &EmitContext) -> PathBuf {
        shell_limit_dir(&root_of(c)).join("index.js")
    }
    let cases: [(&str, Change); 9] = [
        ("the file removed", |c, _| {
            std::fs::remove_file(plugin(c)).unwrap()
        }),
        ("one byte changed", |c, _| {
            std::fs::write(plugin(c), SHELL_LIMIT_PLUGIN.replacen("shell", "shelL", 1)).unwrap();
        }),
        ("a link in its place", |c, _| {
            let copy = scratch("copy").join("index.js");
            std::fs::write(&copy, SHELL_LIMIT_PLUGIN).unwrap();
            std::fs::remove_file(plugin(c)).unwrap();
            std::os::unix::fs::symlink(&copy, plugin(c)).unwrap();
        }),
        ("the package another folder", |_, env| {
            set_inline(env, |d| {
                d["plugins"][0]["package"] = json!("/somewhere/else")
            });
        }),
        ("another number", |_, env| {
            set_file(env, |d| {
                d["plugins"][0]["options"]["timeoutMs"] = json!(120_000)
            });
        }),
        ("plugins missing in the file layer", |_, env| {
            set_file(env, |d| {
                d.as_object_mut().unwrap().remove("plugins");
            });
        }),
        ("plugins missing in the inline layer", |_, env| {
            set_inline(env, |d| {
                d.as_object_mut().unwrap().remove("plugins");
            });
        }),
        ("a second entry", |_, env| {
            set_inline(env, |d| {
                let first = d["plugins"][0].clone();
                d["plugins"].as_array_mut().unwrap().push(first);
            });
        }),
        ("no inline layer", |_, env| {
            env.remove("OPENCODE_CONFIG_CONTENT");
        }),
    ];
    for (what, change) in cases {
        let c = limited(900_000);
        let mut env = emit(&c, Line::V2).unwrap().env;
        change(&c, &mut env);
        let err = check_limit(&c.config_dir, &env, 900_000).unwrap_err();
        assert_eq!(err, NOT_HELD, "{what}");
        assert!(
            !err.contains("900") && !err.contains('/'),
            "{what}: no value, no path"
        );
        let s = settings(Some(900));
        assert_eq!(
            check_command_limit("opencode", &c.config_dir, &env, &s).unwrap_err(),
            NOT_HELD,
            "{what}"
        );
    }
    // A missing log variable is no refusal: the two are diagnostics.
    let c = limited(900_000);
    let mut env = emit(&c, Line::V2).unwrap().env;
    env.remove("OPENCODE_PRINT_LOGS");
    env.remove("OPENCODE_LOG_LEVEL");
    check_limit(&c.config_dir, &env, 900_000).unwrap();
    assert!(
        ready_line(&env).is_none(),
        "without them the line is never seen"
    );
}

#[test]
fn b800_a_default_that_cannot_be_placed_is_told_and_a_users_limit_still_ends_the_run() {
    use agentd::direct::engine_config::{LimitStep, limit_notice, limit_step};
    // The plugin file is gone before the engine starts.
    let broken = |seconds: Option<u32>| {
        let s = settings(seconds);
        let c = limited(run_limit("opencode", &s).unwrap().millis());
        let env = emit(&c, Line::V2).unwrap().env;
        std::fs::remove_file(shell_limit_dir(&root_of(&c)).join("index.js")).unwrap();
        limit_step("opencode", &c.config_dir, &env, &s)
    };
    // No user setting: told once, and the run goes on with the engine's own limit and a wait that fits it.
    let sentence = "This run's command time limit of 600 seconds could not be placed for OpenCode. Commands that set no time limit of their own are stopped after 120 seconds.";
    assert_eq!(
        broken(None),
        LimitStep::Told {
            notice: sentence.to_string(),
            wait_s: 108
        }
    );
    let event = limit_notice(sentence);
    assert_eq!(
        event,
        json!({"turn": 1, "kind": "notify", "reason": "command_limit", "outageMinutes": null, "detail": sentence})
    );
    // The user's own setting: as every user setting that cannot be read back, the run does not start, and nothing is told.
    assert_eq!(broken(Some(900)), LimitStep::Ended(NOT_HELD.to_string()));
    // An intact placement, and an engine with no such limit, are held.
    let c = limited(600_000);
    let env = emit(&c, Line::V2).unwrap().env;
    assert_eq!(
        limit_step("opencode", &c.config_dir, &env, &settings(None)),
        LimitStep::Held
    );
    assert_eq!(
        limit_step(
            "fake-acp",
            Path::new("/nonexistent"),
            &BTreeMap::new(),
            &settings(None)
        ),
        LimitStep::Held
    );
    // Cline keeps today's rule: only a limit the user set is looked at, and a mismatch ends the run.
    assert_eq!(
        limit_step(
            "cline",
            Path::new("/nonexistent"),
            &BTreeMap::new(),
            &settings(None)
        ),
        LimitStep::Held
    );
    assert!(matches!(
        limit_step(
            "cline",
            Path::new("/nonexistent"),
            &BTreeMap::new(),
            &settings(Some(900))
        ),
        LimitStep::Ended(_)
    ));
}

// ---- the wait bound ---------------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_wait_bound_follows_the_limit() {
    for (limit, wait) in [
        (600, 240),
        (120, 108),
        (1800, 240),
        (60, 54),
        (1, 1),
        (2, 1),
        (266, 239),
        (267, 240),
    ] {
        assert_eq!(
            wait_seconds(CommandSeconds::new(limit).unwrap()),
            wait,
            "{limit}"
        );
    }
    // A wait is shorter than the limit for every limit of 2 seconds and more, so the limit never cuts it.
    for limit in 2..=CommandSeconds::MAX {
        assert!(
            wait_seconds(CommandSeconds::new(limit).unwrap()) < limit,
            "{limit}"
        );
    }
    assert_eq!(wait_seconds_of_ms(ENGINE_DEFAULT_LIMIT_MS), 108);
    let of = |engine: &str, seconds: Option<u32>| wait_env(engine, &settings(seconds));
    assert_eq!(of("cline", None), Some(("HAMH_WAIT_S", "240".to_string())));
    assert_eq!(
        of("cline", Some(60)),
        Some(("HAMH_WAIT_S", "54".to_string()))
    );
    assert_eq!(
        of("opencode", None),
        Some(("HAMH_WAIT_S", "240".to_string()))
    );
    assert_eq!(
        of("opencode", Some(1800)),
        Some(("HAMH_WAIT_S", "240".to_string()))
    );
    assert_eq!(of("fake-acp", None), None);
    assert_eq!(WAIT_ENV, "HAMH_WAIT_S");
}

#[test]
fn b826_the_backstop_takes_the_bound_the_launch_carries_and_only_for_the_first_engine() {
    use agentd::engine::direct::guard_wait_bound;
    let launch = |s: &str| BTreeMap::from([(WAIT_ENV.to_string(), s.to_string())]);
    // The bound is read from the launch's own environment, so a bound a module lowered (108 for the engine's own limit) is the one judged.
    assert_eq!(guard_wait_bound("cline", &launch("240")), Some(240));
    assert_eq!(guard_wait_bound("cline", &launch("108")), Some(108));
    assert_eq!(guard_wait_bound("cline", &launch("54")), Some(54));
    // A launch without the variable, or with a value that is no whole number above 0, forgives nothing.
    assert_eq!(guard_wait_bound("cline", &BTreeMap::new()), None);
    assert_eq!(guard_wait_bound("fake-acp", &BTreeMap::new()), None);
    for bad in ["", "0", "-5", "2.5", "four"] {
        assert_eq!(guard_wait_bound("cline", &launch(bad)), None, "{bad:?}");
    }
    // The second engine's pending call frame is titled `shell` whatever the command is: five different long commands in a row look
    // like one call that was sent again, so its launch gives the count no bound, although the launch carries the variable.
    assert_eq!(
        wait_env("opencode", &settings(None)),
        Some(("HAMH_WAIT_S", "240".to_string()))
    );
    assert_eq!(guard_wait_bound("opencode", &launch("240")), None);
}

// ---- the sign at engine start -------------------------------------------------------------------------------------------------------

/// A log file and agentd's handle on it, as a captured spawn makes them.
fn log_with(text: &str) -> (PathBuf, agentd::engine::StderrCapture) {
    let path = scratch("log").join("engine-stderr.log");
    std::fs::write(&path, text).unwrap();
    let capture = agentd::engine::capture_of(&path).unwrap();
    (path, capture)
}

#[tokio::test]
async fn b800_confirm_finds_the_ready_line_anywhere_in_the_log() {
    let env = emit(&limited(600_000), Line::V2).unwrap().env;
    let line = "hamh.shell-limit: ready 600000";
    assert_eq!(ready_line(&env).as_deref(), Some(line));
    assert_eq!(READY_PREFIX, "hamh.shell-limit: ready ");
    let short = Duration::from_millis(600);
    let look = |text: String| {
        let env = env.clone();
        async move {
            let (_path, capture) = log_with(&text);
            confirm_launch("opencode", &env, &capture, short).await
        }
    };
    assert_eq!(look(format!("{line}\n")).await, Confirm::Confirmed);
    assert_eq!(look(format!("{line}\r\n")).await, Confirm::Confirmed);
    // Behind other lines, as at a chattier log level: the line sat at byte 5,951 in one measured start.
    let filler = "level=INFO message=\"something the engine says at start\"\n".repeat(110);
    assert!(filler.len() > 5_951);
    assert_eq!(look(format!("{filler}{line}\n")).await, Confirm::Confirmed);
    // Another number, a line that only starts with the text, a line not yet ended, and an empty log are no confirmation.
    for text in [
        "hamh.shell-limit: ready 120000\n".to_string(),
        format!("{line}0\n"),
        format!("x {line}\n"),
        line.to_string(),
        String::new(),
    ] {
        assert!(
            matches!(look(text.clone()).await, Confirm::Missing { .. }),
            "{text:?}"
        );
    }
    assert_eq!(
        look(String::new()).await,
        Confirm::Missing { headline: None }
    );
    // The engine's own warning is the headline of a missing confirmation.
    let warning = "level=WARN message=\"failed to load plugin\" target=/x/plugins/hamh-shell-limit cause=\"Expected identifier but found end of file\"";
    let Confirm::Missing { headline } = look(format!("{warning}\n")).await else {
        panic!("a log without the line is no confirmation");
    };
    assert!(headline.unwrap().contains("failed to load plugin"));
    // An engine with no sign is not asked, at once.
    let (_path, capture) = log_with("");
    for engine in ["cline", "fake-acp"] {
        assert_eq!(
            confirm_launch(engine, &env, &capture, Duration::from_secs(600)).await,
            Confirm::NotAsked
        );
    }
    assert_eq!(
        confirm_launch(
            "opencode",
            &BTreeMap::new(),
            &capture,
            Duration::from_secs(600)
        )
        .await,
        Confirm::NotAsked,
        "a launch without the plugin has no sign"
    );
    // A line that appears during the wait is found.
    let (path, capture) = log_with("");
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&path, format!("{line}\n")).unwrap();
    });
    assert_eq!(
        confirm_launch("opencode", &env, &capture, Duration::from_secs(5)).await,
        Confirm::Confirmed
    );
    writer.await.unwrap();
}

#[test]
fn b800_the_ready_line_is_dropped_from_what_reads_as_the_engines_last_words() {
    let ready = "hamh.shell-limit: ready 600000\n";
    assert!(without_lines_starting(ready.as_bytes(), READY_PREFIX).is_empty());
    assert!(without_lines_starting(ready.trim_end().as_bytes(), READY_PREFIX).is_empty());
    let log = format!("level=WARN first\n{ready}level=ERROR second\r\n{ready}tail without an end");
    assert_eq!(
        String::from_utf8(without_lines_starting(log.as_bytes(), READY_PREFIX)).unwrap(),
        "level=WARN first\nlevel=ERROR second\r\ntail without an end"
    );
    // A line that only holds the text is kept byte for byte.
    let other = "note: hamh.shell-limit: ready 1\n";
    assert_eq!(
        without_lines_starting(other.as_bytes(), READY_PREFIX),
        other.as_bytes()
    );
    assert_eq!(agentd::engine::config::HARNESS_LINE_PREFIX, READY_PREFIX);
}

// ---- the three sentences ----------------------------------------------------------------------------------------------------------

#[test]
fn b800_the_three_notices_read_word_for_word() {
    assert_eq!(
        limit_not_placed("opencode", 600, 120),
        "This run's command time limit of 600 seconds could not be placed for OpenCode. Commands that set no time limit of their own are stopped after 120 seconds."
    );
    assert_eq!(
        limit_not_confirmed("opencode", 600, 120),
        "OpenCode has not confirmed this run's command time limit of 600 seconds. If it is not in force, commands that set no limit of their own stop after 120 seconds."
    );
    assert_eq!(
        limit_not_applied("opencode", 120, 600),
        "OpenCode stopped a command after 120 seconds, not after this run's 600: the command time limit was not applied to it."
    );
    assert_eq!(LIMIT_REASON, "command_limit");
    assert_eq!(ENGINE_DEFAULT_LIMIT_MS, 120_000);
    // The client shows 160 characters of a notice (`ham_core::notice::NOTICE_MAX_CHARS`): each sentence fits with the largest numbers.
    for sentence in [
        limit_not_placed("opencode", 1800, 120),
        limit_not_confirmed("opencode", 1800, 120),
        limit_not_applied("opencode", 1800, 1800),
    ] {
        assert!(sentence.chars().count() <= 160, "{sentence}");
    }
    let env = emit(&limited(900_000), Line::V2).unwrap().env;
    assert_eq!(
        agentd::engine::config::limit_not_confirmed("opencode", &env).unwrap(),
        limit_not_confirmed("opencode", 900, 120)
    );
    assert!(agentd::engine::config::limit_not_confirmed("cline", &env).is_none());
}

// ---- the engine's own cut ----------------------------------------------------------------------------------------------------------

const CUT: &str = "Command exceeded timeout of 3000 ms. Retry with a larger timeout if the command is expected to take longer.";

fn final_frame(id: &str, texts: &[&str], metadata: Value) -> Value {
    json!({
        "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "rawInput": null,
        "content": texts.iter().map(|t| json!({"type": "content", "content": {"type": "text", "text": t}})).collect::<Vec<_>>(),
        "rawOutput": {"metadata": metadata},
    })
}

fn cut_meta() -> Value {
    json!({"status": "completed", "truncated": false, "timeout": true})
}

fn pending(id: &str) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "execute", "status": "pending", "title": "shell", "rawInput": {"cwd": "/repo"}})
}

fn running(id: &str, input: Value) -> Value {
    json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress", "title": input["command"], "rawInput": input})
}

#[test]
fn b800_a_cut_is_read_from_the_engines_flag_and_the_number_from_its_text() {
    let text = format!("(no output)\n\n{CUT}");
    let cut = final_frame("c1", &[&text, "Timed out before completion"], cut_meta());
    assert_eq!(opencode_cut(&cut), Some(EngineCut { ms: Some(3000) }));
    // A call the engine ran under another tool name: another sentence follows the engine's words.
    let bash = final_frame(
        "c1",
        &[
            &text.replace("3000", "5000"),
            "Timed out before completion There is no \"bash\" tool, so this ran as \"shell\". Call \"shell\" directly.",
        ],
        cut_meta(),
    );
    assert_eq!(opencode_cut(&bash), Some(EngineCut { ms: Some(5000) }));
    // The command printed the sentence itself before it was cut: the engine's own number is the last one.
    let printed = final_frame(
        "c1",
        &[&format!("{CUT}\n\n{}", CUT.replace("3000", "120000"))],
        cut_meta(),
    );
    assert_eq!(
        opencode_cut(&printed),
        Some(EngineCut { ms: Some(120_000) })
    );
    // The flag without readable words is still a cut; the words without the flag are none.
    assert_eq!(
        opencode_cut(&final_frame("c1", &["(no output)"], cut_meta())),
        Some(EngineCut { ms: None })
    );
    let only_words = final_frame(
        "c1",
        &[CUT],
        json!({"status": "completed", "truncated": false, "exit": 0}),
    );
    assert_eq!(opencode_cut(&only_words), None);
    assert_eq!(
        opencode_cut(&running("c1", json!({"command": "sleep 8"}))),
        None
    );
    // The first engine's words are read as before.
    assert!(cline_cut("Command timed out after 600000ms"));
    assert!(!cline_cut("Command timed out after a while"));
}

#[test]
fn b800_a_command_cut_by_the_second_engine_reads_as_timed_out() {
    let text = format!("(no output)\n\n{CUT}");
    let mut r = Recorder::new(Vec::new()).with_engine("opencode");
    r.on_tool_call(&pending("c1"));
    r.on_tool_call_update(&running(
        "c1",
        json!({"command": "sleep 8; echo SLEPT-8", "timeout": 3000, "cwd": "/repo"}),
    ));
    r.on_tool_call_update(&final_frame(
        "c1",
        &[&text, "Timed out before completion"],
        cut_meta(),
    ));
    let calls = r.calls();
    let call = calls.last().unwrap();
    assert_eq!(call.verdict, CallVerdict::TimedOut, "{call:?}");
    assert_eq!(call.verdict.word(), "timed_out");
    // The record keeps the call's real output, not the engine's metadata.
    assert!(
        call.output_tail
            .contains("Command exceeded timeout of 3000 ms"),
        "{call:?}"
    );
    assert!(!call.output_tail.contains("truncated"), "{call:?}");

    // The same words without the engine's flag: a command that printed them and ended well.
    let mut r = Recorder::new(Vec::new()).with_engine("opencode");
    r.on_tool_call(&pending("c2"));
    r.on_tool_call_update(&running("c2", json!({"command": "echo words"})));
    r.on_tool_call_update(&final_frame(
        "c2",
        &[CUT],
        json!({"status": "completed", "truncated": false, "exit": 0}),
    ));
    assert_eq!(r.calls().last().unwrap().verdict, CallVerdict::Ok);

    // The first engine's cut still reads as timed out.
    let mut r = Recorder::new(Vec::new()).with_engine("cline");
    r.on_tool_call(&json!({"sessionUpdate": "tool_call", "toolCallId": "c3", "status": "in_progress", "title": "run_commands: sleep 700",
        "rawInput": {"commands": ["sleep 700"]}}));
    r.on_tool_call_update(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "c3", "status": "failed",
        "rawOutput": [{"query": "sleep 700", "result": "Command timed out after 600000ms", "success": false}]}));
    assert_eq!(r.calls().last().unwrap().verdict, CallVerdict::TimedOut);
}

fn events(rec: &EventRecorder) -> Vec<Value> {
    rec.drain()
        .into_iter()
        .filter(|e| e.event_type == EventType::TurnProgress)
        .filter_map(|e| match e.payload {
            Payload::Inline { value } => Some(value),
            _ => None,
        })
        .filter(|v| v["kind"] == "notify")
        .collect()
}

fn update(u: Value) -> Value {
    json!({"sessionId": "s1", "update": u})
}

/// One shell call through a collector: the pending frame, the frame with the input (when `input` is given) and a final frame cut at `cut_ms`.
fn feed(c: &mut DirectCollector, id: &str, input: Option<Value>, cut_ms: u64) {
    use agentd::acp::AcpHandler;
    c.on_notification("session/update", &update(pending(id)));
    if let Some(input) = input {
        c.on_notification("session/update", &update(running(id, input)));
    }
    let text = format!(
        "(no output)\n\n{}",
        CUT.replace("3000", &cut_ms.to_string())
    );
    c.on_notification(
        "session/update",
        &update(final_frame(
            id,
            &[&text, "Timed out before completion"],
            cut_meta(),
        )),
    );
}

#[tokio::test(start_paused = true)]
async fn b800_a_cut_at_another_number_is_told_once_per_engine_session() {
    let collector = |limit: Option<u64>, engine: &str| {
        let rec = EventRecorder::new("run-1", "direct");
        (
            DirectCollector::new(rec.clone())
                .with_engine(engine)
                .with_command_limit(limit),
            rec,
        )
    };
    let no_limit = || Some(json!({"command": "sleep 130; echo NO", "cwd": "/repo"}));
    let sentence = "OpenCode stopped a command after 120 seconds, not after this run's 600: the command time limit was not applied to it.";

    let (mut c, rec) = collector(Some(600_000), "opencode");
    feed(&mut c, "c1", no_limit(), 120_000);
    let told = events(&rec);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["reason"], "command_limit");
    assert_eq!(told[0]["detail"], sentence);
    assert_eq!(told[0]["outageMinutes"], Value::Null);
    // A second such call in the same engine session: nothing more.
    feed(&mut c, "c2", no_limit(), 120_000);
    assert!(events(&rec).is_empty());
    // A new engine session is told again, once.
    c.set_session(2);
    feed(&mut c, "c3", no_limit(), 120_000);
    feed(&mut c, "c4", no_limit(), 120_000);
    assert_eq!(events(&rec).len(), 1);

    // What is never told: a limit the model named, a background call, a cut at the run's limit, a call whose input was never seen.
    let silent: [(Option<Value>, u64); 5] = [
        (
            Some(json!({"command": "sleep 130", "timeout": 120_000})),
            120_000,
        ),
        (Some(json!({"command": "sleep 130", "timeout": 3000})), 3000),
        (
            Some(json!({"command": "sleep 130", "background": true})),
            120_000,
        ),
        (no_limit(), 600_000),
        (None, 120_000),
    ];
    for (n, (input, cut)) in silent.into_iter().enumerate() {
        let (mut c, rec) = collector(Some(600_000), "opencode");
        feed(&mut c, "c1", input, cut);
        assert!(events(&rec).is_empty(), "case {n}");
    }
    // A null timeout is no limit of the model's own: told.
    let (mut c, rec) = collector(Some(600_000), "opencode");
    feed(
        &mut c,
        "c1",
        Some(json!({"command": "sleep 130", "timeout": null})),
        120_000,
    );
    assert_eq!(events(&rec).len(), 1);
    // A run limit equal to the engine's own number cannot tell, and says nothing; a collector that was given no limit says nothing.
    let (mut c, rec) = collector(Some(120_000), "opencode");
    feed(&mut c, "c1", no_limit(), 120_000);
    assert!(events(&rec).is_empty());
    let (mut c, rec) = collector(None, "opencode");
    feed(&mut c, "c1", no_limit(), 120_000);
    assert!(events(&rec).is_empty());
    let (mut c, rec) = collector(None, "cline");
    feed(&mut c, "c1", no_limit(), 120_000);
    assert!(events(&rec).is_empty());
}

#[test]
fn b800_the_digest_reads_the_engines_error_output_without_the_ready_line() {
    use agentd::direct::engine_error_tail;
    let dir = scratch("digest-tail");
    let log = dir.join("engine-stderr.log");
    // A normal run of the second engine: the log holds the plugin's line and nothing of the engine's.
    std::fs::write(&log, "hamh.shell-limit: ready 600000\n").unwrap();
    assert_eq!(engine_error_tail(&log, 3 * 1024), None);
    // The engine's own warning stays, byte for byte, whichever side of the line it is on.
    let warning = "level=WARN message=\"failed to load plugin\" target=/x/plugins/hamh-shell-limit";
    std::fs::write(
        &log,
        format!("{warning}\nhamh.shell-limit: ready 600000\nlast words of the engine\n"),
    )
    .unwrap();
    assert_eq!(
        engine_error_tail(&log, 3 * 1024).as_deref(),
        Some(format!("{warning}\nlast words of the engine").as_str())
    );
    // The first engine's log has no such line and is read as it was.
    std::fs::write(&log, "Error: boom\n").unwrap();
    assert_eq!(
        engine_error_tail(&log, 3 * 1024).as_deref(),
        Some("Error: boom")
    );
    // A link the engine planted there is not followed, and a missing log is no tail.
    let link = dir.join("link.log");
    std::os::unix::fs::symlink(&log, &link).unwrap();
    assert_eq!(engine_error_tail(&link, 3 * 1024), None);
    assert_eq!(engine_error_tail(&dir.join("absent.log"), 3 * 1024), None);
}
