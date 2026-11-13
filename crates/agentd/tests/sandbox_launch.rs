//! The launcher's flow with its collaborators given: no engine process unless the kernel says the whole policy is in force.
#![allow(clippy::unwrap_used)]

use agentd::sandbox::{
    Enforcer, Execer, FdSource, Policy, Report, SandboxError, parse_args, run, verify,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

type Calls = Rc<RefCell<Vec<String>>>;

struct FakeEnforcer {
    result: Result<Report, SandboxError>,
    ipc: Result<(), SandboxError>,
    calls: Calls,
}

impl Enforcer for FakeEnforcer {
    fn enforce(&self, _policy: &Policy) -> Result<Report, SandboxError> {
        self.calls.borrow_mut().push("enforce".to_string());
        self.result.clone()
    }
    fn preflight(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    fn deny_sysv_ipc(&self) -> Result<(), SandboxError> {
        self.calls.borrow_mut().push("ipc".to_string());
        self.ipc.clone()
    }
}

/// The table of descriptors: closing one removes it, unless it is one that cannot be closed or one that comes back.
struct FakeFds {
    open: RefCell<Vec<i32>>,
    calls: Calls,
    stubborn: Option<i32>,
    refuses_close: bool,
}

impl FakeFds {
    fn new(open: Vec<i32>, calls: &Calls) -> Self {
        FakeFds {
            open: RefCell::new(open),
            calls: calls.clone(),
            stubborn: None,
            refuses_close: false,
        }
    }
}

impl FdSource for FakeFds {
    fn open_fds(&self) -> Result<Vec<i32>, SandboxError> {
        self.calls.borrow_mut().push("fds".to_string());
        Ok(self.open.borrow().clone())
    }
    fn close(&self, fd: i32) -> Result<(), SandboxError> {
        self.calls.borrow_mut().push(format!("close {fd}"));
        if self.refuses_close {
            return Err(SandboxError::InheritedFd(fd));
        }
        if self.stubborn != Some(fd) {
            self.open.borrow_mut().retain(|f| *f != fd);
        }
        Ok(())
    }
}

struct FakeExec(Calls);

impl Execer for FakeExec {
    fn exec(&self, program: &Path, args: &[String]) -> Result<(), SandboxError> {
        self.0
            .borrow_mut()
            .push(format!("exec {} {}", program.display(), args.join(" ")));
        Ok(())
    }
}

fn good() -> Report {
    Report {
        fully_enforced: true,
        abi: 7,
        no_new_privs: true,
    }
}

fn policy_json() -> String {
    Policy::for_config(&agentd::config::Config::from_vars(|_| None), None).to_json()
}

fn args(program: &str, rest: &[&str]) -> Vec<String> {
    let mut v = vec![
        "--policy-json".to_string(),
        policy_json(),
        "--".to_string(),
        program.to_string(),
    ];
    v.extend(rest.iter().map(|s| s.to_string()));
    v
}

fn go(
    a: &[String],
    result: Result<Report, SandboxError>,
    fds: Vec<i32>,
) -> (Result<(), SandboxError>, Vec<String>) {
    let calls: Calls = Rc::default();
    go_with(a, result, FakeFds::new(fds, &calls), &calls)
}

fn go_with(
    a: &[String],
    result: Result<Report, SandboxError>,
    fds: FakeFds,
    calls: &Calls,
) -> (Result<(), SandboxError>, Vec<String>) {
    go_ipc(a, result, Ok(()), fds, calls)
}

fn go_ipc(
    a: &[String],
    result: Result<Report, SandboxError>,
    ipc: Result<(), SandboxError>,
    fds: FakeFds,
    calls: &Calls,
) -> (Result<(), SandboxError>, Vec<String>) {
    let out = run(
        a,
        &FakeEnforcer {
            result,
            ipc,
            calls: calls.clone(),
        },
        &fds,
        &FakeExec(calls.clone()),
    );
    let seen = calls.borrow().clone();
    (out, seen)
}

#[test]
fn the_whole_policy_in_force_executes_the_program_after_the_checks_in_order() {
    let (out, seen) = go(
        &args("/usr/bin/engine", &["--acp", "--", "x"]),
        Ok(good()),
        vec![0, 1, 2],
    );
    out.unwrap();
    assert_eq!(
        seen,
        vec![
            "fds".to_string(),
            "enforce".to_string(),
            "ipc".to_string(),
            "exec /usr/bin/engine --acp -- x".to_string()
        ],
        "descriptors, then the domain, then the program, with its arguments untouched"
    );
}

#[test]
fn a_failed_installation_executes_nothing() {
    for stage in ["handle", "create", "add_rule", "restrict_self"] {
        let err = SandboxError::Enforce {
            stage: stage.to_string(),
            detail: "injected".to_string(),
        };
        let (out, seen) = go(
            &args("/usr/bin/engine", &[]),
            Err(err.clone()),
            vec![0, 1, 2],
        );
        assert_eq!(out.unwrap_err(), err, "{stage}");
        assert!(
            !seen.iter().any(|c| c.starts_with("exec")),
            "{stage}: {seen:?}"
        );
    }
}

#[test]
fn an_abi_below_six_executes_nothing() {
    for abi in 0..=5u8 {
        let report = Report { abi, ..good() };
        let (out, seen) = go(&args("/usr/bin/engine", &[]), Ok(report), vec![0, 1, 2]);
        assert_eq!(
            out.unwrap_err(),
            SandboxError::AbiTooLow {
                found: abi,
                need: 6
            },
            "abi {abi}"
        );
        assert!(
            !seen.iter().any(|c| c.starts_with("exec")),
            "abi {abi}: {seen:?}"
        );
    }
}

#[test]
fn partial_enforcement_executes_nothing() {
    let report = Report {
        fully_enforced: false,
        ..good()
    };
    let (out, seen) = go(&args("/usr/bin/engine", &[]), Ok(report), vec![0, 1, 2]);
    assert_eq!(out.unwrap_err(), SandboxError::NotFullyEnforced);
    assert!(!seen.iter().any(|c| c.starts_with("exec")), "{seen:?}");
}

#[test]
fn no_new_privs_missing_executes_nothing() {
    let report = Report {
        no_new_privs: false,
        ..good()
    };
    let (out, seen) = go(&args("/usr/bin/engine", &[]), Ok(report), vec![0, 1, 2]);
    assert_eq!(out.unwrap_err(), SandboxError::NoNewPrivs);
    assert!(!seen.iter().any(|c| c.starts_with("exec")), "{seen:?}");
}

#[test]
fn a_policy_that_asks_for_more_than_the_kernel_has_executes_nothing() {
    let mut p = Policy::for_config(&agentd::config::Config::from_vars(|_| None), None);
    p.min_abi = 99;
    let report = good();
    let err = verify(&report, &p).unwrap_err();
    assert_eq!(err, SandboxError::AbiTooLow { found: 7, need: 99 });
}

#[test]
fn an_inherited_descriptor_is_closed_before_the_domain_is_installed() {
    let (out, seen) = go(
        &args("/usr/bin/engine", &["x"]),
        Ok(good()),
        vec![0, 1, 2, 7, 9],
    );
    out.unwrap();
    assert_eq!(
        seen,
        vec![
            "fds".to_string(),
            "close 7".to_string(),
            "close 9".to_string(),
            "fds".to_string(),
            "enforce".to_string(),
            "ipc".to_string(),
            "exec /usr/bin/engine x".to_string()
        ],
        "close, look again, then the domain, the IPC filter, then the program"
    );
}

#[test]
fn a_descriptor_that_cannot_be_closed_or_comes_back_ends_the_launch_before_the_domain() {
    let calls: Calls = Rc::default();
    let mut fds = FakeFds::new(vec![0, 1, 2, 7], &calls);
    fds.refuses_close = true;
    let (out, seen) = go_with(&args("/usr/bin/engine", &[]), Ok(good()), fds, &calls);
    assert_eq!(out.unwrap_err(), SandboxError::InheritedFd(7));
    assert!(
        !seen.iter().any(|c| c == "enforce" || c.starts_with("exec")),
        "{seen:?}"
    );

    let calls: Calls = Rc::default();
    let mut fds = FakeFds::new(vec![0, 1, 2, 7], &calls);
    fds.stubborn = Some(7);
    let (out, seen) = go_with(&args("/usr/bin/engine", &[]), Ok(good()), fds, &calls);
    assert_eq!(
        out.unwrap_err(),
        SandboxError::InheritedFd(7),
        "still there after the close"
    );
    assert!(
        !seen.iter().any(|c| c == "enforce" || c.starts_with("exec")),
        "{seen:?}"
    );
}

#[test]
fn the_real_table_lists_and_closes_a_descriptor_and_leaves_the_standard_ones() {
    use std::os::fd::IntoRawFd;
    let fd = std::fs::File::open("/dev/null").unwrap().into_raw_fd();
    let table = agentd::sandbox::ProcFds;
    assert!(table.open_fds().unwrap().contains(&fd));
    table.close(fd).unwrap();
    let after = table.open_fds().unwrap();
    assert!(!after.contains(&fd), "{after:?}");
    table.close(fd).unwrap();
}

#[test]
fn a_relative_program_is_refused() {
    let (out, seen) = go(&args("engine", &[]), Ok(good()), vec![0, 1, 2]);
    assert_eq!(
        out.unwrap_err(),
        SandboxError::Program("engine".to_string())
    );
    assert!(seen.is_empty(), "refused before any check ran: {seen:?}");
}

#[test]
fn the_arguments_are_parsed_strictly() {
    let (policy, program, rest) = parse_args(&args("/usr/bin/engine", &["a", "--", "b"])).unwrap();
    assert_eq!(policy.min_abi, 6);
    assert_eq!(program, PathBuf::from("/usr/bin/engine"));
    assert_eq!(
        rest,
        vec!["a".to_string(), "--".to_string(), "b".to_string()]
    );
    for bad in [
        vec![],
        vec!["--".to_string(), "/x".to_string()],
        vec!["--policy-json".to_string()],
        vec!["--policy-json".to_string(), policy_json()],
        vec!["--policy-json".to_string(), policy_json(), "--".to_string()],
        vec![
            "--policy-json".to_string(),
            "{}".to_string(),
            "--".to_string(),
            "/x".to_string(),
        ],
        vec![
            "--policy".to_string(),
            policy_json(),
            "--".to_string(),
            "/x".to_string(),
        ],
    ] {
        assert!(
            matches!(
                parse_args(&bad),
                Err(SandboxError::Usage(_) | SandboxError::Policy(_))
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn verify_accepts_exactly_a_full_report_at_or_above_the_required_abi() {
    let p = Policy::for_config(&agentd::config::Config::from_vars(|_| None), None);
    verify(&good(), &p).unwrap();
    verify(&Report { abi: 6, ..good() }, &p).unwrap();
    verify(&Report { abi: 9, ..good() }, &p).unwrap();
    assert!(verify(&Report { abi: 5, ..good() }, &p).is_err());
}

fn ipc_error() -> SandboxError {
    SandboxError::Enforce {
        stage: "seccomp".to_string(),
        detail: "injected: the filter was refused".to_string(),
    }
}

#[test]
fn the_ipc_filter_is_installed_once_the_domain_is_checked_and_before_the_program_starts() {
    let calls: Calls = Rc::default();
    let (out, seen) = go_ipc(
        &args("/usr/bin/engine", &[]),
        Ok(good()),
        Ok(()),
        FakeFds::new(vec![0, 1, 2], &calls),
        &calls,
    );
    out.unwrap();
    let pos = |what: &str| seen.iter().position(|c| c.starts_with(what)).unwrap();
    assert!(pos("enforce") < pos("ipc"), "{seen:?}");
    assert!(pos("ipc") < pos("exec"), "{seen:?}");
}

#[test]
fn a_filter_the_kernel_refuses_ends_the_launch_and_no_program_starts() {
    let calls: Calls = Rc::default();
    let (out, seen) = go_ipc(
        &args("/usr/bin/engine", &[]),
        Ok(good()),
        Err(ipc_error()),
        FakeFds::new(vec![0, 1, 2], &calls),
        &calls,
    );
    assert_eq!(out, Err(ipc_error()));
    assert!(
        !seen.iter().any(|c| c.starts_with("exec")),
        "no program after a refused filter: {seen:?}"
    );
}

#[test]
fn no_filter_is_asked_for_when_the_domain_did_not_come_up() {
    let calls: Calls = Rc::default();
    let (out, seen) = go_ipc(
        &args("/usr/bin/engine", &[]),
        Err(SandboxError::NotFullyEnforced),
        Ok(()),
        FakeFds::new(vec![0, 1, 2], &calls),
        &calls,
    );
    assert_eq!(out, Err(SandboxError::NotFullyEnforced));
    assert!(!seen.iter().any(|c| c == "ipc"), "{seen:?}");
}

#[test]
fn a_policy_that_turns_the_filter_off_does_not_install_it() {
    let mut p = Policy::for_config(&agentd::config::Config::from_vars(|_| None), None);
    p.deny_sysv_ipc = false;
    let a = vec![
        "--policy-json".to_string(),
        p.to_json(),
        "--".to_string(),
        "/usr/bin/engine".to_string(),
    ];
    let (out, seen) = go(&a, Ok(good()), vec![0, 1, 2]);
    out.unwrap();
    assert!(!seen.iter().any(|c| c == "ipc"), "{seen:?}");
    assert!(seen.iter().any(|c| c.starts_with("exec")), "{seen:?}");
}
