//! The window of the Runs table. The table draws only the rows that fit its box; the first row it draws (the view offset) moves with the
//! selection, so that the marked run is always on screen. The offset is a position in the list the filter shows, not an index into the
//! held list. Everything here is pure: positions in, positions out, no terminal type.
//!
//! * [`follow`] gives the offset for one draw: the offset of the last draw moved the least amount that brings the selection into the window,
//!   and kept inside the list, so a list that became shorter (a refresh, a filter, the project scope) or a taller terminal leaves no blank
//!   rows under the last run.
//! * [`page`] is PageUp and PageDown: the selection and the offset move together by a window less one row, and stop at the ends.

/// The rows a page moves by before the first draw has told the window's height.
pub const DEFAULT_WINDOW: usize = 10;

/// The first row of a window of `height` rows over a list of `len` rows, for a selection at position `selected`: `first`, the offset of the
/// last draw, kept inside the list (no blank row under the last run) and then moved the least amount that brings the selection into the window.
/// A list with no row or a window with no row has offset 0. A selection past the end counts as the last row.
pub fn follow(first: usize, selected: usize, len: usize, height: usize) -> usize {
    if len == 0 || height == 0 {
        return 0;
    }
    let first = first.min(len.saturating_sub(height));
    let selected = selected.min(len - 1);
    if selected < first {
        selected
    } else if selected >= first + height {
        selected + 1 - height
    } else {
        first
    }
}

/// PageDown (`down`) or PageUp: the selection and the offset both move by a window less one row (at least one row; [`DEFAULT_WINDOW`] when no
/// draw has told the height yet), so the selection keeps its row of the window, and both stop at the ends of the list. Returns the offset
/// and the position of the selection.
pub fn page(
    first: usize,
    selected: usize,
    len: usize,
    height: usize,
    down: bool,
) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let height = if height == 0 { DEFAULT_WINDOW } else { height };
    let step = height.saturating_sub(1).max(1);
    let first = first.min(len.saturating_sub(height));
    let selected = selected.min(len - 1);
    let (first, selected) = if down {
        (
            (first + step).min(len.saturating_sub(height)),
            (selected + step).min(len - 1),
        )
    } else {
        (first.saturating_sub(step), selected.saturating_sub(step))
    };
    (follow(first, selected, len, height), selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_selection_inside_the_window_leaves_the_offset_where_it_is() {
        assert_eq!(follow(5, 10, 100, 24), 5);
        assert_eq!(follow(5, 5, 100, 24), 5, "the first row of the window");
        assert_eq!(follow(5, 28, 100, 24), 5, "the last row of the window");
    }

    #[test]
    fn the_row_under_the_window_scrolls_it_by_one_row_and_no_row_before_that_does() {
        // A window of 24 rows from 0 shows positions 0 to 23.
        assert_eq!(follow(0, 23, 200, 24), 0, "position 23 is the last row");
        assert_eq!(follow(0, 24, 200, 24), 1, "position 24 is one row under");
        assert_eq!(follow(0, 100, 200, 24), 77);
    }

    #[test]
    fn the_row_above_the_window_scrolls_it_up_by_the_least() {
        assert_eq!(follow(10, 10, 200, 24), 10, "the first row");
        assert_eq!(follow(10, 9, 200, 24), 9, "one row above");
        assert_eq!(follow(10, 0, 200, 24), 0);
    }

    #[test]
    fn a_shorter_list_pulls_the_offset_back_so_that_no_row_under_the_last_run_is_blank() {
        assert_eq!(follow(150, 3, 10, 24), 0, "the list fits the window");
        assert_eq!(follow(100, 90, 95, 24), 71, "the last window of 95 rows");
        assert_eq!(follow(90, 95, 100, 24), 76);
    }

    #[test]
    fn a_taller_window_pulls_the_offset_back_at_the_end_of_the_list() {
        // The last 24 rows of 200 are 176 to 199; the window becomes 44 rows tall.
        assert_eq!(follow(176, 199, 200, 44), 156);
    }

    #[test]
    fn an_empty_list_or_a_window_with_no_rows_has_offset_zero() {
        assert_eq!(follow(7, 3, 0, 24), 0);
        assert_eq!(follow(7, 3, 100, 0), 0);
    }

    #[test]
    fn a_selection_past_the_end_counts_as_the_last_run() {
        assert_eq!(follow(0, 500, 100, 24), 76);
    }

    #[test]
    fn page_down_moves_the_selection_and_the_offset_by_a_window_less_one_row() {
        assert_eq!(page(0, 0, 200, 24, true), (23, 23));
        assert_eq!(
            page(10, 15, 200, 24, true),
            (33, 38),
            "the row in the window"
        );
        assert_eq!(page(23, 23, 200, 24, true), (46, 46));
    }

    #[test]
    fn page_up_is_the_mirror_of_page_down() {
        assert_eq!(page(46, 46, 200, 24, false), (23, 23));
        assert_eq!(page(33, 38, 200, 24, false), (10, 15));
    }

    #[test]
    fn a_page_stops_at_the_first_and_the_last_run() {
        // The last window of 200 rows starts at 176; the selection goes on to the last run.
        assert_eq!(page(176, 190, 200, 24, true), (176, 199));
        assert_eq!(page(176, 199, 200, 24, true), (176, 199));
        assert_eq!(
            page(184, 190, 200, 24, true),
            (176, 199),
            "an offset past the last window"
        );
        assert_eq!(page(10, 20, 200, 24, false), (0, 0));
        assert_eq!(page(0, 5, 200, 24, false), (0, 0));
        assert_eq!(page(0, 0, 200, 24, false), (0, 0));
    }

    #[test]
    fn a_page_of_a_list_that_fits_the_window_moves_the_selection_only() {
        assert_eq!(page(0, 2, 10, 24, true), (0, 9));
        assert_eq!(page(0, 9, 10, 24, false), (0, 0));
    }

    #[test]
    fn a_window_of_one_row_pages_by_one_row() {
        assert_eq!(page(0, 0, 10, 1, true), (1, 1));
        assert_eq!(page(4, 4, 10, 1, false), (3, 3));
    }

    #[test]
    fn before_the_first_draw_a_page_is_the_default_window_less_one() {
        assert_eq!(
            page(0, 0, 100, 0, true),
            (DEFAULT_WINDOW - 1, DEFAULT_WINDOW - 1)
        );
    }

    #[test]
    fn an_empty_list_has_nowhere_to_page_to() {
        assert_eq!(page(3, 3, 0, 24, true), (0, 0));
        assert_eq!(page(3, 3, 0, 24, false), (0, 0));
    }

    proptest! {
        /// Whatever the last offset, the selection, the list and the window, the offset keeps the selection in view, stays inside the list,
        /// leaves no blank row under the last run, and is the offset of the last draw whenever that already did all three.
        #[test]
        fn the_offset_keeps_the_selection_in_view_and_moves_the_least(
            first in 0usize..400,
            selected in 0usize..400,
            len in 1usize..400,
            height in 1usize..80,
        ) {
            let got = follow(first, selected, len, height);
            let max_first = len.saturating_sub(height);
            prop_assert!(got <= max_first, "{got} is past the last window ({max_first})");
            let at = selected.min(len - 1);
            prop_assert!(got <= at && at < got + height, "{at} is not in {got}..{}", got + height);
            let kept = first.min(max_first);
            if kept <= at && at < kept + height {
                prop_assert_eq!(got, kept, "the old offset already showed the selection");
            } else if at < kept {
                prop_assert_eq!(got, at, "scrolled up by more than it had to");
            } else {
                prop_assert_eq!(got, at + 1 - height, "scrolled down by more than it had to");
            }
        }

        /// The four rows at the edges of a window that needs no clamp: the row above scrolls up by one, the first and the last row leave the
        /// offset alone, the row under scrolls down by one. Random positions meet these rows too rarely to prove them.
        #[test]
        fn the_rows_at_the_edges_of_the_window_move_it_by_exactly_one_row_or_not_at_all(
            first in 0usize..300,
            height in 1usize..60,
            below in 1usize..300,
        ) {
            let len = first + height + below;
            prop_assert_eq!(follow(first, first + height - 1, len, height), first, "the last row");
            prop_assert_eq!(follow(first, first, len, height), first, "the first row");
            prop_assert_eq!(follow(first, first + height, len, height), first + 1, "one row under");
            if first > 0 {
                prop_assert_eq!(follow(first, first - 1, len, height), first - 1, "one row above");
            }
        }

        /// A page keeps both numbers inside the list, shows the selection, and moves toward its end.
        #[test]
        fn a_page_keeps_the_selection_in_view_and_moves_the_right_way(
            first in 0usize..400,
            selected in 0usize..400,
            len in 1usize..400,
            height in 1usize..80,
            down in any::<bool>(),
        ) {
            let before = selected.min(len - 1);
            let (f, s) = page(first, selected, len, height, down);
            prop_assert!(s < len);
            prop_assert!(f <= len.saturating_sub(height));
            prop_assert!(f <= s && s < f + height, "{s} is not in {f}..{}", f + height);
            if down {
                prop_assert!(s >= before);
            } else {
                prop_assert!(s <= before);
            }
        }
    }
}
