//! Support for the tests of the tool hook: a rig that holds what `on_startup` is given (a hook environment, the run's
//! directories, the real `[mcp]` table of the `cline` profile and an engine environment) so that a test can start the module, feed it the
//! driver's events and read the engine's registration file back, without a whole run. The whole runs through the fake engine are in
//! `direct_tools_mcp_run.rs`.
#![allow(dead_code, clippy::unwrap_used)]

use crate::common::{repo_root, scratch};
use crate::direct_support::{Direct, Opts, RUN};
use agentd::direct::hooks::{
    CleanLaunch, EngineStop, Event, HookEnv, PromptSent, SessionEnd, SessionEnded, SessionStarted,
};
use agentd::direct::tools::StartParts;
use agentd::direct::tools::ToolsHook;
use agentd::direct::tools::backend::ToolBackend;
use agentd::events::{EventRecorder, TurnControls};
use harness_core::engine_profile::load_profile;
use harness_protocol::engine_profile::EngineMcp;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The file name Cline reads its tool servers from, under `{dataDir}/settings/`.
pub const FILE: &str = "cline_mcp_settings.json";

/// The `[mcp]` table of the real `cline` profile: the path, the template and the cap the product ships.
pub fn real_mcp() -> EngineMcp {
    load_profile(&repo_root().join("engine/profiles/cline.toml"))
        .unwrap()
        .mcp
        .expect("cline.toml carries the [mcp] table")
}

pub struct Rig {
    pub d: Direct,
    pub env: HookEnv,
    pub repo: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    /// Where the profile's template is read from (the repository, as in the tests of the whole driver).
    pub templates_root: PathBuf,
    pub engine_env: BTreeMap<String, String>,
    pub mcp: EngineMcp,
    pub owner: Option<(u32, u32)>,
    /// The environment the engine would hold; the clean launch is made from it.
    pub launch: CleanLaunch,
}

impl Rig {
    /// A rig whose data directory already holds `settings/` (the provider file lives there), as it does when the hook runs.
    pub async fn new() -> Self {
        Self::with_owner(None).await
    }

    pub async fn with_owner(owner: Option<(u32, u32)>) -> Self {
        let d = Direct::start(Opts::default()).await;
        let rec = EventRecorder::new(RUN, "direct");
        let (controls, _) = TurnControls::new();
        let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
        let root = scratch("hook");
        let repo = root.join("repo");
        let data_dir = root.join("data");
        let config_dir = root.join("config");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(data_dir.join("settings")).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(data_dir.join("settings/providers.json"), "{}\n").unwrap();
        let mut engine_env = BTreeMap::new();
        for (k, v) in [
            ("HOME", "/home/agent"),
            ("TMPDIR", "/tmp/agent"),
            ("USER", "agent"),
            ("AWS_CONFIG_FILE", "/x/aws/config"),
            ("AWS_PROFILE", "harness"),
            ("CLINE_MODEL", "some-model"),
            ("HAMH_CONTEXT_DIR", "/ctx"),
        ] {
            engine_env.insert(k.to_string(), v.to_string());
        }
        let launch = CleanLaunch::new(
            &engine_env,
            repo.clone(),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
            data_dir.clone(),
            owner,
        );
        Self {
            d,
            env,
            repo,
            data_dir,
            config_dir,
            templates_root: repo_root(),
            engine_env,
            mcp: real_mcp(),
            owner,
            launch,
        }
    }

    pub fn parts(&mut self) -> StartParts<'_> {
        StartParts {
            env: &self.env,
            repo: &self.repo,
            data_dir: &self.data_dir,
            config_dir: &self.config_dir,
            templates_root: &self.templates_root,
            engine_uid: self.owner,
            mcp: Some(&self.mcp),
            launch: self.launch.clone(),
            engine_env: &mut self.engine_env,
        }
    }

    pub fn set_templates_root(&mut self, root: &Path) {
        self.templates_root = root.to_path_buf();
    }

    /// Starts `hook` with these backends.
    pub async fn start(&mut self, hook: &ToolsHook, backends: Vec<Box<dyn ToolBackend>>) {
        let parts = self.parts();
        hook.start_with(parts, backends).await;
    }

    /// The registration file.
    pub fn file(&self) -> PathBuf {
        self.data_dir.join("settings").join(FILE)
    }

    pub fn settings_dir(&self) -> PathBuf {
        self.data_dir.join("settings")
    }

    pub fn file_text(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap()
    }

    /// The port the registration file names.
    pub fn file_port(&self) -> u16 {
        let v: serde_json::Value = serde_json::from_str(&self.file_text()).unwrap();
        let url = v["mcpServers"]["ham"]["transport"]["url"].as_str().unwrap();
        url.strip_prefix("http://127.0.0.1:")
            .and_then(|r| r.strip_suffix("/mcp"))
            .unwrap_or_else(|| panic!("not a loopback url: {url}"))
            .parse()
            .unwrap()
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.file_port()))
    }

    /// Every log line the module wrote.
    pub fn tools_log(&self) -> Vec<String> {
        self.d
            .log
            .lines()
            .into_iter()
            .filter(|l| l.starts_with("direct tools"))
            .collect()
    }
}

// ---- the driver's events, as a test raises them ---------------------------------------------------------------------------------

pub fn session_started(hook: &ToolsHook, id: &str, number: u32) {
    let response = serde_json::json!({"sessionId": id});
    hook_event(
        hook,
        &Event::SessionStarted(SessionStarted {
            session_id: id,
            number,
            response: &response,
            cwd: "/repo",
            trigger: None,
        }),
    );
}

pub fn prompt_sent(hook: &ToolsHook, id: &str, no: u32) {
    hook_event(
        hook,
        &Event::PromptSent(PromptSent {
            no,
            session_id: id,
            text: "go",
        }),
    );
}

pub fn session_ended(hook: &ToolsHook, id: &str, number: u32, why: SessionEnd) {
    hook_event(
        hook,
        &Event::SessionEnded(SessionEnded {
            session_id: id,
            number,
            why,
        }),
    );
}

pub fn engine_stopped(hook: &ToolsHook, why: EngineStop) {
    hook_event(hook, &Event::EngineStopped(why));
}

fn hook_event(hook: &ToolsHook, e: &Event<'_>) {
    use agentd::direct::hooks::DirectHook;
    hook.on_event(e);
}

/// A shared handle for tests that move the hook into a task.
pub fn shared(hook: ToolsHook) -> Arc<ToolsHook> {
    Arc::new(hook)
}

pub fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}
