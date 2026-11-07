//! The `code_search` tool of agentd: a ripgrep
//! search inside the run's work tree, with the paths it may be given confined to the work tree and the context directory.
//!
//! Three kinds of case, all through the backend's own `call` (the server and its validation are covered by `direct_tools_server.rs`; the few
//! cases here that go through a socket say so):
//! - the real `rg` (`HAMH_RG_BIN`, else `rg` on the search path; a case prints `SKIPPED` and returns where there is none) on a generated tree:
//!   equality with `rg --json` run by hand, the modes, the order, the timing;
//! - a fake `rg` (a shell script in a scratch directory, so these always run) that replays recorded `--json` messages, records what it was
//!   given, sleeps, or fails: the caps, the line rules, the child's identity and environment, the limits, the cancel, the failure words;
//! - the confinement table, which needs no `rg` at all and runs on every strategy (`openat2`, the user-space walk and the automatic choice).
//!
//! The canaries follow the package: a pattern, a path and a text on the standard error of `rg` never reach a log line, a counter, a failure
//! text or a file on disk. Link, `..` and metacharacter cases use ordinary names, plain symbolic links and the characters `&`, `;`, a space and
//! `$HOME`, and assert that the literal arrived.
#![allow(clippy::unwrap_used)]

mod common;
mod tools_support;
#[path = "common/tree.rs"]
mod tree;

use agentd::direct::tools::backend::{
    CancelToken, FailureWord, SpecFn, ToolBackend, ToolContext, ToolOutput,
};
use agentd::direct::tools::backends::rg::confine::{self, Refusal, RootKind, Roots, Strategy};
use agentd::direct::tools::backends::rg::{ANSWER_BUDGET, CodeSearch, Limits};
use agentd::direct::tools::result::render;
use agentd::direct::tools::server::{self, ServerHandle, Shared};
use agentd::direct::tools::stats::Counters;
use agentd::engine::LaunchSpec;
use agentd::logging::MemoryLog;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tools_support::{gone, request, rpc, text_of, until};

// ---- the rig ------------------------------------------------------------------------------------------------------------------------

/// A ripgrep 14 to run the real-`rg` cases with, or `None` (after saying so).
fn real_rg() -> Option<PathBuf> {
    let found = std::env::var_os("HAMH_RG_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PATH").and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join("rg"))
                    .find(|c| c.is_file())
            })
        });
    let Some(rg) = found else {
        eprintln!(
            "SKIPPED: no ripgrep (HAMH_RG_BIN names a ripgrep 14.1.1 binary; the fake-rg cases still run)"
        );
        return None;
    };
    let version = std::process::Command::new(&rg)
        .arg("--version")
        .env_clear()
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if !version.starts_with("ripgrep 14.") {
        eprintln!("SKIPPED: {} is not ripgrep 14 ({version:?})", rg.display());
        return None;
    }
    Some(rg)
}

fn plain_path() -> Vec<PathBuf> {
    ["/usr/local/bin", "/usr/bin", "/bin"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

/// The launch-spec builder of a run, as a test sees it: the working directory is the work tree, the search path is `dirs`, and the standard error
/// goes to a file in `stderr_dir`.
fn spec_fn(
    work: &Path,
    dirs: Vec<PathBuf>,
    run_as: Option<(u32, u32)>,
    env: BTreeMap<String, String>,
    stderr_dir: &Path,
) -> SpecFn {
    let (work, stderr_dir) = (work.to_path_buf(), stderr_dir.to_path_buf());
    Arc::new(move |argv, stderr| LaunchSpec {
        argv,
        env: env.clone(),
        cwd: work.clone(),
        path: dirs.clone(),
        stderr_log: stderr_dir.join(stderr),
        run_as,
        sandbox: None,
    })
}

fn context(work: &Path, context_dir: Option<&Path>, dirs: Vec<PathBuf>) -> ToolContext {
    let stderr_dir = common::scratch("rg-stderr");
    ToolContext::new(
        work.to_path_buf(),
        context_dir.map(Path::to_path_buf),
        spec_fn(work, dirs, None, BTreeMap::new(), &stderr_dir),
        Counters::new(),
    )
}

async fn search(backend: &CodeSearch, ctx: &ToolContext, input: Value) -> ToolOutput {
    backend.call(ctx, input, CancelToken::new()).await
}

/// The text of an answer; a failure panics with its words.
fn ok(out: &ToolOutput) -> String {
    match out {
        ToolOutput::Text { text, .. } => text.clone(),
        ToolOutput::Failed { word, detail } => panic!("failed: {word:?} {detail:?}"),
    }
}

fn truncated(out: &ToolOutput) -> bool {
    matches!(
        out,
        ToolOutput::Text {
            truncated: true,
            ..
        }
    )
}

fn failure(out: &ToolOutput) -> (FailureWord, String) {
    match out {
        ToolOutput::Failed { word, detail } => (*word, detail.clone().unwrap_or_default()),
        ToolOutput::Text { text, .. } => panic!("expected a failure, got {text:?}"),
    }
}

/// `(path, line)` of every match line of an answer (`path:line:text`); context lines (`path-line-text`), separators and notes are left out.
fn hits(text: &str) -> BTreeSet<(String, u64)> {
    ordered_hits(text).into_iter().collect()
}

fn ordered_hits(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|l| {
            let (path, rest) = l.split_once(':')?;
            let (line, _) = rest.split_once(':')?;
            Some((path.to_string(), line.trim().parse().ok()?))
        })
        .collect()
}

fn notes(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with('[')).collect()
}

/// What `rg --json` finds by hand: the same options the tool applies to the results (`extra` adds `--hidden` or `--no-ignore`), no caps and
/// the default (parallel) order. Paths are relative to `tree`.
fn by_hand(rg: &Path, tree: &Path, pattern: &str, extra: &[&str]) -> BTreeSet<(String, u64)> {
    by_hand_ordered(rg, tree, pattern, extra, false)
        .into_iter()
        .collect()
}

fn by_hand_ordered(
    rg: &Path,
    tree: &Path,
    pattern: &str,
    extra: &[&str],
    sorted: bool,
) -> Vec<(String, u64)> {
    let mut cmd = std::process::Command::new(rg);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(["--json", "--no-config", "--max-filesize=2M", "--no-follow"])
        .args(extra);
    if sorted {
        cmd.arg("--sort=path");
    }
    let out = cmd
        .arg("-e")
        .arg(pattern)
        .arg("--")
        .arg(".")
        .current_dir(tree)
        .output()
        .unwrap();
    assert!(
        matches!(out.status.code(), Some(0) | Some(1)),
        "rg by hand: {:?}",
        out.status
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "match")
        .map(|v| {
            let path = v["data"]["path"]["text"].as_str().unwrap();
            (
                path.strip_prefix("./").unwrap_or(path).to_string(),
                v["data"]["line_number"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn write_exec(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ---- recorded --json messages and the fake rg ----------------------------------------------------------------------------------------

fn j_begin(path: &str) -> String {
    json!({"type":"begin","data":{"path":{"text":path}}}).to_string()
}

fn j_end(path: &str) -> String {
    json!({"type":"end","data":{"path":{"text":path},"binary_offset":null,"stats":{"matches":1}}})
        .to_string()
}

fn j_summary() -> String {
    json!({"type":"summary","data":{"elapsed_total":{"secs":0,"nanos":1,"human":"0s"},"stats":{"matches":1}}})
        .to_string()
}

fn j_match(path: &str, line: u64, text: &str, start: usize, end: usize) -> String {
    json!({"type":"match","data":{"path":{"text":path},"lines":{"text":format!("{text}\n")},
        "line_number":line,"absolute_offset":0,
        "submatches":[{"match":{"text":text.get(start..end).unwrap_or("")},"start":start,"end":end}]}})
    .to_string()
}

fn j_context(path: &str, line: u64, text: &str) -> String {
    json!({"type":"context","data":{"path":{"text":path},"lines":{"text":format!("{text}\n")},
        "line_number":line,"absolute_offset":0,"submatches":[]}})
    .to_string()
}

/// A fake `rg`: the script's directory comes first on the search path of the launch spec, so `rg` and `setpriv` resolve to these files.
struct Fake {
    base: PathBuf,
    work: PathBuf,
    bin: PathBuf,
    ctx: Arc<ToolContext>,
    backend: Arc<CodeSearch>,
}

impl Fake {
    /// A fake whose `rg` is the shell `script` (the `#!/bin/sh` line is added). `{BASE}` in the text is the scratch directory.
    fn script(script: &str) -> Self {
        Self::build(script, None, BTreeMap::new(), Limits::default())
    }

    fn build(
        script: &str,
        run_as: Option<(u32, u32)>,
        env: BTreeMap<String, String>,
        limits: Limits,
    ) -> Self {
        let base = canon(&common::scratch("rg-fake"));
        let work = base.join("work");
        let bin = base.join("bin");
        for d in [&work, &bin, &work.join("src"), &work.join(".git")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let text = script.replace("{BASE}", &base.display().to_string());
        write_exec(&bin.join("rg"), &format!("#!/bin/sh\n{text}\n"));
        let mut dirs = vec![bin.clone()];
        dirs.extend(plain_path());
        let ctx = ToolContext::new(
            work.clone(),
            None,
            spec_fn(&work, dirs, run_as, env, &base),
            Counters::new(),
        );
        Fake {
            base,
            work,
            bin,
            ctx: Arc::new(ctx),
            backend: Arc::new(CodeSearch::with_program("rg").with_limits(limits)),
        }
    }

    /// A fake that prints these messages and the summary, and exits with `code`.
    fn replay(messages: &[String], code: i32) -> Self {
        let mut lines = messages.to_vec();
        lines.push(j_summary());
        Self::replay_raw(&lines.join("\n"), code)
    }

    fn replay_raw(stdout: &str, code: i32) -> Self {
        // The fixture is written next to the script; the script cats it. The work tree is known only after the scratch directory exists, so the
        // messages carry `{WORK}` for it.
        let probe = Self::script("exit 0");
        let fixture = probe.base.join("replay.jsonl");
        std::fs::write(
            &fixture,
            stdout.replace("{WORK}", &probe.work.display().to_string()),
        )
        .unwrap();
        write_exec(
            &probe.bin.join("rg"),
            &format!("#!/bin/sh\ncat '{}'\nexit {code}\n", fixture.display()),
        );
        probe
    }

    async fn call(&self, input: Value) -> ToolOutput {
        search(&self.backend, &self.ctx, input).await
    }

    fn record(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.base.join(name)).ok()
    }
}

// ---- the package's cases: results ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn results_equal_rg_run_by_hand() {
    let Some(rg) = real_rg() else { return };
    let work = canon(&common::scratch("rg-equal"));
    tree::generate(&work, 2_000, true);
    let ctx = context(&work, None, plain_path());
    let backend = CodeSearch::with_program(rg.display().to_string());
    // (name, pattern, matches the generated tree holds): a rare needle, a token under the cap of 200, a regular expression, a whole word and a
    // pattern that is nowhere. The counts are the trigger: a tree or a ripgrep that changed would make the equality below vacuous.
    let cases: [(&str, &str, usize); 5] = [
        ("rare needle", tree::RARE, 5),
        ("medium token", tree::MEDIUM, 154),
        ("regex", tree::REGEX, 20),
        ("whole word", r"\bneedle_word\b", 118),
        ("no match", "no_such_token_qq", 0),
    ];
    for (name, pattern, want) in cases {
        let hand = by_hand(&rg, &work, pattern, &[]);
        assert_eq!(
            hand.len(),
            want,
            "TRIGGER ABSENT ({name}): the tree or ripgrep changed"
        );
        let out = search(
            &backend,
            &ctx,
            json!({"pattern": pattern, "maxResults": 200, "maxPerFile": 50}),
        )
        .await;
        let text = ok(&out);
        assert!(!truncated(&out), "{name}: under the caps, nothing is cut");
        assert_eq!(hits(&text), hand, "{name}");
        if want == 0 {
            assert_eq!(text.lines().next(), Some("no matches"));
        }
    }
    eprintln!("TRIGGER PRESENT equality-with-rg cases=5 tree=2000-files");
}

#[tokio::test]
async fn a_capped_answer_is_the_first_matches_in_path_order_and_repeats_byte_for_byte() {
    let Some(rg) = real_rg() else { return };
    let work = canon(&common::scratch("rg-order"));
    tree::generate(&work, 2_000, true);
    let ctx = context(&work, None, plain_path());
    let backend = CodeSearch::with_program(rg.display().to_string());
    let input = json!({"pattern": tree::COMMON});
    let first = search(&backend, &ctx, input.clone()).await;
    let second = search(&backend, &ctx, input).await;
    assert_eq!(ok(&first), ok(&second), "two identical calls, two answers");
    let ours = ordered_hits(&ok(&first));
    let hand: Vec<_> = by_hand_ordered(&rg, &work, tree::COMMON, &[], true)
        .into_iter()
        .take(50)
        .collect();
    assert_eq!(hand.len(), 50, "TRIGGER ABSENT: the token is under the cap");
    assert_eq!(
        ours, hand,
        "the answer is rg's own first 50 matches in path order"
    );
    assert!(truncated(&first));
    let text = ok(&first);
    let note = notes(&text).join(" ");
    assert!(
        note.contains("maxResults") && note.contains("50"),
        "the reason is named: {note:?}"
    );
}

#[tokio::test]
async fn the_matching_line_is_always_in_the_answer() {
    let f = Fake::replay(
        &{
            let p = "{WORK}/src/a.rs";
            let long = format!("{}NEEDLE{}", "a".repeat(3_000), "b".repeat(2_000));
            let multi = format!("{}NEEDLE tail", "é".repeat(400));
            vec![
                j_begin(p),
                j_match(p, 12, "    let x = foo();", 12, 15),
                j_match(p, 40, &long, 3_000, 3_006),
                j_match(p, 41, &multi, 800, 806),
                j_end(p),
            ]
        },
        0,
    );
    let text = ok(&f.call(json!({"pattern": "NEEDLE|foo"})).await);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0], "src/a.rs:12:    let x = foo();",
        "path, line number and the text, exactly: {text}"
    );
    for (no, needle) in [(40, "NEEDLE"), (41, "NEEDLE")] {
        let prefix = format!("src/a.rs:{no}:");
        let line = lines
            .iter()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("line {no} is missing: {text}"));
        let body = &line[prefix.len()..];
        assert!(body.chars().count() <= 300, "line {no}: {}", body.len());
        assert!(
            body.contains(needle),
            "the window of a long line holds the match: {body}"
        );
        assert!(
            body.starts_with('…') || body.ends_with('…'),
            "a cut is marked"
        );
    }
}

#[tokio::test]
async fn lines_are_made_safe_to_read() {
    let p = "{WORK}/src/odd.rs";
    let bytes_path = {
        // A path and a line that are not UTF-8 come as base64 under `bytes`.
        let line = json!({"type":"match","data":{"path":{"bytes":"c3JjL2Y5Z2dlZP8ucnM="},
            "lines":{"bytes":"Y2Fm6SBhbHBoYQo="},"line_number":3,"absolute_offset":0,
            "submatches":[{"match":{"text":"alpha"},"start":5,"end":10}]}});
        line.to_string()
    };
    let f = Fake::replay(
        &[
            j_begin(p),
            j_match(p, 1, "tab\there\u{1b}[31mred\rcr", 0, 3),
            j_match(p, 2, "ends with crlf\r", 0, 4),
            bytes_path,
            j_end(p),
        ],
        0,
    );
    let text = ok(&f.call(json!({"pattern": "tab|ends|alpha"})).await);
    assert!(
        text.contains("src/odd.rs:1:tab\there\u{fffd}[31mred\u{fffd}cr"),
        "control characters other than tab become U+FFFD: {text:?}"
    );
    assert!(
        text.lines().any(|l| l == "src/odd.rs:2:ends with crlf"),
        "a line end is not part of the text: {text:?}"
    );
    assert!(
        text.contains(":3:caf\u{fffd} alpha"),
        "bytes that are not UTF-8 are shown lossily: {text:?}"
    );
    assert!(
        text.contains("src/f9gged\u{fffd}.rs") || text.contains("f9gged\u{fffd}"),
        "so is a path: {text:?}"
    );
    assert!(!text.contains('\r') && !text.contains('\u{1b}'), "{text:?}");
}

#[tokio::test]
async fn context_lines_belong_to_the_matches_that_are_kept() {
    let f = Fake::replay(
        &{
            let p = "{WORK}/src/c.rs";
            vec![
                j_begin(p),
                j_context(p, 9, "nine"),
                j_match(p, 10, "ten match", 4, 9),
                j_context(p, 11, "eleven"),
                j_context(p, 19, "nineteen"),
                j_match(p, 20, "twenty match", 7, 12),
                j_context(p, 21, "twenty-one"),
                j_context(p, 23, "twenty-three"),
                j_match(p, 24, "twenty-four match", 12, 17),
                j_context(p, 25, "twenty-five"),
                j_end(p),
            ]
        },
        0,
    );
    let out = f
        .call(json!({"pattern": "match", "context": 1, "maxPerFile": 2}))
        .await;
    let text = ok(&out);
    let want = [
        "src/c.rs-9-nine",
        "src/c.rs:10:ten match",
        "src/c.rs-11-eleven",
        "--",
        "src/c.rs-19-nineteen",
        "src/c.rs:20:twenty match",
        "src/c.rs-21-twenty-one",
    ];
    let got: Vec<&str> = text.lines().take(want.len()).collect();
    assert_eq!(got, want, "{text}");
    assert!(
        !text.contains("twenty-three")
            && !text.contains("twenty-four")
            && !text.contains("twenty-five"),
        "the third match and its context are not shown, even where the context is near another match: {text}"
    );
    assert!(
        notes(&text).iter().any(|n| n.contains("maxPerFile 2")),
        "{text}"
    );
    assert!(truncated(&out));
}

#[tokio::test]
async fn a_file_with_one_more_than_maxperfile_is_known() {
    let Some(rg) = real_rg() else { return };
    let work = canon(&common::scratch("rg-plus-one"));
    std::fs::create_dir_all(work.join(".git")).unwrap();
    let lines = |n: usize| -> String { (1..=n).map(|i| format!("hit number {i}\n")).collect() };
    std::fs::write(work.join("exact.txt"), lines(10)).unwrap();
    std::fs::write(work.join("more.txt"), lines(11)).unwrap();
    let ctx = context(&work, None, plain_path());
    let backend = CodeSearch::with_program(rg.display().to_string());
    // Exactly maxPerFile matches: all of them, and no cut.
    let out = search(
        &backend,
        &ctx,
        json!({"pattern": "hit", "paths": ["exact.txt"]}),
    )
    .await;
    let text = ok(&out);
    assert_eq!(hits(&text).len(), 10, "{text}");
    assert!(!truncated(&out) && notes(&text).is_empty(), "{text}");
    // One more than maxPerFile: ten lines, and the file is said to have more.
    let out = search(
        &backend,
        &ctx,
        json!({"pattern": "hit", "paths": ["more.txt"]}),
    )
    .await;
    let text = ok(&out);
    assert_eq!(hits(&text).len(), 10, "{text}");
    assert!(truncated(&out), "{text}");
    assert!(
        notes(&text)
            .iter()
            .any(|n| n.contains("maxPerFile 10 reached in 1 file;")),
        "{text}"
    );
    // Both: the note counts the one file that went over.
    let out = search(&backend, &ctx, json!({"pattern": "hit"})).await;
    let text = ok(&out);
    assert_eq!(hits(&text).len(), 20, "{text}");
    assert!(
        notes(&text).iter().any(|n| n.contains("in 1 file;")),
        "{text}"
    );
}

#[tokio::test]
async fn per_file_and_total_caps_and_the_truncation_note() {
    // 100 files of 50 matches each: 5,000 matches. A fake rg does not honour --max-count, so the backend counts for itself.
    let many = |files: usize, per_file: usize| -> Vec<String> {
        let mut m = Vec::new();
        for f in 0..files {
            let p = format!("{{WORK}}/src/f{f:03}.rs");
            m.push(j_begin(&p));
            for i in 0..per_file {
                m.push(j_match(&p, i as u64 + 1, &format!("hit {f}.{i}"), 0, 3));
            }
            m.push(j_end(&p));
        }
        m
    };
    let f = Fake::replay(&many(100, 50), 0);
    let out = f.call(json!({"pattern": "hit"})).await;
    let text = ok(&out);
    let h = ordered_hits(&text);
    assert_eq!(h.len(), 50, "the default total is 50: {text}");
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    for (p, _) in &h {
        *per_file.entry(p.as_str()).or_default() += 1;
    }
    assert!(
        per_file.values().all(|n| *n <= 10),
        "the default per file is 10: {per_file:?}"
    );
    assert_eq!(per_file.len(), 5, "{per_file:?}");
    assert!(truncated(&out));
    let n = notes(&text).join(" | ");
    assert!(n.contains("maxResults") && n.contains("50"), "{n}");
    assert!(n.contains("maxPerFile 10") && n.contains("5 files"), "{n}");

    // maxPerFile 3 with room in the total: three lines of every file.
    let out = f
        .call(json!({"pattern": "hit", "maxPerFile": 3, "maxResults": 200}))
        .await;
    let h = ordered_hits(&ok(&out));
    assert_eq!(h.len(), 200);
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    for (p, _) in &h {
        *per_file.entry(p.as_str()).or_default() += 1;
    }
    assert!(per_file.values().all(|n| *n <= 3), "{per_file:?}");
    assert_eq!(
        per_file.values().filter(|n| **n == 3).count(),
        66,
        "three lines of each of 66 files, two of the 67th: {per_file:?}"
    );

    // Exactly the cap is not a truncation; one more is.
    let f = Fake::replay(&many(5, 10), 0);
    let out = f.call(json!({"pattern": "hit"})).await;
    assert!(!truncated(&out), "50 of 50 matches, 10 of 10 per file");
    assert!(notes(&ok(&out)).is_empty(), "{}", ok(&out));
    let f = Fake::replay(&many(5, 11), 0);
    let out = f.call(json!({"pattern": "hit"})).await;
    assert!(truncated(&out), "an eleventh match in a file is a cut");
    let f = Fake::replay(&many(51, 1), 0);
    let out = f.call(json!({"pattern": "hit"})).await;
    assert!(truncated(&out), "a 51st match is a cut");
    assert_eq!(ordered_hits(&ok(&out)).len(), 50);
}

#[tokio::test]
async fn the_files_and_count_modes_read_the_nul_separated_output() {
    // filesOnly: `path\0` per file; the last entry may come without its terminator.
    let f = Fake::replay_raw(
        "{WORK}/src/a.rs\u{0}{WORK}/src/b.rs\u{0}/somewhere/else/notes.md",
        0,
    );
    let out = f.call(json!({"pattern": "x", "filesOnly": true})).await;
    assert_eq!(ok(&out), "src/a.rs\nsrc/b.rs\n/somewhere/else/notes.md");
    assert!(!truncated(&out));
    let out = f
        .call(json!({"pattern": "x", "filesOnly": true, "maxResults": 2}))
        .await;
    let text = ok(&out);
    assert_eq!(
        &text.lines().collect::<Vec<_>>()[..2],
        ["src/a.rs", "src/b.rs"]
    );
    assert!(
        notes(&text)
            .iter()
            .any(|n| n.contains("2 files (maxResults)")),
        "{text}"
    );
    assert!(truncated(&out));

    // count: `path\0count\n` per file, and the total when nothing was left out.
    let f = Fake::replay_raw(
        "{WORK}/src/a.rs\u{0}3\n{WORK}/src/b.rs\u{0}1\nnot an entry\n",
        0,
    );
    let out = f.call(json!({"pattern": "x", "count": true})).await;
    assert_eq!(
        ok(&out),
        "src/a.rs:3\nsrc/b.rs:1\ntotal: 4 matching lines in 2 files"
    );
    let out = f
        .call(json!({"pattern": "x", "count": true, "maxResults": 1}))
        .await;
    let text = ok(&out);
    assert!(text.starts_with("src/a.rs:3\n"), "{text}");
    assert!(
        !text.contains("total:"),
        "a total of part of the files would mislead"
    );
    assert!(truncated(&out));

    // no output: no match; exit 2 without entries is a refused pattern or glob; exit 2 with entries is an incomplete search.
    assert_eq!(
        ok(&Fake::replay_raw("", 1)
            .call(json!({"pattern": "x", "filesOnly": true}))
            .await),
        "no matches"
    );
    let (word, _) = failure(
        &Fake::replay_raw("", 2)
            .call(json!({"pattern": "x", "count": true}))
            .await,
    );
    assert_eq!(word, FailureWord::Usage);
    let text = ok(&Fake::replay_raw("{WORK}/src/a.rs\u{0}", 2)
        .call(json!({"pattern": "x", "filesOnly": true}))
        .await);
    assert!(text.starts_with("src/a.rs\n[search incomplete"), "{text}");
}

#[tokio::test]
async fn an_answer_stays_inside_what_the_server_will_not_cut() {
    // 200 matches of 290 characters would be 60 KB; the backend stops at its budget and says why, so that the note is not what the server cuts.
    let mut m = Vec::new();
    for f in 0..4 {
        let p = format!("{{WORK}}/src/f{f}.rs");
        m.push(j_begin(&p));
        for i in 0..50 {
            m.push(j_match(&p, i + 1, &"x".repeat(290), 0, 1));
        }
        m.push(j_end(&p));
    }
    let f = Fake::replay(&m, 0);
    let out = f
        .call(json!({"pattern": "x", "maxResults": 200, "maxPerFile": 50}))
        .await;
    let text = ok(&out);
    assert!(truncated(&out));
    assert!(
        ordered_hits(&text).len() < 200 && ordered_hits(&text).len() > 10,
        "{}",
        ordered_hits(&text).len()
    );
    assert!(
        notes(&text).iter().any(|n| n.contains("size")),
        "the size is named as the reason: {:?}",
        notes(&text)
    );
    let rendered = render(&out, 7_500);
    let wire = rendered.value["content"][0]["text"].as_str().unwrap();
    assert!(
        !wire.contains("[shortened:"),
        "the server had nothing left to cut"
    );
    assert!(text.encode_utf16().count() <= ANSWER_BUDGET + 400);
}

#[tokio::test]
async fn a_message_over_the_limit_is_skipped_and_said() {
    let p = "{WORK}/src/big.rs";
    let huge = format!(
        "{{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"{p}\"}},\"lines\":{{\"text\":\"{}\\n\"}},\"line_number\":9,\"absolute_offset\":0,\"submatches\":[]}}}}",
        "y".repeat(5 * 1024 * 1024)
    );
    let raw = [
        j_begin("{WORK}/src/a.rs"),
        j_match("{WORK}/src/a.rs", 1, "before", 0, 3),
        j_end("{WORK}/src/a.rs"),
        j_begin(p),
        huge,
        j_end(p),
        j_begin("{WORK}/src/z.rs"),
        j_match("{WORK}/src/z.rs", 2, "after", 0, 3),
        j_end("{WORK}/src/z.rs"),
        j_summary(),
    ]
    .join("\n");
    let f = Fake::replay_raw(&raw, 0);
    let out = f.call(json!({"pattern": "e"})).await;
    let text = ok(&out);
    assert!(text.contains("src/a.rs:1:before"), "{text}");
    assert!(
        text.contains("src/z.rs:2:after"),
        "the line after the long one is read: {text}"
    );
    assert!(
        notes(&text).iter().any(|n| n.contains("too long")),
        "{text}"
    );
    assert!(truncated(&out));
    assert!(
        !text.contains("yyyyyyyy"),
        "nothing of the long line is shown"
    );
}

// ---- the cases that came out of the review: odd input, odd output ---------------------------------------------------------------

#[tokio::test]
async fn the_backend_checks_its_own_input_and_work_tree() {
    let f = Fake::script("exit 1");
    assert_eq!(
        CodeSearch::default().name(),
        harness_protocol::tools::ToolName::CodeSearch
    );
    assert_eq!(
        CodeSearch::new().name(),
        harness_protocol::tools::ToolName::CodeSearch
    );
    // The server validates before the backend runs; a caller that does not gets the same words.
    for bad in [
        json!({"pattern": 5}),
        json!({"pattern": "x", "maxResults": 0}),
        json!({}),
    ] {
        let (word, detail) = failure(&f.call(bad).await);
        assert_eq!(word, FailureWord::Usage);
        assert!(!detail.is_empty());
    }
    // A work tree that is gone is not a search.
    let gone = f.base.join("no-such-work-tree");
    let ctx = ToolContext::new(
        gone.clone(),
        None,
        spec_fn(&gone, plain_path(), None, BTreeMap::new(), &f.base),
        Counters::new(),
    );
    let (word, detail) = failure(&search(&f.backend, &ctx, json!({"pattern": "x"})).await);
    assert_eq!(word, FailureWord::Unavailable);
    assert!(detail.contains("work tree"), "{detail}");
}

#[tokio::test]
async fn every_member_of_the_input_has_its_flag() {
    let f = Fake::script("printf '%s\\n' \"$@\" > '{BASE}/args.txt'\nexit 1");
    let out = f
        .call(json!({
            "pattern": "-x [a-z]+", "fixed": true, "includeHidden": true, "includeIgnored": true,
            "context": 2, "maxPerFile": 4, "glob": ["*.rs", "!gen/**", "-g"],
        }))
        .await;
    assert_eq!(ok(&out), "no matches");
    let args = f.record("args.txt").unwrap();
    let args: Vec<&str> = args.lines().collect();
    for flag in [
        "--fixed-strings",
        "--hidden",
        "--no-ignore",
        "--context=2",
        "--max-count=5",
        "--json",
    ] {
        assert!(args.contains(&flag), "{flag} in {args:?}");
    }
    let at = |a: &str| {
        args.iter()
            .position(|x| *x == a)
            .unwrap_or_else(|| panic!("{a} in {args:?}"))
    };
    assert!(
        at("--glob=!.git") < at("--glob=*.rs"),
        "ours first, so that the caller's can override: {args:?}"
    );
    assert!(at("--glob=*.rs") < at("--glob=!gen/**") && at("--glob=!gen/**") < at("--glob=-g"));
    let e = at("-e");
    assert_eq!(
        args[e + 1],
        "-x [a-z]+",
        "a pattern that starts with a dash is the argument of -e"
    );
    assert!(e < at("--"), "{args:?}");
}

#[tokio::test]
async fn ripgrep_that_closes_its_output_and_goes_on_running_is_stopped() {
    let f = Fake::script("echo $$ >> '{BASE}/pids.txt'\nexec 1>&-\nsleep 30");
    let t = Instant::now();
    let (word, detail) = failure(&f.call(json!({"pattern": "x"})).await);
    assert_eq!(word, FailureWord::Unavailable);
    assert!(detail.contains("did not finish"), "{detail}");
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    let p = pids(&f);
    assert_eq!(p.len(), 1, "TRIGGER ABSENT: the stand-in ran");
    assert!(
        until(Duration::from_secs(2), || p.iter().all(|x| gone(*x)))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn an_unexpected_exit_status_is_a_failure() {
    let f = Fake::script("exit 3");
    let (word, detail) = failure(&f.call(json!({"pattern": "x"})).await);
    assert_eq!(word, FailureWord::Unavailable);
    assert!(detail.contains("ripgrep failed"), "{detail}");
}

#[tokio::test]
async fn an_unreadable_directory_is_refused_and_a_missing_one_is_not() {
    use std::os::unix::fs::MetadataExt;
    if std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(1)
        == 0
    {
        eprintln!("SKIPPED: running as root, a directory with mode 000 is readable");
        return;
    }
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let f = Fake::script("echo started > '{BASE}/started.txt'\nexit 1");
    let locked = f.work.join("locked");
    std::fs::create_dir_all(locked.join("inner")).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _restore = Restore(locked);
    for entry in ["locked/inner", "locked/nope"] {
        let (word, detail) = failure(&f.call(json!({"pattern": "x", "paths": [entry]})).await);
        assert_eq!(word, FailureWord::Refused, "{entry}");
        assert!(
            detail.contains("could not be opened") && !detail.contains("locked"),
            "{detail}"
        );
    }
    assert!(f.record("started.txt").is_none());
}

#[tokio::test]
async fn odd_messages_are_ignored_and_the_rest_is_read() {
    let p = "{WORK}/src/odd.rs";
    let q = "{WORK}/src/open.rs";
    let r = "{WORK}/src/last.rs";
    let no_path =
        json!({"type":"match","data":{"lines":{"text":"x\n"},"line_number":1,"submatches":[]}})
            .to_string();
    let no_lines =
        json!({"type":"match","data":{"path":{"text":p},"line_number":2,"submatches":[]}})
            .to_string();
    let no_number =
        json!({"type":"match","data":{"path":{"text":p},"lines":{"text":"x\n"},"submatches":[]}})
            .to_string();
    let bad_base64 = json!({"type":"match","data":{"path":{"text":p},"lines":{"bytes":"!!!"},"line_number":3,"submatches":[]}}).to_string();
    // "aGl0Kw==" is the text "hit+"; "+/8=" is two bytes that are not UTF-8 (the two characters that only base64 has).
    let plus_slash = json!({"type":"match","data":{"path":{"text":p},"lines":{"bytes":"+/8="},"line_number":5,"submatches":[]}}).to_string();
    let odd_base64 = json!({"type":"match","data":{"path":{"text":p},"lines":{"bytes":"aGl0Kw=="},"line_number":4,"submatches":[{"start":0}]}}).to_string();
    let no_end_path = json!({"type":"end","data":{}}).to_string();
    let raw = [
        "this is not json".to_string(),
        r#"{"type":"unknown","data":{}}"#.to_string(),
        j_begin(p),
        no_path,
        no_lines,
        no_number,
        bad_base64,
        odd_base64,
        plus_slash,
        no_end_path,
        j_end(p),
        // A file that never gets its `end` is shown at the end of the stream; so is a second one.
        j_begin(q),
        j_match(q, 7, "open file", 0, 4),
        j_begin(r),
        j_match(r, 8, "last file", 0, 4),
        j_summary(),
    ]
    .join("\n");
    let f = Fake::replay_raw(&raw, 0);
    let text = ok(&f.call(json!({"pattern": "x"})).await);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            "src/odd.rs:4:hit+",
            "src/odd.rs:5:\u{fffd}\u{fffd}",
            "src/last.rs:8:last file",
            "src/open.rs:7:open file"
        ],
        "{text}"
    );
    // The stream is cut at the total even among files that never ended.
    let out = f.call(json!({"pattern": "x", "maxResults": 2})).await;
    let text = ok(&out);
    assert_eq!(ordered_hits(&text).len(), 2, "{text}");
    assert!(
        notes(&text)
            .iter()
            .any(|n| n.contains("2 matches (maxResults)")),
        "{text}"
    );
    assert!(truncated(&out));
}

#[tokio::test]
async fn a_match_past_the_per_file_cap_is_only_context_near_the_last_kept_match() {
    let p = "{WORK}/src/c.rs";
    let f = Fake::replay(
        &[
            j_begin(p),
            j_match(p, 10, "first hit", 6, 9),
            j_match(p, 11, "second hit", 7, 10),
            j_match(p, 12, "third hit", 6, 9),
            j_context(p, 13, "after"),
            j_match(p, 40, "far hit", 4, 7),
            j_end(p),
        ],
        0,
    );
    let out = f
        .call(json!({"pattern": "hit", "maxPerFile": 2, "context": 1}))
        .await;
    let text = ok(&out);
    assert_eq!(
        text.lines().take(3).collect::<Vec<_>>(),
        [
            "src/c.rs:10:first hit",
            "src/c.rs:11:second hit",
            "src/c.rs-12-third hit"
        ],
        "the third match is shown as context of the second, the far one not at all: {text}"
    );
    assert!(
        !text.contains("far hit") && !text.contains("after"),
        "{text}"
    );
    assert!(truncated(&out));
}

#[tokio::test]
async fn two_oversize_messages_and_one_without_an_end_of_line() {
    let big = |path: &str, n: u64| {
        format!(
            "{{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"{path}\"}},\"lines\":{{\"text\":\"{}\\n\"}},\"line_number\":{n},\"absolute_offset\":0,\"submatches\":[]}}}}",
            "z".repeat(4 * 1024 * 1024 + 10)
        )
    };
    let raw = [
        big("{WORK}/src/one.rs", 1),
        j_match("{WORK}/src/ok.rs", 2, "fine", 0, 4),
        j_end("{WORK}/src/ok.rs"),
        // the last message has no end of line
        big("{WORK}/src/two.rs", 3),
    ]
    .join("\n");
    let f = Fake::replay_raw(&raw, 0);
    let out = f.call(json!({"pattern": "x"})).await;
    let text = ok(&out);
    assert!(text.contains("src/ok.rs:2:fine"), "{text}");
    assert!(
        notes(&text)
            .iter()
            .any(|n| n.contains("2 lines too long to show were skipped")),
        "{text}"
    );
}

// ---- the package's cases: modes ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn modes() {
    let Some(rg) = real_rg() else { return };
    let work = canon(&common::scratch("rg-modes"));
    tree::generate(&work, 2_000, true);
    // A file of the repository's own metadata that holds the token: even includeHidden leaves `.git` alone.
    std::fs::write(
        work.join(".git/notes.txt"),
        format!("let leaked = \"{}\";\n", tree::RARE),
    )
    .unwrap();
    let ctx = context(&work, None, plain_path());
    let backend = CodeSearch::with_program(rg.display().to_string());
    let all = json!({"maxResults": 200, "maxPerFile": 50});
    let with = |extra: Value| {
        let mut v = all.clone();
        v["pattern"] = json!(tree::RARE);
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };

    let plain = hits(&ok(&search(&backend, &ctx, with(json!({}))).await));
    assert_eq!(plain.len(), 5, "TRIGGER ABSENT: five visible files");

    // filesOnly lists names, once each, in path order.
    let files = ok(&search(&backend, &ctx, with(json!({"filesOnly": true}))).await);
    let names: Vec<&str> = files.lines().collect();
    let want: BTreeSet<String> = plain.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(
        names.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        want
    );
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "in path order");

    // count lists a count per file.
    let counts = ok(&search(&backend, &ctx, with(json!({"count": true}))).await);
    for p in &want {
        assert!(counts.contains(&format!("{p}:1")), "{counts}");
    }
    assert!(counts.contains("total"), "{counts}");

    // includeHidden adds exactly the hidden files, and never `.git`.
    let hidden = hits(&ok(&search(
        &backend,
        &ctx,
        with(json!({"includeHidden": true})),
    )
    .await));
    let added: BTreeSet<String> = hidden.difference(&plain).map(|(p, _)| p.clone()).collect();
    assert_eq!(
        added,
        tree::HIDDEN.iter().map(|s| s.to_string()).collect(),
        "{added:?}"
    );
    assert!(
        !hidden.iter().any(|(p, _)| p.starts_with(".git/")),
        "the repository's metadata is not searched"
    );
    assert_eq!(
        hidden,
        by_hand(&rg, &work, tree::RARE, &["--hidden", "--glob=!.git"])
    );

    // includeIgnored adds exactly the ignored files.
    let ignored = hits(&ok(&search(
        &backend,
        &ctx,
        with(json!({"includeIgnored": true})),
    )
    .await));
    let added: BTreeSet<String> = ignored.difference(&plain).map(|(p, _)| p.clone()).collect();
    assert_eq!(
        added,
        tree::IGNORED.iter().map(|s| s.to_string()).collect(),
        "{added:?}"
    );
    // Neither the binary file nor the one over the size limit is ever searched.
    let both = ok(&search(
        &backend,
        &ctx,
        with(json!({"includeIgnored": true, "includeHidden": true})),
    )
    .await);
    assert!(
        !both.contains("blob.dat") && !both.contains("huge.txt"),
        "{both}"
    );

    // glob: an include and an exclude, and paths, on a token that is spread over the directories (154 files, under the cap).
    let medium = |extra: Value| {
        let mut v = with(extra);
        v["pattern"] = json!(tree::MEDIUM);
        v
    };
    let spread = hits(&ok(&search(&backend, &ctx, medium(json!({}))).await));
    assert_eq!(
        spread.len(),
        154,
        "TRIGGER ABSENT: the token is in 154 files"
    );
    let only = hits(&ok(&search(
        &backend,
        &ctx,
        medium(json!({"glob": ["d00/**"]})),
    )
    .await));
    assert!(
        only.iter().all(|(p, _)| p.starts_with("d00/")) && only.len() == 4,
        "{only:?}"
    );
    let less = hits(&ok(&search(
        &backend,
        &ctx,
        medium(json!({"glob": ["!d00/**"]})),
    )
    .await));
    assert!(less.iter().all(|(p, _)| !p.starts_with("d00/")), "{less:?}");
    assert_eq!(only.len() + less.len(), spread.len());
    let narrow = hits(&ok(&search(
        &backend,
        &ctx,
        medium(json!({"paths": ["d00", "d10"]})),
    )
    .await));
    assert!(
        !narrow.is_empty()
            && narrow
                .iter()
                .all(|(p, _)| p.starts_with("d00/") || p.starts_with("d10/")),
        "{narrow:?}"
    );

    // context lines come as path-line-text, with a separator between groups.
    std::fs::write(
        work.join("ctx.txt"),
        "one\ntwo\nthree TARGET\nfour TARGET\nfive\nsix\nseven\neight\nnine TARGET\nten\n",
    )
    .unwrap();
    let c = ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "TARGET", "paths": ["ctx.txt"], "context": 1}),
    )
    .await);
    let want = [
        "ctx.txt-2-two",
        "ctx.txt:3:three TARGET",
        "ctx.txt:4:four TARGET",
        "ctx.txt-5-five",
        "--",
        "ctx.txt-8-eight",
        "ctx.txt:9:nine TARGET",
        "ctx.txt-10-ten",
    ];
    assert_eq!(c.lines().collect::<Vec<_>>(), want, "{c}");

    // fixed treats the pattern as text.
    std::fs::write(work.join("lit.txt"), "abc\na.c\na*c\n").unwrap();
    let regex = hits(&ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "a.c", "paths": ["lit.txt"]}),
    )
    .await));
    assert_eq!(
        regex.len(),
        3,
        "as a regular expression a.c matches abc, a.c and a*c"
    );
    let fixed = ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "a.c", "paths": ["lit.txt"], "fixed": true}),
    )
    .await);
    assert_eq!(
        fixed.lines().collect::<Vec<_>>(),
        ["lit.txt:2:a.c"],
        "{fixed}"
    );
    let star = ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "a*c", "paths": ["lit.txt"], "fixed": true}),
    )
    .await);
    assert_eq!(
        star.lines().collect::<Vec<_>>(),
        ["lit.txt:3:a*c"],
        "{star}"
    );
    // a pattern that starts with a dash is a pattern.
    std::fs::write(work.join("dash.txt"), "-L is a flag\n").unwrap();
    let dash = ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "-L", "paths": ["dash.txt"], "fixed": true}),
    )
    .await);
    assert_eq!(
        dash.lines().collect::<Vec<_>>(),
        ["dash.txt:1:-L is a flag"]
    );
}

// ---- the package's cases: paths -------------------------------------------------------------------------------------------------------

/// The directories and links of the confinement cases, below a fresh scratch directory:
/// `work/` (the tree: `src/lib.rs`, `.git/`, links), `outside/` (a secret and a directory), `ctx/` (the context directory), `work-evil/`.
struct Confine {
    base: PathBuf,
    work: PathBuf,
    outside: PathBuf,
    ctx: PathBuf,
}

fn confine_fixture() -> Confine {
    let base = canon(&common::scratch("rg-confine"));
    let (work, outside, ctx) = (base.join("work"), base.join("outside"), base.join("ctx"));
    for d in [
        work.join("src"),
        work.join(".git"),
        work.join("a & b;c $HOME dir"),
        outside.join("dir"),
        ctx.clone(),
        base.join("work-evil"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        work.join("src/lib.rs"),
        "pub fn inside() {} // INSIDE_CANARY\n",
    )
    .unwrap();
    std::fs::write(
        work.join("a & b;c $HOME dir/x.txt"),
        "weird INSIDE_CANARY\n",
    )
    .unwrap();
    std::fs::write(outside.join("secret.txt"), "OUTSIDE_CANARY\n").unwrap();
    std::fs::write(outside.join("dir/inner.txt"), "OUTSIDE_CANARY\n").unwrap();
    std::fs::write(ctx.join("notes.md"), "CONTEXT_CANARY\n").unwrap();
    std::fs::write(base.join("work-evil/x.txt"), "OUTSIDE_CANARY\n").unwrap();
    use std::os::unix::fs::symlink;
    symlink("../outside/secret.txt", work.join("link_file")).unwrap();
    symlink("../outside/dir", work.join("link_dir")).unwrap();
    symlink(&outside, work.join("abs_link")).unwrap();
    symlink("src", work.join("inside_link")).unwrap();
    symlink(work.join("src"), work.join("abs_inside_link")).unwrap();
    symlink("loop_b", work.join("loop_a")).unwrap();
    symlink("loop_a", work.join("loop_b")).unwrap();
    Confine {
        base,
        work,
        outside,
        ctx,
    }
}

type Outcome = Result<PathBuf, Refusal>;

fn confine_table(c: &Confine) -> Vec<(String, Outcome)> {
    let w = &c.work;
    let abs = |p: &Path| p.display().to_string();
    let mut t: Vec<(String, Outcome)> = Vec::new();
    for (entry, want) in [
        (".", Ok(w.clone())),
        ("src", Ok(w.join("src"))),
        ("src/", Ok(w.join("src"))),
        ("./src", Ok(w.join("src"))),
        ("src/../src", Ok(w.join("src"))),
        ("src/lib.rs", Ok(w.join("src/lib.rs"))),
        ("inside_link", Ok(w.join("src"))),
        ("inside_link/lib.rs", Ok(w.join("src/lib.rs"))),
        ("a & b;c $HOME dir", Ok(w.join("a & b;c $HOME dir"))),
        ("src/lib.rs/", Err(Refusal::Missing)),
        ("nope.rs", Err(Refusal::Missing)),
        ("src/nope", Err(Refusal::Missing)),
        ("..", Err(Refusal::Outside)),
        ("../outside", Err(Refusal::Outside)),
        ("../outside/secret.txt", Err(Refusal::Outside)),
        ("src/../../outside/secret.txt", Err(Refusal::Outside)),
        ("../ctx", Err(Refusal::Outside)),
        ("../ctx/notes.md", Err(Refusal::Outside)),
        ("link_file", Err(Refusal::Outside)),
        ("link_dir", Err(Refusal::Outside)),
        ("link_dir/inner.txt", Err(Refusal::Outside)),
        ("abs_link", Err(Refusal::Outside)),
        ("abs_link/secret.txt", Err(Refusal::Outside)),
        // Strict resolution: an absolute link is refused even when its target is inside, the way the kernel's RESOLVE_BENEATH refuses it.
        ("abs_inside_link", Err(Refusal::Outside)),
        ("loop_a", Err(Refusal::Outside)),
    ] {
        t.push((entry.to_string(), want));
    }
    for (entry, want) in [
        (abs(w), Ok(w.clone())),
        (abs(&w.join("src")), Ok(w.join("src"))),
        (abs(&w.join("src/lib.rs")), Ok(w.join("src/lib.rs"))),
        (abs(&w.join("src/../src")), Ok(w.join("src"))),
        (format!("{}/", abs(&w.join("src"))), Ok(w.join("src"))),
        (abs(&c.ctx), Ok(c.ctx.clone())),
        (abs(&c.ctx.join("notes.md")), Ok(c.ctx.join("notes.md"))),
        (abs(&c.outside.join("secret.txt")), Err(Refusal::Outside)),
        (abs(&c.base.join("work-evil/x.txt")), Err(Refusal::Outside)),
        (abs(&w.join("../outside/secret.txt")), Err(Refusal::Outside)),
        (abs(&c.ctx.join("../outside/dir")), Err(Refusal::Outside)),
        (abs(&w.join("link_file")), Err(Refusal::Outside)),
        (abs(&w.join("nope")), Err(Refusal::Missing)),
        ("/etc/passwd".to_string(), Err(Refusal::Outside)),
        ("/".to_string(), Err(Refusal::Outside)),
    ] {
        t.push((entry, want));
    }
    t
}

#[test]
fn paths_are_confined() {
    let c = confine_fixture();
    let roots = Roots::new(&c.work, Some(&c.ctx)).unwrap();
    let table = confine_table(&c);
    assert_eq!(roots.work(), c.work);
    assert_eq!(roots.context(), Some(c.ctx.as_path()));
    // A context directory that does not exist is no root.
    let none = Roots::new(&c.work, Some(&c.base.join("no-such-context"))).unwrap();
    assert_eq!(none.context(), None);
    assert!(Roots::new(&c.base.join("no-such-work-tree"), None).is_err());
    let mut strategies = vec![Strategy::Auto, Strategy::Walk];
    if confine::openat2_available() {
        strategies.push(Strategy::Openat2);
    } else {
        eprintln!("SKIPPED strategy openat2: the kernel or the sandbox does not offer it");
    }
    for strategy in strategies {
        for (entry, want) in &table {
            let got = roots.resolve(entry, strategy).map(|r| r.path);
            assert_eq!(
                &got, want,
                "strategy {strategy:?}, entry {entry:?}: the table says {want:?}"
            );
        }
    }
    // The root a path lands in.
    let r = roots
        .resolve(&c.ctx.display().to_string(), Strategy::Auto)
        .unwrap();
    assert_eq!(r.root, RootKind::Context);
    let r = roots.resolve("src", Strategy::Auto).unwrap();
    assert_eq!(r.root, RootKind::Work);
    // A tree whose own path goes through a link is the same tree: an entry given in either form resolves.
    let alias = c.base.join("work_alias");
    std::os::unix::fs::symlink(&c.work, &alias).unwrap();
    let roots = Roots::new(&alias, None).unwrap();
    for strategy in [Strategy::Auto, Strategy::Walk] {
        let given = alias.join("src/lib.rs").display().to_string();
        assert_eq!(
            roots.resolve(&given, strategy).map(|r| r.path),
            Ok(c.work.join("src/lib.rs"))
        );
    }
    // An empty context directory name is no root: the context directory is optional.
    let roots = Roots::new(&c.work, None).unwrap();
    assert_eq!(
        roots
            .resolve(&c.ctx.display().to_string(), Strategy::Auto)
            .map(|r| r.path),
        Err(Refusal::Outside)
    );
}

#[test]
fn the_strategies_agree_on_every_entry_of_a_generated_set() {
    // Every combination of up to three components out of a small alphabet that holds names, a link, `..` and `.`: the user-space walk and the
    // kernel's `openat2` (where there is one) refuse and accept the same entries.
    if !confine::openat2_available() {
        eprintln!("SKIPPED: no openat2, nothing to compare the walk with");
        return;
    }
    let c = confine_fixture();
    let roots = Roots::new(&c.work, Some(&c.ctx)).unwrap();
    let alphabet = [
        "src",
        "..",
        ".",
        "inside_link",
        "link_dir",
        "lib.rs",
        "nope",
        "loop_a",
        "abs_link",
    ];
    let mut compared = 0;
    for a in alphabet {
        for b in alphabet {
            for d in ["", "lib.rs", ".."] {
                let entry = [a, b, d]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("/");
                let walk = roots.resolve(&entry, Strategy::Walk);
                let kernel = roots.resolve(&entry, Strategy::Openat2);
                assert_eq!(walk, kernel, "entry {entry:?}");
                compared += 1;
            }
        }
    }
    assert!(compared > 200);
}

#[tokio::test]
async fn a_refused_path_never_starts_rg_and_names_no_path() {
    let f = Fake::script("echo started > '{BASE}/started.txt'\nexit 1");
    let outside = f.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("s.txt"), "OUTSIDE_CANARY\n").unwrap();
    std::os::unix::fs::symlink("../outside", f.work.join("out_link")).unwrap();
    for (paths, word) in [
        (json!(["out_link"]), FailureWord::Refused),
        (json!(["src", "out_link/s.txt"]), FailureWord::Refused),
        (json!([".."]), FailureWord::Refused),
        (json!([outside.display().to_string()]), FailureWord::Refused),
        (json!(["no_such_dir_canary"]), FailureWord::Usage),
    ] {
        let out = f.call(json!({"pattern": "x", "paths": paths})).await;
        let (got, detail) = failure(&out);
        assert_eq!(got, word, "{paths}");
        assert!(
            !detail.contains("out_link") && !detail.contains("canary") && !detail.contains("s.txt"),
            "the text names no path: {detail:?}"
        );
        assert!(
            f.record("started.txt").is_none(),
            "{paths}: rg was started for a refused path"
        );
    }
}

#[tokio::test]
async fn links_and_traversal_return_nothing_from_outside_with_the_real_rg() {
    let Some(rg) = real_rg() else { return };
    let c = confine_fixture();
    let ctx = context(&c.work, Some(&c.ctx), plain_path());
    let backend = CodeSearch::with_program(rg.display().to_string());
    // Searching the whole tree meets the links and does not follow them.
    let all = ok(&search(&backend, &ctx, json!({"pattern": "OUTSIDE_CANARY"})).await);
    assert_eq!(all, "no matches", "{all}");
    let inside = ok(&search(&backend, &ctx, json!({"pattern": "INSIDE_CANARY"})).await);
    assert!(inside.contains("src/lib.rs:1:"), "{inside}");
    // The literal name with `&`, `;`, a space and `$HOME` arrived whole.
    let weird = ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "INSIDE_CANARY", "paths": ["a & b;c $HOME dir"]}),
    )
    .await);
    assert!(
        weird.contains("a & b;c $HOME dir/x.txt:1:weird INSIDE_CANARY"),
        "{weird}"
    );
    // A path in the context directory is allowed and shows as an absolute path.
    let notes = ok(&search(
        &backend,
        &ctx,
        json!({"pattern": "CONTEXT_CANARY", "paths": [c.ctx.display().to_string()]}),
    )
    .await);
    assert_eq!(
        notes,
        format!("{}:1:CONTEXT_CANARY", c.ctx.join("notes.md").display())
    );
    // A path that leaves both is refused as a whole.
    for paths in [
        json!(["link_file"]),
        json!(["src", "link_dir"]),
        json!(["../outside"]),
    ] {
        let out = search(
            &backend,
            &ctx,
            json!({"pattern": "OUTSIDE_CANARY", "paths": paths}),
        )
        .await;
        assert_eq!(failure(&out).0, FailureWord::Refused, "{paths}");
    }
}

#[tokio::test]
async fn paths_are_sorted_deduplicated_and_without_overlap() {
    let f = Fake::script("printf '%s\\n' \"$@\" > '{BASE}/args.txt'\nexit 1");
    for d in ["d00/s01", "d01"] {
        std::fs::create_dir_all(f.work.join(d)).unwrap();
    }
    let out = f
        .call(json!({"pattern": "x", "paths": ["d01", "d00", "d00", "d00/s01", "./d01/"]}))
        .await;
    assert_eq!(ok(&out), "no matches");
    let args = f.record("args.txt").unwrap();
    let after: Vec<&str> = args.lines().skip_while(|l| *l != "--").skip(1).collect();
    assert_eq!(
        after,
        [
            f.work.join("d00").display().to_string(),
            f.work.join("d01").display().to_string()
        ]
    );
}

#[test]
fn the_kernel_offers_openat2_or_the_walk_is_used() {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    eprintln!(
        "OPENAT2 available={} kernel={}",
        confine::openat2_available(),
        kernel.trim()
    );
}

// ---- the package's cases: the child -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_child_runs_as_the_engine_user_with_a_clean_environment() {
    let mut env = BTreeMap::new();
    env.insert("HAMH_TEST_MARK".to_string(), "from-the-spec".to_string());
    let f = Fake::build(
        "{ echo \"uid=$(id -u)\"; echo \"cwd=$(pwd)\"; echo args:; printf '%s\\n' \"$@\"; echo env:; env | sort; } > '{BASE}/rg.txt'\nexit 1",
        Some((4242, 4343)),
        env,
        Limits::default(),
    );
    // A stand-in `setpriv` that records its arguments and runs the rest, so the uid split is visible without root.
    write_exec(
        &f.bin.join("setpriv"),
        &format!(
            "#!/bin/sh\necho \"$*\" > '{}/setpriv.txt'\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nexec \"$@\"\n",
            f.base.display()
        ),
    );
    let out = f.call(json!({"pattern": "needle"})).await;
    assert_eq!(ok(&out), "no matches");
    let setpriv = f
        .record("setpriv.txt")
        .expect("the child went through the launch wrapper");
    assert!(
        setpriv.starts_with("--reuid 4242 --regid 4343 --clear-groups --no-new-privs --"),
        "{setpriv}"
    );
    let rg = f.record("rg.txt").unwrap();
    let (head, env_part) = rg.split_once("env:\n").unwrap();
    assert!(
        head.contains(&format!("cwd={}", f.work.display())),
        "{head}"
    );
    let args: Vec<&str> = head.split("args:\n").nth(1).unwrap().lines().collect();
    for must in [
        "--json",
        "--no-config",
        "--no-follow",
        "--sort=path",
        "--max-filesize=2M",
    ] {
        assert!(args.contains(&must), "{must} is in {args:?}");
    }
    assert!(
        !args
            .iter()
            .any(|a| *a == "-L" || *a == "--follow" || a.starts_with("-L")),
        "a link is never followed: {args:?}"
    );
    assert!(
        args.contains(&"--max-count=11"),
        "ripgrep is asked for one more than maxPerFile (10), so that a file that has more is known: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.starts_with("--context")),
        "no context unless asked: {args:?}"
    );
    let e = args.iter().position(|a| *a == "-e").unwrap();
    assert_eq!(args[e + 1], "needle", "the pattern is the argument of -e");
    let dash = args.iter().position(|a| *a == "--").unwrap();
    assert!(dash > e, "paths come after --");
    assert_eq!(args[dash + 1], f.work.display().to_string());
    assert_eq!(args.len(), dash + 2, "one path: the work tree");
    let keys: BTreeSet<&str> = env_part
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    assert!(!keys.iter().any(|k| k.starts_with("AWS_")), "{keys:?}");
    assert!(
        keys.contains("HAMH_TEST_MARK") && keys.contains("PATH"),
        "{keys:?}"
    );
    for k in &keys {
        assert!(
            [
                "PATH",
                "LANG",
                "TERM",
                "HAMH_TEST_MARK",
                "PWD",
                "SHLVL",
                "_",
                "OLDPWD"
            ]
            .contains(k),
            "an unexpected variable {k} reached the child: {keys:?}"
        );
    }
}

#[test]
fn the_limits_are_the_packages() {
    let l = Limits::default();
    assert_eq!(l.wall, Duration::from_secs(10));
    assert_eq!(l.concurrent, 4);
    assert_eq!(l.max_message_bytes, 4 * 1024 * 1024);
}

fn sleeper_script() -> &'static str {
    "echo $$ >> '{BASE}/pids.txt'\nsleep 30 &\necho $! >> '{BASE}/pids.txt'\nwait"
}

fn pids(f: &Fake) -> Vec<u32> {
    f.record("pids.txt")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[tokio::test]
async fn a_wall_limit_kills_the_group() {
    let f = Fake::build(
        sleeper_script(),
        None,
        BTreeMap::new(),
        Limits {
            wall: Duration::from_millis(400),
            ..Limits::default()
        },
    );
    let t = Instant::now();
    let out = f.call(json!({"pattern": "x"})).await;
    let took = t.elapsed();
    let (word, detail) = failure(&out);
    assert_eq!(word, FailureWord::Timeout);
    assert!(detail.contains("limit"), "{detail}");
    assert!(took >= Duration::from_millis(400), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    let p = pids(&f);
    assert_eq!(
        p.len(),
        2,
        "TRIGGER ABSENT: the shell and its child ran: {p:?}"
    );
    assert!(
        until(Duration::from_secs(2), || p.iter().all(|x| gone(*x)))
            .await
            .is_some(),
        "a process of the group is left: {p:?}"
    );
}

#[tokio::test]
async fn cancel_kills_the_child_within_one_second() {
    let f = Fake::script(sleeper_script());
    let token = CancelToken::new();
    let call = {
        let (b, c, t) = (f.backend.clone(), f.ctx.clone(), token.clone());
        tokio::spawn(async move { b.call(&c, json!({"pattern": "x"}), t).await })
    };
    assert!(
        until(Duration::from_secs(5), || pids(&f).len() == 2)
            .await
            .is_some(),
        "TRIGGER ABSENT: the search never started"
    );
    let t = Instant::now();
    token.cancel();
    let out = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .expect("the call ended within a second of the cancel")
        .unwrap();
    assert!(matches!(out, ToolOutput::Failed { .. }));
    let p = pids(&f);
    assert!(
        until(Duration::from_secs(1), || p.iter().all(|x| gone(*x)))
            .await
            .is_some(),
        "{p:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(1) + Duration::from_millis(100));
}

#[tokio::test]
async fn four_at_once_and_the_fifth_is_busy() {
    let f = Fake::script(sleeper_script());
    let tokens: Vec<CancelToken> = (0..4).map(|_| CancelToken::new()).collect();
    let calls: Vec<_> = tokens
        .iter()
        .map(|t| {
            let (b, c, t) = (f.backend.clone(), f.ctx.clone(), t.clone());
            tokio::spawn(async move { b.call(&c, json!({"pattern": "x"}), t).await })
        })
        .collect();
    assert!(
        until(Duration::from_secs(10), || pids(&f).len() == 8)
            .await
            .is_some(),
        "TRIGGER ABSENT: four searches never ran at once: {:?}",
        pids(&f)
    );
    let t = Instant::now();
    let fifth = f.call(json!({"pattern": "x"})).await;
    assert_eq!(failure(&fifth).0, FailureWord::Busy);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "busy is immediate"
    );
    for t in &tokens {
        t.cancel();
    }
    for c in calls {
        tokio::time::timeout(Duration::from_secs(2), c)
            .await
            .unwrap()
            .unwrap();
    }
    let p = pids(&f);
    assert!(
        until(Duration::from_secs(2), || p.iter().all(|x| gone(*x)))
            .await
            .is_some()
    );
    // The slots are free again.
    let token = CancelToken::new();
    let again = {
        let (b, c, t) = (f.backend.clone(), f.ctx.clone(), token.clone());
        tokio::spawn(async move { b.call(&c, json!({"pattern": "x"}), t).await })
    };
    assert!(
        until(Duration::from_secs(5), || pids(&f).len() == 10)
            .await
            .is_some()
    );
    token.cancel();
    tokio::time::timeout(Duration::from_secs(2), again)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn rg_failures_are_told_in_fixed_words() {
    let canary = "CANARY-STDERR-9c1e";
    // exit 2 with no summary: ripgrep did not accept the pattern or a glob.
    let f = Fake::script(&format!(
        "echo 'rg: regex parse error: {canary}' >&2\nexit 2"
    ));
    let (word, detail) = failure(&f.call(json!({"pattern": "(unclosed"})).await);
    assert_eq!(word, FailureWord::Usage);
    assert!(detail.contains("pattern"), "{detail}");
    assert!(
        !detail.contains(canary) && !detail.contains("unclosed"),
        "{detail}"
    );

    // exit 2 after a finished search: some files could not be read; what was found is real.
    let f = Fake::replay(
        &[
            j_begin("{WORK}/src/a.rs"),
            j_match("{WORK}/src/a.rs", 1, "found it", 0, 5),
            j_end("{WORK}/src/a.rs"),
        ],
        2,
    );
    let out = f.call(json!({"pattern": "found"})).await;
    let text = ok(&out);
    assert!(text.contains("src/a.rs:1:found it"), "{text}");
    assert!(
        notes(&text).iter().any(|n| n.contains("incomplete")),
        "{text}"
    );
    let f = Fake::replay_raw(&j_summary(), 2);
    let text = ok(&f.call(json!({"pattern": "found"})).await);
    assert!(text.starts_with("no matches"), "{text}");
    assert!(
        notes(&text).iter().any(|n| n.contains("incomplete")),
        "{text}"
    );

    // exit 1 is no match, and the shell's 127 (no such program behind the wrapper) is not installed.
    let f = Fake::replay_raw(&j_summary(), 1);
    assert_eq!(ok(&f.call(json!({"pattern": "x"})).await), "no matches");
    let f = Fake::script("exit 127");
    let (word, detail) = failure(&f.call(json!({"pattern": "x"})).await);
    assert_eq!(word, FailureWord::Unavailable);
    assert!(detail.contains("ripgrep"), "{detail}");
    // A signal nobody sent (an out-of-memory kill) is a fault, and what was read before it is not shown.
    let f = Fake::script("cat '{BASE}/nothing' 2>/dev/null\nkill -9 $$");
    let (word, _) = failure(&f.call(json!({"pattern": "x"})).await);
    assert_eq!(word, FailureWord::Unavailable);
    // No program at all.
    let f = Fake::script("exit 0");
    let missing = CodeSearch::with_program("/no/such/rg/binary");
    let (word, detail) = failure(&search(&missing, &f.ctx, json!({"pattern": "x"})).await);
    assert_eq!(word, FailureWord::Unavailable);
    assert!(detail.contains("ripgrep"), "{detail}");
}

#[tokio::test]
async fn a_pattern_of_more_than_one_line_is_a_usage_error_without_a_child() {
    let f = Fake::script("echo started > '{BASE}/started.txt'\nexit 1");
    for pattern in ["a\nb", "a\rb"] {
        let (word, detail) = failure(&f.call(json!({"pattern": pattern})).await);
        assert_eq!(word, FailureWord::Usage);
        assert!(detail.contains("one line"), "{detail}");
    }
    assert!(f.record("started.txt").is_none());
}

// ---- the package's cases: through the server --------------------------------------------------------------------------------------------

struct Served {
    shared: Arc<Shared>,
    handle: ServerHandle,
    log: Arc<MemoryLog>,
    fake: Fake,
}

async fn serve(fake: Fake) -> Served {
    let log = MemoryLog::new();
    let ctx = ToolContext::new(
        fake.work.clone(),
        None,
        spec_fn(
            &fake.work,
            {
                let mut d = vec![fake.bin.clone()];
                d.extend(plain_path());
                d
            },
            None,
            BTreeMap::new(),
            &fake.base,
        ),
        Counters::new(),
    );
    let backend = CodeSearch::with_program("rg");
    let shared = Shared::new(vec![Box::new(backend)], ctx, log.clone(), 7_500);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = server::start(listener, shared.clone());
    Served {
        shared,
        handle,
        log,
        fake,
    }
}

impl Served {
    async fn call(&self, arguments: Value) -> Value {
        let r = rpc(
            self.handle.addr(),
            &request(
                json!(1),
                "tools/call",
                json!({"name": "code_search", "arguments": arguments}),
            ),
        )
        .await;
        r.json()
    }
}

#[tokio::test]
async fn invalid_input_is_a_plain_usage_error() {
    let s = serve(Fake::script("echo started > '{BASE}/started.txt'\nexit 1")).await;
    for (arguments, what) in [
        (
            json!({"pattern": "x".repeat(1001)}),
            "a 1,001-character pattern",
        ),
        (
            json!({"pattern": "ok", "maxResults": 201}),
            "maxResults 201",
        ),
        (json!({"pattern": "ok", "bogus": 1}), "an unknown member"),
    ] {
        let answer = s.call(arguments).await;
        let text = text_of(&answer);
        assert!(text.starts_with("error: usage:"), "{what}: {text}");
        assert_eq!(answer["result"]["isError"], true, "{what}");
        assert!(
            !text.contains("xxxxxxxxxx") && !text.contains("bogus"),
            "{what}: {text}"
        );
    }
    assert!(s.fake.record("started.txt").is_none(), "rg never ran");
}

#[tokio::test]
async fn the_pattern_never_reaches_a_log() {
    const PATTERN: &str = "CANARY-PATTERN-7f3a";
    const PATH: &str = "canary-dir-7f3a";
    const STDERR: &str = "CANARY-STDERR-7f3a";
    // (what happens, the fake rg's script, the arguments of the call)
    let cases: Vec<(&str, String, Value)> = vec![
        (
            "no match",
            "exit 1".to_string(),
            json!({"pattern": PATTERN}),
        ),
        (
            "rg refuses the pattern",
            format!("echo 'rg: regex parse error: {PATTERN} {STDERR}' >&2\nexit 2"),
            json!({"pattern": PATTERN}),
        ),
        (
            "a refused path",
            "exit 1".to_string(),
            json!({"pattern": PATTERN, "paths": [format!("../{PATH}")]}),
        ),
        (
            "a missing path",
            "exit 1".to_string(),
            json!({"pattern": PATTERN, "paths": [PATH]}),
        ),
        (
            "a signal",
            "echo \"$@\" >&2\nkill -9 $$".to_string(),
            json!({"pattern": PATTERN}),
        ),
    ];
    for (name, script, arguments) in cases {
        let s = serve(Fake::script(&script)).await;
        let answer = s.call(arguments).await;
        let text = text_of(&answer);
        let counters = serde_json::to_string(&s.shared.stats().to_counts()).unwrap();
        let log = s.log.lines().join("\n");
        for (where_, haystack) in [
            ("the answer", &text),
            ("a log line", &log),
            ("a counter", &counters),
        ] {
            for canary in [PATTERN, PATH, STDERR] {
                assert!(
                    !haystack.contains(canary),
                    "{name}: {canary} is in {where_}: {haystack}"
                );
            }
        }
        assert!(
            tree_holds_no(&s.fake.base, STDERR),
            "{name}: the standard error of rg is on disk"
        );
        s.handle.stop().await;
    }
    // A search that finds the pattern shows it in the answer (that is its result) and nowhere else.
    let p = "{WORK}/src/a.rs";
    let s = serve(Fake::replay(
        &[
            j_begin(p),
            j_match(p, 3, &format!("let s = \"{PATTERN}\";"), 9, 28),
            j_end(p),
        ],
        0,
    ))
    .await;
    let answer = s.call(json!({"pattern": PATTERN})).await;
    assert!(
        text_of(&answer).contains(PATTERN),
        "the result is the result"
    );
    let counters = serde_json::to_string(&s.shared.stats().to_counts()).unwrap();
    assert!(!s.log.lines().join("\n").contains(PATTERN) && !counters.contains(PATTERN));
    s.handle.stop().await;
}

/// Whether no file below `dir` holds `needle` (the stand-in programs in `bin` are the test's own text and are not looked at).
fn tree_holds_no(dir: &Path, needle: &str) -> bool {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.file_name().is_some_and(|n| n == "bin") {
            continue;
        }
        if p.is_dir() {
            if !tree_holds_no(&p, needle) {
                return false;
            }
        } else if std::fs::read(&p)
            .map(|b| String::from_utf8_lossy(&b).contains(needle))
            .unwrap_or(false)
        {
            return false;
        }
    }
    true
}

// ---- timing ------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_ten_thousand_file_tree_is_searched_fast() {
    let Some(rg) = real_rg() else { return };
    let work = canon(&common::scratch("rg-10k"));
    tree::generate(&work, 10_000, false);
    let ctx = context(&work, None, plain_path());
    let backend = CodeSearch::with_program(rg.display().to_string());
    let patterns = [
        tree::RARE,
        tree::COMMON,
        tree::REGEX,
        "no_such_token_qq",
        "needle_word",
    ];
    let mut times = Vec::new();
    for round in 0..6 {
        for p in patterns {
            let t = Instant::now();
            let out = search(&backend, &ctx, json!({"pattern": p})).await;
            let took = t.elapsed();
            assert!(matches!(out, ToolOutput::Text { .. }), "{p}");
            if round > 0 {
                times.push(took);
            }
        }
    }
    times.sort();
    let p50 = times[times.len() / 2];
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    eprintln!(
        "TIMING code_search files=10000 calls={} p50={}ms p95={}ms (target p95 <= 1500 ms)",
        times.len(),
        p50.as_millis(),
        p95.as_millis()
    );
    assert!(p95 < Duration::from_millis(4_500), "p95 {p95:?}");
}
