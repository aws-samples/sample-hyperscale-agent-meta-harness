//! The `ham` binary end to end against the local `/v1` API:
//! credentials from a `credential_process` profile named in `config.toml`, one signed
//! `POST /v1/direct/runs`, watch to completion, the automatic download and the final card; `ham watch`
//! rehydrates from the API with empty state and cache directories; `ham pull` and `ham link`; exit
//! codes; and no prompt, key or token ever reaches stdout or stderr unasked.
#![allow(clippy::unwrap_used)]

mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::Ordering;
use support::{AKID, Fake, SECRET, TOKEN};

/// The local `/v1` API as the `direct` stage, with the autopilot finishing each run it starts (or none).
async fn direct_fake(autopilot: bool) -> std::sync::Arc<Fake> {
    Fake::start_with(autopilot).await
}

const POSTS: &str = "/v1/direct/runs";

struct Home {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    downloads: PathBuf,
}

/// Fresh XDG directories, a config naming the fake and a `credential_process` profile.
fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
        downloads: root.join("downloads"),
        root,
    };
    for d in [&h.config, &h.state, &h.cache, &h.downloads] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint,
            support::REGION
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
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            support::REGION
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
}

/// A clean environment that keeps coverage instrumentation (`LLVM_PROFILE_FILE` and friends).
fn clean(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    ham_env(h, args, &[]).await
}

async fn ham_env(h: &Home, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    clean(&mut cmd)
        .args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", &h.config)
        .env("XDG_STATE_HOME", &h.state)
        .env("XDG_CACHE_HOME", &h.cache)
        .env("HAMH_DOWNLOAD_DIR", &h.downloads)
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        // After the defaults, so that a case can replace one (a PATH with no AWS CLI on it).
        .envs(extra.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn is_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p).unwrap().next().is_none()
}

/// Nothing secret in either stream: no signing secret, session token or SigV4 header. (The
/// presigned URL is printed on purpose: P16.)
fn assert_no_secrets(o: &Output) {
    for stream in [text(&o.stdout), text(&o.stderr)] {
        for secret in [
            SECRET,
            TOKEN,
            "AWS4-HMAC-SHA256",
            ", Signature=",
            "authorization",
        ] {
            assert!(!stream.contains(secret), "{secret:?} leaked:\n{stream}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_starts_one_run_watches_it_downloads_the_zip_and_exits_0() {
    let fake = direct_fake(true).await;
    let h = home(&fake, "cli-start");
    let o = ham(
        &h,
        &[
            "Add a percent_change(a, b) function with tests",
            "--s3-only",
            "--direct",
            "--no-rules",
            "--json",
        ],
    )
    .await;
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    let lines: Vec<Value> = text(&o.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let run = lines[0]["runId"].as_str().unwrap().to_string();
    assert!(lines[0]["idempotencyKey"].is_string());
    let last = lines.last().unwrap();
    assert_eq!(last["exitCode"], 0);
    assert_eq!(last["run"]["status"], "complete");
    let path = PathBuf::from(last["download"]["path"].as_str().unwrap());
    assert!(path.starts_with(&h.downloads) && path.exists(), "{path:?}");
    let posts = fake.mem.posts(POSTS);
    assert_eq!(posts.len(), 1);
    assert!(!posts[0].headers["idempotency-key"].is_empty());
    let seen = fake.mem.seen();
    assert!(
        seen.iter()
            .filter(|s| !s.path.starts_with("/objects/"))
            .all(|s| s.signature.is_ok() && s.headers["x-amz-security-token"] == TOKEN)
    );
    assert!(fake.mem.inner.lock().unwrap().runs.contains_key(&run));
    assert_no_secrets(&o);
    assert!(is_empty_dir(&h.state) && is_empty_dir(&h.cache));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_rehydrates_with_empty_state_and_pull_and_link_print_the_card() {
    let fake = direct_fake(true).await;
    let first = home(&fake, "cli-first");
    let o = ham(
        &first,
        &[
            "rehydrate me",
            "--s3-only",
            "--direct",
            "--no-rules",
            "--json",
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let run =
        serde_json::from_str::<Value>(text(&o.stdout).lines().next().unwrap()).unwrap()["runId"]
            .as_str()
            .unwrap()
            .to_string();

    // A new client host state: fresh XDG directories, the same profile.
    let h = home(&fake, "cli-rehydrate");
    let o = ham(&h, &["watch", &run]).await;
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    let out = text(&o.stdout);
    for want in [
        format!("Run       {run} complete (delivered)"),
        "Outcome   Cline reported done".to_string(),
        format!(
            "Zip       {}",
            h.downloads
                .join(format!("python-calc-{run}-abc1234.zip"))
                .display()
        ),
    ] {
        assert!(out.contains(&want), "missing {want:?}:\n{out}");
    }
    assert!(
        is_empty_dir(&h.state) && is_empty_dir(&h.cache),
        "no local state"
    );
    let config_files: Vec<_> = std::fs::read_dir(h.config.join("ham")).unwrap().collect();
    // A default user has no rules, skills or engine files: invariant 5 lets the folders exist only when the user makes them.
    assert_eq!(config_files.len(), 1, "only config.toml");
    assert_no_secrets(&o);

    let o = ham(&h, &["pull", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(
        out.contains(&format!("python-calc-{run}-abc1234.zip")),
        "{out}"
    );
    assert!(out.contains("(expires 20"), "{out}");
    assert!(out.contains("Outcome   Cline reported done"), "{out}");
    assert!(!out.contains("Evidence"), "{out}");

    let o = ham(&h, &["pull", &run, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert_eq!(v["runId"], run.as_str());
    assert_eq!(v["sha256"].as_str().unwrap().len(), 64);
    assert!(v["url"].as_str().unwrap().contains("X-Amz-Expires=900"));
    assert!(v["expiresAt"].is_string());
    assert!(v.get("evidence").is_none(), "{v}");
    assert_eq!(v["label"], "Cline reported done");
    assert_eq!(v["outcome"], "reported_done");

    let o = ham(&h, &["link", &run]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(
        out.contains("/objects/runs/") && out.contains("expires "),
        "{out}"
    );

    let o = ham(&h, &["runs", "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert!(
        v["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["runId"] == run.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_config_and_api_errors_map_to_exit_codes_without_leaking_the_prompt() {
    let fake = direct_fake(false).await;
    let h = home(&fake, "cli-errors");
    for args in [
        vec!["--repo", "cc:x"],
        vec!["x", "--repo", "cc:a", "--s3-only"],
        vec!["watch", "a", "b"],
        vec!["pull"],
    ] {
        let o = ham(&h, &args).await;
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(
            text(&o.stderr).starts_with("usage:"),
            "{args:?}: {}",
            text(&o.stderr)
        );
    }

    let secret_prompt = "a prompt that must never be logged 7f3a";
    fake.faults.post_503.store(100, Ordering::SeqCst);
    let o = ham(&h, &[secret_prompt, "--s3-only", "--direct", "--no-rules"]).await;
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(err.contains("retry with the same Idempotency-Key"), "{err}");
    assert!(!err.contains(secret_prompt) && !text(&o.stdout).contains(secret_prompt));
    assert_no_secrets(&o);
    assert!(fake.mem.posts(POSTS).len() > 1, "retried");
    let keys: std::collections::BTreeSet<String> = fake
        .mem
        .posts(POSTS)
        .iter()
        .map(|p| p.headers["idempotency-key"].clone())
        .collect();
    assert_eq!(keys.len(), 1, "every retry reuses the key");

    let o = ham(&h, &["watch", "01K5ZQ4T8M3N6P7R9S0V1W2X3Y"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("no run"), "{}", text(&o.stderr));

    let bare = support::scratch("cli-noconfig");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    let o = clean(&mut cmd)
        .args(["runs"])
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &bare)
        .env("XDG_CONFIG_HOME", bare.join("nothing"))
        .output()
        .await
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(
        err.contains("api_endpoint") && err.contains("config.toml"),
        "{err}"
    );

    // The commands of the multi-agent line are not commands: a word that is no subcommand is a prompt, and a prompt needs a target.
    let o = ham(&h, &["steer", "A-none", "-m", "x"]).await;
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).starts_with("usage:"), "{}", text(&o.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_credentials_the_newest_run_and_the_remaining_output_forms() {
    let fake = direct_fake(true).await;
    let h = home(&fake, "cli-env");
    let o = ham(
        &h,
        &["first run", "--s3-only", "--direct", "--no-rules", "--json"],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let o = ham(&h, &["second run", "--s3-only", "--direct", "--no-rules"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    let newest = out
        .lines()
        .next()
        .unwrap()
        .strip_prefix("started run ")
        .unwrap()
        .to_string();

    // `ham watch` with no run id watches the newest run.
    let o = ham(&h, &["watch"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains(&format!("Run       {newest} complete")));

    let o = ham(&h, &["runs"]).await;
    let out = text(&o.stdout);
    assert_eq!(out.lines().count(), 2, "{out}");
    // The status is the Run item's (the harness updates it; the local API does not).
    let first = out.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("{newest}  active (planning)  s3-only  20")),
        "{out}"
    );

    let o = ham(&h, &["link", &newest, "--json"]).await;
    let v: Value = serde_json::from_str(text(&o.stdout).trim()).unwrap();
    assert!(v["url"].as_str().unwrap().contains("X-Amz-Expires=900"));
    assert_eq!(v["runId"], newest.as_str());

    // A run without a partial zip names what it has.
    let o = ham(&h, &["pull", &newest, "--artifact", "partial"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("no matching artifact; the run has"),
        "{}",
        text(&o.stderr)
    );

    // No awsProfile or region in config.toml: the environment's credentials and Region.
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!("apiEndpoint = \"{}\"\n", fake.endpoint),
    )
    .unwrap();
    let o = ham_env(
        &h,
        &["runs", "--json"],
        &[
            ("AWS_ACCESS_KEY_ID", AKID),
            ("AWS_SECRET_ACCESS_KEY", SECRET),
            ("AWS_SESSION_TOKEN", TOKEN),
            ("AWS_REGION", support::REGION),
        ],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_no_secrets(&o);

    // No Region anywhere, or a profile that does not exist: exit 1 with the reason.
    let o = ham(&h, &["runs"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("no Region"), "{}", text(&o.stderr));
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"missing\"\n",
            fake.endpoint,
            support::REGION
        ),
    )
    .unwrap();
    let o = ham(&h, &["runs"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("AWS credentials"),
        "{}",
        text(&o.stderr)
    );
    std::fs::write(h.config.join("ham/config.toml"), "surprise = 1\n").unwrap();
    let o = ham(&h, &["runs"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("config.toml"),
        "{}",
        text(&o.stderr)
    );
    std::fs::write(
        h.config.join("ham/config.toml"),
        "apiEndpoint = \"http://example.com/live\"\nregion = \"us-west-2\"\nawsProfile = \"ham-test\"\n",
    )
    .unwrap();
    let o = ham(&h, &["runs"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("loopback"), "{}", text(&o.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assume_role_profile_with_no_aws_cli_is_refused_with_one_reason_and_no_panic() {
    // ham links aws-config without its HTTPS client (S11) and so cannot assume a role itself; it asks the AWS CLI. With no AWS CLI on
    // PATH the profile is refused with one reason, never a panic, however cargo unified the features, and ham calls nothing. (PATH holds
    // an empty directory so that no AWS CLI of the machine running the test is started.)
    let fake = Fake::start().await;
    let h = home(&fake, "cli-no-aws-cli");
    std::fs::write(
        h.root.join("aws-config"),
        "[profile ham-test]\nrole_arn = arn:aws:iam::123456789012:role/ham-test\nsource_profile = base\n\
         region = us-west-2\n[profile base]\naws_access_key_id = AKIDEXAMPLE\n\
         aws_secret_access_key = example-secret\n",
    )
    .unwrap();
    let empty = h.root.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let started = std::time::Instant::now();
    let o = ham_env(
        &h,
        &["runs", "--json"],
        &[("PATH", &empty.display().to_string())],
    )
    .await;
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(err.contains("the AWS CLI was not found"), "{err}");
    assert!(err.contains("2.9.0"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "{err}"
    );
    assert_no_secrets(&o);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nonce_from_the_environment_makes_the_idempotency_key_reproducible() {
    // The live E2E sets HAMH_IDEMPOTENCY_NONCE so it can resend the composer's request with the
    // composer's key and prove the API answers with the same run.
    let fake = direct_fake(true).await;
    let h = home(&fake, "cli-nonce");
    let argv = [
        "nonce prompt",
        "--repo",
        "cc:hamh-e2e-python-calc",
        "--direct",
        "--no-rules",
        "--json",
    ];
    let o = ham_env(&h, &argv, &[("HAMH_IDEMPOTENCY_NONCE", "e2e-nonce-1")]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let first: Value = serde_json::from_str(text(&o.stdout).lines().next().unwrap()).unwrap();
    let ham_core::command::Command::StartDirect(args) =
        ham_core::command::parse(&argv).unwrap().command
    else {
        panic!()
    };
    let req = ham_core::request::direct_run_request(
        &args,
        Default::default(),
        &ham_core::models::ModelPrefs::default(),
    )
    .unwrap();
    assert_eq!(
        first["idempotencyKey"],
        ham_core::request::direct_idempotency_key(&req, "e2e-nonce-1").as_str()
    );
    // The same launch again: the same key, so the same run and no second execution.
    let o = ham_env(&h, &argv, &[("HAMH_IDEMPOTENCY_NONCE", "e2e-nonce-1")]).await;
    let again: Value = serde_json::from_str(text(&o.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(again["runId"], first["runId"]);
    assert_eq!(fake.mem.inner.lock().unwrap().started.len(), 1);

    for bad in ["has space", &"n".repeat(65), ""] {
        let o = ham_env(&h, &argv, &[("HAMH_IDEMPOTENCY_NONCE", bad)]).await;
        if bad.is_empty() {
            assert_eq!(o.status.code(), Some(0), "an empty value is unset");
            continue;
        }
        assert_eq!(o.status.code(), Some(1), "{bad:?}");
        assert!(
            text(&o.stderr).contains("HAMH_IDEMPOTENCY_NONCE"),
            "{}",
            text(&o.stderr)
        );
    }
}

fn allow_item(t: &str, retired_record: Option<&str>) -> harness_protocol::store::RepoAllowItem {
    // An item stored by an older backend may still carry the retired member; the contract has no field for it, so it is read from the stored JSON.
    let mut stored =
        serde_json::json!({"target": t, "addedBy": support::ME, "addedAt": "2026-09-27T03:00:00Z"});
    if let Some(record) = retired_record {
        stored["audrecRecordId"] = serde_json::json!(record);
    }
    serde_json::from_value(stored).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_prints_one_line_per_check_and_lists_the_allowlist() {
    // As corrected by C15: identity and account, the API, the catalog
    // freshness, the git-host modes, the allowlist, then every allowlist entry. On the direct stage the catalog is the five
    // models of `engine/catalog/models.json` and their smoke records.
    let fake = direct_fake(false).await;
    {
        let mut g = fake.mem.inner.lock().unwrap();
        g.allow.insert(
            "cc:hamh-e2e-foo".into(),
            allow_item("cc:hamh-e2e-foo", None),
        );
        // an entry stored before the review record was retired: the list does not show the record
        g.allow.insert(
            "gh:amzn/app".into(),
            allow_item("gh:amzn/app", Some("RECORD-1")),
        );
    }
    let h = home(&fake, "doctor");
    let o = ham(&h, &["doctor"]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 8, "{out}");
    assert_eq!(
        lines[0],
        format!("PASS identity {} account 800000000022", support::ME)
    );
    assert_eq!(lines[1], format!("PASS api {} answered", fake.endpoint));
    // The versions of the two ends: the stand-in answers with this build's version, as the backend does.
    assert_eq!(
        lines[2],
        format!(
            "PASS versions ham v{v} | backend v{v}",
            v = ham::version::CLIENT
        )
    );
    // The age of the newest smoke record is shown and is never a warning without `--catalog-max-age`, however old the compiled-in list is.
    assert!(
        lines[3].starts_with("PASS catalog 5 selectable rows, newest smoke "),
        "{out}"
    );
    assert_eq!(
        &lines[4..],
        [
            "PASS git-host cc: iam (repo-gateway role credentials; no git secret) | gh: not supported | gl: not supported",
            "PASS allowlist 2 entries",
            "  cc:hamh-e2e-foo added by arn:aws:sts::800000000022:assumed-role/Admin/testuser at 2026-09-27T03:00:00Z",
            "  gh:amzn/app added by arn:aws:sts::800000000022:assumed-role/Admin/testuser at 2026-09-27T03:00:00Z",
        ]
    );
    let o = ham(&h, &["--json", "doctor"]).await;
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["checks"].as_array().unwrap().len(), 6);
    assert_eq!(v["checks"][2]["name"], "versions");
    assert_eq!(v["checks"][5]["name"], "allowlist");
    assert!(
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["pass"] == true)
    );
    assert!(
        v["allowlist"][1].get("audrecRecordId").is_none(),
        "{}",
        v["allowlist"][1]
    );
    assert_no_secrets(&o);
    // An allowlist that cannot be read is a FAIL line and exit 1.
    fake.mem.inner.lock().unwrap().allow_down = true;
    let o = ham(&h, &["doctor"]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("FAIL allowlist HTTP 503")),
        "{out}"
    );
}

/// The `catalog` line of a `ham doctor` run and the age it states (the number before ` h old)`).
fn catalog_line(out: &str) -> (String, f64) {
    let line = out
        .lines()
        .find(|l| l.contains(" catalog "))
        .unwrap_or_else(|| panic!("no catalog line in {out}"))
        .to_string();
    let age = line
        .split(" h old)")
        .next()
        .and_then(|head| head.rsplit('(').next())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("no age in {line}"));
    (line, age)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_warns_about_the_catalog_age_only_when_given_a_limit() {
    // First slice: the compiled-in model list is as old as the build, so its age is never a warning unless the caller names a limit.
    let fake = direct_fake(false).await;
    let h = home(&fake, "doctor-age");

    // A limit nobody can exceed is a PASS, in text and in JSON.
    let o = ham(&h, &["doctor", "--catalog-max-age", "1000000"]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
    assert!(catalog_line(&out).0.starts_with("PASS catalog "), "{out}");
    let o = ham(&h, &["--json", "doctor", "--catalog-max-age", "1000000"]).await;
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["checks"][3]["name"], "catalog");
    assert_eq!(v["checks"][3]["level"], "pass");

    // A limit of one hour: the line's own age decides, so the test holds on a catalog of any age. (Today the compiled-in list is days old,
    // which is the case the WARN branch covers; a list refreshed within the hour takes the PASS branch.) Near the limit nothing is asserted,
    // because the age is printed to a tenth of an hour.
    let o = ham(&h, &["doctor", "--catalog-max-age", "1"]).await;
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "a warning never fails: {out}");
    let (line, age) = catalog_line(&out);
    if age >= 1.2 {
        assert!(
            line.starts_with("WARN catalog ") && line.contains(" hours old (refresh them)"),
            "{line}"
        );
    } else if age <= 0.8 {
        assert!(line.starts_with("PASS catalog "), "{line}");
    }
    let o = ham(&h, &["--json", "doctor", "--catalog-max-age", "1"]).await;
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(o.status.code(), Some(0));
    if age >= 1.2 {
        assert_eq!(v["checks"][3]["level"], "warn");
        assert_eq!(v["checks"][3]["pass"], true);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_refuses_a_bad_catalog_age_limit_before_any_check_runs() {
    let fake = direct_fake(false).await;
    let h = home(&fake, "doctor-age-bad");
    for bad in ["0", "abc", "-3", "1.5", "", "24h"] {
        let o = ham(&h, &["doctor", "--catalog-max-age", bad]).await;
        assert_eq!(o.status.code(), Some(2), "{bad:?}: {}", text(&o.stderr));
        assert_eq!(text(&o.stdout), "", "{bad:?}: nothing is checked");
        let err = text(&o.stderr);
        assert!(
            err.contains("--catalog-max-age takes whole hours, 1 or more"),
            "{bad:?}: {err}"
        );
    }
    let o = ham(&h, &["doctor", "--catalog-max-age"]).await;
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).contains("--catalog-max-age needs a value"),
        "{}",
        text(&o.stderr)
    );
}
