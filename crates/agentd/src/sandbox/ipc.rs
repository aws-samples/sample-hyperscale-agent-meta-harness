//! The seccomp filter of the engine's launcher: the kernel's IPC objects and the doors around Landlock's TCP rules.
//!
//! **SysV IPC.** Landlock does not cover SysV shared memory, message queues or semaphores, and the IPC namespace is shared with
//! agentd, so a process of the same user could read a segment that agentd or the package proxy made (measured on the Instances machine: `shmget` and
//! `msgget` of an object made outside the domain worked from inside it). The container allows an unprivileged process with `no_new_privs` to stack a
//! seccomp filter (the `unshare(CLONE_NEWIPC)` route is closed by the container's own filter), so the launcher installs this one right after the
//! domain: every call below answers `EPERM`, for the engine and for everything it starts. POSIX message queues live in the same namespace and are
//! closed with them. POSIX shared memory and semaphores are files under `/dev/shm`, which the policy grants like `/tmp` (`SHARED_TMP`: agentd keeps nothing there).
//!
//! **The doors around the TCP rules.** Landlock's `CONNECT_TCP` right is the only barrier between the engine and
//! agentd's own port, the platform's credential-vending port and port 80, and it is asked only at `connect()` of a TCP socket. Three routes do not
//! ask it, and this filter closes each with `EPERM`:
//!
//! - TCP Fast Open: `sendto`, `sendmsg` or `sendmmsg` with `MSG_FASTOPEN` opens the connection inside the send. Kernels before the Landlock fix
//!   (Linux 6.18.54 has it) never ask the connect rule. The flags argument is `sendto` argument 3, `sendmsg` argument 2 and `sendmmsg` argument 3.
//! - Multipath TCP and SMC: a socket of protocol `IPPROTO_MPTCP` (`socket` argument 2) or of family `AF_SMC` (`socket` argument 0) is no TCP socket to
//!   Landlock, and both fall back to plain TCP when the peer does not speak the protocol, so a connect on one reaches an ordinary listener.
//! - io_uring: a send queued on the ring (`IORING_OP_SENDMSG` with `MSG_FASTOPEN`) never passes the argument rules above. `io_uring_setup`,
//!   `io_uring_enter` and `io_uring_register` answer `EPERM`; libuv and Tokio fall back to their ordinary system calls when the ring is refused.
//!
//! **The private namespace.** On the Instances machine the engine can run as the overflow user in an IPC namespace of its own (the
//! nobody route: `images/worker-python/ham-nobody-exec` makes the namespaces before the launcher runs, and the launcher checks them). There the System V calls
//! reach nothing of agentd's, so [`SysvIpc::AllowedInPrivateNamespace`] leaves them out of the list and a database server can keep its interlock segment.
//! Every other rule stays: the POSIX message queues, io_uring and the doors above.
//!
//! What is still open: UDP (Landlock does not cover it, and the three listeners are TCP), the three Unix sockets of the platform's process 1, and a
//! program that reaches a closed port from outside the engine's family (a separate network namespace would close that, and it is not built).

use super::launch::SandboxError;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch, apply_filter,
};
use std::collections::BTreeMap;

/// What the filter does about System V IPC (shared memory, message queues, semaphores).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysvIpc {
    /// The calls answer `EPERM`: the IPC namespace is agentd's.
    Deny,
    /// The calls are allowed: the process is in an IPC namespace of its own, which the launcher has checked.
    AllowedInPrivateNamespace,
}

/// The System V calls among [`DENIED`].
pub const SYSV: &[&str] = &[
    "shmget",
    "shmat",
    "shmctl",
    "shmdt",
    "msgget",
    "msgsnd",
    "msgrcv",
    "msgctl",
    "semget",
    "semop",
    "semtimedop",
    "semctl",
];

/// The calls the filter answers with `EPERM` whatever their arguments: name and number on this machine.
pub const DENIED: &[(&str, i64)] = &[
    ("shmget", libc::SYS_shmget),
    ("shmat", libc::SYS_shmat),
    ("shmctl", libc::SYS_shmctl),
    ("shmdt", libc::SYS_shmdt),
    ("msgget", libc::SYS_msgget),
    ("msgsnd", libc::SYS_msgsnd),
    ("msgrcv", libc::SYS_msgrcv),
    ("msgctl", libc::SYS_msgctl),
    ("semget", libc::SYS_semget),
    ("semop", libc::SYS_semop),
    ("semtimedop", libc::SYS_semtimedop),
    ("semctl", libc::SYS_semctl),
    ("mq_open", libc::SYS_mq_open),
    ("mq_unlink", libc::SYS_mq_unlink),
    ("mq_timedsend", libc::SYS_mq_timedsend),
    ("mq_timedreceive", libc::SYS_mq_timedreceive),
    ("mq_notify", libc::SYS_mq_notify),
    ("mq_getsetattr", libc::SYS_mq_getsetattr),
    ("io_uring_setup", libc::SYS_io_uring_setup),
    ("io_uring_enter", libc::SYS_io_uring_enter),
    ("io_uring_register", libc::SYS_io_uring_register),
];

/// `MSG_FASTOPEN` (`include/uapi/linux/socket.h`).
pub const MSG_FASTOPEN: u64 = 0x2000_0000;
/// `IPPROTO_MPTCP` (`include/uapi/linux/in.h`).
pub const IPPROTO_MPTCP: u64 = 262;
/// `AF_SMC` (`include/linux/socket.h`).
pub const AF_SMC: u64 = 43;

/// How an argument is compared with the rule's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Test {
    /// The argument equals the value.
    Equals,
    /// Every bit of the value is set in the argument (`arg & value == value`).
    HasBits,
}

/// A call that answers `EPERM` only when one of its arguments has a given value. The argument is read as 32 bits: `flags`, `domain` and
/// `protocol` are `int` in the kernel's ABI, and the upper half of their register is undefined on a 64-bit machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgRule {
    pub name: &'static str,
    pub nr: i64,
    /// Position of the argument, counted from 0 (`man 2` order).
    pub arg: u8,
    pub test: Test,
    pub value: u64,
}

/// The calls denied for one argument value (see the module text). Several rules for one call are alternatives.
pub const DENIED_WHEN: &[ArgRule] = &[
    ArgRule {
        name: "sendto",
        nr: libc::SYS_sendto,
        arg: 3,
        test: Test::HasBits,
        value: MSG_FASTOPEN,
    },
    ArgRule {
        name: "sendmsg",
        nr: libc::SYS_sendmsg,
        arg: 2,
        test: Test::HasBits,
        value: MSG_FASTOPEN,
    },
    ArgRule {
        name: "sendmmsg",
        nr: libc::SYS_sendmmsg,
        arg: 3,
        test: Test::HasBits,
        value: MSG_FASTOPEN,
    },
    ArgRule {
        name: "socket",
        nr: libc::SYS_socket,
        arg: 2,
        test: Test::Equals,
        value: IPPROTO_MPTCP,
    },
    ArgRule {
        name: "socket",
        nr: libc::SYS_socket,
        arg: 0,
        test: Test::Equals,
        value: AF_SMC,
    },
];

fn fail(detail: impl std::fmt::Display) -> SandboxError {
    SandboxError::Enforce {
        stage: "seccomp".to_string(),
        detail: detail.to_string(),
    }
}

/// The seccomp rule of one [`ArgRule`].
fn rule_of(r: &ArgRule) -> Result<SeccompRule, SandboxError> {
    let op = match r.test {
        Test::Equals => SeccompCmpOp::Eq,
        Test::HasBits => SeccompCmpOp::MaskedEq(r.value),
    };
    let condition = SeccompCondition::new(r.arg, SeccompCmpArgLen::Dword, op, r.value)
        .map_err(|e| fail(format!("{}: {e}", r.name)))?;
    SeccompRule::new(vec![condition]).map_err(|e| fail(format!("{}: {e}", r.name)))
}

/// The unconditional list for a System V rule: [`DENIED`], without the System V calls when they are allowed.
fn denied_for(sysv: SysvIpc) -> Vec<(&'static str, i64)> {
    DENIED
        .iter()
        .copied()
        .filter(|(name, _)| sysv == SysvIpc::Deny || !SYSV.contains(name))
        .collect()
}

/// The names of the calls the filter answers with `EPERM` whatever their arguments.
pub fn denied_names(sysv: SysvIpc) -> Vec<&'static str> {
    denied_for(sysv).into_iter().map(|(n, _)| n).collect()
}

/// The names of the calls the filter denies for one argument value (a call can appear twice, one entry for each rule).
pub fn argument_rule_names(sysv: SysvIpc) -> Vec<&'static str> {
    let denied = denied_for(sysv);
    DENIED_WHEN
        .iter()
        .filter(|r| !denied.iter().any(|(_, nr)| *nr == r.nr))
        .map(|r| r.name)
        .collect()
}

/// The program of the filter for this machine's architecture: every listed call answers `EPERM`, any other is allowed.
pub fn program() -> Result<BpfProgram, SandboxError> {
    program_with(SysvIpc::Deny)
}

/// The program of the filter with the given System V rule.
pub fn program_with(sysv: SysvIpc) -> Result<BpfProgram, SandboxError> {
    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|e| fail(format!("the architecture {}: {e}", std::env::consts::ARCH)))?;
    let denied = denied_for(sysv);
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        denied.iter().map(|(_, nr)| (*nr, Vec::new())).collect();
    for r in DENIED_WHEN {
        // A call denied whatever its arguments needs no rule for one value (an empty list of rules is the unconditional match).
        if denied.iter().any(|(_, nr)| *nr == r.nr) {
            continue;
        }
        rules.entry(r.nr).or_default().push(rule_of(r)?);
    }
    let deny = SeccompAction::Errno(u32::try_from(libc::EPERM).unwrap_or(1));
    SeccompFilter::new(rules, SeccompAction::Allow, deny, arch)
        .map_err(fail)?
        .try_into()
        .map_err(fail)
}

/// Installs the filter on the calling process (it applies to every program the process starts). It sets `no_new_privs`, which the process has
/// already; a kernel or container that refuses a filter ends the launch.
pub fn install() -> Result<(), SandboxError> {
    install_with(SysvIpc::Deny)
}

/// Installs the filter with the given System V rule.
pub fn install_with(sysv: SysvIpc) -> Result<(), SandboxError> {
    apply_filter(&program_with(sysv)?).map_err(fail)
}
