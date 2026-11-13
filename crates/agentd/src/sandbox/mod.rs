//! How the engine is kept away from agentd's secrets.
//!
//! The microVM runs agentd as root and the engine as a second user (`setpriv`); the Instances container has no capabilities, so there is no
//! second user. There the engine runs in a Landlock domain and agentd is non-dumpable. The deployment picks the mode with
//! `AGENTD_ENGINE_SANDBOX` (`uid` or `landlock`); an unset or unknown value means no engine process.

pub mod enforce;
pub mod identity;
pub mod ipc;
pub mod launch;
pub mod mode;
pub mod policy;
pub mod tool_env;

pub use enforce::{Enforcer, LandlockEnforcer, Report, verify};
pub use identity::{
    HelperCheck, IDENTITY_VAR, Identity, IdentityPreflight, IdentitySetting, NamespaceIds,
    NsSource, ProcNs, check as check_nobody_route, route_of_this_process, word_of_user,
};
pub use ipc::SysvIpc;
pub use launch::{
    Execer, FdSource, ProcFds, REFUSED_EXIT, RealExecer, SandboxError, main_sandbox_exec,
    parse_args, run, run_with,
};
pub use mode::{
    ENV_VAR, Mode, Setting, note_process_mode, ownership_changes_allowed, set_non_dumpable,
};
pub use policy::{Grant, MIN_ABI, NobodyRoute, Policy, SHARED_TMP};
pub use tool_env::tool_env;

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A process that starts in a Landlock domain: the launcher binary (agentd itself) and the policy it applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub launcher: PathBuf,
    pub policy: Arc<Policy>,
    /// `ham-nobody-exec`, started in front of the launcher on the nobody route; it and `policy.nobody` are set together.
    pub identity_helper: Option<PathBuf>,
}

impl Sandbox {
    /// `argv` (its program resolved to an absolute path) behind the launcher.
    pub fn wrap(&self, argv: Vec<String>) -> std::io::Result<Vec<String>> {
        let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
        let program = argv
            .first()
            .ok_or_else(|| invalid("empty argv".to_string()))?;
        if !Path::new(program).is_absolute() {
            return Err(invalid(format!(
                "the engine program {program} does not resolve to an absolute path"
            )));
        }
        if !self.launcher.is_absolute() {
            return Err(invalid(format!(
                "the sandbox launcher {} is not an absolute path",
                self.launcher.display()
            )));
        }
        let mut out = Vec::new();
        match (&self.identity_helper, &self.policy.nobody) {
            (Some(helper), Some(_)) => {
                if !helper.is_absolute() {
                    return Err(invalid(format!(
                        "the nobody helper {} is not an absolute path",
                        helper.display()
                    )));
                }
                out.push(helper.display().to_string());
                out.push("--".to_string());
            }
            (Some(_), None) => {
                return Err(invalid(
                    "a nobody helper is set and the policy has no nobody route".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(invalid(
                    "the policy names the nobody route and no helper starts it".to_string(),
                ));
            }
            (None, None) => {}
        }
        out.extend([
            self.launcher.display().to_string(),
            "sandbox-exec".to_string(),
            "--policy-json".to_string(),
            self.policy.to_json(),
            "--".to_string(),
        ]);
        out.extend(argv);
        Ok(out)
    }
}

/// The question the driver asks before it fetches or places anything for a run in the `landlock` mode: can this kernel give the domain at all.
pub trait Preflight: Send + Sync {
    fn check(&self) -> Result<(), SandboxError>;
}

impl Preflight for LandlockEnforcer {
    fn check(&self) -> Result<(), SandboxError> {
        Enforcer::preflight(self)
    }
}

impl Sandbox {
    /// The same system grants for another family of processes whose folders are `writable` (the report steps run in a folder of their own): what
    /// the run's launch added (its writable and its required read-only folders, its private paths) is dropped and `writable` takes its place; the shared
    /// scratch directories are granted again.
    pub fn rebased(&self, writable: &[&Path]) -> Sandbox {
        let mut policy = (*self.policy).clone();
        policy.read_write.clear();
        policy.private.clear();
        policy.read_only.retain(|g| g.optional);
        for p in writable {
            policy = policy.with_read_write(p);
        }
        // The scratch directories belong to every family of processes of the run, the steps' included.
        let (policy, _) = policy.with_shared_tmp();
        Sandbox {
            launcher: self.launcher.clone(),
            policy: Arc::new(policy),
            identity_helper: self.identity_helper.clone(),
        }
    }
}
