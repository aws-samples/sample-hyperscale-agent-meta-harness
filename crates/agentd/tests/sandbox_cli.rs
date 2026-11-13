//! The real launcher binary: `agentd sandbox-exec` refuses, with its own exit code, whatever is wrong, and never starts the program then.
#![allow(clippy::unwrap_used)]

use agentd::sandbox::{Policy, REFUSED_EXIT};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agentd")
}

fn policy(min_abi: u8) -> String {
    let mut p = Policy::for_config(&agentd::config::Config::from_vars(|_| None), None);
    p.min_abi = min_abi;
    let text = p.to_json();
    // `to_json` never writes a policy the launcher would refuse as invalid; a test of an unreachable ABI edits the text.
    text.replacen("\"minAbi\":6", &format!("\"minAbi\":{min_abi}"), 1)
}

fn launch(policy: &str, program: &str, rest: &[&str]) -> Output {
    Command::new(bin())
        .args(["sandbox-exec", "--policy-json", policy, "--", program])
        .args(rest)
        .env_clear()
        .output()
        .unwrap()
}

fn kernel_has_abi_six() -> bool {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut parts = release.trim().split(['.', '-']);
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor) >= (6, 12)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

#[test]
fn no_arguments_is_a_usage_refusal() {
    let out = Command::new(bin())
        .arg("sandbox-exec")
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(i32::from(REFUSED_EXIT)));
    assert!(
        text(&out.stderr).contains("agentd sandbox-exec: refused"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_bad_policy_is_refused_and_the_program_does_not_run() {
    let out = launch("{}", "/bin/echo", &["SHOULD-NOT-RUN"]);
    assert_eq!(out.status.code(), Some(i32::from(REFUSED_EXIT)));
    assert!(!text(&out.stdout).contains("SHOULD-NOT-RUN"));
}

#[test]
fn a_policy_that_asks_for_an_abi_no_kernel_has_never_starts_the_program() {
    let out = launch(&policy(99), "/bin/echo", &["SHOULD-NOT-RUN"]);
    assert_eq!(
        out.status.code(),
        Some(i32::from(REFUSED_EXIT)),
        "{}",
        text(&out.stderr)
    );
    assert!(
        !text(&out.stdout).contains("SHOULD-NOT-RUN"),
        "the program ran"
    );
    let err = text(&out.stderr);
    assert!(err.contains("agentd sandbox-exec: refused"), "{err}");
    assert!(err.contains("Landlock"), "the reason names Landlock: {err}");
}

#[test]
fn a_kernel_without_abi_six_refuses_the_ordinary_policy_too() {
    if kernel_has_abi_six() {
        eprintln!(
            "NOT RUN: this kernel has Landlock ABI 6 or higher; the refusal is covered by the unreachable-ABI test"
        );
        return;
    }
    let out = launch(&policy(6), "/bin/echo", &["SHOULD-NOT-RUN"]);
    assert_eq!(
        out.status.code(),
        Some(i32::from(REFUSED_EXIT)),
        "{}",
        text(&out.stderr)
    );
    assert!(!text(&out.stdout).contains("SHOULD-NOT-RUN"));
}

#[test]
fn an_inherited_descriptor_does_not_stop_the_launch_it_is_closed_and_the_domain_decides() {
    // The shell opens descriptor 7 without close-on-exec and replaces itself with the launcher, which inherits it. A policy no kernel can satisfy
    // makes the launcher refuse at the domain: it got past the descriptors, so the refusal is Landlock's and not the descriptor's.
    let out = Command::new("sh")
        .arg("-c")
        .arg("exec 7</dev/null; exec \"$0\" sandbox-exec --policy-json \"$1\" -- /bin/echo SHOULD-NOT-RUN")
        .arg(bin())
        .arg(policy(99))
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(i32::from(REFUSED_EXIT)),
        "{}",
        text(&out.stderr)
    );
    let err = text(&out.stderr);
    assert!(!err.contains("file descriptor"), "{err}");
    assert!(err.contains("Landlock"), "{err}");
    assert!(!text(&out.stdout).contains("SHOULD-NOT-RUN"));
}

#[test]
fn where_the_kernel_has_abi_six_the_program_runs_inside_the_domain_without_the_inherited_descriptor()
 {
    if !kernel_has_abi_six() {
        eprintln!(
            "NOT RUN: this kernel is older than Linux 6.12 (no Landlock ABI 6); the positive path runs on the Instances machine"
        );
        return;
    }
    let out = launch(&policy(6), "/bin/echo", &["hello-from-the-domain"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("hello-from-the-domain"));
    // Descriptor 7 was open when the launcher started; the program lists its own descriptors and finds none above 2.
    let listed = Command::new("sh")
        .arg("-c")
        .arg("exec 7</dev/null; exec \"$0\" sandbox-exec --policy-json \"$1\" -- /bin/ls /proc/self/fd")
        .arg(bin())
        .arg(policy(6))
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(listed.status.code(), Some(0), "{}", text(&listed.stderr));
    assert!(
        !text(&listed.stdout).lines().any(|l| l.trim() == "7"),
        "{}",
        text(&listed.stdout)
    );
}
