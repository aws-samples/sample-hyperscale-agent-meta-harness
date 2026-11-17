//! The `engine_config` module of the seam: after the startup wrote the engine's argv,
//! environment and two JSON files, the module reads them back and compares them with what the user's settings call for. Only then does it post
//! the `engine-config` event (with `readBack: "ok"`) and fill `DirectSummary.engineConfig`. A mismatch ends the run `stopped_setup_failed`
//! ("did not start") before the first model call.
//!
//! The test module plays the part that could change a file between the write and the read-back: it is registered first, so it runs first in the
//! startup, and edits the file, as a root-owned writer of another module or a mistake in the driver would.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::BoxFuture;
use agentd::direct::Deps;
use agentd::direct::engine_config::EngineConfigHook;
use agentd::direct::hooks::{DirectHook, Hooks, StartupCtx};
use direct_support::{Direct, Opts, RUN, sha};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::runtime::TurnOutcome;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

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

fn all_five() -> Value {
    json!({"engineConfig": {"engine": "cline", "schema": 1, "settings": {
        "reasoning": {"effort": "high"},
        "commands": {"timeout_seconds": 900},
        "context": {"compaction": "basic", "compaction_trigger": 0.8},
        "tools": {"disable": ["fetch_web_content"]}
    }}})
}

/// An edit of what the startup placed.
type Edit = fn(&mut StartupCtx<'_>);

/// A module that edits what the startup placed, between the write and the read-back.
struct Tamper {
    edit: Edit,
}

impl std::fmt::Debug for Tamper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tamper")
    }
}

impl DirectHook for Tamper {
    fn name(&self) -> &'static str {
        "tamper"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            (self.edit)(ctx);
            Ok(())
        })
    }
}

/// The run with the tamper module first and the engine settings module after it.
async fn run_with(d: &Direct, edit: Edit) -> harness_protocol::runtime::InvocationResponse {
    let tamper: Arc<dyn DirectHook> = Arc::new(Tamper { edit });
    let module: Arc<dyn DirectHook> = Arc::new(EngineConfigHook::new());
    let deps = Deps {
        hooks: Some(Hooks::new(vec![tamper, module])),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

fn edit_json(path: &Path, edit: impl FnOnce(&mut Value)) {
    let dir = path.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    edit(&mut v);
    std::fs::write(path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

fn nothing(_: &mut StartupCtx<'_>) {}

fn change_the_compaction_trigger(ctx: &mut StartupCtx<'_>) {
    edit_json(&ctx.data_dir.join("settings/providers.json"), |v| {
        v["providers"]["bedrock"]["settings"]["compaction"]["thresholdRatio"] = json!(0.5);
    });
}

fn empty_the_locked_tool_list(ctx: &mut StartupCtx<'_>) {
    edit_json(&ctx.config_dir.join("locked/global-settings.json"), |v| {
        v["disabledTools"] = json!([]);
    });
}

fn switch_telemetry_on(ctx: &mut StartupCtx<'_>) {
    edit_json(&ctx.config_dir.join("locked/global-settings.json"), |v| {
        v["telemetryOptOut"] = json!(false);
    });
}

fn drop_the_time_limit(ctx: &mut StartupCtx<'_>) {
    ctx.engine_env.remove("CLINE_RUN_COMMANDS_TIMEOUT_MS");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_change_in_between_the_read_back_passes_and_the_event_says_so() {
    // The control: the same two modules, the first one changing nothing. The mismatches below are the trigger, not the setup.
    let mut d = Direct::start(Opts::default()).await;
    with_members(&mut d, all_five());
    let resp = run_with(&d, nothing).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    let events = d.aws.progress("engine-config");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].1["readBack"], "ok");
    assert!(d.summary().engine_config.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_back_mismatch_stops_the_run() {
    let cases: [(&str, Edit, &str); 4] = [
        (
            "compaction trigger changed in the provider file",
            change_the_compaction_trigger,
            "provider file",
        ),
        (
            "tool list emptied in the locked settings",
            empty_the_locked_tool_list,
            "global settings",
        ),
        (
            "telemetry switched on in the locked settings",
            switch_telemetry_on,
            "global settings",
        ),
        (
            "command time limit removed from the environment",
            drop_the_time_limit,
            "environment",
        ),
    ];
    for (what, edit, words) in cases {
        let mut d = Direct::start(Opts::default()).await;
        with_members(&mut d, all_five());
        let resp = run_with(&d, edit).await;
        assert_ne!(resp.outcome, TurnOutcome::Completed, "{what}: {resp:?}");
        let s = d.summary();
        assert_eq!(
            s.outcome,
            DirectOutcome::StoppedSetupFailed,
            "{what}: {s:?}"
        );
        assert_eq!(s.label, "Cline did not start", "{what}");
        let detail = s.detail.clone().unwrap();
        assert!(
            detail.contains("engine settings") && detail.contains(words),
            "{what}: {detail}"
        );
        // No value of the document, and none of the file, in the words.
        for value in ["0.5", "0.8", "900", "fetch_web_content"] {
            assert!(!detail.contains(value), "{what}: {detail}");
        }
        // Before the first model call: the engine never got a prompt, and nothing says the settings were placed.
        assert!(d.prompts_received().is_empty(), "{what}");
        assert!(d.aws.progress("engine-config").is_empty(), "{what}");
        assert!(s.engine_config.is_none(), "{what}");
    }
    println!(
        "ENGINE-CONFIG TRIGGER PRESENT: 4 changes between the write and the read-back each ended stopped_setup_failed before a prompt"
    );
}

// ---- step 0 for the engine whose launch always carries a limit ---------------------------------------------------------

const NOT_PLACED: &str = "This run's command time limit of 600 seconds could not be placed for OpenCode. Commands that set no time limit of their own are stopped after 120 seconds.";
const NOT_PLACED_LOG: &str =
    "direct engine command limit not placed: the engine's own limit applies";

/// The module's startup for `engine` on an empty configuration directory, which holds no plugin: what it returned, the notices it
/// recorded, the engine's environment afterwards and the log.
async fn startup_without_a_placement(
    engine: &str,
    members: Option<Value>,
) -> (
    Result<(), String>,
    Vec<Value>,
    std::collections::BTreeMap<String, String>,
    Vec<String>,
) {
    use agentd::direct::engine_config::StartParts;
    use agentd::direct::hooks::HookEnv;
    use agentd::events::{EventRecorder, TurnControls};
    let mut d = Direct::start(Opts::default()).await;
    if let Some(members) = members {
        with_members(&mut d, members);
    }
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv {
        engine: engine.to_string(),
        ..HookEnv::new(&d.app, &d.req, &rec, &controls)
    };
    let request = serde_json::from_slice(&d.request_json).unwrap();
    let root = common::scratch("limit-startup");
    let (config_dir, data_dir) = (root.join("config"), root.join("data"));
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut engine_env =
        std::collections::BTreeMap::from([("HAMH_WAIT_S".to_string(), "240".to_string())]);
    let result = EngineConfigHook::new().start_with(StartParts {
        env: &env,
        request: &request,
        config_dir: &config_dir,
        data_dir: &data_dir,
        argv: &[],
        engine_env: &mut engine_env,
    });
    let notices = rec
        .drain()
        .into_iter()
        .filter_map(|e| match e.payload {
            harness_protocol::envelope::Payload::Inline { value } if value["kind"] == "notify" => {
                Some(value)
            }
            _ => None,
        })
        .collect();
    (result, notices, engine_env, d.log.lines())
}

#[tokio::test(flavor = "multi_thread")]
async fn b800_the_module_tells_a_default_that_cannot_be_placed_and_lowers_the_wait() {
    // No engine settings and no plugin where the engine reads it: one notice, one log line, a wait that fits the engine's own limit, and
    // the startup goes on.
    let (result, notices, engine_env, log) = startup_without_a_placement("opencode", None).await;
    assert_eq!(result, Ok(()));
    assert_eq!(
        notices,
        [
            json!({"turn": 1, "kind": "notify", "reason": "command_limit", "outageMinutes": null, "detail": NOT_PLACED})
        ]
    );
    assert_eq!(engine_env["HAMH_WAIT_S"], "108");
    assert_eq!(
        log.iter().filter(|l| l.as_str() == NOT_PLACED_LOG).count(),
        1,
        "{log:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b800_the_module_ends_the_startup_for_a_users_limit_and_says_nothing_for_the_first_engine()
{
    // The user's own limit that cannot be read back: the startup ends, nothing is told and the wait stays.
    let users = json!({"engineConfig": {"engine": "opencode", "schema": 1, "settings": {"commands": {"timeout_seconds": 900}}}});
    let (result, notices, engine_env, log) =
        startup_without_a_placement("opencode", Some(users)).await;
    assert_eq!(
        result,
        Err(
            "engine settings: the engine's configuration does not hold the command time limit"
                .to_string()
        )
    );
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(engine_env["HAMH_WAIT_S"], "240");
    assert!(!log.iter().any(|l| l == NOT_PLACED_LOG));
    // The control: the engine that carries a limit only when the user set one has nothing to read back here.
    let (result, notices, engine_env, log) = startup_without_a_placement("cline", None).await;
    assert_eq!(result, Ok(()));
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(engine_env["HAMH_WAIT_S"], "240");
    assert!(!log.iter().any(|l| l == NOT_PLACED_LOG));
}
