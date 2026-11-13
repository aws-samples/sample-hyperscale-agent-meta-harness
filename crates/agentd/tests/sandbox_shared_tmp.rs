//! The shared scratch directories of a `landlock` run and the variables that keep a
//! JVM in the run's own folders (b630-kernel-3). The grant of `/tmp`, `/var/tmp` and `/dev/shm` stands on a fact: nothing of agentd's is in them.
//! This file pins the fact (a scan of the sources for a path in a shared directory, and the policy refusing to cover a path of agentd's), the
//! composition of the grant, and the composition of the variables. `sandbox_tool_env.rs` runs the driver and reads the result back.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::config::Config;
use agentd::sandbox::{Policy, SHARED_TMP, Sandbox, tool_env};
use common::repo_root;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn cfg() -> Config {
    Config::from_vars(|k| match k {
        "AGENTD_LISTEN" => Some("0.0.0.0:8080".to_string()),
        "AGENTD_EXE" => Some("/opt/harness/bin/agentd".to_string()),
        _ => None,
    })
}

fn rw(p: &Policy) -> Vec<&Path> {
    p.read_write.iter().map(|g| g.path.as_path()).collect()
}

#[test]
fn the_three_scratch_directories_are_writable_optional_and_valid() {
    let (p, left_out) = Policy::for_config(&cfg(), None).with_shared_tmp();
    assert!(left_out.is_empty(), "{left_out:?}");
    for dir in ["/tmp", "/var/tmp", "/dev/shm"] {
        assert!(rw(&p).contains(&Path::new(dir)), "{dir}: {:?}", rw(&p));
    }
    assert!(p.read_write.iter().all(|g| g.optional));
    assert_eq!(SHARED_TMP, ["/tmp", "/var/tmp", "/dev/shm"]);
    p.validate().unwrap();
    // Asking twice grants once.
    let (again, _) = p.clone().with_shared_tmp();
    assert_eq!(again.read_write.len(), 3);
}

#[test]
fn the_base_policy_alone_still_grants_nothing_writable() {
    assert!(Policy::for_config(&cfg(), None).read_write.is_empty());
}

#[test]
fn agentds_default_folders_lie_outside_every_scratch_directory() {
    // The layout of the worker image and of the Instances runtime: the workspace root, the data root and the runtime folder.
    let c = cfg();
    for own in [&c.workspace_root, &c.data_root, &c.runtime_dir] {
        for dir in SHARED_TMP {
            assert!(
                !own.starts_with(dir) && !Path::new(dir).starts_with(own),
                "{} and {dir} overlap",
                own.display()
            );
        }
    }
    let run = c.data_root.join("run1/direct");
    let (p, left_out) = Policy::for_config(&c, None)
        .with_read_write(&run.join("data"))
        .with_read_write(&run.join("home"))
        .with_read_write(&run.join("tmp"))
        .with_private(&run.join("input"))
        .with_private(&run.join("snapshot.git"))
        .with_private(&c.runtime_dir)
        .with_shared_tmp();
    assert!(left_out.is_empty(), "{left_out:?}");
    p.validate().unwrap();
}

#[test]
fn a_path_of_agentd_inside_a_scratch_directory_keeps_the_directory_out_of_the_grant() {
    // `AGENTD_RUNTIME_DIR=/tmp/agentd`: granting `/tmp` would make the credential socket's folder writable by the engine.
    let (p, left_out) = Policy::for_config(&cfg(), None)
        .with_private(Path::new("/tmp/agentd"))
        .with_shared_tmp();
    assert_eq!(left_out, [PathBuf::from("/tmp")]);
    assert!(!rw(&p).contains(&Path::new("/tmp")));
    assert!(rw(&p).contains(&Path::new("/var/tmp")));
    assert!(rw(&p).contains(&Path::new("/dev/shm")));
    p.validate().unwrap();
}

#[test]
fn a_folder_of_the_run_inside_a_scratch_directory_keeps_the_directory_out_of_the_grant() {
    // A read-only folder of the run inside `/var/tmp` would become writable by the grant, since grants add up.
    let (p, left_out) = Policy::for_config(&cfg(), None)
        .with_read_only(Path::new("/var/tmp/run/config"))
        .with_shared_tmp();
    assert_eq!(left_out, [PathBuf::from("/var/tmp")]);
    assert!(!rw(&p).contains(&Path::new("/var/tmp")));
    p.validate().unwrap();
    // And a folder that holds a scratch directory (a grant of `/var`, say) leaves it out the same way.
    let (p, left_out) = Policy::for_config(&cfg(), None)
        .with_read_only(Path::new("/var"))
        .with_shared_tmp();
    assert_eq!(left_out, [PathBuf::from("/var/tmp")]);
    p.validate().unwrap();
}

#[test]
fn the_steps_sandbox_keeps_the_scratch_directories_and_nothing_else_writable_but_its_folder() {
    let (base, _) = Policy::for_config(&cfg(), None)
        .with_read_write(Path::new("/var/lib/agentd/run1/direct/data"))
        .with_shared_tmp();
    let sandbox = Sandbox {
        launcher: PathBuf::from("/opt/harness/bin/agentd"),
        policy: Arc::new(base),
        identity_helper: None,
    };
    let work = Path::new("/var/lib/agentd/run1/direct/report-x");
    let steps = sandbox.rebased(&[work]);
    let w = rw(&steps.policy);
    assert!(w.contains(&work), "{w:?}");
    for dir in SHARED_TMP {
        assert!(w.contains(&Path::new(dir)), "{dir}: {w:?}");
    }
    assert!(
        !w.contains(&Path::new("/var/lib/agentd/run1/direct/data")),
        "the run's own folder is not the steps': {w:?}"
    );
    assert_eq!(w.len(), 4, "{w:?}");
    steps.policy.validate().unwrap();
}

/// The `.rs` files below `dir`, tests excluded (a file in a `tests` folder or named `tests.rs`).
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "tests") {
                sources(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The agentd and package-proxy sources name no shared scratch directory and make no temp file of their own. This is the list of every path they
/// create (the facts the grant stands on): the run's data folder (`AGENTD_DATA_ROOT`), the credential socket's folder (`AGENTD_RUNTIME_DIR`), the
/// workspace root (`AGENTD_WORKSPACE_ROOT`), and, for the proxy, nothing on disk. A new use of a shared directory fails here and has to be argued.
#[test]
fn no_source_of_agentd_or_the_package_proxy_keeps_state_in_a_shared_scratch_directory() {
    let root = repo_root();
    let mut files = Vec::new();
    sources(&root.join("crates/agentd/src"), &mut files);
    sources(&root.join("crates/pkgproxy/src"), &mut files);
    assert!(files.len() > 50, "{} files", files.len());
    let needles = [
        "\"/tmp",
        "/var/tmp",
        "/dev/shm",
        "temp_dir(",
        "tempfile",
        "mkstemp",
    ];
    let mut hits = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap().display().to_string();
        // The sandbox module is where the directories are named (the grant itself and its documents).
        if rel.starts_with("crates/agentd/src/sandbox/") {
            continue;
        }
        for (i, line) in std::fs::read_to_string(f).unwrap().lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if needles.iter().any(|n| code.contains(n)) {
                hits.push(format!("{rel}:{}: {}", i + 1, code.trim()));
            }
        }
    }
    // The one use is the test-only context of the tool server's conformance example (`ToolContext::for_directory`, which says so).
    assert_eq!(
        hits.len(),
        1,
        "a source names a shared scratch directory or makes a temp file; the engine may write there, so agentd's own files must not be: {hits:#?}"
    );
    assert!(
        hits[0].starts_with("crates/agentd/src/direct/tools/backend.rs:")
            && hits[0].contains("std::env::temp_dir()"),
        "{hits:#?}"
    );
}

// ---- the variables ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_jvm_is_told_the_runs_home_and_temp_folder_and_gradle_a_home_of_its_own() {
    let env = BTreeMap::new();
    let got = tool_env(
        &env,
        Path::new("/var/lib/agentd/r/direct/home"),
        Path::new("/var/lib/agentd/r/direct/tmp"),
    );
    assert_eq!(
        got["JAVA_TOOL_OPTIONS"],
        "-Duser.home=/var/lib/agentd/r/direct/home -Djava.io.tmpdir=/var/lib/agentd/r/direct/tmp"
    );
    assert_eq!(
        got["GRADLE_USER_HOME"],
        "/var/lib/agentd/r/direct/home/.gradle"
    );
    assert_eq!(got.len(), 2);
}

#[test]
fn an_existing_value_stays_and_the_runs_folders_come_after_it_so_they_win() {
    let mut env = BTreeMap::new();
    env.insert(
        "JAVA_TOOL_OPTIONS".to_string(),
        "  -Xmx512m -Duser.home=/elsewhere ".to_string(),
    );
    let got = tool_env(&env, Path::new("/h"), Path::new("/t"));
    assert_eq!(
        got["JAVA_TOOL_OPTIONS"],
        "-Xmx512m -Duser.home=/elsewhere -Duser.home=/h -Djava.io.tmpdir=/t"
    );
    // A blank value counts as none.
    env.insert("JAVA_TOOL_OPTIONS".to_string(), "   ".to_string());
    assert_eq!(
        tool_env(&env, Path::new("/h"), Path::new("/t"))["JAVA_TOOL_OPTIONS"],
        "-Duser.home=/h -Djava.io.tmpdir=/t"
    );
}

#[test]
fn a_folder_name_with_a_space_is_quoted_because_the_jvm_splits_at_spaces() {
    let got = tool_env(&BTreeMap::new(), Path::new("/a b/home"), Path::new("/t"));
    assert_eq!(
        got["JAVA_TOOL_OPTIONS"],
        "-Duser.home=\"/a b/home\" -Djava.io.tmpdir=/t"
    );
}
