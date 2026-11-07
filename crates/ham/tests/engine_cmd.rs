//! `ham engine <name> init|show|check|edit|set|unset|import` and the writer behind them:
//! the real `ham` binary in a temp HOME. `set` and `unset` change one line of `~/.config/ham/engines/<name>.toml` and leave every other byte
//! alone; a change that would leave the file refused writes nothing; `import` validates and copies; an engine without a key table answers that it
//! has none. No verb needs the network, the config file or AWS credentials.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use home::{Home, ham_env, text};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, SystemTime};

/// A home with no config file at all.
fn bare(label: &str) -> Home {
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
    h
}

fn path(h: &Home) -> PathBuf {
    h.config.join("ham/engines/cline.toml")
}

fn put(h: &Home, content: &str) {
    std::fs::create_dir_all(h.config.join("ham/engines")).unwrap();
    std::fs::write(path(h), content).unwrap();
}

fn read(h: &Home) -> String {
    std::fs::read_to_string(path(h)).unwrap_or_default()
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    ham_env(h, args, &[]).await
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn out(o: &Output) -> String {
    text(&o.stdout)
}

fn err(o: &Output) -> String {
    text(&o.stderr)
}

fn both(o: &Output) -> String {
    format!("{}\n{}", out(o), err(o))
}

fn sha(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn mtime(h: &Home) -> SystemTime {
    std::fs::metadata(path(h)).unwrap().modified().unwrap()
}

fn set_mtime_long_ago(h: &Home) -> SystemTime {
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path(h))
        .unwrap();
    f.set_modified(then).unwrap();
    then
}

fn names_in_engines_dir(h: &Home) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(h.config.join("ham/engines"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

const TWO_KEYS: &str =
    "# a note\nschema = 1\n[reasoning]\neffort = \"high\"\n[commands]\ntimeout_seconds = 900\n";

// ---- init and import -----------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_writes_a_template_with_every_key_commented_and_import_validates_then_copies() {
    let h = bare("eng-init");
    assert!(!h.config.join("ham/engines").exists());
    let o = ham(&h, &["engine", "cline", "init"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    let template = ham_core::engines::settings_template("cline").unwrap();
    assert_eq!(read(&h), template);
    assert_eq!(
        std::fs::metadata(path(&h)).unwrap().permissions().mode() & 0o777,
        0o600,
        "a new file is private"
    );
    assert_eq!(names_in_engines_dir(&h), ["cline.toml"], "no temp file");
    for row in harness_protocol::direct::CLINE_KEYS {
        let key = row.path.split_once('.').unwrap().1;
        assert!(
            template
                .lines()
                .any(|l| l.starts_with(&format!("# {key} = "))),
            "{key} is not commented out"
        );
    }
    // The template reads as a file that sets nothing.
    let o = ham(&h, &["engine", "cline", "check"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(out(&o).contains(": ok"), "{}", out(&o));

    // A second init changes nothing: the bytes and the modification time stay.
    let then = set_mtime_long_ago(&h);
    let o = ham(&h, &["engine", "cline", "init"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(out(&o).contains("already exists"), "{}", out(&o));
    assert_eq!((read(&h), mtime(&h)), (template.clone(), then));

    // Import: the file is validated, then copied byte for byte, comments included.
    let mine = "# mine\nschema = 1\n[reasoning]\neffort = \"low\" # quick\n";
    let src = h.root.join("mine.toml");
    std::fs::write(&src, mine).unwrap();
    let o = ham(&h, &["engine", "cline", "import", src.to_str().unwrap()]).await;
    assert_eq!(code(&o), 2, "{}", both(&o));
    assert!(
        err(&o).contains("already exists") && err(&o).contains("--replace"),
        "{}",
        err(&o)
    );
    assert_eq!(
        read(&h),
        template,
        "an existing file is not replaced without --replace"
    );
    let o = ham(
        &h,
        &[
            "engine",
            "cline",
            "import",
            src.to_str().unwrap(),
            "--replace",
        ],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert_eq!(read(&h), mine);
    assert_eq!(names_in_engines_dir(&h), ["cline.toml"], "no temp file");

    // A file the check refuses is not imported, and the file that is there stays.
    let bad = h.root.join("bad.toml");
    std::fs::write(&bad, "schema = 1\n\n[aws]\nprofile = \"x\"\n").unwrap();
    let o = ham(
        &h,
        &[
            "engine",
            "cline",
            "import",
            bad.to_str().unwrap(),
            "--replace",
        ],
    )
    .await;
    assert_eq!(code(&o), 2, "{}", both(&o));
    assert!(
        err(&o).contains("line 4") && err(&o).contains("aws.profile"),
        "{}",
        err(&o)
    );
    assert_eq!(read(&h), mine);
    // Importing the same bytes again writes nothing.
    let o = ham(&h, &["engine", "cline", "import", src.to_str().unwrap()]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(out(&o).contains("nothing was written"), "{}", out(&o));
    // A file that is not there.
    let o = ham(&h, &["engine", "cline", "import", "/no/such/file.toml"]).await;
    assert_eq!(code(&o), 2, "{}", both(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_into_a_home_with_no_file_makes_the_folder_and_a_private_file() {
    let h = bare("eng-import-new");
    let src = h.root.join("mine.toml");
    std::fs::write(&src, TWO_KEYS).unwrap();
    let o = ham(&h, &["engine", "cline", "import", src.to_str().unwrap()]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert_eq!(read(&h), TWO_KEYS);
    assert_eq!(
        std::fs::metadata(path(&h)).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

// ---- show and check --------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_prints_the_file_sha256_and_the_settings_hash_and_check_exits_2_on_failure() {
    let h = bare("eng-show");
    put(&h, TWO_KEYS);
    let want = ham_core::engine_file::read_text(TWO_KEYS, "cline")
        .config
        .unwrap()
        .canonical_sha256();
    let o = ham(&h, &["engine", "cline", "show"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    let shown = out(&o);
    assert!(shown.contains(&path(&h).display().to_string()), "{shown}");
    assert!(
        shown.contains(&format!("file sha256: {}", sha(TWO_KEYS))),
        "{shown}"
    );
    assert!(shown.contains(&format!("settings hash: {want}")), "{shown}");
    assert!(
        shown.contains("reasoning.effort = high")
            && shown.contains("commands.timeout_seconds = 900 s"),
        "{shown}"
    );
    assert!(shown.contains("(line 4)"), "{shown}");

    // Two files that differ only in comments and order have the same settings hash and different file hashes.
    let other = "schema = 1\n[commands]\ntimeout_seconds = 900\n# no note here\n[reasoning]\neffort = 'high'\n";
    put(&h, other);
    let o = ham(&h, &["engine", "cline", "show"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(
        out(&o).contains(&format!("settings hash: {want}")),
        "{}",
        out(&o)
    );
    assert!(
        out(&o).contains(&format!("file sha256: {}", sha(other))),
        "{}",
        out(&o)
    );
    assert_ne!(sha(TWO_KEYS), sha(other));

    // The JSON view carries the same facts.
    let o = ham(&h, &["engine", "cline", "show", "--json"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    let v: serde_json::Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["settingsSha256"], want);
    assert_eq!(v["fileSha256"], sha(other));
    assert_eq!(v["keys"].as_array().unwrap().len(), 2);
    assert_eq!(v["exists"], true);

    // A file the check refuses: show and check say which line and exit 2.
    put(&h, "schema = 1\n\n[aws]\nprofile = \"x\"\n");
    for verb in ["show", "check"] {
        let o = ham(&h, &["engine", "cline", verb]).await;
        assert_eq!(code(&o), 2, "{verb}: {}", both(&o));
        let all = both(&o);
        assert!(
            all.contains("line 4") && all.contains("aws.profile"),
            "{verb}: {all}"
        );
        assert!(all.contains("root invariants 2 and 8"), "{verb}: {all}");
    }
    let o = ham(&h, &["engine", "cline", "check", "--json"]).await;
    assert_eq!(code(&o), 2);
    let v: serde_json::Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["problems"][0]["code"], "denied_key");
    assert_eq!(v["problems"][0]["line"], 4);

    // No file: show and check say so, exit 0 and make nothing.
    let h2 = bare("eng-show-none");
    for verb in ["show", "check"] {
        let o = ham(&h2, &["engine", "cline", verb]).await;
        assert_eq!(code(&o), 0, "{verb}: {}", both(&o));
        assert!(
            !h2.config.join("ham/engines").exists(),
            "{verb} made the folder"
        );
    }
}

// ---- set and unset ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_and_unset_use_the_patcher_and_keep_comments() {
    let h = bare("eng-set");
    // On an absent file, `schema = 1` is written first, then the key.
    let o = ham(&h, &["engine", "cline", "set", "reasoning.effort", "high"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert_eq!(out(&o).trim(), "reasoning.effort = high");
    assert_eq!(read(&h), "schema = 1\n\n[reasoning]\neffort = \"high\"\n");
    assert_eq!(names_in_engines_dir(&h), ["cline.toml"], "no temp file");
    assert_eq!(
        std::fs::metadata(path(&h)).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Comments, order and every other byte stay; a trailing comment on the value's line stays.
    let commented = "# my note\nschema = 1\n\n[reasoning]\neffort = \"low\"   # quick\n\n[tools]\ndisable = []\n";
    put(&h, commented);
    let o = ham(
        &h,
        &["engine", "cline", "set", "reasoning.effort", "medium"],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert_eq!(read(&h), commented.replace("\"low\"", "\"medium\""));
    let o = ham(
        &h,
        &[
            "engine",
            "cline",
            "set",
            "tools.disable",
            "spawn_agent, fetch_web_content",
        ],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(
        read(&h).contains("disable = [\"spawn_agent\", \"fetch_web_content\"]"),
        "{}",
        read(&h)
    );
    let o = ham(
        &h,
        &["engine", "cline", "set", "commands.timeout_seconds", "900"],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(
        read(&h).ends_with("\n[commands]\ntimeout_seconds = 900\n"),
        "{}",
        read(&h)
    );
    let o = ham(
        &h,
        &[
            "engine",
            "cline",
            "set",
            "context.compaction_trigger",
            "0.9",
        ],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(
        read(&h).contains("compaction_trigger = 0.9\n"),
        "{}",
        read(&h)
    );
    assert!(read(&h).starts_with("# my note\nschema = 1\n"));
    let o = ham(&h, &["engine", "cline", "check"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));

    // Setting what a key already holds writes nothing: the bytes and the modification time stay.
    let before = read(&h);
    let then = set_mtime_long_ago(&h);
    let o = ham(
        &h,
        &["engine", "cline", "set", "reasoning.effort", "medium"],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(out(&o).contains("already medium"), "{}", out(&o));
    assert_eq!((read(&h), mtime(&h)), (before, then));

    // Unset removes the key's line and keeps the comments.
    let o = ham(&h, &["engine", "cline", "unset", "reasoning.effort"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert_eq!(out(&o).trim(), "reasoning.effort = (not set)");
    let after = read(&h);
    assert!(!after.contains("effort"), "{after}");
    assert!(after.starts_with("# my note\nschema = 1\n"), "{after}");
    assert!(
        after.contains("disable = [\"spawn_agent\", \"fetch_web_content\"]"),
        "{after}"
    );
    // A key that is not set: nothing to remove, exit 0.
    let o = ham(&h, &["engine", "cline", "unset", "reasoning.effort"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(out(&o).contains("is not set"), "{}", out(&o));
    assert_eq!(read(&h), after);
    // Unset of everything leaves a file that sets nothing.
    for key in [
        "tools.disable",
        "commands.timeout_seconds",
        "context.compaction_trigger",
    ] {
        assert_eq!(
            code(&ham(&h, &["engine", "cline", "unset", key]).await),
            0,
            "{key}"
        );
    }
    let o = ham(&h, &["engine", "cline", "show"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(out(&o).contains("no setting is set"), "{}", out(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_that_the_check_refuses_writes_nothing_and_says_why() {
    let h = bare("eng-refuse");
    for (args, want) in [
        (
            vec!["commands.timeout_seconds", "1801"],
            "a whole number from 1 to 1800",
        ),
        (
            vec!["commands.timeout_seconds", "abc"],
            "takes a whole number",
        ),
        (
            vec!["commands.timeout_seconds", "900.5"],
            "takes a whole number",
        ),
        (
            vec!["reasoning.effort", "none"],
            "one of: default, off, low, medium, high, xhigh",
        ),
        (
            vec!["reasoning.effort", "max"],
            "one of: default, off, low, medium, high, xhigh",
        ),
        (
            vec!["context.compaction_trigger", "1"],
            "a number from 0.10 to 0.99",
        ),
        (vec!["context.compaction_trigger", "nan"], "takes a number"),
        (
            vec!["tools.disable", "spawn_agent,spawn_agent"],
            "lists a value twice",
        ),
        (
            vec!["tools.disable", "mcp"],
            "a list of: spawn_agent, fetch_web_content",
        ),
        (
            vec!["aws.profile", "x"],
            "key \"aws.profile\" is not allowed",
        ),
        (
            vec!["provider", "x"],
            "the model, provider and limits come from the run's model",
        ),
        (vec!["reasoning.efort", "low"], "is not an engine setting"),
        (
            vec!["Reasoning.Effort", "low"],
            "not the name of an engine setting",
        ),
        (vec!["a/b", "low"], "not the name of an engine setting"),
    ] {
        let mut argv = vec!["engine", "cline", "set"];
        argv.extend(&args);
        let o = ham(&h, &argv).await;
        assert_eq!(code(&o), 2, "{args:?}: {}", both(&o));
        assert!(
            err(&o).contains(want),
            "{args:?}: {want:?} not in {}",
            err(&o)
        );
        assert!(
            !h.config.join("ham/engines").exists(),
            "{args:?} made the folder"
        );
    }
    // The value is never repeated.
    let o = ham(
        &h,
        &[
            "engine",
            "cline",
            "set",
            "reasoning.effort",
            "secret-looking-text",
        ],
    )
    .await;
    assert_eq!(code(&o), 2);
    assert!(!both(&o).contains("secret-looking-text"), "{}", both(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_refuses_a_file_that_is_not_toml_or_is_newer_and_changes_nothing() {
    let h = bare("eng-unreadable");
    for (content, want) in [
        ("schema = 1\n[reasoning\n", "not valid TOML"),
        ("schema = 2\n[reasoning]\neffort = \"low\"\n", "newer ham"),
        ("schema = 1\nschema = 1\n", "not valid TOML"),
    ] {
        put(&h, content);
        let then = set_mtime_long_ago(&h);
        for args in [
            vec!["engine", "cline", "set", "reasoning.effort", "high"],
            vec!["engine", "cline", "unset", "reasoning.effort"],
        ] {
            let o = ham(&h, &args).await;
            assert_eq!(code(&o), 2, "{content:?} {args:?}: {}", both(&o));
            assert!(err(&o).contains(want), "{content:?}: {}", err(&o));
            assert_eq!((read(&h), mtime(&h)), (content.to_string(), then));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_key_is_removed_with_unset_while_set_refuses_to_leave_it() {
    let h = bare("eng-repair");
    let broken = "schema = 1\n[aws]\nprofile = \"x\"\n[reasoning]\neffort = \"low\"\n";
    put(&h, broken);
    // A set would leave the denied key in the file, so it is refused and nothing is written.
    let o = ham(&h, &["engine", "cline", "set", "reasoning.effort", "high"]).await;
    assert_eq!(code(&o), 2, "{}", both(&o));
    assert!(
        err(&o).contains("aws.profile") && err(&o).contains("Nothing was written"),
        "{}",
        err(&o)
    );
    assert_eq!(read(&h), broken);
    // Unset removes it, whatever the table, so the file can be repaired.
    let o = ham(&h, &["engine", "cline", "unset", "aws.profile"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(!read(&h).contains("aws"), "{}", read(&h));
    assert!(read(&h).contains("effort = \"low\""));
    let o = ham(&h, &["engine", "cline", "check"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_file_keeps_its_link_and_mode_and_leaves_no_temp_file() {
    let h = bare("eng-link");
    let real = h.root.join("dotfiles/cline.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, TWO_KEYS).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::create_dir_all(h.config.join("ham/engines")).unwrap();
    symlink(&real, path(&h)).unwrap();
    let o = ham(&h, &["engine", "cline", "set", "reasoning.effort", "low"]).await;
    assert_eq!(code(&o), 0, "{}", both(&o));
    assert!(
        std::fs::symlink_metadata(path(&h))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        TWO_KEYS.replace("\"high\"", "\"low\"")
    );
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let leftovers: Vec<String> = std::fs::read_dir(real.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, ["cline.toml"], "no temp file beside the target");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_changed_between_load_and_rename_stops_the_write() {
    // The writer reads the file again just before the rename and compares its SHA-256 with the one loaded.
    let h = bare("eng-race");
    put(&h, TWO_KEYS);
    let stale = Sha256::digest(b"what the file used to hold");
    let words = "engines/cline.toml changed on disk; reloaded; nothing written";
    let e = ham::config_cmd::commit_file(&path(&h), Some(stale.into()), "schema = 1\n", words)
        .unwrap_err();
    assert_eq!(e, words);
    assert_eq!(read(&h), TWO_KEYS, "the file is as it was");
    assert_eq!(
        names_in_engines_dir(&h),
        ["cline.toml"],
        "the temp file is gone"
    );
    // With the digest it was read with, the same call writes.
    let now = Sha256::digest(TWO_KEYS.as_bytes());
    ham::config_cmd::commit_file(&path(&h), Some(now.into()), "schema = 1\n", words).unwrap();
    assert_eq!(read(&h), "schema = 1\n");
}

// ---- other engines, hostile files ------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_engine_name_answers_no_schema_in_this_build() {
    let h = bare("eng-others");
    for name in ["pi", "opencode", "codex", "kiro"] {
        for verb in ["show", "check", "init"] {
            let o = ham(&h, &["engine", name, verb]).await;
            assert_eq!(code(&o), 2, "{name} {verb}: {}", both(&o));
            assert!(
                err(&o).contains("no settings schema in this build yet"),
                "{name} {verb}: {}",
                err(&o)
            );
        }
    }
    assert!(!h.config.join("ham/engines").exists(), "a folder was made");
    // A word that is no engine at all.
    let o = ham(&h, &["engine", "nope", "show"]).await;
    assert_eq!(code(&o), 2, "{}", both(&o));
    assert!(
        err(&o).contains("unknown engine") && err(&o).contains("cline"),
        "{}",
        err(&o)
    );
    // A word that could be a path is a usage error before anything runs.
    let o = ham(&h, &["engine", "../x", "show"]).await;
    assert_eq!(code(&o), 2, "{}", both(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_characters_and_escape_sequences_in_a_key_never_reach_stdout_or_stderr() {
    let h = bare("eng-hostile");
    put(
        &h,
        "schema = 1\n\"\\u001b[31mevil\\u0007\" = 1\n[\"x\\u202ey\"]\nk = 1\n[reasoning]\n\"\\u001b]0;t\\u0007\" = \"low\"\n",
    );
    for args in [
        vec!["engine", "cline", "check"],
        vec!["engine", "cline", "show"],
        vec!["engine", "cline", "check", "--json"],
        vec!["engine", "cline", "show", "--json"],
        vec!["engine", "cline", "set", "reasoning.effort", "low"],
        vec!["engine", "cline", "unset", "reasoning.effort"],
    ] {
        let o = ham(&h, &args).await;
        let all = both(&o);
        for c in all.chars() {
            assert!(!c.is_control() || c == '\n', "{args:?}: {c:?} in {all:?}");
        }
        assert!(!all.contains('\u{202e}'), "{args:?}: {all:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parse_error_prints_line_and_column_and_never_the_source_line() {
    let h = bare("eng-parse-error");
    // A credential-shaped line assembled at run time, with a syntax error after it, in a file other tools link to.
    let id: String = ["A", "K", "I", "A"].concat() + &"Q".repeat(16);
    let real = h.root.join("elsewhere.toml");
    std::fs::write(
        &real,
        format!("schema = 1\n[tools]\napi_access = {id} oops\n"),
    )
    .unwrap();
    std::fs::create_dir_all(h.config.join("ham/engines")).unwrap();
    symlink(&real, path(&h)).unwrap();
    for verb in ["check", "show"] {
        let o = ham(&h, &["engine", "cline", verb]).await;
        assert_eq!(code(&o), 2, "{verb}: {}", both(&o));
        let all = both(&o);
        assert!(all.contains("line 3 column"), "{verb}: {all}");
        assert!(
            !all.contains(&id) && !all.contains("api_access"),
            "{verb}: {all}"
        );
    }
}
