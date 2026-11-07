//! `ham "<prompt>" --direct --model <id>`: one signed `POST /v1/direct/runs` whose body is
//! the prompt unchanged, one model, the rules from `~/.config/ham/rules/` (or `--rules`) and the nudge from
//! `~/.config/ham/continue.md`. The real binary against the local `/v1` API, which runs the real control-api handler as the
//! `direct` stage. Edits to a rules file change the next run's sha256.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham_core::rules::{DEFAULT_CONTINUE, DEFAULT_RULES};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{AKID, Fake, SECRET, TOKEN};

const MODEL: &str = "global.anthropic.claude-sonnet-5-5";
const PROMPT: &str = "  Add a percent_change(a, b) function with tests\n\nKeep it small.  ";

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_start_sends_the_prompt_unchanged_the_model_the_rules_and_the_nudge() {
    let fake = direct_fake().await;
    let h = home(&fake, "direct-start");
    // `--pack none`: the pointer-only default, one rule; the lifecycle contract that the plain command adds has its own tests (`tests/pack.rs`).
    assert_eq!(
        ham(&h, &["rules", "init", "--pack", "none"], &[])
            .await
            .status
            .code(),
        Some(0)
    );
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let first: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
    assert!(first["runId"].is_string() && first["idempotencyKey"].is_string());

    let posts = fake.mem.posts("/v1/direct/runs");
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].headers["idempotency-key"],
        first["idempotencyKey"].as_str().unwrap()
    );
    assert!(posts[0].signature.is_ok(), "SigV4 like every /v1 call");
    let b = body_of(&fake, 0);
    assert_eq!(b["prompt"], PROMPT, "the prompt travels unchanged");
    assert_eq!(b["model"], MODEL);
    assert_eq!(b["target"]["mode"], "s3-only");
    assert_eq!(b["rules"].as_array().unwrap().len(), 1);
    assert_eq!(b["rules"][0]["path"], "00-my-rules.md");
    assert_eq!(b["rules"][0]["content"], DEFAULT_RULES);
    assert_eq!(b["rules"][0]["sha256"], sha(DEFAULT_RULES.as_bytes()));
    assert_eq!(
        b["nudge"]["maxNudges"], 0,
        "the nudge is off unless the user turns it on"
    );
    assert!(
        b["nudge"]["text"]
            .as_str()
            .unwrap()
            .starts_with("Re-read my request.")
    );
    assert_eq!(
        b["nudge"]["sha256"],
        sha(b["nudge"]["text"].as_str().unwrap().as_bytes())
    );
    for gone in ["models", "width", "modes", "plan", "budget"] {
        assert!(b.get(gone).is_none(), "{gone}");
    }
    // The server stored the same request and the run finished through the fake harness.
    let last: Value = serde_json::from_str(out(&o).lines().last().unwrap()).unwrap();
    assert_eq!(last["run"]["status"], "complete");
    let _ = DEFAULT_CONTINUE;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_to_the_rules_changes_the_next_runs_sha256_and_not_the_first() {
    let fake = direct_fake().await;
    let h = home(&fake, "direct-edit");
    ham(&h, &["rules", "init"], &[]).await;
    assert_eq!(ham(&h, &start_args(&[]), &[]).await.status.code(), Some(0));
    let editor = h.script(
        "ed.sh",
        r#"printf '\n- Never mock the database.\n' >> "$1""#,
    );
    let o = ham(&h, &["rules", "edit"], &[("EDITOR", &editor)]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(ham(&h, &start_args(&[]), &[]).await.status.code(), Some(0));
    let (a, b) = (body_of(&fake, 0), body_of(&fake, 1));
    assert_eq!(a["rules"][0]["sha256"], sha(DEFAULT_RULES.as_bytes()));
    assert_ne!(a["rules"][0]["sha256"], b["rules"][0]["sha256"]);
    assert!(
        b["rules"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("- Never mock the database.\n")
    );
    assert_eq!(
        b["rules"][0]["sha256"],
        sha(b["rules"][0]["content"].as_str().unwrap().as_bytes())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_rules_the_run_does_not_start_unless_the_user_says_no_rules() {
    let fake = direct_fake().await;
    let h = home(&fake, "direct-norules");
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("ham rules init") && err(&o).contains("--no-rules"),
        "{}",
        err(&o)
    );
    assert!(
        fake.mem.posts("/v1/direct/runs").is_empty(),
        "nothing was sent"
    );
    let o = ham(&h, &start_args(&["--no-rules"]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let b = body_of(&fake, 0);
    assert_eq!(b["rules"].as_array().unwrap().len(), 0);
    assert!(
        b.get("nudge").is_none(),
        "no continue.md, so no nudge is sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_and_continue_flags_override_the_default_locations() {
    let fake = direct_fake().await;
    let h = home(&fake, "direct-override");
    ham(&h, &["rules", "init"], &[]).await;
    let dir = h.root.join("proj-rules");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("proj.md"), "Project rule.\n").unwrap();
    let cont = h.root.join("c.md");
    std::fs::write(&cont, "---\nmax_nudges: 9\n---\nOnward.\n").unwrap();
    let o = ham(
        &h,
        &start_args(&[
            "--rules",
            dir.to_str().unwrap(),
            "--continue",
            cont.to_str().unwrap(),
        ]),
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let b = body_of(&fake, 0);
    assert_eq!(b["rules"].as_array().unwrap().len(), 1);
    assert_eq!(b["rules"][0]["path"], "proj.md");
    assert_eq!(b["nudge"]["maxNudges"], 9);
    assert_eq!(b["nudge"]["text"], "Onward.\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_rules_file_stops_the_run_before_anything_is_sent() {
    let fake = direct_fake().await;
    let h = home(&fake, "direct-badrules");
    ham(&h, &["rules", "init"], &[]).await;
    std::fs::write(h.rules_dir().join("AGENTS.md"), "x\n").unwrap();
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("AGENTS.md"), "{}", err(&o));
    assert!(fake.mem.posts("/v1/direct/runs").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_the_list_does_not_name_is_refused_by_ham_before_anything_is_sent() {
    // `ham` resolves --model against the same list the server holds (aliases included), so a model
    // that is not one of the supported ones never reaches the wire. The server's own 422 is the next test.
    let fake = direct_fake().await;
    let h = home(&fake, "direct-refused");
    ham(&h, &["rules", "init"], &[]).await;
    let o = ham(
        &h,
        &[
            PROMPT,
            "--s3-only",
            "--direct",
            "--model",
            "zai.glm-5",
            "--json",
        ],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(1));
    let e = err(&o);
    assert!(
        e.contains("unknown model") && e.contains("--model") && e.contains("zai.glm-5"),
        "{e}"
    );
    assert!(
        e.contains("us.anthropic.claude-opus-5-5") && e.contains("opus"),
        "lists the five: {e}"
    );
    assert!(
        fake.mem.posts("/v1/direct/runs").is_empty(),
        "nothing was sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_servers_refusal_of_a_model_is_shown_verbatim_and_not_retried() {
    // A client whose list is newer than the server's can still meet a 422: the API call shows it as the server worded it.
    let fake = direct_fake().await;
    let api = Api::new(std::sync::Arc::new(
        HttpsPlane::new(
            &fake.endpoint,
            Signer::new(support::REGION, support::creds()),
        )
        .unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: std::time::Duration::from_millis(10),
    });
    let request = harness_protocol::api::v1::StartDirectRunRequest {
        input_wait_s: None,
        research: None,
        report_tests: None,
        report_step_s: None,
        goal: None,
        third_party: None,
        attachments: vec![],
        review: None,
        prompt: PROMPT.into(),
        target: harness_protocol::target::RunTarget::S3Only,
        base_ref: None,
        model: "zai.glm-5".into(),
        rules: vec![],
        nudge: None,
        skills: vec![],
        continue_from: None,
        engine_config: None,
        compute: None,
        build: None,
        max_run_s: None,
    };
    let e = api
        .start_direct_run(&request, "key-refused")
        .await
        .unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains("422") && text.contains("unknown-model"),
        "{text}"
    );
    assert!(
        text.contains("us.anthropic.claude-opus-5-5"),
        "lists the five: {text}"
    );
    assert_eq!(
        fake.mem.posts("/v1/direct/runs").len(),
        1,
        "a 422 is not retried"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_existing_but_empty_rules_directory_stops_the_run_and_sends_nothing() {
    let fake = direct_fake().await;
    let h = home(&fake, "direct-emptydir");
    ham(&h, &["rules", "init", "--pack", "none"], &[]).await;
    std::fs::remove_file(h.rules_dir().join("00-my-rules.md")).unwrap();
    assert!(
        h.rules_dir().is_dir(),
        "the directory stays, with no rule in it"
    );
    let o = ham(&h, &start_args(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(1), "{}\n{}", out(&o), err(&o));
    assert!(
        err(&o).contains("ham rules init") && err(&o).contains("--no-rules"),
        "{}",
        err(&o)
    );
    assert!(
        fake.mem.posts("/v1/direct/runs").is_empty(),
        "nothing was sent"
    );
}
