//! The README's picture of the ham mascot, `docs/images/ham-mascot.svg`: a small SVG of plain rectangles in the twelve colours of
//! the terminal app's mascot (`ham::tui::mascot::PALETTE`). It carries no script, no style sheet, no font, no text and no reference to another
//! file or address, so a viewer draws it without fetching anything. It is the first frame of the large form of the terminal drawing, with the
//! flame and the smoke under the feet: the test rasterises its paths and compares them with the 14 by 24 pixel grid of that form, so the
//! picture and the terminal cannot drift apart. It has no mouth: every path is built from straight moves only (no curve can draw a smile), and
//! the grid has no mark below the eyes.
#![allow(clippy::unwrap_used)]

use ham::tui::mascot::{self, Form, Look, PALETTE};
use ratatui::style::Color;
use std::collections::BTreeSet;

fn svg() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/images/ham-mascot.svg"
    );
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Every `#rrggbb` or `#rgb` colour written in the file, lower-cased.
fn colours(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let hex: String = text[i + 1..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            if !hex.is_empty() {
                found.insert(format!("#{}", hex.to_ascii_lowercase()));
            }
        }
        i += 1;
    }
    found
}

/// The value of attribute `name` in the tag text, if it has one.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

/// The `<path .../>` elements, as (fill, d).
fn paths(text: &str) -> Vec<(String, String)> {
    text.split("<path ")
        .skip(1)
        .map(|rest| {
            let tag = rest.split("/>").next().unwrap();
            (
                attr(tag, "fill").unwrap().to_ascii_lowercase(),
                attr(tag, "d").unwrap().to_string(),
            )
        })
        .collect()
}

/// The rectangles of a path that only knows `M x y h w v h h -w z`: (x, y, w, h).
fn rects(d: &str) -> Vec<(usize, usize, usize, usize)> {
    d.split('M')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            // "3 2h2v2h-2z"
            let (x, rest) = s.split_once(' ').unwrap();
            let (y, rest) = rest.split_once('h').unwrap();
            let (w, rest) = rest.split_once('v').unwrap();
            let (h, rest) = rest.split_once('h').unwrap();
            assert_eq!(rest, format!("-{w}z"), "not a plain rectangle: M{s}");
            (
                x.parse().unwrap(),
                y.parse().unwrap(),
                w.parse().unwrap(),
                h.parse().unwrap(),
            )
        })
        .collect()
}

/// The pixel letter of a palette colour by name.
fn letter_named(name: &str) -> char {
    match name {
        "orange" => 'O',
        "eyewhite" => 'W',
        "ink" => 'K',
        "shirt" => 'G',
        "shorts" => 'T',
        "flame_yellow" => 'Y',
        "flame_orange" => 'F',
        "flame_red" => 'R',
        "smoke_dark" => 'D',
        "smoke_mid" => 'M',
        "smoke_light" => 'L',
        "smoke_pale" => 'P',
        other => panic!("{other} is not a palette colour"),
    }
}

/// The pixel letter of the palette colour with this hex.
fn letter_of(hex: &str) -> char {
    match PALETTE.iter().find(|(_, _, h)| *h == hex) {
        Some((name, _, _)) => letter_named(name),
        None => panic!("{hex} is not a palette colour"),
    }
}

/// The 14 by 24 grid the large form draws in the terminal (frame 0), one letter per pixel, `.` for the background.
fn large_grid() -> Vec<String> {
    let letter = |c: Option<Color>| match c {
        None => '.',
        Some(Color::Indexed(i)) => {
            letter_named(PALETTE.iter().find(|(_, index, _)| *index == i).unwrap().0)
        }
        Some(other) => panic!("{other:?}"),
    };
    let mut pixels = Vec::new();
    for line in mascot::lines(Form::Large, Look::Blocks) {
        let (mut top, mut bottom) = (String::new(), String::new());
        for span in &line.spans {
            for glyph in span.content.chars() {
                let (fg, bg) = (span.style.fg, span.style.bg);
                let (t, b) = match glyph {
                    ' ' => (None, None),
                    '█' => (fg, fg),
                    '▀' => (fg, bg),
                    '▄' => (bg, fg),
                    other => panic!("{other:?}"),
                };
                top.push(letter(t));
                bottom.push(letter(b));
            }
        }
        pixels.push(top);
        pixels.push(bottom);
    }
    pixels
}

#[test]
fn the_picture_is_small_and_describes_itself() {
    let text = svg();
    assert!(text.len() <= 4096, "{} bytes", text.len());
    assert!(text.starts_with("<svg "), "{}", &text[..text.len().min(40)]);
    assert!(text.trim_end().ends_with("</svg>"));
    assert!(text.contains("viewBox=\"0 0 14 24\""));
    let title = text
        .split("<title>")
        .nth(1)
        .and_then(|t| t.split("</title>").next());
    let desc = text
        .split("<desc>")
        .nth(1)
        .and_then(|t| t.split("</desc>").next());
    assert_eq!(
        title.map(str::trim),
        Some("The ham mascot, rocket flame and smoke")
    );
    assert!(
        desc.is_some_and(|d| d.trim().len() >= 20),
        "a <desc> is required"
    );
    let desc = desc.unwrap().to_ascii_lowercase();
    assert!(
        desc.contains("no mouth"),
        "the description says the character has no mouth: {desc}"
    );
    assert!(
        desc.contains("flame") && desc.contains("smoke"),
        "the description says there is a flame and smoke under the feet: {desc}"
    );
}

#[test]
fn the_picture_fetches_nothing_runs_nothing_and_holds_no_text() {
    let text = svg().to_ascii_lowercase();
    for banned in [
        "<script",
        "<style",
        "<image",
        "<text",
        "<tspan",
        "<foreignobject",
        "<use",
        "<a ",
        "<iframe",
        "<link",
        "font",
        "href",
        "url(",
        "@import",
        "javascript:",
        "data:",
        "onload",
        "onclick",
        "onerror",
        "onmouse",
        "<!entity",
        "<!doctype",
    ] {
        assert!(!text.contains(banned), "the picture holds `{banned}`");
    }
    // The one address is the XML namespace every SVG declares.
    assert_eq!(text.matches("http").count(), 1, "{text}");
    assert!(text.contains("xmlns=\"http://www.w3.org/2000/svg\""));
}

#[test]
fn the_picture_uses_exactly_the_colours_of_the_terminal_mascot() {
    let text = svg();
    let used = colours(&text);
    let allowed: BTreeSet<String> = PALETTE
        .iter()
        .map(|(_, _, hex)| (*hex).to_string())
        .collect();
    assert_eq!(used, allowed, "the colours of the picture and of PALETTE");
}

#[test]
fn every_path_is_built_from_straight_moves_so_no_curve_can_draw_a_smile() {
    for (fill, d) in paths(&svg()) {
        assert!(
            d.chars()
                .all(|c| c.is_ascii_digit() || "MhvzZ -".contains(c)),
            "{fill}: {d}"
        );
        assert!(!d.is_empty());
        let _ = rects(&d);
    }
}

#[test]
fn the_picture_is_the_large_form_pixel_for_pixel() {
    let mut grid = vec![vec!['.'; 14]; 24];
    for (fill, d) in paths(&svg()) {
        let letter = letter_of(&fill);
        for (x, y, w, h) in rects(&d) {
            for row in grid.iter_mut().skip(y).take(h) {
                for pixel in row.iter_mut().skip(x).take(w) {
                    assert_eq!(
                        *pixel, '.',
                        "two paths paint a pixel of the rectangle at ({x},{y})"
                    );
                    *pixel = letter;
                }
            }
        }
    }
    let drawn: Vec<String> = grid.iter().map(|r| r.iter().collect()).collect();
    assert_eq!(drawn, large_grid());
}
