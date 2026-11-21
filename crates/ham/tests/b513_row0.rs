//! The first row with the update notice: the pure part of `tui::version_tag`. The notice is a short text one blank left of the version
//! tag, bold and reversed. The title keeps its 36 columns; the tag takes a shorter form before the notice does, the notice a shorter form
//! before it is dropped, and the tag is never given up for it. Without a notice every function gives what the tag alone gave.
#![allow(clippy::unwrap_used)]

use ham::tui::version_tag::{self, GAP, NoticeWords, Row0, TITLE_KEEP};
use ham::version;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The tests that pin the process-wide backend cell run one at a time.
static CELL: Mutex<()> = Mutex::new(());

fn pinned(backend: Option<&str>) -> MutexGuard<'static, ()> {
    let guard = CELL.lock().unwrap_or_else(PoisonError::into_inner);
    version::set_backend_for_test(backend);
    guard
}

fn words(version: &str) -> NoticeWords {
    let (long, short) = ham_core::update::notice_words(&ham_core::update::UpdateNotice {
        version: version.to_string(),
    });
    NoticeWords { long, short }
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

fn title_buffer(width: u16, title: &str) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, 3));
    buf.set_string(0, 0, title, Style::default().add_modifier(Modifier::BOLD));
    buf.set_string(0, 1, "second row", Style::default());
    buf
}

/// What `row0_for_frame` gives, as the two texts.
fn forms(
    client: &str,
    backend: &str,
    notice: Option<&NoticeWords>,
    width: usize,
    left: usize,
) -> Option<(String, Option<String>)> {
    version_tag::row0_for_frame(client, Some(backend), notice, width, left)
        .map(|row| (row.tag.text().to_string(), row.notice))
}

#[test]
fn b513_row0_without_a_notice_is_todays_row() {
    for left in [0usize, 10] {
        for width in 0..=200usize {
            let today = version_tag::for_frame_at("5.24", Some("5.24"), width, left)
                .map(|tag| Row0 { tag, notice: None });
            assert_eq!(
                version_tag::row0_for_frame("5.24", Some("5.24"), None, width, left),
                today,
                "{width} columns, {left} before the title"
            );
        }
    }
    // The overlay without a notice leaves the buffer the tag's overlay left, and names no cells.
    for (width, left) in [(100u16, 0u16), (80, 0), (80, 10), (60, 0), (40, 0), (8, 0)] {
        let title = "ham · Run 01K6A0000000000000000000R1 · a long title that reaches the tag";
        let mut today = title_buffer(width, title);
        let area = today.area;
        version_tag::overlay_at(&mut today, area, left, "5.24", Some("5.24"));
        let mut with = title_buffer(width, title);
        let rect = version_tag::overlay_with(&mut with, area, left, "5.24", Some("5.24"), None);
        assert_eq!(rect, None, "{width}");
        assert_eq!(with, today, "{width} columns, {left} before the title");
    }
}

/// The forms of the first row for every width, by the room rule: 36 columns of title, a blank, the notice, a blank, the tag.
fn assert_forms_by_width(client: &str, newest: &str, long_at: [usize; 5]) {
    let w = words(newest);
    let full = format!("ham v{client} | backend v{client}");
    let short = format!("h v{client} | b v{client}");
    let bare = format!("{client}|{client}");
    // The room each form needs, from the formula; the brief's numbers for a one-digit and a two-digit major number are pinned by the caller.
    let need = |notice: &str, tag: &str| {
        TITLE_KEEP + GAP + notice.chars().count() + GAP + tag.chars().count()
    };
    assert_eq!(
        [
            need(&w.long, &full),
            need(&w.long, &short),
            need(&w.short, &full),
            need(&w.short, &short),
            need(&w.short, &bare),
        ],
        long_at,
        "the room of the five forms"
    );
    for left in [0usize, 10] {
        for width in 0..=200usize {
            let room = width.saturating_sub(left);
            let expected = if room >= long_at[0] {
                Some((full.clone(), Some(w.long.clone())))
            } else if room >= long_at[1] {
                Some((short.clone(), Some(w.long.clone())))
            } else if room >= long_at[2] {
                Some((full.clone(), Some(w.short.clone())))
            } else if room >= long_at[3] {
                Some((short.clone(), Some(w.short.clone())))
            } else if room >= long_at[4] {
                Some((bare.clone(), Some(w.short.clone())))
            } else {
                // No form with a notice fits: the tag as without one. The tag is never given up for the notice.
                version_tag::for_frame_at(client, Some(client), width, left)
                    .map(|tag| (tag.text().to_string(), None))
            };
            let got = forms(client, client, Some(&w), width, left);
            assert_eq!(got, expected, "{width} columns, {left} before the title");
            // Whenever a notice is drawn, the title keeps its columns and one blank stands on each side of the notice.
            if let Some((tag, Some(notice))) = &got {
                assert!(
                    left + TITLE_KEEP + GAP + notice.chars().count() + GAP + tag.chars().count()
                        <= width,
                    "{width} columns: {notice:?} {tag:?}"
                );
            }
        }
    }
}

#[test]
fn b513_row0_forms_by_width() {
    assert_eq!((TITLE_KEEP, GAP), (36, 1));
    // A one-digit major number: the notice is 15 or 6 columns, the tag 25, 17 or 9.
    assert_forms_by_width("5.24", "5.25", [78, 70, 69, 61, 53]);
    // A two-digit major number adds one column to the long notice and two to each tag.
    assert_forms_by_width("10.01", "10.02", [81, 73, 71, 63, 55]);

    // The rows a person sees, at the widths where a form changes (no title band).
    let w = words("5.25");
    let end = |width: usize| {
        let (tag, notice) = forms("5.24", "5.24", Some(&w), width, 0).unwrap();
        match notice {
            Some(notice) => format!("{notice} {tag}"),
            None => tag,
        }
    };
    assert_eq!(end(100), "v5.25 available ham v5.24 | backend v5.24");
    assert_eq!(end(78), "v5.25 available ham v5.24 | backend v5.24");
    assert_eq!(end(77), "v5.25 available h v5.24 | b v5.24");
    assert_eq!(end(70), "v5.25 available h v5.24 | b v5.24");
    assert_eq!(end(69), "update ham v5.24 | backend v5.24");
    assert_eq!(end(68), "update h v5.24 | b v5.24");
    assert_eq!(end(61), "update h v5.24 | b v5.24");
    assert_eq!(end(60), "update 5.24|5.24");
    assert_eq!(end(53), "update 5.24|5.24");
    assert_eq!(end(52), "5.24|5.24");
    assert_eq!(end(9), "5.24|5.24");
    assert_eq!(forms("5.24", "5.24", Some(&w), 8, 0), None);
    // With the title band the first 10 columns hold the mascot's head: the same forms 10 columns later.
    let banded = |width: usize| forms("5.24", "5.24", Some(&w), width, 10).unwrap();
    let pair = |tag: &str, notice: &str| (tag.to_string(), Some(notice.to_string()));
    assert_eq!(
        banded(88),
        pair("ham v5.24 | backend v5.24", "v5.25 available")
    );
    assert_eq!(banded(80), pair("h v5.24 | b v5.24", "v5.25 available"));
    assert_eq!(banded(79), pair("ham v5.24 | backend v5.24", "update"));
    assert_eq!(banded(72), pair("h v5.24 | b v5.24", "update"));
    // A backend that is not known keeps its slot, so the room is the same.
    let unknown = version_tag::row0_for_frame("5.24", None, Some(&w), 78, 0).unwrap();
    assert_eq!(unknown.tag.text(), "ham v5.24 | backend v?.??");
    assert_eq!(unknown.notice.as_deref(), Some("v5.25 available"));
}

#[test]
fn b513_the_overlay_draws_the_notice_one_blank_left_of_the_tag() {
    let w = words("5.25");
    let mut buf = title_buffer(100, "ham · Runs");
    let area = buf.area;
    let rect = version_tag::overlay_with(&mut buf, area, 0, "5.24", Some("5.24"), Some(&w));
    let row = row_text(&buf, 0);
    assert_eq!(row.chars().count(), 100);
    assert!(row.starts_with("ham · Runs "), "{row:?}");
    assert!(
        row.ends_with(" v5.25 available ham v5.24 | backend v5.24"),
        "{row:?}"
    );
    // The cells of the notice: 15 columns that end one blank before the 25 columns of the tag.
    let x = 100 - 25 - 1 - 15;
    assert_eq!(rect, Some(Rect::new(x, 0, 15, 1)));
    let button = Modifier::BOLD | Modifier::REVERSED;
    for cell_x in x..x + 15 {
        assert!(
            buf[(cell_x, 0)].modifier.contains(button),
            "column {cell_x}: {:?}",
            buf[(cell_x, 0)]
        );
    }
    // The blank on each side of it is no part of the button.
    for cell_x in [x - 1, x + 15] {
        assert_eq!(buf[(cell_x, 0)].symbol(), " ", "column {cell_x}");
        assert!(
            !buf[(cell_x, 0)].modifier.contains(Modifier::REVERSED),
            "column {cell_x}"
        );
    }
    // The tag keeps its own style and the title its own.
    assert!(buf[(100 - 25, 0)].modifier.contains(Modifier::DIM));
    assert!(!buf[(100 - 25, 0)].modifier.contains(Modifier::REVERSED));
    assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(
        row_text(&buf, 1).trim_end(),
        "second row",
        "only row 0 is touched"
    );
}

#[test]
fn b513_a_title_that_would_touch_the_notice_is_clipped_one_cell_before_its_blank() {
    let w = words("5.25");
    let title = "ham · Run 01K6A0000000000000000000R1 · a title that is long enough to reach the notice and the tag";
    let mut buf = title_buffer(100, title);
    let area = buf.area;
    let rect =
        version_tag::overlay_with(&mut buf, area, 0, "5.24", Some("5.24"), Some(&w)).unwrap();
    let row = row_text(&buf, 0);
    assert!(
        row.ends_with("… v5.25 available ham v5.24 | backend v5.24"),
        "{row:?}"
    );
    assert_eq!(buf[(rect.x - 1, 0)].symbol(), " ");
    assert_eq!(buf[(rect.x - 2, 0)].symbol(), "…");
    assert!(
        row.starts_with("ham · Run 01K6A0000000000000000000R1"),
        "the title keeps its first 36 columns: {row:?}"
    );
    // With the title band the head's columns are not read and not cleared.
    let mut buf = title_buffer(100, &format!("{}{title}", "#".repeat(10)));
    let area = buf.area;
    version_tag::overlay_with(&mut buf, area, 10, "5.24", Some("5.24"), Some(&w)).unwrap();
    let row = row_text(&buf, 0);
    assert!(
        row.starts_with("##########ham · Run 01K6A0000000000000000000R1"),
        "{row:?}"
    );
    assert!(
        row.ends_with("… v5.25 available ham v5.24 | backend v5.24"),
        "{row:?}"
    );
}

#[test]
fn b513_a_row_too_narrow_for_the_notice_keeps_the_tag_and_names_no_cells() {
    let w = words("5.25");
    for width in [52u16, 40, 20, 9] {
        let mut buf = title_buffer(width, "ham · Runs");
        let area = buf.area;
        let rect = version_tag::overlay_with(&mut buf, area, 0, "5.24", Some("5.24"), Some(&w));
        assert_eq!(rect, None, "{width}");
        let row = row_text(&buf, 0);
        assert!(row.ends_with("5.24|5.24"), "{width}: {row:?}");
        assert!(!row.contains("update"), "{width}: {row:?}");
    }
    // A frame with no row draws nothing.
    let mut buf = Buffer::empty(Rect::new(0, 0, 100, 0));
    let area = buf.area;
    assert_eq!(
        version_tag::overlay_with(&mut buf, area, 0, "5.24", Some("5.24"), Some(&w)),
        None
    );
}

#[test]
fn b513_reserve_with_is_the_room_of_the_tag_and_the_notice() {
    let client = version::CLIENT;
    let _g = pinned(Some(client));
    let w = words("99.99");
    let cells = |text: &str| text.chars().count();
    let full = format!("ham v{client} | backend v{client}");
    let short = format!("h v{client} | b v{client}");
    // Without a notice it is the room of the tag.
    for width in [200u16, 80, 60, 40, 8] {
        assert_eq!(
            version_tag::reserve_with(width, 0, None),
            version_tag::reserve_at(width, 0),
            "{width}"
        );
    }
    // With one: the tag, the notice and the blank before each.
    assert_eq!(
        version_tag::reserve_with(200, 0, Some(&w)),
        cells(&full) + GAP + cells(&w.long) + GAP
    );
    let narrow = u16::try_from(TITLE_KEEP + GAP + cells(&w.short) + GAP + cells(&short)).unwrap();
    assert_eq!(
        version_tag::reserve_with(narrow, 0, Some(&w)),
        cells(&short) + GAP + cells(&w.short) + GAP
    );
    // Too narrow for a notice: the tag's room alone.
    assert_eq!(
        version_tag::reserve_with(40, 0, Some(&w)),
        version_tag::reserve_at(40, 0)
    );
}

#[test]
fn b513_split_row0_reads_the_title_the_notice_and_the_tag() {
    let tag = "ham v5.24 | backend v5.24";
    let split = version_tag::split_row0;
    assert_eq!(
        split(&format!("ham · Runs              v5.25 available {tag}")),
        ("ham · Runs", Some("v5.25 available"), Some(tag))
    );
    assert_eq!(
        split("ham · Runs    update h v5.24 | b v5.24"),
        ("ham · Runs", Some("update"), Some("h v5.24 | b v5.24"))
    );
    assert_eq!(
        split("ham · Run 01K6A0000000000000000000R1… update 5.24|5.24"),
        (
            "ham · Run 01K6A0000000000000000000R1…",
            Some("update"),
            Some("5.24|5.24")
        )
    );
    assert_eq!(
        split(&format!("v10.02 available {tag}")),
        ("", Some("v10.02 available"), Some(tag)),
        "a row that holds nothing else"
    );
    // No notice: the title and the tag, as `split_tag` reads them.
    let plain = format!("ham · Runs                 {tag}");
    let (title, read) = version_tag::split_tag(&plain);
    assert_eq!(split(&plain), (title, None, read));
    // No tag: nothing is read as a notice, because a notice is never drawn without the tag.
    assert_eq!(
        split("ham · Runs    v5.25 available"),
        ("ham · Runs    v5.25 available", None, None)
    );
    // Text that only looks like a notice stays title.
    for title in [
        "ham · Runs  v5.2 available",
        "ham · Runs  v5.25 availables",
        "ham · Runs  xv5.25 available",
        "ham · Runs  V5.25 available",
        "ham · Runs  updates",
        "ham · Runs  reupdate",
        "ham · Runs  v5.25  available",
    ] {
        let row = format!("{title} {tag}");
        assert_eq!(split(&row), (title, None, Some(tag)), "{row:?}");
    }
}
