//! The router and the command runner at the cli layer: every subcommand typed in the chat box goes
//! through the router (`ham_core::router`) to the parser the shell uses, and runs in-process through `ham::cli::run_captured`, which
//! returns the text it wrote as data. The shell form is the compiled binary against the same stand-in `/v1` API (the real control-api
//! handler over an in-memory store). Their text is compared after the stated normalization: the home directory and the run id are
//! masked; a download link keeps its URL out of the panel; doctor compares check names and levels.
//!
#![allow(clippy::unwrap_used)]

mod clonefix;
mod home;
mod support;

use clonefix::{build, git_in};
use ham::api::Api;
use ham::cli::{ExecContext, run_captured};
use ham::https::HttpsPlane;
use ham::output::{CmdOutput, Stream};
use ham::sigv4::Signer;
use ham_core::config::{Config, parse_config};
use ham_core::router::{
    CONFIRM_DELAY_MS, ConfirmKey, ConfirmStep, Confirmation, RouteContext, RouteOutcome, route,
};
use home::{Home, ham, home, new_run, text};
use serde_json::json;
use std::sync::Arc;
use support::{Fake, REGION, creds};

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
}

/// One home directory with its config, as the compiled binary sees it and as the in-process runner is given it.
struct Side {
    h: Home,
    cfg: Config,
}

fn side(fake: &Fake, label: &str) -> Side {
    let h = home(fake, label);
    let cfg =
        parse_config(&std::fs::read_to_string(h.config.join("ham/config.toml")).unwrap()).unwrap();
    Side { h, cfg }
}

/// What a command printed and how it ended.
#[derive(Debug)]
struct Printed {
    code: i32,
    out: String,
    err: String,
}

async fn shell(s: &Side, args: &[&str]) -> Printed {
    let o = ham(&s.h, args).await;
    Printed {
        code: o.status.code().unwrap(),
        out: text(&o.stdout),
        err: text(&o.stderr),
    }
}

fn ctx_for(run: &str) -> RouteContext {
    RouteContext {
        focused_run: Some(run.to_string()),
        tainted: false,
    }
}

/// The in-process run of an invocation, as the chat box runs it once the command may run.
async fn exec(
    fake: &Fake,
    s: &Side,
    invocation: &ham_core::command::Invocation,
) -> (Printed, CmdOutput) {
    let api = api(fake);
    let (config, root, downloads) = (
        s.h.config.display().to_string(),
        s.h.root.display().to_string(),
        s.h.downloads.display().to_string(),
    );
    let env = move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config.clone()),
        "HOME" => Some(root.clone()),
        "HAMH_DOWNLOAD_DIR" => Some(downloads.clone()),
        _ => None,
    };
    let out = run_captured(
        invocation,
        &ExecContext {
            api: &api,
            cfg: &s.cfg,
            downloads: &s.h.downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: None,
        },
    )
    .await;
    (
        Printed {
            code: out.code,
            out: out.stdout_text(),
            err: out.stderr_text(),
        },
        out,
    )
}

/// A typed line: the router must read it as a command that may run at once.
async fn panel_full(fake: &Fake, s: &Side, line: &str, ctx: &RouteContext) -> (Printed, CmdOutput) {
    let invocation = match route(line, ctx) {
        RouteOutcome::Command { invocation, .. } => invocation,
        other => panic!("{line}: {other:?}"),
    };
    exec(fake, s, &invocation).await
}

async fn panel(fake: &Fake, s: &Side, line: &str, ctx: &RouteContext) -> Printed {
    panel_full(fake, s, line, ctx).await.0
}

/// The normalization of plan 5.2: each side's home directory and every run id are masked.
fn norm(t: &str, sides: &[&Side], runs: &[&str]) -> String {
    let mut t = t.to_string();
    for s in sides {
        t = t.replace(&s.h.root.display().to_string(), "<HOME>");
    }
    for r in runs {
        t = t.replace(r, "<RUN>");
    }
    mask_clock(&t)
}

/// A presigned link carries the second it was made in (`X-Amz-Date=20261016T235027Z`) and the time it expires (`(expires 2026-10-17T00:05:27Z)`).
/// The shell side and the panel side are two calls, so a second can pass between them: the clock in the link is masked.
fn mask_clock(t: &str) -> String {
    const KEYS: [(&str, usize); 2] = [("X-Amz-Date=", 16), ("(expires ", 20)];
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    while let Some((at, key, len)) = KEYS
        .iter()
        .filter_map(|(k, n)| rest.find(k).map(|at| (at, *k, *n)))
        .min_by_key(|(at, _, _)| *at)
    {
        let end = at + key.len();
        out.push_str(&rest[..end]);
        out.push_str("<TIME>");
        let skip: usize = rest[end..].chars().take(len).map(char::len_utf8).sum();
        rest = &rest[end + skip..];
    }
    out.push_str(rest);
    out
}

#[test]
fn the_clock_in_a_presigned_link_is_masked_and_nothing_else() {
    let a = "Link  http://h/z?X-Amz-Date=20261016T235027Z&X-Amz-Expires=900&X-Amz-Signature=fake (expires 2026-10-17T00:05:27Z)";
    let b = "Link  http://h/z?X-Amz-Date=20261016T235028Z&X-Amz-Expires=900&X-Amz-Signature=fake (expires 2026-10-17T00:05:28Z)";
    assert_ne!(a, b);
    assert_eq!(mask_clock(a), mask_clock(b));
    assert_eq!(
        mask_clock(a),
        "Link  http://h/z?X-Amz-Date=<TIME>&X-Amz-Expires=900&X-Amz-Signature=fake (expires <TIME>)"
    );
    assert_eq!(mask_clock("no clock here"), "no clock here");
    // A different link stays different.
    assert_ne!(
        mask_clock("a?X-Amz-Date=20261016T235027Z&n=1"),
        mask_clock("a?X-Amz-Date=20261016T235027Z&n=2")
    );
}

fn same(what: &str, sh: &Printed, pn: &Printed, sides: &[&Side], runs: &[&str]) {
    assert_eq!(
        sh.code, pn.code,
        "{what}: exit code\nshell stderr: {}\npanel stderr: {}",
        sh.err, pn.err
    );
    assert_eq!(
        norm(&sh.out, sides, runs),
        norm(&pn.out, sides, runs),
        "{what}: standard output"
    );
    assert_eq!(
        norm(&sh.err, sides, runs),
        norm(&pn.err, sides, runs),
        "{what}: standard error"
    );
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Read-only commands: the panel text is the shell text.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_commands_print_the_same_text_in_the_panel_as_in_the_shell() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-readonly").await;
    let (sh, pn) = (side(&fake, "ro-shell"), side(&fake, "ro-panel"));
    let ctx = ctx_for(&run);
    let sides = [&sh, &pn];
    for (typed, argv) in [
        ("/runs", vec!["runs"]),
        ("ham runs --json", vec!["runs", "--json"]),
        ("/runs --probes", vec!["runs", "--probes"]),
        ("/models", vec!["models"]),
        ("/models --json", vec!["models", "--json"]),
        ("/models --server", vec!["models", "--server"]),
        (
            "ham models --server --json",
            vec!["models", "--server", "--json"],
        ),
        ("/config", vec!["config"]),
        ("/config --json", vec!["config", "--json"]),
        ("/config get region", vec!["config", "get", "region"]),
        ("/config get nosuchkey", vec!["config", "get", "nosuchkey"]),
        (
            "/config get default_model",
            vec!["config", "get", "default_model"],
        ),
        // No rules directory yet: the same refusal in both.
        ("/rules show", vec!["rules", "show"]),
        // The focused run is the default run id; the shell is given it.
        ("/pr", vec!["pr", run.as_str()]),
        ("/pr --json", vec!["pr", run.as_str(), "--json"]),
        ("ham pr", vec!["pr", run.as_str()]),
    ] {
        let s = shell(&sh, &argv).await;
        let p = panel(&fake, &pn, typed, &ctx).await;
        same(typed, &s, &p, &sides, &[&run]);
    }
    // A command that failed said why, on standard error, in both.
    let o = panel(&fake, &pn, "/config get nosuchkey", &ctx).await;
    assert_eq!(o.code, 1);
    assert!(o.err.starts_with("ham: "), "{o:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_in_the_panel_has_the_same_checks_and_levels_as_in_the_shell() {
    let fake = Fake::start().await;
    let (sh, pn) = (side(&fake, "doc-shell"), side(&fake, "doc-panel"));
    let s = shell(&sh, &["doctor"]).await;
    let p = panel(&fake, &pn, "/doctor", &ctx_for("x")).await;
    // Compared: the level and the name of every check (the details carry ages and times).
    let shape = |t: &str| -> Vec<String> {
        t.lines()
            .filter(|l| !l.starts_with(' '))
            .map(|l| l.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .collect()
    };
    assert_eq!(s.code, p.code, "{s:?} {p:?}");
    assert_eq!(shape(&s.out), shape(&p.out), "{}\n---\n{}", s.out, p.out);
    assert!(
        shape(&p.out)
            .iter()
            .any(|l| l.starts_with("PASS identity") && !l.is_empty()),
        "{}",
        p.out
    );
    // The JSON form is one object in both, with the same checks.
    let sj: serde_json::Value =
        serde_json::from_str(shell(&sh, &["doctor", "--json"]).await.out.trim()).unwrap();
    let pj: serde_json::Value = serde_json::from_str(
        panel(&fake, &pn, "ham doctor --json", &ctx_for("x"))
            .await
            .out
            .trim(),
    )
    .unwrap();
    let names = |v: &serde_json::Value| -> Vec<(String, String)> {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap().to_string(),
                    c["level"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    assert_eq!(names(&sj), names(&pj));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_in_the_panel_keeps_the_url_out_of_the_text_and_hands_it_over_separately() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-link").await;
    fake.mem.complete_run(&run, true);
    let (sh, pn) = (side(&fake, "link-shell"), side(&fake, "link-panel"));
    let s = shell(&sh, &["link", &run]).await;
    assert_eq!(s.code, 0, "{}", s.err);
    let shell_url = s.out.split(' ').next().unwrap().to_string();
    assert!(shell_url.contains("/objects/"), "{}", s.out);
    let (p, full) = panel_full(&fake, &pn, "/link", &ctx_for(&run)).await;
    assert_eq!(p.code, 0, "{}", p.err);
    // The panel names the expiry and the keys, and shows no URL, no query and no signature.
    assert!(
        p.out.starts_with("Download link ready, expires ") && p.out.contains("; u copies, o opens"),
        "{}",
        p.out
    );
    for needle in ["http", "://", "?", "X-Amz", "Signature", "/objects/"] {
        assert!(
            !p.out.contains(needle) && !p.err.contains(needle),
            "{needle}: {p:?}"
        );
        assert!(
            full.lines().iter().all(|l| !l.contains(needle)),
            "{needle} in the lines"
        );
    }
    assert!(full.chunks.iter().all(|c| c.stream == Stream::Out));
    // The link itself is carried apart, for the key press: the same object as the shell printed.
    let bearer = full.bearer.clone().expect("the link is handed over");
    assert_eq!(
        bearer.split('?').next(),
        shell_url.split('?').next(),
        "{bearer} vs {shell_url}"
    );
    // The JSON form of the panel shows the same text, not the URL.
    let (pj, fullj) = panel_full(&fake, &pn, "/link --json", &ctx_for(&run)).await;
    assert!(!pj.out.contains("http") && fullj.bearer.is_some(), "{pj:?}");
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Local writes.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_writes_do_the_same_in_the_panel_as_in_the_shell() {
    let fake = Fake::start().await;
    let (sh, pn) = (side(&fake, "lw-shell"), side(&fake, "lw-panel"));
    let sides = [&sh, &pn];
    let ctx = ctx_for("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    // The rules files: the same text, and the same files on disk.
    let s = shell(&sh, &["rules", "init"]).await;
    let p = panel(&fake, &pn, "/rules init", &ctx).await;
    same("rules init", &s, &p, &sides, &[]);
    assert_eq!(s.code, 0, "{s:?}");
    let files = |side: &Side| -> Vec<(String, Vec<u8>)> {
        let mut v = Vec::new();
        for d in [side.h.config.join("ham/rules"), side.h.config.join("ham")] {
            for e in std::fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_file() && e.file_name() != "config.toml" {
                    v.push((
                        e.file_name().to_string_lossy().into_owned(),
                        std::fs::read(e.path()).unwrap(),
                    ));
                }
            }
        }
        v.sort();
        v
    };
    assert_eq!(files(&sh), files(&pn));
    // A second run keeps what is there, in both.
    let s = shell(&sh, &["rules", "init"]).await;
    let p = panel(&fake, &pn, "/rules init", &ctx).await;
    same("rules init again", &s, &p, &sides, &[]);
    let s = shell(&sh, &["rules", "show"]).await;
    let p = panel(&fake, &pn, "/rules show", &ctx).await;
    same("rules show", &s, &p, &sides, &[]);
    assert_eq!(s.code, 0, "{s:?}");
    // The config file: the same text and the same bytes.
    for (typed, argv) in [
        (
            "/config set default_model opus",
            vec!["config", "set", "default_model", "opus"],
        ),
        (
            "/config set default_repo cc:team-app",
            vec!["config", "set", "default_repo", "cc:team-app"],
        ),
        (
            "/config set default_model nosuchmodel",
            vec!["config", "set", "default_model", "nosuchmodel"],
        ),
        (
            "/config unset default_repo",
            vec!["config", "unset", "default_repo"],
        ),
    ] {
        let s = shell(&sh, &argv).await;
        let p = panel(&fake, &pn, typed, &ctx).await;
        same(typed, &s, &p, &sides, &[]);
        assert_eq!(
            std::fs::read(sh.h.config.join("ham/config.toml")).unwrap(),
            std::fs::read(pn.h.config.join("ham/config.toml")).unwrap(),
            "{typed}: the config files differ"
        );
    }
}

/// `t` with the query of every web link cut off: the form of the card that the panel shows (the signed query is a bearer capability).
fn without_link_query(t: &str) -> String {
    let body = t
        .lines()
        .map(|l| {
            l.split(' ')
                .map(|w| {
                    if w.starts_with("http://") || w.starts_with("https://") {
                        w.split('?').next().unwrap_or(w)
                    } else {
                        w
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    if t.ends_with('\n') {
        format!("{body}\n")
    } else {
        body
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_in_the_panel_keeps_the_signed_query_of_the_link_out_of_the_text_and_hands_the_link_over_apart()
 {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-pull-link").await;
    fake.mem.complete_run(&run, true);
    let (sh, pn) = (side(&fake, "pl-shell"), side(&fake, "pl-panel"));
    let s = shell(&sh, &["pull", &run]).await;
    assert_eq!(s.code, 0, "{s:?}");
    // The shell prints the whole link.
    assert!(
        s.out.contains("X-Amz-") || s.out.contains("Signature="),
        "the trigger: the shell's card holds a signed link: {}",
        s.out
    );
    let (p, full) = panel_full(&fake, &pn, "/pull", &ctx_for(&run)).await;
    assert_eq!(p.code, 0, "{p:?}");
    for needle in ["?", "X-Amz", "Signature"] {
        assert!(
            !p.out.contains(needle) && !p.err.contains(needle),
            "{needle}: {p:?}"
        );
        assert!(
            full.lines().iter().all(|l| !l.contains(needle)),
            "{needle} in the lines"
        );
    }
    // The panel is the shell's card with the signed query cut from the link: the same rows otherwise.
    assert_eq!(
        norm(&without_link_query(&s.out), &[&sh, &pn], &[&run]),
        norm(&p.out, &[&sh, &pn], &[&run])
    );
    // The whole link is carried apart, for the key press: the same object as the shell printed.
    let bearer = full.bearer.clone().expect("the link is handed over");
    let shell_url = s
        .out
        .lines()
        .find(|l| l.starts_with("Link"))
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap()
        .to_string();
    assert_eq!(bearer.split('?').next(), shell_url.split('?').next());
    assert!(bearer.contains('?'), "the bearer is the whole link");
    // The JSON form shows no link in the panel either.
    let (pj, fullj) = panel_full(&fake, &pn, "/pull --json", &ctx_for(&run)).await;
    assert!(
        !pj.out.contains("X-Amz") && !pj.out.contains("Signature") && fullj.bearer.is_some(),
        "{pj:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_in_the_panel_downloads_the_same_zip_and_prints_the_same_card() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-pull").await;
    fake.mem.complete_run(&run, true);
    let (sh, pn) = (side(&fake, "pull-shell"), side(&fake, "pull-panel"));
    let s = shell(&sh, &["pull", &run]).await;
    let p = panel(&fake, &pn, "/pull", &ctx_for(&run)).await;
    assert_eq!(s.code, 0, "{s:?}");
    // The panel's card names the link without its signed query.
    let s_cut = Printed {
        code: s.code,
        out: without_link_query(&s.out),
        err: s.err.clone(),
    };
    same("pull", &s_cut, &p, &[&sh, &pn], &[&run]);
    let zips = |side: &Side| -> Vec<(String, Vec<u8>)> {
        std::fs::read_dir(&side.h.downloads)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(zips(&sh), zips(&pn));
    assert!(!zips(&pn).is_empty());
}

async fn delivered(fake: &Fake, label: &str) -> (String, clonefix::Fixture) {
    let run = new_run(fake, label).await;
    let fx = build(label, &run);
    fake.mem.inner.lock().unwrap().deliverable = Some(fx.zip_bytes());
    fake.mem.complete_run(&run, true);
    (run, fx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_in_the_panel_makes_the_same_repository_and_prints_the_same_text() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "router-clone").await;
    let (sh, pn) = (side(&fake, "clone-shell"), side(&fake, "clone-panel"));
    let (dsh, dpn) = (sh.h.root.join("out"), pn.h.root.join("out"));
    let s = shell(&sh, &["clone", &run, &dsh.display().to_string()]).await;
    let p = panel(
        &fake,
        &pn,
        &format!("/clone {run} {}", dpn.display()),
        &ctx_for(&run),
    )
    .await;
    assert_eq!(s.code, 0, "{s:?}");
    same("clone", &s, &p, &[&sh, &pn], &[&run]);
    for d in [&dsh, &dpn] {
        assert_eq!(git_in(d, &["rev-parse", "HEAD"]).unwrap(), fx.head);
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Commands that change a run: the second Enter, then the same text.

/// What the chat box does with a command that has a preview: it shows the preview, ignores an Enter that comes too soon, and runs
/// the command on the second Enter. Returns the router's outcome and whether the command was run.
async fn through_the_box(
    fake: &Fake,
    s: &Side,
    line: &str,
    ctx: &RouteContext,
) -> (RouteOutcome, Option<Printed>) {
    let outcome = route(line, ctx);
    let Some(mut c) = Confirmation::new(&outcome, 10_000) else {
        return (outcome, None);
    };
    assert_eq!(
        c.press(ConfirmKey::Enter, 10_000 + 50),
        ConfirmStep::Waiting(ham_core::router::Wait::TooSoon)
    );
    let typed = c.expected().unwrap_or("").to_string();
    for ch in typed.chars() {
        c.press(ConfirmKey::Char(ch), 10_000 + CONFIRM_DELAY_MS);
    }
    assert_eq!(
        c.press(ConfirmKey::Enter, 10_000 + CONFIRM_DELAY_MS + 1),
        ConfirmStep::Run
    );
    let ran = exec(fake, s, c.invocation()).await.0;
    (outcome, Some(ran))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_asks_for_a_second_enter_and_then_prints_the_shell_text() {
    let fake = Fake::start().await;
    let (r_shell, r_panel) = (
        new_run(&fake, "router-cancel-a").await,
        new_run(&fake, "router-cancel-b").await,
    );
    for r in [&r_shell, &r_panel] {
        fake.mem.add_agent(r, "direct", "root", "running");
    }
    let (sh, pn) = (side(&fake, "cancel-shell"), side(&fake, "cancel-panel"));
    let before = fake.mem.seen().len();
    let ctx = ctx_for(&r_panel);
    // Typed: a preview that names the run, and nothing has reached the stand-in yet.
    let outcome = route("/cancel", &ctx);
    match &outcome {
        RouteOutcome::Command { preview, .. } => {
            assert!(preview.as_ref().unwrap().text.contains(&r_panel));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fake.mem.seen().len(),
        before,
        "a preview sends nothing to the stand-in"
    );
    assert!(fake.mem.controls_of(&r_panel).is_empty());
    // The second Enter runs it.
    let s = shell(&sh, &["cancel", &r_shell]).await;
    let (_, p) = through_the_box(&fake, &pn, "/cancel", &ctx).await;
    let p = p.expect("the command ran after the second Enter");
    same("cancel", &s, &p, &[&sh, &pn], &[&r_shell, &r_panel]);
    assert_eq!(s.code, 0, "{s:?}");
    assert_eq!(fake.mem.controls_of(&r_shell).len(), 1);
    assert_eq!(
        fake.mem.controls_of(&r_panel).len(),
        1,
        "exactly one cancel for the run"
    );
    // The same refusal when the run is finished.
    for r in [&r_shell, &r_panel] {
        fake.mem
            .inner
            .lock()
            .unwrap()
            .runs
            .get_mut(r)
            .unwrap()
            .status = harness_protocol::goal::GoalStatus::Complete;
    }
    let s = shell(&sh, &["cancel", &r_shell]).await;
    let (_, p) = through_the_box(&fake, &pn, "/cancel", &ctx).await;
    same(
        "cancel again",
        &s,
        &p.unwrap(),
        &[&sh, &pn],
        &[&r_shell, &r_panel],
    );
    assert_eq!(s.code, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_answer_commands_print_the_shell_text() {
    let fake = Fake::start().await;
    let (sh, pn) = (side(&fake, "ans-shell"), side(&fake, "ans-panel"));
    // A run nobody waits on: the same refusal for each command.
    let (a, b) = (
        new_run(&fake, "router-ans-a").await,
        new_run(&fake, "router-ans-b").await,
    );
    let ctx = ctx_for(&b);
    for (typed, shell_argv) in [
        ("/finish --no-wait", vec!["finish", a.as_str(), "--no-wait"]),
        ("/new-session", vec!["new-session", a.as_str()]),
        ("/say hello there", vec!["say", a.as_str(), "hello there"]),
        ("/try-model opus", vec!["try-model", a.as_str(), "opus"]),
    ] {
        let s = shell(&sh, &shell_argv).await;
        let (_, p) = through_the_box(&fake, &pn, typed, &ctx).await;
        same(typed, &s, &p.unwrap(), &[&sh, &pn], &[&a, &b]);
        assert_eq!(s.code, 1, "{typed}: {s:?}");
        assert!(s.err.contains("is not waiting for you"), "{s:?}");
    }
    // A run with an open question: finish, with the answer queued.
    let (c, d) = (
        new_run(&fake, "router-ans-c").await,
        new_run(&fake, "router-ans-d").await,
    );
    for r in [&c, &d] {
        fake.mem.add_agent(r, "direct", "root", "running");
    }
    fake.mem.needs_input(&c, "engine_stop");
    fake.mem.needs_input(&d, "engine_stop");
    let s = shell(&sh, &["finish", &c, "--no-wait"]).await;
    let (_, p) = through_the_box(&fake, &pn, "/finish --no-wait", &ctx_for(&d)).await;
    same("finish", &s, &p.unwrap(), &[&sh, &pn], &[&c, &d]);
    assert_eq!(s.code, 0, "{s:?}");
    assert_eq!(
        fake.mem.controls_of(&c).len(),
        fake.mem.controls_of(&d).len()
    );
    // The text of a message is sent exactly as typed: quotes, an apostrophe and a dash stay.
    let (e, f) = (
        new_run(&fake, "router-ans-e").await,
        new_run(&fake, "router-ans-f").await,
    );
    for r in [&e, &f] {
        fake.mem.add_agent(r, "direct", "root", "running");
    }
    fake.mem.needs_input(&e, "engine_stop");
    let q_f = fake.mem.needs_input(&f, "engine_stop");
    let message = "don't \"quote\" me -- --no-wait";
    let s = shell(&sh, &["say", &e, "--no-wait", "--text", message]).await;
    assert_eq!(s.code, 0, "{s:?}");
    // The stand-in plays agentd: it acknowledges the answer once it is posted.
    let acker = {
        let (fake, f, q) = (fake.clone(), f.clone(), q_f.clone());
        tokio::spawn(async move {
            loop {
                if !fake.mem.posts(&format!("/v1/runs/{f}/input")).is_empty() {
                    fake.mem
                        .input_event(&f, "input-received", &q, json!({"textBytes": 1}));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
    };
    let (_, p) = through_the_box(&fake, &pn, &format!("/say {message}"), &ctx_for(&f)).await;
    acker.await.unwrap();
    let p = p.unwrap();
    assert_eq!(p.code, 0, "{p:?}");
    // The answer the stand-in queued carries the text exactly as typed.
    let sent: Vec<Option<String>> = fake
        .mem
        .controls_of(&f)
        .into_iter()
        .filter_map(|c| match c.control {
            harness_protocol::api::internal::PendingControl::Instruct { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        vec![Some(message.to_string())],
        "one answer, byte for byte"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_typed_in_the_chat_pushes_a_delivered_run_to_a_bare_repository() {
    let fake = Fake::start().await;
    let (run, fx) = delivered(&fake, "router-push").await;
    let (sh, pn) = (side(&fake, "push-shell"), side(&fake, "push-panel"));
    let bare = |s: &Side| {
        let dir = s.h.root.join("remote.git");
        std::fs::create_dir_all(&dir).unwrap();
        git_in(&dir, &["init", "-q", "--bare", "--initial-branch", "main"]).unwrap();
        dir
    };
    let (rsh, rpn) = (bare(&sh), bare(&pn));
    let s = shell(&sh, &["push", &run, &rsh.display().to_string()]).await;
    assert_eq!(s.code, 0, "{s:?}");
    // Without the URL the box asks for it; the stand-in has seen nothing of a push until the second Enter.
    let ctx = ctx_for(&run);
    let before = fake.mem.seen().len();
    let asked = route("ham push", &ctx);
    assert!(
        matches!(asked, RouteOutcome::Incomplete { .. }),
        "{asked:?}"
    );
    let done = ham_core::router::complete(
        match &asked {
            RouteOutcome::Incomplete { argv, .. } => argv,
            _ => unreachable!(),
        },
        &rpn.display().to_string(),
    );
    assert_eq!(fake.mem.seen().len(), before);
    let mut c = Confirmation::new(&done, 0).expect("a push is confirmed");
    assert_eq!(c.expected(), Some(&run[run.len() - 4..]));
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS + 1),
        ConfirmStep::Waiting(ham_core::router::Wait::TypeAnswer)
    );
    for ch in run[run.len() - 4..].chars() {
        c.press(ConfirmKey::Char(ch), CONFIRM_DELAY_MS + 2);
    }
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS + 3),
        ConfirmStep::Run
    );
    assert_eq!(
        fake.mem.seen().len(),
        before,
        "nothing was sent before the confirmation ran"
    );
    let p = panel(&fake, &pn, &format!("/push {}", rpn.display()), &ctx).await;
    assert_eq!(p.code, 0, "{p:?}");
    // The result line is the same. git's own lines go to the process's standard error, where the shell shows them; the runner
    // leaves them to a caller that has handed the terminal over, so the panel's captured text holds none.
    assert_eq!(
        norm(&s.out, &[&sh, &pn], &[&run]),
        norm(&p.out, &[&sh, &pn], &[&run])
    );
    assert!(s.err.contains("[new branch]"), "{}", s.err);
    assert!(p.err.is_empty(), "{}", p.err);
    let branch = format!("harness/{run}");
    for r in [&rsh, &rpn] {
        assert_eq!(
            git_in(r, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            fx.head,
            "the remote's head equals the run's head"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Lines that must send nothing.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_chat_and_the_start_form_send_nothing_to_the_stand_in() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-nothing").await;
    let ctx = ctx_for(&run);
    let before = fake.mem.seen().len();
    let started = fake.mem.inner.lock().unwrap().runs.len();
    for line in [
        "/cancle",
        "ham cancle",
        "/nonsense",
        "//cancel",
        "/export",
        "/now hello",
        "ham \"build X\" --s3-only --direct",
        "ham build --s3-only --direct",
        "ham \"build X\"",
        "/help",
        "/new",
        "/open",
        "/say",
        "/say --run nope hi",
        "/cancel\nmore",
        "/send hello",
        "hello there",
        "/usr/bin/env is missing",
        " /cancel",
        "",
    ] {
        let o = route(line, &ctx);
        assert!(
            !matches!(
                o,
                RouteOutcome::Command { .. } | RouteOutcome::Incomplete { .. }
            ),
            "{line:?} must not be a command: {o:?}"
        );
    }
    // A pasted `/cancel` is a command that waits for a second Enter; without it nothing is sent.
    let pasted = RouteContext {
        focused_run: Some(run.clone()),
        tainted: true,
    };
    let o = route("/cancel", &pasted);
    let mut c = Confirmation::new(&o, 0).unwrap();
    assert_eq!(
        c.press(ConfirmKey::Enter, 10),
        ConfirmStep::Waiting(ham_core::router::Wait::TooSoon)
    );
    assert_eq!(c.press(ConfirmKey::Esc, 20), ConfirmStep::Cancelled);
    assert_eq!(
        fake.mem.seen().len(),
        before,
        "nothing reached the stand-in"
    );
    assert_eq!(
        fake.mem.inner.lock().unwrap().runs.len(),
        started,
        "no run was started"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_and_the_start_form_are_not_run_by_the_panel_runner() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-nonpanel").await;
    let pn = side(&fake, "np-panel");
    let before = fake.mem.seen().len();
    let (p, _) = panel_full(&fake, &pn, "/watch", &ctx_for(&run)).await;
    assert_eq!(p.code, 2, "{p:?}");
    assert!(p.err.contains("watch"), "{p:?}");
    // A start is built by hand here: the router never returns one as a command, and the runner refuses it.
    let start = ham_core::command::parse(&["x", "--s3-only", "--direct"]).unwrap();
    let api = api(&fake);
    let env = |_: &str| None;
    let out = run_captured(
        &start,
        &ExecContext {
            api: &api,
            cfg: &pn.cfg,
            downloads: &pn.h.downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: None,
        },
    )
    .await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr_text().contains("New run form"),
        "{}",
        out.stderr_text()
    );
    assert_eq!(
        fake.mem.seen().len(),
        before,
        "nothing reached the stand-in"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standard_input_is_not_read_in_the_panel() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "router-stdin").await;
    fake.mem.add_agent(&run, "direct", "root", "running");
    fake.mem.needs_input(&run, "engine_stop");
    let pn = side(&fake, "stdin-panel");
    let inv = ham_core::command::parse(&["say", run.as_str(), "-"]).unwrap();
    let api = api(&fake);
    let env = |_: &str| None;
    let out = run_captured(
        &inv,
        &ExecContext {
            api: &api,
            cfg: &pn.cfg,
            downloads: &pn.h.downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: None,
        },
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(
        out.stderr_text().contains("standard input"),
        "{}",
        out.stderr_text()
    );
    // With a reader given, the text is what it returns.
    let reader = || Ok("from the box".to_string());
    let out = run_captured(
        &ham_core::command::parse(&["say", run.as_str(), "-", "--no-wait"]).unwrap(),
        &ExecContext {
            api: &api,
            cfg: &pn.cfg,
            downloads: &pn.h.downloads,
            chunk: 16 << 20,
            env: &env,
            stdin: Some(&reader),
        },
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr_text());
}

// ---------------------------------------------------------------------------------------------------------------------------------
// The shell: a mistyped command word, no terminal.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shell_names_the_closest_command_for_a_mistyped_word_and_exits_2() {
    let fake = Fake::start().await;
    let sh = side(&fake, "typo-shell");
    for (args, closest) in [
        (vec!["pul"], "pull"),
        (vec!["cancle"], "cancel"),
        (vec!["psuh"], "push"),
        (vec!["modles", "--json"], "models"),
    ] {
        let o = shell(&sh, &args).await;
        assert_eq!(o.code, 2, "{args:?}: {o:?}");
        assert!(
            o.err.contains(&format!("closest: {closest}")) && o.err.contains("--s3-only"),
            "{args:?}: {}",
            o.err
        );
        assert!(o.out.is_empty(), "{}", o.out);
    }
    // A prompt that is a real word still gets the old message about the missing target, and a complete start form is not refused.
    let o = shell(&sh, &["hello"]).await;
    assert_eq!(o.code, 2);
    assert!(o.err.contains("exactly one of --repo"), "{}", o.err);
    // No arguments and no terminal: the usage line, exit 2.
    let o = shell(&sh, &[]).await;
    assert_eq!(o.code, 2);
    assert!(o.err.contains("ham <command>"), "{}", o.err);
    for name in ham_core::registry::subcommand_names() {
        assert!(o.err.contains(name), "{name} missing from {}", o.err);
    }
}
