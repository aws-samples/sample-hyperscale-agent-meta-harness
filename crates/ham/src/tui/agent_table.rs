//! The agent table of the run screen as text rows: the body of the agents card.
//!
//! The run screen gives its cards to the conversation at every draw. The agents card holds a row for every agent of the run, and a row
//! costs work: each of its cells is cleaned (`sanitize_line`, also for text that `RunState` already cleaned) and cut and padded to its
//! column. A row is made of its agent, of the window that agent reports and of the layout of the table, and of nothing else. So
//! [`AgentTable`] keeps each row with what it was made of and makes a row again only when one of the three changed: a draw of a run with
//! 5,000 agents makes the rows that changed since the draw before it, not 5,000 rows. What the table shows is what it showed.

use ham_core::agents::AgentRow;
use ham_core::sanitize::sanitize_line;
use ham_core::tables::compact_tokens;
use ratatui::text::Span;
use std::collections::HashMap;

/// The table shows the MODEL column from this width on.
const WIDE_MIN_WIDTH: usize = 100;

/// One line of the table below its column heads.
#[derive(Clone, Copy, Debug)]
pub enum Item<'a> {
    /// The heading of a group: its title and the number of its agents.
    Group(&'a str, usize),
    /// The row of one agent.
    Agent(&'a AgentRow),
}

/// What a row depends on besides its agent: the columns that show and the width of the LAST column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    /// The MODEL column shows.
    wide: bool,
    /// The CTX column shows: a session of the run reports its window.
    ctx: bool,
    /// The columns of the LAST cell.
    last_w: usize,
}

impl Layout {
    /// The layout of a table `width` columns wide.
    fn of(width: usize, ctx: bool) -> Self {
        // The MODEL column needs the width of a 100-column terminal; a narrower one keeps the summary readable instead.
        let wide = width + 2 >= WIDE_MIN_WIDTH;
        let fixed: usize = columns(wide, ctx).iter().map(|(_, w)| w + 1).sum();
        Self {
            wide,
            ctx,
            last_w: width.saturating_sub(fixed).max(4),
        }
    }
}

/// The heads and widths of the columns before LAST.
fn columns(wide: bool, ctx: bool) -> Vec<(&'static str, usize)> {
    let mut cols = if wide {
        vec![("AGENT", 14), ("ENGINE", 9), ("MODEL", 30), ("TURNS", 5)]
    } else {
        vec![("AGENT", 14), ("ENGINE", 9), ("TURNS", 5)]
    };
    if ctx {
        cols.insert(cols.len() - 1, ("CTX", 6));
    }
    cols
}

/// The columns of one character. A space or a visible ASCII character takes one column; any other character is measured as a `Span`
/// of its own. The table fits five cells for every agent: a million characters for 5,000 agents, each of which was a `String` and a
/// `Span` made only to learn that it is one column wide.
fn char_cols(c: char) -> usize {
    if c == ' ' || c.is_ascii_graphic() {
        1
    } else {
        Span::raw(c.to_string()).width()
    }
}

/// `text` cut to `width` terminal columns and padded to it.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_cols(c);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(width - used));
    out
}

/// The row of one agent: its cells cleaned, fitted to the columns and joined, without the blanks at its end. `window` is the context
/// window the agent's session reports; it shows in the CTX column, `-` while there is none.
fn row_line(
    r: &AgentRow,
    window: Option<u32>,
    cols: &[(&'static str, usize)],
    layout: Layout,
) -> String {
    let dash = |x: &Option<String>| {
        x.as_deref()
            .map(sanitize_line)
            .unwrap_or_else(|| "-".into())
    };
    let mut cells = vec![sanitize_line(&r.agent_id), dash(&r.engine)];
    if layout.wide {
        cells.push(dash(&r.model_id));
    }
    if layout.ctx {
        cells.push(window.map_or_else(|| "-".to_string(), compact_tokens));
    }
    cells.push(r.turns.to_string());
    let mut text: String = cells
        .iter()
        .zip(cols)
        .map(|(c, (_, w))| fit(c, *w) + " ")
        .collect();
    text.push_str(&fit(&sanitize_line(&r.last), layout.last_w));
    text.trim_end().to_string()
}

/// A row as it was last made, with what it was made of.
#[derive(Debug)]
struct Kept {
    row: AgentRow,
    window: Option<u32>,
    line: String,
    /// The call of [`AgentTable::lines`] that last listed the agent.
    seen: u64,
}

/// The agent table and the rows it keeps between draws.
#[derive(Debug, Default)]
pub struct AgentTable {
    /// The layout the kept rows were made for.
    layout: Option<Layout>,
    /// The rows by agent id.
    kept: HashMap<String, Kept>,
    /// How many times [`AgentTable::lines`] was called.
    calls: u64,
    rows_made: u64,
}

impl AgentTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The table as text rows for a card `width` columns wide: the column heads, then `items` in order, a heading for each group and
    /// a row for each agent. `ctx` says whether the CTX column shows, and `window` gives the window an agent's session reports. A row
    /// is made only for an agent that is new or whose row, window or layout changed since the last call; an agent that is no longer
    /// listed is forgotten.
    pub fn lines<'a>(
        &mut self,
        width: usize,
        ctx: bool,
        items: impl IntoIterator<Item = Item<'a>>,
        window: impl Fn(&str) -> Option<u32>,
    ) -> Vec<String> {
        let layout = Layout::of(width, ctx);
        if self.layout != Some(layout) {
            self.kept.clear();
            self.layout = Some(layout);
        }
        self.calls += 1;
        let call = self.calls;
        let cols = columns(layout.wide, layout.ctx);
        let mut head: String = cols.iter().map(|(h, w)| fit(h, *w) + " ").collect();
        head.push_str("LAST");
        let mut lines = vec![head];
        let mut agents = 0usize;
        for item in items {
            match item {
                Item::Group(title, n) => lines.push(format!("▾ {title} ({n})")),
                Item::Agent(r) => {
                    agents += 1;
                    let window = if ctx { window(&r.agent_id) } else { None };
                    let held = match self.kept.get_mut(&r.agent_id) {
                        Some(k) if k.row == *r && k.window == window => {
                            k.seen = call;
                            Some(k.line.clone())
                        }
                        _ => None,
                    };
                    let line = match held {
                        Some(line) => line,
                        None => {
                            self.rows_made += 1;
                            let line = row_line(r, window, &cols, layout);
                            self.kept.insert(
                                r.agent_id.clone(),
                                Kept {
                                    row: r.clone(),
                                    window,
                                    line: line.clone(),
                                    seen: call,
                                },
                            );
                            line
                        }
                    };
                    lines.push(line);
                }
            }
        }
        // An agent that this call did not list has left the table (another run is in focus, or the run was read again).
        if self.kept.len() > agents {
            self.kept.retain(|_, k| k.seen == call);
        }
        lines
    }

    /// How many rows [`AgentTable::lines`] has made so far.
    pub fn rows_made(&self) -> u64 {
        self.rows_made
    }
}
