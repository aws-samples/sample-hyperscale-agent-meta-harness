//! The "nobody" route of the engine on the Instances machine: the setting that turns it on or off, the policy member that carries agentd's
//! namespaces, the launcher's check of what it was started in, the filter that lets System V IPC through only in the private namespace, and the command line that puts
//! the helper in front of the launcher.
#![allow(clippy::unwrap_used)]

use agentd::config::Config;
use agentd::sandbox::{
    Enforcer, Execer, FdSource, IDENTITY_VAR, Identity, IdentitySetting, NamespaceIds, NobodyRoute,
    NsSource, Policy, Report, Sandbox, SandboxError, SysvIpc, ipc, run_with, word_of_user,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

type Calls = Rc<RefCell<Vec<String>>>;

fn config(vars: &[(&str, &str)]) -> Config {
    Config::from_vars(|k| {
        vars.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_string())
    })
}

fn route() -> NobodyRoute {
    NobodyRoute {
        agentd_ipc: "ipc:[4026532731]".to_string(),
        agentd_user: "user:[4026532728]".to_string(),
    }
}

// ---- the setting ---------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_variable_is_named_for_the_engine_identity() {
    assert_eq!(IDENTITY_VAR, "AGENTD_ENGINE_IDENTITY");
}

#[test]
fn unset_and_empty_mean_nobody_because_that_is_the_default_of_the_landlock_mode() {
    assert_eq!(
        IdentitySetting::from_value(None),
        IdentitySetting::Identity(Identity::Nobody)
    );
    assert_eq!(
        IdentitySetting::from_value(Some("")),
        IdentitySetting::Identity(Identity::Nobody)
    );
    assert_eq!(
        IdentitySetting::from_value(Some("  \n")),
        IdentitySetting::Identity(Identity::Nobody)
    );
    assert_eq!(IdentitySetting::default().resolve(), Ok(Identity::Nobody));
}

#[test]
fn the_two_words_parse_and_surrounding_space_is_ignored() {
    assert_eq!(
        IdentitySetting::from_value(Some("nobody")),
        IdentitySetting::Identity(Identity::Nobody)
    );
    assert_eq!(
        IdentitySetting::from_value(Some(" agentd\n")),
        IdentitySetting::Identity(Identity::Agentd)
    );
}

#[test]
fn every_other_value_is_refused_with_the_variable_and_the_value_in_the_reason() {
    for v in [
        "root",
        "Nobody",
        "NOBODY",
        "off",
        "auto",
        "0",
        "uid",
        "nobody,agentd",
    ] {
        let IdentitySetting::Refused(why) = IdentitySetting::from_value(Some(v)) else {
            panic!("{v:?} must be refused");
        };
        assert!(why.contains("AGENTD_ENGINE_IDENTITY"), "{v:?}: {why}");
        assert!(why.contains(v), "{v:?}: {why}");
        assert!(
            why.contains("nobody") && why.contains("agentd"),
            "the reason names the two words: {why}"
        );
        assert_eq!(IdentitySetting::from_value(Some(v)).resolve(), Err(why));
    }
}

#[test]
fn the_summary_word_names_who_the_engine_ran_as() {
    assert_eq!(Identity::Nobody.word(), "nobody");
    assert_eq!(Identity::Agentd.word(), "agentd");
}

#[test]
fn the_name_of_an_engine_user_becomes_a_plain_word_for_the_summary() {
    // The summary refuses anything but 1 to 32 lower-case letters, digits, - and _; a user name the deployment chose must not cost the run its summary.
    assert_eq!(word_of_user("agent"), "agent");
    assert_eq!(word_of_user("Agent.User"), "agent_user");
    assert_eq!(word_of_user("svc-1_x"), "svc-1_x");
    assert_eq!(word_of_user(&"a".repeat(40)), "a".repeat(32));
    assert_eq!(word_of_user(""), "agentd");
    assert_eq!(word_of_user("  \n"), "agentd");
}

#[test]
fn config_reads_the_variable_and_the_helper_path() {
    let c = config(&[]);
    assert_eq!(
        c.engine_identity,
        IdentitySetting::Identity(Identity::Nobody)
    );
    assert_eq!(
        c.identity_helper,
        PathBuf::from("/opt/harness/bin/ham-nobody-exec")
    );
    let c = config(&[
        ("AGENTD_ENGINE_IDENTITY", "agentd"),
        ("AGENTD_NOBODY_EXEC", "/x/helper"),
    ]);
    assert_eq!(
        c.engine_identity,
        IdentitySetting::Identity(Identity::Agentd)
    );
    assert_eq!(c.identity_helper, PathBuf::from("/x/helper"));
}

// ---- the policy member ---------------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_policy_without_the_route_has_no_such_member_so_the_old_launcher_and_the_drift_test_see_what_they_saw()
 {
    let p = Policy::for_config(&config(&[]), None);
    assert!(p.nobody.is_none());
    assert!(!p.to_json().contains("nobody"), "{}", p.to_json());
}

#[test]
fn the_route_travels_in_the_policy_json_and_comes_back_equal() {
    let p = Policy::for_config(&config(&[]), None).with_nobody(route());
    let json = p.to_json();
    assert!(
        json.contains(
            "\"nobody\":{\"agentdIpc\":\"ipc:[4026532731]\",\"agentdUser\":\"user:[4026532728]\"}"
        ),
        "{json}"
    );
    assert_eq!(Policy::from_json(&json).unwrap(), p);
}

#[test]
fn a_route_whose_namespace_names_are_not_namespace_names_is_refused_by_validate() {
    for (ipc, user) in [
        ("", "user:[1]"),
        ("ipc:[1]", ""),
        ("ipc:[x]", "user:[1]"),
        ("user:[1]", "user:[1]"),
        ("ipc:[1]", "ipc:[1]"),
        ("ipc:[1", "user:[1]"),
    ] {
        let p = Policy::for_config(&config(&[]), None).with_nobody(NobodyRoute {
            agentd_ipc: ipc.to_string(),
            agentd_user: user.to_string(),
        });
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("nobody"), "{ipc:?} {user:?}: {err}");
    }
    Policy::for_config(&config(&[]), None)
        .with_nobody(route())
        .validate()
        .unwrap();
}

// ---- the command line ----------------------------------------------------------------------------------------------------------------------------------

fn sandbox(helper: Option<&str>, with_route: bool) -> Sandbox {
    let mut policy = Policy::for_config(&config(&[]), None);
    if with_route {
        policy = policy.with_nobody(route());
    }
    Sandbox {
        launcher: PathBuf::from("/opt/harness/bin/agentd"),
        policy: Arc::new(policy),
        identity_helper: helper.map(PathBuf::from),
    }
}

#[test]
fn the_helper_goes_in_front_of_the_launcher_and_nothing_else_moves() {
    let sb = sandbox(Some("/opt/harness/bin/ham-nobody-exec"), true);
    let argv = sb
        .wrap(vec!["/usr/bin/git".to_string(), "status".to_string()])
        .unwrap();
    assert_eq!(
        argv[..7].to_vec(),
        vec![
            "/opt/harness/bin/ham-nobody-exec".to_string(),
            "--".to_string(),
            "/opt/harness/bin/agentd".to_string(),
            "sandbox-exec".to_string(),
            "--policy-json".to_string(),
            sb.policy.to_json(),
            "--".to_string(),
        ]
    );
    assert_eq!(
        argv[7..].to_vec(),
        vec!["/usr/bin/git".to_string(), "status".to_string()]
    );
}

#[test]
fn without_a_helper_the_command_line_is_the_one_of_before() {
    let argv = sandbox(None, false)
        .wrap(vec!["/usr/bin/git".to_string()])
        .unwrap();
    assert_eq!(argv[0], "/opt/harness/bin/agentd");
    assert_eq!(argv[1], "sandbox-exec");
}

#[test]
fn a_helper_without_the_route_or_the_route_without_a_helper_is_refused_before_anything_starts() {
    let a = sandbox(Some("/opt/harness/bin/ham-nobody-exec"), false)
        .wrap(vec!["/usr/bin/git".to_string()])
        .unwrap_err();
    assert_eq!(a.kind(), std::io::ErrorKind::InvalidInput);
    assert!(a.to_string().contains("nobody"), "{a}");
    let b = sandbox(None, true)
        .wrap(vec!["/usr/bin/git".to_string()])
        .unwrap_err();
    assert_eq!(b.kind(), std::io::ErrorKind::InvalidInput);
    assert!(b.to_string().contains("helper"), "{b}");
    let c = sandbox(Some("ham-nobody-exec"), true)
        .wrap(vec!["/usr/bin/git".to_string()])
        .unwrap_err();
    assert!(c.to_string().contains("absolute"), "{c}");
}

#[test]
fn a_rebased_sandbox_keeps_the_helper_and_the_route() {
    let sb = sandbox(Some("/opt/harness/bin/ham-nobody-exec"), true)
        .rebased(&[Path::new("/mnt/workspace/r1/report")]);
    assert_eq!(
        sb.identity_helper,
        Some(PathBuf::from("/opt/harness/bin/ham-nobody-exec"))
    );
    assert_eq!(sb.policy.nobody, Some(route()));
}

// ---- the launcher --------------------------------------------------------------------------------------------------------------------------------------

struct FakeEnforcer {
    calls: Calls,
}

impl Enforcer for FakeEnforcer {
    fn enforce(&self, _policy: &Policy) -> Result<Report, SandboxError> {
        self.calls.borrow_mut().push("enforce".to_string());
        Ok(Report {
            fully_enforced: true,
            abi: 7,
            no_new_privs: true,
        })
    }
    fn preflight(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    fn deny_sysv_ipc(&self) -> Result<(), SandboxError> {
        self.calls.borrow_mut().push("filter Deny".to_string());
        Ok(())
    }
    fn install_filter(&self, sysv: SysvIpc) -> Result<(), SandboxError> {
        self.calls.borrow_mut().push(format!("filter {sysv:?}"));
        Ok(())
    }
}

struct NoFds;

impl FdSource for NoFds {
    fn open_fds(&self) -> Result<Vec<i32>, SandboxError> {
        Ok(vec![0, 1, 2])
    }
    fn close(&self, _fd: i32) -> Result<(), SandboxError> {
        Ok(())
    }
}

struct FakeExec(Calls);

impl Execer for FakeExec {
    fn exec(&self, program: &Path, _args: &[String]) -> Result<(), SandboxError> {
        self.0
            .borrow_mut()
            .push(format!("exec {}", program.display()));
        Ok(())
    }
}

struct FakeNs {
    ids: NamespaceIds,
    euid: u32,
    calls: Calls,
}

impl NsSource for FakeNs {
    fn namespaces(&self) -> Result<NamespaceIds, SandboxError> {
        self.calls.borrow_mut().push("ns".to_string());
        Ok(self.ids.clone())
    }
    fn euid(&self) -> u32 {
        self.euid
    }
    fn overflow_uid(&self) -> u32 {
        65534
    }
}

fn args(policy: &Policy) -> Vec<String> {
    vec![
        "--policy-json".to_string(),
        policy.to_json(),
        "--".to_string(),
        "/usr/bin/true".to_string(),
    ]
}

fn launch(
    policy: &Policy,
    ipc: &str,
    user: &str,
    euid: u32,
) -> (Result<(), SandboxError>, Vec<String>) {
    let calls: Calls = Rc::default();
    let ns = FakeNs {
        ids: NamespaceIds {
            ipc: ipc.to_string(),
            user: user.to_string(),
        },
        euid,
        calls: calls.clone(),
    };
    let r = run_with(
        &args(policy),
        &FakeEnforcer {
            calls: calls.clone(),
        },
        &NoFds,
        &FakeExec(calls.clone()),
        &ns,
    );
    let log = calls.borrow().clone();
    (r, log)
}

#[test]
fn in_its_own_namespaces_as_the_overflow_user_the_launcher_lets_system_v_ipc_through_and_runs_the_program()
 {
    let p = Policy::for_config(&config(&[]), None).with_nobody(route());
    let (r, calls) = launch(&p, "ipc:[11]", "user:[12]", 65534);
    r.unwrap();
    assert_eq!(
        calls,
        vec![
            "ns",
            "enforce",
            "filter AllowedInPrivateNamespace",
            "exec /usr/bin/true"
        ]
    );
}

#[test]
fn a_launcher_still_in_agentds_ipc_namespace_refuses_and_starts_nothing() {
    let p = Policy::for_config(&config(&[]), None).with_nobody(route());
    let (r, calls) = launch(&p, "ipc:[4026532731]", "user:[12]", 65534);
    assert_eq!(r, Err(SandboxError::SharedNamespace("ipc".to_string())));
    assert_eq!(calls, vec!["ns"], "no domain, no filter, no program");
}

#[test]
fn a_launcher_still_in_agentds_user_namespace_refuses_and_starts_nothing() {
    let p = Policy::for_config(&config(&[]), None).with_nobody(route());
    let (r, calls) = launch(&p, "ipc:[11]", "user:[4026532728]", 65534);
    assert_eq!(r, Err(SandboxError::SharedNamespace("user".to_string())));
    assert_eq!(calls, vec!["ns"]);
}

#[test]
fn a_launcher_that_reads_as_any_other_user_refuses_and_starts_nothing() {
    let p = Policy::for_config(&config(&[]), None).with_nobody(route());
    for euid in [0, 901, 1000] {
        let (r, calls) = launch(&p, "ipc:[11]", "user:[12]", euid);
        assert_eq!(
            r,
            Err(SandboxError::NotNobody {
                got: euid,
                want: 65534
            }),
            "{euid}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c == "enforce" || c.starts_with("exec") || c.starts_with("filter")),
            "{calls:?}"
        );
    }
}

#[test]
fn the_error_texts_name_the_route_so_the_run_log_explains_a_refusal() {
    assert!(
        SandboxError::SharedNamespace("ipc".to_string())
            .to_string()
            .contains("ipc")
    );
    let t = SandboxError::NotNobody {
        got: 0,
        want: 65534,
    }
    .to_string();
    assert!(t.contains("65534") && t.contains('0'), "{t}");
}

#[test]
fn a_policy_without_the_route_never_asks_for_the_namespaces_and_keeps_the_deny_rule() {
    let p = Policy::for_config(&config(&[]), None);
    let (r, calls) = launch(&p, "ipc:[4026532731]", "user:[4026532728]", 0);
    r.unwrap();
    assert_eq!(calls, ["enforce", "filter Deny", "exec /usr/bin/true"]);
}

// ---- the filter ----------------------------------------------------------------------------------------------------------------------------------------

const SYSV: [&str; 12] = [
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

#[test]
fn the_deny_filter_is_the_list_of_decision_0219_and_the_private_namespace_filter_drops_only_the_system_v_calls()
 {
    let deny = ipc::denied_names(SysvIpc::Deny);
    let allowed = ipc::denied_names(SysvIpc::AllowedInPrivateNamespace);
    for name in SYSV {
        assert!(deny.contains(&name), "{name} is denied by default");
        assert!(
            !allowed.contains(&name),
            "{name} is allowed in the private namespace"
        );
    }
    let rest: Vec<&str> = deny.iter().copied().filter(|n| !SYSV.contains(n)).collect();
    assert_eq!(
        allowed, rest,
        "everything else stays denied: the POSIX queues and io_uring"
    );
    for must in [
        "mq_open",
        "mq_unlink",
        "mq_timedsend",
        "mq_timedreceive",
        "mq_notify",
        "mq_getsetattr",
        "io_uring_setup",
        "io_uring_enter",
        "io_uring_register",
    ] {
        assert!(allowed.contains(&must), "{must} stays denied");
    }
}

#[test]
fn the_door_rules_around_the_tcp_ports_are_in_both_filters() {
    for sysv in [SysvIpc::Deny, SysvIpc::AllowedInPrivateNamespace] {
        let rules = ipc::argument_rule_names(sysv);
        for must in ["sendto", "sendmsg", "sendmmsg", "socket"] {
            assert!(rules.contains(&must), "{sysv:?}: {must}");
        }
        ipc::program_with(sysv).unwrap();
    }
}
