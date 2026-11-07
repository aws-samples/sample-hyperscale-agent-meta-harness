//! The review prompt and the setting on the client side. `ham rules init` writes `~/.config/ham/review.md` (the default
//! prompt) and never over an edit; `ham rules edit review` opens it and checks it; `ham rules show` prints what the next run sends; a run sends
//! it unread as the `review` member, `run.review_round = off` sends `{"mode":"off"}` alone, and a user with no file and no setting sends no
//! member. The real binary against the local `/v1` API, which runs the real control-api handler as the `direct` stage.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::rules::{REVIEW_NAME, default_review_file};
use harness_protocol::direct::DEFAULT_REVIEW_PROMPT;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{AKID, Fake, SECRET, TOKEN};

const MODEL: &str = "global.anthropic.claude-sonnet-5-5";
const PROMPT: &str = "Add a percent_change(a, b) function with tests";

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

struct Home {
    root: PathBuf,
}

impl Home {
    fn rules_dir(&self) -> PathBuf {
        self.root.join(".config/ham/rules")
    }
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.root.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

/// A temp HOME with `~/.config/ham/config.toml` naming the fake, and a `credential_process` profile.
fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    std::fs::create_dir_all(root.join(".config/ham")).unwrap();
    std::fs::write(
        root.join(".config/ham/config.toml"),
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
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    Home { root }
}

async fn ham(h: &Home, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", &h.root)
        .env("HOME", &h.root)
        .env("HAMH_DOWNLOAD_DIR", h.root.join("downloads"))
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

async fn direct_fake() -> std::sync::Arc<Fake> {
    Fake::start_with(true).await
}

fn start_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![PROMPT, "--s3-only", "--direct", "--model", MODEL, "--json"];
    a.extend(extra);
    a
}

fn body_of(fake: &Fake, n: usize) -> Value {
    let posts = fake.mem.posts("/v1/direct/runs");
    serde_json::from_slice(&posts[n].body).unwrap()
}

impl Home {
    fn review_md(&self) -> PathBuf {
        self.root.join(".config/ham/review.md")
    }
    fn config_toml(&self) -> PathBuf {
        self.root.join(".config/ham/config.toml")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_writes_the_review_file_once_and_never_over_an_edit() {
    let fake = direct_fake().await;
    let h = home(&fake, "review-init");
    let o = ham(&h, &["rules", "init", "--pack", "none"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.review_md()).unwrap(),
        default_review_file()
    );
    let t = out(&o);
    assert!(t.contains(&h.review_md().display().to_string()), "{t}");
    assert!(
        t.contains("ham rules edit review") && t.contains("run.review_round"),
        "says how to change it and how to switch it off: {t}"
    );
    assert!(
        t.contains(&format!("created {}", h.review_md().display())),
        "{t}"
    );
    // An edit stays; the second init says so.
    std::fs::write(h.review_md(), "---\nguard_minutes: 45\n---\nMy review.\n").unwrap();
    let o = ham(&h, &["rules", "init", "--pack", "none"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(
        std::fs::read_to_string(h.review_md()).unwrap(),
        "---\nguard_minutes: 45\n---\nMy review.\n"
    );
    assert!(
        out(&o).contains(&format!("kept {}", h.review_md().display())),
        "{}",
        out(&o)
    );
    // The plain command (the lifecycle pack) writes it too.
    let h2 = home(&fake, "review-init-pack");
    assert_eq!(
        ham(&h2, &["rules", "init"], &[]).await.status.code(),
        Some(0)
    );
    assert!(h2.review_md().is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_prints_the_review_file_and_json_and_says_when_there_is_none() {
    let fake = direct_fake().await;
    let h = home(&fake, "review-show");
    ham(&h, &["rules", "init", "--pack", "none"], &[]).await;
    let o = ham(&h, &["rules", "show"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let t = out(&o);
    assert!(
        t.contains(&format!("review: {}", h.review_md().display()))
            && t.contains("guard the stage's own")
            && t.contains(&sha(DEFAULT_REVIEW_PROMPT.as_bytes())),
        "{t}"
    );
    assert!(t.contains("----- review.md -----"), "{t}");
    let o = ham(&h, &["rules", "show", "--json"], &[]).await;
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["review"]["text"], DEFAULT_REVIEW_PROMPT);
    assert_eq!(v["review"]["sha256"], sha(DEFAULT_REVIEW_PROMPT.as_bytes()));
    assert!(v["review"]["guardS"].is_null());
    std::fs::write(h.review_md(), "---\nguard_minutes: 30\n---\nLook again.\n").unwrap();
    let o = ham(&h, &["rules", "show"], &[]).await;
    assert!(out(&o).contains("guard 30 min"), "{}", out(&o));
    let o = ham(&h, &["rules", "show", "--json"], &[]).await;
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["review"]["guardS"], 1800);
    // No file: nothing is sent, and `show` says the harness uses its own prompt.
    std::fs::remove_file(h.review_md()).unwrap();
    let o = ham(&h, &["rules", "show"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(
        out(&o).contains("review: none") && out(&o).contains("its own review prompt"),
        "{}",
        out(&o)
    );
    let o = ham(&h, &["rules", "show", "--json"], &[]).await;
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert!(v["review"].is_null());
    // A review file with a problem is the same error a run would stop with.
    std::fs::write(h.review_md(), "---\nguard_minutes: 0\n---\nX\n").unwrap();
    let o = ham(&h, &["rules", "show"], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("guard_minutes"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_review_opens_the_file_checks_it_and_names_the_fix() {
    let fake = direct_fake().await;
    let h = home(&fake, "review-edit");
    // A missing file starts from `init`.
    let editor = h.script("ed.sh", r#"printf 'Look again.\n' >> "$1""#);
    let o = ham(&h, &["rules", "edit", "review"], &[("EDITOR", &editor)]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = std::fs::read_to_string(h.review_md()).unwrap();
    assert!(
        text.starts_with("---\n# guard_minutes") && text.ends_with("Look again.\n"),
        "{text}"
    );
    assert!(out(&o).contains("saved"), "{}", out(&o));
    // The file name works too.
    let o = ham(&h, &["rules", "edit", REVIEW_NAME], &[("EDITOR", &editor)]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    // A change the checks refuse is reported with the way to fix it.
    let bad = h.script(
        "bad.sh",
        r#"printf -- '---\nguard_minutes: 9000\n---\nGo.\n' > "$1""#,
    );
    let o = ham(&h, &["rules", "edit", "review"], &[("EDITOR", &bad)]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("guard_minutes") && err(&o).contains("ham rules edit review"),
        "{}",
        err(&o)
    );
    // The rules directory is not touched by it.
    assert!(!h.rules_dir().join(REVIEW_NAME).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_sends_the_review_file_unread_and_a_user_with_none_sends_no_member() {
    let fake = direct_fake().await;
    let h = home(&fake, "review-run");
    // No file and no setting: the request has no `review` member.
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "none"], &[])
            .await
            .status
            .code(),
        Some(0)
    );
    std::fs::remove_file(h.review_md()).unwrap();
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    assert!(
        body_of(&fake, 0).get("review").is_none(),
        "{}",
        body_of(&fake, 0)
    );
    // The default file: the text as typed, its hash, no guard, the round on.
    std::fs::write(h.review_md(), default_review_file()).unwrap();
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    let r = body_of(&fake, 1)["review"].clone();
    assert_eq!(r["mode"], "auto");
    assert_eq!(r["text"], DEFAULT_REVIEW_PROMPT);
    assert_eq!(r["sha256"], sha(DEFAULT_REVIEW_PROMPT.as_bytes()));
    assert!(r.get("guardS").is_none());
    // An edited file with a guard.
    std::fs::write(
        h.review_md(),
        "---\nguard_minutes: 45\n---\nOnly {{facts}}.\n",
    )
    .unwrap();
    assert_eq!(ham(&h, &start_args(&[]), &[]).await.status.code(), Some(0));
    let r = body_of(&fake, 2)["review"].clone();
    assert_eq!(r["text"], "Only {{facts}}.\n");
    assert_eq!(r["guardS"], 2700);
    // The setting off: the switch alone, the file is not sent.
    let o = ham(&h, &["config", "set", "run.review_round", "off"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    assert!(
        std::fs::read_to_string(h.config_toml())
            .unwrap()
            .contains("review_round = \"off\"")
    );
    assert_eq!(ham(&h, &start_args(&[]), &[]).await.status.code(), Some(0));
    assert_eq!(
        body_of(&fake, 3)["review"],
        serde_json::json!({"mode": "off"})
    );
    // Back to auto: the file is sent again.
    assert_eq!(
        ham(&h, &["config", "set", "run.review_round", "auto"], &[])
            .await
            .status
            .code(),
        Some(0)
    );
    assert_eq!(ham(&h, &start_args(&[]), &[]).await.status.code(), Some(0));
    assert_eq!(body_of(&fake, 4)["review"]["mode"], "auto");
    assert_eq!(fake.mem.posts("/v1/direct/runs").len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_file_with_a_problem_stops_the_run_before_anything_is_sent() {
    let fake = direct_fake().await;
    let h = home(&fake, "review-bad");
    ham(&h, &["rules", "init", "--pack", "none"], &[]).await;
    std::fs::write(h.review_md(), "---\nguard_minutes: 0\n---\nGo.\n").unwrap();
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("review.md") && err(&o).contains("guard_minutes"),
        "{}",
        err(&o)
    );
    assert!(
        fake.mem.posts("/v1/direct/runs").is_empty(),
        "nothing was sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_setting_value_is_refused_where_the_other_run_keys_refuse_theirs() {
    let fake = direct_fake().await;
    let h = home(&fake, "review-badset");
    for bad in ["on", "always", "true", ""] {
        let o = ham(&h, &["config", "set", "run.review_round", bad], &[]).await;
        assert_ne!(o.status.code(), Some(0), "{bad:?}");
        assert!(err(&o).contains("auto or off"), "{bad:?}: {}", err(&o));
    }
    assert!(
        !std::fs::read_to_string(h.config_toml())
            .unwrap()
            .contains("review_round")
    );
}
