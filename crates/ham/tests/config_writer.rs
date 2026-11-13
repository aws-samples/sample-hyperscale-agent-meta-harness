//! The writer behind `ham config` as the code calls it: the file is read once, bounded; the new bytes are
//! written to a temp file in the same folder; and just before the rename the file is read again and its SHA-256 compared with the one loaded,
//! so an edit made by someone else in between stops the write. The callers that used to drop the user's comments (the composer's `d` key and
//! `/config set` in the message box) go through the same writer, and the box shows the one-way-door note before the first write that adds a
//! table. The doctor lists what is wrong with the file.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::Api;
use ham::cli::{ExecContext, run_captured};
use ham::config_cmd::{commit, load_file};
use ham::doctor::config_checks;
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::router::{RouteContext, RouteOutcome, route};
use std::path::PathBuf;
use std::sync::Arc;

const OPUS: &str = "us.anthropic.claude-opus-5-5";
const OWNER: &str = "# ham settings\n# keep this note\napiEndpoint = \"https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\"\nregion = \"us-east-1\"\nawsProfile = \"ham-test\" # the profile\n";

/// How the writer is told where the config folder is.
type EnvFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct Side {
    config: PathBuf,
}

impl Side {
    fn new(label: &str, content: Option<&str>) -> Side {
        let root = support::scratch(label);
        let config = root.join("config");
        std::fs::create_dir_all(config.join("ham")).unwrap();
        let s = Side { config };
        if let Some(c) = content {
            std::fs::write(s.file(), c).unwrap();
        }
        s
    }
    fn file(&self) -> PathBuf {
        self.config.join("ham/config.toml")
    }
    fn read(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap_or_default()
    }
    fn env(&self) -> EnvFn {
        let dir = self.config.display().to_string();
        Box::new(move |k: &str| (k == "XDG_CONFIG_HOME").then(|| dir.clone()))
    }
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.config.join("ham"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

// ---- the compare before the rename -----------------------------------------------------------------------------------------------

#[test]
fn a_file_that_changed_between_load_and_rename_stops_the_write() {
    let s = Side::new("writer-race", Some(OWNER));
    let env = s.env();
    let loaded = load_file(&env).unwrap();
    assert_eq!(loaded.text(), OWNER);
    // Someone else edits the file after it was loaded.
    let theirs = format!("{OWNER}# edited by another tool\n");
    std::fs::write(s.file(), &theirs).unwrap();
    let e = commit(&loaded, "region = \"eu-west-1\"\n").unwrap_err();
    assert!(
        e.contains("changed on disk; reloaded; nothing written"),
        "{e}"
    );
    assert_eq!(s.read(), theirs, "their edit is still there");
    assert_eq!(s.names(), ["config.toml"], "no temp file is left");
    // Loaded again, the same write goes through and is exactly the text given.
    let loaded = load_file(&env).unwrap();
    commit(&loaded, "region = \"eu-west-1\"\n").unwrap();
    assert_eq!(s.read(), "region = \"eu-west-1\"\n");
    assert_eq!(s.names(), ["config.toml"]);
}

#[test]
fn a_file_that_appeared_or_went_away_meanwhile_stops_the_write_too() {
    // Absent at load, created by someone before the rename.
    let s = Side::new("writer-appeared", None);
    let env = s.env();
    let loaded = load_file(&env).unwrap();
    assert!(!loaded.exists());
    std::fs::write(s.file(), "region = \"us-west-2\"\n").unwrap();
    let e = commit(&loaded, "defaultModel = \"x\"\n").unwrap_err();
    assert!(e.contains("changed on disk"), "{e}");
    assert_eq!(s.read(), "region = \"us-west-2\"\n");
    // Present at load, removed before the rename.
    let s = Side::new("writer-removed", Some(OWNER));
    let env = s.env();
    let loaded = load_file(&env).unwrap();
    std::fs::remove_file(s.file()).unwrap();
    let e = commit(&loaded, "region = \"x\"\n").unwrap_err();
    assert!(e.contains("changed on disk"), "{e}");
    assert!(!s.file().exists());
    // The same bytes written again by another tool are no change: the compare is on the content, not the time.
    let s = Side::new("writer-same", Some(OWNER));
    let env = s.env();
    let loaded = load_file(&env).unwrap();
    std::fs::write(s.file(), OWNER).unwrap();
    commit(&loaded, "region = \"y\"\n").unwrap();
    assert_eq!(s.read(), "region = \"y\"\n");
}

#[test]
fn the_load_reads_at_most_the_cap_and_refuses_what_is_not_a_regular_file() {
    let s = Side::new("writer-load", Some(&format!("# {}\n", "a".repeat(200_000))));
    let env = s.env();
    let loaded = load_file(&env).unwrap();
    // Only the cap and one more byte are held, however large the file is.
    assert_eq!(
        loaded.held_bytes(),
        ham_core::settings::MAX_CONFIG_BYTES + 1
    );
    assert!(loaded.read.has_errors());
    let shown = loaded.read.render_errors();
    assert!(
        shown.contains("200003") && shown.contains("65536"),
        "{shown}"
    );
    let s = Side::new("writer-dir", None);
    let env = s.env();
    std::fs::create_dir(s.config.join("ham/config.toml")).unwrap();
    let e = load_file(&env).unwrap_err();
    assert!(e.contains("not a regular file"), "{e}");
}

// ---- the callers that dropped comments ----------------------------------------------------------------------------------------------

struct NoNetwork;

impl ControlPlane for NoNetwork {
    async fn call(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("no network in this test".into()))
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Err(TransportError::Refused("no network in this test".into()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_composer_d_key_and_slash_config_set_keep_comments() {
    // The composer's `d` key: `Effect::SaveDefaultModel` runs this.
    let s = Side::new("writer-d", Some(OWNER));
    let env = s.env();
    let stored = ham::tui::save_default_model_with_env(&env, "opus").unwrap();
    assert_eq!(stored, OPUS);
    assert_eq!(s.read(), format!("{OWNER}defaultModel = \"{OPUS}\"\n"));
    assert_eq!(s.names(), ["config.toml"]);
    // The same pick again writes nothing new.
    assert_eq!(
        ham::tui::save_default_model_with_env(&env, OPUS).unwrap(),
        OPUS
    );
    assert_eq!(s.read(), format!("{OWNER}defaultModel = \"{OPUS}\"\n"));
    // `/config set` in the message box: the router reads the line, the in-process runner runs the invocation.
    let s = Side::new("writer-box", Some(OWNER));
    let env = s.env();
    let api = Api::new(Arc::new(NoNetwork));
    let RouteOutcome::Command { invocation, .. } =
        route("/config set default_model opus", &RouteContext::default())
    else {
        panic!("a command");
    };
    let cfg = ham_core::config::Config::default();
    let downloads = s.config.join("downloads");
    let out = run_captured(
        &invocation,
        &ExecContext {
            api: &api,
            cfg: &cfg,
            downloads: &downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: None,
        },
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr_text());
    assert_eq!(s.read(), format!("{OWNER}defaultModel = \"{OPUS}\"\n"));
}

// ---- the door in the message box -------------------------------------------------------------------------------------------------

#[test]
fn the_box_shows_the_door_note_before_the_first_write_that_adds_a_table_and_only_then() {
    use ham::tui::commands::door_preview;
    let s = Side::new("writer-door", Some(OWNER));
    let env = s.env();
    let outcome = |line: &str| route(line, &RouteContext::default());
    // A new key in a table opens the door: the line gets a preview that says so, and nothing is written by showing it.
    let first = outcome("/config set run.goal off");
    assert!(matches!(first, RouteOutcome::Command { preview: None, .. }));
    let armed = door_preview(&first, &env).expect("a preview for the first table");
    match &armed {
        RouteOutcome::Command {
            argv,
            preview: Some(p),
            ..
        } => {
            assert_eq!(argv, &["config", "set", "run.goal", "off"]);
            assert!(
                p.text.contains(&format!(
                    "ham versions before v{} refuse this file",
                    ham::version::CLIENT
                )) && p.text.contains("`ham config unset run.goal`"),
                "{}",
                p.text
            );
            assert!(p.confirm_with.is_none(), "Enter alone confirms it");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(s.read(), OWNER);
    // A legacy key, an unset, a value that will be refused, a security key and a line that is not a set have no door preview.
    for line in [
        "/config set default_model opus",
        "/config unset run.goal",
        "/config set run.goal maybe",
        "/config set nokey 1",
        "/config set region us-west-2",
        "/config show",
        "/config get run.goal",
    ] {
        assert!(door_preview(&outcome(line), &env).is_none(), "{line}");
    }
    // Once the file has a table, the second new key has none.
    std::fs::write(
        s.file(),
        format!("{OWNER}configVersion = 1\n\n[run]\ngoal = false\n"),
    )
    .unwrap();
    for line in ["/config set run.rules off", "/config set ui.bell off"] {
        assert!(door_preview(&outcome(line), &env).is_none(), "{line}");
    }
    // A file the box cannot edit gets no preview either; the command itself says why.
    std::fs::write(s.file(), "region = \n").unwrap();
    assert!(door_preview(&first, &env).is_none());
}

// ---- the doctor ------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_doctor_lists_what_is_wrong_with_the_file_and_nothing_when_it_is_fine() {
    let s = Side::new("writer-doctor", None);
    let env = s.env();
    // No file and a clean file add no line, so the output of a healthy machine is what it was.
    assert!(config_checks(&env).is_empty());
    std::fs::write(s.file(), OWNER).unwrap();
    assert!(config_checks(&env).is_empty());
    // Each error is a FAIL line of its own with its line number; a warning passes.
    std::fs::write(
        s.file(),
        format!("{OWNER}configVersion = 1\n\n[run]\ngoall = true\nmouse = 1\n"),
    )
    .unwrap();
    let checks = config_checks(&env);
    assert!(!checks.is_empty() && checks.iter().all(|c| c.name == "config"));
    assert!(checks.iter().all(|c| !c.pass), "{checks:?}");
    assert!(
        checks[0].detail.contains("line 9") && checks[0].detail.contains("goall"),
        "{:?}",
        checks[0]
    );
    std::fs::write(
        s.file(),
        "configVersion = 2\nregion = \"us-east-1\"\n[ui]\nfuture = 1\n",
    )
    .unwrap();
    let checks = config_checks(&env);
    assert!(
        !checks.is_empty() && checks.iter().all(|c| c.pass && c.warn),
        "{checks:?}"
    );
    // A file that is not TOML is one FAIL line with the position.
    std::fs::write(s.file(), "region = \n").unwrap();
    let checks = config_checks(&env);
    assert_eq!(checks.len(), 1);
    assert!(
        !checks[0].pass && checks[0].detail.contains("line 1"),
        "{checks:?}"
    );
}
