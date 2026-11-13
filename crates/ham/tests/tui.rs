//! The TUI: the Runs screen, `n` opens the New-run composer, and the dashboard of a
//! run, its delivery card and the clone key. The composer's model field and Submit are `tui_model.rs`; the Artifacts screen is
//! `tui_w2.rs`. Screens are pinned by insta snapshots at 80x24, 120x40 and 200x60 through ratatui's TestBackend. Agent text
//! reaches the screen only through `sanitize_line`.
#![allow(clippy::unwrap_used)]

mod clonefix;
mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::clone::Cloned;
use ham::https::HttpsPlane;
use ham::pull::Pulled;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, perform};
use harness_protocol::api::v1::{
    ArtifactView, DownloadUrlResponse, EventsPage, RunSummary, StartDirectRunRequest,
};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::ArtifactKind;
use harness_protocol::target::RunTarget;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];
const RUN: &str = "01K6A0000000000000000000R1";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

/// The TUI of the direct line, with no stored model choice.
fn new_app(nonce: &str, default_repo: Option<&str>) -> App {
    App::new_direct(
        nonce.into(),
        default_repo.map(str::to_string),
        DirectSettings::default(),
    )
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(key(c))
}

fn type_text(app: &mut App, s: &str) {
    for ch in s.chars() {
        let effects = if ch == '\n' {
            press(app, KeyCode::Enter)
        } else {
            press(app, KeyCode::Char(ch))
        };
        assert!(effects.is_empty(), "typing {ch:?} caused {effects:?}");
    }
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        // The version tag at the end of row 0 changes with every release (tui_version_tag.rs pins it); the snapshots hold the screen without it.
        out.push_str(if y == 0 {
            ham::tui::version_tag::split_tag(line.trim_end()).0
        } else {
            line.trim_end()
        });
        out.push('\n');
    }
    out
}

fn snapshot_sizes(app: &App, name: &str) {
    for (w, h) in SIZES {
        insta::assert_snapshot!(format!("{name}_{w}x{h}"), screen_text(app, w, h));
    }
}

fn summary(id: &str, status: GoalStatus, phase: GoalPhase, target: RunTarget) -> RunSummary {
    RunSummary {
        needs_input: None,
        run_id: id.into(),
        status,
        phase,
        target,
        created_at: "2026-09-25T20:00:00Z".into(),
        owner_principal_arn: support::ME.into(),
        kind: None,
        aborted: None,
        continue_from: None,
        status_reason: None,
        resume_mode: None,
        session: None,
    }
}

fn runs() -> Msg {
    Msg::Runs(Ok(vec![
        summary(
            RUN,
            GoalStatus::Complete,
            GoalPhase::Delivering,
            serde_json::from_value(json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}))
                .unwrap(),
        ),
        summary(
            "01K6A0000000000000000000R0",
            GoalStatus::Active,
            GoalPhase::Gating,
            RunTarget::S3Only,
        ),
    ]))
}

/// An agent's event when `agent` is set, else a harness event.
fn env(n: u32, t: EventType, agent: Option<&str>, v: Value) -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("01K6A00000000000000000{n:04}"),
        run_id: RUN.into(),
        task_id: None,
        agent_id: agent.map(str::to_string),
        seq: 0,
        ts: "2026-09-25T20:00:00Z".into(),
        event_type: t,
        authority: if agent.is_some() {
            Authority::Agent
        } else {
            Authority::Harness
        },
        payload: Payload::Inline { value: v },
    }
}

/// `session.started` is harness-authored (the dispatcher starts sessions).
fn started(n: u32, agent: &str, engine: &str, model: &str) -> Envelope {
    env(
        n,
        EventType::SessionStarted,
        None,
        json!({"agentId": agent, "engine": engine, "modelId": model}),
    )
}

fn delivered_pages() -> Msg {
    pages(all_events())
}

/// The run while Cline works: no PR or delivery yet.
fn working_pages() -> Msg {
    pages(
        all_events()
            .into_iter()
            .filter(|e| {
                !matches!(
                    e.event_type,
                    EventType::DeliveryPr
                        | EventType::GoalStatus
                        | EventType::DeliveryReady
                        | EventType::RunCompleted
                )
            })
            .collect(),
    )
}

fn all_events() -> Vec<Envelope> {
    vec![
        env(
            1,
            EventType::RunCreated,
            None,
            json!({"target": {"mode": "s3-only"}}),
        ),
        started(2, "direct", "cline", SONNET),
        env(
            3,
            EventType::TurnProgress,
            Some("direct"),
            json!({"text": "reviewing the diff"}),
        ),
        env(
            5,
            EventType::TurnEnded,
            Some("direct"),
            json!({"summary": "wrote tests \u{1b}]0;sentinel\u{7}and \u{1b}]8;;https://docs.example.test\u{7}docs\u{1b}]8;;\u{7}"}),
        ),
        env(
            22,
            EventType::DeliveryPr,
            None,
            json!({"url": "https://example.test/pr/7"}),
        ),
        env(
            23,
            EventType::GoalStatus,
            None,
            json!({"status": "complete", "phase": "delivering"}),
        ),
        env(
            24,
            EventType::DeliveryReady,
            None,
            json!({"artifactId": "01K6A00000000000000000ZP01", "sha256": "ab".repeat(32), "sizeBytes": 2048}),
        ),
        env(
            25,
            EventType::RunCompleted,
            None,
            json!({"status": "complete", "outcome": "reported_done", "label": "Cline reported done"}),
        ),
    ]
}

fn pages(events: Vec<Envelope>) -> Msg {
    // Agent events live on their shards; the dashboard merges every page.
    let mut by_shard: std::collections::BTreeMap<u8, Vec<Envelope>> = Default::default();
    for e in events {
        let s = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
        by_shard.entry(s).or_default().push(e);
    }
    Msg::Pages(Ok(by_shard
        .into_iter()
        .map(|(s, events)| {
            let next_after = events.last().map(|e| e.event_id.clone());
            (s, EventsPage { events, next_after })
        })
        .collect()))
}

#[test]
fn dashboard_snapshots_while_agents_work() {
    let mut app = new_app("n", None);
    app.open_run(RUN);
    let effects = app.update(working_pages());
    assert!(effects.is_empty(), "nothing to download yet: {effects:?}");
    snapshot_sizes(&app, "dashboard_working");
    // The agent table is a folded card at the end of the conversation: Shift-Tab selects it and Space opens it.
    assert!(!screen_text(&app, 80, 24).contains("wrote tests and docs"));
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Char(' '));
    assert!(screen_text(&app, 80, 24).contains("wrote tests and docs"));
}

fn downloaded() -> Msg {
    Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(Pulled {
        path: PathBuf::from("/downloads/python-calc-R1-abc1234.zip"),
        artifact: ArtifactView {
            artifact_id: "01K6A00000000000000000ZP01".into(),
            kind: ArtifactKind::Zip,
            s3_uri: "s3://b/runs/R1/deliverable/python-calc-R1-abc1234.zip".into(),
            sha256: "ab".repeat(32),
            size_bytes: 2048,
        },
        url: DownloadUrlResponse {
            url: "https://b.s3.us-west-2.amazonaws.com/runs/R1/deliverable/python-calc-R1-abc1234.zip?X-Amz-Expires=900".into(),
            expires_at: "2026-09-25T20:15:00Z".into(),
        },
        ranges: vec![(0, 2047)],
        resumed_from: 0,
    }),
    }
}

#[test]
fn the_runs_screen_lists_runs_and_n_opens_the_composer() {
    let mut app = new_app("nonce-1", None);
    assert_eq!(app.screen(), Screen::Runs);
    assert!(app.start().contains(&Effect::LoadRuns));
    app.update(runs());
    insta::assert_snapshot!("runs_80x24", screen_text(&app, 80, 24));
    let effects = press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    assert!(
        effects.is_empty(),
        "the composer lists the compiled-in models: {effects:?}"
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    press(&mut app, KeyCode::Down);
    let effects = press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen(), Screen::Dashboard);
    assert_eq!(app.focused_run(), Some("01K6A0000000000000000000R0"));
    // The run opens with its first poll and the load of its detail (the prompt and the totals the header shows), one read of its row for
    // its name, then the first page of the stored conversation and the run's messages.
    assert!(
        matches!(
            effects.as_slice(),
            [
                Effect::Poll { .. },
                Effect::LoadDetail { .. },
                Effect::ReadRunName { .. },
                Effect::Ext(ham::tui::ext::Job::Transcript { .. }),
                Effect::Ext(ham::tui::ext::Job::Messages { .. })
            ]
        ),
        "{effects:?}"
    );
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
}

/// The Repository field offers only the target the service serves;
/// the first Step Functions state refuses the others before any agent runs.
#[test]
fn the_composer_names_only_the_served_repository_target() {
    let mut app = new_app("nonce-1", None);
    app.update(runs());
    press(&mut app, KeyCode::Char('n'));
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        assert!(text.contains("Repository (cc:<name>)"), "{w}x{h}:\n{text}");
        assert!(
            !text.contains("gh:") && !text.contains("gl:"),
            "{w}x{h}:\n{text}"
        );
    }
}

#[test]
fn the_composer_refuses_an_empty_prompt_or_a_bad_target_and_s3_only_needs_no_repo() {
    let mut app = new_app("n", None);
    press(&mut app, KeyCode::Char('n'));
    let submit = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(app.handle_key(submit).is_empty());
    assert!(screen_text(&app, 120, 40).contains("a prompt is required"));
    type_text(&mut app, "x");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    type_text(&mut app, "github.com/x");
    assert!(app.handle_key(submit).is_empty());
    assert!(screen_text(&app, 120, 40).contains("unknown target scheme"));
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right); // --s3-only
    let effects = app.handle_key(submit);
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert_eq!(args.target, ham_core::command::TargetArg::S3Only);
    assert_eq!(
        args.model.as_deref(),
        Some(SONNET),
        "the model on the screen"
    );
}

#[test]
fn dashboard_snapshots_and_agent_text_is_sanitized() {
    let mut app = new_app("n", None);
    app.open_run(RUN);
    let effects = app.update(delivered_pages());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Download { run_id, .. } if run_id == RUN)),
        "delivery starts the download: {effects:?}"
    );
    app.update(downloaded());
    snapshot_sizes(&app, "dashboard_delivered");
    // Open the agents card (the delivery card is the last block, the agents the one before it): its text is cleaned like the rest.
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Char(' '));
    for (w, h) in SIZES {
        let text = screen_text(&app, w, h);
        assert!(!text.contains('\u{1b}') && !text.contains("sentinel"));
        assert!(!text.contains("docs.example.test"), "{text}");
    }
    let wide = screen_text(&app, 200, 60);
    assert!(wide.contains("wrote tests and docs"), "{wide}");
    // A second page with the same events changes nothing and starts no second download.
    assert!(
        !app.update(delivered_pages())
            .iter()
            .any(|e| matches!(e, Effect::Download { .. }))
    );
}

fn cloned() -> Cloned {
    Cloned {
        dir: PathBuf::from("/downloads/python-calc"),
        branch: format!("harness/{RUN}"),
        head: "0123456789abcdef0123456789abcdef01234567".into(),
        commits: 2,
        files: 7,
        top: format!("python-calc-{RUN}-abc1234"),
        partial: false,
        zip: PathBuf::from("/downloads/python-calc-R1-abc1234.zip"),
        zip_sha256: "ab".repeat(32),
        warnings: vec![],
    }
}

/// The delivery card names the clone command, and `C` opens the shared clone flow of `ham clone <run>`.
/// The answer replaces the hint on the card.
#[test]
fn the_delivery_card_names_the_clone_command_and_the_c_key_clones_the_run_through_the_shared_flow()
{
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    // The zip is still downloading: C does not wait for it. The shared flow fetches the zip itself when it is not on this machine.
    let effects = press(&mut app, KeyCode::Char('C'));
    assert_eq!(
        effects,
        vec![Effect::Clone {
            run_id: RUN.into(),
            cwd: None,
        }]
    );
    // The card has no Clone row until the zip is on it; the notice row says the clone is under way.
    let text = screen_text(&app, 140, 40);
    assert!(!text.contains("still downloading"), "{text}");
    assert!(
        text.contains("cloning… (the zip is fetched first"),
        "{text}"
    );
    assert!(
        press(&mut app, KeyCode::Char('C')).is_empty(),
        "one clone at a time"
    );
    app.update(Msg::Cloned {
        run_id: RUN.into(),
        result: Ok(cloned()),
    });
    app.update(downloaded());
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains(&format!(
            "Clone     /downloads/python-calc (branch harness/{RUN}, 7 files, tree equals src/)"
        )),
        "{text}"
    );
    assert!(
        text.contains("PR        https://example.test/pr/7"),
        "the card keeps its other lines: {text}"
    );
    assert!(
        press(&mut app, KeyCode::Char('C')).is_empty(),
        "already cloned: the card shows where"
    );
}

/// With the zip already on this machine the key does the same: the shared flow keeps a zip that verifies and clones from it.
#[test]
fn the_card_names_the_clone_command_before_the_key_is_pressed() {
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    app.update(downloaded());
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains(&format!("Clone     press C, or run: ham clone {RUN}")),
        "{text}"
    );
    let effects = press(&mut app, KeyCode::Char('C'));
    assert_eq!(
        effects,
        vec![Effect::Clone {
            run_id: RUN.into(),
            cwd: None,
        }]
    );
}

/// The directory of the clone is the one `ham clone <run>` uses: the working directory the app was started in, when it was told (`set_clone_dir`).
#[test]
fn c_clones_into_the_directory_the_app_was_told_to_use() {
    let mut app = new_app("n", None);
    app.set_clone_dir(Some(PathBuf::from("/work/here")));
    app.open_run(RUN);
    app.update(delivered_pages());
    let effects = press(&mut app, KeyCode::Char('C'));
    assert_eq!(
        effects,
        vec![Effect::Clone {
            run_id: RUN.into(),
            cwd: Some(PathBuf::from("/work/here")),
        }]
    );
}

#[test]
fn a_failed_clone_shows_its_reason_and_c_tries_again_and_a_partial_zip_is_cloned_too() {
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    app.update(downloaded());
    press(&mut app, KeyCode::Char('C'));
    app.update(Msg::Cloned {
        run_id: RUN.into(),
        result: Err(
            "git is not available. On macOS, install the Xcode command line tools with xcode-select --install".into(),
        ),
    });
    let text = screen_text(&app, 160, 40);
    assert!(
        text.contains("Clone     failed: git is not available"),
        "{text}"
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('C')).len(),
        1,
        "a retry is allowed"
    );

    // A held run's zip is a `partial` artifact: C clones it too (the shared flow chooses the artifact and flags it in the answer).
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    let Msg::Downloaded {
        result: Ok(mut p), ..
    } = downloaded()
    else {
        unreachable!()
    };
    p.artifact.kind = ArtifactKind::Partial;
    app.update(Msg::Downloaded {
        run_id: RUN.into(),
        result: Ok(p),
    });
    let effects = press(&mut app, KeyCode::Char('C'));
    assert!(
        matches!(effects.as_slice(), [Effect::Clone { .. }]),
        "{effects:?}"
    );
}

/// A clone answers on a blocking thread, so its answer can arrive after the person opened another run. The answer carries the run it
/// is for, and the dashboard of any other run ignores it: it must not replace that run's card with someone else's directory.
#[test]
fn a_clone_answer_for_another_run_is_ignored() {
    let other = "01K6A0000000000000000000R2";
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    app.update(downloaded());
    assert_eq!(press(&mut app, KeyCode::Char('C')).len(), 1);
    app.update(Msg::Cloned {
        run_id: other.into(),
        result: Ok(cloned()),
    });
    let text = screen_text(&app, 120, 40);
    assert!(text.contains("Clone     cloning"), "{text}");
    assert!(!text.contains("/downloads/python-calc (branch"), "{text}");
    app.update(Msg::Cloned {
        run_id: other.into(),
        result: Err("git is not available".into()),
    });
    let text = screen_text(&app, 120, 40);
    assert!(text.contains("Clone     cloning"), "{text}");
    assert!(!text.contains("git is not available"), "{text}");
    // The answer for the run on screen lands.
    app.update(Msg::Cloned {
        run_id: RUN.into(),
        result: Ok(cloned()),
    });
    assert!(screen_text(&app, 120, 40).contains("/downloads/python-calc (branch"));
}

/// `C` before anything is delivered only says so. After the zip failed to download it still clones: the shared flow downloads again.
#[test]
fn c_before_delivery_says_so_and_after_a_failed_download_it_still_clones() {
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(working_pages());
    assert!(press(&mut app, KeyCode::Char('C')).is_empty());
    assert!(
        screen_text(&app, 120, 40)
            .contains("nothing is delivered yet, so there is nothing to clone"),
        "{}",
        screen_text(&app, 120, 40)
    );
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    app.update(Msg::Downloaded {
        run_id: RUN.into(),
        result: Err("download: sha256 mismatch".into()),
    });
    let effects = press(&mut app, KeyCode::Char('C'));
    assert_eq!(
        effects,
        vec![Effect::Clone {
            run_id: RUN.into(),
            cwd: None,
        }],
        "the clone flow downloads the zip again"
    );
}

/// A run of the stand-in stage that completed with the fixture's zip, the fixture, and the API.
async fn delivered_run(label: &str) -> (Arc<Fake>, Api<HttpsPlane>, String, clonefix::Fixture) {
    let fake = Fake::start().await;
    let a = api(&fake);
    let run = a
        .start_direct_run(
            &serde_json::from_value(json!({
                "prompt": label, "target": {"mode": "s3-only"}, "model": SONNET
            }))
            .unwrap(),
            &format!("k-{label}"),
        )
        .await
        .unwrap()
        .run_id;
    let fx = clonefix::build(label, &run);
    fake.mem.inner.lock().unwrap().deliverable = Some(fx.zip_bytes());
    fake.mem.complete_run(&run, true);
    (fake, a, run, fx)
}

#[tokio::test]
async fn the_clone_effect_fetches_the_zip_and_clones_it_into_the_working_directory() {
    let (_fake, a, run, fx) = delivered_run("tui-clone").await;
    let downloads = support::scratch("tui-clone-downloads");
    let here = support::scratch("tui-clone-here");
    // No zip is on this machine: the effect downloads it by the pull rules into the download directory, then clones into the working one.
    assert!(std::fs::read_dir(&downloads).unwrap().next().is_none());
    let msg = perform(
        &a,
        &downloads,
        Effect::Clone {
            run_id: run.clone(),
            cwd: Some(here.clone()),
        },
    )
    .await;
    let Some(Msg::Cloned {
        run_id,
        result: Ok(c),
    }) = msg
    else {
        panic!("{msg:?}")
    };
    assert_eq!(run_id, run, "the answer names the run it is for");
    assert_eq!(c.dir, here.join("python-calc"));
    assert_eq!(c.head, fx.head);
    clonefix::assert_tree(&c.dir, &fx.items);
    assert!(
        c.zip.starts_with(&downloads) && c.zip.is_file(),
        "the zip stays in the download directory: {:?}",
        c.zip
    );
    // A second clone into the same name stops on the directory that is not empty and says so.
    let msg = perform(
        &a,
        &downloads,
        Effect::Clone {
            run_id: run.clone(),
            cwd: Some(here),
        },
    )
    .await;
    let Some(Msg::Cloned {
        run_id,
        result: Err(e),
    }) = msg
    else {
        panic!("{msg:?}")
    };
    assert_eq!(run_id, run);
    assert!(e.contains("not empty"), "{e}");
}

#[tokio::test]
async fn the_clone_effect_without_a_directory_uses_the_download_directory() {
    let (_fake, a, run, fx) = delivered_run("tui-clone-dl").await;
    let downloads = support::scratch("tui-clone-dl-downloads");
    let msg = perform(
        &a,
        &downloads,
        Effect::Clone {
            run_id: run.clone(),
            cwd: None,
        },
    )
    .await;
    let Some(Msg::Cloned { result: Ok(c), .. }) = msg else {
        panic!("{msg:?}")
    };
    assert_eq!(c.dir, downloads.join("python-calc"));
    clonefix::assert_tree(&c.dir, &fx.items);
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[test]
fn runs_screen_states_and_keys() {
    let mut app = new_app("n", None);
    assert!(screen_text(&app, 80, 24).contains("no runs yet: press n to start one"));
    app.update(Msg::Runs(Err("HTTP 403: not authorized (http)".into())));
    assert!(screen_text(&app, 80, 24).contains("cannot list runs: HTTP 403"));
    assert!(press(&mut app, KeyCode::Enter).is_empty(), "no run to open");
    app.update(runs());
    let wide = screen_text(&app, 120, 40);
    assert!(
        wide.contains("CREATED") && wide.contains("2026-09-25T20:00:00Z"),
        "{wide}"
    );
    assert!(!screen_text(&app, 80, 24).contains("CREATED"));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('k'));
    assert!(screen_text(&app, 120, 40).contains(&format!("▶ {RUN}")));
    assert_eq!(press(&mut app, KeyCode::Char('r')), vec![Effect::LoadRuns]);
    assert!(press(&mut app, KeyCode::F(5)).is_empty());
    assert_eq!(app.handle_key(ctrl('c')), vec![Effect::Quit]);
    let mut release = key(KeyCode::Char('q'));
    release.kind = crossterm::event::KeyEventKind::Release;
    assert!(
        app.handle_key(release).is_empty(),
        "key releases are ignored"
    );
    let mut new_repo = new_app("n", None);
    new_repo.update(Msg::Runs(Ok(vec![summary(
        RUN,
        GoalStatus::Active,
        GoalPhase::Planning,
        serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new"})).unwrap(),
    )])));
    assert!(screen_text(&new_repo, 120, 40).contains("new cc:hamh-e2e-new"));
}

#[test]
fn the_runs_screen_leaves_out_probe_runs() {
    let probe: RunSummary = serde_json::from_value(json!({
        "runId": "01ZZZZZZZZZZZZZZZZZZZZPROB", "status": "cleared", "phase": "green",
        "target": {"mode": "s3-only"}, "createdAt": "2026-09-29T20:00:00Z",
        "ownerPrincipalArn": support::ME, "kind": "probe"
    }))
    .unwrap();
    let mut app = new_app("n", None);
    app.update(Msg::Runs(Ok(vec![
        probe,
        summary(
            RUN,
            GoalStatus::Active,
            GoalPhase::Gating,
            RunTarget::S3Only,
        ),
    ])));
    let screen = screen_text(&app, 120, 40);
    assert!(screen.contains(&format!("▶ {RUN}")), "{screen}");
    assert!(!screen.contains("01ZZZZZZZZZZZZZZZZZZZZPROB"), "{screen}");
}

#[test]
fn composer_editing_keys_and_states() {
    let mut app = new_app("n", None);
    press(&mut app, KeyCode::Char('n'));
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Backspace);
    assert!(
        app.handle_key(ctrl('x')).is_empty(),
        "other Ctrl keys do nothing"
    );
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Left); // wraps to --s3-only
    assert!(screen_text(&app, 120, 40).contains("not used with --s3-only"));
    press(&mut app, KeyCode::Right); // --repo
    press(&mut app, KeyCode::Right); // --new-repo
    press(&mut app, KeyCode::Tab);
    type_text(&mut app, "cc:new-onee");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Tab);
    type_text(&mut app, "mainx");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Tab); // Options
    press(&mut app, KeyCode::Tab); // Model
    press(&mut app, KeyCode::Enter); // confirms the model on the screen
    press(&mut app, KeyCode::Tab); // Submit
    let effects = press(&mut app, KeyCode::Enter);
    let [Effect::SubmitDirect { args, .. }] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert_eq!(args.prompt, "ab");
    assert_eq!(
        args.target,
        ham_core::command::TargetArg::NewRepo("cc:new-one".parse().unwrap())
    );
    assert_eq!(args.base.as_deref(), Some("main"));
    assert!(screen_text(&app, 120, 40).contains("submitting"));
    app.update(Msg::Started(Err(
        "HTTP 422: this Idempotency-Key was used".into()
    )));
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains("error: HTTP 422") && !text.contains("submitting"),
        "{text}"
    );
    // Shift-Tab goes once round the seven fields back to Submit; Esc leaves.
    for _ in 0..7 {
        press(&mut app, KeyCode::BackTab);
    }
    assert!(screen_text(&app, 120, 40).contains("▶ [ Submit ]"));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen(), Screen::Runs);
    // A composer opens with nothing to load.
    assert!(press(&mut app, KeyCode::Char('n')).is_empty());
}

#[test]
fn dashboard_panels_for_blocked_and_download_states() {
    let mut app = new_app("n", None);
    app.open_run(RUN);
    let events = vec![
        env(
            1,
            EventType::RunCreated,
            None,
            json!({"target": {"mode": "s3-only"}}),
        ),
        started(2, "direct", "cline", SONNET),
        env(
            5,
            EventType::GoalBlocked,
            None,
            json!({"reason": "the session ended \u{1b}[31mbefore any work was saved"}),
        ),
        // An agent cannot complete the run: the agent-authored event is counted and changes nothing.
        env(
            6,
            EventType::GoalStatus,
            Some("direct"),
            json!({"status": "complete"}),
        ),
    ];
    app.update(pages(events));
    let text = screen_text(&app, 120, 40);
    for want in [
        "reason: the session ended before any work was saved",
        "ignored 1 forged events",
        // A blocked run has ended: the card offers the retry in place of a cancel.
        "blocked · r retry",
        "blocked (",
    ] {
        assert!(text.contains(want), "missing {want:?}:\n{text}");
    }
    assert!(!text.contains('\u{1b}'));
    // The keys of the multi-agent line do nothing.
    for c in ['p', 'R', 'y', 'x'] {
        assert!(press(&mut app, KeyCode::Char(c)).is_empty(), "{c}");
    }
    assert!(press(&mut app, KeyCode::Tab).is_empty());
    app.update(Msg::Pages(Err("HTTP 503: unavailable".into())));
    app.tick();
    app.update(Msg::Pages(Err("HTTP 503: unavailable".into())));
    app.tick();
    app.update(Msg::Pages(Err("HTTP 503: unavailable".into())));
    assert!(screen_text(&app, 120, 40).contains("Connection lost, retrying (2 s): HTTP 503"));

    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(delivered_pages());
    assert!(screen_text(&app, 120, 40).contains("downloading the zip"));
    app.update(Msg::Downloaded {
        run_id: RUN.into(),
        result: Err("download: sha256 mismatch".into()),
    });
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains("download failed: download: sha256 mismatch"),
        "{text}"
    );
    assert!(text.contains(&format!("run: ham pull {RUN}")), "{text}");
    app.update(Msg::Detail(Ok(serde_json::from_value(json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "delivering", "target": {"mode": "s3-only"},
                    "createdAt": "2026-09-25T20:00:00Z", "ownerPrincipalArn": support::ME},
        "tokensUsed": 0, "costUsd": 1.5,
        "cost": {"pricesVersion": "2026-10-01", "turnsWithoutUsage": 0, "lines": [
            {"modelId": SONNET, "engine": "cline", "usd": 1.5, "usage": {
                "inputTokens": 4000, "outputTokens": 321, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0}}]}
    }))
    .unwrap())));
    app.update(downloaded());
    assert!(screen_text(&app, 120, 40).contains("Tokens    4321  USD 1.50"));
    assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::LoadRuns]);
    assert_eq!(app.screen(), Screen::Runs);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    let mut dash = new_app("n", None);
    dash.open_run(RUN);
    assert!(press(&mut dash, KeyCode::Char('z')).is_empty());
    assert!(press(&mut dash, KeyCode::Char('n')).is_empty());
    assert_eq!(dash.screen(), Screen::Composer);
}

#[test]
fn the_dashboard_says_why_a_run_ended_early() {
    // A run whose root execution was stopped, failed or timed out is cleared by the
    // harness with the execution's end; the dashboard shows it where a blocked run shows why.
    let mut app = new_app("n", None);
    app.open_run(RUN);
    app.update(pages(vec![
        env(1, EventType::RunCreated, None, json!({"target": {"mode": "s3-only"}})),
        started(2, "direct", "cline", SONNET),
        env(3, EventType::GoalStatus, None, json!({"status": "cleared", "phase": "green"})),
        env(
            4,
            EventType::RunAborted,
            None,
            json!({"status": "ABORTED", "error": "HarnessOperatorStop",
                   "cause": "operator: orphan check \u{1b}[31m", "stoppedAt": "2026-09-29T21:30:26Z"}),
        ),
    ]));
    let text = screen_text(&app, 120, 40);
    for want in [
        "ended early",
        "ABORTED HarnessOperatorStop",
        "cause: operator: orphan check",
    ] {
        assert!(text.contains(want), "missing {want:?}:\n{text}");
    }
    // A cleared run names its status alone: its phase (`green`) is only the last one the service wrote (it read `cleared (green)`).
    assert!(text.lines().any(|l| l.trim() == "cleared"), "{text}");
    assert!(!text.contains("cleared ("), "{text}");
    assert!(!text.contains('\u{1b}'));
    assert!(
        !text.contains("blocked ·"),
        "a cleared run has no blocked panel:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_event_loop_draws_runs_effects_and_quits() {
    let fake = Fake::start().await;
    let a = api(&fake);
    let run = a
        .start_direct_run(
            &serde_json::from_value::<StartDirectRunRequest>(
                json!({"prompt": "loop", "target": {"mode": "s3-only"}, "model": SONNET}),
            )
            .unwrap(),
            "k-loop",
        )
        .await
        .unwrap()
        .run_id;
    fake.mem.complete_direct_run_on(
        &run,
        SONNET,
        harness_protocol::direct::DirectOutcome::ReportedDone,
        None,
    );
    let dir = support::scratch("tui-loop");

    // The Runs screen: the list loads, Ctrl-C quits.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let mut app = new_app("n", None);
    let first = app.start();
    let quitter = tokio::spawn({
        let fake = fake.clone();
        async move {
            for _ in 0..500 {
                if fake
                    .mem
                    .seen()
                    .iter()
                    .any(|s| s.path == "/v1/runs" && s.method == "GET")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.send(crossterm::event::Event::Key(ctrl('c'))).unwrap();
            tx
        }
    });
    let view = ham::tui::event_loop(
        &mut term,
        a.clone(),
        dir.clone(),
        app,
        first,
        rx,
        Duration::from_millis(20),
    )
    .await
    .unwrap();
    assert!(view.is_none());
    let _tx = quitter.await.unwrap();
    let buf = term.backend().buffer().clone();
    let text: String = (0..40u16)
        .map(|y| {
            (0..120u16)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("ham · Runs"), "{text}");

    // The dashboard of a delivered run: polled on the timer, the zip downloads, then quit.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = new_app("n", None);
    let first = app.open_run(&run);
    let zip = dir.join(format!("python-calc-{run}-abc1234.zip"));
    let quitter = tokio::spawn({
        let zip = zip.clone();
        async move {
            for _ in 0..500 {
                if zip.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.send(crossterm::event::Event::Resize(120, 40)).unwrap();
            tx.send(crossterm::event::Event::Key(key(KeyCode::Char('q'))))
                .unwrap();
            tx
        }
    });
    let view = ham::tui::event_loop(&mut term, a, dir, app, first, rx, Duration::from_millis(20))
        .await
        .unwrap()
        .unwrap();
    let _tx = quitter.await.unwrap();
    assert_eq!(view.status, Some(GoalStatus::Complete));
    assert!(zip.exists());
}

/// The key reader of the real TUI is a thread that ends when `crossterm::event::read` fails (the terminal is gone). The loop used to keep
/// drawing and polling for ever then, because only a quit key ended it: a `ham` that nobody could use or stop. A flaky test showed it:
/// its driver panicked, the sender went with it and the test hung instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_event_loop_ends_when_nobody_can_type_any_more() {
    let fake = Fake::start().await;
    let a = api(&fake);
    let dir = support::scratch("tui-loop-closed");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crossterm::event::Event>();
    drop(tx);
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let mut app = new_app("n", None);
    let first = app.start();
    let ended = tokio::time::timeout(
        Duration::from_secs(10),
        ham::tui::event_loop(&mut term, a, dir, app, first, rx, Duration::from_millis(20)),
    )
    .await;
    let view = ended
        .expect("the loop kept running although its key channel was closed")
        .unwrap();
    assert!(view.is_none(), "no run was focused");
}
