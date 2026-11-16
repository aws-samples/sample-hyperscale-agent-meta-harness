//! The run screen builds the rows of its agent table at every draw. A row is made of its agent, the window the agent reports and
//! the layout of the table: cleaning and fitting the cells of 5,000 agents again and again was 44 of 100 samples of a draw. The table
//! keeps each row with what it was made of, makes a row again only when one of the three changed, and shows exactly what it showed.

use ham::tui::agent_table::{AgentTable, Item};
use ham_core::agents::{AgentGroup, AgentRow};
use std::collections::BTreeMap;

fn agent(i: usize) -> AgentRow {
    AgentRow {
        agent_id: format!("A{i:04}"),
        engine: Some("cline".into()),
        model_id: Some(if i.is_multiple_of(2) {
            "us.anthropic.claude-sonnet-5".into()
        } else {
            "qwen.qwen3-coder-480b-a35b-v1:0".into()
        }),
        group: AgentGroup::Working,
        turns: 1,
        last: format!("editing src/module_{i}.py"),
    }
}

fn agents(n: usize) -> Vec<AgentRow> {
    (0..n).map(agent).collect()
}

/// The lines of the table for `rows` under one group heading, with the windows of `windows` (agent id to tokens).
fn lines(
    table: &mut AgentTable,
    width: usize,
    rows: &[AgentRow],
    windows: &BTreeMap<String, u32>,
) -> Vec<String> {
    let items =
        std::iter::once(Item::Group("Working", rows.len())).chain(rows.iter().map(Item::Agent));
    table.lines(width, !windows.is_empty(), items, |id| {
        windows.get(id).copied()
    })
}

/// What a table that has made nothing before shows.
fn fresh(width: usize, rows: &[AgentRow], windows: &BTreeMap<String, u32>) -> Vec<String> {
    lines(&mut AgentTable::new(), width, rows, windows)
}

const NO_WINDOWS: BTreeMap<String, u32> = BTreeMap::new();

#[test]
fn b836_the_rows_are_the_text_the_table_always_showed() {
    // The heads and one row of each layout, written here with the padding of `format!`: a second way to the same text.
    let rows = agents(2);
    let wide = fresh(118, &rows, &NO_WINDOWS);
    assert_eq!(
        wide,
        vec![
            format!(
                "{:<14} {:<9} {:<30} {:<5} LAST",
                "AGENT", "ENGINE", "MODEL", "TURNS"
            ),
            "▾ Working (2)".to_string(),
            format!(
                "{:<14} {:<9} {:<30} {:<5} editing src/module_0.py",
                "A0000", "cline", "us.anthropic.claude-sonnet-5", "1"
            ),
            // a model id of 31 characters is cut to the 30 columns of its cell
            format!(
                "{:<14} {:<9} {:<30} {:<5} editing src/module_1.py",
                "A0001", "cline", "qwen.qwen3-coder-480b-a35b-v1:", "1"
            ),
        ]
    );
    let narrow = fresh(78, &rows, &NO_WINDOWS);
    assert_eq!(
        narrow[0],
        format!("{:<14} {:<9} {:<5} LAST", "AGENT", "ENGINE", "TURNS")
    );
    assert_eq!(
        narrow[2],
        format!(
            "{:<14} {:<9} {:<5} editing src/module_0.py",
            "A0000", "cline", "1"
        )
    );
    // The CTX column shows once a session reports its window; an agent that reports none has a dash.
    let windows = BTreeMap::from([("A0001".to_string(), 1_000_000)]);
    let ctx = fresh(118, &rows, &windows);
    assert_eq!(
        ctx[0],
        format!(
            "{:<14} {:<9} {:<30} {:<6} {:<5} LAST",
            "AGENT", "ENGINE", "MODEL", "CTX", "TURNS"
        )
    );
    assert_eq!(
        ctx[2],
        format!(
            "{:<14} {:<9} {:<30} {:<6} {:<5} editing src/module_0.py",
            "A0000", "cline", "us.anthropic.claude-sonnet-5", "-", "1"
        )
    );
    assert_eq!(
        ctx[3],
        format!(
            "{:<14} {:<9} {:<30} {:<6} {:<5} editing src/module_1.py",
            "A0001", "cline", "qwen.qwen3-coder-480b-a35b-v1:", "1M", "1"
        )
    );
    // An agent with no engine, no model and no summary yet: dashes, and no blanks at the end of the row.
    let bare = AgentRow {
        agent_id: "A0002".into(),
        engine: None,
        model_id: None,
        group: AgentGroup::Working,
        turns: 0,
        last: String::new(),
    };
    assert_eq!(
        fresh(118, &[bare], &NO_WINDOWS)[2],
        format!("{:<14} {:<9} {:<30} 0", "A0002", "-", "-")
    );
}

#[test]
fn b836_a_cell_is_cleaned_and_cut_to_its_column() {
    let mut row = agent(0);
    // a line break and a tab inside a summary become blanks, and a summary longer than the LAST column is cut
    row.last = "ran\tthe tests\nall of them passed and then several more words follow here".into();
    let line = fresh(78, &[row], &NO_WINDOWS).remove(2);
    assert_eq!(
        line,
        format!(
            "{:<14} {:<9} {:<5} ran the tests all of them passed and then sever",
            "A0000", "cline", "1"
        )
    );
    assert_eq!(line.chars().count(), 78);
}

#[test]
fn b836_the_rows_of_agents_that_did_not_change_are_made_once() {
    let rows = agents(500);
    let mut table = AgentTable::new();
    let first = lines(&mut table, 118, &rows, &NO_WINDOWS);
    assert_eq!(first.len(), 502);
    assert_eq!(table.rows_made(), 500);
    for _ in 0..10 {
        assert_eq!(lines(&mut table, 118, &rows, &NO_WINDOWS), first);
    }
    assert_eq!(
        table.rows_made(),
        500,
        "ten more draws of the same agents made their rows again"
    );
}

#[test]
fn b836_a_changed_agent_makes_its_row_again_and_no_other() {
    let mut rows = agents(500);
    let mut table = AgentTable::new();
    let before = lines(&mut table, 118, &rows, &NO_WINDOWS);
    rows[7].last = "running the tests".into();
    rows[7].turns = 2;
    let after = lines(&mut table, 118, &rows, &NO_WINDOWS);
    assert_eq!(
        table.rows_made(),
        501,
        "only the row of the agent that changed is made again"
    );
    assert_ne!(after, before);
    assert!(
        after[9].ends_with("2     running the tests"),
        "{}",
        after[9]
    );
    assert_eq!(after, fresh(118, &rows, &NO_WINDOWS));
    // every field of the row counts
    rows[8].engine = Some("opencode".into());
    rows[9].model_id = None;
    rows[10].agent_id = "B0010".into();
    let after = lines(&mut table, 118, &rows, &NO_WINDOWS);
    assert_eq!(table.rows_made(), 504);
    assert_eq!(after, fresh(118, &rows, &NO_WINDOWS));
}

#[test]
fn b836_another_layout_makes_every_row_again() {
    let rows = agents(50);
    let mut table = AgentTable::new();
    lines(&mut table, 118, &rows, &NO_WINDOWS);
    // narrower than the MODEL column needs: other columns
    assert_eq!(
        lines(&mut table, 78, &rows, &NO_WINDOWS),
        fresh(78, &rows, &NO_WINDOWS)
    );
    assert_eq!(table.rows_made(), 100);
    // wider: the LAST column has more room, so a long summary is cut later
    assert_eq!(
        lines(&mut table, 198, &rows, &NO_WINDOWS),
        fresh(198, &rows, &NO_WINDOWS)
    );
    assert_eq!(table.rows_made(), 150);
    // the first session that reports its window brings the CTX column
    let windows = BTreeMap::from([("A0003".to_string(), 200_000)]);
    assert_eq!(
        lines(&mut table, 198, &rows, &windows),
        fresh(198, &rows, &windows)
    );
    assert_eq!(table.rows_made(), 200);
    // and the same layout again makes nothing
    lines(&mut table, 198, &rows, &windows);
    assert_eq!(table.rows_made(), 200);
}

#[test]
fn b836_the_window_of_an_agent_is_part_of_its_row() {
    let rows = agents(50);
    let mut windows = BTreeMap::from([("A0003".to_string(), 200_000)]);
    let mut table = AgentTable::new();
    lines(&mut table, 118, &rows, &windows);
    assert_eq!(table.rows_made(), 50);
    // a window that changed, and an agent that reports one for the first time
    windows.insert("A0003".into(), 1_000_000);
    windows.insert("A0004".into(), 200_000);
    let after = lines(&mut table, 118, &rows, &windows);
    assert_eq!(table.rows_made(), 52);
    assert_eq!(after, fresh(118, &rows, &windows));
    assert!(after[5].contains(" 1M "), "{}", after[5]);
}

#[test]
fn b836_an_agent_that_left_the_table_is_forgotten() {
    let rows = agents(50);
    let mut table = AgentTable::new();
    lines(&mut table, 118, &rows, &NO_WINDOWS);
    // another run: fewer agents, some with the ids of the first run and other rows
    let mut other = agents(20);
    for r in &mut other {
        r.last = "reading the task".into();
    }
    assert_eq!(
        lines(&mut table, 118, &other, &NO_WINDOWS),
        fresh(118, &other, &NO_WINDOWS)
    );
    assert_eq!(table.rows_made(), 70);
    // the first run again: its rows were forgotten, the 30 the other run never listed included
    assert_eq!(
        lines(&mut table, 118, &rows, &NO_WINDOWS),
        fresh(118, &rows, &NO_WINDOWS)
    );
    assert_eq!(table.rows_made(), 120);
}

/// Four agents under two headings.
fn two_groups(rows: &[AgentRow]) -> Vec<Item<'_>> {
    vec![
        Item::Group("Working", 2),
        Item::Agent(&rows[0]),
        Item::Agent(&rows[1]),
        Item::Group("Completed", 2),
        Item::Agent(&rows[2]),
        Item::Agent(&rows[3]),
    ]
}

#[test]
fn b836_the_groups_keep_their_headings_and_their_order() {
    let mut rows = agents(4);
    for r in &mut rows[2..] {
        r.group = AgentGroup::Completed;
    }
    let mut table = AgentTable::new();
    let first = table.lines(118, false, two_groups(&rows), |_| None);
    assert_eq!(first[1], "▾ Working (2)");
    assert_eq!(first[4], "▾ Completed (2)");
    assert!(first[5].starts_with("A0002 "), "{}", first[5]);
    assert_eq!(table.lines(118, false, two_groups(&rows), |_| None), first);
    assert_eq!(table.rows_made(), 4);
}
