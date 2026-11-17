//! Git in a scratch copy of the run's repository.
//!
//! The review round asks the engine for a scratch worktree of the starting commit outside the repository
//! (`git worktree add --detach "$(mktemp -d)/base" BASE`), and `mktemp` puts it under the session's temp folder. In 51 of 65 runs on a test
//! stage git then refused every command inside the scratch copy: `fatal: detected dubious ownership in repository at
//! '/var/lib/agentd/<run>/direct/tmp/tmp.<x>/<name>'`.
//!
//! The cause is who the folders belong to, as git reads it. For a linked worktree git compares three paths with the user it runs as: the
//! `.git` file of the scratch folder, the scratch folder, and the worktree's own folder inside the repository's `.git`
//! (`<repo>/.git/worktrees/<name>`). The first two are on the machine's own disk and belong to the engine's user. The third is on the session
//! storage with the checkout, and that storage reports root as the owner of whatever the engine's user makes there (a file the engine's own
//! test run wrote lists as `root root`, while the checkout, which agentd gives to the engine's user by name, lists as the engine's). The
//! repository itself passes git's check because agentd gave it and its `.git` to the engine's user; the worktree's folder was made later.
//!
//! The fix is in the engine's own environment (`engine::launch::base_env`): git's `safe.directory` setting for the run's repository, for
//! what is inside it and for what is inside the run's temp folder. These tests start real git with the launch agentd builds for each
//! engine. A normal user cannot make a folder of another user, so the first tests use git's own switch for "these paths are another
//! user's" (`GIT_TEST_ASSUME_DIFFERENT_OWNER`), and each of them first shows that git refuses without agentd's settings. The last test has
//! two real owners and needs root (it is listed as ignored; run it as root in a container, `crates/agentd/AGENTS.md`).
#![allow(clippy::unwrap_used)]

mod common;

use agentd::engine::launch::{
    BaseEnv, GIT_CONFIG_COUNT, GIT_CONFIG_COUNT_MAX, Launch, SAFE_DIRECTORY, Standalone,
    add_git_safe_directories, base_env, git_safe_directories, standalone,
};
use agentd::engine::user_settings::UserSettings;
use agentd::engine::{LaunchSpec, chown_tree, engine_identity, spawn};
use common::{git, repo_root, scratch};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::AsyncReadExt;

const ENGINES: [&str; 2] = ["cline", "opencode"];
const REFUSED: &str = "dubious ownership";
/// git's own switch (its test suite uses it): every path counts as another user's, so only `safe.directory` decides.
const OTHER_OWNER: &str = "GIT_TEST_ASSUME_DIFFERENT_OWNER";

/// The launch agentd builds for `engine` in a scratch folder: the environment block of a direct run (`base_env`), the repository at
/// `<root>/repo` and the temp folder at `<root>/tmp`.
fn launch_of(engine: &str, root: &Path) -> Launch {
    standalone(&Standalone {
        engine,
        profiles_dir: &repo_root().join("engine/profiles"),
        root,
        model: "anthropic.claude-sonnet-5-5",
        region: "us-west-2",
        command_timeout_s: None,
        endpoint: None,
        idle_timeout: None,
    })
    .unwrap()
}

/// The variable that the repository's CI entry point exports for every step of a round of CI. The script is not read here: a published
/// copy of the repository does not ship it. A test of that script holds its name for a round equal to this one.
const CI_ROUND: &str = "HAMH_CI_RUNNING";

/// Whether the git that printed `version` (the line of `git --version`) reads the leading-path form of `safe.directory` (`<folder>/*`).
/// git 2.46 and newer do; an older git reads it as one literal path, so the tests that start real git with that form cannot run with it.
/// Outside a round of CI such a git gives `Ok(false)`: the caller prints `NOT RUN` and stops. Inside a round it is an error: a round that
/// skipped these tests would prove nothing about the fix and would show no failure.
fn leading_path_form(version: &str, ci_round: bool) -> Result<bool, String> {
    let found = version.trim();
    let mut parts = found
        .trim_start_matches("git version ")
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) >= (2, 46) {
        return Ok(true);
    }
    if ci_round {
        return Err(format!(
            "`{found}` is older than git 2.46, so the tests of git's settings for a run's own folders cannot run in this round of CI: \
             install git 2.46 or newer on the host that runs the round"
        ));
    }
    Ok(false)
}

/// Whether the tests that need the leading-path form can run with the git of this machine ([`leading_path_form`]).
fn git_has_leading_path_form() -> bool {
    let out = Command::new("git").arg("--version").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let ci_round = std::env::var_os(CI_ROUND).is_some_and(|v| !v.is_empty());
    match leading_path_form(&text, ci_round) {
        Ok(true) => true,
        Ok(false) => {
            println!("NOT RUN: {} is older than 2.46", text.trim());
            false
        }
        Err(why) => panic!("{why}"),
    }
}

#[test]
fn b859_an_older_git_fails_a_round_of_ci_and_is_named_outside_one() {
    for new in [
        "git version 2.46.0",
        "git version 2.47.3",
        "git version 2.50.1\n",
        "git version 2.53.0",
        "git version 2.100.0",
        "git version 3.0.0",
        "git version 2.47.3.windows.1",
    ] {
        for ci_round in [false, true] {
            assert_eq!(
                leading_path_form(new, ci_round),
                Ok(true),
                "{new:?}, in a round of CI: {ci_round}"
            );
        }
    }
    for old in [
        "git version 2.45.2",
        "git version 2.34.1\n",
        "git version 1.8.5",
        "git version 2",
        "",
        "not git",
    ] {
        // Outside a round the caller says `NOT RUN` and stops.
        assert_eq!(leading_path_form(old, false), Ok(false), "{old:?}");
        // Inside a round the test fails, and the message names the git that was found and the version that is needed.
        let why =
            leading_path_form(old, true).expect_err("an older git is an error in a round of CI");
        assert!(
            why.contains(old.trim()) && why.contains("2.46"),
            "{old:?}: {why}"
        );
    }
}

/// What of the engine's environment a git command of the engine's shell reads: git's own variables, the home and the temp folder.
fn git_part(env: &BTreeMap<String, String>, with_settings: bool) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(k, _)| {
            matches!(k.as_str(), "HOME" | "TMPDIR") || (with_settings && k.starts_with("GIT_"))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `git <args>` in `dir` as a command of the engine's shell would run it, with every path counted as another user's. No system
/// configuration of the build machine is read; the global one is the (empty) home of the launch. Returns whether git succeeded and what it
/// printed on standard error.
fn engine_git(env: &BTreeMap<String, String>, dir: &Path, args: &[&str]) -> (bool, String) {
    let (code, _, err) = engine_run(env, dir, &[&["git"][..], args].concat());
    (code == Some(0), err)
}

/// `argv` in `dir` the same way: a command of the engine's shell that starts git through another program (`env -u NAME git ...`).
/// Returns the exit code and what the command printed on standard output and on standard error.
fn engine_run(
    env: &BTreeMap<String, String>,
    dir: &Path,
    argv: &[&str],
) -> (Option<i32>, String, String) {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LANG", "C.UTF-8")
        .envs(env)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(OTHER_OWNER, "1")
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

/// A repository with one commit at the launch's working directory, and a scratch worktree of it under the launch's temp folder, made the
/// way the review round asks for it. Returns the scratch worktree.
fn repo_with_scratch_worktree(launch: &Launch, name: &str) -> PathBuf {
    let repo = &launch.cwd;
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(repo, &["add", "a.txt"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    // `mktemp -d` of the engine's shell: a folder with a name of its own under the temp folder the launch names.
    let made = Path::new(&launch.env["TMPDIR"]).join("tmp.b859scratch");
    std::fs::create_dir_all(&made).unwrap();
    let scratch_tree = made.join(name);
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            scratch_tree.to_str().unwrap(),
            "HEAD",
        ],
    );
    scratch_tree
}

#[test]
fn b859_git_works_in_a_scratch_worktree_whose_folders_show_another_owner() {
    if !git_has_leading_path_form() {
        return;
    }
    for engine in ENGINES {
        let root = scratch(&format!("b859-{engine}"));
        let launch = launch_of(engine, &root);
        let tree = repo_with_scratch_worktree(&launch, "base");

        // The trigger: without agentd's git settings, git refuses the scratch copy in the words of the saved runs.
        let (ok, err) = engine_git(
            &git_part(&launch.env, false),
            &tree,
            &["status", "--porcelain"],
        );
        assert!(
            !ok && err.contains(REFUSED),
            "{engine}: TRIGGER NOT PRESENT: {err}"
        );

        // With the launch's own environment, the commands of the review round work in the scratch copy.
        let env = git_part(&launch.env, true);
        for args in [
            &["status", "--porcelain"][..],
            &["checkout", "-q", "HEAD", "--", "a.txt"][..],
            &["rev-parse", "HEAD"][..],
        ] {
            let (ok, err) = engine_git(&env, &tree, args);
            assert!(ok, "{engine}: git {args:?} in the scratch worktree: {err}");
        }
    }
}

#[test]
fn b859_git_works_in_the_repository_and_in_a_repository_inside_it() {
    if !git_has_leading_path_form() {
        return;
    }
    for engine in ENGINES {
        let root = scratch(&format!("b859-in-{engine}"));
        let launch = launch_of(engine, &root);
        repo_with_scratch_worktree(&launch, "base");
        // A repository the engine makes inside the checkout (a submodule, a fixture of the project's tests) is on the session storage too.
        let inner = launch.cwd.join("vendor/inner");
        std::fs::create_dir_all(&inner).unwrap();
        git(&inner, &["init", "-q", "-b", "main"]);

        let (ok, err) = engine_git(
            &git_part(&launch.env, false),
            &launch.cwd,
            &["status", "--porcelain"],
        );
        assert!(
            !ok && err.contains(REFUSED),
            "{engine}: TRIGGER NOT PRESENT: {err}"
        );

        let env = git_part(&launch.env, true);
        for dir in [&launch.cwd, &inner] {
            let (ok, err) = engine_git(&env, dir, &["status", "--porcelain"]);
            assert!(ok, "{engine}: git status in {}: {err}", dir.display());
        }
    }
}

#[test]
fn b859_a_repository_outside_the_runs_folders_is_still_refused() {
    if !git_has_leading_path_form() {
        return;
    }
    for engine in ENGINES {
        let root = scratch(&format!("b859-out-{engine}"));
        let launch = launch_of(engine, &root);
        // Beside the repository and the temp folder, not inside either: the home folder of the launch, a folder of the scratch root,
        // folders whose names begin like the repository's and the temp folder's, the temp folder itself (only what is inside it is
        // named) and, last, the folder that holds them all.
        for other in [
            root.join("home/other"),
            root.join("elsewhere"),
            root.join("repo2"),
            root.join("repo.bak"),
            root.join("tmpx"),
            root.join("tmp"),
            root.clone(),
        ] {
            std::fs::create_dir_all(&other).unwrap();
            git(&other, &["init", "-q", "-b", "main"]);
            let (ok, err) = engine_git(
                &git_part(&launch.env, true),
                &other,
                &["status", "--porcelain"],
            );
            assert!(
                !ok && err.contains(REFUSED),
                "{engine}: git accepted {} as safe: {err}",
                other.display()
            );
        }
        // No value is the wildcard, and each one names the repository or the temp folder of this launch.
        let values: Vec<&String> = launch
            .env
            .iter()
            .filter(|(k, _)| k.starts_with("GIT_CONFIG_VALUE_"))
            .map(|(_, v)| v)
            .collect();
        assert!(
            !values.is_empty(),
            "{engine}: the launch carries no git setting"
        );
        let (repo, tmp) = (
            launch.cwd.display().to_string(),
            launch.env["TMPDIR"].clone(),
        );
        for v in values {
            assert!(
                *v == repo || *v == format!("{repo}/*") || *v == format!("{tmp}/*"),
                "{engine}: {v}"
            );
        }
    }
}

/// `env` with one value of its sequence of git settings left out, and the sequence numbered again.
fn without_value(env: &BTreeMap<String, String>, value: &str) -> BTreeMap<String, String> {
    let count: usize = env[GIT_CONFIG_COUNT].parse().unwrap();
    let kept: Vec<(String, String)> = (0..count)
        .map(|n| {
            (
                env[&format!("GIT_CONFIG_KEY_{n}")].clone(),
                env[&format!("GIT_CONFIG_VALUE_{n}")].clone(),
            )
        })
        .filter(|(_, v)| v != value)
        .collect();
    assert_eq!(kept.len() + 1, count, "the sequence holds {value} once");
    let mut out: BTreeMap<String, String> = env
        .iter()
        .filter(|(k, _)| !k.starts_with("GIT_CONFIG_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.insert(GIT_CONFIG_COUNT.to_string(), kept.len().to_string());
    for (n, (k, v)) in kept.into_iter().enumerate() {
        out.insert(format!("GIT_CONFIG_KEY_{n}"), k);
        out.insert(format!("GIT_CONFIG_VALUE_{n}"), v);
    }
    out
}

/// A command of the engine's shell that brings git settings of its own in its environment (`GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=...
/// git ...`, as two commands of the saved runs did) replaces the run's sequence for that command: git reads one sequence, the one its
/// count names. The pages and the crate's contract say so; this test holds git to it. The repository itself is used, whose value has no
/// pattern, so the test runs with every git that knows `safe.directory`.
#[test]
fn b859_a_command_with_its_own_count_loses_the_runs_settings() {
    for engine in ENGINES {
        let root = scratch(&format!("b859-own-{engine}"));
        let launch = launch_of(engine, &root);
        let repo = &launch.cwd;
        git(repo, &["init", "-q", "-b", "main"]);
        let env = git_part(&launch.env, true);

        // The trigger: with the run's settings git works in the repository although every path counts as another user's.
        let (ok, err) = engine_git(&env, repo, &["status", "--porcelain"]);
        assert!(ok, "{engine}: TRIGGER NOT PRESENT: {err}");

        // The command's own sequence of one setting takes the place of the run's three.
        let mut own = env.clone();
        for (k, v) in [
            (GIT_CONFIG_COUNT, "1"),
            ("GIT_CONFIG_KEY_0", "core.abbrev"),
            ("GIT_CONFIG_VALUE_0", "12"),
        ] {
            own.insert(k.to_string(), v.to_string());
        }
        let (ok, err) = engine_git(&own, repo, &["status", "--porcelain"]);
        assert!(
            !ok && err.contains(REFUSED),
            "{engine}: git read the run's settings beside the command's own: {err}"
        );
    }
}

/// The pages name one way to run a command with git's own owner check: a rule tells the engine to start it with
/// `env -u GIT_CONFIG_COUNT`. Without the count git reads none of the run's values, whatever pairs the environment still holds. This test
/// holds git to it, with the command line of the pages.
#[test]
fn b859_a_command_without_the_count_gets_gits_owner_check_back() {
    for engine in ENGINES {
        let root = scratch(&format!("b859-unset-{engine}"));
        let launch = launch_of(engine, &root);
        let repo = &launch.cwd;
        git(repo, &["init", "-q", "-b", "main"]);
        let env = git_part(&launch.env, true);
        let unset = ["env", "-u", GIT_CONFIG_COUNT, "git"];
        let read = ["config", "--get-all", SAFE_DIRECTORY];

        // The trigger: with the run's settings git works in the repository and reads the values.
        let (ok, err) = engine_git(&env, repo, &["status", "--porcelain"]);
        assert!(ok, "{engine}: TRIGGER NOT PRESENT: {err}");
        let (ok, err) = engine_git(&env, Path::new("/"), &read);
        assert!(
            ok,
            "{engine}: TRIGGER NOT PRESENT: git reads no value: {err}"
        );

        let status = [&unset[..], &["status", "--porcelain"][..]].concat();
        let (code, _, err) = engine_run(&env, repo, &status);
        assert!(
            code != Some(0) && err.contains(REFUSED),
            "{engine}: git still read the run's settings: {code:?} {err}"
        );
        // `git config --get-all` exits 1 and prints nothing for a key that has no value.
        let (code, out, err) = engine_run(&env, Path::new("/"), &[&unset[..], &read[..]].concat());
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (Some(1), "", ""),
            "{engine}: git still read a value of safe.directory"
        );
    }
}

const RUN_REPO: &str = "/mnt/workspace/01RUN/direct/repo";
const RUN_TMP: &str = "/var/lib/agentd/01RUN/direct/tmp";

#[test]
fn b859_the_settings_name_the_repository_what_is_in_it_and_what_is_in_the_temp_folder() {
    assert_eq!(
        git_safe_directories(Path::new(RUN_REPO), Path::new(RUN_TMP)),
        [
            RUN_REPO.to_string(),
            format!("{RUN_REPO}/*"),
            format!("{RUN_TMP}/*")
        ]
    );
    // A folder that is no absolute path gives no value: git warns about such a value in every command that reads it.
    assert!(git_safe_directories(Path::new("repo"), Path::new("tmp")).is_empty());
    assert_eq!(
        git_safe_directories(Path::new("repo"), Path::new(RUN_TMP)),
        [format!("{RUN_TMP}/*")]
    );
}

/// The environment block of a direct run for `engine`, with `profile_env` as the engine profile's own launch environment.
fn block(engine: &str, profile_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    base_env(BaseEnv {
        home: Path::new("/var/lib/agentd/01RUN/direct/home"),
        tmp: Path::new(RUN_TMP),
        repo: Path::new(RUN_REPO),
        engine,
        profile_env,
        built_in: BTreeMap::new(),
        settings: &UserSettings::default(),
        aws: BTreeMap::new(),
        env_remove: &[],
    })
}

#[test]
fn b859_the_environment_block_of_both_engines_carries_the_settings_after_a_sequence_that_is_there()
{
    let wanted = git_safe_directories(Path::new(RUN_REPO), Path::new(RUN_TMP));
    for engine in ENGINES {
        let plain = block(engine, &BTreeMap::new());
        assert_eq!(plain[GIT_CONFIG_COUNT], "3", "{engine}");
        for (i, value) in wanted.iter().enumerate() {
            assert_eq!(
                plain[&format!("GIT_CONFIG_KEY_{i}")],
                SAFE_DIRECTORY,
                "{engine}"
            );
            assert_eq!(&plain[&format!("GIT_CONFIG_VALUE_{i}")], value, "{engine}");
        }

        // A sequence that a source before agentd's brought stays as it is, and agentd's values follow it.
        let own: BTreeMap<String, String> = [
            (GIT_CONFIG_COUNT, "1"),
            ("GIT_CONFIG_KEY_0", "core.autocrlf"),
            ("GIT_CONFIG_VALUE_0", "false"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let kept = block(engine, &own);
        assert_eq!(kept[GIT_CONFIG_COUNT], "4", "{engine}");
        assert_eq!(kept["GIT_CONFIG_KEY_0"], "core.autocrlf", "{engine}");
        assert_eq!(kept["GIT_CONFIG_VALUE_0"], "false", "{engine}");
        for (i, value) in wanted.iter().enumerate() {
            assert_eq!(
                kept[&format!("GIT_CONFIG_KEY_{}", i + 1)],
                SAFE_DIRECTORY,
                "{engine}"
            );
            assert_eq!(
                &kept[&format!("GIT_CONFIG_VALUE_{}", i + 1)],
                value,
                "{engine}"
            );
        }

        // git itself reads the whole sequence: the earlier setting and the three values, in order.
        let read = |key: &str| {
            let out = Command::new("git")
                .args(["config", "--get-all", key])
                .current_dir("/")
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .envs(git_part(&kept, true))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(read("core.autocrlf"), ["false"], "{engine}");
        assert_eq!(read(SAFE_DIRECTORY), wanted, "{engine}");
    }
}

#[test]
fn b859_a_count_that_is_no_number_is_left_as_it_is() {
    // What git refuses as a count: no number, something after the digits, a count above its bound, and one that the three values would
    // take above it (the last three).
    let above = (GIT_CONFIG_COUNT_MAX + 1).to_string();
    let no_room = (GIT_CONFIG_COUNT_MAX - 2).to_string();
    let largest = usize::MAX.to_string();
    for count in [
        "two",
        "-1",
        "1 2",
        "3 ",
        "  ",
        "0x10",
        above.as_str(),
        no_room.as_str(),
        largest.as_str(),
    ] {
        let mut env: BTreeMap<String, String> =
            [(GIT_CONFIG_COUNT.to_string(), count.to_string())].into();
        let before = env.clone();
        add_git_safe_directories(&mut env, Path::new(RUN_REPO), Path::new(RUN_TMP));
        assert_eq!(env, before, "{count:?}");
    }
    // What git reads as a count: an empty one is zero, and white space before the digits is skipped.
    for (count, first) in [("", 0), (" 1", 1), ("007", 7)] {
        let mut env: BTreeMap<String, String> =
            [(GIT_CONFIG_COUNT.to_string(), count.to_string())].into();
        add_git_safe_directories(&mut env, Path::new(RUN_REPO), Path::new(RUN_TMP));
        assert_eq!(env[GIT_CONFIG_COUNT], (first + 3).to_string(), "{count:?}");
        assert_eq!(
            env[&format!("GIT_CONFIG_KEY_{first}")],
            SAFE_DIRECTORY,
            "{count:?}"
        );
        assert_eq!(
            env[&format!("GIT_CONFIG_VALUE_{}", first + 2)],
            format!("{RUN_TMP}/*"),
            "{count:?}"
        );
    }
}

/// `sh -c <script>` as `owner` through agentd's launch path, with `env`: the exit code and what it printed (both streams).
async fn sh_as(
    owner: (u32, u32),
    env: &BTreeMap<String, String>,
    script: &str,
    logs: &Path,
) -> (Option<i32>, String) {
    let spec = LaunchSpec {
        argv: vec!["sh".into(), "-c".into(), format!("{{ {script}; }} 2>&1")],
        env: env.clone(),
        cwd: PathBuf::from("/"),
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        stderr_log: logs.join("sh.err"),
        run_as: Some(owner),
        sandbox: None,
    };
    let mut child = spawn(&spec).unwrap();
    drop(child.stdin.take());
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    (child.wait().await.unwrap().code(), out)
}

/// Two real owners, as in a session: root (agentd) hands the checkout and the temp folder to the engine's user, the engine's user makes the
/// scratch worktree, and the worktree's folder inside `.git` then belongs to root, which is what the session storage reports for a folder
/// the engine's user made there.
#[tokio::test]
#[ignore = "needs root for two real owners; run as root in a container that has setpriv and the git of the worker image's release (crates/agentd/AGENTS.md)"]
async fn b859_two_owners_git_as_the_engine_user_works_in_the_scratch_worktree() {
    let euid = rustix::process::geteuid().as_raw();
    assert_eq!(
        euid, 0,
        "run this test as root: it drops to the engine user"
    );
    assert!(
        git_has_leading_path_form(),
        "this test needs git 2.46 or newer"
    );
    let owner = engine_identity(Some("nobody"))
        .await
        .expect("the engine user `nobody` resolves while running as root")
        .expect("running as root gives a uid split");
    for engine in ENGINES {
        let root = scratch(&format!("b859-owners-{engine}"));
        // The folder above the engine's own is root's and can be passed through, as `/var/lib/agentd/<run>/direct` is.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let launch = launch_of(engine, &root);
        let (repo, tmp, home) = (
            launch.cwd.clone(),
            PathBuf::from(&launch.env["TMPDIR"]),
            PathBuf::from(&launch.env["HOME"]),
        );
        // agentd, as root: the checkout with its first commit, then the engine's user owns what it writes (step 8 of a direct run).
        let root_git = |args: &[&str]| {
            let out = Command::new("git")
                .args([
                    "-c",
                    "safe.directory=*",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        root_git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        root_git(&["add", "a.txt"]);
        root_git(&["commit", "-q", "-m", "base"]);
        chown_tree(&[&repo, &tmp, &home], owner).await;
        let logs = scratch(&format!("b859-owners-logs-{engine}"));

        // The engine's user makes the scratch worktree; nothing differs yet, and git works in it without any setting.
        let bare = git_part(&launch.env, false);
        let tree = tmp.join("tmp.b859owners/base");
        let (code, out) = sh_as(
            owner,
            &bare,
            &format!(
                "mkdir -p {made} && cd {repo} && git worktree add -q --detach {tree} HEAD && git -C {tree} status --porcelain",
                made = tree.parent().unwrap().display(),
                repo = repo.display(),
                tree = tree.display()
            ),
            &logs,
        )
        .await;
        assert_eq!(
            code,
            Some(0),
            "{engine}: the scratch worktree as the engine's user: {out}"
        );

        // The session storage reports root for the folder the engine's user made inside `.git`.
        let entry = repo.join(".git/worktrees/base");
        chown_tree(&[&entry], (0, 0)).await;

        let status = format!("git -C {} status --porcelain", tree.display());
        let (code, out) = sh_as(owner, &bare, &status, &logs).await;
        assert!(
            code != Some(0) && out.contains(REFUSED),
            "{engine}: TRIGGER NOT PRESENT: {code:?} {out}"
        );

        let settings = git_part(&launch.env, true);
        let (code, out) = sh_as(owner, &settings, &status, &logs).await;
        assert_eq!(
            code,
            Some(0),
            "{engine}: git in the scratch worktree with the launch's settings: {out}"
        );

        // The scratch worktree as the source of a local transport, as the git of the worker image's release handles it. The process that
        // serves a local source does not check the owner there: `ls-remote` and `fetch` need no setting.
        let made = tree.parent().unwrap();
        let ls_remote = format!("git ls-remote {} HEAD", tree.display());
        let (code, out) = sh_as(owner, &bare, &ls_remote, &logs).await;
        assert_eq!(
            code,
            Some(0),
            "{engine}: ls-remote with the scratch worktree as the source, no settings: {out}"
        );
        let fetch = format!(
            "git init -q {to} && git -C {to} fetch -q {tree} HEAD",
            to = made.join("fetched").display(),
            tree = tree.display()
        );
        let (code, out) = sh_as(owner, &bare, &fetch, &logs).await;
        assert_eq!(
            code,
            Some(0),
            "{engine}: fetch with the scratch worktree as the source, no settings: {out}"
        );
        // `git clone <path>` reads the source in the clone process, which checks the worktree's folder inside `.git`.
        let clone = |to: &str| {
            format!(
                "git clone -q {} {}",
                tree.display(),
                made.join(to).display()
            )
        };
        let (code, out) = sh_as(owner, &bare, &clone("clone-bare"), &logs).await;
        assert!(
            code != Some(0) && out.contains(REFUSED) && out.contains(".git/worktrees/base"),
            "{engine}: TRIGGER NOT PRESENT: {code:?} {out}"
        );
        // The value that covers that folder is `<repo>/*`: without it the clone is refused again.
        let narrow = without_value(&settings, &format!("{}/*", repo.display()));
        let (code, out) = sh_as(owner, &narrow, &clone("clone-narrow"), &logs).await;
        assert!(
            code != Some(0) && out.contains(REFUSED),
            "{engine}: a clone from the scratch worktree without `<repo>/*`: {code:?} {out}"
        );
        let (code, out) = sh_as(owner, &settings, &clone("clone-full"), &logs).await;
        assert_eq!(
            code,
            Some(0),
            "{engine}: a clone from the scratch worktree with the launch's settings: {out}"
        );

        // A scratch worktree beside the checkout (`git worktree add ../base2`), in the run's folder on the session storage, is not named:
        // when that storage reports root for what the engine's user made there, git refuses it with the launch's settings too.
        let beside = root.join("base2");
        std::fs::create_dir(&beside).unwrap();
        chown_tree(&[&beside], owner).await;
        let add = format!(
            "cd {} && git worktree add -q --detach ../base2 HEAD && git -C ../base2 status --porcelain",
            repo.display()
        );
        let (code, out) = sh_as(owner, &bare, &add, &logs).await;
        assert_eq!(
            code,
            Some(0),
            "{engine}: a worktree beside the checkout as the engine's user: {out}"
        );
        let entry_beside = repo.join(".git/worktrees/base2");
        chown_tree(&[&beside, &entry_beside], (0, 0)).await;
        let status_beside = format!("git -C {} status --porcelain", beside.display());
        for (name, env) in [("no settings", &bare), ("the launch's settings", &settings)] {
            let (code, out) = sh_as(owner, env, &status_beside, &logs).await;
            assert!(
                code != Some(0) && out.contains(REFUSED),
                "{engine}: a worktree beside the checkout, {name}: {code:?} {out}"
            );
        }
    }
}
