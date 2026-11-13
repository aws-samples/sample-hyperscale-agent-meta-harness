//! `ham-admin repos allow|deny|list` end to end against the local `/v1` API over the real
//! control-api handler: an operator changes the
//! repository allowlist; a refusal prints repo-gateway's exact reason and exits 1; usage errors
//! exit 2; nothing secret reaches either stream.
#![allow(clippy::unwrap_used)]

#[path = "../../ham/tests/support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::process::Output;
use support::{AKID, Fake, ME, SECRET, TOKEN};

/// A config naming the fake and a `credential_process` profile.
fn home(fake: &Fake, label: &str) -> PathBuf {
    let root = support::scratch(label);
    std::fs::create_dir_all(root.join("config/ham")).unwrap();
    std::fs::write(
        root.join("config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint,
            support::REGION
        ),
    )
    .unwrap();
    let script = root.join("creds.sh");
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
        root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            support::REGION
        ),
    )
    .unwrap();
    std::fs::write(root.join("aws-credentials"), "").unwrap();
    root
}

async fn admin(root: &PathBuf, args: &[&str]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham-admin"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("AWS_CONFIG_FILE", root.join("aws-config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", root.join("aws-credentials"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn assert_no_secrets(o: &Output) {
    for stream in [text(&o.stdout), text(&o.stderr)] {
        for secret in [SECRET, TOKEN, "AWS4-HMAC-SHA256", ", Signature="] {
            assert!(!stream.contains(secret), "{secret:?} leaked:\n{stream}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_deny_and_list_change_the_allowlist_and_refusals_print_the_exact_reason() {
    let fake = Fake::start().await;
    let h = home(&fake, "admin");
    let o = admin(&h, &["repos", "allow", "cc:hamh-e2e-foo"]).await;
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    let out = text(&o.stdout);
    assert!(
        out.starts_with(&format!("allowed cc:hamh-e2e-foo (added by {ME} at ")),
        "{out}"
    );
    assert_no_secrets(&o);

    // A name outside the prefix is refused with the exact reason of the API.
    let o = admin(&h, &["repos", "allow", "cc:other-repo"]).await;
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        text(&o.stderr).trim_end(),
        "ham-admin: refused: cc:other-repo is outside the hamh-e2e- prefix"
    );
    // The operator allows any other repository without a review record: no organization is singled out.
    let o = admin(&h, &["repos", "allow", "gh:amzn/app"]).await;
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}{}",
        text(&o.stdout),
        text(&o.stderr)
    );

    let o = admin(&h, &["repos", "list"]).await;
    assert_eq!(o.status.code(), Some(0));
    let lines: Vec<String> = text(&o.stdout).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with(&format!("cc:hamh-e2e-foo added by {ME} at ")),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with(&format!("gh:amzn/app added by {ME} at ")),
        "{lines:?}"
    );

    let o = admin(&h, &["repos", "deny", "cc:hamh-e2e-foo"]).await;
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(text(&o.stdout).trim_end(), "denied cc:hamh-e2e-foo");
    let left: Vec<String> = fake
        .mem
        .inner
        .lock()
        .unwrap()
        .allow
        .keys()
        .cloned()
        .collect();
    assert_eq!(left, ["gh:amzn/app"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_exit_2_before_any_call() {
    let fake = Fake::start().await;
    let h = home(&fake, "admin-usage");
    for args in [
        vec![],
        vec!["repos"],
        vec!["repos", "allow"],
        vec!["repos", "allow", "svn:x"],
        vec!["repos", "allow", "cc:hamh-e2e-x", "--reason"],
        vec!["repos", "allow", "cc:hamh-e2e-x", "--audrec", "A-1"],
        vec!["repos", "deny", "cc:hamh-e2e-x", "--audrec", "A-1"],
        vec!["repos", "merge", "cc:hamh-e2e-x"],
        vec!["policy", "write"],
    ] {
        let o = admin(&h, &args).await;
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr)
                .contains("usage: ham-admin repos allow <target> | deny <target> | list"),
            "{args:?}: {}",
            text(&o.stderr)
        );
    }
    assert!(
        fake.mem.seen().iter().all(|s| !s.path.contains("/admin/")),
        "no call was made"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_and_help_answer_without_configuration_or_an_api_call() {
    // An operator checks which build they hold before any configuration exists, so neither flag reads
    // the `ham` config file, signs a request or calls `/v1`.
    let fake = Fake::start().await;
    let configured = home(&fake, "admin-version");
    let bare = support::scratch("admin-version-bare");
    let version = format!("ham-admin {}", env!("CARGO_PKG_VERSION"));
    let usage = "usage: ham-admin repos allow <target> | deny <target> | list";
    for root in [&configured, &bare] {
        for flag in ["--version", "-V"] {
            let o = admin(root, &[flag]).await;
            assert_eq!(o.status.code(), Some(0), "{flag}: {}", text(&o.stderr));
            assert_eq!(text(&o.stdout).trim_end(), version, "{flag}");
            assert_eq!(text(&o.stderr), "", "{flag}");
        }
        for flag in ["--help", "-h"] {
            let o = admin(root, &[flag]).await;
            assert_eq!(o.status.code(), Some(0), "{flag}: {}", text(&o.stderr));
            assert_eq!(text(&o.stdout).trim_end(), usage, "{flag}");
            assert_eq!(text(&o.stderr), "", "{flag}");
        }
    }
    // Only the bare flag: anything after it is still a usage error (exit 2, the usage line on stderr).
    for args in [vec!["--version", "x"], vec!["--help", "repos", "list"]] {
        let o = admin(&configured, &args).await;
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains(usage),
            "{args:?}: {}",
            text(&o.stderr)
        );
    }
    assert!(fake.mem.seen().is_empty(), "no call was made");
}

// COMPAT-CAH: the operator tool reads the config folder through the same rule as `ham` (the folder of the earlier command name is read when it is
// the only one that holds files) and prints the same notice, once, on standard error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_tool_reads_the_earlier_folder_with_the_same_notice() {
    let fake = Fake::start().await;
    let h = home(&fake, "admin-earlier");
    std::fs::rename(h.join("config/ham"), h.join("config/cah")).unwrap(); // COMPAT-CAH
    let o = admin(&h, &["repos", "list"]).await;
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}{}",
        text(&o.stdout),
        text(&o.stderr)
    );
    let err = text(&o.stderr);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.starts_with("Your settings are read from ") && err.contains("ham config migrate"),
        "{err}"
    );
    assert!(!h.join("config/ham").exists());
}
