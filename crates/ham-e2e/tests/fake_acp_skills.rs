//! The skills side of the fake ACP engine (`fake-acp-engine config skills --json`, and its `skills` tool call): the offline stand-in for
//! the pinned engine's skills discovery.
//!
//! What it copies from the pinned engine (Cline 3.0.71, read with `cline config skills --json`): skills
//! come from `<config dir>/skills`, then the repository's `.agents/skills`, `.cline/skills`, `.claude/skills` and `.clinerules/skills`, then
//! `$HOME/.agents/skills` (never `$HOME/.cline/skills`); a name listed twice is listed once, the first source wins (the configuration
//! directory beats the repository); the file must be named `SKILL.md`, must have text after its frontmatter, and is listed under the
//! frontmatter `name` (the folder name when there is none); `instructions` is the text after the frontmatter trimmed at both ends.
//! The `skills` tool call is resolved against the same discovery: found, it completes and the skill's `FAKE-ACP-WRITE <path> <text>` lines
//! are carried out in the workspace (what a model would do from the sentences above them); not found, it fails and nothing is written.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let d = base.join(format!(
        "fake-skills-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.subsec_nanos())
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
}

/// A world: a configuration directory, a repository (the engine's current directory) and a HOME.
struct World {
    config: PathBuf,
    repo: PathBuf,
    home: PathBuf,
}

fn world(label: &str) -> World {
    let d = scratch(label);
    let w = World {
        config: d.join("config"),
        repo: d.join("repo"),
        home: d.join("home"),
    };
    for p in [&w.config, &w.repo, &w.home] {
        std::fs::create_dir_all(p).unwrap();
    }
    w
}

fn listing(w: &World, mode: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"));
    cmd.args(["config", "skills", "--json", "--config"])
        .arg(&w.config)
        .current_dir(&w.repo)
        .env("HOME", &w.home)
        .env_remove("FAKE_ACP_SKILLS_MODE");
    if let Some(m) = mode {
        cmd.env("FAKE_ACP_SKILLS_MODE", m);
    }
    cmd.output().unwrap()
}

fn rows(w: &World) -> Vec<Value> {
    let out = listing(w, None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Value>(&out.stdout)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn names(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_row_has_the_name_description_trimmed_instructions_frontmatter_and_the_absolute_path() {
    let w = world("row");
    put(
        &w.config,
        "skills/probe-skill/SKILL.md",
        &skill_md(
            "probe-skill",
            "A probe skill. Use when asked for the probe.",
            "\n\n# Probe body\n\nStep one: do the probe.\n\n",
        ),
    );
    let r = rows(&w);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["name"], "probe-skill");
    assert_eq!(
        r[0]["description"],
        "A probe skill. Use when asked for the probe."
    );
    assert_eq!(
        r[0]["instructions"],
        "# Probe body\n\nStep one: do the probe."
    );
    assert_eq!(r[0]["frontmatter"]["name"], "probe-skill");
    assert_eq!(
        r[0]["path"].as_str().unwrap(),
        w.config
            .join("skills/probe-skill/SKILL.md")
            .display()
            .to_string()
    );
}

#[test]
fn the_engine_keeps_line_endings_inside_the_body_and_trims_only_its_ends() {
    let w = world("crlf");
    put(
        &w.config,
        "skills/crlf/SKILL.md",
        "---\r\nname: crlf\r\ndescription: Crlf body.\r\n---\r\n# Title\r\n\r\nLine one.\r\n\r\nLast.\r\n",
    );
    let r = rows(&w);
    assert_eq!(
        r[0]["instructions"],
        "# Title\r\n\r\nLine one.\r\n\r\nLast."
    );
}

#[test]
fn skills_come_from_the_config_directory_the_repository_and_home_and_the_first_source_wins() {
    let w = world("sources");
    let doc =
        |n: &str, who: &str| skill_md(n, &format!("from {who}"), &format!("body from {who}\n"));
    put(
        &w.config,
        "skills/in-config/SKILL.md",
        &doc("in-config", "config"),
    );
    for root in [".agents", ".cline", ".claude", ".clinerules"] {
        let n = format!("in-repo-{}", root.trim_start_matches('.'));
        put(
            &w.repo,
            &format!("{root}/skills/{n}/SKILL.md"),
            &doc(&n, "repo"),
        );
    }
    put(
        &w.home,
        ".agents/skills/in-home/SKILL.md",
        &doc("in-home", "home"),
    );
    // Not read: HOME's .cline/skills (measured on the pinned engine).
    put(
        &w.home,
        ".cline/skills/not-read/SKILL.md",
        &doc("not-read", "home cline"),
    );
    // The same name in the repository and the configuration directory: one row, the configuration directory's.
    put(&w.repo, ".agents/skills/dup/SKILL.md", &doc("dup", "repo"));
    put(&w.config, "skills/dup/SKILL.md", &doc("dup", "config"));

    let r = rows(&w);
    let mut got = names(&r);
    got.sort();
    assert_eq!(
        got,
        [
            "dup",
            "in-config",
            "in-home",
            "in-repo-agents",
            "in-repo-claude",
            "in-repo-cline",
            "in-repo-clinerules"
        ]
    );
    let dup = r.iter().find(|x| x["name"] == "dup").unwrap();
    assert_eq!(dup["description"], "from config");
    assert!(
        dup["path"]
            .as_str()
            .unwrap()
            .starts_with(&w.config.display().to_string())
    );
    let home = r.iter().find(|x| x["name"] == "in-home").unwrap();
    assert!(
        home["path"]
            .as_str()
            .unwrap()
            .starts_with(&w.home.display().to_string())
    );
}

#[test]
fn a_file_the_engine_would_not_list_is_not_listed() {
    let w = world("unlisted");
    // Measured on the pinned engine: a lower-case file name and an empty body are not listed; no frontmatter is listed under the folder
    // name with no description; the frontmatter name wins over the folder name.
    put(
        &w.config,
        "skills/lower/skill.md",
        &skill_md("lower", "d.", "Body.\n"),
    );
    put(
        &w.config,
        "skills/emptybody/SKILL.md",
        "---\nname: emptybody\ndescription: d.\n---\n\n  \n",
    );
    put(&w.config, "skills/nofm/SKILL.md", "No frontmatter body.\n");
    put(
        &w.config,
        "skills/mismatch/SKILL.md",
        &skill_md("other-name", "d.", "Body.\n"),
    );
    let r = rows(&w);
    assert_eq!(names(&r), ["nofm", "other-name"]);
    let nofm = r.iter().find(|x| x["name"] == "nofm").unwrap();
    assert!(nofm.get("description").is_none());
    assert_eq!(nofm["instructions"], "No frontmatter body.");
    let other = r.iter().find(|x| x["name"] == "other-name").unwrap();
    assert!(
        other["path"]
            .as_str()
            .unwrap()
            .ends_with("mismatch/SKILL.md")
    );
}

#[test]
fn the_frontmatter_reader_handles_quotes_folded_text_numbers_stray_lines_and_an_unclosed_block() {
    let w = world("fm");
    put(
        &w.config,
        "skills/odd/SKILL.md",
        "---\n# comment\nname: 'odd'\nextra: 7\nstray line\ndescription: >-\n  Folded over\n  two lines.\n\n  after a gap\nlast: \"q\"\n---\nBody odd.\n",
    );
    put(
        &w.config,
        "skills/unclosed/SKILL.md",
        "---\nname: unclosed\ndescription: d\nBody never closed.\n",
    );
    let r = rows(&w);
    let odd = r.iter().find(|x| x["name"] == "odd").unwrap();
    assert_eq!(odd["description"], "Folded over two lines. after a gap");
    assert_eq!(odd["frontmatter"]["extra"], 7);
    assert_eq!(odd["frontmatter"]["last"], "q");
    assert_eq!(odd["instructions"], "Body odd.");
    // A frontmatter block that never closes is not a frontmatter: the whole text is the instructions, under the folder name.
    let unclosed = r.iter().find(|x| x["name"] == "unclosed").unwrap();
    assert!(unclosed.get("description").is_none());
    assert!(
        unclosed["instructions"]
            .as_str()
            .unwrap()
            .starts_with("---")
    );
}

#[test]
fn no_skill_at_all_is_an_empty_array() {
    let w = world("empty");
    let out = listing(&w, None);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[]");
}

#[test]
fn the_skills_mode_breaks_the_listing_the_way_a_real_one_can_break() {
    let w = world("modes");
    put(
        &w.config,
        "skills/one/SKILL.md",
        &skill_md("one", "d.", "Body one.\n"),
    );
    put(
        &w.config,
        "skills/two/SKILL.md",
        &skill_md("two", "d.", "Body two.\n"),
    );
    let out = listing(&w, Some("fail"));
    assert!(!out.status.success());
    let out = listing(&w, Some("garbage"));
    assert!(out.status.success());
    assert!(serde_json::from_slice::<Value>(&out.stdout).is_err());
    let rows_of = |mode: &str| -> Vec<Value> {
        serde_json::from_slice::<Value>(&listing(&w, Some(mode)).stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    };
    assert_eq!(names(&rows_of("missing")), ["two"]);
    let changed = rows_of("changed");
    assert_eq!(changed[0]["instructions"], "Body one. (changed)");
    assert_eq!(changed[1]["instructions"], "Body two.");
    let extra = rows_of("extra");
    assert_eq!(extra.len(), 3);
    assert_eq!(extra[2]["name"], "planted");
    // The extra row names a file that exists and is outside the repository.
    assert!(Path::new(extra[2]["path"].as_str().unwrap()).exists());
    assert!(
        !extra[2]["path"]
            .as_str()
            .unwrap()
            .starts_with(&w.repo.display().to_string())
    );
}

#[test]
fn a_config_directory_is_required() {
    let out = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
        .args(["config", "skills", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

// ---- the `skills` tool call ---------------------------------------------------------------------------------------------------------

struct Session {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(w: &World, log: &Path) -> Self {
        let transcripts = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/engines/fake-acp/transcripts");
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-acp-engine"))
            .arg("--transcripts")
            .arg(&transcripts)
            .arg("--workspace")
            .arg(&w.repo)
            .current_dir(&w.repo)
            .env("HOME", &w.home)
            .env("FAKE_ACP_SCENARIO", "skills_canary")
            .env("FAKE_ACP_CONFIG_DIR", &w.config)
            .env("FAKE_ACP_LOG", log)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            out,
            next_id: 1,
        }
    }

    /// Sends a request and returns every message up to and including its response.
    fn call(&mut self, method: &str, params: Value) -> Vec<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut seen = Vec::new();
        loop {
            let mut l = String::new();
            assert!(
                self.out.read_line(&mut l).unwrap() > 0,
                "the engine closed its output"
            );
            let v: Value = serde_json::from_str(l.trim()).unwrap();
            let done = v["id"] == id && v.get("method").is_none();
            seen.push(v);
            if done {
                return seen;
            }
        }
    }
}

impl Drop for Session {
    /// The engine exits 0 when its input closes; ending it that way lets an instrumented build write its coverage data.
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

fn run_canary(w: &World) -> Vec<Value> {
    run_canary_logged(w, &w.config.parent().unwrap().join("fake-acp.log"))
}

fn run_canary_logged(w: &World, log: &Path) -> Vec<Value> {
    let mut s = Session::start(w, log);
    s.call(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    s.call("session/new", json!({"cwd": w.repo, "mcpServers": []}));
    s.call(
        "session/prompt",
        json!({"sessionId": "fake-session-0001", "prompt": [{"type": "text", "text": "go"}]}),
    )
}

fn call_update(msgs: &[Value]) -> &Value {
    msgs.iter()
        .map(|m| &m["params"]["update"])
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "fake-call-1")
        .expect("an update for the skills call")
}

#[test]
fn a_skills_call_for_a_skill_the_engine_can_find_completes_and_carries_out_its_directive() {
    let w = world("call-found");
    put(
        &w.config,
        "skills/lifecycle-canary/SKILL.md",
        &skill_md(
            "lifecycle-canary",
            "Use when asked to run the canary.",
            "Create the marker.\n\nFAKE-ACP-WRITE skill-canary.txt ran by lifecycle-canary\n",
        ),
    );
    let msgs = run_canary(&w);
    let u = call_update(&msgs);
    assert_eq!(u["status"], "completed", "{u}");
    assert!(u.to_string().contains("Create the marker."), "{u}");
    assert_eq!(
        std::fs::read_to_string(w.repo.join("skill-canary.txt")).unwrap(),
        "ran by lifecycle-canary\n"
    );
}

#[test]
fn a_skills_call_for_a_skill_the_engine_cannot_find_fails_and_writes_nothing() {
    let w = world("call-missing");
    let msgs = run_canary(&w);
    let u = call_update(&msgs);
    assert_eq!(u["status"], "failed", "{u}");
    assert!(u.to_string().contains("lifecycle-canary"), "{u}");
    assert!(!w.repo.join("skill-canary.txt").exists());
}

#[test]
fn a_directive_cannot_write_outside_the_workspace() {
    let w = world("call-escape");
    let absolute = w.config.parent().unwrap().join("abs-escape.txt");
    put(
        &w.config,
        "skills/lifecycle-canary/SKILL.md",
        &skill_md(
            "lifecycle-canary",
            "d.",
            &format!(
                "FAKE-ACP-WRITE ../escaped.txt nope\nFAKE-ACP-WRITE {} nope\n",
                absolute.display()
            ),
        ),
    );
    let log = w.config.parent().unwrap().join("escape.log");
    let msgs = run_canary_logged(&w, &log);
    assert_eq!(call_update(&msgs)["status"], "completed");
    // The trigger: both directives were read and refused, so the two missing files are not for lack of trying.
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        text.matches("refused: outside the workspace").count(),
        2,
        "{text}"
    );
    assert!(!w.repo.parent().unwrap().join("escaped.txt").exists());
    assert!(!absolute.exists());
}
