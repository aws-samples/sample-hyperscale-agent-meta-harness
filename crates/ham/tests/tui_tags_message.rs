//! `/tag` in the message box of a run: `+kind:value` or a bare word adds a tag, `-kind:value` removes one, a bare word without
//! a kind is a topic. The word changes the run on the server, so the box shows a preview that names both lists and waits for a second Enter
//! (the class the registry gives the word); the second Enter sends one edit for the run on screen. A word outside the grammar shows the rule and
//! sends nothing. The answer updates the tags of the run on screen and says what the run holds now.
#![allow(clippy::unwrap_used)]

mod chatfx;
mod tagsfx;

use chatfx::{Clock, RUN, base_events, enter, press, ready, send, type_text};
use crossterm::event::KeyCode;
use ham::tui::{App, Effect, Msg, TagsOrigin};
use ham_core::registry::{self, Availability, Effects, Kind, RunArg, TextArg};
use ham_core::runtag::wire::{TagEdit, TagEditRequest, TagEditResponse, TagEditResult};
use harness_protocol::api::v1::RunSummary;
use tagsfx::{named, s3_only};

const NAME: &str = "Monthly CSV export with manifest";

/// The run on screen in typing mode, with the Runs list's row for it: a model's name and two tags.
fn on_screen() -> App {
    let mut app = ready(base_events());
    app.set_now(tagsfx::unix(tagsfx::NOW));
    let runs: Vec<RunSummary> = vec![s3_only(RUN, "2026-10-20T12:00:00Z")];
    tagsfx::read_all(
        &mut app,
        runs,
        vec![named(
            RUN,
            NAME,
            Some(&["project:billing-export", "tech:s3"]),
        )],
    );
    app
}

fn screen(app: &App) -> String {
    chatfx::screen(app, 120, 40)
}

fn edit(add: &[&str], remove: &[&str]) -> TagEdit {
    TagEdit {
        run_id: RUN.into(),
        add: add.iter().map(|t| (*t).to_string()).collect(),
        remove: remove.iter().map(|t| (*t).to_string()).collect(),
    }
}

fn answer(origin: TagsOrigin, result: TagEditResult) -> Msg {
    Msg::TagsEdited {
        origin,
        results: vec![Ok(TagEditResponse {
            results: vec![result],
        })],
    }
}

fn word() -> TagsOrigin {
    TagsOrigin::Word {
        run_id: RUN.to_string(),
    }
}

#[test]
fn tag_word_previews_then_edits() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    // The line shows a preview that names both lists and waits; nothing is sent.
    let effects = send(&mut app, &mut clock, "/tag +project:x -tech:s3 topic:z");
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("Add: project:x topic:z"), "{text}");
    assert!(text.contains("Remove: tech:s3"), "{text}");
    assert!(
        text.contains(&format!("Change the tags of run {RUN}?")),
        "the preview names the run: {text}"
    );
    // A second Enter inside 300 ms is the same keystroke (a burst or a repeat): it sends nothing and the preview stays.
    assert!(enter(&mut app, &mut clock, 100).is_empty());
    assert!(screen(&app).contains("Add: project:x topic:z"));
    // The second Enter after the delay sends one edit for the run on screen, with the normalised tags, bare word as a topic.
    let effects = enter(&mut app, &mut clock, 400);
    let [Effect::EditTags { origin, batches }] = effects.as_slice() else {
        panic!("one edit effect: {effects:?}");
    };
    assert_eq!(origin, &word());
    assert_eq!(
        batches,
        &vec![TagEditRequest {
            edits: vec![edit(&["project:x", "topic:z"], &["tech:s3"])]
        }]
    );
    // The line was sent: the box is empty and the preview is gone.
    let text = screen(&app);
    assert!(!text.contains("Add: project:x"), "{text}");
    assert!(!text.contains("/tag +project:x"), "{text}");
}

#[test]
fn the_answer_updates_the_name_row_and_the_note_says_what_the_run_holds_now() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/tag +project:x");
    enter(&mut app, &mut clock, 400);
    // The row under the header shows the tags the held row has: the name, two spaces, the tags (dim).
    let before = screen(&app);
    let row = before.lines().nth(1).unwrap();
    assert_eq!(
        row,
        format!("{NAME}  project:billing-export tech:s3"),
        "{before}"
    );
    app.update(answer(
        word(),
        TagEditResult::ok(RUN, vec!["project:x".into(), "tech:s3".into()]),
    ));
    let after = screen(&app);
    assert_eq!(
        after.lines().nth(1).unwrap(),
        format!("{NAME}  project:x tech:s3"),
        "{after}"
    );
    // A run with no name draws no tags row, so the note always prints the effective list.
    assert!(after.contains("tags now: project:x tech:s3"), "{after}");
}

#[test]
fn an_unchanged_edit_says_so_and_an_invalid_one_shows_the_servers_rule() {
    let mut app = on_screen();
    app.update(answer(
        word(),
        TagEditResult::unchanged(RUN, vec!["project:billing-export".into(), "tech:s3".into()]),
    ));
    let text = screen(&app);
    assert!(text.contains("no change"), "{text}");
    assert!(
        text.contains("project:billing-export tech:s3"),
        "the tags are still the old ones: {text}"
    );
    let mut app = on_screen();
    app.update(answer(
        word(),
        TagEditResult::invalid(RUN, "a run holds at most 12 tags of its own"),
    ));
    let text = screen(&app);
    assert!(
        text.contains("a run holds at most 12 tags of its own"),
        "{text}"
    );
    assert_eq!(
        text.lines().nth(1).unwrap(),
        format!("{NAME}  project:billing-export tech:s3"),
        "{text}"
    );
    // The statuses a person may meet besides: not found and a lost race.
    let mut app = on_screen();
    app.update(answer(word(), TagEditResult::not_found(RUN)));
    assert!(screen(&app).contains("not found"), "{}", screen(&app));
    let mut app = on_screen();
    app.update(answer(word(), TagEditResult::conflict(RUN)));
    assert!(screen(&app).contains("try again"), "{}", screen(&app));
}

#[test]
fn a_stage_without_the_route_says_so() {
    let mut app = on_screen();
    app.update(Msg::TagsEdited {
        origin: word(),
        results: vec![Err("this service cannot edit tags yet".into())],
    });
    let text = screen(&app);
    assert!(text.contains("this service cannot edit tags yet"), "{text}");
    assert_eq!(
        text.lines().nth(1).unwrap(),
        format!("{NAME}  project:billing-export tech:s3")
    );
}

#[test]
fn an_invalid_word_shows_the_rule_and_sends_nothing() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/tag +foo:bar");
    assert!(effects.is_empty(), "{effects:?}");
    let rule = ham_core::runtag::edit::parse_tag_words("+foo:bar")
        .unwrap_err()
        .message();
    let text = screen(&app);
    assert!(
        text.contains(rule),
        "the panel gives the rule {rule:?}:\n{text}"
    );
    assert!(text.contains("Nothing was sent"), "{text}");
    // The text stays in the box; a second Enter does not send it.
    assert!(text.contains("/tag +foo:bar"), "{text}");
    assert!(enter(&mut app, &mut clock, 400).is_empty());
    // A tag that is both added and removed is refused at the preview, not left to the server.
    let mut app = on_screen();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/tag +topic:a -topic:a");
    let text = screen(&app);
    assert!(text.contains("Nothing was sent"), "{text}");
    assert!(!text.contains("Add:"), "{text}");
    assert!(enter(&mut app, &mut clock, 400).is_empty());
}

#[test]
fn tag_with_no_argument_shows_the_usage() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    let effects = send(&mut app, &mut clock, "/tag");
    assert!(effects.is_empty(), "{effects:?}");
    let text = screen(&app);
    assert!(text.contains("+kind:value"), "{text}");
    assert!(text.contains("-kind:value"), "{text}");
    assert!(enter(&mut app, &mut clock, 400).is_empty());
}

#[test]
fn esc_and_any_other_key_cancel_the_preview_and_a_changed_line_is_previewed_again() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/tag +topic:a");
    assert!(screen(&app).contains("Add: topic:a"));
    assert!(press(&mut app, &mut clock, KeyCode::Esc, 400).is_empty());
    assert!(!screen(&app).contains("Add: topic:a"));
    // The text is still in the box: Enter previews it again, and still sends nothing.
    assert!(enter(&mut app, &mut clock, 400).is_empty());
    assert!(screen(&app).contains("Add: topic:a"));
    // Typing a letter withdraws the preview; the letter goes to the box, and Enter previews the changed line from the start.
    let mut app = on_screen();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/tag +topic:a");
    type_text(&mut app, &mut clock, "b");
    assert!(!screen(&app).contains("Add: topic:a"), "{}", screen(&app));
    let effects = enter(&mut app, &mut clock, 400);
    assert!(
        effects.is_empty(),
        "the changed line is previewed, not sent: {effects:?}"
    );
    assert!(screen(&app).contains("Add: topic:ab"), "{}", screen(&app));
}

#[test]
fn a_preview_does_not_outlive_the_run_it_was_made_for() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/tag +topic:a");
    assert!(screen(&app).contains("Add: topic:a"));
    // Another run comes on screen (a link, the open-anything box): its box must not send the old preview with its Enter.
    let _ = app.open_run("01K6A0000000000000000000R2");
    let effects = enter(&mut app, &mut clock, 400);
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::EditTags { .. })),
        "{effects:?}"
    );
    assert!(!screen(&app).contains("Add: topic:a"), "{}", screen(&app));
}

#[test]
fn tag_is_a_registered_remote_mutation_word() {
    let e = registry::find("tag").expect("the registry holds /tag");
    assert_eq!(e.kind, Kind::TuiOnly);
    assert_eq!(e.status, Availability::Built);
    assert_eq!(e.run_arg, RunArg::None);
    assert_eq!(e.text, TextArg::Required);
    assert!(e.effects.contains(Effects::REMOTE_MUTATION));
    assert!(
        e.class().needs_confirm(),
        "a word that writes the server asks for a second Enter"
    );
    assert_eq!(e.usage, "<+kind:value | -kind:value | word>...");
    assert_eq!(e.help, "Add or remove tags on this run");
    // The help panel and the popup list it.
    let help = registry::help_text();
    assert!(help.contains("/tag"), "{help}");
    assert!(registry::popup("ta").iter().any(|p| p.name == "tag"));
    // 29 words were built before it; with `/tag` they were 30, `/engine` is the 31st, `/retry` the 32nd, `/resume` the 33rd and `/update` the 34th.
    let built = registry::entries()
        .iter()
        .filter(|e| e.status == Availability::Built)
        .count();
    assert_eq!(built, 34);
}

#[test]
fn the_help_panel_of_the_box_lists_tag() {
    let mut app = on_screen();
    let mut clock = Clock::new();
    send(&mut app, &mut clock, "/help tag");
    let text = screen(&app);
    assert!(text.contains("Add or remove tags on this run"), "{text}");
}
