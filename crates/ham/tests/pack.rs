//! The lifecycle pack on the user's machine: `ham rules init --pack lifecycle [--skills|--skills-as-rules]` adds the
//! contract (and the skills) beside the user's rules, `ham lifecycle init <dir>` copies the pack into a repository, and a run sends what
//! was installed. The real `ham` binary in a temp HOME; no network and no AWS credentials.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::pack;
use ham_core::rules::{DEFAULT_RULES, MAX_RULE_FILES};
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
    fn skills_dir(&self) -> PathBuf {
        self.root.join(".config/ham/skills")
    }
    fn contract(&self) -> PathBuf {
        self.rules_dir().join("10-lifecycle.md")
    }
    fn run(&self, args: &[&str]) -> Output {
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
        cmd.stdin(std::process::Stdio::null()).output().unwrap()
    }
}

/// Every path under `root`, relative, sorted; a symbolic link is listed with its target.
fn tree(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let mut es: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let ft = e.file_type().unwrap();
            if ft.is_symlink() {
                out.push(format!(
                    "{rel} -> {}",
                    std::fs::read_link(&p).unwrap().display()
                ));
            } else {
                out.push(rel);
                if ft.is_dir() {
                    walk(&p, root, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
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

/// The folders of the skills folder, sorted; none when the folder is absent.
fn skill_folders(h: &Home) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(h.skills_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The names of the seven pack skills, sorted.
fn pack_skill_names() -> Vec<String> {
    let mut names = pack::skill_names();
    names.sort();
    names
}

/// A skill of the user's own: a valid folder the pack does not own.
fn own_skill(h: &Home, name: &str) {
    let dir = h.skills_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Mine.\n---\nDo my thing.\n"),
    )
    .unwrap();
}

// ---- ham rules init --pack lifecycle (route T0a) ------------------------------------------------------------------------------

#[test]
fn a_first_install_adds_the_contract_beside_the_default_rules() {
    let h = Home::new("pack-first");
    let o = h.run(&["rules", "init", "--pack", "lifecycle"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    assert_eq!(
        std::fs::read_to_string(h.rules_dir().join("00-my-rules.md")).unwrap(),
        DEFAULT_RULES,
        "a first install also writes the user's own default rules, as `rules init` does"
    );
    let text = out(&o);
    assert!(
        text.contains("created") && text.contains("10-lifecycle.md"),
        "{text}"
    );
    assert!(
        text.contains(&pack::version()),
        "names the pack version: {text}"
    );
    assert!(
        text.contains(&pack::digest()[..12]),
        "names the pack digest: {text}"
    );
    // `--pack lifecycle` is the pack's default install, the same as a bare `rules init`: the contract
    // and the seven skills. The contract alone is `--no-skills` (below).
    assert_eq!(
        skill_folders(&h).len(),
        7,
        "the pack's default install carries the skills"
    );
}

#[test]
fn installing_twice_changes_nothing() {
    let h = Home::new("pack-twice");
    assert_eq!(code(&h.run(&["rules", "init", "--pack", "lifecycle"])), 0);
    let before = tree(&h.root);
    let mtime = std::fs::metadata(h.contract()).unwrap().modified().unwrap();
    let o = h.run(&["rules", "init", "--pack", "lifecycle"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("kept") && out(&o).contains("already"),
        "{}",
        out(&o)
    );
    assert_eq!(tree(&h.root), before);
    assert_eq!(
        std::fs::metadata(h.contract()).unwrap().modified().unwrap(),
        mtime
    );
}

#[test]
fn an_edited_contract_is_kept_and_the_packs_text_is_named_by_its_hash() {
    let h = Home::new("pack-edited");
    assert_eq!(code(&h.run(&["rules", "init", "--pack", "lifecycle"])), 0);
    std::fs::write(h.contract(), "# My own lifecycle\n\nTests first.\n").unwrap();
    let o = h.run(&["rules", "init", "--pack", "lifecycle"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        "# My own lifecycle\n\nTests first.\n"
    );
    let text = out(&o);
    assert!(text.contains("kept") && text.contains("edited"), "{text}");
    assert!(text.contains(&sha(pack::CONTRACT.as_bytes())), "{text}");
}

#[test]
fn show_lists_the_contract_with_its_sha256_after_the_install() {
    let h = Home::new("pack-show");
    assert_eq!(code(&h.run(&["rules", "init", "--pack", "lifecycle"])), 0);
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let text = out(&o);
    assert!(text.contains("10-lifecycle.md"), "{text}");
    assert!(text.contains(&sha(pack::CONTRACT.as_bytes())), "{text}");
    let j: Value = serde_json::from_str(&out(&h.run(&["rules", "show", "--json"]))).unwrap();
    let names: Vec<&str> = j["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["00-my-rules.md", "10-lifecycle.md"]);
    assert_eq!(j["rules"][1]["sha256"], sha(pack::CONTRACT.as_bytes()));
}

#[test]
fn an_install_that_would_pass_the_file_limit_writes_nothing() {
    let h = Home::new("pack-full");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    for i in 0..MAX_RULE_FILES {
        std::fs::write(h.rules_dir().join(format!("r{i:02}.md")), "rule\n").unwrap();
    }
    let before = tree(&h.root);
    let o = h.run(&["rules", "init", "--pack", "lifecycle"]);
    assert_eq!(code(&o), 1, "{}{}", out(&o), err(&o));
    assert!(
        err(&o).contains(&format!("limit is {MAX_RULE_FILES}")),
        "{}",
        err(&o)
    );
    assert!(!h.contract().exists());
    assert_eq!(
        tree(&h.root),
        before,
        "the whole install is planned first: nothing was written"
    );
}

#[test]
fn a_user_file_with_the_name_of_a_pack_rule_is_never_overwritten() {
    let h = Home::new("pack-name");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.contract(), "Mine, from before the pack existed.\n").unwrap();
    let o = h.run(&["rules", "init", "--pack", "lifecycle"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        "Mine, from before the pack existed.\n"
    );
}

#[test]
fn the_skills_as_rules_option_adds_seven_valid_rule_files_in_step_order() {
    let h = Home::new("pack-as-rules");
    let o = h.run(&["rules", "init", "--pack", "lifecycle", "--skills-as-rules"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let want: Vec<String> = pack::skills_as_rules()
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(want.len(), 7);
    for name in &want {
        let on_disk = std::fs::read_to_string(h.rules_dir().join(name)).unwrap();
        assert!(!on_disk.starts_with("---"), "{name}: frontmatter removed");
        assert!(on_disk.starts_with("# lifecycle-"), "{name}");
    }
    let j: Value = serde_json::from_str(&out(&h.run(&["rules", "show", "--json"]))).unwrap();
    let names: Vec<String> = j["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    let mut expect = vec!["00-my-rules.md".to_string(), "10-lifecycle.md".to_string()];
    expect.extend(want);
    assert_eq!(names, expect);
    assert!(!h.skills_dir().exists(), "as rules means no skills folder");
    // Repeating it changes nothing.
    let before = tree(&h.root);
    assert_eq!(
        code(&h.run(&["rules", "init", "--pack", "lifecycle", "--skills-as-rules"])),
        0
    );
    assert_eq!(tree(&h.root), before);
}

// ---- the default of `ham rules init` is the lifecycle pack's contract ---------------------------------

#[test]
fn plain_rules_init_installs_the_contract_beside_the_default_rules_and_pack_none_does_not() {
    let h = Home::new("pack-default");
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    assert_eq!(
        std::fs::read_to_string(h.rules_dir().join("00-my-rules.md")).unwrap(),
        DEFAULT_RULES
    );
    assert!(
        out(&o).contains(&pack::version()) && out(&o).contains("10-lifecycle.md"),
        "{}",
        out(&o)
    );
    assert_eq!(
        skill_folders(&h).len(),
        7,
        "a bare `rules init` also installs the seven skills"
    );
    // Repeating it changes nothing.
    let before = tree(&h.root);
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    assert_eq!(tree(&h.root), before);

    // `--pack none` is the pointer-only default of before: the default rules file and the nudge, no contract.
    let n = Home::new("pack-none");
    let o = n.run(&["rules", "init", "--pack", "none"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(!n.contract().exists());
    assert_eq!(
        tree(&n.root),
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
fn the_pack_options_alone_mean_the_lifecycle_pack_and_none_refuses_them() {
    let h = Home::new("pack-opts");
    let o = h.run(&["rules", "init", "--skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    assert_eq!(std::fs::read_dir(h.skills_dir()).unwrap().count(), 7);
    let n = Home::new("pack-none-opts");
    for flag in ["--skills", "--skills-as-rules"] {
        let o = n.run(&["rules", "init", "--pack", "none", flag]);
        assert_eq!(code(&o), 2, "{flag}: {}{}", out(&o), err(&o));
        assert!(err(&o).starts_with("usage: "), "{}", err(&o));
    }
    assert!(tree(&n.root).is_empty(), "a usage error writes nothing");
}

#[test]
fn a_directory_of_your_own_rules_still_gets_the_contract_but_not_the_default_file() {
    let h = Home::new("pack-own");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.rules_dir().join("team.md"), "Team rule.\n").unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(h.contract().exists());
    assert!(!h.rules_dir().join("00-my-rules.md").exists());
    assert_eq!(
        std::fs::read_to_string(h.rules_dir().join("team.md")).unwrap(),
        "Team rule.\n"
    );
}

// ---- ham rules init --pack lifecycle --skills (route T1) ------------------------------------------------------------------------

#[test]
fn the_skills_option_writes_seven_skill_folders_with_the_packs_bytes() {
    let h = Home::new("pack-skills");
    let o = h.run(&["rules", "init", "--pack", "lifecycle", "--skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for s in pack::skills() {
        for f in &s.files {
            let p = h.skills_dir().join(&s.name).join(&f.path);
            assert_eq!(
                std::fs::read_to_string(&p).unwrap(),
                f.content,
                "{}",
                p.display()
            );
        }
    }
    let dirs: Vec<String> = std::fs::read_dir(h.skills_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(dirs.len(), 7, "{dirs:?}");
    let script = h
        .skills_dir()
        .join("lifecycle-check/scripts/lifecycle_check.py");
    assert!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o100 != 0,
        "the script is executable"
    );
    assert!(out(&o).contains("7 skills"), "{}", out(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT,
        "the contract comes with the skills"
    );
}

#[test]
fn an_edited_skill_file_is_kept_and_the_rest_is_still_installed() {
    let h = Home::new("pack-skill-edit");
    assert_eq!(
        code(&h.run(&["rules", "init", "--pack", "lifecycle", "--skills"])),
        0
    );
    let tdd = h.skills_dir().join("lifecycle-tdd/SKILL.md");
    std::fs::write(
        &tdd,
        "---\nname: lifecycle-tdd\ndescription: Mine.\n---\nMy test-first rules.\n",
    )
    .unwrap();
    let gone = h.skills_dir().join("lifecycle-loop/SKILL.md");
    std::fs::remove_file(&gone).unwrap();
    let o = h.run(&["rules", "init", "--pack", "lifecycle", "--skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        std::fs::read_to_string(&tdd)
            .unwrap()
            .contains("My test-first rules.")
    );
    assert!(gone.exists(), "a missing file is written again");
    assert!(
        out(&o).contains("kept") && out(&o).contains("lifecycle-tdd"),
        "{}",
        out(&o)
    );
}

#[test]
fn a_skills_folder_that_is_a_symbolic_link_is_not_written_through() {
    let h = Home::new("pack-skill-link");
    let elsewhere = support::scratch("pack-skill-elsewhere");
    std::fs::create_dir_all(h.root.join(".config/ham")).unwrap();
    symlink(&elsewhere, h.skills_dir()).unwrap();
    let o = h.run(&["rules", "init", "--pack", "lifecycle", "--skills"]);
    assert_eq!(code(&o), 1, "{}{}", out(&o), err(&o));
    assert!(err(&o).contains("symbolic link"), "{}", err(&o));
    assert!(
        tree(&elsewhere).is_empty(),
        "nothing was written through the link"
    );
}

// ---- the default of `ham rules init` carries the skills --------------------------------------------------
//
// "did we include my skills in this harness - by default?" The measured configuration is the contract plus the seven skills
// (the RealSkills arm); a bare `ham rules init`, which `setup.sh` runs, now writes both. `--no-skills` is the contract alone (the old default).

/// Every file below `root` with its relative path and bytes, sorted.
fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.push((
                    p.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn a_bare_init_writes_the_seven_skill_folders_with_the_bytes_of_lifecycle_pack() {
    let h = Home::new("default-skills");
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(skill_folders(&h), pack_skill_names());
    // Every file of lifecycle/pack/.agents/skills is there with its bytes, and nothing else.
    let want = files_under(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lifecycle/pack/.agents/skills"),
    );
    assert!(
        want.len() >= 8,
        "seven SKILL.md and the self-check script: {:?}",
        want.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    assert_eq!(
        files_under(&h.skills_dir()),
        want,
        "the installed skills are the pack's files, byte for byte"
    );
    for name in pack_skill_names() {
        assert!(
            h.skills_dir().join(&name).join("SKILL.md").is_file(),
            "{name} has its SKILL.md"
        );
    }
    let script = h
        .skills_dir()
        .join("lifecycle-check/scripts/lifecycle_check.py");
    assert!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o100 != 0,
        "the check script is executable"
    );
    // The contract and the default rules come as before.
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    assert!(h.rules_dir().join("00-my-rules.md").is_file());
    // Nothing is written outside the temp HOME's config folder.
    let top: Vec<String> = std::fs::read_dir(&h.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(top, [".config"], "only the config folder: {top:?}");
}

#[test]
fn a_bare_init_names_each_skill_it_wrote() {
    let h = Home::new("default-skills-out");
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let text = out(&o);
    assert!(text.contains("7 skills"), "{text}");
    for name in pack_skill_names() {
        let line = text
            .lines()
            .find(|l| l.contains(&name))
            .unwrap_or_else(|| panic!("a line for {name}: {text}"));
        assert!(line.contains("created"), "{line}");
    }
}

/// The two reference files of the delivery skill, as the pack holds them.
const DOX_REFERENCES: [(&str, &str); 2] = [
    (
        "lifecycle-delivery/references/dox-root.md",
        ".agents/skills/lifecycle-delivery/references/dox-root.md",
    ),
    (
        "lifecycle-delivery/references/dox-notice.md",
        ".agents/skills/lifecycle-delivery/references/dox-notice.md",
    ),
];

fn pack_bytes(path: &str) -> &'static str {
    pack::files()
        .iter()
        .find(|f| f.path == path)
        .unwrap()
        .content
}

#[test]
fn a_bare_init_writes_the_two_delivery_references_with_the_pack_bytes() {
    // Pack 1.0.8 (DOX): the starter is the user's own default for every new project, so it is an ordinary editable file (not read-only, not executable).
    let h = Home::new("dox-references");
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for (installed, in_pack) in DOX_REFERENCES {
        let path = h.skills_dir().join(installed);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            pack_bytes(in_pack),
            "{installed} has the pack's bytes"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert!(
            mode & 0o200 != 0,
            "{installed} is writable by its owner: {mode:o}"
        );
        assert!(mode & 0o111 == 0, "{installed} is not executable: {mode:o}");
    }
    let text = out(&o);
    assert!(
        text.lines()
            .any(|l| l.contains("created lifecycle-delivery") && l.contains("(3 files)")),
        "the output counts the delivery skill's three files: {text}"
    );
}

#[test]
fn an_edited_starter_is_kept_by_a_second_init_and_the_output_says_so() {
    // The starter is the override: the user's text in that file wins, and `rules init` never replaces it.
    let h = Home::new("dox-starter-edited");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let starter = h
        .skills_dir()
        .join("lifecycle-delivery/references/dox-root.md");
    assert!(starter.is_file(), "the first init wrote the starter");
    std::fs::write(&starter, "DOX off\n").unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(&starter).unwrap(), "DOX off\n");
    assert!(
        out(&o)
            .lines()
            .any(|l| l.contains("dox-root.md") && l.contains("kept") && l.contains("edited")),
        "{}",
        out(&o)
    );
}

#[test]
fn no_skills_writes_no_reference_files() {
    // Control first: the default install writes them (so the absence below is the flag's doing, not an empty pack).
    let control = Home::new("no-refs-control");
    assert_eq!(code(&control.run(&["rules", "init"])), 0);
    for (installed, _) in DOX_REFERENCES {
        assert!(
            control.skills_dir().join(installed).is_file(),
            "{installed}"
        );
    }
    let h = Home::new("no-refs-no-skills");
    assert_eq!(code(&h.run(&["rules", "init", "--no-skills"])), 0);
    assert!(
        !h.skills_dir().exists(),
        "no skills folder, so no reference files"
    );
    for (installed, _) in DOX_REFERENCES {
        assert!(!h.skills_dir().join(installed).exists(), "{installed}");
    }
}

#[test]
fn pack_none_writes_no_reference_files() {
    let control = Home::new("pack-none-refs-control");
    assert_eq!(code(&control.run(&["rules", "init"])), 0);
    assert!(
        control
            .skills_dir()
            .join("lifecycle-delivery/references/dox-root.md")
            .is_file()
    );
    let h = Home::new("pack-none-refs");
    assert_eq!(code(&h.run(&["rules", "init", "--pack", "none"])), 0);
    assert!(!h.skills_dir().exists(), "--pack none installs no skills");
    for (installed, _) in DOX_REFERENCES {
        assert!(!h.skills_dir().join(installed).exists(), "{installed}");
    }
}

#[test]
fn no_skills_writes_the_contract_and_the_default_rules_and_no_skills() {
    for args in [
        vec!["rules", "init", "--no-skills"],
        vec!["rules", "init", "--pack", "lifecycle", "--no-skills"],
    ] {
        let h = Home::new("no-skills");
        let o = h.run(&args);
        assert_eq!(code(&o), 0, "{args:?}: {}{}", out(&o), err(&o));
        assert_eq!(
            std::fs::read_to_string(h.contract()).unwrap(),
            pack::CONTRACT,
            "{args:?}"
        );
        assert!(h.rules_dir().join("00-my-rules.md").is_file(), "{args:?}");
        assert!(h.root.join(".config/ham/continue.md").is_file(), "{args:?}");
        assert!(!h.skills_dir().exists(), "{args:?}: no skills folder");
        assert!(!out(&o).contains("7 skills"), "{args:?}: {}", out(&o));
        // Repeating it changes nothing.
        let before = tree(&h.root);
        assert_eq!(code(&h.run(&args)), 0);
        assert_eq!(tree(&h.root), before, "{args:?}");
    }
}

#[test]
fn pack_none_writes_neither_the_contract_nor_the_skills() {
    let want = [
        ".config",
        ".config/ham",
        ".config/ham/continue.md",
        ".config/ham/review.md",
        ".config/ham/rules",
        ".config/ham/rules/00-my-rules.md",
    ];
    for args in [
        vec!["rules", "init", "--pack", "none"],
        // `--no-skills` says what `--pack none` already does.
        vec!["rules", "init", "--pack", "none", "--no-skills"],
    ] {
        let h = Home::new("pack-none-skills");
        let o = h.run(&args);
        assert_eq!(code(&o), 0, "{args:?}: {}{}", out(&o), err(&o));
        assert_eq!(tree(&h.root), want, "{args:?}");
    }
}

#[test]
fn skills_and_no_skills_together_are_a_usage_error_that_writes_nothing() {
    let h = Home::new("skills-vs-no-skills");
    for args in [
        vec!["rules", "init", "--skills", "--no-skills"],
        vec![
            "rules",
            "init",
            "--pack",
            "lifecycle",
            "--no-skills",
            "--skills",
        ],
    ] {
        let o = h.run(&args);
        assert_eq!(code(&o), 2, "{args:?}: {}{}", out(&o), err(&o));
        assert!(err(&o).starts_with("usage: "), "{}", err(&o));
        assert!(
            err(&o).contains("--skills") && err(&o).contains("--no-skills"),
            "{}",
            err(&o)
        );
    }
    assert!(tree(&h.root).is_empty(), "a usage error writes nothing");
}

#[test]
fn the_skills_flag_still_means_the_same_and_skills_as_rules_still_writes_no_folder() {
    // `--skills` is accepted (documented commands keep working) and writes what a bare init writes.
    let a = Home::new("skills-flag");
    assert_eq!(code(&a.run(&["rules", "init", "--skills"])), 0);
    let b = Home::new("bare-flag");
    assert_eq!(code(&b.run(&["rules", "init"])), 0);
    assert_eq!(tree(&a.root), tree(&b.root));
    // `--skills-as-rules` alone is the measurement arm "skill bodies as rules": the contract, seven rule files, and no skills folder.
    let c = Home::new("as-rules-only");
    assert_eq!(code(&c.run(&["rules", "init", "--skills-as-rules"])), 0);
    assert!(!c.skills_dir().exists());
    assert_eq!(pack::skills_as_rules().len(), 7);
    // `--no-skills` beside it says the same.
    let d = Home::new("as-rules-no-skills");
    assert_eq!(
        code(&d.run(&["rules", "init", "--skills-as-rules", "--no-skills"])),
        0
    );
    assert_eq!(tree(&d.root), tree(&c.root));
    // Both asked for: both written.
    let e = Home::new("as-rules-and-skills");
    assert_eq!(
        code(&e.run(&["rules", "init", "--skills-as-rules", "--skills"])),
        0
    );
    assert_eq!(skill_folders(&e).len(), 7);
    assert!(e.rules_dir().join("21-lifecycle-research.md").is_file());
}

#[test]
fn a_second_bare_init_keeps_edited_files_adds_the_missing_and_lists_both() {
    let h = Home::new("default-twice");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let first = tree(&h.root);
    // The user edits one skill and the contract, and loses one skill file.
    let tdd = h.skills_dir().join("lifecycle-tdd/SKILL.md");
    std::fs::write(
        &tdd,
        "---\nname: lifecycle-tdd\ndescription: Mine.\n---\nMy test-first rules.\n",
    )
    .unwrap();
    std::fs::write(h.contract(), "# My own lifecycle\n").unwrap();
    let gone = h.skills_dir().join("lifecycle-loop/SKILL.md");
    std::fs::remove_file(&gone).unwrap();
    let tdd_mtime = std::fs::metadata(&tdd).unwrap().modified().unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let text = out(&o);
    assert!(
        std::fs::read_to_string(&tdd)
            .unwrap()
            .contains("My test-first rules."),
        "an edited skill file is kept"
    );
    assert_eq!(
        std::fs::metadata(&tdd).unwrap().modified().unwrap(),
        tdd_mtime
    );
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        "# My own lifecycle\n",
        "an edited contract is kept"
    );
    assert!(gone.is_file(), "a missing skill file is written again");
    assert!(
        text.lines()
            .any(|l| l.contains("kept") && l.contains("lifecycle-tdd") && l.contains("edited")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("created") && l.contains("lifecycle-loop")),
        "{text}"
    );
    assert_eq!(
        tree(&h.root),
        first,
        "the same set of files as the first run"
    );
    // A third run has nothing to write: no line says created.
    let third = h.run(&["rules", "init"]);
    assert_eq!(code(&third), 0);
    assert!(
        !out(&third).lines().any(|l| l.starts_with("created")),
        "{}",
        out(&third)
    );
}

#[test]
fn a_bare_init_in_a_folder_from_before_adds_the_skills_and_changes_nothing_else() {
    // A user who ran an older `ham rules init`: their rules, an edited contract, a nudge file, no skills folder.
    let h = Home::new("default-before");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.rules_dir().join("00-my-rules.md"), "My rules.\n").unwrap();
    std::fs::write(h.rules_dir().join("team.md"), "Team rule.\n").unwrap();
    std::fs::write(h.contract(), "# An older lifecycle\n").unwrap();
    std::fs::write(h.root.join(".config/ham/continue.md"), "Keep going.\n").unwrap();
    let keep: Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> = [
        h.rules_dir().join("00-my-rules.md"),
        h.rules_dir().join("team.md"),
        h.contract(),
        h.root.join(".config/ham/continue.md"),
    ]
    .into_iter()
    .map(|p| {
        let m = std::fs::metadata(&p).unwrap().modified().unwrap();
        (p.clone(), std::fs::read(&p).unwrap(), m)
    })
    .collect();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        skill_folders(&h),
        pack_skill_names(),
        "the missing skills are added"
    );
    for (p, bytes, mtime) in keep {
        assert_eq!(std::fs::read(&p).unwrap(), bytes, "{}", p.display());
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime,
            "{}",
            p.display()
        );
    }
    let rules: Vec<String> = std::fs::read_dir(h.rules_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(rules.len(), 3, "no rule file was added: {rules:?}");
}

#[test]
fn twenty_five_skills_of_yours_fit_beside_the_seven_and_twenty_six_do_not() {
    // Twenty-five of yours and the pack's seven make the thirty-two a run takes.
    let own = |h: &Home, n: usize| {
        for i in 0..n {
            own_skill(h, &format!("my-skill-{i:02}"));
        }
    };
    let fits = Home::new("room-fits");
    own(&fits, 25);
    let o = fits.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(skill_folders(&fits).len(), 32, "{:?}", skill_folders(&fits));

    // Twenty-six of yours leave no room for all seven. A bare init must not turn a working setup into one whose every run is refused: it installs the
    // contract, leaves the skills folder as it is, and says why and what to do.
    let two = Home::new("room-two");
    own(&two, 26);
    let o = two.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(skill_folders(&two).len(), 26, "no pack skill was written");
    assert_eq!(
        std::fs::read_to_string(two.contract()).unwrap(),
        pack::CONTRACT,
        "the contract is installed"
    );
    let text = out(&o);
    assert!(
        text.contains("my-skill-00") && text.contains("my-skill-25"),
        "names your skills: {text}"
    );
    assert!(
        text.contains("at most 32") && text.contains("--no-skills"),
        "says the limit and how to ask for the contract alone: {text}"
    );
}

#[test]
fn skills_asked_for_beside_twenty_six_of_yours_is_refused_before_anything_is_written() {
    let h = Home::new("room-explicit");
    for i in 0..26 {
        own_skill(&h, &format!("my-skill-{i:02}"));
    }
    let before = tree(&h.root);
    let o = h.run(&["rules", "init", "--skills"]);
    assert_eq!(code(&o), 1, "{}{}", out(&o), err(&o));
    let e = err(&o);
    assert!(
        e.contains("33 skills") && e.contains("at most 32"),
        "names the count and the limit: {e}"
    );
    assert!(
        e.contains("my-skill-00") && e.contains("--no-skills"),
        "names your skills and the contract-only install: {e}"
    );
    assert_eq!(tree(&h.root), before, "the whole install is planned first");
}

#[test]
fn a_bare_init_beside_a_linked_skills_folder_installs_the_contract_and_says_so() {
    let h = Home::new("default-link");
    let elsewhere = support::scratch("default-link-elsewhere");
    std::fs::create_dir_all(h.root.join(".config/ham")).unwrap();
    symlink(&elsewhere, h.skills_dir()).unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(h.contract().is_file());
    assert!(
        tree(&elsewhere).is_empty(),
        "nothing written through the link"
    );
    assert!(out(&o).contains("symbolic link"), "{}", out(&o));
}

// ---- ham lifecycle init <dir> (route T2) ----------------------------------------------------------------------------------------

#[test]
fn lifecycle_init_into_an_empty_folder_creates_the_contract_and_seven_skill_folders() {
    let h = Home::new("life-empty");
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
        pack::CONTRACT
    );
    let skills: Vec<String> = std::fs::read_dir(repo.join(".agents/skills"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(skills.len(), 7, "{skills:?}");
    for f in pack::files() {
        assert_eq!(
            std::fs::read_to_string(repo.join(f.path)).unwrap(),
            f.content,
            "{}",
            f.path
        );
    }
    for rel in [
        ".agents/skills/lifecycle-delivery/references/dox-root.md",
        ".agents/skills/lifecycle-delivery/references/dox-notice.md",
    ] {
        assert!(
            repo.join(rel).is_file(),
            "{rel} is in the folder `lifecycle init` fills"
        );
    }
    let text = out(&o);
    assert!(
        text.contains(&pack::version()) && text.contains(&pack::digest()[..12]),
        "{text}"
    );
    assert!(
        std::fs::metadata(repo.join(".agents/skills/lifecycle-check/scripts/lifecycle_check.py"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100
            != 0,
        "the script is executable"
    );
}

#[test]
fn the_installed_self_check_script_runs_in_a_new_repository() {
    let h = Home::new("life-script");
    let repo = h.root.join("repo");
    assert_eq!(
        code(&h.run(&["lifecycle", "init", repo.to_str().unwrap()])),
        0,
        "init"
    );
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("HOME", &h.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    assert!(git(&["add", "-A"]).status.success());
    let c = git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.test",
        "commit",
        "-q",
        "-m",
        "Install the lifecycle pack\n\nThe pack files only; no code yet.",
    ]);
    assert!(c.status.success(), "{}", String::from_utf8_lossy(&c.stderr));
    let script = repo.join(".agents/skills/lifecycle-check/scripts/lifecycle_check.py");
    let o = Command::new("python3")
        .arg("-I")
        .arg(&script)
        .arg("--repo")
        .arg(&repo)
        .env("HOME", &h.root)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    // A repository that holds only the pack has done none of the work: the script runs from where `lifecycle init` put it and answers with a
    // verdict (exit 1, every check named), not with a crash.
    assert_eq!(
        o.status.code(),
        Some(1),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("LIFECYCLE-CHECK FAIL") && text.contains("LC-1"),
        "{text}"
    );
    assert!(
        o.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn lifecycle_init_into_an_empty_folder_passes_the_structure_mode_of_the_installed_self_check() {
    // The folder `ham lifecycle init` fills passes the mode of the self-check that needs no records (no git here either), and the
    // verdict names its own last line, which is not the line the harness reads as the self-check.
    let h = Home::new("life-structure");
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert_eq!(
        code(&h.run(&["lifecycle", "init", repo.to_str().unwrap()])),
        0,
        "init"
    );
    assert!(!repo.join(".git").exists());
    let script = repo.join(".agents/skills/lifecycle-check/scripts/lifecycle_check.py");
    let run = |extra: &[&str]| {
        Command::new("python3")
            .arg("-I")
            .arg(&script)
            .arg("--repo")
            .arg(&repo)
            .args(extra)
            .env("HOME", &h.root)
            .output()
            .unwrap()
    };
    let o = run(&["--structure"]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.trim_end()
            .ends_with("LIFECYCLE-STRUCTURE PASS pass=3 fail=0"),
        "{text}"
    );
    assert!(!text.contains("LIFECYCLE-CHECK"), "{text}");
    // A folder missing a skill is told so.
    std::fs::remove_dir_all(repo.join(".agents/skills/lifecycle-loop")).unwrap();
    let o = run(&["--structure"]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(
        text.contains("ST-2 skills FAIL") && text.contains("lifecycle-loop"),
        "{text}"
    );
}

#[test]
fn lifecycle_init_creates_the_directory_and_repeating_it_changes_nothing() {
    let h = Home::new("life-new");
    let repo = h.root.join("a/b/repo");
    assert_eq!(
        code(&h.run(&["lifecycle", "init", repo.to_str().unwrap()])),
        0
    );
    let before = tree(&repo);
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("kept") && out(&o).contains("already"),
        "{}",
        out(&o)
    );
    assert_eq!(tree(&repo), before);
}

#[test]
fn a_repository_that_has_its_own_agents_md_keeps_it_and_is_told_the_contract_is_not_in_it() {
    let h = Home::new("life-own");
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("AGENTS.md"),
        "# Our repository\n\nRun make test.\n",
    )
    .unwrap();
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
        "# Our repository\n\nRun make test.\n"
    );
    let text = out(&o);
    assert!(
        text.contains("kept") && text.contains("AGENTS.md"),
        "{text}"
    );
    assert!(
        text.contains("not in it") || text.contains("is not installed"),
        "says the contract did not go in: {text}"
    );
    assert!(
        repo.join(".agents/skills/lifecycle-tdd/SKILL.md").exists(),
        "the skills still install"
    );
}

#[test]
fn the_kiro_option_links_kiro_skills_to_the_agents_skills() {
    let h = Home::new("life-kiro");
    let repo = h.root.join("repo");
    assert_eq!(
        code(&h.run(&["lifecycle", "init", repo.to_str().unwrap(), "--kiro"])),
        0
    );
    let link = repo.join(".kiro/skills");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        PathBuf::from("../.agents/skills")
    );
    assert!(
        link.join("lifecycle-tdd/SKILL.md").exists(),
        "the link resolves"
    );
    // Repeating it keeps the link; a real folder where the link belongs is kept and named.
    assert_eq!(
        code(&h.run(&["lifecycle", "init", repo.to_str().unwrap(), "--kiro"])),
        0
    );
    std::fs::remove_file(&link).unwrap();
    std::fs::create_dir_all(&link).unwrap();
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap(), "--kiro"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        link.is_dir() && std::fs::read_link(&link).is_err(),
        "the folder stays"
    );
    assert!(out(&o).contains(".kiro/skills"), "{}", out(&o));
}

#[test]
fn lifecycle_init_refuses_a_file_a_link_and_a_path_with_a_link_inside() {
    let h = Home::new("life-bad");
    let file = h.root.join("file");
    std::fs::write(&file, "x").unwrap();
    let o = h.run(&["lifecycle", "init", file.to_str().unwrap()]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("not a directory"), "{}", err(&o));

    let elsewhere = support::scratch("life-elsewhere");
    let repo = h.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    symlink(&elsewhere, repo.join(".agents")).unwrap();
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap()]);
    assert_eq!(code(&o), 1, "{}{}", out(&o), err(&o));
    assert!(err(&o).contains("symbolic link"), "{}", err(&o));
    assert!(
        tree(&elsewhere).is_empty(),
        "nothing was written through the link"
    );
}

#[test]
fn lifecycle_init_with_json_prints_one_record() {
    let h = Home::new("life-json");
    let repo = h.root.join("repo");
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap(), "--json"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["pack"], "lifecycle");
    assert_eq!(v["version"], pack::version());
    assert_eq!(v["digest"], pack::digest());
    assert_eq!(v["files"].as_array().unwrap().len(), pack::files().len());
    assert_eq!(v["files"][0]["action"], "created");
}

// ---- an install of an earlier pack is brought up to date ---------------------------------------------------
//
// `place_file` used to keep every file that differed from the pack's text, so a machine that had installed pack 1.0.7 kept the 1.0.7 delivery
// skill for ever and never received the DOX starter. `lifecycle/pack.history.json` lists the earlier texts of each pack file; a file in the user's
// own folders (the rules folder and the skills folder) that equals one of them is replaced, and says so. Everything else is kept.
// The earlier texts are the files of `fixtures/lifecycle/pack-earlier/`, taken from git.

const OLD_CONTRACT_1_0_6: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.6-AGENTS.md.txt");
const OLD_DELIVERY_1_0_7: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.7-lifecycle-delivery-SKILL.md.txt");
const OLD_SCRIPT_1_0_7: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.7-lifecycle_check.py.txt");

const OLD_CONTRACT_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-AGENTS.md.txt");
const OLD_DELIVERY_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-lifecycle-delivery-SKILL.md.txt");
const OLD_LOOP_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-lifecycle-loop-SKILL.md.txt");
const OLD_SCRIPT_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-lifecycle_check.py.txt");
const OLD_CONTRACT_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-AGENTS.md.txt");
const OLD_DELIVERY_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-lifecycle-delivery-SKILL.md.txt");
const OLD_LOOP_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-lifecycle-loop-SKILL.md.txt");
const OLD_SCRIPT_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-lifecycle_check.py.txt");

const DELIVERY_PATH: &str = ".agents/skills/lifecycle-delivery/SKILL.md";
const LOOP_PATH: &str = ".agents/skills/lifecycle-loop/SKILL.md";
const SCRIPT_PATH: &str = ".agents/skills/lifecycle-check/scripts/lifecycle_check.py";

/// A machine that ran `ham rules init` with pack 1.0.7: the current install, with the delivery skill and the self-check script as pack 1.0.7
/// wrote them and without the two reference files of the delivery skill, which are newer than that pack.
fn install_as_1_0_7(h: &Home) {
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let skills = h.skills_dir();
    std::fs::write(
        skills.join("lifecycle-delivery/SKILL.md"),
        OLD_DELIVERY_1_0_7,
    )
    .unwrap();
    std::fs::write(
        skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_0_7,
    )
    .unwrap();
    for name in ["dox-root.md", "dox-notice.md"] {
        std::fs::remove_file(skills.join("lifecycle-delivery/references").join(name)).unwrap();
    }
    std::fs::remove_dir(skills.join("lifecycle-delivery/references")).unwrap();
}

#[test]
fn a_bare_init_replaces_an_unedited_1_0_7_delivery_skill_and_says_so() {
    let h = Home::new("upgrade-delivery");
    install_as_1_0_7(&h);
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.skills_dir().join("lifecycle-delivery/SKILL.md")).unwrap(),
        pack_bytes(DELIVERY_PATH),
        "the delivery skill is the pack's"
    );
    let text = out(&o);
    assert!(
        text.lines().any(|l| l
            == "updated lifecycle-delivery/SKILL.md (was pack 1.0.7; not edited by you)"),
        "{text}"
    );
    // The two reference files that 1.0.7 did not have are written beside it, so the DOX starter reaches the machine.
    for (installed, in_pack) in DOX_REFERENCES {
        assert_eq!(
            std::fs::read_to_string(h.skills_dir().join(installed)).unwrap(),
            pack_bytes(in_pack),
            "{installed}"
        );
    }
    assert!(
        text.lines()
            .any(|l| l.starts_with("created lifecycle-delivery")),
        "the new files are named: {text}"
    );
    assert!(
        text.contains("(4 files written)"),
        "two references written and two files replaced: {text}"
    );
    assert!(
        !text.contains("edited by you; not changed"),
        "nothing was kept as edited: {text}"
    );
}

#[test]
fn a_bare_init_replaces_an_unedited_1_0_7_self_check_script_and_keeps_it_executable() {
    let h = Home::new("upgrade-script");
    install_as_1_0_7(&h);
    let script = h
        .skills_dir()
        .join("lifecycle-check/scripts/lifecycle_check.py");
    let before = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
    assert!(
        before & 0o100 != 0,
        "the 1.0.7 install had an executable script"
    );
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(&script).unwrap(),
        pack_bytes(SCRIPT_PATH)
    );
    assert_eq!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        before,
        "the mode of the old file is kept"
    );
    assert!(
        out(&o).lines().any(|l| l
            == "updated lifecycle-check/scripts/lifecycle_check.py (was pack 1.0.7; not edited by you)"),
        "{}",
        out(&o)
    );
    let mut found: Vec<String> = std::fs::read_dir(script.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    found.sort();
    assert_eq!(
        found,
        ["lifecycle_check.py", "scanner_kit.py"],
        "the scripts folder holds the two scripts of the pack and no temporary file"
    );
}

#[test]
fn a_bare_init_keeps_an_edited_delivery_skill_and_says_so() {
    let h = Home::new("upgrade-edited");
    install_as_1_0_7(&h);
    let skill = h.skills_dir().join("lifecycle-delivery/SKILL.md");
    let edited = format!("{OLD_DELIVERY_1_0_7}\nAlso list the open questions.\n");
    std::fs::write(&skill, &edited).unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), edited);
    let text = out(&o);
    assert!(
        text.lines()
            .any(|l| l == "kept lifecycle-delivery/SKILL.md (edited by you; not changed)"),
        "{text}"
    );
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("updated") && l.contains("lifecycle-delivery")),
        "{text}"
    );
    // The script was not edited and is still brought up to date.
    assert!(
        text.lines()
            .any(|l| l.starts_with("updated lifecycle-check/scripts")),
        "{text}"
    );
}

#[test]
fn a_second_bare_init_changes_nothing() {
    let h = Home::new("upgrade-twice");
    install_as_1_0_7(&h);
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let before = files_under(&h.root);
    let mtimes: Vec<std::time::SystemTime> = pack::files()
        .iter()
        .filter(|f| f.path.starts_with(".agents/skills/"))
        .map(|f| {
            let rel = f.path.strip_prefix(".agents/skills/").unwrap();
            std::fs::metadata(h.skills_dir().join(rel))
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        files_under(&h.root),
        before,
        "the same files with the same bytes"
    );
    let after: Vec<std::time::SystemTime> = pack::files()
        .iter()
        .filter(|f| f.path.starts_with(".agents/skills/"))
        .map(|f| {
            let rel = f.path.strip_prefix(".agents/skills/").unwrap();
            std::fs::metadata(h.skills_dir().join(rel))
                .unwrap()
                .modified()
                .unwrap()
        })
        .collect();
    assert_eq!(after, mtimes, "no file was written again");
    assert!(
        !out(&o)
            .lines()
            .any(|l| l.starts_with("created") || l.starts_with("updated")),
        "{}",
        out(&o)
    );
}

#[test]
fn an_update_touches_only_the_files_it_replaces() {
    let h = Home::new("upgrade-neighbours");
    install_as_1_0_7(&h);
    let tdd = h.skills_dir().join("lifecycle-tdd/SKILL.md");
    let rules = h.rules_dir().join("00-my-rules.md");
    let keep: Vec<(PathBuf, std::time::SystemTime)> = [tdd, rules]
        .into_iter()
        .map(|p| {
            let m = std::fs::metadata(&p).unwrap().modified().unwrap();
            (p, m)
        })
        .collect();
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    for (p, m) in keep {
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            m,
            "{}",
            p.display()
        );
    }
}

#[test]
fn a_bare_init_replaces_an_unedited_1_0_6_contract_and_says_so() {
    let h = Home::new("upgrade-contract");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    std::fs::write(h.contract(), OLD_CONTRACT_1_0_6).unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    let line = out(&o)
        .lines()
        .find(|l| l.starts_with("updated"))
        .unwrap_or_else(|| panic!("an updated line: {}", out(&o)))
        .to_string();
    assert!(
        line.ends_with("10-lifecycle.md (was pack 1.0.6; not edited by you)"),
        "{line}"
    );
    assert!(
        line.contains(h.rules_dir().to_str().unwrap()),
        "the line names the file with its folder: {line}"
    );
}

#[test]
fn a_bare_init_brings_an_unedited_1_0_9_install_up_to_date_and_says_so() {
    // Pack 1.1.0 moved the records out of docs when `.ham/records` says so; a machine that installed 1.0.9 gets the four files it changed.
    let h = Home::new("upgrade-1-0-9");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let skills = h.skills_dir();
    std::fs::write(h.contract(), OLD_CONTRACT_1_0_9).unwrap();
    std::fs::write(
        skills.join("lifecycle-delivery/SKILL.md"),
        OLD_DELIVERY_1_0_9,
    )
    .unwrap();
    std::fs::write(skills.join("lifecycle-loop/SKILL.md"), OLD_LOOP_1_0_9).unwrap();
    std::fs::write(
        skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_0_9,
    )
    .unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    for (installed, in_pack) in [
        ("lifecycle-delivery/SKILL.md", DELIVERY_PATH),
        ("lifecycle-loop/SKILL.md", LOOP_PATH),
        ("lifecycle-check/scripts/lifecycle_check.py", SCRIPT_PATH),
    ] {
        assert_eq!(
            std::fs::read_to_string(skills.join(installed)).unwrap(),
            pack_bytes(in_pack),
            "{installed}"
        );
        let want = format!("updated {installed} (was pack 1.0.9; not edited by you)");
        assert!(out(&o).lines().any(|l| l == want), "{want}: {}", out(&o));
    }
    assert!(
        out(&o).lines().any(|l| l.starts_with("updated")
            && l.ends_with("10-lifecycle.md (was pack 1.0.9; not edited by you)")),
        "{}",
        out(&o)
    );
    assert!(
        !out(&o).contains("edited by you; not changed"),
        "nothing was kept as edited: {}",
        out(&o)
    );
}

#[test]
fn a_bare_init_brings_an_unedited_1_1_0_install_up_to_date_and_says_so() {
    // Pack 1.2.0 puts the records in the hidden folder `.ham` when the repository has no `.ham/records`; a machine that installed 1.1.0 gets the four files it changed.
    let h = Home::new("upgrade-1-1-0");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let skills = h.skills_dir();
    std::fs::write(h.contract(), OLD_CONTRACT_1_1_0).unwrap();
    std::fs::write(
        skills.join("lifecycle-delivery/SKILL.md"),
        OLD_DELIVERY_1_1_0,
    )
    .unwrap();
    std::fs::write(skills.join("lifecycle-loop/SKILL.md"), OLD_LOOP_1_1_0).unwrap();
    std::fs::write(
        skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_1_0,
    )
    .unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    for (installed, in_pack) in [
        ("lifecycle-delivery/SKILL.md", DELIVERY_PATH),
        ("lifecycle-loop/SKILL.md", LOOP_PATH),
        ("lifecycle-check/scripts/lifecycle_check.py", SCRIPT_PATH),
    ] {
        assert_eq!(
            std::fs::read_to_string(skills.join(installed)).unwrap(),
            pack_bytes(in_pack),
            "{installed}"
        );
        let want = format!("updated {installed} (was pack 1.1.0; not edited by you)");
        assert!(out(&o).lines().any(|l| l == want), "{want}: {}", out(&o));
    }
    assert!(
        out(&o).lines().any(|l| l.starts_with("updated")
            && l.ends_with("10-lifecycle.md (was pack 1.1.0; not edited by you)")),
        "{}",
        out(&o)
    );
    assert!(
        !out(&o).contains("edited by you; not changed"),
        "nothing was kept as edited: {}",
        out(&o)
    );
}

#[test]
fn a_bare_init_keeps_an_edited_1_1_0_contract_and_says_so() {
    // A user who edited the installed contract keeps the docs paths of it; the init says so, and the user's file stays theirs.
    let h = Home::new("upgrade-1-1-0-edited");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let edited = format!("{OLD_CONTRACT_1_1_0}\nAlso list the open questions.\n");
    std::fs::write(h.contract(), &edited).unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(h.contract()).unwrap(), edited);
    assert!(
        out(&o)
            .lines()
            .any(|l| l.starts_with("kept")
                && l.contains("10-lifecycle.md (edited by you; not changed")),
        "{}",
        out(&o)
    );
}

#[test]
fn a_bare_init_keeps_an_edited_1_0_9_loop_skill_and_says_so() {
    let h = Home::new("upgrade-1-0-9-edited");
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    let skill = h.skills_dir().join("lifecycle-loop/SKILL.md");
    let edited = format!("{OLD_LOOP_1_0_9}\nAlso list the open questions.\n");
    std::fs::write(&skill, &edited).unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), edited);
    assert!(
        out(&o)
            .lines()
            .any(|l| l == "kept lifecycle-loop/SKILL.md (edited by you; not changed)"),
        "{}",
        out(&o)
    );
}

#[test]
fn no_skills_still_brings_an_unedited_earlier_contract_up_to_date() {
    let h = Home::new("upgrade-contract-only");
    assert_eq!(code(&h.run(&["rules", "init", "--no-skills"])), 0);
    std::fs::write(h.contract(), OLD_CONTRACT_1_0_6).unwrap();
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        pack::CONTRACT
    );
    assert!(!h.skills_dir().exists(), "no skills were asked for");
}

#[test]
fn pack_none_leaves_an_old_contract_alone() {
    let h = Home::new("upgrade-pack-none");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.contract(), OLD_CONTRACT_1_0_6).unwrap();
    let o = h.run(&["rules", "init", "--pack", "none"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.contract()).unwrap(),
        OLD_CONTRACT_1_0_6,
        "the pack was not asked for, so none of its files is touched"
    );
    assert!(
        !out(&o).lines().any(|l| l.starts_with("updated")),
        "{}",
        out(&o)
    );
}

#[test]
fn lifecycle_init_keeps_an_old_pack_file() {
    // `ham lifecycle init` writes into a repository, where every file is the repository's: an earlier pack text is kept like any other.
    let h = Home::new("upgrade-repo");
    let repo = h.root.join("repo");
    let skill = repo.join(DELIVERY_PATH);
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, OLD_DELIVERY_1_0_7).unwrap();
    let o = h.run(&["lifecycle", "init", repo.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), OLD_DELIVERY_1_0_7);
    assert!(
        out(&o)
            .lines()
            .any(|l| l.contains("kept") && l.contains("lifecycle-delivery/SKILL.md")),
        "{}",
        out(&o)
    );
    assert!(!out(&o).contains("updated"), "{}", out(&o));
    let json = h.run(&["lifecycle", "init", repo.to_str().unwrap(), "--json"]);
    let v: Value = serde_json::from_str(out(&json).trim()).unwrap();
    let action = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == DELIVERY_PATH)
        .unwrap()["action"]
        .clone();
    assert_eq!(action, "kept");
}
