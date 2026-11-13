//! Engine process: argv and env from the profile, a clean environment, a new process group, and an
//! optional uid split (`setpriv` to the engine user when agentd runs as root).
//!
//! The engine-specific code of agentd lives here and nowhere else in `src`:
//! `config` (built-in config files and the ACP model value), `opencode` (the OpenCode arm: its session configuration, launch additions and defaults), `watchdog` (the tool-idle limit each engine starts with), `goal` (the goal plugin the loop of a run uses and the files it keeps), `layout` (where the harness's files for an engine go, how that place is protected and which kill switches the launch carries), `direct` (what an engine says
//! about its rules, its launch settings and its configuration directory in a direct run), `direct_words` (the plain words of the
//! user correction path that name the engine) and `faults` (the text an
//! engine gives for its own timeout), `tools` (the tool names and the title convention of each engine) and `user_settings` (the user's engine settings of protocol 1.39.0, typed, and where each one is placed). The generic modules ask these by engine name.
//!
//! The engine process's end is described here too, and the engine-neutral modules ask for it: `exit` (how the process ended,
//! in words) and `stderr` (the rotated stderr log and agentd's own handle on it), for the failed turn's error text.

pub mod config;
pub mod direct;
pub mod direct_words;
pub mod exit;
pub mod faults;
pub mod goal;
pub mod layout;
pub mod opencode;
pub mod stderr;
pub mod tools;
pub mod user_settings;
pub mod watchdog;

pub use exit::{Exit, describe_exit, exit_of};
pub use stderr::{RawTail, StderrCapture, prev_stderr_path};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Directories searched for `argv[0]`; also the engine's `PATH`.
    pub path: Vec<PathBuf>,
    /// Engine stderr, on local disk (never agentd's log).
    pub stderr_log: PathBuf,
    /// `(uid, gid)` to run as, through `setpriv`.
    pub run_as: Option<(u32, u32)>,
    /// The Landlock sandbox to start the process in, through `agentd sandbox-exec`. Never set together with `run_as`.
    pub sandbox: Option<crate::sandbox::Sandbox>,
}

/// `argv[0]` resolved against the search path when it has no slash.
pub fn resolve_program(program: &str, path: &[PathBuf]) -> PathBuf {
    if program.contains('/') {
        return PathBuf::from(program);
    }
    path.iter()
        .map(|d| d.join(program))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(program))
}

/// The argv actually executed: `argv[0]` resolved, wrapped in `setpriv` for a uid split or in `agentd sandbox-exec` for a Landlock sandbox.
pub fn command_argv(spec: &LaunchSpec) -> std::io::Result<Vec<String>> {
    let first = spec
        .argv
        .first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut argv: Vec<String> = vec![resolve_program(first, &spec.path).display().to_string()];
    argv.extend(spec.argv.iter().skip(1).cloned());
    if let Some(sandbox) = &spec.sandbox {
        if spec.run_as.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a launch has both a uid split and a sandbox: the deployment picks one mode",
            ));
        }
        return sandbox.wrap(argv);
    }
    if let Some((uid, gid)) = spec.run_as {
        let mut wrapped = vec![
            resolve_program("setpriv", &spec.path).display().to_string(),
            "--reuid".into(),
            uid.to_string(),
            "--regid".into(),
            gid.to_string(),
            "--clear-groups".into(),
            "--no-new-privs".into(),
            "--".into(),
        ];
        wrapped.extend(argv);
        argv = wrapped;
    }
    Ok(argv)
}

/// Starts a process with its stderr in a new `stderr_log` that truncates the one there (a short-lived command; the engine
/// uses [`spawn_captured`]).
pub fn spawn(spec: &LaunchSpec) -> std::io::Result<Child> {
    let argv = command_argv(spec)?;
    if let Some(parent) = spec.stderr_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stderr = std::fs::File::create(&spec.stderr_log)?;
    launch(spec, &argv, stderr)
}

/// Starts the engine as [`spawn`] does, except that the previous `stderr_log` is kept as `engine-stderr.prev.log` (one
/// is kept) instead of truncated, and the new log is created without following a link. Returns the handle agentd reads the
/// end of the log through, whatever happens to the path while the engine runs (`stderr`). A start that fails before the
/// engine runs puts the previous log back, so it never takes the place of the last real run's words.
pub fn spawn_captured(spec: &LaunchSpec) -> std::io::Result<(Child, StderrCapture)> {
    let argv = command_argv(spec)?;
    if let Some(parent) = spec.stderr_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let moved = stderr::rotate(&spec.stderr_log);
    let started = stderr::open_fresh(&spec.stderr_log).and_then(|stderr| {
        let clone = stderr.try_clone()?;
        launch(spec, &argv, stderr).map(|child| (child, StderrCapture::new(clone)))
    });
    if started.is_err() && moved {
        // The engine never ran: its predecessor's log stays the log.
        stderr::restore_rotated(&spec.stderr_log);
    }
    started
}

fn launch(spec: &LaunchSpec, argv: &[String], stderr: std::fs::File) -> std::io::Result<Child> {
    let joined = std::env::join_paths(&spec.path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .env("PATH", joined)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .kill_on_drop(true);
    cmd.spawn()
}

/// SIGKILL to the engine's process group (its tools included).
pub async fn kill_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status()
            .await;
    }
    let _ = child.kill().await;
}

/// What the engine's user needs from the machine: who agentd is, and who the named user is. A seam so a test can describe a root agentd
/// without being one.
pub trait EngineUsers: Send + Sync {
    /// The effective uid of this process, or why it cannot be told.
    fn euid(&self) -> Result<u32, String>;
    /// `(uid, gid)` of `user`: `Ok(None)` when the machine has no such user, `Err` when the lookup itself failed.
    fn lookup<'a>(
        &'a self,
        user: &'a str,
    ) -> crate::BoxFuture<'a, Result<Option<(u32, u32)>, String>>;
}

/// The real machine: `geteuid(2)` and the `id` command.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemUsers;

impl EngineUsers for SystemUsers {
    fn euid(&self) -> Result<u32, String> {
        Ok(rustix::process::geteuid().as_raw())
    }

    fn lookup<'a>(
        &'a self,
        user: &'a str,
    ) -> crate::BoxFuture<'a, Result<Option<(u32, u32)>, String>> {
        Box::pin(async move {
            // `id` exits 1 and says so on stderr for a name the machine does not have; a command that cannot run at all is a failed lookup.
            let ask = |flag: &'static str| async move {
                let out = Command::new("id")
                    .args([flag, "--", user])
                    .output()
                    .await
                    .map_err(|e| format!("id could not run: {e}"))?;
                if !out.status.success() {
                    return Ok(None);
                }
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| "id printed something that is not a number".to_string())
            };
            match (ask("-u").await?, ask("-g").await?) {
                (Some(uid), Some(gid)) => Ok(Some((uid, gid))),
                _ => Ok(None),
            }
        })
    }
}

/// Who the engine runs as. `Ok(None)` is a start without a uid split, and only two cases give it: no engine user is named (the deployment
/// turned the split off), or agentd is known not to be root. Every other case in which a user is named and
/// the split cannot be set up is an error, so the engine is never started as root by accident:
/// agentd's own uid cannot be read, the lookup fails, the user does not exist, or the user is uid 0.
pub async fn resolve_engine_identity(
    users: &dyn EngineUsers,
    user: Option<&str>,
) -> Result<Option<(u32, u32)>, String> {
    let Some(user) = user.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let refuse = |why: String| {
        format!(
            "engine user {user:?} could not be resolved ({why}); agentd does not start the engine as root"
        )
    };
    let euid = users
        .euid()
        .map_err(|e| refuse(format!("agentd's own uid cannot be read: {e}")))?;
    if euid != 0 {
        return Ok(None);
    }
    match users.lookup(user).await {
        Err(e) => Err(refuse(format!("the lookup failed: {e}"))),
        Ok(None) => Err(refuse("no such user".to_string())),
        Ok(Some((0, _))) => Err(refuse("the user is uid 0".to_string())),
        Ok(Some(ids)) => Ok(Some(ids)),
    }
}

/// [`resolve_engine_identity`] on this machine.
pub async fn engine_identity(user: Option<&str>) -> Result<Option<(u32, u32)>, String> {
    resolve_engine_identity(&SystemUsers, user).await
}

/// Gives the engine user its directories (uid split only).
pub async fn chown_tree(dirs: &[&Path], owner: (u32, u32)) {
    for d in dirs {
        let _ = Command::new("chown")
            .args(["-R", &format!("{}:{}", owner.0, owner.1)])
            .arg(d)
            .status()
            .await;
    }
}
