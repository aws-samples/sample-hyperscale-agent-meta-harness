//! The id is drawn whole at 80, 100 and 120 columns; an id longer than the room is cut with a visible `…`, never silently; the other
//! columns stay under their headings.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::tui::{App, DirectSettings, Effect, Msg, Screen};
use harness_protocol::api::v1::{ArtifactView, EventsPage};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::store::ArtifactKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

const RUN: &str = "01K6A0000000000000000000R1";
/// The ids the packager makes: `zip-`, `evidence-` and `partial-` and the first 20 hex digits of the SHA-256 (`delivery::packager`).
const ZIP: &str = "zip-20ddf01c23f1e244dfcd";
const EVIDENCE: &str = "evidence-3006d045ad5979a10847";
const PARTIAL: &str = "partial-0b1c2d3e4f5a6b7c8d9e";
/// An id longer than any the packager makes.
const LONG: &str = "partial-ffeeddccbbaa99887766-0123456789";

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

fn created() -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01K6A000000000000000000000".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-09-27T09:00:00Z".into(),
        event_type: EventType::RunCreated,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: json!({"target": {"mode": "s3-only"}}),
        },
    }
}

fn artifact(id: &str, kind: ArtifactKind, sha12: char, name: &str, size: u64) -> ArtifactView {
    ArtifactView {
        artifact_id: id.into(),
        kind,
        s3_uri: format!("s3://hamh-live-harness-800000000022/runs/{RUN}/deliverable/{name}"),
        sha256: sha12.to_string().repeat(64),
        size_bytes: size,
    }
}

/// The Artifacts screen of a run that lists `ids` (a zip, an evidence zip and a partial zip, in that order, then more zips).
fn screen_of(ids: &[&str]) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    app.open_run(RUN);
    let e = created();
    let next_after = Some(e.event_id.clone());
    let shard = harness_protocol::store::event_shard(e.authority, e.agent_id.as_deref());
    app.update(Msg::Pages(Ok(vec![(
        shard,
        EventsPage {
            events: vec![e],
            next_after,
        },
    )])));
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    assert_eq!(effects, vec![Effect::LoadArtifacts { run_id: RUN.into() }]);
    assert_eq!(app.screen(), Screen::Artifacts);
    let kinds = [
        (ArtifactKind::Zip, "python-calc-R1-abc1234.zip"),
        (ArtifactKind::Evidence, "evidence.zip"),
        (ArtifactKind::Partial, "python-calc-R1-abc1234-partial.zip"),
    ];
    let shas = ['a', 'b', 'c', 'd', 'e', 'f'];
    let list: Vec<ArtifactView> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let (kind, name) = kinds[i % kinds.len()];
            artifact(
                id,
                kind,
                shas[i % shas.len()],
                name,
                10_000 * (i as u64 + 1),
            )
        })
        .collect();
    app.update(Msg::Artifacts(Ok(list)));
    app
}

/// The screen row that holds the first `n` characters of `id`.
fn row<'a>(screen: &'a str, id: &str, n: usize) -> &'a str {
    let head: String = id.chars().take(n).collect();
    screen
        .lines()
        .find(|l| l.contains(&head))
        .unwrap_or_else(|| panic!("no row holds {head:?}:\n{screen}"))
}

/// The terminal column where `needle` starts in `line`.
fn col(line: &str, needle: &str) -> usize {
    let at = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not in {line:?}"));
    ratatui::text::Span::raw(line[..at].to_string()).width()
}

#[test]
fn the_longest_id_the_packager_makes_is_drawn_whole_at_80_100_and_120_columns() {
    let app = screen_of(&[ZIP, EVIDENCE, PARTIAL]);
    for w in [80u16, 100, 120] {
        let t = screen_text(&app, w, 24);
        for id in [ZIP, EVIDENCE, PARTIAL] {
            assert!(
                row(&t, id, id.len()).contains(id),
                "{id} ({} characters) is cut at {w} columns:\n{t}",
                id.len()
            );
        }
        assert!(!t.contains('…'), "nothing is elided at {w} columns:\n{t}");
    }
    assert_eq!(EVIDENCE.len(), 29, "the id that the 28 column limit cut");
}

#[test]
fn the_other_columns_stay_under_their_headings() {
    let app = screen_of(&[ZIP, EVIDENCE, PARTIAL]);
    for w in [80u16, 100, 120, 200] {
        let t = screen_text(&app, w, 24);
        let header = t.lines().find(|l| l.contains("ARTIFACT")).unwrap();
        let (size_at, sha_at, file_at) = (
            col(header, "SIZE"),
            col(header, "SHA-256"),
            col(header, "FILE"),
        );
        for (id, size, sha, file) in [
            (ZIP, "9.8 KiB", "aaaaaaaaaaaa", "python-c"),
            (EVIDENCE, "19.5 KiB", "bbbbbbbbbbbb", "evidence.z"),
            (PARTIAL, "29.3 KiB", "cccccccccccc", "python-c"),
        ] {
            let r = row(&t, id, id.len());
            assert_eq!(col(r, size), size_at, "SIZE under its heading at {w}: {r}");
            assert_eq!(col(r, sha), sha_at, "SHA-256 under its heading at {w}: {r}");
            assert_eq!(col(r, file), file_at, "FILE under its heading at {w}: {r}");
        }
    }
}

#[test]
fn an_id_longer_than_the_packager_makes_widens_the_column_when_the_terminal_has_the_room() {
    let app = screen_of(&[ZIP, LONG, PARTIAL]);
    let t = screen_text(&app, 200, 24);
    assert!(row(&t, LONG, LONG.len()).contains(LONG), "{t}");
    assert!(!t.contains('…'), "{t}");
    // The columns after it keep their headings.
    let header = t.lines().find(|l| l.contains("ARTIFACT")).unwrap();
    let r = row(&t, LONG, LONG.len());
    assert_eq!(col(r, "bbbbbbbbbbbb"), col(header, "SHA-256"), "{r}");
}

#[test]
fn an_id_that_does_not_fit_ends_with_an_ellipsis_and_never_wraps() {
    let app = screen_of(&[ZIP, EVIDENCE, LONG]);
    for w in [60u16, 70, 80] {
        let t = screen_text(&app, w, 24);
        let header = t.lines().find(|l| l.contains("ARTIFACT")).unwrap();
        // The ARTIFACT column is as wide as the gap between its heading and SIZE.
        let room = col(header, "SIZE") - col(header, "ARTIFACT") - 1;
        // The id does not fit, so the FILE column has given way to its least (4 columns, the border is the last column) before the id is cut.
        assert_eq!(
            usize::from(w) - 1 - col(header, "FILE"),
            4,
            "FILE gives way first at {w}: {header:?}"
        );
        for id in [ZIP, EVIDENCE, LONG] {
            let r = row(&t, id, 10.min(room - 1));
            let at = r.find(&id[..10.min(room - 1)]).unwrap();
            let cell: String = r[at..].chars().take(room).collect();
            if id.chars().count() <= room {
                assert!(
                    cell.starts_with(id),
                    "{id} fits {room} columns at {w}: {r:?}"
                );
            } else {
                let kept: String = id.chars().take(room - 1).collect();
                assert_eq!(
                    cell,
                    format!("{kept}…"),
                    "{id} is cut to {room} columns with a visible marker at {w}, not silently: {r:?}"
                );
            }
        }
    }
    // At 80 columns the ids the packager makes stay whole beside a longer one: the FILE column gives way first.
    let t = screen_text(&app, 80, 24);
    assert!(row(&t, EVIDENCE, 12).contains(EVIDENCE), "{t}");
    assert!(
        !row(&t, LONG, 12).contains(LONG) && row(&t, LONG, 12).contains('…'),
        "{t}"
    );
    assert_eq!(t.lines().filter(|l| l.contains('…')).count(), 1, "{t}");
}

#[test]
fn the_screen_at_80_100_and_120_columns() {
    let app = screen_of(&[ZIP, EVIDENCE, PARTIAL]);
    for (w, h) in [(80u16, 24u16), (100, 24), (120, 24)] {
        insta::assert_snapshot!(format!("ids_{w}x{h}"), screen_text(&app, w, h));
    }
}
