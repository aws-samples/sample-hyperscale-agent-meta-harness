//! Who the engine is on the Instances machine.
//!
//! The container runs agentd as root and gives it no capability, so the engine is root too, and programs that refuse root fail (PostgreSQL's `initdb`, a setup
//! script that checks the user id). The container refuses `unshare`, `setns` and every write of `uid_map`, and allows `clone3` with new user and IPC namespaces.
//! The **nobody route** uses that: `images/worker-python/ham-nobody-exec` makes a child in new user and IPC namespaces with a group map and no user map, so
//! the child reads as the overflow user (65534) and holds no capability, the kernel still knows it as agentd's user (what it writes stays readable by agentd),
//! and its IPC namespace is its own. The launcher then checks what it was started in and installs the filter that lets System V IPC through.
//!
//! `AGENTD_ENGINE_IDENTITY` turns the route off: `nobody` (the default of the `landlock` mode) or `agentd` (the engine runs as agentd's own user, as it did
//! before the route). The `uid` mode does not read it.

use super::launch::SandboxError;
use super::policy::NobodyRoute;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The environment variable that names the engine's identity in the `landlock` mode.
pub const IDENTITY_VAR: &str = "AGENTD_ENGINE_IDENTITY";

/// The user id the kernel shows for an id that has no mapping, when `/proc/sys/kernel/overflowuid` cannot be read.
pub const OVERFLOW_UID: u32 = 65534;

/// How long the helper's own check may take.
const HELPER_CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// Who the engine runs as in the `landlock` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    /// The overflow user, in user and IPC namespaces of its own (the nobody route).
    Nobody,
    /// agentd's own user (root in the Instances container), in agentd's namespaces.
    Agentd,
}

impl Identity {
    /// The word the run summary records.
    pub fn word(self) -> &'static str {
        match self {
            Identity::Nobody => "nobody",
            Identity::Agentd => "agentd",
        }
    }
}

/// What `AGENTD_ENGINE_IDENTITY` said. A value that is not one of the two words is kept with the reason, so the run can end with it: opting out of the
/// default by a typo must not silently keep the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentitySetting {
    Identity(Identity),
    Refused(String),
}

impl Default for IdentitySetting {
    fn default() -> Self {
        IdentitySetting::Identity(Identity::Nobody)
    }
}

impl IdentitySetting {
    /// The setting for the text of the variable: unset or blank is the default (`nobody`); `nobody` and `agentd` in lower case are the two words.
    pub fn from_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") | Some("nobody") => IdentitySetting::Identity(Identity::Nobody),
            Some("agentd") => IdentitySetting::Identity(Identity::Agentd),
            Some(other) => IdentitySetting::Refused(format!(
                "{IDENTITY_VAR}={other:?} is not an identity; the deployment must leave it unset or set it to nobody or agentd"
            )),
        }
    }

    /// The identity, or why the engine must not start.
    pub fn resolve(&self) -> Result<Identity, String> {
        match self {
            IdentitySetting::Identity(i) => Ok(*i),
            IdentitySetting::Refused(why) => Err(why.clone()),
        }
    }
}

/// The most characters of the summary's identity word (`harness_protocol::direct::MAX_ENGINE_IDENTITY_CHARS`).
const WORD_MAX: usize = 32;

/// The summary word for the engine user a deployment named: the summary accepts 1 to 32 lower-case letters, digits, `-` and `_`, so the name is lower-cased, any
/// other character becomes `_`, and it is cut to 32; an empty name is `agentd`.
pub fn word_of_user(name: &str) -> String {
    let word: String = name
        .trim()
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            l if l.is_ascii_lowercase() || l.is_ascii_digit() || l == '-' || l == '_' => l,
            _ => '_',
        })
        .take(WORD_MAX)
        .collect();
    if word.is_empty() {
        Identity::Agentd.word().to_string()
    } else {
        word
    }
}

/// The names of a process's user and IPC namespaces, as `readlink /proc/self/ns/<kind>` prints them (`ipc:[4026532731]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceIds {
    pub ipc: String,
    pub user: String,
}

/// What the launcher asks about itself; a test gives it a fake.
pub trait NsSource {
    fn namespaces(&self) -> Result<NamespaceIds, SandboxError>;
    /// The effective user id the process reads.
    fn euid(&self) -> u32;
    /// The id the kernel shows for an unmapped user.
    fn overflow_uid(&self) -> u32;
}

/// The real `/proc` of this process.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcNs;

impl NsSource for ProcNs {
    fn namespaces(&self) -> Result<NamespaceIds, SandboxError> {
        let read = |kind: &str| {
            std::fs::read_link(format!("/proc/self/ns/{kind}"))
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|e| {
                    SandboxError::Usage(format!("the {kind} namespace could not be read: {e}"))
                })
        };
        Ok(NamespaceIds {
            ipc: read("ipc")?,
            user: read("user")?,
        })
    }

    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn overflow_uid(&self) -> u32 {
        std::fs::read_to_string("/proc/sys/kernel/overflowuid")
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(OVERFLOW_UID)
    }
}

/// The nobody route of a launch checked against the process that runs the launcher: other user and IPC namespaces than agentd's, and the overflow user.
/// Anything else ends the launch: the route's promise (a private IPC namespace, so the System V calls are safe) is not kept.
pub fn check(route: &NobodyRoute, src: &dyn NsSource) -> Result<(), SandboxError> {
    let own = src.namespaces()?;
    if own.ipc == route.agentd_ipc {
        return Err(SandboxError::SharedNamespace("ipc".to_string()));
    }
    if own.user == route.agentd_user {
        return Err(SandboxError::SharedNamespace("user".to_string()));
    }
    let (got, want) = (src.euid(), src.overflow_uid());
    if got != want {
        return Err(SandboxError::NotNobody { got, want });
    }
    Ok(())
}

/// agentd's own namespaces, as the route of a launch names them.
pub fn route_of_this_process() -> Result<NobodyRoute, SandboxError> {
    let ids = ProcNs.namespaces()?;
    Ok(NobodyRoute {
        agentd_ipc: ids.ipc,
        agentd_user: ids.user,
    })
}

/// Asked before a run on the nobody route fetches or places anything: can this machine make the namespaces, and does the helper work.
pub trait IdentityPreflight: Send + Sync {
    fn check(&self, helper: &Path) -> Result<(), SandboxError>;
}

/// Runs `ham-nobody-exec --check`, which does everything but the exec and prints `ham-nobody-exec: ok euid=65534 egid=1000`.
#[derive(Clone, Copy, Debug, Default)]
pub struct HelperCheck;

impl IdentityPreflight for HelperCheck {
    fn check(&self, helper: &Path) -> Result<(), SandboxError> {
        let fail = |why: String| SandboxError::Helper(why);
        let mut child = Command::new(helper)
            .args(["--check", "--", "/bin/true"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| fail(format!("{} did not start: {e}", helper.display())))?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < HELPER_CHECK_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(fail(format!(
                        "{} did not finish in {} s",
                        helper.display(),
                        HELPER_CHECK_TIMEOUT.as_secs()
                    )));
                }
                Err(e) => {
                    return Err(fail(format!(
                        "{} could not be waited for: {e}",
                        helper.display()
                    )));
                }
            }
        };
        let (mut out, mut err) = (String::new(), String::new());
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_string(&mut out);
        }
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        if status.success() && out.contains("ham-nobody-exec: ok euid=") {
            return Ok(());
        }
        let line = err
            .lines()
            .find(|l| l.starts_with("ham-nobody-exec: refused"))
            .or_else(|| err.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("no message");
        Err(fail(format!(
            "{status}: {}",
            line.chars().take(300).collect::<String>()
        )))
    }
}
