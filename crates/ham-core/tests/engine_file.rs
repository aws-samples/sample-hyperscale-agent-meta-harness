//! The engine file on the client: `~/.config/ham/engines/<engine>.toml` is read once
//! into one buffer, parsed with the `toml` crate's spans, converted to typed JSON and checked by `harness_protocol::direct::check_engine_config`, the
//! one check that control-api and agentd call too. The file is the user's own text; comments never leave the Mac. A default user has no file and
//! sends no member.
#![allow(clippy::unwrap_used)]

use ham_core::engine_file::{self, Disk, EngineFile, Reading, Source};
use harness_protocol::direct::check_engine_config;
use serde_json::json;
use std::cell::Cell;
use std::path::{Path, PathBuf};

const ENGINE: &str = "cline";

fn read(text: &str) -> Reading {
    engine_file::read_text(text, ENGINE)
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ham-engine-file-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A file with every key of the table, comments and all.
const FULL: &str = "# my settings\nschema = 1\n\n[reasoning]\neffort = \"high\"   # think harder\n\n[commands]\ntimeout_seconds = 900\n\n[context]\ncompaction = \"agentic\"\ncompaction_trigger = 0.90\n\n[tools]\ndisable = [\"spawn_agent\"]\n";

// ---- the default user ----------------------------------------------------------------------------------------------------------------

#[test]
fn a_default_user_has_no_engines_folder_and_the_request_has_no_member() {
    // The path is `<config dir>/ham/engines/<engine>.toml`, and reading a file that is not there makes no folder and sends nothing.
    let root = scratch("default");
    let config = root.join("config");
    std::fs::create_dir_all(config.join("ham")).unwrap();
    std::fs::write(config.join("ham/config.toml"), "region = \"us-east-1\"\n").unwrap();
    let dir = config.display().to_string();
    let env = move |k: &str| (k == "XDG_CONFIG_HOME").then(|| dir.clone());
    let path = engine_file::engine_path(&env, ENGINE).unwrap();
    assert_eq!(path, config.join("ham/engines/cline.toml"));
    let file = EngineFile::load(&path, &Disk).unwrap();
    assert!(!file.exists());
    assert_eq!(file.file_sha256(), None);
    let r = file.read(ENGINE);
    assert!(r.ok() && r.config.is_none(), "{:?}", r.problems);
    assert!(
        !config.join("ham/engines").exists(),
        "reading made the folder"
    );
    println!("ENGINE-FILE TRIGGER PRESENT: no engines folder, member None");
}

#[test]
fn the_path_follows_home_when_there_is_no_xdg_folder_and_the_engine_is_never_a_path() {
    let env = |k: &str| (k == "HOME").then(|| "/home/me".to_string());
    assert_eq!(
        engine_file::engine_path(&env, ENGINE).unwrap(),
        PathBuf::from("/home/me/.config/ham/engines/cline.toml")
    );
    // Only an engine of the roadmap has a file: a name with a separator or a dot cannot leave the folder.
    for bad in [
        "../x",
        "a/b",
        "",
        ".",
        "..",
        "cline.toml",
        "cline\0",
        "nope",
    ] {
        assert_eq!(engine_file::engine_path(&env, bad), None, "{bad:?}");
    }
    assert_eq!(engine_file::shown_path(ENGINE), "engines/cline.toml");
}

#[test]
fn a_file_with_only_schema_1_sends_nothing() {
    for text in [
        "",
        "# only a comment\n",
        "schema = 1\n",
        "schema = 1\n[tools]\n",
        "schema = 1\n[reasoning]\n[commands]\n",
    ] {
        let r = read(text);
        assert!(r.ok(), "{text:?}: {:?}", r.problems);
        assert!(r.config.is_none(), "{text:?}");
    }
}

// ---- reading once --------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Counting {
    opens: Cell<u32>,
}

impl Source for Counting {
    fn read(&self, _path: &Path) -> Result<Option<(Vec<u8>, u64)>, String> {
        self.opens.set(self.opens.get() + 1);
        Ok(Some((FULL.as_bytes().to_vec(), FULL.len() as u64)))
    }
}

#[test]
fn the_file_is_read_once_into_one_buffer() {
    let double = Counting::default();
    let file = EngineFile::load(Path::new("/a/engines/cline.toml"), &double).unwrap();
    // Everything the verbs and the request need comes from that one buffer.
    let _ = file.read(ENGINE);
    let _ = file.read(ENGINE);
    let _ = file.text();
    let _ = file.file_sha256();
    let _ = file.read(ENGINE).line_of("reasoning.effort");
    assert_eq!(double.opens.get(), 1, "the file was opened more than once");
}

// ---- conversion ----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_file_with_keys_becomes_the_typed_member_and_passes_the_shared_check() {
    let r = read(FULL);
    assert!(r.ok(), "{:?}", r.problems);
    let cfg = r.config.clone().expect("a member");
    assert_eq!((cfg.engine.as_str(), cfg.schema), (ENGINE, 1));
    assert_eq!(
        serde_json::Value::Object(cfg.settings.clone()),
        json!({
            "reasoning": {"effort": "high"},
            "commands": {"timeout_seconds": 900},
            "context": {"compaction": "agentic", "compaction_trigger": 0.9},
            "tools": {"disable": ["spawn_agent"]},
        })
    );
    check_engine_config(&cfg).unwrap();
    assert_eq!(cfg.key_count(), 5);
    // The keys the file sets, with the line of each.
    assert_eq!(r.line_of("reasoning.effort"), Some(5));
    assert_eq!(r.line_of("commands.timeout_seconds"), Some(8));
    assert_eq!(r.line_of("tools.disable"), Some(15));
    assert_eq!(r.line_of("tools.nothing"), None);
}

#[test]
fn comments_order_and_notation_do_not_change_the_settings_hash() {
    let a = read(FULL).config.unwrap().canonical_sha256();
    let same = "schema = 1\n[tools]\ndisable = ['spawn_agent']\n[context]\ncompaction_trigger = 0.9\ncompaction = 'agentic'\n[commands]\ntimeout_seconds = 900\n[reasoning]\neffort = 'high'\n";
    let b = read(same).config.unwrap().canonical_sha256();
    assert_eq!(a, b);
    let other = FULL.replace("high", "low");
    assert_ne!(a, read(&other).config.unwrap().canonical_sha256());
    assert_eq!(a.len(), 64);
}

#[test]
fn an_inline_table_is_read_like_a_standard_table() {
    let r = read("schema = 1\nreasoning = { effort = \"low\" }\n");
    assert!(r.ok(), "{:?}", r.problems);
    assert_eq!(r.config.unwrap().get("reasoning.effort").unwrap(), "low");
}

// ---- problems, with their lines ------------------------------------------------------------------------------------------------------

fn only(r: &Reading) -> &engine_file::Problem {
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    &r.problems[0]
}

#[test]
fn a_denied_key_names_its_line_its_reason_and_the_invariant_and_never_the_value() {
    let text = "schema = 1\n\n[aws]\nprofile = \"secret-looking-value\"\n";
    let r = read(text);
    assert!(r.config.is_none());
    let p = only(&r);
    assert_eq!(p.code, "denied_key");
    assert_eq!(p.line, Some(4));
    assert_eq!(p.key.as_deref(), Some("aws.profile"));
    let shown = p.render("engines/cline.toml");
    assert_eq!(
        shown,
        "engines/cline.toml line 4: key \"aws.profile\" is not allowed: the harness supplies credentials itself; this cannot be set (root invariants 2 and 8)"
    );
    assert!(!shown.contains("secret-looking-value"));
    let all = r.refusal("engines/cline.toml");
    assert!(all.ends_with("Nothing was sent."), "{all}");
}

#[test]
fn an_unknown_key_and_a_value_out_of_range_name_their_lines() {
    let r = read("schema = 1\n[reasoning]\nefort = \"low\"\n");
    let p = only(&r);
    assert_eq!((p.code.as_str(), p.line), ("unknown_key", Some(3)));
    assert!(p.message.contains("efort"), "{}", p.message);

    let r = read("schema = 1\n[commands]\n# a comment\ntimeout_seconds = 1801\n");
    let p = only(&r);
    assert_eq!((p.code.as_str(), p.line), ("out_of_range", Some(4)));
    assert!(
        p.message.contains("a whole number from 1 to 1800"),
        "{}",
        p.message
    );
    assert!(!p.message.contains("1801"), "the value is never echoed");

    let r = read("schema = 1\n[context]\ncompaction_trigger = 1\n");
    assert_eq!(only(&r).code, "out_of_range");
    let r = read("schema = 1\n[context]\ncompaction_trigger = 0.05\n");
    assert_eq!(only(&r).code, "out_of_range");
    let r = read("schema = 1\n[reasoning]\neffort = \"max\"\n");
    assert_eq!(only(&r).code, "out_of_range");
    let r = read("schema = 1\n[reasoning]\neffort = \"none\"\n");
    assert_eq!(only(&r).code, "out_of_range");
    let r = read("schema = 1\n[tools]\ndisable = [\"spawn_agent\", \"spawn_agent\"]\n");
    assert_eq!(only(&r).code, "duplicate_value");
}

#[test]
fn a_wrong_type_is_named_with_its_line() {
    let r = read("schema = 1\n[commands]\ntimeout_seconds = \"900\"\n");
    let p = only(&r);
    assert_eq!((p.code.as_str(), p.line), ("bad_type", Some(3)));
    let r = read("schema = 1\n[commands]\ntimeout_seconds = 9.5\n");
    assert_eq!(only(&r).code, "bad_type");
    let r = read("schema = 1\n[tools]\ndisable = \"spawn_agent\"\n");
    assert_eq!(only(&r).code, "bad_type");
}

#[test]
fn the_schema_line_is_needed_once_a_key_is_set_and_a_newer_one_is_named() {
    let r = read("[tools]\ndisable = []\n[reasoning]\neffort = \"low\"\n");
    let p = only(&r);
    assert_eq!(p.code, "missing_schema");
    // The line of the first thing in the file, where the schema line belongs.
    assert_eq!(p.line, Some(1));
    assert!(p.message.contains("schema = 1"), "{}", p.message);

    let r = read("schema = 2\n[reasoning]\neffort = \"low\"\n");
    let p = only(&r);
    assert_eq!((p.code.as_str(), p.line), ("newer_schema", Some(1)));
    assert!(p.message.contains("newer ham"), "{}", p.message);

    for text in [
        "schema = \"1\"\n",
        "schema = 1.0\n",
        "schema = true\n",
        "schema = 0\n",
        "schema = -1\n",
    ] {
        let r = read(text);
        let p = only(&r);
        assert!(
            p.code == "bad_type" || p.code == "bad_schema",
            "{text:?}: {}",
            p.code
        );
        assert_eq!(p.line, Some(1));
    }
}

#[test]
fn every_problem_of_a_conversion_is_reported_and_none_hides_another() {
    let r = read(
        "schema = 1\n[commands]\ntimeout_seconds = 1979-05-27T07:32:00Z\n[context]\ncompaction_trigger = nan\n[tools]\ndisable = [1, 2]\n",
    );
    let lines: Vec<_> = r
        .problems
        .iter()
        .map(|p| (p.code.as_str(), p.line))
        .collect();
    assert_eq!(
        lines,
        [
            ("bad_type", Some(3)),
            ("bad_type", Some(5)),
            ("bad_type", Some(7))
        ]
    );
}

// ---- the file on disk ----------------------------------------------------------------------------------------------------------------

#[test]
fn the_disk_source_follows_a_link_and_refuses_a_directory_and_a_device() {
    let dir = scratch("disk");
    let real = dir.join("real.toml");
    std::fs::write(&real, FULL).unwrap();
    let link = dir.join("link.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let file = EngineFile::load(&link, &Disk).unwrap();
    assert!(file.exists());
    assert_eq!(file.text(), FULL);
    assert_eq!(file.file_sha256().unwrap().len(), 64);
    let e = EngineFile::load(&dir, &Disk).unwrap_err();
    assert!(e.contains("not a regular file"), "{e}");
    let e = EngineFile::load(Path::new("/dev/null"), &Disk).unwrap_err();
    assert!(e.contains("not a regular file"), "{e}");
    let missing = EngineFile::load(&dir.join("nope.toml"), &Disk).unwrap();
    assert!(!missing.exists());
}

#[test]
fn the_template_that_init_writes_reads_as_no_settings() {
    let text = ham_core::engines::settings_template(ENGINE).expect("a template");
    let r = read(&text);
    assert!(r.ok(), "{:?}\n{text}", r.problems);
    assert!(r.config.is_none());
    assert!(text.starts_with('#'));
    assert!(text.lines().any(|l| l == "schema = 1"));
}
