//! `opencode acp` of the v1 line binds `127.0.0.1` on port 0 and connects to itself. The engine's sandbox
//! must leave an ephemeral loopback bind and a connect to that port alone, and keep closed the ports that are agentd's.
#![allow(clippy::unwrap_used)]

use agentd::config::Config;
use agentd::sandbox::Policy;

fn policy() -> Policy {
    Policy::for_config(
        &Config::from_vars(|k| match k {
            "AGENTD_LISTEN" => Some("0.0.0.0:8080".to_string()),
            "AGENTD_ENGINE_PATH" => Some("/opt/harness/bin:/usr/bin:/bin".to_string()),
            "AGENTD_EXE" => Some("/opt/harness/bin/agentd".to_string()),
            _ => None,
        }),
        Some("http://169.254.170.2:36407"),
    )
}

#[test]
fn b790_the_ephemeral_bind_of_port_zero_is_never_denied() {
    let p = policy();
    assert!(!p.deny_bind.contains(&0), "{:?}", p.deny_bind);
    assert!(!p.deny_connect.contains(&0));
    p.validate().unwrap();
}

#[test]
fn b790_a_port_the_kernel_hands_out_can_be_bound_and_connected_to() {
    let p = policy();
    for port in [32768u16, 40123, 45000, 60999] {
        assert!(!p.deny_bind.contains(&port), "bind {port}");
        assert!(!p.deny_connect.contains(&port), "connect {port}");
    }
}

#[test]
fn b790_only_the_ports_of_agentd_and_the_platform_stay_closed() {
    let p = policy();
    // agentd's own port and the credential endpoint's port, for connect and bind; plain HTTP for connect.
    for port in [8080u16, 36407] {
        assert!(
            p.deny_connect.contains(&port) && p.deny_bind.contains(&port),
            "{port}"
        );
    }
    assert!(p.deny_connect.contains(&80));
    let mut closed: Vec<u16> = p.deny_connect.clone();
    closed.sort_unstable();
    assert_eq!(closed, [80, 8080, 36407]);
}

/// Runs a program under the real Landlock rules and binds `127.0.0.1:0` in it. Skips, saying so, on a kernel without the TCP rules
/// (ABI 4 and later, Linux 6.7).
#[test]
fn b790_under_the_real_rules_an_engine_can_bind_port_zero_on_loopback() {
    let abi = std::fs::read_to_string("/sys/kernel/security/lsm").unwrap_or_default();
    if !abi.contains("landlock") {
        eprintln!(
            "SKIPPED: this kernel has no Landlock ({}); the policy tests above cover the rules",
            abi.trim()
        );
        return;
    }
    let mut policy = policy();
    for dir in ["/usr", "/lib", "/lib64", "/bin", "/etc"] {
        policy = policy.with_read_only(std::path::Path::new(dir));
    }
    let json = serde_json::to_string(&policy).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["sandbox-exec", "--policy-json", &json, "--", "/usr/bin/python3", "-c"])
        .arg("import socket; s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(1); c = socket.create_connection(s.getsockname()); print('bound', s.getsockname()[1])")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("bound"),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
