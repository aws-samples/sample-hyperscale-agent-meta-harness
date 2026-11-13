//! Which model a direct run uses, and how the user changes it.
//! The real `ham` binary against the local `/v1` API, which runs
//! the real control-api handler as the `direct` stage. Order, strongest first: `--model <id|alias>`, `HAMH_MODEL`, `default_model`
//! in `config.toml`, then Claude Sonnet 5.5. `ham config set|get|unset` and `ham models` need no network and no credentials.
#![allow(clippy::unwrap_used)]

mod support;

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use support::{AKID, Fake, SECRET, TOKEN};

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";
const ASTRA: &str = "us.openai.gpt-6-astra";

struct Home {
    root: PathBuf,
}

impl Home {
    fn config(&self) -> PathBuf {
        self.root.join(".config/ham/config.toml")
    }
    fn read_config(&self) -> String {
        std::fs::read_to_string(self.config()).unwrap_or_default()
    }
    fn write_config(&self, text: &str) {
        std::fs::create_dir_all(self.root.join(".config/ham")).unwrap();
        std::fs::write(self.config(), text).unwrap();
    }
}

/// A temp HOME with no config at all: the offline commands must work on a fresh machine.
fn bare_home(label: &str) -> Home {
    Home {
        root: support::scratch(label),
    }
}

/// A temp HOME whose `config.toml` names the fake and a `credential_process` profile, plus `extra` lines.
fn home(fake: &Fake, label: &str, extra: &str) -> Home {
    let h = bare_home(label);
    let root = &h.root;
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
    h.write_config(&format!(
        "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n{extra}",
        fake.endpoint,
        support::REGION
    ));
    h
}

async fn ham(h: &Home, args: &[&str], env: &[(&str, &str)]) -> Output {
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

fn posted_model(fake: &Fake, n: usize) -> String {
    let posts = fake.mem.posts("/v1/direct/runs");
    let b: Value = serde_json::from_slice(&posts[n].body).unwrap();
    b["model"].as_str().unwrap().to_string()
}

/// A direct start with the flags given after the usual words; `--json` keeps stdout to JSON lines.
fn start<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec![
        "Add a function",
        "--s3-only",
        "--direct",
        "--no-rules",
        "--json",
    ];
    a.extend(extra);
    a
}

// ---- ham config ----------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_set_default_model_stores_the_full_id_and_keeps_every_other_key() {
    let fake = Fake::start().await;
    let h = home(
        &fake,
        "cfg-set",
        "defaultRepo = \"cc:hamh-e2e-python-calc\"\n",
    );
    let o = ham(&h, &["config", "set", "default_model", "opus"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains(OPUS), "{}", out(&o));
    let text = h.read_config();
    assert!(
        text.contains(&format!("defaultModel = \"{OPUS}\"")),
        "{text}"
    );
    for kept in [
        "apiEndpoint",
        "region",
        "awsProfile",
        "defaultRepo = \"cc:hamh-e2e-python-calc\"",
    ] {
        assert!(text.contains(kept), "{kept} lost: {text}");
    }
    // The full id and the alias spelling of a different case give the same file.
    let o = ham(&h, &["config", "set", "defaultModel", "SOL"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(
        h.read_config()
            .contains(&format!("defaultModel = \"{SOL}\""))
    );
    let dir: Vec<_> = std::fs::read_dir(h.root.join(".config/ham"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(dir, ["config.toml"], "the write leaves no temp file behind");
    assert!(fake.mem.seen().is_empty(), "config needs no network");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_works_on_a_fresh_machine_with_no_config_file_and_no_endpoint() {
    let h = bare_home("cfg-fresh");
    let o = ham(&h, &["config", "set", "default_model", "astra"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(h.read_config(), format!("defaultModel = \"{ASTRA}\"\n"));
    let o = ham(&h, &["config", "get", "default_model"], &[]).await;
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(out(&o).trim(), ASTRA);
    let o = ham(&h, &["config", "get", "default_model", "--json"], &[]).await;
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(
        (v["key"].as_str(), v["value"].as_str()),
        (Some("default_model"), Some(ASTRA))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_refuses_an_unknown_model_or_key_and_changes_nothing() {
    let h = bare_home("cfg-bad");
    h.write_config("region = \"us-east-1\"\n");
    let before = h.read_config();
    for bad in [
        "gpt-9",
        "zai.glm-5",
        "",
        "us.anthropic.claude-sonnet-5-5 extra",
    ] {
        let o = ham(&h, &["config", "set", "default_model", bad], &[]).await;
        assert_eq!(o.status.code(), Some(1), "{bad:?}");
        let e = err(&o);
        for want in ["default_model", "sonnet", "opus", "sol", "astra"] {
            assert!(e.contains(want), "{bad:?}: {e}");
        }
        assert_eq!(h.read_config(), before, "{bad:?}");
    }
    let o = ham(&h, &["config", "set", "default-model", "opus"], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("default_model") && err(&o).contains("api_endpoint"),
        "{}",
        err(&o)
    );
    // Usage errors are exit 2.
    let o = ham(&h, &["config", "set", "default_model"], &[]).await;
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert_eq!(h.read_config(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_get_unset_and_list() {
    let h = bare_home("cfg-list");
    h.write_config(&format!(
        "region = \"us-east-1\"\ndefaultModel = \"{OPUS}\"\n"
    ));
    let o = ham(&h, &["config"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = out(&o);
    for want in [
        format!("default_model = {OPUS}"),
        "region = us-east-1".to_string(),
        "api_endpoint = (not set)".to_string(),
    ] {
        assert!(text.contains(&want), "{want}: {text}");
    }
    let o = ham(&h, &["config", "unset", "default_model"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!h.read_config().contains("defaultModel"));
    assert!(h.read_config().contains("region"));
    // An unset key: nothing on stdout, exit 1 (like `git config`), and for the model the stderr says what applies.
    let o = ham(&h, &["config", "get", "default_model"], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(out(&o).trim(), "");
    assert!(err(&o).contains("Claude Sonnet 5.5"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_refuses_to_overwrite_a_file_it_cannot_read() {
    let h = bare_home("cfg-corrupt");
    // A file that is not TOML is never overwritten.
    h.write_config("this is not toml {\n");
    let o = ham(&h, &["config", "set", "default_model", "opus"], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("config.toml"), "{}", err(&o));
    assert_eq!(
        h.read_config(),
        "this is not toml {\n",
        "a file that is not TOML is left as it is"
    );
    // A file with a key this build does not know is read for the keys it knows and patched around: the change is
    // made on its own line, nothing the file holds is erased, and the key it cannot place is named on standard error.
    h.write_config("surprise = 1\n");
    let o = ham(&h, &["config", "set", "default_model", "opus"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(
        h.read_config(),
        format!("surprise = 1\ndefaultModel = \"{OPUS}\"\n")
    );
    assert!(
        err(&o).contains("line 1") && err(&o).contains("surprise"),
        "{}",
        err(&o)
    );
    // The commands that start work still refuse such a file and name the line.
    let o = ham(&h, &["models"], &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("line 1") && err(&o).contains("surprise"),
        "{}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_file_that_links_to_a_credential_file_reports_line_and_column_and_never_the_line()
{
    let h = bare_home("cfg-link-credentials");
    // A credential-shaped line, assembled at run time.
    let head = ["aws_secret_access", "_key"].concat();
    let secret = ["wJalrXUtn", "FEMI/K7MDENG", "/bPxRfiCY", "EXAMPLEKEY"].concat();
    let target = h.root.join("not-a-config");
    std::fs::write(&target, format!("[default]\n{head} = {secret}\n")).unwrap();
    std::fs::create_dir_all(h.root.join(".config/ham")).unwrap();
    std::os::unix::fs::symlink(&target, h.config()).unwrap();
    for args in [
        vec!["models"],
        vec!["config"],
        vec!["config", "get", "region"],
    ] {
        let o = ham(&h, &args, &[]).await;
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", err(&o));
        let e = err(&o);
        assert!(
            e.contains("line 2") && e.contains("column"),
            "{args:?}: {e}"
        );
        for part in [&secret[..], &secret[4..14], &head[..]] {
            assert!(
                !e.contains(part) && !out(&o).contains(part),
                "{args:?}: the credential line leaked: {e}"
            );
        }
    }
    assert!(
        std::fs::symlink_metadata(h.config())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is left as it is"
    );
}

// ---- ham models ----------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_lists_the_five_offline_with_the_default_marked() {
    let h = bare_home("models-plain");
    let o = ham(&h, &["models"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = out(&o);
    for id in [OPUS, SONNET, SOL, ASTRA, "us.zai.glm-5.3"] {
        assert!(text.contains(id), "{id}: {text}");
    }
    assert!(
        !text.contains("zai.glm-5 ") && !text.contains("zai.glm-5\n"),
        "{text}"
    );
    let sonnet = text.lines().find(|l| l.contains(SONNET)).unwrap();
    assert!(
        sonnet.contains("default") && sonnet.trim_start().starts_with('*'),
        "{text}"
    );
    assert!(sonnet.contains("built-in default"), "{text}");
    for other in [OPUS, SOL, ASTRA] {
        let l = text.lines().find(|l| l.contains(other)).unwrap();
        assert!(
            !l.trim_start().starts_with('*') && !l.contains("default"),
            "{l}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_marks_the_model_the_next_run_uses_by_the_same_order_as_a_start() {
    let h = bare_home("models-order");
    h.write_config(&format!("defaultModel = \"{SOL}\"\n"));
    let marked = |o: &Output| -> (String, String) {
        let text = out(o);
        let l = text
            .lines()
            .find(|l| l.trim_start().starts_with('*'))
            .unwrap_or_default()
            .to_string();
        (l.clone(), text)
    };
    let o = ham(&h, &["models"], &[]).await;
    let (l, _) = marked(&o);
    assert!(l.contains(SOL) && l.contains("config default_model"), "{l}");
    let o = ham(&h, &["models"], &[("HAMH_MODEL", "opus")]).await;
    let (l, _) = marked(&o);
    assert!(l.contains(OPUS) && l.contains("HAMH_MODEL"), "{l}");
    // An empty variable is unset.
    let o = ham(&h, &["models"], &[("HAMH_MODEL", "")]).await;
    assert!(marked(&o).0.contains(SOL));
    // A variable that names nothing is an error, and the list is still printed so the user can pick.
    let o = ham(&h, &["models"], &[("HAMH_MODEL", "gpt-9")]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("gpt-9") && err(&o).contains("HAMH_MODEL"),
        "{}",
        err(&o)
    );
    assert!(out(&o).contains(SONNET), "{}", out(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_json_carries_the_default_and_the_selection() {
    let h = bare_home("models-json");
    let o = ham(&h, &["models", "--json"], &[("HAMH_MODEL", "astra")]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["default"], SONNET);
    assert_eq!(v["selected"]["id"], ASTRA);
    assert_eq!(v["selected"]["source"], "HAMH_MODEL");
    let models = v["models"].as_array().unwrap();
    assert_eq!(models.len(), 6);
    let default_rows: Vec<&str> = models
        .iter()
        .filter(|m| m["default"] == true)
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(default_rows, [SONNET]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_server_reads_the_direct_stage_list() {
    let fake = direct_fake().await;
    let h = home(&fake, "models-server", "");
    let o = ham(&h, &["models", "--server"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let line = out(&o)
        .lines()
        .find(|l| l.contains(SONNET))
        .unwrap_or_default()
        .to_string();
    assert!(line.contains("default"), "{}", out(&o));
    assert!(
        fake.mem
            .seen()
            .iter()
            .any(|s| s.method == "GET" && s.path == "/v1/direct/models")
    );
    let o = ham(&h, &["models", "--server", "--json"], &[]).await;
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["default"], SONNET);
    // The multi-agent catalog view is gone.
    let o = ham(&h, &["models", "--catalog"], &[]).await;
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
}

// ---- ham "<prompt>" --direct ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_start_with_nothing_set_runs_claude_sonnet_5_5() {
    let fake = direct_fake().await;
    let h = home(&fake, "start-default", "");
    let o = ham(&h, &start(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    assert_eq!(posted_model(&fake, 0), SONNET);
    let first: Value = serde_json::from_str(out(&o).lines().next().unwrap()).unwrap();
    assert_eq!(
        first["model"], SONNET,
        "the JSON line says which model the run uses"
    );
    assert_eq!(first["modelSource"], "built-in default");
    // The plain form names it on stderr, so stdout keeps its first line.
    let o = ham(
        &h,
        &["Add a function", "--s3-only", "--direct", "--no-rules"],
        &[],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).starts_with("started run "), "{}", out(&o));
    assert!(
        err(&o).contains("Claude Sonnet 5.5") && err(&o).contains("built-in default"),
        "{}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flag_beats_the_variable_beats_the_config_file_beats_the_default() {
    let fake = direct_fake().await;
    let h = home(
        &fake,
        "start-order",
        &format!("defaultModel = \"{ASTRA}\"\n"),
    );
    type Case<'a> = (&'a [&'a str], &'a [(&'a str, &'a str)], &'a str);
    let cases: [Case; 6] = [
        (&["--model", "opus"], &[("HAMH_MODEL", "sol")], OPUS),
        (&["--model", SOL], &[], SOL),
        (&[], &[("HAMH_MODEL", "sol")], SOL),
        (&[], &[("HAMH_MODEL", "")], ASTRA),
        (&[], &[], ASTRA),
        (&["--model", "Sonnet"], &[], SONNET),
    ];
    for (i, (flags, env, want)) in cases.iter().enumerate() {
        let o = ham(&h, &start(flags), env).await;
        assert_eq!(o.status.code(), Some(0), "case {i}: {}", err(&o));
        assert_eq!(posted_model(&fake, i), *want, "case {i}: {flags:?} {env:?}");
    }
    // Without the config key the default applies again.
    h.write_config(
        &h.read_config()
            .replace(&format!("defaultModel = \"{ASTRA}\"\n"), ""),
    );
    let o = ham(&h, &start(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(posted_model(&fake, cases.len()), SONNET);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_choice_stops_the_start_before_anything_is_sent_and_names_its_source() {
    let fake = direct_fake().await;
    let h = home(&fake, "start-bad", "");
    for (flags, env, source) in [
        (vec!["--model", "gpt-9"], vec![], "--model"),
        (vec![], vec![("HAMH_MODEL", "gpt-9")], "HAMH_MODEL"),
        (vec!["--model", "zai.glm-5"], vec![], "--model"),
    ] {
        let o = ham(&h, &start(&flags), &env).await;
        assert_eq!(o.status.code(), Some(1), "{flags:?} {env:?}");
        let e = err(&o);
        assert!(e.contains(source), "{e}");
        for want in ["sonnet", "opus", "sol", "astra"] {
            assert!(e.contains(want), "{e}");
        }
    }
    h.write_config(&format!("{}defaultModel = \"gpt-9\"\n", h.read_config()));
    let o = ham(&h, &start(&[]), &[]).await;
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("config default_model"), "{}", err(&o));
    // A model set by a stronger layer is used and the wrong weaker value is not looked at.
    let o = ham(&h, &start(&["--model", "opus"]), &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(posted_model(&fake, 0), OPUS);
    assert_eq!(
        fake.mem.posts("/v1/direct/runs").len(),
        1,
        "no wrong choice reached the server"
    );
}

// ---- details a second reading asked for --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_value_says_how_to_fix_it_where_it_came_from() {
    let h = bare_home("fix-hints");
    let o = ham(&h, &["models"], &[("HAMH_MODEL", "gpt-9")]).await;
    assert!(err(&o).contains("unset HAMH_MODEL"), "{}", err(&o));
    h.write_config("defaultModel = \"gpt-9\"\n");
    let o = ham(&h, &["models"], &[]).await;
    let e = err(&o);
    assert!(
        e.contains("config default_model") && e.contains("ham config unset default_model"),
        "{e}"
    );
    // `config set` is the fix, and its own refusal does not blame a layer.
    let o = ham(&h, &["config", "set", "default_model", "nope"], &[]).await;
    assert!(!err(&o).contains("(from"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_set_follows_a_symlink_keeps_the_mode_and_leaves_no_temp_file() {
    let h = bare_home("cfg-link");
    let real = h.root.join("dotfiles/ham-config.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, "region = \"us-east-1\"\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::create_dir_all(h.root.join(".config/ham")).unwrap();
    std::os::unix::fs::symlink(&real, h.config()).unwrap();
    let o = ham(&h, &["config", "set", "default_model", "opus"], &[]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(
        std::fs::symlink_metadata(h.config())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link stays a link"
    );
    let text = std::fs::read_to_string(&real).unwrap();
    assert!(
        text.contains(&format!("defaultModel = \"{OPUS}\"")) && text.contains("region"),
        "{text}"
    );
    let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "the file keeps its permission bits");
    for dir in [h.root.join(".config/ham"), h.root.join("dotfiles")] {
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains(".tmp")),
            "{dir:?}: {names:?}"
        );
    }
}

#[test]
fn a_server_whose_default_differs_from_this_build_is_noted() {
    let mut list = ham_core::models::builtin().list();
    assert_eq!(ham::config_cmd::default_drift(&list), None);
    list.default = OPUS.into();
    let note = ham::config_cmd::default_drift(&list).unwrap();
    assert!(note.contains(OPUS) && note.contains(SONNET), "{note}");
}
