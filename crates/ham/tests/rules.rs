//! `ham rules init|edit|show`: the user's rules live in `~/.config/ham/rules/`, the
//! nudge text in `~/.config/ham/continue.md`. The real `ham` binary in a temp HOME (never the real one): no config file, no
//! network and no AWS credentials are needed, and nothing is written outside the HOME.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::rules::{DEFAULT_CONTINUE, DEFAULT_RULES};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    fn new(label: &str) -> Home {
        Home {
            root: support::scratch(label),
        }
    }
    fn rules_dir(&self) -> PathBuf {
        self.root.join(".config/ham/rules")
    }
    fn rules(&self) -> PathBuf {
        self.rules_dir().join("00-my-rules.md")
    }
    fn continue_md(&self) -> PathBuf {
        self.root.join(".config/ham/continue.md")
    }
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.root.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
    fn run(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ham"));
        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
                cmd.env(k, v);
            }
        }
        cmd.args(args)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("TMPDIR", &self.root);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null()).output().unwrap()
    }
    /// Every path under the HOME, relative, sorted.
    fn tree(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            let mut es: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap())
                .collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let p = e.path();
                out.push(p.strip_prefix(root).unwrap().display().to_string());
                if e.file_type().unwrap().is_dir() {
                    walk(&p, root, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out);
        out
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

#[test]
fn init_writes_the_default_rules_and_the_nudge_file_under_home_and_nowhere_else() {
    // `--pack none` is the pointer-only default of before; the plain command adds the lifecycle contract (see `tests/pack.rs`).
    let h = Home::new("rules-init");
    let o = h.run(&["rules", "init", "--pack", "none"], &[]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(h.rules()).unwrap(), DEFAULT_RULES);
    assert_eq!(std::fs::read(h.rules()).unwrap().len(), DEFAULT_RULES.len());
    assert!(
        DEFAULT_RULES.contains("## Environment of the cloud session"),
        "ham rules init writes the environment section"
    );
    assert_eq!(
        std::fs::read_to_string(h.continue_md()).unwrap(),
        DEFAULT_CONTINUE
    );
    let text = out(&o);
    assert!(
        text.contains("created") && text.contains(&h.rules().display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&h.continue_md().display().to_string()),
        "{text}"
    );
    assert!(
        text.contains("ham rules edit"),
        "says how to change it: {text}"
    );
    // Only the rules file, the two prompt files and their directories exist; invariant 5 names the rules directory, `continue.md` and
    // `review.md` among the user's own files.
    assert_eq!(
        h.tree(),
        [
            ".config",
            ".config/ham",
            ".config/ham/continue.md",
            ".config/ham/review.md",
            ".config/ham/rules",
            ".config/ham/rules/00-my-rules.md"
        ]
    );
}

#[test]
fn init_twice_changes_nothing_and_never_overwrites_an_edit() {
    let h = Home::new("rules-init-twice");
    assert_eq!(code(&h.run(&["rules", "init"], &[])), 0);
    std::fs::write(h.rules(), "# Mine\n\nAlways write the test first.\n").unwrap();
    std::fs::write(h.continue_md(), "Carry on.\n").unwrap();
    let before = h.tree();
    let o = h.run(&["rules", "init"], &[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(out(&o).contains("kept"), "{}", out(&o));
    assert_eq!(
        std::fs::read_to_string(h.rules()).unwrap(),
        "# Mine\n\nAlways write the test first.\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.continue_md()).unwrap(),
        "Carry on.\n"
    );
    assert_eq!(h.tree(), before);
}

#[test]
fn init_leaves_a_user_who_already_has_rule_files_alone() {
    let h = Home::new("rules-init-existing");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.rules_dir().join("team.md"), "Team rule.\n").unwrap();
    let o = h.run(&["rules", "init"], &[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        !h.rules().exists(),
        "the default is not added beside the user's own rules"
    );
    assert!(
        h.continue_md().exists(),
        "the missing nudge file is still written"
    );
}

#[test]
fn init_refuses_a_rules_path_that_is_not_a_directory() {
    let h = Home::new("rules-init-file");
    std::fs::create_dir_all(h.root.join(".config/ham")).unwrap();
    std::fs::write(h.root.join(".config/ham/rules"), "oops").unwrap();
    let o = h.run(&["rules", "init"], &[]);
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("rules") && err(&o).contains("not a directory"),
        "{}",
        err(&o)
    );
}

#[test]
fn show_prints_the_effective_rules_and_the_nudge() {
    let h = Home::new("rules-show");
    h.run(&["rules", "init", "--pack", "none"], &[]);
    std::fs::write(h.rules_dir().join("second.md"), "Second.\n").unwrap();
    let o = h.run(&["rules", "show"], &[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let t = out(&o);
    assert!(t.contains(&h.rules_dir().display().to_string()), "{t}");
    assert!(
        t.contains("00-my-rules.md") && t.contains("second.md"),
        "{t}"
    );
    assert!(
        t.contains(&sha(DEFAULT_RULES.as_bytes()))
            && t.contains(&format!("{} bytes", DEFAULT_RULES.len())),
        "{t}"
    );
    assert!(
        t.contains("Test-first development and proving it works") && t.contains("Second."),
        "{t}"
    );
    assert!(
        t.contains("continue.md") && t.contains("max_nudges 0"),
        "{t}"
    );
    assert!(
        t.contains("Re-read my request."),
        "show prints the nudge text too: {t}"
    );
    assert!(
        t.find("00-my-rules.md").unwrap() < t.find("second.md").unwrap(),
        "sorted by name"
    );

    let o = h.run(&["rules", "show", "--json"], &[]);
    assert_eq!(code(&o), 0);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["rules"].as_array().unwrap().len(), 2);
    assert_eq!(v["rules"][0]["path"], "00-my-rules.md");
    assert_eq!(v["rules"][0]["sha256"], sha(DEFAULT_RULES.as_bytes()));
    assert_eq!(v["continue"]["maxNudges"], 0);
    assert_eq!(v["rulesDir"], h.rules_dir().display().to_string());
}

#[test]
fn show_before_init_says_to_run_init_and_writes_nothing() {
    let h = Home::new("rules-show-none");
    let o = h.run(&["rules", "show"], &[]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("ham rules init"), "{}", err(&o));
    assert!(h.tree().is_empty(), "show writes nothing: {:?}", h.tree());
    // An empty rules directory is the same.
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    let o = h.run(&["rules", "show"], &[]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("ham rules init"), "{}", err(&o));
}

#[test]
fn show_names_each_kind_of_bad_rules_file_and_exits_1() {
    type Setup = fn(&Path);
    let cases: Vec<(&str, Setup, &str)> = vec![
        (
            "AGENTS.md",
            |d| std::fs::write(d.join("AGENTS.md"), "Rule.\n").unwrap(),
            "AGENTS.md",
        ),
        (
            "empty",
            |d| std::fs::write(d.join("e.md"), "  \n").unwrap(),
            "e.md",
        ),
        (
            "dashes",
            |d| std::fs::write(d.join("d.md"), "---\nrule\n").unwrap(),
            "d.md",
        ),
        (
            "badyaml",
            |d| std::fs::write(d.join("y.md"), "---\na: [x\n---\nB\n").unwrap(),
            "y.md",
        ),
        (
            "oversize",
            // Longer than a whole request can be: refused before it is read, so a file of holes is as good as one of text.
            |d| {
                std::fs::File::create(d.join("big.md"))
                    .unwrap()
                    .set_len(6_225_921)
                    .unwrap()
            },
            "big.md",
        ),
        (
            "utf8",
            |d| std::fs::write(d.join("u.md"), [0xff, 0xfe, 0x41]).unwrap(),
            "u.md",
        ),
        (
            "notes.txt",
            |d| std::fs::write(d.join("notes.txt"), "x").unwrap(),
            "notes.txt",
        ),
        (
            "unicode",
            |d| std::fs::write(d.join("règles.md"), "Rule.\n").unwrap(),
            "règles.md",
        ),
        (
            "subdir",
            |d| std::fs::create_dir(d.join("sub")).unwrap(),
            "sub",
        ),
        (
            "dangling-symlink",
            |d| symlink("/no/such/target.md", d.join("link.md")).unwrap(),
            "link.md",
        ),
        (
            "symlink-to-a-directory",
            |d| {
                // "link.md" sorts before "zz-target", so the link's own problem is the first one reported.
                std::fs::create_dir(d.join("zz-target")).unwrap();
                symlink(d.join("zz-target"), d.join("link.md")).unwrap();
            },
            "link.md",
        ),
        (
            "dupcase",
            |d| {
                // APFS is case-insensitive and ext4 is not: two names that differ only in case must not both be sent.
                std::fs::write(d.join("a.md"), "A.\n").unwrap();
                std::fs::write(d.join("A.md"), "B.\n").unwrap();
            },
            "duplicate",
        ),
    ];
    for (label, setup, want) in cases {
        let h = Home::new(&format!("rules-bad-{label}"));
        std::fs::create_dir_all(h.rules_dir()).unwrap();
        std::fs::write(h.rules_dir().join("good.md"), "Good.\n").unwrap();
        setup(&h.rules_dir());
        let o = h.run(&["rules", "show"], &[]);
        assert_eq!(code(&o), 1, "{label}: {}", out(&o));
        assert!(err(&o).contains(want), "{label}: {}", err(&o));
        assert!(
            out(&o).is_empty(),
            "{label}: nothing is printed as if it were fine"
        );
    }
}

#[test]
fn hidden_files_such_as_editor_swap_files_are_ignored() {
    let h = Home::new("rules-hidden");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.rules_dir().join("good.md"), "Good.\n").unwrap();
    std::fs::write(h.rules_dir().join(".good.md.swp"), [0u8, 1, 2]).unwrap();
    std::fs::write(h.rules_dir().join(".DS_Store"), [0u8, 1, 2]).unwrap();
    let o = h.run(&["rules", "show", "--json"], &[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["rules"].as_array().unwrap().len(), 1);
}

#[test]
fn rules_override_reads_a_directory_or_a_single_file_instead_of_the_default() {
    let h = Home::new("rules-override");
    h.run(&["rules", "init"], &[]);
    let dir = h.root.join("project-rules");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("only.md"), "Only.\n").unwrap();
    let o = h.run(
        &["rules", "show", "--rules", dir.to_str().unwrap(), "--json"],
        &[],
    );
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["rules"].as_array().unwrap().len(), 1);
    assert_eq!(v["rules"][0]["path"], "only.md");
    let file = h.root.join("single.md");
    std::fs::write(&file, "Single.\n").unwrap();
    let o = h.run(
        &["rules", "show", "--rules", file.to_str().unwrap(), "--json"],
        &[],
    );
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["rules"][0]["path"], "single.md");
    assert_eq!(v["rules"][0]["content"], "Single.\n");
    // A path that does not exist, and a file with a bad name, are errors that name the path.
    let o = h.run(&["rules", "show", "--rules", "/no/such/place"], &[]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("/no/such/place"), "{}", err(&o));
    let bad = h.root.join("AGENTS.md");
    std::fs::write(&bad, "x\n").unwrap();
    let o = h.run(&["rules", "show", "--rules", bad.to_str().unwrap()], &[]);
    assert_eq!(code(&o), 1);
}

#[test]
fn a_symlinked_rules_file_is_read_through_so_a_dotfile_manager_works() {
    let h = Home::new("rules-symlink");
    let dotfiles = h.root.join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::write(dotfiles.join("tdd.md"), "Linked rule.\n").unwrap();
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    symlink(dotfiles.join("tdd.md"), h.rules_dir().join("tdd.md")).unwrap();
    let o = h.run(&["rules", "show", "--json"], &[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    // The run sends the link's name and the target's content, as a regular file the VM can write.
    assert_eq!(v["rules"][0]["path"], "tdd.md");
    assert_eq!(v["rules"][0]["content"], "Linked rule.\n");
    // The whole rules directory may itself be a link.
    let moved = h.root.join("real-rules");
    std::fs::rename(h.rules_dir(), &moved).unwrap();
    symlink(&moved, h.rules_dir()).unwrap();
    let o = h.run(&["rules", "show", "--json"], &[]);
    assert_eq!(code(&o), 0, "{}", err(&o));
}

#[test]
fn continue_file_override_and_a_bad_continue_file() {
    let h = Home::new("rules-continue");
    h.run(&["rules", "init"], &[]);
    let c = h.root.join("my-continue.md");
    std::fs::write(&c, "---\nmax_nudges: 7\n---\nKeep at it.\n").unwrap();
    let o = h.run(
        &["rules", "show", "--continue", c.to_str().unwrap(), "--json"],
        &[],
    );
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["continue"]["maxNudges"], 7);
    assert_eq!(v["continue"]["text"], "Keep at it.\n");
    std::fs::write(&c, "---\nmax_nudges: many\n---\nX\n").unwrap();
    let o = h.run(&["rules", "show", "--continue", c.to_str().unwrap()], &[]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("max_nudges"), "{}", err(&o));
    // A missing default continue.md is fine for show (no nudge applies); a named missing one is not.
    std::fs::remove_file(h.continue_md()).unwrap();
    let o = h.run(&["rules", "show", "--json"], &[]);
    assert_eq!(code(&o), 0);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert!(v["continue"].is_null());
    let o = h.run(&["rules", "show", "--continue", "/no/such.md"], &[]);
    assert_eq!(code(&o), 1);
}

#[test]
fn edit_opens_the_default_file_in_the_editor_and_reports_the_new_sha() {
    let h = Home::new("rules-edit");
    h.run(&["rules", "init"], &[]);
    let editor = h.script(
        "ed.sh",
        r#"printf '\n- Extra rule from the editor.\n' >> "$1""#,
    );
    let o = h.run(&["rules", "edit"], &[("EDITOR", &editor)]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let now = std::fs::read(h.rules()).unwrap();
    assert!(String::from_utf8_lossy(&now).ends_with("- Extra rule from the editor.\n"));
    assert!(out(&o).contains(&sha(&now)), "{}", out(&o));
    assert!(out(&o).contains("next run"), "{}", out(&o));
    // The same sha is what show (and so the next run) reads.
    let o = h.run(&["rules", "show", "--json"], &[]);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["rules"][0]["sha256"], sha(&now));
    assert_ne!(sha(&now), sha(DEFAULT_RULES.as_bytes()));
}

#[test]
fn edit_without_a_rules_file_starts_from_init() {
    let h = Home::new("rules-edit-fresh");
    let editor = h.script("ed.sh", r#"printf '%s\n' "$1" > "$0.arg""#);
    let o = h.run(&["rules", "edit"], &[("EDITOR", &editor)]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(h.rules()).unwrap(), DEFAULT_RULES);
    let arg = std::fs::read_to_string(h.root.join("ed.sh.arg")).unwrap();
    assert_eq!(arg.trim(), h.rules().display().to_string());
}

#[test]
fn edit_can_name_another_rules_file_or_the_nudge_file() {
    let h = Home::new("rules-edit-named");
    h.run(&["rules", "init"], &[]);
    let editor = h.script("ed.sh", r#"printf 'Named rule.\n' > "$1""#);
    let o = h.run(&["rules", "edit", "team.md"], &[("EDITOR", &editor)]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(
        std::fs::read_to_string(h.rules_dir().join("team.md")).unwrap(),
        "Named rule.\n"
    );
    let editor = h.script(
        "ed2.sh",
        r#"printf -- '---\nmax_nudges: 1\n---\nGo.\n' > "$1""#,
    );
    let o = h.run(&["rules", "edit", "continue"], &[("EDITOR", &editor)]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        std::fs::read_to_string(h.continue_md())
            .unwrap()
            .contains("max_nudges: 1")
    );
    // A name that could leave the rules directory or hit a reserved name never reaches the editor.
    let marker = h.root.join("editor-ran");
    let editor = h.script("ed3.sh", &format!("mkdir {}", marker.display()));
    for bad in ["../x.md", "a/b.md", "AGENTS.md", "x.txt", "règles.md"] {
        let o = h.run(&["rules", "edit", bad], &[("EDITOR", &editor)]);
        assert_eq!(code(&o), 1, "{bad}");
        assert!(!marker.exists(), "{bad}: the editor must not run");
    }
}

#[test]
fn edit_that_leaves_an_invalid_file_exits_1_and_says_which_rule_is_wrong() {
    let h = Home::new("rules-edit-bad");
    h.run(&["rules", "init"], &[]);
    let editor = h.script("ed.sh", r#": > "$1""#);
    let o = h.run(&["rules", "edit"], &[("EDITOR", &editor)]);
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("00-my-rules.md") && err(&o).contains("empty"),
        "{}",
        err(&o)
    );
    assert!(
        err(&o).contains("ham rules edit"),
        "says how to fix it: {}",
        err(&o)
    );
}

#[test]
fn edit_reports_an_editor_that_fails_or_cannot_start() {
    let h = Home::new("rules-edit-fail");
    h.run(&["rules", "init"], &[]);
    let fails = h.script("fail.sh", "exit 3");
    let o = h.run(&["rules", "edit"], &[("EDITOR", &fails)]);
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("editor") && err(&o).contains('3'),
        "{}",
        err(&o)
    );
    assert_eq!(std::fs::read_to_string(h.rules()).unwrap(), DEFAULT_RULES);
    let o = h.run(
        &["rules", "edit"],
        &[("EDITOR", Path::new("/no/such/editor"))],
    );
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("/no/such/editor"), "{}", err(&o));
}

#[test]
fn edit_prefers_visual_then_editor_and_accepts_arguments_and_quotes() {
    let h = Home::new("rules-edit-which");
    h.run(&["rules", "init"], &[]);
    // Both scripts append to the LAST argument, so a flag before the file is fine.
    let visual = h.script(
        "visual.sh",
        r#"for f; do :; done; printf 'visual\n' >> "$f""#,
    );
    let editor = h.script(
        "editor.sh",
        r#"for f; do :; done; printf 'editor\n' >> "$f""#,
    );
    let o = h.run(
        &["rules", "edit"],
        &[("VISUAL", &visual), ("EDITOR", &editor)],
    );
    assert_eq!(code(&o), 0, "{}", err(&o));
    let text = std::fs::read_to_string(h.rules()).unwrap();
    assert!(
        text.ends_with("visual\n") && !text.contains("editor\n"),
        "VISUAL wins: {text:?}"
    );
    let with_flag = format!("{} --wait", editor.display());
    let o = h.run(&["rules", "edit"], &[("EDITOR", Path::new(&with_flag))]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        std::fs::read_to_string(h.rules())
            .unwrap()
            .ends_with("editor\n"),
        "EDITOR with an argument"
    );
    // A path with a space is quoted by the user, as in git.
    let spaced = h.root.join("my editor.sh");
    std::fs::write(&spaced, "#!/bin/sh\nprintf 'spaced\\n' >> \"$1\"\n").unwrap();
    std::fs::set_permissions(&spaced, std::fs::Permissions::from_mode(0o755)).unwrap();
    let quoted = format!("'{}'", spaced.display());
    let o = h.run(&["rules", "edit"], &[("EDITOR", Path::new(&quoted))]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        std::fs::read_to_string(h.rules())
            .unwrap()
            .ends_with("spaced\n")
    );
}

#[test]
fn the_rules_commands_need_no_config_or_credentials_and_usage_errors_exit_2() {
    let h = Home::new("rules-usage");
    for args in [
        &["rules"][..],
        &["rules", "bogus"],
        &["rules", "init", "x"],
        &["rules", "show", "--nope"],
    ] {
        let o = h.run(args, &[]);
        assert_eq!(code(&o), 2, "{args:?}: {}", err(&o));
        assert!(err(&o).starts_with("usage: "), "{}", err(&o));
    }
    assert!(h.tree().is_empty());
}
