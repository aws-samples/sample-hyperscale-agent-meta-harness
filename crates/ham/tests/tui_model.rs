//! The model in the New-run composer of a direct run.
//! A direct composer (`App::new_direct`) shows the model the run will use
//! in its title and lists the supported models from the compiled-in `models.json` (no network): it opens on the user's default
//! (`HAMH_MODEL`, then the config file's `default_model`, else Claude Sonnet 5.5), the user picks another with the arrow keys, and
//! the pick lasts for that run only. `d` on the model field saves the pick as the default (`Effect::SaveDefaultModel`), and
//! nothing else writes the config file. Submit sends `POST /v1/direct/runs` with the pick. Screens are pinned by insta
//! snapshots at 80x24, 120x40 and 200x60.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, perform};
use ham_core::command::{DirectArgs, TargetArg};
use ham_core::models::{ModelPrefs, Models, builtin};
use harness_protocol::direct::MAX_REQUEST_BYTES;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds};

const SIZES: [(u16, u16); 3] = [(80, 24), (120, 40), (200, 60)];
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";
const ASTRA: &str = "us.openai.gpt-6-astra";

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn type_text(app: &mut App, s: &str) {
    for ch in s.chars() {
        let effects = press(app, KeyCode::Char(ch));
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
        let mut x = 0;
        while x < w {
            let sym = buf[(x, y)].symbol();
            line.push_str(sym);
            x += (ratatui::text::Span::raw(sym).width() as u16).max(1);
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

fn prefs(env: Option<&str>, config: Option<&str>) -> ModelPrefs {
    ModelPrefs {
        env: env.map(str::to_string),
        config: config.map(str::to_string),
    }
}

fn settings(p: ModelPrefs) -> DirectSettings {
    DirectSettings {
        prefs: p,
        rules: None,
        no_rules: true,
    }
}

/// A composer on the Runs screen's `n`, with the prompt focused.
fn composer(p: ModelPrefs) -> App {
    let mut app = App::new_direct(
        "nonce-1".into(),
        Some("cc:hamh-e2e-python-calc".into()),
        settings(p),
    );
    let effects = press(&mut app, KeyCode::Char('n'));
    assert!(
        effects.is_empty(),
        "the direct composer needs no network: {effects:?}"
    );
    assert_eq!(app.screen(), Screen::Composer);
    app
}

fn focus_model(app: &mut App) {
    for _ in 0..5 {
        press(app, KeyCode::Tab); // target, repository, base ref, options, model
    }
}

fn line_with(text: &str, needle: &str) -> String {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_default()
        .to_string()
}

// ---- the model shown and the list ------------------------------------------------------------------------------------

#[test]
fn the_direct_composer_opens_on_claude_sonnet_5_5_and_lists_the_supported_models() {
    let app = composer(ModelPrefs::default());
    assert_eq!(app.chosen_model(), Some(SONNET));
    let text = screen_text(&app, 120, 40);
    let title = text.lines().next().unwrap();
    assert!(
        title.contains("New run") && title.contains("Claude Sonnet 5.5"),
        "{text}"
    );
    assert!(
        title.contains("default"),
        "the title says it is the default: {text}"
    );
    for id in [OPUS, SONNET, SOL, ASTRA, "us.zai.glm-5.3"] {
        assert!(text.contains(id), "{id} missing: {text}");
    }
    assert!(
        !text.contains("Context") && !text.contains("/v1/models"),
        "{text}"
    );
    assert!(
        line_with(&text, SONNET).contains("(•)"),
        "the chosen row is marked: {text}"
    );
    for other in [OPUS, SOL, ASTRA] {
        assert!(line_with(&text, other).contains("( )"), "{other}: {text}");
    }
    // The composer lists the compiled-in models: opening it loads nothing.
    let mut fresh = App::new_direct("n".into(), None, settings(ModelPrefs::default()));
    assert!(press(&mut fresh, KeyCode::Char('n')).is_empty());
}

#[test]
fn it_opens_on_the_users_default_in_the_order_variable_then_config_then_built_in() {
    for (p, want, label) in [
        (prefs(None, Some("sol")), SOL, "config default_model"),
        (prefs(Some("astra"), Some("sol")), ASTRA, "HAMH_MODEL"),
        (prefs(Some(""), Some(OPUS)), OPUS, "config default_model"),
        (prefs(Some("  "), None), SONNET, "built-in default"),
    ] {
        let app = composer(p.clone());
        assert_eq!(app.chosen_model(), Some(want), "{p:?}");
        let text = screen_text(&app, 120, 40);
        assert!(
            text.lines().next().unwrap().contains(label),
            "{label}: {text}"
        );
    }
}

#[test]
fn a_wrong_variable_or_config_value_still_opens_the_composer_on_sonnet_and_says_so() {
    for (p, source) in [
        (prefs(Some("gpt-9"), None), "HAMH_MODEL"),
        (prefs(None, Some("zai.glm-5")), "config default_model"),
    ] {
        let app = composer(p.clone());
        assert_eq!(app.chosen_model(), Some(SONNET), "{p:?}");
        let text = screen_text(&app, 120, 40);
        assert!(
            text.contains(source) && text.contains("not a supported model"),
            "{text}"
        );
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("ham models lists them"),
            "it points at the list: {text}"
        );
    }
}

#[test]
fn a_fallback_must_be_confirmed_before_it_is_sent() {
    // The variable names nothing; the composer is on Sonnet with a note. A user who missed the note must not send Sonnet by accident.
    let mut app = composer(prefs(Some("opsu"), None));
    type_text(&mut app, "Fix it");
    let effects = submit(&mut app);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen_text(&app, 160, 40);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("press Enter on the Model field to confirm"),
        "{text}"
    );
    // Enter on the Model field confirms the model on the screen; then it sends.
    focus_model(&mut app);
    assert!(press(&mut app, KeyCode::Enter).is_empty());
    let (args, _) = only_direct(submit(&mut app));
    assert_eq!(args.model.as_deref(), Some(SONNET));
    // Choosing another model confirms too.
    let mut app = composer(prefs(None, Some("zai.glm-5")));
    type_text(&mut app, "Fix it");
    focus_model(&mut app);
    press(&mut app, KeyCode::Up);
    let (args, _) = only_direct(submit(&mut app));
    assert_eq!(args.model.as_deref(), Some(OPUS));
    // A good default never asks.
    let mut app = composer(prefs(None, Some("sol")));
    type_text(&mut app, "Fix it");
    assert_eq!(only_direct(submit(&mut app)).0.model.as_deref(), Some(SOL));
}

#[test]
fn saving_a_default_under_a_ham_model_override_says_the_variable_still_wins() {
    let mut app = composer(prefs(Some("astra"), None));
    focus_model(&mut app);
    press(&mut app, KeyCode::Up);
    let id = app.chosen_model().unwrap().to_string();
    let effects = press(&mut app, KeyCode::Char('d'));
    assert_eq!(effects.len(), 1);
    app.update(Msg::DefaultSaved(Ok(id)));
    let text = screen_text(&app, 200, 40);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("HAMH_MODEL is set and still wins"), "{text}");
    assert!(!text.contains("new composers open on it"), "{text}");
}

// ---- picking ----------------------------------------------------------------------------------------------------------

#[test]
fn the_arrow_keys_pick_a_model_and_the_title_follows() {
    let mut app = composer(ModelPrefs::default());
    focus_model(&mut app);
    let order: Vec<String> = builtin()
        .models()
        .iter()
        .filter(|m| m.selectable())
        .map(|m| m.id.clone())
        .collect();
    let at = order.iter().position(|m| m == SONNET).unwrap();
    // Down moves to the next selectable model in list order, Up back; the ends hold.
    press(&mut app, KeyCode::Home);
    assert_eq!(app.chosen_model(), Some(order[0].as_str()));
    press(&mut app, KeyCode::Up);
    assert_eq!(app.chosen_model(), Some(order[0].as_str()), "the top holds");
    press(&mut app, KeyCode::End);
    assert_eq!(app.chosen_model(), Some(order[order.len() - 1].as_str()));
    press(&mut app, KeyCode::Down);
    assert_eq!(
        app.chosen_model(),
        Some(order[order.len() - 1].as_str()),
        "the bottom holds"
    );
    for _ in 0..order.len() {
        press(&mut app, KeyCode::Up);
    }
    for _ in 0..at {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(app.chosen_model(), Some(SONNET));
    press(&mut app, KeyCode::Down);
    let next = app.chosen_model().unwrap().to_string();
    assert_eq!(next, order[at + 1]);
    let text = screen_text(&app, 120, 40);
    let name = &builtin().find(&next).unwrap().name;
    assert!(
        text.lines().next().unwrap().contains(name.as_str()),
        "{text}"
    );
    assert!(
        text.lines().next().unwrap().contains("chosen here"),
        "a pick made in the composer says so: {text}"
    );
    assert!(line_with(&text, &next).contains("(•)"), "{text}");
}

#[test]
fn a_model_without_a_smoke_record_is_listed_as_pending_and_cannot_be_picked() {
    let models: &'static Models = Box::leak(Box::new(
        Models::parse(
            &json!({"schemaVersion": 1, "models": [
                {"id": "a.one", "name": "One", "vendor": "v", "alias": "one", "default": true, "baseModelId": "v.one", "protocol": "converse",
                 "region": "us-east-1", "contextTokens": 100000, "maxOutputTokens": 8192, "smokeRecordId": "s6:x+K3:y"},
                {"id": "a.two", "name": "Two", "vendor": "v", "alias": "two", "baseModelId": "v.two", "protocol": "converse",
                 "region": "us-east-1", "contextTokens": 100000, "maxOutputTokens": 8192},
                {"id": "a.three", "name": "Three", "vendor": "v", "alias": "three", "baseModelId": "v.three", "protocol": "converse",
                 "region": "us-east-1", "contextTokens": 100000, "maxOutputTokens": 8192, "smokeRecordId": "s6:z+K3:w"}
            ]})
            .to_string(),
        )
        .unwrap(),
    ));
    let mut app = App::new_direct_with(models, "n".into(), None, settings(ModelPrefs::default()));
    press(&mut app, KeyCode::Char('n'));
    focus_model(&mut app);
    let text = screen_text(&app, 120, 40);
    assert!(line_with(&text, "a.two").contains("pending"), "{text}");
    press(&mut app, KeyCode::Down);
    assert_eq!(
        app.chosen_model(),
        Some("a.three"),
        "the pending row is skipped"
    );
    press(&mut app, KeyCode::Up);
    assert_eq!(app.chosen_model(), Some("a.one"));
    // A default or a stored choice that is pending falls back to the list's default.
    let app = App::new_direct_with(models, "n".into(), None, settings(prefs(None, Some("two"))));
    assert_eq!(app.chosen_model(), Some("a.one"));
}

#[test]
fn a_pick_lasts_for_that_composer_only_and_nothing_is_saved_by_picking() {
    let mut app = composer(ModelPrefs::default());
    focus_model(&mut app);
    for key in [
        KeyCode::Down,
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::End,
        KeyCode::Home,
    ] {
        let effects = press(&mut app, key);
        assert!(effects.is_empty(), "picking is not saving: {effects:?}");
    }
    press(&mut app, KeyCode::End);
    assert_ne!(app.chosen_model(), Some(SONNET));
    // Esc and a new composer: back to the default.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.chosen_model(), Some(SONNET));
}

// ---- submit -----------------------------------------------------------------------------------------------------------

fn submit(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
}

fn only_direct(effects: Vec<Effect>) -> (DirectArgs, String) {
    match effects.as_slice() {
        [Effect::SubmitDirect { args, nonce }] => (args.clone(), nonce.clone()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn submit_sends_the_model_on_the_screen_with_the_prompt_unchanged() {
    // Untouched: the default.
    let mut app = composer(ModelPrefs::default());
    type_text(&mut app, "Add a percent_change(a, b) function");
    let (args, _) = only_direct(submit(&mut app));
    assert_eq!(
        args.model.as_deref(),
        Some(SONNET),
        "the shown model travels as an explicit choice"
    );
    assert_eq!(args.prompt, "Add a percent_change(a, b) function");
    assert!(matches!(args.target, TargetArg::Repo(_)));
    assert!(args.no_rules && args.rules.is_none());
    // A pick.
    let mut app = composer(prefs(None, Some("sol")));
    type_text(&mut app, "Fix it");
    focus_model(&mut app);
    press(&mut app, KeyCode::Up);
    let picked = app.chosen_model().unwrap().to_string();
    assert_ne!(picked, SOL);
    let (args, _) = only_direct(submit(&mut app));
    assert_eq!(args.model.as_deref(), Some(picked.as_str()));
    // Enter on the Submit field does the same.
    press(&mut app, KeyCode::Tab);
    let effects = press(&mut app, KeyCode::Enter);
    assert_eq!(
        only_direct(effects).0.model.as_deref(),
        Some(picked.as_str())
    );
}

#[test]
fn submit_refuses_an_empty_prompt_and_a_bad_repository_with_the_parsers_words() {
    // No default repository, so the Repository field starts empty.
    let mut app = App::new_direct("n".into(), None, settings(ModelPrefs::default()));
    press(&mut app, KeyCode::Char('n'));
    assert!(submit(&mut app).is_empty());
    assert!(screen_text(&app, 120, 40).contains("a prompt is required"));
    type_text(&mut app, "x");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    type_text(&mut app, "not-a-repo");
    assert!(submit(&mut app).is_empty());
    let text = screen_text(&app, 120, 40);
    assert!(text.contains("--repo"), "{text}");
}

// ---- save as default --------------------------------------------------------------------------------------------------

#[test]
fn d_on_the_model_field_asks_to_save_the_pick_and_the_answer_updates_the_default() {
    let mut app = composer(ModelPrefs::default());
    // On the prompt field `d` is a letter.
    type_text(&mut app, "d");
    focus_model(&mut app);
    press(&mut app, KeyCode::Up);
    let picked = app.chosen_model().unwrap().to_string();
    let effects = press(&mut app, KeyCode::Char('d'));
    assert_eq!(
        effects,
        [Effect::SaveDefaultModel {
            model_id: picked.clone()
        }]
    );
    // Nothing is claimed before the answer.
    assert!(!screen_text(&app, 120, 40).contains("Saved"));
    let effects = app.update(Msg::DefaultSaved(Ok(picked.clone())));
    assert!(effects.is_empty());
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains("Saved") && text.contains("default_model"),
        "{text}"
    );
    assert!(line_with(&text, &picked).contains("your default"), "{text}");
    // The next composer opens on it.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.chosen_model(), Some(picked.as_str()));
    let title = screen_text(&app, 120, 40)
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(title.contains("config default_model"), "{title}");
}

#[test]
fn a_failed_save_says_so_and_changes_no_default() {
    let mut app = composer(ModelPrefs::default());
    focus_model(&mut app);
    press(&mut app, KeyCode::Down);
    let picked = app.chosen_model().unwrap().to_string();
    let effects = press(&mut app, KeyCode::Char('d'));
    assert_eq!(effects.len(), 1);
    app.update(Msg::DefaultSaved(Err(
        "/home/x/.config/ham/config.toml: Permission denied".into(),
    )));
    let text = screen_text(&app, 120, 40);
    assert!(
        text.contains("could not save") && text.contains("Permission denied"),
        "{text}"
    );
    assert!(
        !line_with(&text, &picked).contains("your default"),
        "the failed save changed no default: {text}"
    );
    assert_eq!(
        app.chosen_model(),
        Some(picked.as_str()),
        "the pick stays for this run"
    );
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.chosen_model(), Some(SONNET));
}

// ---- a short terminal keeps every model on the screen ------------------------------------------------------------------

const ALIASES: [&str; 5] = ["opus", "sonnet", "sol", "astra", "glm"];

/// The rows of the Model box, one per listed alias, by the `) <alias> ` that follows the radio mark.
fn model_rows(text: &str) -> Vec<String> {
    ALIASES
        .iter()
        .map(|a| line_with(text, &format!(") {a} ")))
        .collect()
}

/// The merge review found GLM 5.3 squeezed off an 80x24 screen whenever the message line wrapped to two lines: the
/// prompt box kept four rows (`Min(4)`), the model box lost one, and a pick of the fifth model could not be seen.
#[test]
fn all_five_model_rows_stay_on_an_80x24_screen_when_the_message_wraps_to_two_lines() {
    let app = composer(prefs(None, Some("zai.glm-5"))); // "config default_model ... is not a supported model; ..."
    let text = screen_text(&app, 80, 24);
    assert!(
        text.contains("is not a supported model"),
        "the long message is on the screen: {text}"
    );
    for (alias, row) in ALIASES.iter().zip(model_rows(&text)) {
        assert!(!row.is_empty(), "no row for {alias} at 80x24:\n{text}");
    }
}

/// Picking the last model and saving it adds a note under the list; the chosen row stays visible with its mark.
#[test]
fn the_picked_fifth_model_and_the_saved_note_fit_together_on_an_80x24_screen() {
    let mut app = composer(ModelPrefs::default());
    focus_model(&mut app);
    for _ in 0..3 {
        press(&mut app, KeyCode::Down); // sonnet, sol, astra, glm
    }
    let picked = app.chosen_model().unwrap().to_string();
    assert!(picked.contains("glm"), "the fifth row is GLM 5.3: {picked}");
    press(&mut app, KeyCode::Char('d'));
    app.update(Msg::DefaultSaved(Ok(picked)));
    let text = screen_text(&app, 80, 24);
    assert!(
        text.contains("Saved") && text.contains("default_model"),
        "{text}"
    );
    let rows = model_rows(&text);
    assert!(
        rows.iter().all(|r| !r.is_empty()),
        "every row is shown at 80x24:\n{text}"
    );
    assert!(
        rows[4].contains("(•)") && rows[4].contains("your default"),
        "the chosen row shows its mark and the default note: {}",
        rows[4]
    );
}

// ---- the whole path against the direct stage --------------------------------------------------------------------------

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_posts_the_picked_model_to_the_direct_route_and_opens_the_dashboard() {
    let fake = Fake::start_with(true).await;
    let a = api(&fake);
    let dir = support::scratch("tui-model-flow");
    let rules = dir.join("rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(rules.join("00-my-rules.md"), "Write the test first.\n").unwrap();
    let mut app = App::new_direct(
        ham::start::new_nonce(),
        Some("cc:hamh-e2e-python-calc".into()),
        DirectSettings {
            prefs: ModelPrefs::default(),
            rules: Some(rules.display().to_string()),
            no_rules: false,
        },
    );
    press(&mut app, KeyCode::Char('n'));
    type_text(&mut app, "Add a percent_change(a, b) function with tests");
    focus_model(&mut app);
    press(&mut app, KeyCode::Home);
    let picked = app.chosen_model().unwrap().to_string();
    assert_ne!(picked, "", "a model is on the screen");
    let effects = submit(&mut app);
    assert_eq!(effects.len(), 1);
    let msg = perform(&a, Path::new(&dir), effects.into_iter().next().unwrap())
        .await
        .unwrap();
    let Msg::Started(Ok(started)) = &msg else {
        panic!("{msg:?}")
    };
    let run_id = started.run_id.clone();
    app.update(msg);
    assert_eq!(app.screen(), Screen::Dashboard);
    assert_eq!(app.focused_run(), Some(run_id.as_str()));

    let posts = fake.mem.posts("/v1/direct/runs");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].signature, Ok(()));
    assert!(!posts[0].headers["idempotency-key"].is_empty());
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(body["model"], picked.as_str());
    assert_eq!(
        body["prompt"],
        "Add a percent_change(a, b) function with tests"
    );
    assert_eq!(body["rules"][0]["path"], "00-my-rules.md");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_refusal_shows_in_the_composer_and_keeps_the_pick() {
    let fake = Fake::start_with(true).await;
    // control-api refuses a prompt over its limit (413 `payload-too-large`) and the client does not check the length first:
    // the refusal text reaches the composer.
    let a = api(&fake);
    let dir = support::scratch("tui-model-413");
    let mut app = composer(prefs(None, Some("opus")));
    // A person gets a prompt over the bound of a request into the Prompt by pasting it, which is one edit. The editor of the crate
    // lays the text out again after every edit, so typing it key by key is quadratic.
    assert!(
        app.handle_paste("x".repeat(MAX_REQUEST_BYTES + 1))
            .is_empty()
    );
    let msg = perform(&a, &dir, submit(&mut app).into_iter().next().unwrap())
        .await
        .unwrap();
    app.update(msg);
    let text = screen_text(&app, 120, 40);
    assert!(text.contains("413"), "{text}");
    assert!(text.contains("payload-too-large"), "{text}");
    assert_eq!(app.screen(), Screen::Composer);
    assert_eq!(app.chosen_model(), Some(OPUS));
}

// ---- snapshots --------------------------------------------------------------------------------------------------------

#[test]
fn snapshots_of_the_direct_composer() {
    let mut app = composer(ModelPrefs::default());
    type_text(&mut app, "Add a percent_change(a, b) function with tests");
    snapshot_sizes(&app, "dm_composer_default");

    focus_model(&mut app);
    press(&mut app, KeyCode::Up);
    snapshot_sizes(&app, "dm_composer_picked");

    press(&mut app, KeyCode::Char('d'));
    let id = app.chosen_model().unwrap().to_string();
    app.update(Msg::DefaultSaved(Ok(id)));
    snapshot_sizes(&app, "dm_composer_saved");

    let mut app = composer(prefs(Some("astra"), Some("sol")));
    type_text(&mut app, "Use the variable");
    snapshot_sizes(&app, "dm_composer_from_variable");

    let app = composer(prefs(None, Some("zai.glm-5")));
    snapshot_sizes(&app, "dm_composer_bad_config");

    // The fifth model picked and saved: the last row and the saved note together on the shortest screen. The merge review found
    // GLM 5.3 squeezed off an 80x24 screen whenever the message wrapped.
    let mut app = composer(ModelPrefs::default());
    focus_model(&mut app);
    for _ in 0..3 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Char('d'));
    let id = app.chosen_model().unwrap().to_string();
    app.update(Msg::DefaultSaved(Ok(id)));
    insta::assert_snapshot!("dm_composer_glm_saved_80x24", screen_text(&app, 80, 24));
}
