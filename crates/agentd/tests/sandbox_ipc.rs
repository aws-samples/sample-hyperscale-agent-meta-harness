//! The SysV IPC filter: a process that installed it cannot make or reach a shared-memory segment, a message queue or a
//! semaphore set, and a process that did not can. The filter is irreversible for the process that installs it, so the check runs in a child of this
//! test binary (the test runs itself again with a marker) and uses the `ipcmk` program of util-linux as the caller.
#![allow(clippy::unwrap_used)]

use agentd::sandbox::ipc::{self, SysvIpc};
use std::process::Command;

const CHILD: &str = "HAMH_IPC_FILTER_CHILD";
const CHILD_PRIVATE: &str = "HAMH_IPC_FILTER_PRIVATE_CHILD";

fn ipcmk(flag: &str) -> std::process::Output {
    Command::new("ipcmk").args([flag, "4096"]).output().unwrap()
}

fn have_ipcmk() -> bool {
    Command::new("ipcmk").arg("--help").output().is_ok()
}

/// Removes what an `ipcmk` run made (the id is the last number on its line).
fn remove(flag: &str, out: &std::process::Output) {
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(id) = text
        .split_whitespace()
        .last()
        .filter(|w| w.parse::<u64>().is_ok())
    {
        let rm = match flag {
            "-M" => "-m",
            "-Q" => "-q",
            _ => "-s",
        };
        let _ = Command::new("ipcrm").args([rm, id]).output();
    }
}

#[test]
fn the_filter_names_the_shared_memory_message_and_semaphore_calls() {
    let names: Vec<&str> = ipc::DENIED.iter().map(|(n, _)| *n).collect();
    for must in [
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
    ] {
        assert!(names.contains(&must), "{must} is denied: {names:?}");
    }
    assert!(
        ipc::DENIED.iter().all(|(_, n)| *n > 0),
        "every call has a number on this machine"
    );
}

#[test]
fn a_process_with_the_filter_cannot_make_what_a_process_without_it_can() {
    if std::env::var(CHILD).is_ok() {
        // The child: first the control, then the filter, then the same calls.
        for flag in ["-M", "-Q", "-S"] {
            let before = ipcmk(flag);
            assert!(
                before.status.success(),
                "{flag} before the filter: {before:?}"
            );
            remove(flag, &before);
        }
        ipc::install().unwrap();
        for flag in ["-M", "-Q", "-S"] {
            let after = ipcmk(flag);
            assert!(
                !after.status.success(),
                "{flag} must be refused after the filter: {after:?}"
            );
            let words = String::from_utf8_lossy(&after.stderr).to_lowercase();
            assert!(
                words.contains("not permitted"),
                "{flag}: the call fails with EPERM: {words}"
            );
        }
        return;
    }
    if !have_ipcmk() || !cfg!(target_os = "linux") {
        eprintln!("NOT RUN: ipcmk (util-linux) is not installed");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--exact",
            "a_process_with_the_filter_cannot_make_what_a_process_without_it_can",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_filter_also_names_the_three_io_uring_calls() {
    let names: Vec<&str> = ipc::DENIED.iter().map(|(n, _)| *n).collect();
    for must in ["io_uring_setup", "io_uring_enter", "io_uring_register"] {
        assert!(names.contains(&must), "{must} is denied: {names:?}");
    }
}

/// The rule table, pinned: the position of each argument is the one in `man 2 sendto`, `sendmsg`, `sendmmsg` and `socket`, and a change here
/// is a change of what the engine can reach.
#[test]
fn the_argument_rules_name_the_flags_and_the_protocol_where_the_system_calls_take_them() {
    let rule = |name: &str, arg: u8| {
        ipc::DENIED_WHEN
            .iter()
            .find(|r| r.name == name && r.arg == arg)
            .unwrap_or_else(|| panic!("no rule for {name} argument {arg}: {:?}", ipc::DENIED_WHEN))
    };
    // int sendto(int sockfd, const void *buf, size_t len, int flags, const struct sockaddr *dest_addr, socklen_t addrlen)
    let r = rule("sendto", 3);
    assert_eq!(
        (r.nr, r.test, r.value),
        (libc::SYS_sendto, ipc::Test::HasBits, 0x2000_0000)
    );
    // ssize_t sendmsg(int sockfd, const struct msghdr *msg, int flags)
    let r = rule("sendmsg", 2);
    assert_eq!(
        (r.nr, r.test, r.value),
        (libc::SYS_sendmsg, ipc::Test::HasBits, 0x2000_0000)
    );
    // int sendmmsg(int sockfd, struct mmsghdr *msgvec, unsigned int vlen, int flags)
    let r = rule("sendmmsg", 3);
    assert_eq!(
        (r.nr, r.test, r.value),
        (libc::SYS_sendmmsg, ipc::Test::HasBits, 0x2000_0000)
    );
    // int socket(int domain, int type, int protocol): IPPROTO_MPTCP is 262, AF_SMC is 43
    let r = rule("socket", 2);
    assert_eq!(
        (r.nr, r.test, r.value),
        (libc::SYS_socket, ipc::Test::Equals, 262)
    );
    let r = rule("socket", 0);
    assert_eq!(
        (r.nr, r.test, r.value),
        (libc::SYS_socket, ipc::Test::Equals, 43)
    );
    assert_eq!(ipc::DENIED_WHEN.len(), 5, "{:?}", ipc::DENIED_WHEN);
    // The constants are the kernel's.
    assert_eq!(ipc::MSG_FASTOPEN, libc::MSG_FASTOPEN as u64);
}

#[test]
fn no_call_is_denied_both_without_and_with_an_argument() {
    for r in ipc::DENIED_WHEN {
        assert!(
            !ipc::DENIED.iter().any(|(_, nr)| *nr == r.nr),
            "{} is denied whatever its arguments, so a rule for one value means nothing",
            r.name
        );
    }
}

#[test]
fn the_program_builds_for_this_machine() {
    let program = ipc::program().unwrap();
    // One instruction at least per listed call, and the arguments rules add more than the plain list.
    assert!(program.len() > ipc::DENIED.len() + ipc::DENIED_WHEN.len());
}

/// The errno of `io_uring_setup(0, NULL)` as a Python child sees it (x86-64 and arm64 both number it 425): `EINVAL` (22) when the call is allowed, `EPERM` (1) when
/// the filter answers it. `None` when Python is not there.
fn io_uring_errno() -> Option<i32> {
    let out = Command::new("python3")
        .args([
            "-c",
            "import ctypes; l = ctypes.CDLL(None, use_errno=True); l.syscall(425, 0, 0); print(ctypes.get_errno())",
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The filter for a private IPC namespace lets the System V calls through and keeps every other rule. The check runs in a child of this test
/// binary, as the deny filter's does. The child's IPC namespace is the host's here: the test only proves what the filter lets through, and removes what it makes.
#[test]
fn a_process_with_the_private_namespace_filter_makes_system_v_objects_and_still_cannot_use_io_uring()
 {
    if std::env::var(CHILD_PRIVATE).is_ok() {
        let before = io_uring_errno();
        ipc::install_with(SysvIpc::AllowedInPrivateNamespace).unwrap();
        for flag in ["-M", "-Q", "-S"] {
            let out = ipcmk(flag);
            assert!(
                out.status.success(),
                "{flag} must work in the private namespace: {out:?}"
            );
            remove(flag, &out);
        }
        match (before, io_uring_errno()) {
            (Some(b), Some(a)) if b != 1 => assert_eq!(
                a, 1,
                "io_uring_setup must answer EPERM after the filter (it answered {b} before)"
            ),
            _ => eprintln!(
                "NOT RUN: io_uring part (no python3, or the machine refuses io_uring itself)"
            ),
        }
        return;
    }
    if !have_ipcmk() || !cfg!(target_os = "linux") {
        eprintln!("NOT RUN: ipcmk (util-linux) is not installed");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--exact",
            "a_process_with_the_private_namespace_filter_makes_system_v_objects_and_still_cannot_use_io_uring",
            "--nocapture",
        ])
        .env(CHILD_PRIVATE, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
