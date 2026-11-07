//! Engine configuration written before launch: the AWS `agent` profile whose `credential_process`
//! is `agentd creds`, the built-in configs of the engines agentd knows by name (M2F and
//! M2 working setups; `crate::engine::config`), and a profile's own `configEmitters` rendered from
//! templates.

use harness_protocol::engine_profile::EngineProfile;
use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmitContext {
    pub engine: String,
    pub model: String,
    pub region: String,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub agentd_exe: PathBuf,
    pub socket: PathBuf,
    /// The run's output-token cap.
    pub max_output_tokens: Option<u32>,
    /// The model's context window for this run: written to the engine's provider settings so its own
    /// catalog cannot decide it. `None` (or 0) leaves the emitted files byte for byte as they were.
    pub context_tokens: Option<u32>,
    /// The user's compaction settings (protocol 1.39.0, `context.compaction` and `context.compaction_trigger`). `None` leaves the emitted files
    /// byte for byte as they were.
    pub compaction: Option<crate::engine::user_settings::CompactionBlock>,
}

impl EmitContext {
    /// The window to write, if the turn sets a real one.
    pub(crate) fn window(&self) -> Option<u32> {
        self.context_tokens.filter(|n| *n > 0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Emitted {
    pub files: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
}

pub(crate) fn write(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

pub(crate) fn pretty(v: &serde_json::Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

/// `<configDir>/aws/config` with `[profile agent]`.
pub fn emit_aws_config(ctx: &EmitContext) -> io::Result<PathBuf> {
    let path = ctx.config_dir.join("aws/config");
    write(
        &path,
        &format!(
            "[profile agent]\nregion = {}\ncredential_process = {} creds --socket {}\n",
            ctx.region,
            ctx.agentd_exe.display(),
            ctx.socket.display()
        ),
    )?;
    Ok(path)
}

/// The engine's AWS environment: the agent profile only, never raw keys or a bearer token.
pub fn aws_env(ctx: &EmitContext, config: &Path) -> BTreeMap<String, String> {
    [
        ("AWS_CONFIG_FILE", config.display().to_string()),
        (
            "AWS_SHARED_CREDENTIALS_FILE",
            ctx.config_dir
                .join("aws/no-shared-credentials")
                .display()
                .to_string(),
        ),
        ("AWS_PROFILE", "agent".to_string()),
        ("AWS_REGION", ctx.region.clone()),
        ("AWS_DEFAULT_REGION", ctx.region.clone()),
        ("AWS_EC2_METADATA_DISABLED", "true".to_string()),
        ("AWS_SDK_LOAD_CONFIG", "1".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// The value an engine's ACP `model` option takes for a catalog model id.
pub fn acp_model_value(engine: &str, model: &str) -> String {
    crate::engine::config::acp_model_value(engine, model)
}

/// Built-in config of an engine agentd knows by name (`crate::engine::config`); others get none.
pub fn emit_engine(ctx: &EmitContext) -> io::Result<Emitted> {
    crate::engine::config::emit(ctx)
}

/// Replaces `{name}` for every name in `vars`; other braces stay as written.
pub fn render(template: &str, vars: &BTreeMap<&str, String>) -> String {
    vars.iter().fold(template.to_string(), |acc, (k, v)| {
        acc.replace(&format!("{{{k}}}"), v)
    })
}

/// Renders a profile's `configEmitters`; templates must stay inside `templates_root`.
pub fn emit_profile(
    profile: &EngineProfile,
    templates_root: &Path,
    vars: &BTreeMap<&str, String>,
) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for e in &profile.config_emitters {
        let rel = Path::new(&e.template);
        if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("template {} leaves the templates root", e.template),
            ));
        }
        let text = std::fs::read_to_string(templates_root.join(rel))?;
        let path = PathBuf::from(render(&e.path, vars));
        write(&path, &render(&text, vars))?;
        out.push(path);
    }
    Ok(out)
}
