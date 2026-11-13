//! The one row under the Ctrl-R bar: the matching prompt as pieces, each occurrence of the query a marked piece, cut to the width of the
//! terminal around the first occurrence. The module is pure. It takes the text of the row, the ranges of the occurrences (in characters, as
//! `ham_core::history::Shown` gives them) and the width in terminal cells, and gives back pieces that together take no more cells than the
//! width. The screen draws a marked piece with a style attribute (reversed), so the marks show without colour.
//!
//! * **Fits.** Text that fits the width is whole.
//! * **Cut.** Text that does not fit is cut to a window that holds the first occurrence: it starts a few cells before it (a quarter of the
//!   width, at most [`CONTEXT`]) and shows an [`ELLIPSIS`] on the side that was cut. A width of three cells or less has no room for an ellipsis:
//!   the window starts at the occurrence.
//! * **Cells.** A character takes the cells a terminal gives it ([`cell_width`]): a wide character two, a combining mark none. A window
//!   never starts on a mark with no letter before it, and a wide character that does not fit is left out whole.

use super::select::cell_width;
use std::ops::Range;

/// The character shown where the text was cut.
pub const ELLIPSIS: char = '\u{2026}';
/// The most cells shown before the first occurrence when the text is cut on the left.
pub const CONTEXT: usize = 12;
/// The width up to which a cut row has no ellipsis.
const NO_ELLIPSIS: usize = 3;

/// A stretch of the row that is drawn in one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    /// An occurrence of the query.
    pub marked: bool,
}

/// `text` with its `marks` (ranges of characters; a range past the end is clipped) as pieces that take at most `width` cells. Nothing for a
/// width of 0 or an empty text.
pub fn cut(text: &str, marks: &[Range<usize>], width: usize) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if width == 0 || n == 0 {
        return Vec::new();
    }
    let mut marked = vec![false; n];
    for r in marks {
        for m in marked.iter_mut().take(r.end.min(n)).skip(r.start.min(n)) {
            *m = true;
        }
    }
    let cells: Vec<usize> = chars.iter().map(|&c| cell_width(c)).collect();
    let total: usize = cells.iter().sum();
    let first = marked.iter().position(|&m| m).unwrap_or(0);
    let w = if total <= width {
        Window {
            from: 0,
            to: n,
            left: false,
            right: false,
        }
    } else {
        window(&cells, first, width)
    };
    pieces(&chars, &marked, &w)
}

/// The part of the text that is shown: `from..to`, and whether an ellipsis stands on the left and on the right.
struct Window {
    from: usize,
    to: usize,
    left: bool,
    right: bool,
}

/// The end of the longest run of characters from `from` that takes at most `budget` cells.
fn fill(cells: &[usize], from: usize, budget: usize) -> usize {
    let mut used = 0;
    let mut end = from;
    while end < cells.len() && used + cells[end] <= budget {
        used += cells[end];
        end += 1;
    }
    end
}

/// A window of `width` cells over text that is wider than that, with the character `first` in it.
fn window(cells: &[usize], first: usize, width: usize) -> Window {
    if width <= NO_ELLIPSIS {
        let from = back_over_marks(cells, first);
        return Window {
            from,
            to: fill(cells, from, width),
            left: false,
            right: false,
        };
    }
    // A few cells of context before the occurrence, but no window starts on a mark with no letter before it.
    let context = (width / 4).min(CONTEXT);
    let mut from = first;
    let mut used = 0;
    while from > 0 && used + cells[from - 1] <= context {
        from -= 1;
        used += cells[from];
    }
    let with_context = layout(cells, from, width);
    // When the context pushed the occurrence out of the window (a wide character at a narrow width), it starts at the occurrence.
    if with_context.to > first {
        return with_context;
    }
    layout(cells, first, width)
}

/// `from`, moved back over zero-width characters (a combining mark with no letter before it is drawn on nothing).
fn back_over_marks(cells: &[usize], mut from: usize) -> usize {
    while from > 0 && cells[from] == 0 {
        from -= 1;
    }
    from
}

/// The window that starts at `from` (moved back over zero-width characters): ellipses on the sides that are cut, each taking one cell.
fn layout(cells: &[usize], from: usize, width: usize) -> Window {
    let n = cells.len();
    let from = back_over_marks(cells, from);
    let left = from > 0;
    let budget = width - usize::from(left);
    // One cell stays free for the ellipsis on the right, unless the rest of the text fits without it.
    let mut to = fill(cells, from, budget - 1);
    let mut right = to < n;
    if right {
        let all = fill(cells, from, budget);
        if all >= n {
            to = n;
            right = false;
        }
    }
    Window {
        from,
        to,
        left,
        right,
    }
}

fn ellipsis() -> Piece {
    Piece {
        text: ELLIPSIS.to_string(),
        marked: false,
    }
}

/// The characters of the window as pieces: runs of the same mark, the ellipses apart.
fn pieces(chars: &[char], marked: &[bool], w: &Window) -> Vec<Piece> {
    let mut out = Vec::new();
    if w.left {
        out.push(ellipsis());
    }
    let mut i = w.from;
    while i < w.to {
        let m = marked[i];
        let mut text = String::new();
        while i < w.to && marked[i] == m {
            text.push(chars[i]);
            i += 1;
        }
        out.push(Piece { text, marked: m });
    }
    if w.right {
        out.push(ellipsis());
    }
    out
}
