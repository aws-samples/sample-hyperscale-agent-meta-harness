//! Cargo in a direct run: the settings agentd gives the engine's cargo. The pieces are small and pure: the
//! build switches for a session of 2 vCPU, the cargo home with the proxy's `config.toml`, the job count a deployment can change, and the
//! engine's PATH, which gains the Rust toolchain's bin directory of the worker image. The runs that use them are in
//! `direct_invocation.rs` (the engine's environment) and `direct_report_run.rs` (the report steps).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::config::{Config, DirectConfig};
use agentd::direct::cargo;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

fn direct(pairs: &[(&str, &str)]) -> DirectConfig {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    DirectConfig::from_vars(&|k| m.get(k).cloned())
}

fn config(pairs: &[(&str, &str)]) -> Config {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Config::from_vars(|k| m.get(k).cloned())
}

/// An engine home as agentd makes it before the proxy starts: an empty directory.
fn engine_home(label: &str) -> PathBuf {
    let home = common::scratch(label).join("home");
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// The user and group this test runs as.
fn me() -> (u32, u32) {
    let m = std::fs::metadata("/proc/self").unwrap();
    (m.uid(), m.gid())
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

/// The engine's PATH of the worker image (the Dockerfile's `AGENTD_ENGINE_PATH`).
const IMAGE_PATH: &str = "/opt/harness/bin:/opt/harness/gate-tools/lang/bin:/usr/local/bin:/opt/harness/gate-tools/bin:/usr/bin:/bin";
const RUST_BIN: &str = "/opt/harness/gate-tools/lang/rust/bin";

#[test]
fn the_build_switches_are_exactly_these_and_hold_no_credential() {
    let want: BTreeMap<String, String> = [
        ("CARGO_BUILD_JOBS", "2"),
        ("CARGO_INCREMENTAL", "0"),
        ("CARGO_NET_GIT_FETCH_WITH_CLI", "false"),
        ("CARGO_PROFILE_DEV_DEBUG", "0"),
        ("CARGO_PROFILE_TEST_DEBUG", "0"),
        ("RUST_BACKTRACE", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(cargo::build_env(2), want);
    for k in cargo::build_env(2).keys() {
        assert!(!k.contains("TOKEN") && !k.contains("CREDENTIAL") && !k.contains("REGISTR"));
    }
}

#[test]
fn the_job_count_follows_the_argument() {
    assert_eq!(cargo::build_env(1)["CARGO_BUILD_JOBS"], "1");
    assert_eq!(cargo::build_env(8)["CARGO_BUILD_JOBS"], "8");
    assert_eq!(cargo::DEFAULT_JOBS, 2, "a session has 2 vCPU");
}

#[test]
fn the_cargo_home_is_a_hidden_directory_of_the_engines_home() {
    assert_eq!(
        cargo::home_dir(std::path::Path::new("/var/lib/agentd/run/home")),
        PathBuf::from("/var/lib/agentd/run/home/.cargo")
    );
}

#[test]
fn the_proxys_config_is_written_under_the_cargo_home_and_named_by_cargo_home() {
    let home = engine_home("cargo-home").join(".cargo");
    let vars = cargo::place_config(&home, "[net]\nretry = 3\n", None).unwrap();
    assert_eq!(
        vars,
        BTreeMap::from([("CARGO_HOME".to_string(), home.display().to_string())])
    );
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        "[net]\nretry = 3\n"
    );
    // Written again (a retried start), it replaces the file.
    cargo::place_config(&home, "[net]\nretry = 5\n", None).unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        "[net]\nretry = 5\n"
    );
}

#[test]
fn a_cargo_home_that_cannot_be_made_is_an_error_not_a_panic() {
    let dir = common::scratch("cargo-blocked");
    let blocker = dir.join("file");
    std::fs::write(&blocker, "x").unwrap();
    assert!(cargo::place_config(&blocker.join(".cargo"), "x", None).is_err());
}

#[test]
fn the_job_count_is_two_unless_the_deployment_says_otherwise() {
    assert_eq!(direct(&[]).cargo_jobs, 2);
    assert_eq!(direct(&[]), DirectConfig::default());
    assert_eq!(direct(&[("AGENTD_DIRECT_CARGO_JOBS", "4")]).cargo_jobs, 4);
    assert_eq!(direct(&[("AGENTD_DIRECT_CARGO_JOBS", " 3 ")]).cargo_jobs, 3);
    assert_eq!(direct(&[("AGENTD_DIRECT_CARGO_JOBS", "1")]).cargo_jobs, 1);
    for bad in ["0", "-1", "two", "", "2.5"] {
        assert_eq!(
            direct(&[("AGENTD_DIRECT_CARGO_JOBS", bad)]).cargo_jobs,
            2,
            "{bad:?} is the default"
        );
    }
    assert_eq!(
        direct(&[("AGENTD_DIRECT_CARGO_JOBS", "1000")]).cargo_jobs,
        cargo::MAX_JOBS
    );
    assert_eq!(cargo::MAX_JOBS, 64);
}

#[test]
fn the_rust_bin_directory_joins_the_engines_path_before_the_system_directories() {
    let c = config(&[("AGENTD_ENGINE_PATH", IMAGE_PATH)]);
    assert_eq!(
        c.engine_path,
        paths(&[
            "/opt/harness/bin",
            "/opt/harness/gate-tools/lang/bin",
            RUST_BIN,
            "/usr/local/bin",
            "/opt/harness/gate-tools/bin",
            "/usr/bin",
            "/bin",
        ])
    );
}

#[test]
fn the_default_path_gains_it_too() {
    assert_eq!(
        config(&[]).engine_path,
        paths(&[
            "/opt/harness/bin",
            RUST_BIN,
            "/usr/local/bin",
            "/usr/bin",
            "/bin"
        ])
    );
}

#[test]
fn a_path_with_no_system_directory_gets_it_at_the_end() {
    let c = config(&[("AGENTD_ENGINE_PATH", "/opt/harness/bin:/engine")]);
    assert_eq!(
        c.engine_path,
        paths(&["/opt/harness/bin", "/engine", RUST_BIN])
    );
}

#[test]
fn a_path_that_already_names_it_is_left_alone() {
    let given = format!("/opt/harness/bin:{RUST_BIN}:/usr/bin:/bin");
    let c = config(&[("AGENTD_ENGINE_PATH", given.as_str())]);
    assert_eq!(
        c.engine_path,
        paths(&["/opt/harness/bin", RUST_BIN, "/usr/bin", "/bin"])
    );
}

#[test]
fn a_deployment_can_move_or_switch_off_the_rust_directory() {
    let c = config(&[
        ("AGENTD_ENGINE_PATH", "/opt/harness/bin:/usr/bin:/bin"),
        ("AGENTD_RUST_BIN", "/toolchains/rust/bin"),
    ]);
    assert_eq!(
        c.engine_path,
        paths(&[
            "/opt/harness/bin",
            "/toolchains/rust/bin",
            "/usr/bin",
            "/bin"
        ])
    );
    let off = config(&[
        ("AGENTD_ENGINE_PATH", "/opt/harness/bin:/usr/bin:/bin"),
        ("AGENTD_RUST_BIN", ""),
    ]);
    assert_eq!(
        off.engine_path,
        paths(&["/opt/harness/bin", "/usr/bin", "/bin"])
    );
}

// ---- the placement does not follow what the engine planted -------------------------------
//
// agentd writes the file as root while the home belongs to the engine, and a run's home can hold what an earlier invocation of the engine left.
// Nothing here may be written through a link: a link at `.cargo` or at `config.toml` is refused and its target is untouched.

#[test]
fn the_placed_directory_and_file_belong_to_the_engine_user_and_the_engine_can_work_in_them() {
    let home = engine_home("cargo-modes");
    let dir = home.join(".cargo");
    cargo::place_config(&dir, "[net]\nretry = 3\n", Some(me())).unwrap();
    let (d, f) = (
        std::fs::metadata(&dir).unwrap(),
        std::fs::metadata(dir.join("config.toml")).unwrap(),
    );
    assert!(d.is_dir() && f.is_file());
    assert_eq!((d.uid(), d.gid()), me());
    assert_eq!((f.uid(), f.gid()), me());
    // The engine's cargo makes `registry/`, `.package-cache` and `git/` here: the owner has every permission, and the others read only.
    assert_eq!(d.permissions().mode() & 0o777, 0o755);
    assert_eq!(f.permissions().mode() & 0o777, 0o644);
    assert_eq!(names(&dir), ["config.toml"], "no temporary file is left");
    std::fs::create_dir(dir.join("registry")).unwrap();
    std::fs::write(dir.join(".package-cache"), "").unwrap();
}

#[test]
fn a_symlink_in_place_of_the_cargo_directory_is_not_followed() {
    let home = engine_home("cargo-link-dir");
    let outside = common::scratch("cargo-outside");
    std::fs::write(outside.join("config.toml"), "keep").unwrap();
    symlink(&outside, home.join(".cargo")).unwrap();
    assert!(cargo::place_config(&home.join(".cargo"), "[net]\n", Some(me())).is_err());
    assert_eq!(
        std::fs::read_to_string(outside.join("config.toml")).unwrap(),
        "keep"
    );
    assert_eq!(names(&outside), ["config.toml"], "nothing was made there");
    assert!(
        std::fs::symlink_metadata(home.join(".cargo"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is left as the engine made it"
    );
}

#[test]
fn a_symlink_in_place_of_config_toml_is_not_followed() {
    let home = engine_home("cargo-link-file");
    let dir = home.join(".cargo");
    std::fs::create_dir(&dir).unwrap();
    let victim = common::scratch("cargo-victim").join("passwd");
    std::fs::write(&victim, "do not touch").unwrap();
    symlink(&victim, dir.join("config.toml")).unwrap();
    assert!(cargo::place_config(&dir, "[net]\n", Some(me())).is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do not touch");
    assert!(
        std::fs::symlink_metadata(dir.join("config.toml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn a_dangling_symlink_at_config_toml_is_not_followed_either() {
    // A write through it would create the target.
    let home = engine_home("cargo-link-dangling");
    let dir = home.join(".cargo");
    std::fs::create_dir(&dir).unwrap();
    let target = common::scratch("cargo-dangling").join("new-file");
    symlink(&target, dir.join("config.toml")).unwrap();
    assert!(cargo::place_config(&dir, "[net]\n", None).is_err());
    assert!(!target.exists(), "the write went through the link");
}

#[test]
fn a_cargo_path_that_is_not_a_plain_directory_or_file_is_refused() {
    let home = engine_home("cargo-not-plain");
    // `.cargo` is a regular file
    std::fs::write(home.join(".cargo"), "x").unwrap();
    assert!(cargo::place_config(&home.join(".cargo"), "[net]\n", None).is_err());
    // `config.toml` is a directory
    let home = engine_home("cargo-not-plain-2");
    std::fs::create_dir_all(home.join(".cargo/config.toml")).unwrap();
    assert!(cargo::place_config(&home.join(".cargo"), "[net]\n", None).is_err());
    // the engine home itself is missing
    let gone = common::scratch("cargo-no-home").join("home");
    assert!(cargo::place_config(&gone.join(".cargo"), "[net]\n", None).is_err());
}

#[test]
fn a_hard_link_at_config_toml_is_replaced_and_the_other_name_keeps_its_bytes() {
    let home = engine_home("cargo-hard-link");
    let dir = home.join(".cargo");
    std::fs::create_dir(&dir).unwrap();
    let other = common::scratch("cargo-hard-other").join("other");
    std::fs::write(&other, "other name").unwrap();
    std::fs::hard_link(&other, dir.join("config.toml")).unwrap();
    cargo::place_config(&dir, "[net]\nretry = 3\n", None).unwrap();
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "other name");
    assert_eq!(
        std::fs::read_to_string(dir.join("config.toml")).unwrap(),
        "[net]\nretry = 3\n"
    );
}

#[test]
fn an_earlier_config_file_is_replaced_whatever_its_mode() {
    let home = engine_home("cargo-replace");
    let dir = home.join(".cargo");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "[build]\nrustc-wrapper = \"x\"\n").unwrap();
    std::fs::set_permissions(
        dir.join("config.toml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    cargo::place_config(&dir, "[net]\nretry = 3\n", Some(me())).unwrap();
    let f = std::fs::metadata(dir.join("config.toml")).unwrap();
    assert_eq!(f.permissions().mode() & 0o777, 0o644);
    assert_eq!(
        std::fs::read_to_string(dir.join("config.toml")).unwrap(),
        "[net]\nretry = 3\n"
    );
    assert_eq!(names(&dir), ["config.toml"]);
}

/// Only root can give files to another user, so this runs where the suite runs as root and says so where it does not.
#[test]
fn run_as_root_the_result_belongs_to_the_engine_user_and_a_directory_of_a_third_user_is_refused() {
    if me().0 != 0 {
        eprintln!(
            "SKIPPED: the suite does not run as root, so no file can be given to another user"
        );
        return;
    }
    let engine = (65534u32, 65534u32);
    let home = engine_home("cargo-root");
    let dir = home.join(".cargo");
    cargo::place_config(&dir, "[net]\nretry = 3\n", Some(engine)).unwrap();
    for p in [dir.clone(), dir.join("config.toml")] {
        let m = std::fs::metadata(&p).unwrap();
        assert_eq!((m.uid(), m.gid()), engine, "{}", p.display());
    }
    // The second invocation of a run finds the directory the first one gave to the engine: that is the expected owner.
    cargo::place_config(&dir, "[net]\nretry = 5\n", Some(engine)).unwrap();
    // A directory owned by anyone else is not.
    let home = engine_home("cargo-root-third");
    let dir = home.join(".cargo");
    std::fs::create_dir(&dir).unwrap();
    std::os::unix::fs::chown(&dir, Some(12345), Some(12345)).unwrap();
    assert!(cargo::place_config(&dir, "[net]\n", Some(engine)).is_err());
}

// ---- the target folder leaves the 1 GiB checkout mount ---------------------------------------------------------------------------
//
// The checkout is on the session's managed storage, which holds 1 GiB and reports no use. A cargo `target/` in it filled the mount
// (965 MB in one `cargo test --workspace --no-run`) and the next write failed. The root volume has 5.7 GiB free, so the engine's cargo
// puts its target folder there, outside the checkout, unless the deployment says otherwise.

#[test]
fn the_target_setting_is_the_root_volume_folder_unless_the_deployment_names_another_or_checkout() {
    use cargo::TargetSetting as T;
    assert_eq!(direct(&[]).cargo_target, T::Default);
    assert_eq!(
        direct(&[("AGENTD_CARGO_TARGET_DIR", "")]).cargo_target,
        T::Default
    );
    assert_eq!(
        direct(&[("AGENTD_CARGO_TARGET_DIR", "   ")]).cargo_target,
        T::Default
    );
    // `checkout` keeps cargo's own default: a `target/` in the checkout.
    for v in ["checkout", " checkout ", "Checkout", "CHECKOUT"] {
        assert_eq!(
            direct(&[("AGENTD_CARGO_TARGET_DIR", v)]).cargo_target,
            T::Checkout,
            "{v:?}"
        );
    }
    // An absolute folder is taken as it is.
    assert_eq!(
        direct(&[("AGENTD_CARGO_TARGET_DIR", " /var/cache/cargo-target ")]).cargo_target,
        T::Dir(PathBuf::from("/var/cache/cargo-target"))
    );
    assert_eq!(
        direct(&[("AGENTD_CARGO_TARGET_DIR", "/var/cache/cargo-target/")]).cargo_target,
        T::Dir(PathBuf::from("/var/cache/cargo-target"))
    );
}

#[test]
fn a_target_setting_that_is_not_an_absolute_plain_path_is_the_default() {
    use cargo::TargetSetting as T;
    for bad in [
        "target",
        "./target",
        "../target",
        "~/target",
        "/var/../etc",
        "/var/./x",
        "/",
        "relative/path",
        "/with\0nul",
        "on",
        "none",
    ] {
        assert_eq!(
            direct(&[("AGENTD_CARGO_TARGET_DIR", bad)]).cargo_target,
            T::Default,
            "{bad:?} is the default"
        );
    }
}

#[test]
fn the_target_folder_is_made_beside_the_runs_other_folders_and_checkout_has_none() {
    use cargo::TargetSetting as T;
    let agent_data = Path::new("/var/lib/agentd/run1/direct");
    assert_eq!(
        cargo::target_dir(&T::Default, agent_data),
        Some(PathBuf::from("/var/lib/agentd/run1/direct/cargo-target"))
    );
    assert_eq!(cargo::target_dir(&T::Checkout, agent_data), None);
    assert_eq!(
        cargo::target_dir(&T::Dir(PathBuf::from("/srv/t")), agent_data),
        Some(PathBuf::from("/srv/t"))
    );
    assert_eq!(cargo::TARGET_FOLDER, "cargo-target");
}

#[test]
fn the_placed_target_folder_is_a_plain_directory_the_engine_owns_and_cargo_is_told_where_it_is() {
    let base = common::scratch("cargo-target-place");
    let dir = base.join("cargo-target");
    let vars = cargo::place_target_dir(&dir, Some(me())).unwrap();
    assert_eq!(
        vars,
        BTreeMap::from([("CARGO_TARGET_DIR".to_string(), dir.display().to_string())])
    );
    let m = std::fs::metadata(&dir).unwrap();
    assert!(m.is_dir());
    assert_eq!((m.uid(), m.gid()), me());
    assert_eq!(m.permissions().mode() & 0o777, 0o755);
    // A second invocation of the same run finds the folder and keeps what is in it: the build cache survives.
    std::fs::write(dir.join("kept"), "x").unwrap();
    cargo::place_target_dir(&dir, Some(me())).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("kept")).unwrap(), "x");
}

#[test]
fn a_target_folder_whose_parent_is_missing_is_made_with_it() {
    let base = common::scratch("cargo-target-parents");
    let dir = base.join("a/b/cargo-target");
    cargo::place_target_dir(&dir, None).unwrap();
    assert!(dir.is_dir());
}

#[test]
fn a_symlink_in_place_of_the_target_folder_is_not_followed_not_even_into_the_checkout() {
    let base = common::scratch("cargo-target-link");
    let checkout = common::scratch("cargo-target-checkout");
    symlink(&checkout, base.join("cargo-target")).unwrap();
    assert!(cargo::place_target_dir(&base.join("cargo-target"), Some(me())).is_err());
    assert!(names(&checkout).is_empty(), "nothing was made there");
    assert!(
        std::fs::symlink_metadata(base.join("cargo-target"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is left as it was found"
    );
}

#[test]
fn a_file_in_place_of_the_target_folder_is_refused() {
    let base = common::scratch("cargo-target-file");
    std::fs::write(base.join("cargo-target"), "x").unwrap();
    assert!(cargo::place_target_dir(&base.join("cargo-target"), None).is_err());
}

#[test]
fn run_as_root_the_target_folder_belongs_to_the_engine_user_and_a_third_users_folder_is_refused() {
    if me().0 != 0 {
        eprintln!(
            "SKIPPED: the suite does not run as root, so no folder can be given to another user"
        );
        return;
    }
    let engine = (65534u32, 65534u32);
    let base = common::scratch("cargo-target-root");
    let dir = base.join("cargo-target");
    cargo::place_target_dir(&dir, Some(engine)).unwrap();
    let m = std::fs::metadata(&dir).unwrap();
    assert_eq!((m.uid(), m.gid()), engine);
    cargo::place_target_dir(&dir, Some(engine)).unwrap();
    let other = base.join("third");
    std::fs::create_dir(&other).unwrap();
    std::os::unix::fs::chown(&other, Some(12345), Some(12345)).unwrap();
    assert!(cargo::place_target_dir(&other, Some(engine)).is_err());
}
