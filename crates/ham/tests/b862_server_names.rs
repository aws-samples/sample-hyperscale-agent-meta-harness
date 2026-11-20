//! `ham models --server` says when the stage holds another short name for a model than this client.
//!
//! The model list is compiled into each binary, and a short name is resolved in the client only. When a release renames a row, a client
//! and a stage of different releases hold different short names for the same model id: the stage's table shows a name that `--model` in
//! this client does not take, or takes for another model. `ham models --server` prints the stage's rows as it always did and, when the
//! stage's name for an id differs from this client's own name for it, one note line under the table that names the ids and both names. A
//! full id means the same model on both sides, and the note says so.
//!
//! The stage of another release here is the local `/v1` API with one switch: it serves a list this test makes from this build's own list.
#![allow(clippy::unwrap_used)]

mod support;

use ham::config_cmd::name_drift;
use ham_core::models::{ModelList, ModelRow, builtin, builtin_catalog};
use harness_protocol::common::Engine;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use support::{AKID, Fake, REGION, SECRET, TOKEN};

/// A model both sides list; the test changes the stage's name for it.
const MODEL: &str = "us.openai.gpt-6.1-sol";
/// A short name with a dot, as the names of a later release have: the client takes the stage's rows as they are.
const STAGE_NAME: &str = "other-1.0";
const NOTE_START: &str = "ham: note: the stage's short names differ from this ham's";

/// This client's own short name of [`MODEL`].
fn own_name() -> String {
    builtin()
        .models()
        .iter()
        .find(|m| m.id == MODEL)
        .and_then(|m| m.alias.clone())
        .expect("this client lists the model with a short name")
}

/// This build's list as a stage serves it, with `change` applied to it.
fn stage_list(change: impl FnOnce(&mut ModelList)) -> ModelList {
    let mut list = builtin_catalog().list();
    change(&mut list);
    list
}

/// The stage's list with the stage's own name for [`MODEL`], on every engine that lists it.
fn renamed() -> ModelList {
    stage_list(|list| {
        for row in list.models.iter_mut().filter(|r| r.id == MODEL) {
            row.alias = Some(STAGE_NAME.into());
        }
    })
}

async fn stage(list: Option<&ModelList>) -> Arc<Fake> {
    let fake = Fake::start_with(true).await;
    *fake.faults.models_answer.lock().unwrap() = list.map(|l| serde_json::to_string(l).unwrap());
    fake
}

/// A temp HOME with `~/.config/ham/config.toml` naming the fake, and a `credential_process` profile that prints the test stand-ins.
fn home(fake: &Fake, label: &str) -> PathBuf {
    let root = support::scratch(label);
    std::fs::create_dir_all(root.join(".config/ham")).unwrap();
    std::fs::write(
        root.join(".config/ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint, REGION
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
            REGION
        ),
    )
    .unwrap();
    std::fs::write(root.join("aws-credentials"), "").unwrap();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    root
}

fn command(root: &PathBuf, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", root)
        .env("HOME", root)
        .env("HAMH_DOWNLOAD_DIR", root.join("downloads"))
        .env("AWS_CONFIG_FILE", root.join("aws-config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", root.join("aws-credentials"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null());
    cmd
}

async fn ham(root: &PathBuf, args: &[&str]) -> Output {
    command(root, args).output().await.unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn notes(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with(NOTE_START)).collect()
}

/// TRIGGER PRESENT: the client read the stage's list, and the stage's row of the model carries the stage's name.
fn assert_trigger(fake: &Fake, table: &str) {
    assert!(
        fake.mem
            .seen()
            .iter()
            .any(|s| s.method == "GET" && s.path == "/v1/direct/models"),
        "TRIGGER PRESENT: the client asked the stage for its list"
    );
    let row = table
        .lines()
        .find(|l| l.contains(MODEL))
        .unwrap_or_else(|| panic!("the stage's row of the model is not printed:\n{table}"));
    assert!(
        row.contains(STAGE_NAME),
        "TRIGGER PRESENT: the table shows the stage's own name, as it always did: {row}"
    );
}

// ---- the command ------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_models_server_says_which_short_names_the_stage_holds_differently() {
    let fake = stage(Some(&renamed())).await;
    let root = home(&fake, "b862-server-names");
    let o = ham(&root, &["models", "--server"]).await;
    let (table, said) = (out(&o), err(&o));
    assert_eq!(o.status.code(), Some(0), "{said}");
    assert_trigger(&fake, &table);
    let found = notes(&said);
    assert_eq!(found.len(), 1, "one note line:\n{said}");
    let note = found[0];
    let own = own_name();
    assert!(
        note.contains(&format!(
            "{MODEL} is {STAGE_NAME} on the stage and is {own} in this ham"
        )),
        "the id and both names: {note}"
    );
    assert_eq!(
        note.matches(MODEL).count(),
        1,
        "a model both engines list is named once: {note}"
    );
    assert!(
        note.contains("--model") && note.contains("ham models"),
        "it says whose names --model takes and where they are: {note}"
    );
    assert!(
        note.contains("full id"),
        "it names what means the same on both sides: {note}"
    );
    assert!(
        notes(&table).is_empty(),
        "the note is not in the table: {table}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_the_note_stands_under_the_table() {
    // Both streams go to one file, so the file holds the lines in the order the command wrote them.
    let fake = stage(Some(&renamed())).await;
    let root = home(&fake, "b862-server-names-order");
    let path = root.join("both.txt");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    let status = command(&root, &["models", "--server"])
        .stdout(Stdio::from(file.try_clone().unwrap()))
        .stderr(Stdio::from(file))
        .status()
        .await
        .unwrap();
    let both = std::fs::read_to_string(&path).unwrap();
    assert_eq!(status.code(), Some(0), "{both}");
    assert_trigger(&fake, &both);
    let lines: Vec<&str> = both.lines().collect();
    let note_at = lines
        .iter()
        .position(|l| l.starts_with(NOTE_START))
        .unwrap_or_else(|| panic!("no note:\n{both}"));
    // The rows of the table are the lines that hold a model id and are not the note, which names an id too.
    let last_row = lines
        .iter()
        .rposition(|l| {
            !l.starts_with(NOTE_START) && builtin().models().iter().any(|m| l.contains(&m.id))
        })
        .unwrap();
    assert!(
        note_at > last_row,
        "the note comes after the last row of the table (row {last_row}, note {note_at}):\n{both}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_with_json_the_note_is_on_standard_error_and_standard_output_is_the_list() {
    let fake = stage(Some(&renamed())).await;
    let root = home(&fake, "b862-server-names-json");
    let o = ham(&root, &["models", "--server", "--json"]).await;
    let said = err(&o);
    assert_eq!(o.status.code(), Some(0), "{said}");
    let text = out(&o);
    assert_eq!(text.trim().lines().count(), 1, "one JSON document: {text}");
    let v: Value = serde_json::from_str(text.trim()).unwrap();
    let row = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == MODEL)
        .unwrap();
    assert_eq!(
        row["alias"], STAGE_NAME,
        "TRIGGER PRESENT: the JSON carries the stage's own name"
    );
    assert_eq!(notes(&said).len(), 1, "{said}");
    assert!(!text.contains("short names differ"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_a_stage_with_this_clients_names_gets_no_note() {
    // The stage as it is: this build's own list.
    let fake = stage(None).await;
    let root = home(&fake, "b862-server-names-same");
    let o = ham(&root, &["models", "--server"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(out(&o).contains(MODEL), "{}", out(&o));
    assert!(notes(&err(&o)).is_empty(), "{}", err(&o));
    // The same list served through the switch: still none.
    let fake = stage(Some(&stage_list(|_| {}))).await;
    let root = home(&fake, "b862-server-names-same-served");
    let o = ham(&root, &["models", "--server"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(notes(&err(&o)).is_empty(), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b862_a_row_of_a_newer_stage_that_this_client_does_not_list_is_printed_and_not_compared() {
    // A stage of a later release lists a model this client does not know, under a name with a dot. The client prints the row and has no
    // name of its own to compare it with.
    let newer = stage_list(|list| {
        let mut row: ModelRow = list.models.iter().find(|r| r.id == MODEL).cloned().unwrap();
        row.id = "us.openai.gpt-9-zeta".into();
        row.name = "GPT-9 Zeta".into();
        row.alias = Some("zeta-9.1".into());
        row.default = false;
        row.engine = None;
        list.models.push(row);
    });
    let fake = stage(Some(&newer)).await;
    let root = home(&fake, "b862-server-names-newer");
    let o = ham(&root, &["models", "--server"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let table = out(&o);
    let row = table
        .lines()
        .find(|l| l.contains("us.openai.gpt-9-zeta"))
        .unwrap_or_else(|| panic!("the row of the newer stage is not printed:\n{table}"));
    assert!(
        row.contains("zeta-9.1") && row.contains("GPT-9 Zeta"),
        "{row}"
    );
    assert!(notes(&err(&o)).is_empty(), "{}", err(&o));
}

// ---- the rule ---------------------------------------------------------------------------------------------------------------------

#[test]
fn b862_the_note_names_each_model_once_with_both_names() {
    assert_eq!(name_drift(&builtin_catalog().list()), None);
    let note = name_drift(&renamed()).expect("a note");
    let own = own_name();
    assert!(
        note.starts_with("note: the stage's short names differ from this ham's for 1 model: "),
        "{note}"
    );
    assert!(
        note.contains(&format!(
            "{MODEL} is {STAGE_NAME} on the stage and is {own} in this ham"
        )),
        "{note}"
    );
    assert_eq!(note.lines().count(), 1, "one line: {note}");
    // Two models: both are named, in the stage's order, and the count says two.
    let other = builtin()
        .models()
        .iter()
        .find(|m| m.id != MODEL && m.alias.is_some())
        .unwrap();
    let two = stage_list(|list| {
        for row in &mut list.models {
            if row.id == MODEL {
                row.alias = Some(STAGE_NAME.into());
            } else if row.id == other.id {
                row.alias = None;
            }
        }
    });
    let note = name_drift(&two).expect("a note");
    assert!(note.contains("for 2 models: "), "{note}");
    assert!(
        note.contains(&format!(
            "{} has no short name on the stage and is {} in this ham",
            other.id,
            other.alias.as_deref().unwrap()
        )),
        "{note}"
    );
}

#[test]
fn b862_only_a_model_both_sides_list_for_the_same_engine_is_compared() {
    // A row this client does not list, and a row of the second engine for a model this client lists on the first engine only.
    let cline_only = builtin()
        .models()
        .iter()
        .find(|m| m.selectable() && builtin_catalog().check(Engine::OPENCODE, &m.id).is_err())
        .expect("a model this client offers on the default engine only");
    let list = stage_list(|list| {
        let mut unknown = list.models[0].clone();
        unknown.id = "us.openai.gpt-9-zeta".into();
        unknown.alias = Some("zeta".into());
        unknown.default = false;
        let mut other_engine = list
            .models
            .iter()
            .find(|r| r.id == cline_only.id)
            .cloned()
            .unwrap();
        other_engine.engine = Some(Engine::OPENCODE);
        other_engine.alias = Some("another".into());
        other_engine.default = false;
        list.models.push(unknown);
        list.models.push(other_engine);
    });
    assert_eq!(name_drift(&list), None);
}

#[test]
fn b862_the_stages_text_is_made_safe_for_the_terminal() {
    // The stage's name is text of another machine: an escape sequence in it never reaches the terminal.
    let list = stage_list(|list| {
        for row in list.models.iter_mut().filter(|r| r.id == MODEL) {
            row.alias = Some("so\u{1b}[31ml\u{7}".into());
        }
    });
    let note = name_drift(&list).expect("a note");
    assert!(
        !note.contains('\u{1b}') && !note.contains('\u{7}'),
        "{note:?}"
    );
    assert!(note.contains(MODEL), "{note}");
}

/// This release against a stage of the release before it: that stage lists the six earlier models and still calls GPT-6.1 Sol `sol`
/// and GPT-6 Astra `astra`, the names that mean the two newer models in this ham. The note names both models with both names. The two
/// models the stage does not list are not compared: a start on one of them gets the sentence for an older backend.
#[test]
fn b862_a_stage_of_the_release_before_gets_the_note_for_the_two_names_that_moved() {
    let mut list = builtin_catalog().list();
    list.models.retain(|m| !m.id.contains("gpt-6.2"));
    for m in &mut list.models {
        match m.id.as_str() {
            "us.openai.gpt-6.1-sol" => m.alias = Some("sol".into()),
            "us.openai.gpt-6-astra" => m.alias = Some("astra".into()),
            _ => {}
        }
    }
    let note = name_drift(&list).expect("a note");
    assert!(note.contains("for 2 models"), "{note}");
    assert!(
        note.contains("us.openai.gpt-6.1-sol is sol on the stage and is sol-6.1 in this ham"),
        "{note}"
    );
    assert!(
        note.contains("us.openai.gpt-6-astra is astra on the stage and is astra-6 in this ham"),
        "{note}"
    );
    assert!(!note.contains("gpt-6.2"), "{note}");
    // This ham reads `sol` and `astra` as the newer models whatever a stage calls the earlier ones.
    let own = ham_core::models::builtin();
    assert_eq!(own.find("sol").unwrap().id, "us.openai.gpt-6.2-sol");
    assert_eq!(own.find("astra").unwrap().id, "us.openai.gpt-6.2-astra");
}
