//! Provider data sharing in the AWS account: a run on a model that needs provider data sharing turns sharing on in the stage's account
//! when the user agrees. In the terminal app, a pick of such a model with the setting off asks first: the question names the model, what sharing
//! means and the account and Region it changes, and only `y` starts the run, with the mode switched and read back. `run.provider_sharing` on in
//! `config.toml` is the user's agreement: the switch runs without a question and the run screen says so in one line. A denied switch or a switch
//! whose read back does not let the model run stops the start before `POST /v1/direct/runs`, with one plain message; so does `n`.
//!
//! The stage is the local `/v1` API (the real control-api handler); the Bedrock control plane is a stand-in that answers the reads and the switch
//! from a script and records what was asked. No call leaves the machine.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::api::Api;
use ham::http::{ControlPlane, Response, RetentionAnswer, TransportError};
use ham::tui::ext::{Done, Job, perform_with_env};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen, SharingAsk, submit_msg_with_env};
use ham_core::command::{Command, DirectArgs, parse};
use ham_core::sharing::account_of_arn;
use harness_protocol::api::v1::StartRunResponse;
use home::{Home, home};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use support::Fake;

const FABLE: &str = "global.anthropic.claude-fable-5-2";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const STARTS: &str = "/v1/direct/runs";

type Answer = Option<Result<RetentionAnswer, TransportError>>;

/// The local stage for `/v1`, and a scripted Bedrock control plane for the mode: each read takes the next answer of `reads` (the last one is
/// repeated), the switch answers `put` and records the mode it was asked for. `whoami` replaces the stage's answer to `GET /v1/whoami`.
struct Plane {
    inner: ham::https::HttpsPlane,
    reads: Mutex<VecDeque<Answer>>,
    put: Answer,
    whoami: Option<String>,
    /// The stage refuses every start with this message (a start that fails after the account was looked at).
    refuse_start: Mutex<Option<String>>,
    gets: Mutex<usize>,
    puts: Mutex<Vec<String>>,
}

impl ControlPlane for Plane {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        if let (Some(why), "POST", STARTS) = (&*self.refuse_start.lock().unwrap(), method, path) {
            return Ok(Response {
                status: 422,
                body: serde_json::json!({"code": "refused", "message": why})
                    .to_string()
                    .into_bytes(),
                ..Response::default()
            });
        }
        if let (Some(w), "/v1/whoami") = (&self.whoami, path) {
            return Ok(Response {
                status: 200,
                body: w.as_bytes().to_vec(),
                ..Response::default()
            });
        }
        self.inner.call(method, path, headers, body).await
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        self.inner.fetch(url, range, if_match).await
    }

    async fn data_retention(&self, _target: Option<&str>) -> Answer {
        *self.gets.lock().unwrap() += 1;
        let mut q = self.reads.lock().unwrap();
        if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            q.front().cloned().unwrap_or(None)
        }
    }

    async fn put_data_retention(&self, _target: Option<&str>, mode: &str) -> Answer {
        self.puts.lock().unwrap().push(mode.to_string());
        self.put.clone()
    }
}

fn answer(status: u16, body: &str) -> Answer {
    Some(Ok(RetentionAnswer {
        region: support::REGION.into(),
        response: Response {
            status,
            body: body.as_bytes().to_vec(),
            ..Response::default()
        },
    }))
}

fn mode(m: &str) -> Answer {
    answer(
        200,
        &format!(r#"{{"mode":"{m}","updatedAt":"2026-11-10T16:00:00Z"}}"#),
    )
}

fn account() -> &'static str {
    account_of_arn(support::ME).unwrap()
}

struct Stage {
    fake: Arc<Fake>,
    plane: Arc<Plane>,
    api: Api<Plane>,
    home: Home,
}

impl Stage {
    async fn new(label: &str, reads: Vec<Answer>, put: Answer) -> Stage {
        Stage::with_whoami(label, reads, put, None).await
    }

    async fn with_whoami(
        label: &str,
        reads: Vec<Answer>,
        put: Answer,
        whoami: Option<String>,
    ) -> Stage {
        let fake = Fake::start_with(true).await;
        let h = home(&fake, label);
        let inner = ham::https::HttpsPlane::new(
            &fake.endpoint,
            ham::sigv4::Signer::new(support::REGION, support::creds()),
        )
        .unwrap();
        let plane = Arc::new(Plane {
            inner,
            reads: Mutex::new(reads.into()),
            put,
            whoami,
            refuse_start: Mutex::new(None),
            gets: Mutex::new(0),
            puts: Mutex::new(Vec::new()),
        });
        let api = Api::new(plane.clone());
        Stage {
            fake,
            plane,
            api,
            home: h,
        }
    }

    fn sharing_on(&self) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.home.config.join("ham/config.toml"))
            .unwrap();
        writeln!(f, "\n[run]\nprovider_sharing = true").unwrap();
    }

    fn env(&self) -> impl Fn(&str) -> Option<String> + Sync + use<> {
        let (config, home) = (
            self.home.config.display().to_string(),
            self.home.root.display().to_string(),
        );
        move |k: &str| match k {
            "XDG_CONFIG_HOME" => Some(config.clone()),
            "HOME" => Some(home.clone()),
            _ => None,
        }
    }

    async fn submit(&self, args: &DirectArgs) -> Msg {
        submit_msg_with_env(&self.api, args, "b814-nonce", None, &self.env()).await
    }

    fn starts(&self) -> Vec<Value> {
        self.fake
            .mem
            .posts(STARTS)
            .iter()
            .map(|p| serde_json::from_slice(&p.body).unwrap())
            .collect()
    }

    fn puts(&self) -> Vec<String> {
        self.plane.puts.lock().unwrap().clone()
    }

    fn gets(&self) -> usize {
        *self.plane.gets.lock().unwrap()
    }
}

fn args(model: &str, extra: &[&str]) -> DirectArgs {
    let mut words: Vec<String> = ["--prompt", "build x", "--s3-only", "--no-rules"]
        .iter()
        .map(ToString::to_string)
        .collect();
    words.extend(extra.iter().map(ToString::to_string));
    words.extend(["--direct", "--model", model].map(ToString::to_string));
    match parse(&words).unwrap().command {
        Command::StartDirect(a) => a,
        other => panic!("not a start: {other:?}"),
    }
}

fn agreed(model: &str) -> DirectArgs {
    let mut a = args(model, &[]);
    a.provider_sharing = Some(true);
    a
}

fn marker(case: &str, s: &Stage) {
    println!(
        "B814 TRIGGER PRESENT: case={case} reads={} switches={:?} starts={}",
        s.gets(),
        s.puts(),
        s.starts().len()
    );
}

// ---- the composer asks when the setting is off ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_a_pick_with_the_setting_off_asks_and_names_the_model_the_meaning_the_account_and_the_region()
 {
    let s = Stage::new(
        "b814-ask",
        vec![mode("default")],
        mode("provider_data_share"),
    )
    .await;
    let msg = s.submit(&args(FABLE, &[])).await;
    marker("ask", &s);
    let Msg::SharingAsk(ask) = msg else {
        panic!("a pick of a model that needs sharing asks first: {msg:?}")
    };
    for want in [
        "Claude Fable 5.2",
        FABLE,
        "model's provider",
        "30 days",
        account(),
        support::REGION,
        "from default to provider_data_share",
    ] {
        assert!(
            ask.question.contains(want),
            "{want:?} missing: {}",
            ask.question
        );
    }
    assert_eq!(ask.nonce, "b814-nonce");
    assert_eq!(ask.args.model.as_deref(), Some(FABLE));
    assert!(s.puts().is_empty(), "nothing is switched before the answer");
    assert!(
        s.starts().is_empty(),
        "nothing is started before the answer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_an_explicit_off_is_refused_as_before_and_asks_nothing() {
    let s = Stage::new(
        "b814-off",
        vec![mode("default")],
        mode("provider_data_share"),
    )
    .await;
    let msg = s.submit(&args(FABLE, &["--provider-sharing", "off"])).await;
    let Msg::Started(Err(e)) = msg else {
        panic!("--provider-sharing off is the user's refusal: {msg:?}")
    };
    assert!(e.contains("needs provider data sharing"), "{e}");
    assert_eq!((s.gets(), s.puts().len(), s.starts().len()), (0, 0, 0));
}

// ---- the agreement switches the mode, reads it back and starts ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_the_answer_y_switches_the_mode_reads_it_back_and_starts_with_one_line() {
    let s = Stage::new(
        "b814-yes",
        vec![mode("default"), mode("provider_data_share")],
        mode("provider_data_share"),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    marker("approved", &s);
    let Msg::StartedWith { response, note } = msg else {
        panic!("an agreed switch starts the run and says so: {msg:?}")
    };
    assert!(!response.run_id.is_empty());
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
    assert_eq!(s.gets(), 2, "one read before the switch and one read back");
    for want in [
        account(),
        support::REGION,
        "from default",
        "provider_data_share",
        "you agreed for this run",
        "Claude Fable 5.2",
    ] {
        assert!(note.contains(want), "{want:?} missing: {note}");
    }
    assert!(!note.contains('\n'), "one line: {note}");
    let starts = s.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["providerSharing"], Value::Bool(true));
    assert_eq!(starts[0]["model"], Value::String(FABLE.into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_the_setting_on_in_the_config_switches_without_a_question_and_says_so_in_one_line() {
    let s = Stage::new(
        "b814-config",
        vec![mode("inherit"), mode("provider_data_share")],
        mode("provider_data_share"),
    )
    .await;
    s.sharing_on();
    let msg = s.submit(&args(FABLE, &[])).await;
    marker("config", &s);
    let Msg::StartedWith { note, .. } = msg else {
        panic!("the setting is the agreement: no question: {msg:?}")
    };
    assert!(note.contains("run.provider_sharing is on"), "{note}");
    assert!(note.contains("from inherit"), "{note}");
    assert!(!note.contains('\n'), "{note}");
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
    assert_eq!(s.starts().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_a_mode_that_already_lets_the_model_run_is_not_touched() {
    for m in ["provider_data_share", "aws_review"] {
        let s = Stage::new(&format!("b814-keep-{m}"), vec![mode(m)], mode("default")).await;
        let msg = s.submit(&agreed(FABLE)).await;
        assert!(matches!(msg, Msg::Started(Ok(_))), "{m}: {msg:?}");
        assert!(s.puts().is_empty(), "{m}: never switched");
        assert_eq!(s.starts().len(), 1, "{m}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_a_model_that_needs_nothing_reads_no_mode() {
    let s = Stage::new(
        "b814-sonnet",
        vec![mode("default")],
        mode("provider_data_share"),
    )
    .await;
    s.sharing_on();
    let msg = s.submit(&args(SONNET, &[])).await;
    assert!(matches!(msg, Msg::Started(Ok(_))), "{msg:?}");
    assert_eq!((s.gets(), s.puts().len(), s.starts().len()), (0, 0, 1));
}

// ---- what ham cannot read it leaves alone ----------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_a_read_that_is_denied_changes_nothing_and_the_run_goes_as_before_with_one_line() {
    let s = Stage::new(
        "b814-readdenied",
        vec![answer(403, "{}")],
        mode("provider_data_share"),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::StartedWith { note, .. } = msg else {
        panic!("{msg:?}")
    };
    assert!(note.contains("did not check or change"), "{note}");
    assert!(note.contains("bedrock:GetAccountDataRetention"), "{note}");
    assert!(s.puts().is_empty());
    assert_eq!(s.starts().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_credentials_of_another_account_switch_nothing() {
    let whoami = format!(
        r#"{{"principalArn":"arn:aws:sts::444455556666:assumed-role/Dev/x","account":"{}"}}"#,
        account()
    );
    let s = Stage::with_whoami(
        "b814-other",
        vec![mode("default")],
        mode("provider_data_share"),
        Some(whoami),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::StartedWith { note, .. } = msg else {
        panic!("{msg:?}")
    };
    assert!(
        note.contains("444455556666") && note.contains(account()),
        "{note}"
    );
    assert!(
        s.puts().is_empty(),
        "the stage's account is not the credentials' account"
    );
    assert_eq!(s.starts().len(), 1);
}

// ---- a switch that fails stops the start --------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_a_denied_switch_stops_the_start_before_any_model_call() {
    let s = Stage::new(
        "b814-putdenied",
        vec![mode("default")],
        answer(
            403,
            r#"{"message":"User is not authorized to perform: bedrock:PutAccountDataRetention"}"#,
        ),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    marker("denied", &s);
    let Msg::Started(Err(e)) = msg else {
        panic!("{msg:?}")
    };
    assert!(e.contains("bedrock:PutAccountDataRetention"), "{e}");
    assert!(e.contains("No run was started"), "{e}");
    assert!(!e.contains('\n'), "one plain message: {e}");
    assert!(s.starts().is_empty(), "no start, so no model call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_a_switch_whose_read_back_does_not_let_the_model_run_stops_the_start() {
    let s = Stage::new(
        "b814-stale",
        vec![mode("default")],
        mode("provider_data_share"),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::Started(Err(e)) = msg else {
        panic!("{msg:?}")
    };
    assert!(e.contains("reads default"), "{e}");
    assert_eq!(s.puts().len(), 1);
    assert!(s.starts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b814_the_follow_up_stops_on_a_denied_switch_as_well() {
    let s = Stage::new("b814-followup", vec![mode("default")], answer(403, "{}")).await;
    s.sharing_on();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(s.home.config.join("ham/config.toml"))
            .unwrap();
        writeln!(f, "rules = false").unwrap();
    }
    {
        use std::io::Write;
        let path = s.home.config.join("ham/config.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "defaultModel = \"{FABLE}\"\n{text}").unwrap();
    }
    let done = perform_with_env(
        &s.api,
        Job::StartFollowUp {
            run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
            text: "now the docs".into(),
            head: None,
            engine: None,
        },
        &s.env(),
    )
    .await;
    let Done::FollowUpStarted { result: Err(e), .. } = done else {
        panic!("{done:?}")
    };
    assert!(e.contains("bedrock:PutAccountDataRetention"), "{e}");
    assert!(s.starts().is_empty());
}

// ---- the screen ---------------------------------------------------------------------------------------------------------------------------

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn screen_text(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn composer() -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.screen(), Screen::Composer);
    app
}

const QUESTION: &str = "Claude Fable 5.2 (global.anthropic.claude-fable-5-2) needs provider data sharing: the prompts and answers of this run go to the model's provider. ham switches account 111122223333 in us-east-1 from default to provider_data_share.";

fn asked(app: &mut App) {
    app.update(Msg::SharingAsk(Box::new(SharingAsk {
        args: args(FABLE, &[]),
        nonce: "n-0".into(),
        backend: None,
        model: "Claude Fable 5.2".into(),
        id: FABLE.into(),
        question: QUESTION.into(),
        shown: shown_switch(),
    })));
}

/// What [`QUESTION`] shows: a switch of account 111122223333 in us-east-1 from `default`.
fn shown_switch() -> ham_core::sharing::Shown {
    ham_core::sharing::Shown {
        region: "us-east-1".into(),
        account: Some("111122223333".into()),
        plan: ham_core::sharing::Plan::Switch {
            account: "111122223333".into(),
            from: "default".into(),
        },
    }
}

#[test]
fn b814_the_chosen_row_that_needs_sharing_shows_what_happens_on_submit() {
    let mut app = composer();
    for _ in 0..5 {
        press(&mut app, KeyCode::Tab);
    }
    press(&mut app, KeyCode::End);
    assert_eq!(app.chosen_model(), Some(FABLE));
    let text = flat(&screen_text(&app, 120, 40));
    assert!(text.contains("asks before"), "{text}");
}

#[test]
fn b814_the_question_is_drawn_with_its_keys_at_80x24_and_120x40() {
    for (w, h) in [(80, 24), (120, 40)] {
        let mut app = composer();
        asked(&mut app);
        let text = flat(&screen_text(&app, w, h));
        for want in [
            "111122223333",
            "us-east-1",
            "provider_data_share",
            "y ",
            "Esc",
        ] {
            assert!(text.contains(want), "{w}x{h}: {want:?} missing: {text}");
        }
    }
}

#[test]
fn b814_y_starts_the_same_run_with_the_agreement() {
    let mut app = composer();
    asked(&mut app);
    let fx = press(&mut app, KeyCode::Char('y'));
    let [
        Effect::SubmitAgreed {
            args,
            nonce,
            backend,
            shown,
        },
    ] = fx.as_slice()
    else {
        panic!("{fx:?}")
    };
    assert_eq!(nonce, "n-0", "the same start, so the same key");
    assert_eq!(args.provider_sharing, Some(true));
    assert_eq!(args.model.as_deref(), Some(FABLE));
    assert_eq!(backend, &None);
    assert_eq!(
        shown,
        &shown_switch(),
        "the answer carries what the question showed"
    );
}

/// With several backends the question holds the backend its start was aimed at, and `y` sends the start to that backend.
#[test]
fn fix515_y_sends_the_start_to_the_backend_the_question_was_asked_for() {
    let mut app = composer();
    app.update(Msg::SharingAsk(Box::new(SharingAsk {
        args: args(FABLE, &[]),
        nonce: "n-0".into(),
        backend: Some("lab".into()),
        model: "Claude Fable 5.2".into(),
        id: FABLE.into(),
        question: QUESTION.into(),
        shown: shown_switch(),
    })));
    let fx = press(&mut app, KeyCode::Char('Y'));
    let [Effect::SubmitAgreed { backend, .. }] = fx.as_slice() else {
        panic!("{fx:?}")
    };
    assert_eq!(backend.as_deref(), Some("lab"));
}

#[test]
fn b814_n_or_esc_starts_nothing_and_says_so_in_one_plain_line() {
    for key in [KeyCode::Char('n'), KeyCode::Esc] {
        let mut app = composer();
        asked(&mut app);
        assert!(press(&mut app, key).is_empty(), "{key:?}");
        assert_eq!(app.screen(), Screen::Composer, "{key:?}");
        let text = flat(&screen_text(&app, 120, 40));
        assert!(text.contains("Not started"), "{key:?}: {text}");
        assert!(
            press(&mut app, KeyCode::Char('y')).is_empty(),
            "the question is gone"
        );
    }
}

#[test]
fn b814_other_keys_keep_the_question() {
    let mut app = composer();
    asked(&mut app);
    assert!(press(&mut app, KeyCode::Char('x')).is_empty());
    assert!(press(&mut app, KeyCode::Tab).is_empty());
    let fx = press(&mut app, KeyCode::Char('y'));
    assert!(
        matches!(fx.as_slice(), [Effect::SubmitAgreed { .. }]),
        "{fx:?}"
    );
}

#[test]
fn b814_the_switch_line_is_shown_on_the_run_screen() {
    let mut app = composer();
    app.update(Msg::StartedWith {
        response: StartRunResponse {
            run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
            status: harness_protocol::goal::GoalStatus::Active,
        },
        note: "ham switched the Bedrock data retention mode of AWS account 111122223333".into(),
    });
    assert_eq!(app.screen(), Screen::Dashboard);
    let text = flat(&screen_text(&app, 160, 40));
    assert!(
        text.contains("ham switched the Bedrock data retention mode"),
        "{text}"
    );
}

/// The whole line of the switch stays readable on the run screen: the notice row is one row, so the live proof of 2026-11-11 saw the run screen
/// at 160 columns cut the line before the command that sets the mode back. The run screen keeps the line as a card of the run, wrapped to the
/// width, at 160x40 and at 80x24, also after another notice took the notice row.
#[test]
fn b814_the_whole_switch_line_stays_on_the_run_screen_as_a_card() {
    let note = ham_core::sharing::switched_line(
        "Claude Fable 5.2",
        "111122223333",
        "us-east-1",
        "inherit",
        "provider_data_share",
        ham_core::sharing::Agreement::ThisRun,
    );
    for (w, h) in [(160, 40), (80, 24)] {
        let mut app = composer();
        app.update(Msg::StartedWith {
            response: StartRunResponse {
                run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
                status: harness_protocol::goal::GoalStatus::Active,
            },
            note: note.clone(),
        });
        assert_eq!(app.screen(), Screen::Dashboard);
        app.say("another notice");
        let text = flat(&screen_text(&app, w, h));
        assert!(text.contains(&flat(&note)), "{w}x{h}: {text}");
    }
}

/// A follow-up that started with a line of provider data sharing keeps the line as a card of the new run, whose screen opens; the run it
/// followed shows no such card.
#[test]
fn b814_a_follow_up_that_started_keeps_its_line_as_a_card_of_the_new_run() {
    let note = ham_core::sharing::switched_line(
        "Claude Fable 5.2",
        "111122223333",
        "us-east-1",
        "default",
        "provider_data_share",
        ham_core::sharing::Agreement::Setting,
    );
    let mut app = composer();
    app.update(Msg::Started(Ok(StartRunResponse {
        run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
        status: harness_protocol::goal::GoalStatus::Active,
    })));
    assert_eq!(app.screen(), Screen::Dashboard);
    let before = flat(&screen_text(&app, 160, 40));
    assert!(!before.contains("provider data sharing"), "{before}");
    app.update(Msg::Ext(Done::FollowUpNoted {
        run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
        result: Ok("01M4ZHFBV5SDX7J92E6YRH4QTX".into()),
        note: note.clone(),
    }));
    let text = flat(&screen_text(&app, 160, 40));
    assert!(text.contains("01M4ZHFBV5SDX7J92E6YRH4QTX"), "{text}");
    assert!(text.contains(&flat(&note)), "{text}");
}

// ---- ham doctor agrees with the switch ---------------------------------------------------------------------------------------------------

fn doctor_line(m: &str, on: bool) -> ham::doctor::Check {
    let r = Response {
        status: 200,
        body: format!(r#"{{"mode":"{m}"}}"#).into_bytes(),
        ..Response::default()
    };
    ham::doctor::sharing_check_of("us-east-1", on, Ok(&r))
}

#[test]
fn b814_doctor_counts_aws_review_as_a_mode_the_model_runs_in() {
    let c = doctor_line("aws_review", true);
    assert_eq!((c.pass, c.warn), (true, false), "{}", c.detail);
}

#[test]
fn b814_doctor_says_ham_switches_a_mode_that_does_not_share_when_a_run_starts() {
    let c = doctor_line("default", true);
    assert!(c.warn, "{}", c.detail);
    assert!(
        c.detail.contains("ham switches it")
            && c.detail.contains("bedrock:PutAccountDataRetention"),
        "{}",
        c.detail
    );
}

// ---- what the person agreed to on screen is what happens ---------------------------------------------------------------------------------

/// The answer of the start the composer sends for `key`: the effect goes to the same runner the event loop uses, with the environment of the
/// stage's config folder.
async fn press_and_run(s: &Stage, app: &mut App, key: KeyCode) -> Option<Msg> {
    let mut last = None;
    for effect in press(app, key) {
        last = ham::tui::perform_with_env(&s.api, &s.home.downloads, effect, &s.env()).await;
    }
    last
}

/// The question on screen for a pick of Claude Fable 5.2 with the setting off, and the composer that shows it.
async fn asked_live(s: &Stage) -> (App, SharingAsk) {
    let Msg::SharingAsk(ask) = s.submit(&args(FABLE, &[])).await else {
        panic!("a pick of a model that needs sharing asks first")
    };
    println!("FIX515 QUESTION: {}", ask.question);
    let mut app = composer();
    app.update(Msg::SharingAsk(ask.clone()));
    (app, *ask)
}

/// The read behind the question failed, so the question says that ham changes nothing in the account. The read after `y` succeeds and shows
/// `inherit`: ham switches nothing and starts nothing, and asks again with what it now knows. The second `y` agrees to the switch it names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_y_to_a_question_that_said_nothing_changes_switches_nothing_and_asks_again() {
    let failed: Answer = Some(Err(TransportError::Network("timed out".into())));
    let s = Stage::new(
        "fix515-ask-failed",
        vec![
            failed,
            mode("inherit"),
            mode("inherit"),
            mode("provider_data_share"),
        ],
        mode("provider_data_share"),
    )
    .await;
    let (mut app, ask) = asked_live(&s).await;
    assert!(
        ask.question.contains("changes nothing in the account"),
        "{}",
        ask.question
    );
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    println!(
        "FIX515 TRIGGER PRESENT: the question said nothing changes, the read after y shows inherit; reads={} switches={:?} starts={}",
        s.gets(),
        s.puts(),
        s.starts().len()
    );
    assert_eq!(
        s.puts(),
        Vec::<String>::new(),
        "the question said that ham changes nothing in the account"
    );
    assert!(
        s.starts().is_empty(),
        "nothing starts before the person has seen what changed"
    );
    let Some(Msg::SharingAsk(again)) = msg else {
        panic!("ham asks again with the new facts: {msg:?}")
    };
    println!("FIX515 SECOND QUESTION: {}", again.question);
    for want in [
        "ham changed nothing and started nothing",
        account(),
        support::REGION,
        "from inherit to provider_data_share",
    ] {
        assert!(
            again.question.contains(want),
            "{want:?} missing: {}",
            again.question
        );
    }
    app.update(Msg::SharingAsk(again));
    let text = flat(&screen_text(&app, 120, 40));
    assert!(
        text.contains("from inherit to provider_data_share"),
        "{text}"
    );
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    let Some(Msg::StartedWith { note, .. }) = msg else {
        panic!("the second y agrees to the switch on screen: {msg:?}")
    };
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
    assert_eq!(s.starts().len(), 1);
    assert!(
        note.contains("from inherit") && note.contains("you agreed for this run"),
        "{note}"
    );
}

/// The question named a switch from `default`; after `y` the account reads `inherit`. The mode is not the one the person saw, so nothing is
/// switched or started and the question is asked again with the mode the account is in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_y_does_not_switch_from_another_mode_than_the_question_named() {
    let s = Stage::new(
        "fix515-ask-mode",
        vec![mode("default"), mode("inherit")],
        mode("provider_data_share"),
    )
    .await;
    let (mut app, ask) = asked_live(&s).await;
    assert!(
        ask.question.contains("from default to provider_data_share"),
        "{}",
        ask.question
    );
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    assert_eq!(s.puts(), Vec::<String>::new());
    assert!(s.starts().is_empty());
    let Some(Msg::SharingAsk(again)) = msg else {
        panic!("ham asks again with the new facts: {msg:?}")
    };
    assert!(
        again
            .question
            .contains("from inherit to provider_data_share"),
        "{}",
        again.question
    );
}

/// The question named a switch; after `y` the read fails. ham can no longer see the state the person agreed to change, so it switches nothing
/// and starts nothing, and the new question says that it cannot check the mode. `y` to that question starts the run with the line that says
/// ham left the account alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_y_does_not_start_when_the_read_that_succeeded_now_fails() {
    let failed: Answer = Some(Err(TransportError::Network("timed out".into())));
    let s = Stage::new(
        "fix515-ask-fails",
        vec![mode("default"), failed],
        mode("provider_data_share"),
    )
    .await;
    let (mut app, _) = asked_live(&s).await;
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    assert_eq!(s.puts(), Vec::<String>::new());
    assert!(s.starts().is_empty());
    let Some(Msg::SharingAsk(again)) = msg else {
        panic!("ham asks again with the new facts: {msg:?}")
    };
    assert!(
        again.question.contains("cannot check") && again.question.contains("changes nothing"),
        "{}",
        again.question
    );
    app.update(Msg::SharingAsk(again));
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    let Some(Msg::StartedWith { note, .. }) = msg else {
        panic!("y to a question that changes nothing starts the run: {msg:?}")
    };
    assert!(note.contains("did not check or change"), "{note}");
    assert_eq!(s.puts(), Vec::<String>::new(), "nothing was ever switched");
    assert_eq!(s.starts().len(), 1);
}

/// The account is as the question showed it: `y` switches the mode the question named, reads it back and starts the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_y_to_a_question_whose_facts_still_hold_switches_and_starts() {
    let s = Stage::new(
        "fix515-ask-same",
        vec![
            mode("default"),
            mode("default"),
            mode("provider_data_share"),
        ],
        mode("provider_data_share"),
    )
    .await;
    let (mut app, _) = asked_live(&s).await;
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    let Some(Msg::StartedWith { note, .. }) = msg else {
        panic!("the facts hold, so y switches and starts: {msg:?}")
    };
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
    assert_eq!(s.starts().len(), 1);
    assert!(note.contains("from default"), "{note}");
}

// ---- answers of the account that the earlier cases left out ------------------------------------------------------------------------------

/// The switch answers with the new mode and the read back gives nothing: the answer of the switch itself confirms the mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_the_answer_of_the_switch_confirms_the_mode_when_the_read_back_gives_nothing() {
    let s = Stage::new(
        "fix515-noreadback",
        vec![mode("default"), None],
        mode("provider_data_share"),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::StartedWith { note, .. } = msg else {
        panic!("{msg:?}")
    };
    assert!(
        note.contains("from default to provider_data_share"),
        "{note}"
    );
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
    assert_eq!(s.starts().len(), 1);
}

/// The stage's answer to "who am I" cannot be read: ham cannot tell whose account the mode belongs to, so it changes nothing and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_caller_that_cannot_be_told_leaves_the_account_alone() {
    let s = Stage::with_whoami(
        "fix515-whoami",
        vec![mode("default")],
        mode("provider_data_share"),
        Some("no answer a client can read".into()),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::StartedWith { note, .. } = msg else {
        panic!("{msg:?}")
    };
    assert!(
        note.contains("ham could not tell which AWS account your credentials belong to"),
        "{note}"
    );
    assert_eq!(s.puts(), Vec::<String>::new());
    assert_eq!(s.starts().len(), 1);
}

/// A switch that does not answer may still have reached Bedrock: the read back decides. A mode that lets the model run starts the run; a mode
/// that does not stops it, with the words of both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_switch_that_does_not_answer_is_decided_by_the_read_back() {
    let lost: Answer = Some(Err(TransportError::Timeout("PUT /data-retention".into())));
    let s = Stage::new(
        "fix515-lost-ok",
        vec![mode("default"), mode("provider_data_share")],
        lost.clone(),
    )
    .await;
    let msg = s.submit(&agreed(FABLE)).await;
    assert!(matches!(msg, Msg::StartedWith { .. }), "{msg:?}");
    assert_eq!(s.starts().len(), 1);

    let s = Stage::new("fix515-lost-stays", vec![mode("default")], lost).await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::Started(Err(e)) = msg else {
        panic!("{msg:?}")
    };
    assert!(
        e.contains("did not answer") && e.contains("the mode reads default"),
        "{e}"
    );
    assert!(e.ends_with("No run was started."), "{e}");
    assert!(s.starts().is_empty());
}

/// A backend that reads the mode and has no way to switch it: the start stops and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_backend_that_cannot_switch_the_mode_stops_the_start() {
    let s = Stage::new("fix515-noput", vec![mode("default")], None).await;
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::Started(Err(e)) = msg else {
        panic!("{msg:?}")
    };
    assert!(
        e.contains("Bedrock refused the switch")
            && e.contains("this backend cannot change the mode"),
        "{e}"
    );
    assert!(s.starts().is_empty());
}

/// The account was switched and the stage then refuses the start: the error leads with the line of the switch, because the account changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_start_that_fails_after_the_switch_says_the_switch_first() {
    let s = Stage::new(
        "fix515-startfails",
        vec![mode("default"), mode("provider_data_share")],
        mode("provider_data_share"),
    )
    .await;
    *s.plane.refuse_start.lock().unwrap() = Some("the stage takes no run now".into());
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::Started(Err(e)) = msg else {
        panic!("{msg:?}")
    };
    assert!(
        e.starts_with("ham switched the Bedrock data retention mode of AWS account"),
        "{e}"
    );
    assert!(e.contains("the stage takes no run now"), "{e}");
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
}

/// A stage with no AWS account behind it (a stand-in): the question says that ham cannot check the mode and changes nothing, and `y` starts the
/// run with no line about an account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_on_a_stage_with_no_account_y_starts_the_run_without_a_line() {
    let s = Stage::new("fix515-noaccount", vec![None], None).await;
    let (mut app, ask) = asked_live(&s).await;
    assert!(
        ask.question.contains("cannot check") && ask.question.contains("changes nothing"),
        "{}",
        ask.question
    );
    let msg = press_and_run(&s, &mut app, KeyCode::Char('y')).await;
    assert!(matches!(msg, Some(Msg::Started(Ok(_)))), "{msg:?}");
    assert_eq!(s.puts(), Vec::<String>::new());
    assert_eq!(s.starts().len(), 1);
    assert_eq!(s.starts()[0]["providerSharing"], Value::Bool(true));
}

/// The composer's start as a result: the answer of a start that came with a line, and the refusal in place of the question.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_the_start_as_a_result_carries_the_run_or_the_refusal() {
    let s = Stage::new(
        "fix515-result",
        vec![mode("default"), mode("provider_data_share")],
        mode("provider_data_share"),
    )
    .await;
    let started =
        ham::tui::submit_direct_with_env(&s.api, &agreed(FABLE), "fix515-result", &s.env())
            .await
            .unwrap();
    assert!(!started.run_id.is_empty());
    assert_eq!(s.puts(), vec!["provider_data_share".to_string()]);
    let refused =
        ham::tui::submit_direct_with_env(&s.api, &args(FABLE, &[]), "fix515-result-2", &s.env())
            .await
            .unwrap_err();
    assert!(
        refused.contains("needs provider data sharing, and the setting is off"),
        "{refused}"
    );
    assert_eq!(s.starts().len(), 1, "the refused start sent nothing");
}

/// A config file that cannot be read stops the start before any account is looked at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_config_that_cannot_be_read_stops_the_start_before_any_read() {
    let s = Stage::new(
        "fix515-badconfig",
        vec![mode("default")],
        mode("provider_data_share"),
    )
    .await;
    std::fs::write(
        s.home.config.join("ham/config.toml"),
        "this is = = no toml\n",
    )
    .unwrap();
    let msg = s.submit(&agreed(FABLE)).await;
    let Msg::Started(Err(e)) = msg else {
        panic!("{msg:?}")
    };
    assert!(e.contains("config.toml"), "{e}");
    assert_eq!((s.gets(), s.puts().len(), s.starts().len()), (0, 0, 0));
}

/// A follow-up whose start failed after the account was switched: the screen says the switch first, then why the run did not start.
#[test]
fn fix515_a_follow_up_that_failed_after_the_switch_shows_the_switch_first() {
    let note = ham_core::sharing::switched_line(
        "Claude Fable 5.2",
        "111122223333",
        "us-east-1",
        "default",
        "provider_data_share",
        ham_core::sharing::Agreement::Setting,
    );
    let mut app = composer();
    app.update(Msg::Started(Ok(StartRunResponse {
        run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
        status: harness_protocol::goal::GoalStatus::Active,
    })));
    app.update(Msg::Ext(Done::FollowUpNoted {
        run_id: "01M4ZHFBV5SDX7J92E6YRH4QTW".into(),
        result: Err("the stage takes no run now".into()),
        note: note.clone(),
    }));
    // The notice is one row: on a wide screen it holds the line of the switch and, after it, why the run did not start.
    let text = flat(&screen_text(&app, 600, 40));
    let switch = text
        .find("ham switched the Bedrock data retention mode")
        .unwrap_or_else(|| panic!("{text}"));
    let why = text
        .find("the follow-up run did not start: the stage takes no run now")
        .unwrap_or_else(|| panic!("{text}"));
    assert!(switch < why, "{text}");
}

/// A retry and a resume that started after ham looked at the account: each new run keeps its line as its card, and the notice shows the line.
#[test]
fn fix515_a_retry_and_a_resume_that_switched_keep_the_line_as_a_card_of_the_new_run() {
    let note = ham_core::sharing::switched_line(
        "Claude Fable 5.2",
        "111122223333",
        "us-east-1",
        "inherit",
        "provider_data_share",
        ham_core::sharing::Agreement::Setting,
    );
    const BLOCKED: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
    const NEW: &str = "01M4ZHFBV5SDX7J92E6YRH4QTX";

    // The retry: the blocked run is on screen, the new run opens with the card.
    let mut app = composer();
    app.update(Msg::Started(Ok(StartRunResponse {
        run_id: BLOCKED.into(),
        status: harness_protocol::goal::GoalStatus::Active,
    })));
    app.update(Msg::Ext(Done::RetryNoted {
        run_id: BLOCKED.into(),
        result: Ok(NEW.into()),
        note: note.clone(),
    }));
    let text = flat(&screen_text(&app, 160, 40));
    assert!(text.contains(NEW), "{text}");
    assert!(text.contains(&flat(&note)), "{text}");

    // The resume: the Runs screen says the switch first, then what each run did; the new run shows the card when it is opened.
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    assert_eq!(app.screen(), Screen::Runs);
    let fx = app.update(Msg::Ext(Done::ResumedNoted {
        outcomes: vec![ham_core::resume::Outcome {
            run_id: BLOCKED.into(),
            result: Ok(NEW.into()),
        }],
        notes: vec![(NEW.into(), note.clone())],
    }));
    assert!(
        fx.iter().any(|e| matches!(e, Effect::LoadRuns)),
        "the list is read again: {fx:?}"
    );
    let text = flat(&screen_text(&app, 600, 40));
    let switch = text
        .find("ham switched the Bedrock data retention mode")
        .unwrap_or_else(|| panic!("{text}"));
    let resumed = text
        .find("resumed 1 of 1")
        .unwrap_or_else(|| panic!("{text}"));
    assert!(switch < resumed, "{text}");
    app.open_run(NEW);
    let text = flat(&screen_text(&app, 160, 40));
    assert!(text.contains(&flat(&note)), "{text}");
}

/// A stage reached over plain HTTP on this machine is a stand-in with no AWS account behind it: the transport answers neither the read nor
/// the switch, so no call leaves the machine, and a run that agreed starts with no line about an account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix515_a_stand_in_stage_on_this_machine_is_never_read_or_switched() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "fix515-standin");
    let plane = ham::https::HttpsPlane::new(
        &fake.endpoint,
        ham::sigv4::Signer::new(support::REGION, support::creds()),
    )
    .unwrap();
    assert!(plane.data_retention(None).await.is_none());
    assert!(
        plane
            .put_data_retention(None, "provider_data_share")
            .await
            .is_none()
    );
    let api = Api::new(Arc::new(plane));
    let (config, root) = (h.config.display().to_string(), h.root.display().to_string());
    let env = move |k: &str| match k {
        "XDG_CONFIG_HOME" => Some(config.clone()),
        "HOME" => Some(root.clone()),
        _ => None,
    };
    let msg = submit_msg_with_env(&api, &agreed(FABLE), "fix515-standin", None, &env).await;
    assert!(matches!(msg, Msg::Started(Ok(_))), "{msg:?}");
    assert_eq!(fake.mem.posts(STARTS).len(), 1);
}
