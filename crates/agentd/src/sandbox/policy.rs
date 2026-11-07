//! The Landlock policy of one engine launch: which files and ports the engine may use. Plain data, serialised into the launcher's argument.
//!
//! Landlock grants add up: a rule for a folder gives everything beneath it, and a rule for a folder inside it takes nothing away. So the policy
//! is checked for that ([`Policy::validate`]): nothing read-only may sit inside a writable tree, and the paths declared private (agentd's own
//! state) may not sit inside any granted tree or hold one.

use super::launch::SandboxError;
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// The lowest Landlock ABI the policy may ask for: ABI 6 brings the scopes (abstract Unix sockets and signals) on top of the file and TCP rights.
pub const MIN_ABI: u8 = 6;

const VERSION: u32 = 1;

/// System trees the engine reads and executes from. Any of them may be missing on a given image.
const SYSTEM_READ_EXEC: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt/harness"];

/// System trees and files the engine reads (no execution): the certificate stores, the resolver and user databases, the loader's cache, the
/// process table (the domain itself keeps other processes' memory and environment out of reach) and the device and CPU facts of `/sys`.
const SYSTEM_READ_ONLY: &[&str] = &[
    "/proc",
    "/sys",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/etc/alternatives",
    "/etc/ld.so.conf.d",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/localtime",
    "/etc/timezone",
    "/etc/os-release",
    "/etc/ca-certificates.conf",
    "/etc/gitconfig",
    "/etc/services",
    "/etc/protocols",
    "/etc/mime.types",
    "/etc/environment",
];

/// Device files the engine uses, one by one: never the whole of `/dev`. The shared memory directory `/dev/shm` is a scratch directory with its own grant ([`SHARED_TMP`]).
const SYSTEM_DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/pts",
];

/// Scratch directories every process of the machine uses, granted to the engine like the run's own folders.
/// The shipped rules and the lifecycle pack tell the engine to keep logs in `/tmp`, the JVM and glibc's `tmpfile()`
/// name it themselves, and POSIX semaphores and shared memory (Python's `multiprocessing`) are files under `/dev/shm`. In the microVM they work because
/// the directories are world-writable; in the `landlock` mode the engine has agentd's own user, so file modes protect nothing and only the domain does.
/// So the grant stands on one fact, which a test pins: agentd, the package proxy, the credential socket and the tool servers keep nothing in them (their
/// folders are under the run's data folder, `AGENTD_RUNTIME_DIR` and the workspace root). A directory that holds a path of agentd's or of the run is not
/// granted ([`Policy::with_shared_tmp`]).
pub const SHARED_TMP: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];

/// Ports the engine may never connect to, whatever else is open: 80 is the port of the EC2 metadata address (`169.254.169.254`). Landlock names
/// ports and not addresses, so this closes plain HTTP for the engine as well; its installs go through the package proxy and its other traffic is TLS.
const ALWAYS_DENIED_CONNECT: &[u16] = &[80];

/// One path of the policy. A path that is `optional` is skipped when it does not exist; any other missing path ends the launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    pub path: PathBuf,
    #[serde(default)]
    pub optional: bool,
}

impl Grant {
    fn optional(path: &str) -> Self {
        Grant {
            path: PathBuf::from(path),
            optional: true,
        }
    }

    fn required(path: &Path) -> Self {
        Grant {
            path: path.to_path_buf(),
            optional: false,
        }
    }
}

/// The nobody route of a launch: the engine runs as the overflow user in user and IPC namespaces of its own, made by
/// `ham-nobody-exec` before the launcher. These are agentd's namespaces (`ipc:[<inode>]` and `user:[<inode>]`, as `readlink` shows them); the launcher
/// must find itself in other ones, and only then does the filter let System V IPC through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NobodyRoute {
    pub agentd_ipc: String,
    pub agentd_user: String,
}

/// Whether `text` is the name of a namespace of `kind`, as `readlink /proc/self/ns/<kind>` prints it: `<kind>:[<digits>]`.
fn is_namespace_name(text: &str, kind: &str) -> bool {
    text.strip_prefix(kind)
        .and_then(|r| r.strip_prefix(":["))
        .and_then(|r| r.strip_suffix(']'))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub min_abi: u8,
    /// Every file right except the creation of device nodes: the engine's checkout, home, temp and build folders.
    pub read_write: Vec<Grant>,
    /// Read, list and execute.
    pub read_exec: Vec<Grant>,
    /// Read and list.
    pub read_only: Vec<Grant>,
    /// Read, write, truncate and `ioctl` on device files.
    pub devices: Vec<Grant>,
    /// agentd's own state: not applied to the kernel, only checked to lie outside every grant.
    pub private: Vec<PathBuf>,
    /// TCP ports the engine may not connect to. Every other port is allowed (Landlock names ports and not addresses).
    pub deny_connect: Vec<u16>,
    /// TCP ports the engine may not bind.
    pub deny_bind: Vec<u16>,
    /// Whether the launcher installs the seccomp filter that denies SysV IPC (shared memory, message queues, semaphores). A policy that does not say
    /// reads as denying: the filter is off only when the policy says so.
    #[serde(default = "yes")]
    pub deny_sysv_ipc: bool,
    /// The nobody route, absent for the engine that runs as agentd's own user. With it the launcher checks that it is in namespaces of its
    /// own, as the overflow user, and installs the filter that lets System V IPC through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nobody: Option<NobodyRoute>,
}

fn yes() -> bool {
    true
}

/// The port of a URL such as `http://100.88.0.1:36407`, when it names one.
fn port_of(endpoint: &str) -> Option<u16> {
    let text = endpoint.trim();
    let rest = text.split_once("://").map_or(text, |(_, r)| r);
    let host_port = rest.split(['/', '?', '#']).next()?;
    let (_, port) = host_port.rsplit_once(':')?;
    port.parse::<u16>().ok().filter(|p| *p != 0)
}

impl Policy {
    /// The part of the policy every launch shares: the system trees, the devices and the ports of agentd's own services. `vending_endpoint` is
    /// the platform's credential endpoint (`AWS_EC2_METADATA_SERVICE_ENDPOINT`); its port is closed to the engine, which gets its credentials
    /// from agentd.
    pub fn for_config(cfg: &Config, vending_endpoint: Option<&str>) -> Self {
        let mut read_exec: Vec<Grant> = SYSTEM_READ_EXEC
            .iter()
            .map(|p| Grant::optional(p))
            .collect();
        // The engine's search path, this binary (the engine's `credential_process`), and the trees the profiles' `{fixtures}` and templates name.
        for dir in
            cfg.engine_path
                .iter()
                .chain([&cfg.agentd_exe, &cfg.fixtures_root, &cfg.templates_root])
        {
            if let Some(p) = dir.to_str() {
                let g = Grant::optional(p);
                if !read_exec.contains(&g) {
                    read_exec.push(g);
                }
            }
        }
        // A listener on port 0 (a test's) has no port to close.
        let mut denied: Vec<u16> = Some(cfg.listen.port())
            .filter(|p| *p != 0)
            .into_iter()
            .collect();
        denied.extend(vending_endpoint.and_then(port_of));
        denied.dedup();
        let mut deny_connect = denied.clone();
        deny_connect.extend(
            ALWAYS_DENIED_CONNECT
                .iter()
                .copied()
                .filter(|p| !denied.contains(p)),
        );
        Policy {
            version: VERSION,
            min_abi: MIN_ABI,
            read_write: Vec::new(),
            read_exec,
            read_only: SYSTEM_READ_ONLY
                .iter()
                .map(|p| Grant::optional(p))
                .collect(),
            devices: SYSTEM_DEVICES.iter().map(|p| Grant::optional(p)).collect(),
            private: Vec::new(),
            deny_connect,
            deny_bind: denied,
            deny_sysv_ipc: true,
            nobody: None,
        }
    }

    /// Puts the launch on the nobody route; `route` names agentd's own namespaces.
    pub fn with_nobody(mut self, route: NobodyRoute) -> Self {
        self.nobody = Some(route);
        self
    }

    /// Adds a folder the engine may write (once).
    pub fn with_read_write(mut self, path: &Path) -> Self {
        let g = Grant::required(path);
        if !self.read_write.contains(&g) {
            self.read_write.push(g);
        }
        self
    }

    /// Adds a folder the engine may only read (once).
    pub fn with_read_only(mut self, path: &Path) -> Self {
        let g = Grant::required(path);
        if !self.read_only.contains(&g) {
            self.read_only.push(g);
        }
        self
    }

    /// As [`with_read_only`](Self::with_read_only) for a folder that a module makes later or not at all (the context and attachments folder): it is
    /// skipped when it is not there at launch.
    pub fn with_optional_read_only(mut self, path: &Path) -> Self {
        let g = Grant {
            path: path.to_path_buf(),
            optional: true,
        };
        if !self.read_only.contains(&g) {
            self.read_only.push(g);
        }
        self
    }

    /// Adds the shared scratch directories ([`SHARED_TMP`]) as writable, optional folders, after every other grant and private path is in. A
    /// directory that holds one of them, or lies inside one (the machine's `/tmp` as the run's data folder, say), is left out so that nothing of
    /// agentd's or of the run becomes writable by being under it; the paths left out are returned for the log.
    pub fn with_shared_tmp(mut self) -> (Self, Vec<PathBuf>) {
        let mut left_out = Vec::new();
        for dir in SHARED_TMP.iter().map(Path::new) {
            let overlaps = self
                .all_grants()
                .map(|g| g.path.as_path())
                .chain(self.private.iter().map(PathBuf::as_path))
                .any(|p| p.starts_with(dir) || dir.starts_with(p));
            if overlaps {
                left_out.push(dir.to_path_buf());
                continue;
            }
            let g = Grant {
                path: dir.to_path_buf(),
                optional: true,
            };
            if !self.read_write.contains(&g) {
                self.read_write.push(g);
            }
        }
        (self, left_out)
    }

    /// Declares a path of agentd's own state that must stay outside every grant.
    pub fn with_private(mut self, path: &Path) -> Self {
        if !self.private.iter().any(|p| p == path) {
            self.private.push(path.to_path_buf());
        }
        self
    }

    fn all_grants(&self) -> impl Iterator<Item = &Grant> {
        self.read_write
            .iter()
            .chain(&self.read_exec)
            .chain(&self.read_only)
            .chain(&self.devices)
    }

    pub fn validate(&self) -> Result<(), SandboxError> {
        let bad = |why: String| Err(SandboxError::Policy(why));
        if self.version != VERSION {
            return bad(format!("version {} is not {VERSION}", self.version));
        }
        if self.min_abi < MIN_ABI {
            return bad(format!(
                "minAbi {} is below the Landlock ABI {MIN_ABI} that brings the scopes",
                self.min_abi
            ));
        }
        for p in self.all_grants().map(|g| &g.path).chain(&self.private) {
            if !p.is_absolute() {
                return bad(format!("{} is not an absolute path", p.display()));
            }
            if p.components().any(|c| c == Component::ParentDir) {
                return bad(format!("{} contains ..", p.display()));
            }
        }
        if self.read_write.iter().any(|g| g.path == Path::new("/")) {
            return bad("the root directory cannot be writable".to_string());
        }
        if self.deny_connect.contains(&0) || self.deny_bind.contains(&0) {
            return bad("port 0 is not a port to deny".to_string());
        }
        if let Some(route) = &self.nobody {
            for (name, kind) in [(&route.agentd_ipc, "ipc"), (&route.agentd_user, "user")] {
                if !is_namespace_name(name, kind) {
                    return bad(format!(
                        "the nobody route names {name:?} as agentd's {kind} namespace, which is not one ({kind}:[<number>])"
                    ));
                }
            }
        }
        for rw in &self.read_write {
            for g in self.read_exec.iter().chain(&self.read_only) {
                if g.path != rw.path && g.path.starts_with(&rw.path) {
                    return bad(format!(
                        "{} is inside the writable {}: grants add up, so it would be writable",
                        g.path.display(),
                        rw.path.display()
                    ));
                }
            }
        }
        for private in &self.private {
            for g in self.all_grants() {
                if private.starts_with(&g.path) || g.path.starts_with(private) {
                    return bad(format!(
                        "the private path {} and the granted {} overlap",
                        private.display(),
                        g.path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The policy from its JSON, checked.
    pub fn from_json(text: &str) -> Result<Self, SandboxError> {
        let policy: Policy = serde_json::from_str(text)
            .map_err(|e| SandboxError::Policy(format!("the policy is not readable: {e}")))?;
        policy.validate()?;
        Ok(policy)
    }
}
