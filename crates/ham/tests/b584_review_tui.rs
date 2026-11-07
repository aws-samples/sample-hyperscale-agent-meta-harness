//! After the independent review, in the terminal app without a terminal: the refusal of a run marked as customer content is made in the
//! composer for a client with one backend too, and for a client with several backends before the first read of the list has arrived.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::backends::{Backends, BoxBuild, BuildFailure};
use ham::http::{ControlPlane, Response, TransportError};
use ham::tui::{App, DirectSettings, Effect, attach_backends};
use ham_core::backends::BackendDef;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::sync::Arc;

struct Quiet;

impl ControlPlane for Quiet {
    async fn call(
        &self,
        _method: &str,
        _path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        Ok(Response {
            status: 404,
            ..Response::default()
        })
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        Ok(Response::default())
    }
}

fn def(alias: &str, safe: bool) -> BackendDef {
    BackendDef {
        alias: alias.into(),
        api_endpoint: Some(format!("https://{alias}.example/direct")),
        region: None,
        aws_profile: Some(alias.into()),
        customer_content: safe,
    }
}

async fn plane(defs: Vec<BackendDef>, default: Option<&str>) -> Arc<Backends<Quiet>> {
    let factory = |_d: BackendDef| -> BoxBuild<Quiet> {
        Box::pin(async { Ok::<Quiet, BuildFailure>(Quiet) })
    };
    Arc::new(
        Backends::connect(defs, default.map(str::to_string), factory)
            .await
            .unwrap(),
    )
}

fn new_app() -> App {
    App::new_direct(
        "nonce-1".into(),
        None,
        DirectSettings {
            no_rules: true,
            ..DirectSettings::default()
        },
    )
}

fn press(app: &mut App, c: KeyCode) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn ctrl_s(app: &mut App) -> Vec<Effect> {
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
}

fn type_text(app: &mut App, s: &str) {
    for ch in s.chars() {
        press(app, KeyCode::Char(ch));
    }
}

fn screen(app: &App) -> String {
    let (w, h) = (150u16, 50u16);
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
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Opens the composer, types a prompt, picks the zip target and puts `options` in the Options box.
fn compose(app: &mut App, options: &str) {
    press(app, KeyCode::Char('n'));
    type_text(app, "add a test");
    press(app, KeyCode::Tab);
    press(app, KeyCode::Right);
    press(app, KeyCode::Right);
    for _ in 0..3 {
        press(app, KeyCode::Tab);
    }
    type_text(app, options);
}

#[tokio::test]
async fn b584_a_one_backend_client_refuses_the_customer_content_word_unless_its_backend_is_declared_safe()
 {
    let one = plane(vec![def("default", false)], None).await;
    let one_safe = plane(vec![def("default", true)], None).await;
    // Not declared safe: the composer refuses on the form and sends nothing.
    let mut app = new_app();
    attach_backends(&mut app, &one, None);
    compose(&mut app, "--customer-content");
    let effects = ctrl_s(&mut app);
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(
        text.contains("not declared safe for customer content") && text.contains("customerContent"),
        "{text}"
    );
    // Declared safe: the run starts as it always did, with no backend named.
    let mut app = new_app();
    attach_backends(&mut app, &one_safe, None);
    compose(&mut app, "--customer-content");
    let effects = ctrl_s(&mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SubmitDirect { .. }]),
        "{effects:?}"
    );
    // Without the word nothing changes for a one-backend client.
    let mut app = new_app();
    attach_backends(&mut app, &one, None);
    compose(&mut app, "");
    let effects = ctrl_s(&mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SubmitDirect { .. }]),
        "{effects:?}"
    );
}

#[tokio::test]
async fn b584_the_composer_of_a_client_with_several_backends_knows_them_before_the_first_read_of_the_list()
 {
    let two = plane(vec![def("work", true), def("spare", false)], Some("spare")).await;
    // The default backend is `spare`, which is not declared safe; no list has been read, so no `Msg::Backends` has arrived.
    let mut app = new_app();
    attach_backends(&mut app, &two, Some("spare".into()));
    compose(&mut app, "--customer-content");
    let effects = ctrl_s(&mut app);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        screen(&app).contains("not declared safe for customer content"),
        "{}",
        screen(&app)
    );
    // The backend declared safe takes the run, and the effect names it.
    let mut app = new_app();
    attach_backends(&mut app, &two, Some("work".into()));
    compose(&mut app, "--customer-content");
    let effects = ctrl_s(&mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SubmitDirectOn { backend, .. }] if backend == "work"),
        "{effects:?}"
    );
    // A start without a pick is refused for want of a backend, whatever the options hold.
    let mut app = new_app();
    attach_backends(&mut app, &two, None);
    compose(&mut app, "");
    assert!(ctrl_s(&mut app).is_empty());
    assert!(screen(&app).contains("choose the backend for this run"));
}
