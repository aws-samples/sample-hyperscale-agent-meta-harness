//! `agentd sandbox-exec --policy-json <json> -- <program> <args>`: applies the policy, then replaces itself with the program.
//!
//! agentd starts the engine through this subcommand in the `landlock` mode. The launcher is a fresh process (a single thread, nothing loaded
//! but the policy), so it does what a `pre_exec` hook in the multithreaded daemon could not do safely. It closes the descriptors it inherited
//! (an open file, socket or directory handle stays usable inside a Landlock domain), and it refuses, with its own exit code, whenever the whole
//! policy is not in force; the program then never starts.

use super::enforce::{Enforcer, LandlockEnforcer, verify};
use super::identity::{self, NsSource, ProcNs};
use super::ipc::SysvIpc;
use super::policy::Policy;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The exit code of a launcher that refused: a start that did not happen, not an engine that ran.
pub const REFUSED_EXIT: u8 = 125;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    #[error("usage: {0}")]
    Usage(String),
    #[error("the policy is not valid: {0}")]
    Policy(String),
    #[error(
        "file descriptor {0} is open and could not be closed: only standard input, output and error may be inherited"
    )]
    InheritedFd(i32),
    #[error("Landlock could not be installed ({stage}): {detail}")]
    Enforce { stage: String, detail: String },
    #[error("Landlock ABI {found} is below the {need} the policy requires")]
    AbiTooLow { found: u8, need: u8 },
    #[error("Landlock enforced only part of the policy")]
    NotFullyEnforced,
    #[error("no_new_privs is not set")]
    NoNewPrivs,
    #[error("the program {0} is not an absolute path")]
    Program(String),
    #[error("the nobody route needs the engine in its own {0} namespace, and it is in agentd's")]
    SharedNamespace(String),
    #[error(
        "the nobody route needs the engine to read as the overflow user {want}, and it reads as user {got}"
    )]
    NotNobody { got: u32, want: u32 },
    #[error("the nobody helper refused: {0}")]
    Helper(String),
    #[error("exec of {0} failed: {1}")]
    Exec(String, String),
}

/// Replaces the process with the program. The real one never returns on success; a test's records the call and returns `Ok`.
pub trait Execer {
    fn exec(&self, program: &Path, args: &[String]) -> Result<(), SandboxError>;
}

/// The file descriptors this process has open, and the means to close one.
pub trait FdSource {
    fn open_fds(&self) -> Result<Vec<i32>, SandboxError>;
    fn close(&self, fd: i32) -> Result<(), SandboxError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealExecer;

impl Execer for RealExecer {
    fn exec(&self, program: &Path, args: &[String]) -> Result<(), SandboxError> {
        use std::os::unix::process::CommandExt;
        let e = std::process::Command::new(program).args(args).exec();
        Err(SandboxError::Exec(
            program.display().to_string(),
            e.to_string(),
        ))
    }
}

/// The real table of descriptors, from `/proc/self/fd` (the handle of that listing itself is left out).
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcFds;

impl FdSource for ProcFds {
    fn open_fds(&self) -> Result<Vec<i32>, SandboxError> {
        let no = |e: std::io::Error| {
            SandboxError::Usage(format!("the descriptors could not be listed: {e}"))
        };
        let own = std::fs::canonicalize("/proc/self/fd").map_err(no)?;
        let mut fds = Vec::new();
        for entry in std::fs::read_dir("/proc/self/fd").map_err(no)? {
            let entry = entry.map_err(no)?;
            if std::fs::read_link(entry.path()).is_ok_and(|t| t == own) {
                continue;
            }
            if let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            {
                fds.push(fd);
            }
        }
        fds.sort_unstable();
        Ok(fds)
    }

    fn close(&self, fd: i32) -> Result<(), SandboxError> {
        match nix::unistd::close(fd) {
            Ok(()) | Err(nix::errno::Errno::EBADF) => Ok(()),
            Err(_) => Err(SandboxError::InheritedFd(fd)),
        }
    }
}

/// The policy, the program and its arguments from the words after `sandbox-exec`.
pub fn parse_args(args: &[String]) -> Result<(Policy, PathBuf, Vec<String>), SandboxError> {
    let usage = |t: &str| {
        SandboxError::Usage(format!(
            "{t}; agentd sandbox-exec --policy-json <json> -- <program> [args]"
        ))
    };
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("--policy-json") {
        return Err(usage("the first argument must be --policy-json"));
    }
    let json = it
        .next()
        .ok_or_else(|| usage("--policy-json needs the policy"))?;
    let policy = Policy::from_json(json)?;
    if it.next().map(String::as_str) != Some("--") {
        return Err(usage("the policy must be followed by --"));
    }
    let program = it.next().ok_or_else(|| usage("no program follows --"))?;
    Ok((policy, PathBuf::from(program), it.cloned().collect()))
}

/// Closes every descriptor above 2 (Landlock does not take back an open file, socket or directory handle, so one left open stays usable inside the
/// domain), then looks again: a descriptor that could not be closed, or that is still there, ends the launch.
fn close_inherited(fds: &dyn FdSource) -> Result<(), SandboxError> {
    let inherited: Vec<i32> = fds.open_fds()?.into_iter().filter(|fd| *fd > 2).collect();
    if inherited.is_empty() {
        return Ok(());
    }
    for fd in inherited {
        fds.close(fd)?;
    }
    match fds.open_fds()?.into_iter().find(|fd| *fd > 2) {
        Some(fd) => Err(SandboxError::InheritedFd(fd)),
        None => Ok(()),
    }
}

/// The whole launch with its collaborators given: `Ok` means the program would have been executed. The order is fixed: the arguments, the
/// descriptors (closed), the nobody route's namespaces (when the policy has one), the domain, the check of what the kernel enforced, the SysV IPC
/// filter, then the program.
pub fn run(
    args: &[String],
    enforcer: &dyn Enforcer,
    fds: &dyn FdSource,
    execer: &dyn Execer,
) -> Result<(), SandboxError> {
    run_with(args, enforcer, fds, execer, &ProcNs)
}

/// [`run`] with the source of the process's own namespaces and user given as well.
pub fn run_with(
    args: &[String],
    enforcer: &dyn Enforcer,
    fds: &dyn FdSource,
    execer: &dyn Execer,
    ns: &dyn NsSource,
) -> Result<(), SandboxError> {
    let (policy, program, rest) = parse_args(args)?;
    if !program.is_absolute() {
        return Err(SandboxError::Program(program.display().to_string()));
    }
    close_inherited(fds)?;
    // The route's promise is a private IPC namespace and the overflow user; a launcher that finds itself elsewhere starts nothing, and the System V
    // calls stay denied for any launch without the route.
    let sysv = match &policy.nobody {
        Some(route) => {
            identity::check(route, ns)?;
            SysvIpc::AllowedInPrivateNamespace
        }
        None => SysvIpc::Deny,
    };
    let report = enforcer.enforce(&policy)?;
    verify(&report, &policy)?;
    if policy.deny_sysv_ipc {
        enforcer.install_filter(sysv)?;
    }
    execer.exec(&program, &rest)
}

/// The real launcher; returns only when the program was not started.
pub fn main_sandbox_exec(args: &[String]) -> ExitCode {
    match run(args, &LandlockEnforcer, &ProcFds, &RealExecer) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentd sandbox-exec: refused: {e}");
            ExitCode::from(REFUSED_EXIT)
        }
    }
}
