//! The window of the New run screen's Model box.
//!
//! The box used to take one row for every model of the list, so each model added to the list took a row from the prompt box: at 80x24
//! the fixed parts of the screen take 14 rows, six models left the prompt box 4 and eight would leave it 2, which shows no line of the
//! prompt. The box is now a window onto the list. Pure, no ratatui type and no `App` (the pattern of `runs_view.rs`): `ui.rs` asks
//! [`model_window`] at every draw and draws, titles and makes mouse targets for the rows it names.

/// The rows of the screen the list never takes: the 14 fixed rows of the New run screen (the title, the Target, Repository and Base ref
/// boxes, the two border rows of the Model box, the Submit line and the key line), 3 rows for the prompt box and 1 row for a message.
pub const KEPT_ROWS: u16 = 18;

/// The words every title of the Model box ends with.
const TITLE_KEYS: &str = "(↑↓ picks for this run, d saves it as your default)";

/// The rows of the model list the Model box shows: `(first, count)`, the position of the first row shown and how many rows show.
///
/// `total` is the number of models, `picked` the position of the model the screen is on, `height` the height of the screen's area and
/// `backend_rows` the rows of the Backend box (3 with two backends or more, else 0). The count is the height less [`KEPT_ROWS`] and the
/// Backend box, at least one row and at most the whole list. It reads the height only, so the list does not move when a message appears
/// under it: at 24 rows it is 6, and from 26 rows up eight models all show. The window keeps the picked row near its middle and stops at
/// both ends of the list, so the picked row always shows. A terminal shorter than the kept rows shows the picked row alone; an empty
/// list has no rows.
pub fn model_window(total: usize, picked: usize, height: u16, backend_rows: u16) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let room = usize::from(
        height
            .saturating_sub(KEPT_ROWS)
            .saturating_sub(backend_rows),
    );
    let count = room.clamp(1, total);
    let picked = picked.min(total - 1);
    let first = picked.saturating_sub(count / 2).min(total - count);
    (first, count)
}

/// The title of the Model box for the window `(first, count)` of `total` models. While every model shows it is the title the box always
/// had; while rows are hidden it says which rows show, counted from 1: `Model 1-6 of 8 (...)`, or `Model 5 of 8 (...)` for one row.
pub fn model_box_title(first: usize, count: usize, total: usize) -> String {
    if count >= total {
        return format!("Model {TITLE_KEYS}");
    }
    let from = first + 1;
    if count <= 1 {
        format!("Model {from} of {total} {TITLE_KEYS}")
    } else {
        format!("Model {from}-{} of {total} {TITLE_KEYS}", first + count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_inside_the_list_and_holds_the_pick() {
        for total in 1..=12usize {
            for height in 0..64u16 {
                for backend_rows in [0u16, 3] {
                    for picked in 0..total {
                        let (first, count) = model_window(total, picked, height, backend_rows);
                        assert!((1..=total).contains(&count));
                        assert!(first + count <= total);
                        assert!((first..first + count).contains(&picked));
                    }
                }
            }
        }
    }

    #[test]
    fn the_title_counts_rows_from_one() {
        assert_eq!(model_box_title(0, 6, 6), format!("Model {TITLE_KEYS}"));
        assert_eq!(
            model_box_title(1, 6, 8),
            format!("Model 2-7 of 8 {TITLE_KEYS}")
        );
        assert_eq!(
            model_box_title(7, 1, 8),
            format!("Model 8 of 8 {TITLE_KEYS}")
        );
    }
}
