//! The column plan of the Runs table. The table has a gutter, RUN and STATUS at every width; NAME takes what is left once those
//! three are placed, and CREATED, TARGET and PHASE join in that order, each only while NAME keeps at least [`NAME_KEEP`] columns. Below
//! [`NAME_MIN`] columns of room NAME is not shown at all. The plan is pure: the drawing code in `ui.rs` asks for it with the width of the
//! screen and draws the columns it names, in this order on screen: gutter, RUN, STATUS, NAME, TARGET, PHASE, CREATED.

/// The one-character gutter (`>` running, `!` waiting, `x` failed).
pub const GUTTER: u16 = 1;
/// The RUN column: the marker, a space and a 26-character run id.
pub const RUN: u16 = 28;
/// The STATUS column.
pub const STATUS: u16 = 14;
/// The TARGET column.
pub const TARGET: u16 = 24;
/// The PHASE column.
pub const PHASE: u16 = 12;
/// The CREATED column: an RFC 3339 UTC time.
pub const CREATED: u16 = 20;
/// The BACKEND column, drawn only while the client holds two backends or more: an alias up to this wide, cut with an ellipsis.
pub const BACKEND: u16 = 12;
/// The space between two columns.
pub const GAP: u16 = 1;
/// NAME is shown from this many columns.
pub const NAME_MIN: u16 = 12;
/// An optional column joins only while NAME keeps this many columns.
pub const NAME_KEEP: u16 = 30;

/// Which columns the Runs table shows at one width. `name` is the width of the NAME column, `None` when it is not shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunsColumns {
    pub name: Option<u16>,
    pub created: bool,
    pub target: bool,
    pub phase: bool,
}

/// The columns for a screen `width` columns wide. The table has a border of one column on each side, and a gap of one column between its
/// columns: the gutter, RUN, STATUS and the gap before NAME take 46 of the inner columns.
pub fn runs_columns(width: u16) -> RunsColumns {
    let inner = width.saturating_sub(2);
    let fixed = GUTTER + GAP + RUN + GAP + STATUS + GAP;
    let mut left = inner.saturating_sub(fixed);
    let mut plan = RunsColumns {
        name: None,
        created: false,
        target: false,
        phase: false,
    };
    if left < NAME_MIN {
        return plan;
    }
    for (cost, joined) in [
        (CREATED + GAP, &mut plan.created),
        (TARGET + GAP, &mut plan.target),
        (PHASE + GAP, &mut plan.phase),
    ] {
        if left < cost + NAME_KEEP {
            break;
        }
        left -= cost;
        *joined = true;
    }
    plan.name = Some(left);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_underflows_at_any_width() {
        for w in 0..=u16::MAX / 64 {
            let p = runs_columns(w);
            if let Some(n) = p.name {
                assert!(n >= NAME_MIN, "{w}: {p:?}");
            } else {
                assert!(!p.created && !p.target && !p.phase, "{w}: {p:?}");
            }
        }
    }

    #[test]
    fn the_columns_and_gaps_fill_the_inner_width_exactly_when_name_is_shown() {
        for w in 60..400u16 {
            let p = runs_columns(w);
            let mut used = GUTTER + GAP + RUN + GAP + STATUS + GAP + p.name.unwrap();
            for (shown, width) in [(p.target, TARGET), (p.phase, PHASE), (p.created, CREATED)] {
                if shown {
                    used += GAP + width;
                }
            }
            assert_eq!(used, w - 2, "{w}: {p:?}");
        }
    }

    #[test]
    fn b584_the_backend_column_takes_its_room_from_the_plan_and_fills_the_inner_width_exactly() {
        for w in 73..400u16 {
            let p = runs_columns(w - (BACKEND + GAP));
            let mut used =
                GUTTER + GAP + RUN + GAP + STATUS + GAP + BACKEND + GAP + p.name.unwrap();
            for (shown, width) in [(p.target, TARGET), (p.phase, PHASE), (p.created, CREATED)] {
                if shown {
                    used += GAP + width;
                }
            }
            assert_eq!(used, w - 2, "{w}: {p:?}");
        }
    }

    #[test]
    fn optional_columns_join_in_the_order_created_target_phase() {
        for w in 60..400u16 {
            let p = runs_columns(w);
            assert!(p.created || !p.target, "{w}: {p:?}");
            assert!(p.target || !p.phase, "{w}: {p:?}");
        }
    }
}
