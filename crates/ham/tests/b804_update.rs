//! `ham update`: `--check` shows the installed versions, the newest one on main and the changelog lines between; the off switch makes no
//! network call; the update waits for the caller's active runs (a limit, `--no-wait`) and never cancels one; it runs the `setup.sh --update` of
//! the commit it fetched and verified, from that commit's clone; afterwards both versions must be the fetched one, and a client that reports
//! another version is replaced by the earlier client setup kept beside it. The source is a git repository in a scratch folder (`file://`) and
//! the backend is the local `/v1` stand-in.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::output::Output;
use ham::sigv4::Signer;
use ham::update::{DrainEnd, DrainOptions, Git, drain};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use support::{AKID, BackendHeader, Fake, REGION, SECRET, TOKEN, creds};

/// The version after this build's (`5.14` gives `5.15`).
fn next(v: &str, by: u32) -> String {
    let (major, minor) = v.split_once('.').unwrap();
    format!("{major}.{:02}", minor.parse::<u32>().unwrap() + by)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// The stand-in of `setup.sh --update`: it records its arguments, folder and commit, keeps the installed client as `ham.prev` and installs a
/// client that prints `ham v$B804_INSTALL_VERSION`, the way the real setup installs one (a new file, then one rename). Switches for the
/// failure cases: `B804_PREV` (`other`: a `ham.prev` that is not the client that ran; `none`: no `ham.prev`), `B804_INSTALL_KIND` (`broken`: the
/// installed client fails to start; `hangs`: it never answers; `odd`: it prints a line that is no version), `B804_SETUP_SLEEP` (seconds before it ends) and `B804_SETUP_EXIT` (its exit code),
/// `B804_SETUP_SAYS` (a line it prints on standard output first, as setup prints its plan) and `B804_SETUP_STOPS` (a text it prints on standard error before it ends with
/// `B804_SETUP_EXIT`, 7 by default, without installing anything, as setup does when it refuses before it changes anything).
const SETUP: &str = r#"#!/bin/sh
printf '%s\n' "$*" > "$B804_RECORD"
pwd -P >> "$B804_RECORD"
git rev-parse HEAD >> "$B804_RECORD"
prefix=""
while [ $# -gt 0 ]; do
  case $1 in --prefix) prefix=$2; shift ;; esac
  shift
done
[ -z "${B804_SETUP_SAYS:-}" ] || printf '%s\n' "$B804_SETUP_SAYS"
if [ -n "${B804_SETUP_STOPS:-}" ]; then
  printf '%s\n' "$B804_SETUP_STOPS" >&2
  exit "${B804_SETUP_EXIT:-7}"
fi
bin=$prefix/bin
case ${B804_PREV:-keep} in
  keep) ln "$bin/ham" "$bin/ham.prev" 2>/dev/null || cp -p "$bin/ham" "$bin/ham.prev" ;;
  other) printf 'not the client that ran\n' > "$bin/ham.prev" ;;
  none) ;;
esac
case ${B804_INSTALL_KIND:-version} in
  broken) printf '#!/bin/sh\necho "this client cannot start" >&2\nexit 1\n' > "$bin/.ham.setup-new" ;;
  hangs) printf '#!/bin/sh\nexec sleep 45\n' > "$bin/.ham.setup-new" ;;
  odd) printf '#!/bin/sh\necho "hello"\n' > "$bin/.ham.setup-new" ;;
  *) printf '#!/bin/sh\necho "ham v%s"\n' "$B804_INSTALL_VERSION" > "$bin/.ham.setup-new" ;;
esac
chmod 755 "$bin/.ham.setup-new"
mv -f "$bin/.ham.setup-new" "$bin/ham"
[ -z "${B804_SETUP_SLEEP:-}" ] || sleep "$B804_SETUP_SLEEP"
exit "${B804_SETUP_EXIT:-0}"
"#;

/// A source repository whose main holds VERSION `version`, a changelog with `version` and this build's version, and the stand-in setup.
fn source_repo(root: &Path, version: &str) -> (PathBuf, String) {
    source_repo_of(
        root,
        &Source {
            version: Some(version),
            heading: version,
            setup: Some(SETUP),
            init: &["init", "-q", "-b", "main"],
        },
    )
}

/// What a source repository holds: the text of `VERSION` (`None`: no such file), the version of the newest heading of its changelog, the text
/// of `setup.sh` (`None`: no such file) and the arguments of its `git init`.
struct Source<'a> {
    version: Option<&'a str>,
    heading: &'a str,
    setup: Option<&'a str>,
    init: &'a [&'a str],
}

fn source_repo_of(root: &Path, src: &Source<'_>) -> (PathBuf, String) {
    let repo = root.join("source");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, src.init);
    if let Some(version) = src.version {
        std::fs::write(repo.join("VERSION"), format!("{version}\n")).unwrap();
    }
    std::fs::write(
        repo.join("CHANGELOG.md"),
        format!(
            "# Changelog\n\n## [Unreleased]\n\n## [{}] - 2026-11-20\n\n### Added\n\n- The newest change.\n\n## [{}] - 2026-11-10\n\n- An older change.\n",
            src.heading,
            ham::version::CLIENT
        ),
    )
    .unwrap();
    if let Some(text) = src.setup {
        let setup = repo.join("setup.sh");
        std::fs::write(&setup, text).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "release",
        ],
    );
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    (repo, sha)
}

struct Home {
    root: PathBuf,
    downloads: PathBuf,
    record: PathBuf,
}

fn home(fake: &Fake, label: &str, source: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        downloads: root.join("downloads"),
        record: root.join("setup-record.txt"),
        root,
    };
    std::fs::create_dir_all(h.root.join("config/ham")).unwrap();
    std::fs::create_dir_all(&h.downloads).unwrap();
    std::fs::write(
        h.root.join("config/ham/config.toml"),
        format!(
            "configVersion = 1\napiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\nupdateSource = \"{source}\"\n",
            fake.endpoint, REGION
        ),
    )
    .unwrap();
    let script = h.root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        h.root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {REGION}\n",
            script.display()
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
}

/// This build's `ham` in `<root>/bin`, the layout setup installs (`<prefix>/bin/ham`): a hard link to the built binary when the scratch folder
/// is on the same volume, so a test writes no copy of it, and a copy otherwise. Nothing writes into the file: setup installs by a rename and
/// `ham update` restores by a rename.
fn installed(h: &Home) -> PathBuf {
    let bin = h.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let to = bin.join("ham");
    if std::fs::hard_link(env!("CARGO_BIN_EXE_ham"), &to).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_ham"), &to).unwrap();
    }
    to
}

/// `program` with `args` in the clean environment of `h`, with `extra` on top.
fn command(
    h: &Home,
    program: &Path,
    args: &[&str],
    extra: &[(&str, &str)],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", h.root.join("config"))
        .env("XDG_STATE_HOME", h.root.join("state"))
        .env("XDG_CACHE_HOME", h.root.join("cache"))
        .env("HAMH_DOWNLOAD_DIR", &h.downloads)
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("B804_RECORD", &h.record)
        .envs(extra.iter().copied())
        .stdin(std::process::Stdio::null());
    cmd
}

async fn run(
    h: &Home,
    program: &Path,
    args: &[&str],
    extra: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = command(h, program, args, extra);
    // A copy of the binary another test thread forked during may still be open for writing in that child until it execs: retry briefly.
    for _ in 0..50 {
        match cmd.output().await {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => return other.unwrap(),
        }
    }
    panic!("{} stayed busy", program.display())
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn url(repo: &Path) -> String {
    format!("file://{}", repo.display())
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 2,
        base: Duration::from_millis(10),
    })
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id
}

fn nothing_cancelled(fake: &Fake) {
    for s in fake.mem.seen() {
        assert!(
            !(s.path.ends_with("/cancel") || s.path.ends_with("/input"))
                && (s.method == "GET" || s.path.ends_with("/direct/runs")),
            "an update sends no request that changes a run: {} {}",
            s.method,
            s.path
        );
    }
}

#[tokio::test]
async fn b804_check_shows_installed_backend_newest_and_the_changes_between() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("b804-check-src");
    let (repo, sha) = source_repo(&root, &newest);
    let h = home(&fake, "b804-check", &url(&repo));
    let o = run(
        &h,
        Path::new(env!("CARGO_BIN_EXE_ham")),
        &["update", "--check"],
        &[],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    for part in [
        format!("ham v{}", ham::version::CLIENT),
        format!("backend v{}", ham::version::CLIENT),
        format!("v{newest}"),
        sha[..12].to_string(),
        "- The newest change.".to_string(),
        "ham update".to_string(),
    ] {
        assert!(out.contains(&part), "{part:?} missing from:\n{out}");
    }
    assert!(
        !out.contains("An older change"),
        "only the changes above the installed version:\n{out}"
    );
    assert!(!h.record.exists(), "--check runs no setup");

    let o = run(
        &h,
        Path::new(env!("CARGO_BIN_EXE_ham")),
        &["update", "--check", "--json"],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["client"], json!(ham::version::CLIENT));
    assert_eq!(v["backend"], json!(ham::version::CLIENT));
    assert_eq!(v["newest"], json!(newest));
    assert_eq!(v["commit"], json!(sha));
    assert_eq!(v["updateAvailable"], json!(true));
    assert_eq!(v["changes"][0]["version"], json!(newest));
}

#[tokio::test]
async fn b804_off_switch_makes_no_network_call() {
    let fake = Fake::start().await;
    let h = home(&fake, "b804-off", "off");
    for args in [vec!["update", "--check"], vec!["update", "--yes"]] {
        let o = run(&h, Path::new(env!("CARGO_BIN_EXE_ham")), &args, &[]).await;
        let err = text(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {err}");
        assert!(
            err.contains("turned off") && err.contains("update_source"),
            "{err}"
        );
    }
    assert!(
        fake.mem.seen().is_empty(),
        "no API call while updates are off"
    );
    assert!(
        std::fs::read_dir(&h.downloads).unwrap().next().is_none(),
        "nothing fetched"
    );
}

#[tokio::test]
async fn b804_without_a_terminal_and_without_yes_nothing_runs() {
    let fake = Fake::start().await;
    let root = support::scratch("b804-noterm-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "b804-noterm", &url(&repo));
    let bin = installed(&h);
    let o = run(&h, &bin, &["update"], &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(4), "{err}");
    assert!(err.contains("--yes"), "{err}");
    assert!(!h.record.exists(), "setup did not run");
}

#[tokio::test]
async fn b804_update_runs_the_fetched_setup_and_checks_both_versions_afterwards() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("b804-full-src");
    let (repo, sha) = source_repo(&root, &newest);
    let h = home(&fake, "b804-full", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest)],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    let record = std::fs::read_to_string(&h.record).unwrap();
    let lines: Vec<&str> = record.lines().collect();
    let prefix = h.root.canonicalize().unwrap();
    // Setup waits for the runs of everyone on the stage with what is left of ham's own limit (60 minutes by default)
    assert_eq!(
        lines[0],
        format!(
            "--update --prefix {} --yes --wait-for-runs 60",
            prefix.display()
        )
    );
    let clone = h
        .downloads
        .canonicalize()
        .unwrap()
        .join(format!("ham-update-{}", &sha[..12]));
    assert_eq!(
        lines[1],
        clone.display().to_string(),
        "setup runs in the clone of the fetched commit"
    );
    assert_eq!(
        lines[2], sha,
        "the clone holds exactly the commit that was shown"
    );
    assert!(
        out.contains(&format!("Updated: ham v{newest} | backend v{newest}")),
        "{out}"
    );
    assert!(
        out.contains("client only"),
        "the plan names the part that changes:\n{out}"
    );
    nothing_cancelled(&fake);
}

#[tokio::test]
async fn b804_a_client_that_reports_another_version_is_replaced_by_the_earlier_one() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("b804-rollback-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "b804-rollback", &url(&repo));
    let bin = installed(&h);
    let before = std::fs::read(&bin).unwrap();
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", "9.99")],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{out}\n{err}");
    assert!(err.contains("v9.99") && err.contains("restored"), "{err}");
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        before,
        "the earlier client is back in place"
    );
}

#[tokio::test]
async fn b804_update_never_moves_a_version_back_and_does_nothing_when_up_to_date() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() =
        BackendHeader::Value(next(ham::version::CLIENT, 2));
    let root = support::scratch("b804-down-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "b804-down", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest)],
    )
    .await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(7), "{err}");
    assert!(err.contains("never moves a version back"), "{err}");
    assert!(!h.record.exists());

    let root = support::scratch("b804-same-src");
    let (repo, _) = source_repo(&root, ham::version::CLIENT);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Same;
    let h = home(&fake, "b804-same", &url(&repo));
    let bin = installed(&h);
    let o = run(&h, &bin, &["update", "--yes"], &[]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(out.contains("up to date"), "{out}");
    assert!(!h.record.exists());
}

#[tokio::test]
async fn b804_the_wait_ends_when_the_runs_end() {
    let fake = Fake::start().await;
    let id = new_run(&fake, "b804 wait").await;
    let f = fake.clone();
    let done = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        f.mem.finish_run(&done);
    });
    let out = Output::capture();
    let opts = DrainOptions {
        poll: Duration::from_millis(50),
        limit: Duration::from_secs(20),
        no_wait: false,
    };
    let end = drain(&api(&fake), &opts, &out).await.unwrap();
    assert!(matches!(end, DrainEnd::Waited { .. }), "{end:?}");
    let text = out.finish(0).stderr_text();
    assert!(
        text.contains(&id) && text.contains("left"),
        "a visible countdown names the run:\n{text}"
    );
    assert!(text.contains("No run is cancelled"), "{text}");
    nothing_cancelled(&fake);
}

#[tokio::test]
async fn b804_the_wait_has_a_limit_and_never_cancels_a_run() {
    let fake = Fake::start().await;
    let id = new_run(&fake, "b804 limit").await;
    let opts = DrainOptions {
        poll: Duration::from_millis(50),
        limit: Duration::from_millis(300),
        no_wait: false,
    };
    let end = drain(&api(&fake), &opts, &Output::capture()).await.unwrap();
    assert_eq!(end, DrainEnd::TimedOut(vec![id.clone()]));
    nothing_cancelled(&fake);
    let runs = api(&fake).list_runs().await.unwrap();
    assert_eq!(
        runs[0].status,
        harness_protocol::goal::GoalStatus::Active,
        "the run goes on"
    );
}

#[tokio::test]
async fn b804_no_wait_stops_at_once_with_the_list() {
    let fake = Fake::start().await;
    let id = new_run(&fake, "b804 nowait").await;
    let opts = DrainOptions {
        poll: Duration::from_millis(50),
        limit: Duration::from_secs(20),
        no_wait: true,
    };
    let started = std::time::Instant::now();
    let end = drain(&api(&fake), &opts, &Output::capture()).await.unwrap();
    assert_eq!(end, DrainEnd::NotWaiting(vec![id]));
    assert!(started.elapsed() < Duration::from_secs(5));
    let clear = Fake::start().await;
    assert_eq!(
        drain(&api(&clear), &opts, &Output::capture())
            .await
            .unwrap(),
        DrainEnd::Clear
    );
}

#[test]
fn b804_a_copy_that_is_not_the_commit_is_never_reused() {
    let root = support::scratch("b804-fetch");
    let (repo, sha) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let dir = root
        .join("downloads")
        .join(format!("ham-update-{}", &sha[..12]));
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    let g = Git::system();
    let f = g.fetch(&url(&repo), &dir, &sha).unwrap();
    assert_eq!(f.commit, sha);
    assert_eq!(f.tree.len(), 40);
    assert_eq!(g.head_of_main(&url(&repo)).unwrap(), sha);
    // Fetched again: the copy is checked and reused.
    assert_eq!(g.fetch(&url(&repo), &dir, &sha).unwrap().commit, sha);
    // A changed file: refused, and the folder is named.
    std::fs::write(dir.join("setup.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let e = g.fetch(&url(&repo), &dir, &sha).unwrap_err().to_string();
    assert!(e.contains(&dir.display().to_string()), "{e}");
    // A commit the source does not hold: refused.
    let other = root.join("downloads").join("ham-update-other");
    let e = g
        .fetch(&url(&repo), &other, &"0".repeat(40))
        .unwrap_err()
        .to_string();
    assert!(e.contains("0000000"), "{e}");
}

// ---- setup's codes that come after a finished update, and one backend written as a table -------------------------------------------------

/// Setup ends with exit 8 when the update is done and only its runner could not be removed. The backend is deployed and the client is
/// installed by then, so `ham update` checks both versions as after any update, prints the result line and says what setup reported.
#[tokio::test]
async fn fix515_setup_exit_8_after_a_finished_update_still_checks_both_versions() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-exit8-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-exit8", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest), ("B804_SETUP_EXIT", "8")],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    println!(
        "FIX515 TRIGGER PRESENT: setup ended with exit 8 after it installed the client; code={:?}\nSTDOUT:\n{out}\nSTDERR:\n{err}",
        o.status.code()
    );
    assert!(h.record.exists(), "setup ran");
    assert!(
        out.contains(&format!("Updated: ham v{newest} | backend v{newest}")),
        "both versions are checked and the result line is printed:\n{out}"
    );
    assert!(
        err.contains("exit 8") && err.contains("runner"),
        "what setup reported is said:\n{err}"
    );
    assert_eq!(
        o.status.code(),
        Some(8),
        "setup's own code stays the exit code"
    );
}

/// Setup ends with exit 3 when the client is installed and `ham doctor` found a problem. A new client that reports another version is
/// replaced by the earlier one, as after any update, and the lines say both things.
#[tokio::test]
async fn fix515_setup_exit_3_with_a_client_of_another_version_restores_the_earlier_client() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-exit3-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-exit3", &url(&repo));
    let bin = installed(&h);
    let before = std::fs::read(&bin).unwrap();
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", "9.99"), ("B804_SETUP_EXIT", "3")],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    println!(
        "FIX515 TRIGGER PRESENT: setup ended with exit 3 and the new client reports v9.99; code={:?}\nSTDOUT:\n{out}\nSTDERR:\n{err}",
        o.status.code()
    );
    assert_eq!(o.status.code(), Some(3), "{out}\n{err}");
    assert!(err.contains("v9.99") && err.contains("restored"), "{err}");
    assert!(
        err.contains("exit 3") && err.contains("ham doctor"),
        "what setup reported is said:\n{err}"
    );
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        before,
        "the earlier client is back in place"
    );
    assert!(
        out.contains(&format!(
            "After the update: ham v{} | backend v{newest}",
            ham::version::CLIENT
        )),
        "{out}"
    );
}

/// Setup's exit 3 with both versions as expected: the update took effect, the result line is printed, and the exit code stays setup's.
#[tokio::test]
async fn fix515_setup_exit_3_with_both_versions_right_prints_the_result_line() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-exit3ok-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-exit3ok", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest), ("B804_SETUP_EXIT", "3")],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{out}\n{err}");
    assert!(
        out.contains(&format!("Updated: ham v{newest} | backend v{newest}")),
        "{out}"
    );
    assert!(
        err.contains("exit 3") && err.contains("ham doctor"),
        "{err}"
    );
    assert!(!err.contains("restored"), "{err}");
}

/// A code of setup that means the update did not happen is passed through as before, with no check of the versions.
#[tokio::test]
async fn fix515_a_setup_that_failed_is_passed_through_without_a_result_line() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-exit7-src");
    let (repo, _) = source_repo(&root, &newest);
    for (code, words) in [
        ("7", "conflicting operation"),
        ("6", "still running"),
        ("4", "cancelled"),
        ("9", "its message is above"),
    ] {
        let h = home(&fake, &format!("fix515-exit{code}"), &url(&repo));
        let bin = installed(&h);
        let o = run(
            &h,
            &bin,
            &["update", "--yes"],
            &[("B804_INSTALL_VERSION", &newest), ("B804_SETUP_EXIT", code)],
        )
        .await;
        let (out, err) = (text(&o.stdout), text(&o.stderr));
        assert_eq!(
            o.status.code(),
            Some(code.parse::<i32>().unwrap()),
            "{out}\n{err}"
        );
        assert!(
            err.contains(&format!("exit {code}")) && err.contains(words),
            "exit {code}: {err}"
        );
        assert!(
            !out.contains("Updated:") && !out.contains("After the update"),
            "exit {code}: {out}"
        );
    }
}

/// A stage whose recorded commit no clone of the new history holds. The rule is setup's alone (its VERSION comparison has rows U45 to U54 in the setup tests); `ham update`
/// runs the setup of the clone it fetched and copies none of it. A setup that goes ahead after it could not place the commit is an update like any other: its plan line reaches the
/// terminal and both versions are checked afterwards.
#[tokio::test]
async fn b838_a_setup_that_goes_ahead_after_a_commit_it_cannot_place_is_checked_like_any_update() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("b838-ahead-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "b838-ahead", &url(&repo));
    let bin = installed(&h);
    let says = "  order:         the commit the stage records is not in this clone, so git cannot place it; its VERSION 5.15 is lower than this clone's 5.17, so setup treats this update as a newer release. A VERSION orders releases only: if the stage was deployed from your own fork or from commits that you did not publish, check this line before you answer";
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest), ("B804_SETUP_SAYS", says)],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    assert!(
        out.contains(says),
        "setup's own line reaches the terminal:\n{out}"
    );
    assert!(
        out.contains(&format!("Updated: ham v{newest} | backend v{newest}")),
        "{out}"
    );
    nothing_cancelled(&fake);
}

/// Setup stops with exit 7, before it changes anything, for a recorded commit it cannot place when the VERSION does not show the clone to be newer. `ham update` passes the exit
/// code and setup's text through, installs and checks nothing, and names the folder of the clone it ran setup in, because the command setup names (`--revision`) runs there.
/// Its own words do not name a pull: the clone is a fresh one.
#[tokio::test]
async fn b838_a_stop_of_setup_for_a_commit_it_cannot_place_is_passed_through_and_names_its_folder()
{
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("b838-stop-src");
    let (repo, sha) = source_repo(&root, &newest);
    let h = home(&fake, "b838-stop", &url(&repo));
    let bin = installed(&h);
    let before = ham::update::sha256_file(&bin).unwrap();
    let stops = format!(
        "setup.sh: the stage direct runs commit 17e2019dbc7b0d9abf4bcd669e5241b7e10982b5 (VERSION {newest}), which is not in this clone, and its VERSION is the same as this clone's, so setup cannot tell whether {sha} (VERSION {newest}) is newer\n      To deploy this clone's commit on purpose, name it:  ./setup.sh --update --revision {sha}\n      If this clone is old or shallow, a new clone of the repository holds the commits it lacks: git clone <the repository URL>, then run ./setup.sh --update from it.\n      Nothing was changed."
    );
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[
            ("B804_INSTALL_VERSION", &newest),
            ("B804_SETUP_STOPS", &stops),
            ("B804_SETUP_EXIT", "7"),
        ],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(7), "{out}\n{err}");
    for needle in [
        "which is not in this clone",
        "a new clone of the repository",
        &format!("--revision {sha}"),
        "Nothing was changed.",
    ] {
        assert!(
            err.contains(needle),
            "setup's text is passed through ({needle}):\n{err}"
        );
    }
    let clone = h
        .downloads
        .canonicalize()
        .unwrap()
        .join(format!("ham-update-{}", &sha[..12]));
    let given = h.downloads.join(format!("ham-update-{}", &sha[..12]));
    let own: Vec<&str> = err
        .lines()
        .filter(|l| l.starts_with("ham update:"))
        .collect();
    assert!(
        own.iter()
            .any(|l| l.contains("exit 7") && l.contains("conflicting operation")),
        "{err}"
    );
    assert!(
        own.iter()
            .any(|l| l.contains("once it is gone") && l.contains("stops the same way")),
        "ham says when running it again helps (a conflict that ends) and when it does not (a stage commit the clone cannot place): {own:?}"
    );
    assert!(
        own.iter().any(|l| l.contains(&clone.display().to_string())
            || l.contains(&given.display().to_string())),
        "ham names the folder setup ran in, where the command setup names runs: {own:?}"
    );
    assert!(
        own.iter().all(|l| !l.contains("git pull")),
        "ham's own words do not send the user to a pull: {own:?}"
    );
    assert!(
        !out.contains("Updated:") && !out.contains("After the update"),
        "{out}"
    );
    assert_eq!(
        ham::update::sha256_file(&bin).unwrap(),
        before,
        "the client that ran is the client that stays"
    );
    nothing_cancelled(&fake);
}

/// The config of `h` rewritten with its one backend as a `[[backend]]` table and no top-level address.
fn one_backend_as_a_table(h: &Home, fake: &Fake, source: &str) {
    std::fs::write(
        h.root.join("config/ham/config.toml"),
        format!(
            "configVersion = 1\nupdateSource = \"{source}\"\n\n[[backend]]\nalias = \"work\"\napiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint, REGION
        ),
    )
    .unwrap();
}

/// One `[[backend]]` table and no top-level `apiEndpoint` is one backend, so `ham update --check`, which changes nothing, reads its versions
/// like those of any single stage.
#[tokio::test]
async fn fix515_one_backend_written_as_a_table_can_be_checked() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515-onetable-src");
    let (repo, sha) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-onetable", &url(&repo));
    one_backend_as_a_table(&h, &fake, &url(&repo));
    let o = run(
        &h,
        Path::new(env!("CARGO_BIN_EXE_ham")),
        &["update", "--check"],
        &[],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    println!(
        "FIX515 TRIGGER PRESENT: one [[backend]] table and no top-level apiEndpoint; code={:?}\nSTDOUT:\n{out}\nSTDERR:\n{err}",
        o.status.code()
    );
    assert!(
        !err.contains("more than one backend"),
        "one backend is not more than one:\n{err}"
    );
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    assert!(
        out.contains(&format!("v{newest}")) && out.contains(&sha[..12]),
        "{out}"
    );
}

/// The update itself hands the stage to setup, which reads and writes the top-level keys of the file. With the backend in a table it stops
/// before anything is fetched or changed, and its sentence says that and names the way on; it does not say "more than one backend".
#[tokio::test]
async fn fix515_one_backend_written_as_a_table_stops_the_update_in_true_words() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515-onetable-update-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-onetable-update", &url(&repo));
    one_backend_as_a_table(&h, &fake, &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest)],
    )
    .await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(!err.contains("more than one backend"), "{err}");
    for want in [
        "[[backend]] table",
        "top level",
        "api_endpoint",
        "ham update --check",
    ] {
        assert!(err.contains(want), "{want:?} missing: {err}");
    }
    assert!(!h.record.exists(), "setup did not run");
    assert!(fake.mem.seen().is_empty(), "no API call was made");
    assert!(
        std::fs::read_dir(&h.downloads).unwrap().next().is_none(),
        "nothing was fetched"
    );
}

/// Two backends are more than one: `ham update` stops before any call and names the way on.
#[tokio::test]
async fn fix515_two_backends_stop_ham_update_before_any_call() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-twotables-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "fix515-twotables", &url(&repo));
    // The top-level address is one backend and the table is another.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(h.root.join("config/ham/config.toml"))
            .unwrap();
        writeln!(
            f,
            "\n[[backend]]\nalias = \"lab\"\napiEndpoint = \"{}\"\nregion = \"{REGION}\"\nawsProfile = \"ham-test\"",
            fake.endpoint
        )
        .unwrap();
    }
    for args in [vec!["update", "--check"], vec!["update", "--yes"]] {
        let o = run(&h, Path::new(env!("CARGO_BIN_EXE_ham")), &args, &[]).await;
        let err = text(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(
            err.contains("more than one backend") && err.contains("./setup.sh --update"),
            "{args:?}: {err}"
        );
    }
    assert!(fake.mem.seen().is_empty(), "no API call was made");
    assert!(
        std::fs::read_dir(&h.downloads).unwrap().next().is_none(),
        "nothing was fetched"
    );
}

// ---- the source, the copy in the download folder and git ---------------------------------------------------------------------------------

const CHECK: [&str; 2] = ["update", "--check"];
const YES: [&str; 2] = ["update", "--yes"];

fn ham_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_ham"))
}

/// A `GIT_*` variable of the caller does not reach the git that `ham update` runs: with a `GIT_DIR` that names no repository the source is
/// still read.
#[tokio::test]
async fn fix515_a_git_variable_of_the_caller_does_not_reach_git() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515-gitenv-src");
    let (repo, sha) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-gitenv", &url(&repo));
    let nowhere = h.root.join("no-such-repository");
    let o = run(
        &h,
        ham_bin(),
        &CHECK,
        &[
            ("GIT_DIR", nowhere.to_str().unwrap()),
            ("GIT_WORK_TREE", nowhere.to_str().unwrap()),
        ],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    assert!(out.contains(&sha[..12]), "{out}");
}

/// A source that does not answer is named, with git's own first line, and nothing is fetched. The address is a closed port of this machine.
#[tokio::test]
async fn fix515_a_source_that_does_not_answer_is_named_and_nothing_is_fetched() {
    let fake = Fake::start().await;
    let source = "https://127.0.0.1:9/ham.git";
    let h = home(&fake, "fix515-nosource", source);
    for args in [CHECK, YES] {
        let o = run(&h, ham_bin(), &args, &[]).await;
        let err = text(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(
            err.contains(&format!("could not read the main branch of {source}")),
            "{args:?}: {err}"
        );
    }
    assert!(fake.mem.seen().is_empty(), "the backend is not asked");
    assert!(std::fs::read_dir(&h.downloads).unwrap().next().is_none());
}

/// A commit that holds no `VERSION` file cannot be an update: the check stops with git's own words.
#[tokio::test]
async fn fix515_a_commit_without_a_version_file_stops_the_check() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-noversion-src");
    let (repo, _) = source_repo_of(
        &root,
        &Source {
            version: None,
            heading: "9.99",
            setup: Some(SETUP),
            init: &["init", "-q", "-b", "main"],
        },
    );
    let h = home(&fake, "fix515-noversion", &url(&repo));
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("git show failed") && err.contains("VERSION"),
        "{err}"
    );
}

/// A `VERSION` that is not of the form `N.NN` stops the update, and the check says what would stop it.
#[tokio::test]
async fn fix515_a_version_on_main_that_is_no_version_stops_the_update() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-badversion-src");
    let (repo, _) = source_repo_of(
        &root,
        &Source {
            version: Some("banana"),
            heading: "9.99",
            setup: Some(SETUP),
            init: &["init", "-q", "-b", "main"],
        },
    );
    let h = home(&fake, "fix515-badversion", &url(&repo));
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(
        out.contains("ham update would stop: \"banana\" is not a version of the form N.NN"),
        "{out}"
    );
    let bin = installed(&h);
    let o = run(&h, &bin, &YES, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(7), "{err}");
    assert!(
        err.contains("\"banana\" is not a version of the form N.NN")
            && err.contains("No deployment or client change was made."),
        "{err}"
    );
    assert!(!h.record.exists());
}

/// A source whose commit ids are not 40 hexadecimal digits (a SHA-256 repository) is refused: ham cannot name the commit it would check.
#[tokio::test]
async fn fix515_a_source_whose_commit_ids_are_not_sha1_is_refused() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-sha256-src");
    let (repo, sha) = source_repo_of(
        &root,
        &Source {
            version: Some(&next(ham::version::CLIENT, 1)),
            heading: "9.99",
            setup: Some(SETUP),
            init: &["init", "-q", "-b", "main", "--object-format=sha256"],
        },
    );
    assert_eq!(sha.len(), 64, "the stand-in source is a SHA-256 repository");
    let h = home(&fake, "fix515-sha256", &url(&repo));
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(err.contains("answered with no commit id for main"), "{err}");
}

/// What sits where the copy of the commit belongs and is not ham's clone of the source is named and left as it is: a file, and a folder that
/// is no clone of the source.
#[tokio::test]
async fn fix515_something_else_at_the_place_of_the_copy_is_named_and_left() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515-place-src");
    let (repo, sha) = source_repo(&root, &newest);
    let name = format!("ham-update-{}", &sha[..12]);

    let h = home(&fake, "fix515-place-file", &url(&repo));
    let place = h.downloads.join(&name);
    std::fs::write(&place, "mine").unwrap();
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("is not a folder ham made") && err.contains(&name),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&place).unwrap(), "mine");

    let h = home(&fake, "fix515-place-folder", &url(&repo));
    let place = h.downloads.join(&name);
    std::fs::create_dir_all(place.join("kept")).unwrap();
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("is not a clone of") && err.contains(&name),
        "{err}"
    );
    assert!(place.join("kept").is_dir(), "nothing was removed");
}

/// A download folder that cannot be made, and one that cannot be written, stop the check with the reason; nothing is asked of setup.
#[tokio::test]
async fn fix515_a_download_folder_that_cannot_be_used_is_said() {
    use std::os::unix::fs::PermissionsExt;
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515-dl-src");
    let (repo, _) = source_repo(&root, &newest);

    // The folder would sit under a file.
    let h = home(&fake, "fix515-dl-file", &url(&repo));
    let blocker = h.root.join("a-file");
    std::fs::write(&blocker, "x").unwrap();
    let under = blocker.join("downloads");
    let o = run(
        &h,
        ham_bin(),
        &CHECK,
        &[("HAMH_DOWNLOAD_DIR", under.to_str().unwrap())],
    )
    .await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(err.contains("could not create"), "{err}");

    // The folder exists and takes no new entry.
    let h = home(&fake, "fix515-dl-readonly", &url(&repo));
    std::fs::set_permissions(&h.downloads, std::fs::Permissions::from_mode(0o555)).unwrap();
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    std::fs::set_permissions(&h.downloads, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(err.contains("git clone failed"), "{err}");
}

/// A checked-out copy whose `VERSION` is no version is refused by the check of the copy itself, with the folder named.
#[test]
fn fix515_a_copy_whose_version_is_no_version_is_refused() {
    let root = support::scratch("fix515-copyversion");
    let (repo, sha) = source_repo_of(
        &root,
        &Source {
            version: Some("banana"),
            heading: "9.99",
            setup: Some(SETUP),
            init: &["init", "-q", "-b", "main"],
        },
    );
    let dir = root.join("downloads").join("ham-update-copy");
    let e = Git::system()
        .fetch(&url(&repo), &dir, &sha)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("its VERSION is \"banana\"") && e.contains(&dir.display().to_string()),
        "{e}"
    );
}

// ---- the plan and the stops before setup -------------------------------------------------------------------------------------------------

/// A backend behind a `ham` that already runs the newest version: the update changes the backend only, and the changes are those above the
/// backend's version.
#[tokio::test]
async fn fix515_a_backend_behind_a_current_ham_is_an_update_of_the_backend_only() {
    let fake = Fake::start().await;
    let client = ham::version::CLIENT;
    let (major, minor) = client.split_once('.').unwrap();
    let older = format!("{major}.{:02}", minor.parse::<u32>().unwrap() - 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(older.clone());
    let root = support::scratch("fix515-backendonly-src");
    let (repo, _) = source_repo(&root, client);
    let h = home(&fake, "fix515-backendonly", &url(&repo));
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(out.contains(&format!("backend v{older}")), "{out}");
    assert!(out.contains("(backend only)"), "{out}");
    assert!(
        out.contains("- The newest change."),
        "the changes above the backend's version are shown:\n{out}"
    );
}

/// A backend that reports no version: ham cannot tell whether main is newer, says so, and changes nothing.
#[tokio::test]
async fn fix515_a_backend_that_reports_no_version_stops_the_update() {
    let fake = Fake::start().await;
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Absent;
    let root = support::scratch("fix515-noheader-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "fix515-noheader", &url(&repo));
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(out.contains("backend v?.??"), "{out}");
    assert!(
        out.contains("ham update would stop: the backend reports no version"),
        "{out}"
    );
    let bin = installed(&h);
    let o = run(&h, &bin, &YES, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(7), "{err}");
    assert!(
        err.contains("the backend reports no version")
            && err.contains("./setup.sh --update")
            && err.contains("No deployment or client change was made."),
        "{err}"
    );
    assert!(!h.record.exists());
}

/// A backend that does not answer stops the update before the plan, with the reason.
#[tokio::test]
async fn fix515_a_backend_that_does_not_answer_stops_the_update() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-nobackend-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "fix515-nobackend", &url(&repo));
    // The address of the stage is a closed port of this machine.
    let config = h.root.join("config/ham/config.toml");
    let text_of = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text_of.replace(&fake.endpoint, "http://127.0.0.1:9/live"),
    )
    .unwrap();
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(err.contains("could not reach the backend"), "{err}");
}

/// A `ham` that does not run from a folder named `bin` cannot be replaced by setup, which installs into `<prefix>/bin`: the update says so
/// before it asks or changes anything.
#[tokio::test]
async fn fix515_a_ham_outside_a_bin_folder_is_not_updated() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-nobin-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "fix515-nobin", &url(&repo));
    let elsewhere = h.root.join("tools");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let program = elsewhere.join("ham");
    std::fs::copy(env!("CARGO_BIN_EXE_ham"), &program).unwrap();
    let o = run(&h, &program, &YES, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("this ham runs from")
            && err.contains("tools")
            && err.contains("./setup.sh --update --prefix"),
        "{err}"
    );
    assert!(!h.record.exists());
}

/// `--no-wait` with a run of the caller still active: the update stops at once with the run named, and nothing is deployed or cancelled.
#[tokio::test]
async fn fix515_no_wait_with_an_active_run_stops_the_update() {
    let fake = Fake::start().await;
    let id = new_run(&fake, "fix515 nowait").await;
    let root = support::scratch("fix515-nowait-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "fix515-nowait", &url(&repo));
    let bin = installed(&h);
    let o = run(&h, &bin, &["update", "--yes", "--no-wait"], &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(7), "{err}");
    assert!(
        err.contains("1 run(s) are active")
            && err.contains(&id)
            && err.contains("--no-wait stops here"),
        "{err}"
    );
    assert!(!h.record.exists(), "setup did not run");
    nothing_cancelled(&fake);
}

/// A changelog whose newest heading is another version than `VERSION` gets a note; the update goes on.
#[tokio::test]
async fn fix515_a_changelog_whose_newest_heading_is_another_version_gets_a_note() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-heading-src");
    let (repo, _) = source_repo_of(
        &root,
        &Source {
            version: Some(&newest),
            heading: &next(ham::version::CLIENT, 2),
            setup: Some(SETUP),
            init: &["init", "-q", "-b", "main"],
        },
    );
    let h = home(&fake, "fix515-heading", &url(&repo));
    let bin = installed(&h);
    let o = run(&h, &bin, &YES, &[("B804_INSTALL_VERSION", &newest)]).await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "note: the newest heading of CHANGELOG.md at this commit is not {newest}."
        )),
        "{err}"
    );
    assert!(out.contains("Updated:"), "{out}");
}

/// A commit that holds no `setup.sh`: the update cannot start setup, says so and changes nothing.
#[tokio::test]
async fn fix515_a_commit_without_setup_cannot_update() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515-nosetup-src");
    let (repo, _) = source_repo_of(
        &root,
        &Source {
            version: Some(&newest),
            heading: &newest,
            setup: None,
            init: &["init", "-q", "-b", "main"],
        },
    );
    let h = home(&fake, "fix515-nosetup", &url(&repo));
    let bin = installed(&h);
    let before = std::fs::read(&bin).unwrap();
    let o = run(&h, &bin, &YES, &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("could not start") && err.contains("setup.sh"),
        "{err}"
    );
    assert_eq!(
        std::fs::read(&bin).unwrap(),
        before,
        "the client is untouched"
    );
}

// ---- the client and the backend after setup ----------------------------------------------------------------------------------------------

/// The new client reports another version and setup kept no earlier client: ham cannot put one back, says why, and the result line names the
/// version the client reports.
#[tokio::test]
async fn fix515_a_wrong_client_with_no_earlier_one_beside_it_is_said_and_left() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-noprev-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-noprev", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &YES,
        &[("B804_INSTALL_VERSION", "9.99"), ("B804_PREV", "none")],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{out}\n{err}");
    assert!(
        err.contains("reported v9.99") && err.contains("ham.prev cannot be read"),
        "{err}"
    );
    assert!(!err.contains("restored"), "{err}");
    assert!(
        out.contains(&format!("After the update: ham v9.99 | backend v{newest}.")),
        "{out}"
    );
}

/// The file beside the new client is not the client that ran before the update: it is not put back, and the line says that it was left.
#[tokio::test]
async fn fix515_an_earlier_client_that_is_not_the_one_that_ran_is_not_put_back() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-otherprev-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-otherprev", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &YES,
        &[("B804_INSTALL_VERSION", "9.99"), ("B804_PREV", "other")],
    )
    .await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "{err}");
    assert!(
        err.contains("is not the client that ran before this update, so it was left as it is"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(h.root.join("bin/ham.prev")).unwrap(),
        "not the client that ran\n"
    );
}

/// A new client that fails to start is replaced by the earlier one, and the line says what it answered in plain words (no `v` before words
/// that are no version).
#[tokio::test]
async fn fix515_a_new_client_that_fails_to_start_is_replaced_by_the_earlier_one() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-broken-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-broken", &url(&repo));
    let bin = installed(&h);
    let before = std::fs::read(&bin).unwrap();
    let o = run(&h, &bin, &YES, &[("B804_INSTALL_KIND", "broken")]).await;
    let err = text(&o.stderr);
    println!("FIX515 TRIGGER PRESENT: the installed client exits 1 for --version\n{err}");
    assert_eq!(o.status.code(), Some(3), "{err}");
    assert!(
        err.contains("the new ham answered with an error (this client cannot start), not v"),
        "{err}"
    );
    assert!(!err.contains("van error"), "{err}");
    assert!(err.contains("restored"), "{err}");
    assert_eq!(std::fs::read(&bin).unwrap(), before);
}

/// The earlier client is the right one, but the folder does not let ham rename it back: the line says what failed.
#[test]
fn fix515_an_earlier_client_that_cannot_be_renamed_back_is_said() {
    use ham::update::{ClientFit, client_fit, sha256_file};
    use std::os::unix::fs::PermissionsExt;
    let root = support::scratch("fix515-rename");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin = bin_dir.join("ham");
    std::fs::write(&bin, "#!/bin/sh\necho 'ham v9.99'\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prev = bin_dir.join("ham.prev");
    std::fs::write(&prev, "the client that ran").unwrap();
    let before = sha256_file(&prev).unwrap();
    std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let fit = client_fit(&bin, "5.16", Some(&before));
    std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ClientFit::NotRestored { reported, why } = fit else {
        panic!("{fit:?}")
    };
    assert_eq!(reported, "9.99");
    assert!(why.contains("renaming") && why.contains("failed"), "{why}");
    assert!(prev.exists(), "the earlier client is still beside it");

    // A client that is not there at all answers nothing.
    let gone = client_fit(&bin_dir.join("no-such-ham"), "5.16", None);
    let ClientFit::NotRestored { reported, .. } = gone else {
        panic!("{gone:?}")
    };
    assert!(reported.starts_with("no answer ("), "{reported}");
}

/// The backend's version is read again until it is the expected one: a backend that changes its answer a moment after setup ended counts.
#[tokio::test]
async fn fix515_the_backend_version_is_read_again_until_it_is_the_new_one() {
    use ham::update::backend_after;
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let api = api(&fake);
    let f = fake.clone();
    let later = newest.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        *f.faults.backend_header.lock().unwrap() = BackendHeader::Value(later);
    });
    let now = backend_after(
        &api,
        &newest,
        Duration::from_secs(20),
        Duration::from_millis(50),
    )
    .await;
    assert_eq!(now.unwrap().as_deref(), Some(newest.as_str()));
    // A backend that never changes: the last answer comes back when the limit has passed.
    let never = next(ham::version::CLIENT, 3);
    let now = backend_after(
        &api,
        &never,
        Duration::from_millis(200),
        Duration::from_millis(50),
    )
    .await;
    assert_eq!(now.unwrap().as_deref(), Some(newest.as_str()));
}

// ---- the wait ----------------------------------------------------------------------------------------------------------------------------

/// The line of the wait marks a run that waits for an answer with the command that answers it, shows five runs and counts the rest.
#[tokio::test]
async fn fix515_the_wait_marks_a_run_that_needs_an_answer_and_counts_the_runs_it_does_not_list() {
    let fake = Fake::start().await;
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(new_run(&fake, &format!("fix515 wait {i}")).await);
    }
    let asks = ids[5].clone();
    fake.mem.needs_input(&asks, "model_unavailable");
    let out = Output::capture();
    let opts = DrainOptions {
        poll: Duration::from_millis(50),
        limit: Duration::from_millis(200),
        no_wait: false,
    };
    let end = drain(&api(&fake), &opts, &out).await.unwrap();
    assert!(
        matches!(end, DrainEnd::TimedOut(ref left) if left.len() == 6),
        "{end:?}"
    );
    let text = out.finish(0).stderr_text();
    assert!(text.contains("Waiting for 6 runs to end"), "{text}");
    assert!(
        text.contains(&format!("{asks} (waits for your answer: ham say {asks})")),
        "{text}"
    );
    assert!(text.contains(" and 1 more."), "{text}");
    nothing_cancelled(&fake);
}

/// Ctrl-C during the wait stops the update with exit 4: nothing is deployed and no run is cancelled.
#[tokio::test]
async fn fix515_ctrl_c_during_the_wait_stops_the_update_and_cancels_no_run() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let fake = Fake::start().await;
    let id = new_run(&fake, "fix515 ctrl-c").await;
    let root = support::scratch("fix515-ctrlc-src");
    let (repo, _) = source_repo(&root, &next(ham::version::CLIENT, 1));
    let h = home(&fake, "fix515-ctrlc", &url(&repo));
    let bin = installed(&h);
    let mut child = command(&h, &bin, &YES, &[])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        if err.read_line(&mut line).await.unwrap() == 0 {
            break;
        }
        seen.push_str(&line);
        if line.contains("Waiting for 1 run to end") {
            break;
        }
    }
    assert!(seen.contains(&id), "the wait names the run:\n{seen}");
    println!(
        "FIX515 TRIGGER PRESENT: the update waits for run {id}; the interrupt signal goes to ham"
    );
    let pid = child.id().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    let mut rest = String::new();
    err.read_to_string(&mut rest).await.unwrap();
    let status = child.wait().await.unwrap();
    assert_eq!(status.code(), Some(4), "{seen}{rest}");
    assert!(
        rest.contains("the wait was stopped")
            && rest.contains("No deployment or client change was made.")
            && rest.contains("No run was cancelled."),
        "{rest}"
    );
    assert!(!h.record.exists(), "setup did not run");
    nothing_cancelled(&fake);
}

/// Ctrl-C while setup runs belongs to setup: ham says so, waits for setup to end and then checks the versions as after any update.
#[tokio::test]
async fn fix515_ctrl_c_while_setup_runs_is_left_to_setup() {
    use tokio::io::AsyncReadExt;
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-ctrlc-setup-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-ctrlc-setup", &url(&repo));
    let bin = installed(&h);
    let mut child = command(
        &h,
        &bin,
        &YES,
        &[("B804_INSTALL_VERSION", &newest), ("B804_SETUP_SLEEP", "3")],
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    // The stand-in writes its record first: setup has started when the file is there. The wait ends with ham if setup never starts.
    while !h.record.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "ham ended before setup started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = child.id().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    let (mut out, mut err) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .await
        .unwrap();
    let status = child.wait().await.unwrap();
    assert_eq!(status.code(), Some(0), "{out}\n{err}");
    assert!(
        err.contains("Ctrl-C went to setup; ham waits for setup to end."),
        "{err}"
    );
    assert!(
        out.contains(&format!("Updated: ham v{newest} | backend v{newest}")),
        "{out}"
    );
}

// ---- the update driven in this process: the question, the wait and the checks ------------------------------------------------------------

/// A stand-in setup that needs no variable: it keeps the installed client as `ham.prev` and installs one that prints `ham v<version>`.
fn setup_installing(version: &str) -> String {
    format!(
        "#!/bin/sh\nprefix=\"\"\nwhile [ $# -gt 0 ]; do\n  case $1 in --prefix) prefix=$2; shift ;; esac\n  shift\ndone\nbin=$prefix/bin\nln \"$bin/ham\" \"$bin/ham.prev\" 2>/dev/null || cp -p \"$bin/ham\" \"$bin/ham.prev\"\nprintf '#!/bin/sh\\necho \"ham v%s\"\\n' '{version}' > \"$bin/.ham.setup-new\"\nchmod 755 \"$bin/.ham.setup-new\"\nmv -f \"$bin/.ham.setup-new\" \"$bin/ham\"\nexit 0\n"
    )
}

/// One update in this process: the source with `setup`, a home with the installed client, and what `ham::update::run` needs.
struct Driven {
    fake: Arc<Fake>,
    h: Home,
    bin: PathBuf,
    source: String,
    newest: String,
    sha: String,
}

impl Driven {
    async fn new(label: &str) -> Driven {
        Driven::with_setup(label, setup_installing).await
    }

    /// The same, with the stand-in setup that `setup_of(<the newest version>)` gives.
    async fn with_setup(label: &str, setup_of: impl Fn(&str) -> String) -> Driven {
        let fake = Fake::start().await;
        let newest = next(ham::version::CLIENT, 1);
        let root = support::scratch(&format!("{label}-src"));
        let setup = setup_of(&newest);
        let (repo, sha) = source_repo_of(
            &root,
            &Source {
                version: Some(&newest),
                heading: &newest,
                setup: Some(&setup),
                init: &["init", "-q", "-b", "main"],
            },
        );
        let h = home(&fake, label, &url(&repo));
        let bin = installed(&h);
        Driven {
            source: url(&repo),
            fake,
            h,
            bin,
            newest,
            sha,
        }
    }

    fn clone_dir(&self) -> PathBuf {
        self.h
            .downloads
            .join(format!("ham-update-{}", &self.sha[..12]))
    }

    /// `ham update` (with `--yes` when `yes`) on a terminal whose question `ask` answers; the wait reads the runs every 50 ms for at most
    /// `limit`. Returns the exit code, standard output and standard error.
    async fn update<P: ham::http::ControlPlane>(
        &self,
        api: &Api<P>,
        yes: bool,
        limit: Duration,
        ask: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> (i32, String, String) {
        // the update reads the backend's version after setup, so no other test's answer may change it meanwhile
        let _cell = backend_cell().await;
        let ctx = ham::update::Context {
            source: self.source.clone(),
            downloads: self.h.downloads.clone(),
            git: Git::system(),
            exe: self.bin.canonicalize().unwrap(),
            terminal: true,
            ask,
            backend_recheck: Duration::ZERO,
        };
        let req = ham::update::Request {
            check: false,
            yes,
            json: false,
            drain: DrainOptions {
                poll: Duration::from_millis(50),
                limit,
                no_wait: false,
            },
        };
        let out = Output::capture();
        let code = ham::update::run(api, &req, &ctx, &out).await;
        let done = out.finish(code);
        (code, done.stdout_text(), done.stderr_text())
    }
}

fn never_asked(_: &str) -> Option<String> {
    panic!("--yes asks nothing")
}

/// The one question: any answer but yes changes nothing, and the question names the version.
#[tokio::test]
async fn fix515_the_question_answered_with_no_changes_nothing() {
    let d = Driven::new("fix515-ask-no").await;
    for answer in [Some("n\n"), Some("\n"), Some("maybe\n"), None] {
        let asked = std::sync::Mutex::new(Vec::new());
        let ask = |q: &str| {
            asked.lock().unwrap().push(q.to_string());
            answer.map(str::to_string)
        };
        let (code, out, err) = d
            .update(&api(&d.fake), false, Duration::from_secs(5), &ask)
            .await;
        assert_eq!(
            code,
            ham::update::exit::CANCELLED,
            "{answer:?}: {out}\n{err}"
        );
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            [format!("Update to v{}? [y/N] ", d.newest)],
            "{answer:?}"
        );
        assert!(err.contains("Nothing was changed."), "{answer:?}: {err}");
        assert!(out.contains(&format!("commit:  {}", d.sha)), "{out}");
    }
    assert!(
        !d.clone_dir().join("setup.sh").exists(),
        "nothing was checked out"
    );
    nothing_cancelled(&d.fake);
}

/// The question answered with yes: the update waits until the caller's run has ended, says so, runs setup and checks both versions.
#[tokio::test]
async fn fix515_the_question_answered_with_yes_waits_for_the_runs_and_updates() {
    let d = Driven::new("fix515-ask-yes").await;
    let id = new_run(&d.fake, "fix515 ask yes").await;
    // The run ends two seconds after the commit is checked out, which is when the wait begins: the test follows the update, not a clock.
    let ask = |_: &str| {
        let (f, newest, done) = (d.fake.clone(), d.newest.clone(), id.clone());
        let checked_out = d.clone_dir().join("setup.sh");
        tokio::spawn(async move {
            while !checked_out.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            f.mem.finish_run(&done);
            // The stage answers with the new version once it is deployed.
            *f.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest);
        });
        Some(" yes \n".to_string())
    };
    let (code, out, err) = d
        .update(&api(&d.fake), false, Duration::from_secs(3600), &ask)
        .await;
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    assert!(
        err.contains(&format!("Waiting for 1 run to end: {id}")),
        "{err}"
    );
    assert!(err.contains("Your runs have ended (waited "), "{err}");
    assert!(
        out.contains(&format!("Updated: ham v{0} | backend v{0}.", d.newest)),
        "{out}"
    );
    nothing_cancelled(&d.fake);
}

/// The copy in the download folder changes between the question and the answer: it is not the commit that was shown any more, so nothing of
/// it runs.
#[tokio::test]
async fn fix515_a_copy_that_changed_after_the_question_is_not_run() {
    let d = Driven::new("fix515-ask-tamper").await;
    let planted = d.clone_dir().join("planted.sh");
    let ask = |_: &str| {
        std::fs::write(&planted, "#!/bin/sh\n").unwrap();
        Some("y\n".to_string())
    };
    let before = std::fs::read(&d.bin).unwrap();
    let (code, out, err) = d
        .update(&api(&d.fake), false, Duration::from_secs(5), &ask)
        .await;
    assert_eq!(code, ham::update::exit::REFUSED, "{out}\n{err}");
    assert!(
        err.contains("is not a clean copy of commit") && err.contains("planted.sh"),
        "{err}"
    );
    assert_eq!(std::fs::read(&d.bin).unwrap(), before, "setup did not run");
}

/// The caller's run is still active when the limit of the wait is reached: the update stops, names the run and changes nothing.
#[tokio::test]
async fn fix515_a_run_still_active_at_the_limit_of_the_wait_stops_the_update() {
    let d = Driven::new("fix515-limit").await;
    let id = new_run(&d.fake, "fix515 limit").await;
    let before = std::fs::read(&d.bin).unwrap();
    let (code, out, err) = d
        .update(&api(&d.fake), true, Duration::from_secs(1), &never_asked)
        .await;
    assert_eq!(code, ham::update::exit::REFUSED, "{out}\n{err}");
    assert!(
        err.contains("1 run(s) still active after 1 min")
            && err.contains(&id)
            && err.contains("No deployment or client change was made. No run was cancelled."),
        "{err}"
    );
    assert_eq!(std::fs::read(&d.bin).unwrap(), before, "setup did not run");
    nothing_cancelled(&d.fake);
}

/// Setup ended and the backend still reports the old version: the line says which version it reports, the result line says where things
/// stand, and the exit code is 3.
#[tokio::test]
async fn fix515_a_backend_that_still_reports_the_old_version_after_setup_is_said() {
    let d = Driven::new("fix515-oldbackend").await;
    let (code, out, err) = d
        .update(&api(&d.fake), true, Duration::from_secs(5), &never_asked)
        .await;
    assert_eq!(code, ham::update::exit::CHECK_FAILED, "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "the backend reports v{} after the update, not v{}.",
            ham::version::CLIENT,
            d.newest
        )),
        "{err}"
    );
    assert!(
        out.contains(&format!(
            "After the update: ham v{} | backend v{}.",
            d.newest,
            ham::version::CLIENT
        )),
        "{out}"
    );
    assert!(out.contains("backend and client"), "{out}");
}

/// The stage answers until the list of runs is read for the wait, then no more.
struct ListFails {
    inner: HttpsPlane,
    lists: std::sync::atomic::AtomicU32,
    from: u32,
}

impl ham::http::ControlPlane for ListFails {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<ham::http::Response, ham::http::TransportError> {
        if method == "GET" && (path == "/v1/runs" || path.starts_with("/v1/runs?")) {
            let n = self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n >= self.from {
                return Err(ham::http::TransportError::Network(
                    "the stage went away".into(),
                ));
            }
        }
        self.inner.call(method, path, headers, body).await
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<ham::http::Response, ham::http::TransportError> {
        self.inner.fetch(url, range, if_match).await
    }
}

/// The list of runs cannot be read when the wait starts: ham does not know whether a run is active, so it stops before setup.
#[tokio::test]
async fn fix515_runs_that_cannot_be_read_for_the_wait_stop_the_update() {
    let d = Driven::new("fix515-listfails").await;
    let plane = ListFails {
        inner: HttpsPlane::new(&d.fake.endpoint, Signer::new(REGION, creds())).unwrap(),
        lists: std::sync::atomic::AtomicU32::new(0),
        // The first read is the one for the backend's version; the second is the wait's.
        from: 2,
    };
    let api = Api::new(Arc::new(plane)).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let before = std::fs::read(&d.bin).unwrap();
    let (code, out, err) = d
        .update(&api, true, Duration::from_secs(5), &never_asked)
        .await;
    assert_eq!(code, ham::update::exit::FAILED, "{out}\n{err}");
    assert!(
        err.contains("could not read your runs") && err.contains("the stage went away"),
        "{err}"
    );
    assert_eq!(std::fs::read(&d.bin).unwrap(), before, "setup did not run");
}

/// An `update_source` that is neither an `https://` address, a `file:///` path nor the word `off` is a usage error, answered before any
/// connection is made.
#[tokio::test]
async fn fix515_an_update_source_that_is_no_address_is_a_usage_error() {
    let fake = Fake::start().await;
    let h = home(&fake, "fix515-badsource", "ssh://example.com/ham.git");
    for args in [CHECK, YES] {
        let o = run(&h, ham_bin(), &args, &[]).await;
        let err = text(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("update_source"), "{args:?}: {err}");
    }
    assert!(fake.mem.seen().is_empty(), "no API call was made");
    assert!(std::fs::read_dir(&h.downloads).unwrap().next().is_none());
}

/// A new client that never answers `--version` is given up after 30 seconds and replaced by the earlier one; the line says that it gave no
/// answer.
#[tokio::test]
async fn fix515_a_new_client_that_does_not_answer_is_replaced_by_the_earlier_one() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-hangs-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-hangs", &url(&repo));
    let bin = installed(&h);
    let before = std::fs::read(&bin).unwrap();
    let o = run(&h, &bin, &YES, &[("B804_INSTALL_KIND", "hangs")]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "{err}");
    assert!(
        err.contains("the new ham gave no answer (no answer within 30 seconds), not v"),
        "{err}"
    );
    assert!(err.contains("restored"), "{err}");
    assert_eq!(std::fs::read(&bin).unwrap(), before);
}

/// A new client whose `--version` prints a line that is no version is replaced by the earlier one, and the line quotes what it printed.
#[tokio::test]
async fn fix515_a_new_client_that_prints_no_version_is_replaced_by_the_earlier_one() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-odd-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-odd", &url(&repo));
    let bin = installed(&h);
    let before = std::fs::read(&bin).unwrap();
    let o = run(&h, &bin, &YES, &[("B804_INSTALL_KIND", "odd")]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(3), "{err}");
    assert!(
        err.contains("the new ham answered with the line \"hello\", not v"),
        "{err}"
    );
    assert_eq!(std::fs::read(&bin).unwrap(), before);
}

/// A new client that fails to start, with no earlier client beside it: nothing can be put back, and the result line does not invent a version.
#[tokio::test]
async fn fix515_a_broken_client_with_no_earlier_one_gets_a_result_line_without_a_version() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("fix515-broken-noprev-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "fix515-broken-noprev", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &YES,
        &[("B804_INSTALL_KIND", "broken"), ("B804_PREV", "none")],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{out}\n{err}");
    assert!(
        err.contains("answered with an error (this client cannot start), not v")
            && err.contains("ham.prev cannot be read"),
        "{err}"
    );
    assert!(
        out.contains(&format!(
            "After the update: ham of an unknown version | backend v{newest}."
        )),
        "{out}"
    );
}

/// `ham update --check` on a stage and a `ham` that run the newest version says so, lists no change and tells nothing to install.
#[tokio::test]
async fn fix515_check_on_a_current_stage_says_up_to_date() {
    let fake = Fake::start().await;
    let root = support::scratch("fix515-current-src");
    let (repo, sha) = source_repo(&root, ham::version::CLIENT);
    let h = home(&fake, "fix515-current", &url(&repo));
    let o = run(&h, ham_bin(), &CHECK, &[]).await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    let version = ham::version::CLIENT;
    assert!(
        out.contains(&format!("Installed: ham v{version} | backend v{version}")),
        "{out}"
    );
    assert!(out.contains(&sha[..12]), "{out}");
    assert!(out.contains("Up to date."), "{out}");
    assert!(!out.contains("Run ham update"), "{out}");
    assert!(!h.record.exists(), "--check runs no setup");
}

/// Setup ends with exit 3 (the client is installed and `ham doctor` found a problem) and the stage then does not answer the read of its
/// version, which is the state in which doctor fails. `ham update` says that the backend did not answer and gives the backend no version:
/// the one it remembers was read before setup ran (re-review of the 5.15 integration head, finding N4).
#[tokio::test]
async fn fix515b_after_setup_exit_3_a_stage_that_does_not_answer_is_given_no_version() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let root = support::scratch("fix515b-exit3-src");
    let setup = setup_installing(&newest).replace("exit 0\n", "exit 3\n");
    assert!(setup.ends_with("exit 3\n"), "{setup}");
    let (repo, sha) = source_repo_of(
        &root,
        &Source {
            version: Some(&newest),
            heading: &newest,
            setup: Some(&setup),
            init: &["init", "-q", "-b", "main"],
        },
    );
    let h = home(&fake, "fix515b-exit3", &url(&repo));
    let bin = installed(&h);
    let d = Driven {
        source: url(&repo),
        fake,
        h,
        bin,
        newest: newest.clone(),
        sha,
    };
    let plane = Arc::new(ListFails {
        inner: HttpsPlane::new(&d.fake.endpoint, Signer::new(REGION, creds())).unwrap(),
        lists: std::sync::atomic::AtomicU32::new(0),
        // The first read is the one for the backend's version, the second the wait's, the third the one after setup.
        from: 3,
    });
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let (code, out, err) = d
        .update(&api, true, Duration::from_secs(5), &never_asked)
        .await;
    let reads = plane.lists.load(std::sync::atomic::Ordering::SeqCst);
    println!(
        "FIX515B TRIGGER PRESENT: setup ended with exit 3 and the read after it got no answer; code={code} reads={reads}\nSTDOUT:\n{out}\nSTDERR:\n{err}"
    );
    // The trigger: setup ran and ended with 3, the new client is installed, and the one read after setup is the read that failed.
    assert!(
        err.contains("exit 3") && err.contains("ham doctor"),
        "{err}"
    );
    assert_eq!(reads, 3, "the read after setup is the third");
    let installed_now = std::process::Command::new(&d.bin)
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(text(&installed_now.stdout).trim(), format!("ham v{newest}"));
    // What ham update says about the backend.
    let old = ham::version::CLIENT;
    assert!(
        err.contains("the backend did not answer after the update")
            && err.contains("the stage went away"),
        "the line says that the backend did not answer, and why:\n{err}"
    );
    assert!(
        !err.contains("the backend reports"),
        "nothing was read after the update, so nothing is reported:\n{err}"
    );
    assert!(
        out.contains(&format!("After the update: ham v{newest} | backend v?.??.")),
        "the result line gives the backend no version:\n{out}"
    );
    let after_setup = out.split("After the update:").nth(1).unwrap_or_default();
    assert!(
        !after_setup.contains(old) && !err.contains(&format!("v{old}")),
        "v{old} was read before setup and is not printed for the time after it:\n{out}\n{err}"
    );
    assert_eq!(code, ham::update::exit::CHECK_FAILED, "{out}\n{err}");
}

/// The stage answers the list of runs with its version header and a body this client cannot read.
struct ListUnreadable {
    inner: HttpsPlane,
}

impl ham::http::ControlPlane for ListUnreadable {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<ham::http::Response, ham::http::TransportError> {
        let mut answer = self.inner.call(method, path, headers, body).await?;
        if method == "GET" && (path == "/v1/runs" || path.starts_with("/v1/runs?")) {
            answer.body = b"a list this client cannot read".to_vec();
        }
        Ok(answer)
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<ham::http::Response, ham::http::TransportError> {
        self.inner.fetch(url, range, if_match).await
    }
}

/// The backend's version is one process-wide cell (`ham::version`) that every answer of a stand-in stage rewrites. A test that reads what the cell holds after its own
/// answers (through `backend_after`, as `ham update` does after setup) holds this gate while it does, so that in a threaded `cargo test` run another test's answer cannot
/// change the version it reads between its request and its read. nextest starts a process for each test and is not slowed by it.
static BACKEND_CELL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn backend_cell() -> tokio::sync::MutexGuard<'static, ()> {
    BACKEND_CELL.lock().await
}

/// The version `backend_after` gives is one an answer gave after the update, never the one the client remembered from before it. A read
/// that gets no answer gives the failure. A read that fails after its answer carried the version header (a list this client cannot decode,
/// as a newer backend may send) still gives that version: the read is made for the header alone (re-review of the 5.15 integration head,
/// finding N4).
#[tokio::test]
async fn fix515b_backend_after_gives_only_a_version_read_after_the_update() {
    use ham::update::backend_after;
    let _cell = backend_cell().await;
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let once = Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    };
    let plane = |from: u32| ListFails {
        inner: HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
        lists: std::sync::atomic::AtomicU32::new(0),
        from,
    };

    // No answer: the version remembered from before the update is not given.
    ham::version::set_backend_for_test(Some(ham::version::CLIENT));
    let silent = Api::new(Arc::new(plane(1))).with_retry(once);
    let now = backend_after(&silent, &newest, Duration::ZERO, Duration::from_millis(1)).await;
    let why = now.expect_err("a read with no answer gives no version");
    assert!(why.to_string().contains("the stage went away"), "{why}");

    // An answer with the version header and a body the client cannot read: the read fails, and the version of that answer is given.
    ham::version::set_backend_for_test(Some(ham::version::CLIENT));
    let unreadable = Api::new(Arc::new(ListUnreadable {
        inner: HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    }))
    .with_retry(once);
    let read = unreadable.list_runs().await;
    println!(
        "FIX515B TRIGGER PRESENT: the read of the runs fails after an answer with the header: {read:?}"
    );
    assert!(
        matches!(read, Err(ham::api::ApiError::Decode { .. })),
        "{read:?}"
    );
    ham::version::set_backend_for_test(Some(ham::version::CLIENT));
    let now = backend_after(
        &unreadable,
        &newest,
        Duration::ZERO,
        Duration::from_millis(1),
    )
    .await;
    assert_eq!(
        now.expect("the answer carried the version").as_deref(),
        Some(newest.as_str())
    );

    // An answer, then reads with none: the version of that answer was read after the update, and is given.
    ham::version::set_backend_for_test(Some("1.00"));
    *fake.faults.backend_header.lock().unwrap() =
        BackendHeader::Value(ham::version::CLIENT.to_string());
    let answers_once = Api::new(Arc::new(plane(2))).with_retry(once);
    let now = backend_after(
        &answers_once,
        &newest,
        Duration::from_millis(100),
        Duration::from_millis(20),
    )
    .await;
    assert_eq!(
        now.expect("one read after the update answered").as_deref(),
        Some(ham::version::CLIENT),
        "the stage answered once after the update with the old version, then went away"
    );
}

/// What the stand-in setup was started with: its first record line.
fn setup_arguments(h: &Home) -> String {
    std::fs::read_to_string(&h.record)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn b804_wait_ham_update_hands_setup_the_rest_of_its_limit() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("b804-wait-pass-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "b804-wait-pass", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes", "--wait", "7"],
        &[("B804_INSTALL_VERSION", &newest)],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    let prefix = h.root.canonicalize().unwrap();
    assert_eq!(
        setup_arguments(&h),
        format!(
            "--update --prefix {} --yes --wait-for-runs 7",
            prefix.display()
        ),
        "one limit: setup waits for everyone's runs with the minutes ham has left"
    );
    assert!(
        out.contains("runs of everyone on the stage") && out.contains("new runs are refused"),
        "the plan says what the wait covers:\n{out}"
    );
    nothing_cancelled(&fake);
}

#[tokio::test]
async fn b804_wait_no_wait_hands_setup_no_wait() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(newest.clone());
    let root = support::scratch("b804-wait-none-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "b804-wait-none", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes", "--no-wait"],
        &[("B804_INSTALL_VERSION", &newest)],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    let prefix = h.root.canonicalize().unwrap();
    assert_eq!(
        setup_arguments(&h),
        format!("--update --prefix {} --yes", prefix.display()),
        "--no-wait: setup stops at once when anyone's run is active"
    );
    nothing_cancelled(&fake);
}

#[test]
fn b804_wait_the_minutes_handed_to_setup_are_what_is_left_of_the_limit() {
    use ham::update::minutes_left;
    let limit = Duration::from_secs(60 * 60);
    assert_eq!(minutes_left(limit, Duration::ZERO), 60);
    assert_eq!(
        minutes_left(limit, Duration::from_secs(1)),
        60,
        "rounded up"
    );
    assert_eq!(minutes_left(limit, Duration::from_secs(61)), 59);
    assert_eq!(minutes_left(limit, Duration::from_secs(59 * 60 + 30)), 1);
    assert_eq!(
        minutes_left(limit, Duration::from_secs(61 * 60)),
        1,
        "never less than a minute: setup takes 1 to 1440"
    );
    assert_eq!(
        minutes_left(Duration::from_secs(1440 * 60), Duration::ZERO),
        1440
    );
}

/// A deploy took the stage's lock a minute ago: the stage starts no new runs.
fn close_stage(fake: &Fake) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    fake.mem.inner.lock().unwrap().deploy_lock = Some(control_api::handler::DeployLock {
        value:
            "token=tok-b804\nholder=laptop:4242\nroute=runner\nstarted=2026-11-11T16:00:00Z\nbuild="
                .into(),
        written_epoch_s: Some(now - 60),
    });
}

const SENTENCE: &str =
    "This stage is being updated, so it starts no new runs. Try again in a few minutes.";

#[tokio::test]
async fn b804_wait_ham_shows_the_sentence_of_a_stage_that_is_being_updated_and_running_work_can_end()
 {
    let fake = Fake::start().await;
    let root = support::scratch("b804-wait-refused-src");
    let (repo, _) = source_repo(&root, ham::version::CLIENT);
    let h = home(&fake, "b804-wait-refused", &url(&repo));
    let bin = installed(&h);
    // a run that started before the update
    let earlier = new_run(&fake, "an earlier run").await;
    close_stage(&fake);
    let o = run(
        &h,
        &bin,
        &["Add a function", "--s3-only", "--direct", "--no-rules"],
        &[],
    )
    .await;
    let (out, err) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert_eq!(
        err.lines().last().unwrap_or_default(),
        format!("ham: {SENTENCE}"),
        "the service's sentence alone, without the route and the status:\n{err}"
    );
    assert!(!out.contains("started run"), "{out}");
    assert_eq!(
        fake.mem.run_ids(),
        vec![earlier.clone()],
        "no run was created"
    );
    // the list and a cancel still answer: running work can end by itself or by its owner's hand
    let o = run(&h, &bin, &["runs"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains(&earlier), "{}", text(&o.stdout));
    let o = run(&h, &bin, &["cancel", &earlier], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
}

#[tokio::test]
async fn b804_wait_the_answer_of_the_stage_carries_retry_after_and_an_older_client_prints_a_line_that_holds_the_sentence()
 {
    let fake = Fake::start().await;
    close_stage(&fake);
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": "Add a function", "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    let e = api(&fake)
        .start_direct_run(&req, "k-refused")
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(409));
    assert!(!e.is_retryable(), "a 409 is shown at once, not retried");
    // Releases 5.14 and 5.15 have no case for this code: `ham "<prompt>"` prints `ham: POST /v1/direct/runs: <this text>`, which holds the sentence.
    assert_eq!(
        e.to_string(),
        format!("HTTP 409: {SENTENCE} (stage-updating)")
    );
    // their retry and resume lines fall to the generic form of a code they do not know
    assert_eq!(
        ham_core::retry::explain(409, "a-code-of-a-later-release", SENTENCE),
        format!("409 a-code-of-a-later-release: {SENTENCE}")
    );
    assert_eq!(
        ham_core::resume::explain(409, "a-code-of-a-later-release", SENTENCE),
        format!("409 a-code-of-a-later-release: {SENTENCE}")
    );
    // this release shows the sentence alone, everywhere a start can be refused
    assert_eq!(ham::api::start_failure(&e), SENTENCE);
    assert_eq!(
        ham_core::retry::explain(409, "stage-updating", SENTENCE),
        SENTENCE
    );
    assert_eq!(
        ham_core::resume::explain(409, "stage-updating", SENTENCE),
        SENTENCE
    );
    let posts = fake
        .mem
        .seen()
        .into_iter()
        .filter(|s| s.method == "POST" && s.path.ends_with("/direct/runs"))
        .count();
    assert_eq!(posts, 1, "one request, no retry");
}

/// A backend whose version is above the one on main (a label left by a proof or a withdrawn release) stops `ham update` at the plan, before setup runs. The message names the
/// deploy that puts the backend on main's version; a `ham` that is newer than main has no such way, and its message does not offer one.
#[tokio::test]
async fn b839_the_refusal_for_a_backend_newer_than_main_names_the_deploy_that_sets_its_version() {
    let fake = Fake::start().await;
    let newest = next(ham::version::CLIENT, 1);
    let above = next(ham::version::CLIENT, 2);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(above.clone());
    let root = support::scratch("b839-backend-above-src");
    let (repo, _) = source_repo(&root, &newest);
    let h = home(&fake, "b839-backend-above", &url(&repo));
    let bin = installed(&h);
    let o = run(
        &h,
        &bin,
        &["update", "--yes"],
        &[("B804_INSTALL_VERSION", &newest)],
    )
    .await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(7), "{err}");
    assert!(
        err.contains(&format!(
            "the backend runs v{above}, newer than v{newest} on main"
        )),
        "TRIGGER PRESENT: the refusal is the one of a backend above main: {err}"
    );
    assert!(
        err.contains("./setup.sh --update --force-deploy in a clone of main"),
        "the message names the way out: {err}"
    );
    assert!(!h.record.exists(), "no setup ran");

    // a ham that is newer than main: a deploy does not change it, so no deploy is offered
    let (major, counter) = ham::version::CLIENT.split_once('.').unwrap();
    let older = format!("{major}.{:02}", counter.parse::<u32>().unwrap() - 1);
    let root = support::scratch("b839-client-above-src");
    let (repo, _) = source_repo(&root, &older);
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Same;
    let h = home(&fake, "b839-client-above", &url(&repo));
    let bin = installed(&h);
    let o = run(&h, &bin, &["update", "--yes"], &[]).await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(7), "{err}");
    assert!(err.contains("ham runs v"), "{err}");
    assert!(!err.contains("--force-deploy"), "{err}");
}

/// Two stages in one process, one that answers with this ham's version and one that answers with a newer one, as two tests of this binary do when `cargo test` runs them on
/// threads. The reader holds the gate for each read; the writer holds it for each answer; so a read never gets the version the other stage's answer wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn b839_a_read_of_the_backend_version_does_not_get_the_answer_of_another_test() {
    use ham::update::backend_after;
    let reader = Fake::start().await;
    let writer = Fake::start().await;
    *writer.faults.backend_header.lock().unwrap() =
        BackendHeader::Value(next(ham::version::CLIENT, 1));
    let reads = api(&reader);
    let writes = api(&writer);
    let other = tokio::spawn(async move {
        for _ in 0..300 {
            let _cell = backend_cell().await;
            let _ = writes.list_runs().await;
            tokio::task::yield_now().await;
        }
    });
    for i in 0..300 {
        let _cell = backend_cell().await;
        let now = backend_after(&reads, "0.00", Duration::ZERO, Duration::from_millis(1)).await;
        assert_eq!(
            now.expect("the stage answers").as_deref(),
            Some(ham::version::CLIENT),
            "read {i}: the version is the one this stage's answer carried"
        );
    }
    other.await.unwrap();
}

// ---- (U55): a release that changes no file the stage runs ------------------------------------------------------------------------------

/// A stand-in setup of such a release: it installs the client as `setup_installing` does and deploys nothing; what it writes to the stage is the release label,
/// here a marker file that the stand-in stage reads, and it ends a moment later (the real setup builds the client after it writes the label).
fn setup_labelling(version: &str, marker: &Path) -> String {
    let base = setup_installing(version);
    let base = base.strip_suffix("exit 0\n").unwrap();
    format!(
        "{base}printf '%s\\n' '{version}' > '{}'\nsleep 1\nexit 0\n",
        marker.display()
    )
}

/// The stand-in stage reports the label once the marker holds it, as control-api reports its `release-version` parameter once setup has written it.
fn stage_follows_label(fake: &Arc<Fake>, marker: PathBuf) {
    let fake = fake.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(v) = std::fs::read_to_string(&marker)
                && !v.trim().is_empty()
            {
                *fake.faults.backend_header.lock().unwrap() =
                    BackendHeader::Value(v.trim().to_string());
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

/// A stage that runs the source tree of the release already (setup deploys nothing) and whose label becomes the release's: the update ends `Updated`, exit 0,
/// with no word about a backend that reports another version. (A stage that keeps reporting the older version is the next case.)
#[tokio::test]
async fn b839_u55_a_stage_whose_setup_deployed_nothing_and_wrote_its_label_ends_updated() {
    let marker = support::scratch("b839-label").join("label");
    let m = marker.clone();
    let d = Driven::with_setup("b839-label", move |v| setup_labelling(v, &m)).await;
    assert_eq!(
        *d.fake.faults.backend_header.lock().unwrap(),
        BackendHeader::Same,
        "TRIGGER PRESENT: the stage reports this ham's version until setup has written its label"
    );
    stage_follows_label(&d.fake, marker);
    let (code, out, err) = d
        .update(&api(&d.fake), true, Duration::from_secs(5), &never_asked)
        .await;
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    assert!(
        out.contains(&format!("Updated: ham v{0} | backend v{0}.", d.newest)),
        "{out}"
    );
    assert!(!err.contains("the backend reports"), "{err}");
    // `ham doctor` shows the same two versions as equal
    let c = ham::doctor::versions_check(&d.newest, Some(&d.newest));
    assert!(c.pass && !c.warn, "{c:?}");
}

/// The command that gets the backend deployed again, as `ham update` prints it: the prefix of the installed client is the one setup was given, so a second client does not
/// land in another folder, and the folder is the copy that `ham update` checked out.
fn way_out(d: &Driven) -> String {
    format!(
        "./setup.sh --update --force-deploy --prefix {} in {}",
        d.h.root.canonicalize().unwrap().display(),
        d.clone_dir().display()
    )
}

/// Setup ended without an error and the backend still reports the older version: the sentence names what that means and the two things to do.
#[tokio::test]
async fn b839_a_backend_that_still_reports_the_older_version_after_a_clean_setup_names_the_way_out()
{
    let d = Driven::new("b839-oldbackend").await;
    let (code, out, err) = d
        .update(&api(&d.fake), true, Duration::from_secs(5), &never_asked)
        .await;
    assert_eq!(code, ham::update::exit::CHECK_FAILED, "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "the backend reports v{} after the update, not v{}.",
            ham::version::CLIENT,
            d.newest
        )),
        "{err}"
    );
    assert!(err.contains("setup ended without an error"), "{err}");
    assert!(err.contains("ham doctor"), "{err}");
    assert!(
        err.contains(&way_out(&d)),
        "the sentence names the command, with the prefix of the installed client, and the folder to run it in: {err}"
    );
    assert!(
        !err.contains("within a minute"),
        "ham had waited for the backend already, so it does not promise one more minute: {err}"
    );
    // a backend that is newer than the update, or that answers with no version, is a different state and gets no such advice
    *d.fake.faults.backend_header.lock().unwrap() = BackendHeader::Absent;
    let (_, _, err) = d
        .update(&api(&d.fake), true, Duration::from_secs(5), &never_asked)
        .await;
    assert!(!err.contains("setup ended without an error"), "{err}");
}

/// Setup ended with exit 3 or 8 (the update took effect, but setup reported a problem) and the backend still reports the older version: the line says what setup
/// reported, not that it ended without an error, and names the same two ways out.
#[tokio::test]
async fn b839_a_backend_that_still_reports_the_older_version_after_setup_exit_3_or_8_says_what_setup_reported()
 {
    for code in [3, 8] {
        let d = Driven::with_setup(&format!("b839-oldbackend-exit{code}"), move |v| {
            setup_installing(v).replace("exit 0\n", &format!("exit {code}\n"))
        })
        .await;
        let (got, out, err) = d
            .update(&api(&d.fake), true, Duration::from_secs(5), &never_asked)
            .await;
        println!(
            "B839 TRIGGER PRESENT: setup ended with exit {code} and the backend reports {}; code={got}\n{err}",
            ham::version::CLIENT
        );
        assert_eq!(got, ham::update::exit::CHECK_FAILED, "{out}\n{err}");
        assert!(
            err.contains(&format!("setup.sh --update ended with exit {code}")),
            "{err}"
        );
        assert!(
            err.contains(&format!(
                "the backend reports v{} after the update, not v{}.",
                ham::version::CLIENT,
                d.newest
            )),
            "{err}"
        );
        assert!(
            !err.contains("setup ended without an error"),
            "a setup that reported a problem did not end without an error: {err}"
        );
        assert!(
            err.contains(&format!("setup reported exit {code}")),
            "{err}"
        );
        assert!(err.contains("ham doctor"), "{err}");
        assert!(err.contains(&way_out(&d)), "{err}");
        assert!(!err.contains("within a minute"), "{err}");
    }
}
