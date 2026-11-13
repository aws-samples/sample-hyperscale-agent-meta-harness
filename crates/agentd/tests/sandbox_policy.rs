//! The policy of one engine launch: composed from the configuration and the run's folders, checked, and carried as JSON to the launcher.
#![allow(clippy::unwrap_used)]

use agentd::config::Config;
use agentd::sandbox::{MIN_ABI, Policy};
use std::path::{Path, PathBuf};

fn cfg() -> Config {
    Config::from_vars(|k| match k {
        "AGENTD_LISTEN" => Some("0.0.0.0:8080".to_string()),
        "AGENTD_ENGINE_PATH" => Some("/opt/harness/bin:/usr/local/bin:/usr/bin:/bin".to_string()),
        "AGENTD_EXE" => Some("/opt/harness/bin/agentd".to_string()),
        _ => None,
    })
}

fn base() -> Policy {
    Policy::for_config(&cfg(), None)
}

fn paths(grants: &[agentd::sandbox::Grant]) -> Vec<&Path> {
    grants.iter().map(|g| g.path.as_path()).collect()
}

#[test]
fn the_base_policy_asks_for_abi_six_and_is_valid() {
    let p = base();
    assert_eq!(p.version, 1);
    assert_eq!(p.min_abi, MIN_ABI);
    assert_eq!(MIN_ABI, 6);
    p.validate().unwrap();
}

#[test]
fn the_system_trees_are_read_and_execute_and_none_is_writable() {
    let p = base();
    let rx = paths(&p.read_exec);
    for must in ["/usr", "/bin", "/lib", "/opt/harness"] {
        assert!(rx.contains(&Path::new(must)), "{must} is read-exec: {rx:?}");
    }
    assert!(p.read_write.is_empty(), "the base grants nothing writable");
    let ro = paths(&p.read_only);
    for must in ["/proc", "/etc/ssl", "/etc/resolv.conf", "/etc/passwd"] {
        assert!(ro.contains(&Path::new(must)), "{must} is read-only: {ro:?}");
    }
    assert!(!rx.contains(&Path::new("/etc")), "never the whole of /etc");
}

#[test]
fn every_system_path_is_optional_so_a_machine_without_one_still_launches() {
    let p = base();
    for g in p.read_exec.iter().chain(&p.read_only).chain(&p.devices) {
        assert!(g.optional, "{} is optional in the base", g.path.display());
    }
}

#[test]
fn the_engine_search_path_and_the_agentd_binary_are_executable() {
    let p = Policy::for_config(
        &Config::from_vars(|k| match k {
            "AGENTD_ENGINE_PATH" => Some("/srv/tools/bin:/usr/bin".to_string()),
            "AGENTD_EXE" => Some("/srv/agentd/agentd".to_string()),
            _ => None,
        }),
        None,
    );
    let rx = paths(&p.read_exec);
    assert!(rx.contains(&Path::new("/srv/tools/bin")), "{rx:?}");
    assert!(rx.contains(&Path::new("/srv/agentd/agentd")), "{rx:?}");
}

#[test]
fn the_fixtures_and_templates_trees_of_the_profiles_are_readable_and_executable() {
    let p = Policy::for_config(
        &Config::from_vars(|k| match k {
            "AGENTD_FIXTURES_ROOT" => Some("/srv/fixtures".to_string()),
            "AGENTD_TEMPLATES_ROOT" => Some("/srv/templates".to_string()),
            _ => None,
        }),
        None,
    );
    let rx = paths(&p.read_exec);
    assert!(rx.contains(&Path::new("/srv/fixtures")), "{rx:?}");
    assert!(rx.contains(&Path::new("/srv/templates")), "{rx:?}");
    assert_eq!(
        rx.iter()
            .filter(|x| **x == Path::new("/opt/harness"))
            .count(),
        1,
        "a tree two settings name is listed once: {rx:?}"
    );
}

#[test]
fn devices_are_single_files_and_there_is_no_shared_memory_directory() {
    let p = base();
    let dev = paths(&p.devices);
    for must in ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/tty"] {
        assert!(dev.contains(&Path::new(must)), "{must}: {dev:?}");
    }
    assert!(!dev.contains(&Path::new("/dev")), "never the whole /dev");
    assert!(!dev.contains(&Path::new("/dev/shm")), "no shared /dev/shm");
}

#[test]
fn agentds_own_port_is_denied_for_connect_and_bind_and_port_80_for_connect() {
    let p = base();
    assert!(p.deny_connect.contains(&8080), "{:?}", p.deny_connect);
    assert!(p.deny_bind.contains(&8080), "{:?}", p.deny_bind);
    assert!(
        p.deny_connect.contains(&80),
        "the EC2 metadata address is port 80: {:?}",
        p.deny_connect
    );
    assert!(
        !p.deny_bind.contains(&80),
        "a bind to 80 is the kernel's to refuse"
    );
}

#[test]
fn a_listener_on_port_zero_closes_no_port_and_the_policy_stays_valid() {
    let p = Policy::for_config(
        &Config::from_vars(|k| (k == "AGENTD_LISTEN").then(|| "127.0.0.1:0".to_string())),
        None,
    );
    assert!(!p.deny_bind.contains(&0) && !p.deny_connect.contains(&0));
    p.validate().unwrap();
}

#[test]
fn the_platforms_vending_port_is_denied_when_the_endpoint_names_one() {
    let p = Policy::for_config(&cfg(), Some("http://100.88.0.1:36407"));
    assert!(p.deny_connect.contains(&36407));
    assert!(p.deny_bind.contains(&36407));
    assert!(p.deny_connect.contains(&80));
    for bad in [
        "",
        "not a url",
        "http://100.88.0.1",
        "http://h:99999",
        "http://h:0",
    ] {
        let q = Policy::for_config(&cfg(), Some(bad));
        assert_eq!(q.deny_connect, vec![8080, 80], "{bad:?} adds no port");
        assert_eq!(q.deny_bind, vec![8080], "{bad:?} adds no port");
    }
}

#[test]
fn writable_and_read_only_grants_are_added_once() {
    let p = base()
        .with_read_write(Path::new("/var/lib/agentd/r/direct/home"))
        .with_read_write(Path::new("/var/lib/agentd/r/direct/home"))
        .with_read_only(Path::new("/var/lib/agentd/r/direct/config"));
    assert_eq!(
        paths(&p.read_write),
        vec![Path::new("/var/lib/agentd/r/direct/home")]
    );
    assert_eq!(
        paths(&p.read_only)
            .into_iter()
            .filter(|x| *x == Path::new("/var/lib/agentd/r/direct/config"))
            .count(),
        1
    );
    assert!(
        p.read_write.iter().all(|g| !g.optional),
        "a run folder is required"
    );
    p.validate().unwrap();
}

#[test]
fn a_folder_a_module_may_make_later_is_an_optional_read_only_grant() {
    let p = base().with_optional_read_only(Path::new("/var/lib/agentd/r/direct/context"));
    let g = p
        .read_only
        .iter()
        .find(|g| g.path == Path::new("/var/lib/agentd/r/direct/context"))
        .expect("granted");
    assert!(g.optional);
    p.validate().unwrap();
}

#[test]
fn a_read_only_tree_inside_a_writable_one_is_refused_because_grants_add_up() {
    let p = base()
        .with_read_write(Path::new("/var/lib/agentd/r/direct/home"))
        .with_read_only(Path::new("/var/lib/agentd/r/direct/home/rules"));
    let why = p.validate().unwrap_err().to_string();
    assert!(why.contains("rules") && why.contains("writable"), "{why}");
}

#[test]
fn private_state_inside_any_granted_tree_is_refused() {
    let p = base()
        .with_read_write(Path::new("/var/lib/agentd/r/direct/home"))
        .with_private(Path::new("/var/lib/agentd/r/direct/home/secret"));
    let why = p.validate().unwrap_err().to_string();
    assert!(why.contains("private") && why.contains("secret"), "{why}");
    let q = base()
        .with_read_write(Path::new("/var/lib/agentd/r/direct/home"))
        .with_private(Path::new("/var/lib/agentd/r/direct/input"));
    q.validate().unwrap();
    // A private path that holds a granted tree is refused as well: the grant would reach it from above.
    let r = base()
        .with_read_write(Path::new("/var/lib/agentd/r/direct/home"))
        .with_private(Path::new("/var/lib/agentd/r"));
    assert!(r.validate().is_err());
}

#[test]
fn invalid_policies_are_refused() {
    let mut p = base();
    p.version = 2;
    assert!(p.validate().unwrap_err().to_string().contains("version"));
    let mut p = base();
    p.min_abi = 5;
    assert!(p.validate().unwrap_err().to_string().contains("ABI"));
    let rel = base().with_read_write(Path::new("relative/dir"));
    assert!(rel.validate().unwrap_err().to_string().contains("absolute"));
    let dots = base().with_read_write(Path::new("/var/lib/agentd/../etc"));
    assert!(dots.validate().unwrap_err().to_string().contains(".."));
    let root = base().with_read_write(Path::new("/"));
    assert!(
        root.validate().is_err(),
        "the root directory is never writable"
    );
    let mut p = base();
    p.deny_connect = vec![0];
    assert!(p.validate().is_err(), "port 0 is not a port to deny");
}

#[test]
fn the_policy_travels_as_json_and_comes_back_equal() {
    let p = base()
        .with_read_write(Path::new("/var/lib/agentd/r/direct/tmp"))
        .with_private(Path::new("/var/lib/agentd/r/direct/input"));
    let text = p.to_json();
    assert!(
        text.starts_with('{') && text.contains("\"minAbi\":6"),
        "{text}"
    );
    assert_eq!(Policy::from_json(&text).unwrap(), p);
}

#[test]
fn json_with_an_unknown_field_or_a_bad_policy_is_refused() {
    let text = base().to_json();
    let extra = text.replacen('{', "{\"unrestricted\":true,", 1);
    assert!(Policy::from_json(&extra).is_err());
    assert!(Policy::from_json("not json").is_err());
    let weak = text.replacen("\"minAbi\":6", "\"minAbi\":1", 1);
    assert!(Policy::from_json(&weak).is_err(), "validated on the way in");
    let _ = PathBuf::new();
}

#[test]
fn the_base_policy_denies_sysv_ipc_and_a_policy_that_does_not_say_reads_as_denying() {
    assert!(base().deny_sysv_ipc);
    let mut value: serde_json::Value = serde_json::from_str(&base().to_json()).unwrap();
    value.as_object_mut().unwrap().remove("denySysvIpc");
    let read = Policy::from_json(&value.to_string()).unwrap();
    assert!(
        read.deny_sysv_ipc,
        "a launcher given a policy without the member still denies"
    );
}

#[test]
fn a_policy_that_turns_the_filter_off_keeps_it_off_through_the_json() {
    let mut p = base();
    p.deny_sysv_ipc = false;
    assert!(!Policy::from_json(&p.to_json()).unwrap().deny_sysv_ipc);
}

#[test]
fn the_ports_the_engine_needs_stay_open_and_the_three_it_must_not_reach_are_closed() {
    // The package proxy, TLS and the resolver's TCP fallback are open; agentd's contract port, plain HTTP (the metadata address) and the platform's
    // credential port (named by the endpoint variable) are closed for connect.
    let p = Policy::for_config(&cfg(), Some("http://100.88.0.1:34197"));
    for open in [3128u16, 443, 53] {
        assert!(
            !p.deny_connect.contains(&open),
            "{open} is open: {:?}",
            p.deny_connect
        );
        assert!(!p.deny_bind.contains(&open), "{open} may be bound");
    }
    for closed in [8080u16, 80, 34197] {
        assert!(
            p.deny_connect.contains(&closed),
            "{closed} is closed: {:?}",
            p.deny_connect
        );
    }
}
