//! Installing a policy in the calling process, behind a trait so a test can inject what the kernel says.
//!
//! The real installer asks for every right and scope of Landlock ABI 6 with the crate's hard-requirement level: a kernel that cannot give one of
//! them fails the call and no engine starts. A rule that cannot be added (a missing required path) fails the call the same way.

use super::launch::SandboxError;
use super::policy::{Grant, MIN_ABI, Policy};
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, LandlockStatus, NetPort,
    PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetStatus,
    Scope,
};
use std::collections::BTreeSet;

/// The ABI whose rights and scopes the policy handles: a kernel below it cannot enforce the policy.
const ABI_REQUIRED: ABI = ABI::V6;

/// What the kernel reported after the domain was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub fully_enforced: bool,
    pub abi: u8,
    pub no_new_privs: bool,
}

pub trait Enforcer {
    /// Installs `policy` on the calling process and says what the kernel enforced.
    fn enforce(&self, policy: &Policy) -> Result<Report, SandboxError>;

    /// Whether a ruleset of the required ABI can be made at all (nothing is restricted).
    fn preflight(&self) -> Result<(), SandboxError>;

    /// Installs the seccomp filter that denies SysV shared memory, message queues and semaphores to this process and what it starts.
    fn deny_sysv_ipc(&self) -> Result<(), SandboxError>;

    /// Installs the seccomp filter with the given System V rule. The default serves an enforcer that knows only the deny rule.
    fn install_filter(&self, sysv: super::ipc::SysvIpc) -> Result<(), SandboxError> {
        match sysv {
            super::ipc::SysvIpc::Deny => self.deny_sysv_ipc(),
            super::ipc::SysvIpc::AllowedInPrivateNamespace => Err(SandboxError::Enforce {
                stage: "seccomp".to_string(),
                detail: "this enforcer cannot install the filter for a private IPC namespace"
                    .to_string(),
            }),
        }
    }
}

/// The check every report passes before the engine is started.
pub fn verify(report: &Report, policy: &Policy) -> Result<(), SandboxError> {
    if !report.fully_enforced {
        return Err(SandboxError::NotFullyEnforced);
    }
    let need = policy.min_abi.max(MIN_ABI);
    if report.abi < need {
        return Err(SandboxError::AbiTooLow {
            found: report.abi,
            need,
        });
    }
    if !report.no_new_privs {
        return Err(SandboxError::NoNewPrivs);
    }
    Ok(())
}

fn fail(stage: &str, detail: impl std::fmt::Display) -> SandboxError {
    SandboxError::Enforce {
        stage: stage.to_string(),
        detail: detail.to_string(),
    }
}

/// The kernel's Landlock through the `landlock` crate.
#[derive(Clone, Copy, Debug, Default)]
pub struct LandlockEnforcer;

impl LandlockEnforcer {
    fn ruleset() -> Result<Ruleset, SandboxError> {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI_REQUIRED))
            .map_err(|e| fail("handle file rights", e))?
            .handle_access(AccessNet::from_all(ABI_REQUIRED))
            .map_err(|e| fail("handle network rights", e))?
            .scope(Scope::from_all(ABI_REQUIRED))
            .map_err(|e| fail("scope", e))
    }
}

/// One path of a grant with `access`; the rights that only a folder can have are dropped for a file. A missing optional path is skipped.
fn add_path(
    rs: RulesetCreated,
    grant: &Grant,
    access: BitFlags<AccessFs>,
) -> Result<RulesetCreated, SandboxError> {
    let fd = match PathFd::new(&grant.path) {
        Ok(fd) => fd,
        Err(_) if grant.optional && !grant.path.exists() => return Ok(rs),
        Err(e) => return Err(fail("add_rule", format!("{}: {e}", grant.path.display()))),
    };
    let is_dir = std::fs::metadata(&grant.path).is_ok_and(|m| m.is_dir());
    let access = if is_dir {
        access
    } else {
        access & AccessFs::from_file(ABI_REQUIRED)
    };
    if access.is_empty() {
        return Ok(rs);
    }
    rs.add_rule(PathBeneath::new(fd, access))
        .map_err(|e| fail("add_rule", format!("{}: {e}", grant.path.display())))
}

impl Enforcer for LandlockEnforcer {
    fn enforce(&self, policy: &Policy) -> Result<Report, SandboxError> {
        policy.validate()?;
        let mut rs = Self::ruleset()?.create().map_err(|e| fail("create", e))?;
        let all = AccessFs::from_all(ABI_REQUIRED);
        let read_write = all & !(AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::IoctlDev);
        let read_exec = AccessFs::from_read(ABI_REQUIRED);
        let read_only = AccessFs::ReadFile | AccessFs::ReadDir;
        let device =
            AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate | AccessFs::IoctlDev;
        for (grants, access) in [
            (&policy.read_write, read_write),
            (&policy.read_exec, read_exec),
            (&policy.read_only, read_only),
            (&policy.devices, device),
        ] {
            for g in grants {
                rs = add_path(rs, g, access)?;
            }
        }
        // Every TCP port is open except the denied ones; Landlock names ports one by one, and port 0 is the ephemeral range for a bind.
        let no_connect: BTreeSet<u16> = policy.deny_connect.iter().copied().collect();
        let no_bind: BTreeSet<u16> = policy.deny_bind.iter().copied().collect();
        for port in 0..=u16::MAX {
            let mut access = BitFlags::<AccessNet>::empty();
            if port != 0 && !no_connect.contains(&port) {
                access |= AccessNet::ConnectTcp;
            }
            if !no_bind.contains(&port) {
                access |= AccessNet::BindTcp;
            }
            if !access.is_empty() {
                rs = rs
                    .add_rule(NetPort::new(port, access))
                    .map_err(|e| fail("add_rule", format!("port {port}: {e}")))?;
            }
        }
        let status = rs.restrict_self().map_err(|e| fail("restrict_self", e))?;
        let abi = match status.landlock {
            LandlockStatus::Available {
                effective_abi,
                kernel_abi,
            } => kernel_abi.unwrap_or(effective_abi as i32),
            _ => 0,
        };
        Ok(Report {
            fully_enforced: status.ruleset == RulesetStatus::FullyEnforced,
            abi: u8::try_from(abi.clamp(0, i32::from(u8::MAX))).unwrap_or(0),
            no_new_privs: status.no_new_privs,
        })
    }

    fn preflight(&self) -> Result<(), SandboxError> {
        Self::ruleset()?
            .create()
            .map(|_| ())
            .map_err(|e| fail("create", e))
    }

    fn deny_sysv_ipc(&self) -> Result<(), SandboxError> {
        super::ipc::install()
    }

    fn install_filter(&self, sysv: super::ipc::SysvIpc) -> Result<(), SandboxError> {
        super::ipc::install_with(sysv)
    }
}
