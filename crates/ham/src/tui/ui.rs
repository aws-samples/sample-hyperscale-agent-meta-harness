//! Drawing: the Runs screen, the New-run composer, the run dashboard (status,
//! the reconnect banner, the blocked panel, the grouped agent list, the delivery card, all as the chat view of the run) and the Artifacts screen. Every
//! string that came from the API passes through `sanitize_line` here, including agent text that
//! `RunState` already sanitized, so no escape sequence reaches the terminal.

use super::app::{
    App, ArtifactList, BOX_BACKEND, BOX_BASE, BOX_MESSAGE, BOX_MODEL, BOX_OPTIONS, BOX_PROMPT,
    BOX_REPO, BOX_SUBMIT, BOX_TARGET, CloneState, DownloadState, Field, ListLine, MODEL_LIST,
    RUNS_LIST, Screen, TargetKind, refusal,
};
use super::chat::{Band, Card, ChatFacts, ChatView, Header, InputPane};
use super::chat_search::SearchBar;
use super::commands::Armed;
use super::head::{self, Split};
use super::input::{ChatBox, Kind};
use super::keys::{self, Mode};
use super::matchrow;
use super::mouse::{BoxId, PaneId, Target, stable_id};
use super::runs_table::{self, RunsColumns};
use crate::pull::{Pulled, card, local_name};
use crossterm::event::KeyCode;
use ham_core::history::ReverseSearch;
use ham_core::input::InputRequest;
use ham_core::render::{
    CLONE_LABEL, LINK_LABEL, LinkLine, final_card_shown, link_host, link_line, phase_shown,
};
use ham_core::runfilter::position_text;
use ham_core::sanitize::{display_line, sanitize_line};
use ham_core::state::RunView;
use ham_core::tables::{compact_tokens, group_digits};
use harness_protocol::api::v1::RunTarget;
use harness_protocol::direct::{InstructAction, MAX_INPUT_TEXT_BYTES};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

/// The most command rows the popup shows at once, and the text rows of the chip.
const POPUP_ROWS: usize = 7;
const CHIP_ROWS: u16 = 2;

/// The agents table shows the MODEL column from this width on.
const WIDE_MIN_WIDTH: u16 = 100;

/// The most rows the note under the Runs table takes for an error: a long error wraps at word boundaries onto these rows, and one that needs
/// more ends in an ellipsis. Without an error the note is one row.
const RUNS_ERROR_ROWS: usize = 4;

fn s(text: &str) -> String {
    sanitize_line(text)
}

fn enum_text<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "-".into())
}

fn target_text(t: &RunTarget) -> String {
    match t {
        RunTarget::Repo { repo } => s(&repo.to_string()),
        RunTarget::NewRepo { repo } => format!("new {}", s(&repo.to_string())),
        RunTarget::S3Only => "s3-only".into(),
    }
}

fn block(title: &str, focused: bool) -> Block<'static> {
    let title = if focused {
        format!(" ▶ {title} ")
    } else {
        format!(" {title} ")
    };
    let style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style)
}

/// `text` cut to `width` columns, with `…` when something was cut.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The words the empty Options box lists, in the order it lists them. The older words come first, so only room decides how many of the newer
/// ones follow.
const OPTIONS_HINT: [&str; 7] = [
    "--no-rules",
    "--rules",
    "--continue",
    "--input-wait",
    "--goal",
    "--attach",
    "--compute",
];

/// The hint of the empty Options box for a box `width` columns wide: as many whole words of [`OPTIONS_HINT`] as fit, joined by ` · `, so a
/// narrow box never shows half a word. The first word is always there; a box narrower than that clips it.
fn options_hint(width: usize) -> String {
    let mut out = String::new();
    for word in OPTIONS_HINT {
        let next = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} · {word}")
        };
        if !out.is_empty() && next.chars().count() > width {
            break;
        }
        out = next;
    }
    out
}

/// The text of a line without its styles.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|sp| sp.content.as_ref()).collect()
}

/// The message box is the input pane of the chat view: the frozen `ChatBox` signatures, forwarded.
impl InputPane for ChatBox {
    fn rows_at(&self, inner_width: usize) -> usize {
        ChatBox::rows_at(self, inner_width)
    }

    fn render(&self, f: &mut Frame, area: Rect, block: Block<'_>, focused: bool) {
        ChatBox::render(self, f, area, block, focused);
    }
}

/// The message box with the placeholders of pasted text drawn apart.
struct MarkedBox<'a> {
    input: &'a ChatBox,
    marks: Vec<std::ops::Range<usize>>,
}

impl InputPane for MarkedBox<'_> {
    fn rows_at(&self, inner_width: usize) -> usize {
        self.input.rows_at(inner_width)
    }

    fn render(&self, f: &mut Frame, area: Rect, block: Block<'_>, focused: bool) {
        self.input.render_marked(
            f,
            area,
            block,
            focused,
            &self.marks,
            Style::default().fg(Color::Cyan),
        );
    }
}

fn title_line(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(
            Line::from(text.to_string()).style(Style::default().add_modifier(Modifier::BOLD)),
        ),
        area,
    );
}

fn help_line(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(Paragraph::new(text.to_string()), area);
}

impl App {
    /// The agent's CTX cell: its effective window compactly (`session.started` `contextTokens`), `-` while it reports none.
    fn ctx_cell(&self, agent_id: &str) -> String {
        self.view_window(agent_id)
            .map_or_else(|| "-".to_string(), compact_tokens)
    }

    /// The effective window the agent's session reported (`session.started`, or a later
    /// `context.changed`), if any.
    fn view_window(&self, agent_id: &str) -> Option<u32> {
        self.cached
            .as_ref()
            .and_then(|v| v.agents.get(agent_id))
            .and_then(|a| a.context_tokens)
    }

    pub fn draw(&self, f: &mut Frame) {
        let frame = f.area();
        // Every draw makes the hit map again; the draw functions push a region for each box, row, fold, chip and pane they put on screen.
        self.hits.borrow_mut().begin(frame);
        // On a tall, wide terminal the screens lay themselves out below the title band (`head.rs`); otherwise in the whole frame.
        let split = head::split(frame, self.head);
        match self.screen() {
            Screen::Runs => self.draw_runs(f, &split),
            Screen::Composer => {
                if let Some(v) = self.direct_view() {
                    self.draw_composer(f, &v, &split);
                }
            }
            Screen::Dashboard => self.draw_dashboard(f, &split),
            Screen::Artifacts => self.draw_artifacts(f, &split),
            Screen::Models => self.draw_models(f, &split),
            Screen::Reply => self.draw_reply(f, &split),
            Screen::Settings => super::settings::draw(&self.settings, f, &self.hits, &split),
        }
        // The tag explorer takes the whole frame over the Runs screen it was opened on.
        self.draw_tags(f, &split);
        // The mascot's head stands at the left of the band, over whatever the screen and the explorer left there, under the box below. Its smoke
        // is in the frame the app counted, the one of the picture in the empty table.
        head::draw_band(f, frame, &split, self.head, self.mascot_frame());
        // The open-anything box is drawn over whatever screen it was opened on.
        self.draw_openbox(f);
        // The version tag is drawn last, over row 0 of whatever is up.
        super::version_tag::overlay_at(
            f.buffer_mut(),
            frame,
            split.left,
            crate::version::CLIENT,
            crate::version::backend().as_deref(),
        );
    }

    /// The words of the Runs note for a failed read: `cannot list runs: <error>`. `None` while a notice or the mouse hint holds the note, or
    /// when no read has failed.
    fn runs_error_text(&self) -> Option<String> {
        let error = self
            .runs_error
            .as_ref()
            .map(|e| format!("cannot list runs: {}", s(e)));
        // One banner for each backend that did not answer; a client with one backend has none.
        let banners: Vec<String> = self.backend_banners().iter().map(|b| s(b)).collect();
        let banners_empty = banners.is_empty();
        let text = match (error, banners.is_empty()) {
            (Some(e), true) => e,
            (Some(e), false) => format!("{e} · {}", banners.join(" · ")),
            (None, false) => banners.join(" · "),
            (None, true) => return None,
        };
        // A backend that did not answer outranks the first-screen mouse hint; a notice outranks both.
        let hint_hides = self.mouse_hint.is_some() && banners_empty;
        (self.runs_notice.is_none() && !hint_hides).then_some(text)
    }

    fn draw_runs(&self, f: &mut Frame, split: &Split) {
        // An error that does not fit one row wraps onto up to `RUNS_ERROR_ROWS`, so the words that say what to do show on a narrow terminal too.
        // Any other note, and an error that fits, stay one row.
        let width = usize::from(split.area.width);
        let error_rows: Vec<Line<'static>> = match self.runs_error_text() {
            Some(text) if cols(&text) > width => {
                clipped(wrap(&text, width), RUNS_ERROR_ROWS, width)
            }
            _ => Vec::new(),
        };
        let status_rows = u16::try_from(error_rows.len().max(1)).unwrap_or(1);
        let [title, body, status, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(status_rows),
            Constraint::Length(1),
        ])
        .areas(split.area);
        title_line(f, split.title_over(title), "ham · Runs");
        let filter = &self.runs_filter;
        let visible = self.runs_visible();
        // The table is a window onto the shown list: its box has a border above and below and a header row, and the rows between hold the runs
        // from the offset the selection pulls (`runs_view`), so the marked run is always on screen.
        let window = usize::from(body.height.saturating_sub(3));
        let offset = self.runs_window_first(&visible, window);
        let shown_runs = &visible[offset..(offset + window).min(visible.len())];
        // The columns follow the width: RUN, STATUS and NAME first; CREATED, TARGET and PHASE join as NAME keeps its room.
        // With two backends or more a BACKEND column follows STATUS; the rest of the plan is made for the width it leaves.
        let backend_col = self
            .backend_view
            .as_ref()
            .is_some_and(ham_core::backends::BackendView::shows_backend_column);
        let extra = if backend_col {
            runs_table::BACKEND + runs_table::GAP
        } else {
            0
        };
        let plan = runs_table::runs_columns(body.width.saturating_sub(extra));
        let rows: Vec<Row> = shown_runs
            .iter()
            .map(|&i| {
                let r = &self.runs[i];
                let shown = &self.runs_rows[i];
                let mark = if i == self.selected { "▶ " } else { "  " };
                let id = s(&r.run_id);
                let target = target_text(&r.target);
                let mut cells = vec![
                    gutter_cell(shown.gutter),
                    Cell::from(marked_line(
                        &format!("{mark}{id}"),
                        &filter.highlight(&id),
                        2,
                    )),
                    status_cell(r, filter),
                ];
                if backend_col {
                    let alias = self.backend_of(&r.run_id).unwrap_or_else(|| "?".into());
                    cells.push(Cell::from(cut(
                        &s(&alias),
                        usize::from(runs_table::BACKEND),
                    )));
                }
                if let Some(width) = plan.name {
                    cells.push(name_cell(shown, filter, usize::from(width)));
                }
                if plan.target {
                    cells.push(Cell::from(marked_line(
                        &target,
                        &filter.highlight(&target),
                        0,
                    )));
                }
                if plan.phase {
                    cells.push(Cell::from(
                        phase_shown(Some(r.status), Some(r.phase)).unwrap_or("-"),
                    ));
                }
                if plan.created {
                    cells.push(Cell::from(s(&r.created_at)));
                }
                Row::new(cells)
            })
            .collect();
        let mut widths = vec![
            Constraint::Length(runs_table::GUTTER),
            Constraint::Length(runs_table::RUN),
            Constraint::Length(runs_table::STATUS),
        ];
        let mut header = vec!["", "  RUN", "STATUS"];
        if backend_col {
            widths.push(Constraint::Length(runs_table::BACKEND));
            header.push("BACKEND");
        }
        if let Some(width) = plan.name {
            widths.push(Constraint::Length(width));
            header.push("NAME");
        }
        for (shown, width, label) in [
            (plan.target, runs_table::TARGET, "TARGET"),
            (plan.phase, runs_table::PHASE, "PHASE"),
            (plan.created, runs_table::CREATED, "CREATED"),
        ] {
            if shown {
                widths.push(Constraint::Length(width));
                header.push(label);
            }
        }
        // Where the selected run stands among the runs shown: `run 12 of 769` at the right end of the bottom border, which is free at every
        // width (the note line under the table is full at 80 columns). Nothing without a selected run.
        let mut frame = block("runs", false);
        if let Some(at) = visible.iter().position(|&i| i == self.selected) {
            let place = position_text(at + 1, visible.len());
            frame = frame.title_bottom(Line::from(format!(" {place} ")).right_aligned());
        }
        let table = Table::new(rows, widths)
            .header(Row::new(header))
            .block(frame);
        f.render_widget(table, body);
        // The empty table shows the ham mascot once the service has answered that there is no run (`mascot.rs`), unless `ui.mascot` is off.
        super::mascot::draw_runs_empty(
            f,
            body,
            self.head.empty_table
                && self.runs_answered
                && self.runs.is_empty()
                && self.runs_error.is_none(),
            self.head.look,
            self.mascot_frame(),
        );
        {
            // The list scrolls under the wheel; each shown row is a region (a press selects, a second opens).
            let mut map = self.hits.borrow_mut();
            map.push(body, Target::Scroll(PaneId(0)), "↑↓");
            let first = body.y.saturating_add(2);
            let last = body.bottom().saturating_sub(1);
            for (pos, &i) in shown_runs.iter().enumerate() {
                let y = first.saturating_add(u16::try_from(pos).unwrap_or(u16::MAX));
                if y >= last {
                    break;
                }
                map.push(
                    Rect::new(body.x + 1, y, body.width.saturating_sub(2), 1),
                    Target::Row {
                        list: RUNS_LIST,
                        // The position in the list the filter shows, not in the window.
                        index: offset + pos,
                        id: stable_id(&self.runs[i].run_id),
                    },
                    "Enter",
                );
            }
        }
        let selected_run = visible
            .contains(&self.selected)
            .then(|| self.runs.get(self.selected))
            .flatten();
        let hidden = selected_run.map_or_else(String::new, |r| hidden_columns(&plan, r));
        // The selected run's tags come right after the count and the words about runs that wait, before the columns the width hides.
        let tags_part = selected_run
            .and_then(|_| self.runs_rows.get(self.selected))
            .map_or_else(String::new, |row| runs_note_tags(&row.tags));
        // A run that continues another names it.
        let link_part = selected_run
            .and_then(|r| r.continue_from.as_deref())
            .map_or_else(String::new, |c| {
                format!(" · {}", ham_core::retry::link_note(c))
            });
        let note = match (&self.runs_error, self.runs.is_empty()) {
            _ if self.runs_notice.is_some() => {
                self.runs_notice.as_deref().map(s).unwrap_or_default()
            }
            // The first screen of a session that asks for the mouse says how to select text with the terminal itself.
            _ if self.mouse_hint.is_some() && self.backend_banners().is_empty() => {
                self.mouse_hint.as_deref().map(s).unwrap_or_default()
            }
            (Some(_), _) => self.runs_error_text().unwrap_or_default(),
            _ if !self.backend_banners().is_empty() => self.runs_error_text().unwrap_or_default(),
            (None, true) => "no runs yet: press n to start one".into(),
            (None, false) if filter.is_narrowed() => {
                let mut note = filter.summary(visible.len(), self.runs.len());
                if let Some(n) = selected_run.and_then(|r| r.needs_input.as_ref()) {
                    note.push_str(&format!(" · needs input: {}", s(&n.title)));
                }
                note + &tags_part + &link_part + &hidden
            }
            (None, false) => {
                runs_note(&self.runs, self.selected) + &tags_part + &link_part + &hidden
            }
        };
        if error_rows.is_empty() {
            help_line(f, status, &note);
        } else {
            f.render_widget(Paragraph::new(error_rows), status);
        }
        if self.runs_editing {
            // The filter line: every character typed is the filter's; the cursor sits after the text.
            let text = s(filter.text());
            help_line(f, help, &format!("/{text}"));
            let col = u16::try_from(1 + Span::raw(text).width()).unwrap_or(u16::MAX);
            f.set_cursor_position((help.x + col.min(help.width.saturating_sub(1)), help.y));
            self.hits
                .borrow_mut()
                .push(help, Target::Box(BoxId(0)), "/");
        } else {
            // `t tags` is the explorer's key. Without the `p` key the lines are 69 (unfiltered) and 70 (filtered) columns, so the words of
            // `p` have 11 and 10 columns on an 80-column screen and `q quit` stays on it; `s settings` joins an unfiltered line from 91 columns.
            // The `p` key names what a press does: the longest of its wordings that fits the room the other keys leave.
            let (before, after) = if filter.is_narrowed() {
                (
                    "Enter open · ↑↓ · / edit · Esc clears · t tags · ",
                    " · r refresh · q quit",
                )
            } else if help.width >= 91 {
                (
                    "n new run · Enter open · ↑↓ · / find · t tags · ",
                    " · s settings · r refresh · q quit",
                )
            } else {
                (
                    "n new run · Enter open · ↑↓ · / find · t tags · ",
                    " · r refresh · q quit",
                )
            };
            let room = usize::from(help.width).saturating_sub(cols(before) + cols(after));
            let labels = filter
                .scope_key_labels(selected_run.and_then(|_| self.runs_rows.get(self.selected)));
            // A line too narrow for even the shortest wording is clipped at the edge, as it was before.
            let label = labels
                .iter()
                .find(|l| cols(l) <= room)
                .or(labels.last())
                .map_or("p repo", String::as_str);
            let line = format!("{before}{label}{after}");
            help_line(f, help, &line);
            self.push_chips(&line, help);
        }
    }

    /// The composer: Prompt, Target, Repository, Base ref, the Model list and Submit. The title names the
    /// model the run will use; the list shows the supported models with the chosen one marked.
    fn draw_composer(&self, f: &mut Frame, v: &super::app::DirectView, split: &Split) {
        let c = &self.composer;
        let area = split.area;
        let rows = v.rows.len().max(1) as u16;
        let msg = c
            .error
            .as_ref()
            .or(c.notice.as_ref())
            .or(v.saved.as_ref())
            .or(v.note.as_ref());
        // On a short terminal the message keeps at most two lines, so the list of models is never squeezed: the five
        // fixed boxes, the list and the message leave the prompt box three rows at 80x24 (one line of text).
        let max_msg = if area.height <= 24 { 2 } else { 3 };
        let msg_h = msg.map_or(0u16, |e| {
            let chars = e.chars().count() + "error: ".len();
            (chars.div_ceil(usize::from(area.width.max(1))) as u16).clamp(1, max_msg)
        });
        // The Backend box exists only while the client holds two backends or more; with one it has no rows, so nothing moves.
        let backends = self.backend_choices();
        let backend_rows: u16 = if backends.len() >= 2 { 3 } else { 0 };
        let [
            title,
            prompt,
            target,
            repo,
            base,
            backend,
            models,
            submit,
            message,
            help,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(backend_rows),
            Constraint::Length(rows + 2),
            Constraint::Length(1),
            Constraint::Length(msg_h),
            Constraint::Length(1),
        ])
        .areas(area);
        title_line(
            f,
            split.title_over(title),
            &format!("ham · New run · model: {}", s(&v.title)),
        );
        let focus = |field: Field| c.focus == field;
        c.prompt.render(
            f,
            prompt,
            block("Prompt (Enter adds a line)", focus(Field::Prompt)),
            focus(Field::Prompt),
        );
        let radio = |k: TargetKind, label: &str| {
            let dot = if c.kind == k { "(•)" } else { "( )" };
            format!("{dot} {label}")
        };
        f.render_widget(
            Paragraph::new(format!(
                "{}   {}   {}",
                radio(TargetKind::Repo, "--repo"),
                radio(TargetKind::NewRepo, "--new-repo"),
                radio(TargetKind::S3Only, "--s3-only")
            ))
            .block(block("Target (←→ changes)", focus(Field::Target))),
            target,
        );
        let repo_block = block("Repository (cc:<name>)", focus(Field::Repo));
        if c.kind == TargetKind::S3Only {
            f.render_widget(
                Paragraph::new("not used with --s3-only").block(repo_block),
                repo,
            );
        } else {
            c.repo.render(f, repo, repo_block, focus(Field::Repo));
        }
        // The Base ref and the Options share one row, so the prompt box keeps its rows on a short screen.
        let [base_a, options_a] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Min(10)]).areas(base);
        c.base.render(
            f,
            base_a,
            block("Base ref (optional)", focus(Field::Base)),
            focus(Field::Base),
        );
        let options_block = block("Options", focus(Field::Options));
        if c.options.is_empty() {
            let inner = options_block.inner(options_a);
            f.render_widget(
                Paragraph::new(options_hint(usize::from(inner.width)))
                    .style(Style::default().add_modifier(Modifier::DIM))
                    .block(options_block),
                options_a,
            );
            if focus(Field::Options) && inner.width > 0 && inner.height > 0 {
                f.set_cursor_position((inner.x, inner.y));
            }
        } else {
            c.options
                .render(f, options_a, options_block, focus(Field::Options));
        }
        if backends.len() >= 2 {
            let picked = self.backend_picked();
            let radios = backends
                .iter()
                .map(|(a, _)| {
                    let dot = if picked.as_deref() == Some(a.as_str()) {
                        "(•)"
                    } else {
                        "( )"
                    };
                    format!("{dot} {}", s(a))
                })
                .collect::<Vec<_>>()
                .join("   ");
            let note = match picked
                .as_deref()
                .and_then(|p| backends.iter().find(|(a, _)| a == p))
            {
                Some((_, true)) => "  · customer content: allowed",
                Some((_, false)) => "  · customer content: not allowed",
                None => "  · none chosen: pick one before you send",
            };
            f.render_widget(
                Paragraph::new(format!("{radios}{note}")).block(block(
                    "Backend (←→ picks for this run)",
                    focus(Field::Backend),
                )),
                backend,
            );
        }
        // Wide terminals show the Bedrock id beside the alias; narrow ones keep the alias, the name and the window.
        let wide = area.width >= 110;
        let lines: Vec<Line> = v
            .rows
            .iter()
            .map(|r| {
                let cursor = if focus(Field::Model) && r.chosen {
                    "›"
                } else {
                    " "
                };
                let dot = if r.chosen { "(•)" } else { "( )" };
                let id = if wide {
                    format!("{:<35} ", s(&r.id))
                } else {
                    String::new()
                };
                let text = format!(
                    "{cursor}{dot} {:<7} {id}{:<18} ctx {:>9}  {}",
                    s(&r.alias),
                    s(&r.name),
                    group_digits(u64::from(r.window)),
                    r.tag
                );
                let line = Line::from(cut(
                    text.trim_end(),
                    usize::from(area.width.saturating_sub(2)),
                ));
                if r.selectable {
                    line
                } else {
                    line.style(Style::default().add_modifier(Modifier::DIM))
                }
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(block(
                "Model (↑↓ picks for this run, d saves it as your default)",
                focus(Field::Model),
            )),
            models,
        );
        let submit_text = match (c.submitting, focus(Field::Submit)) {
            (true, _) => "  submitting…".to_string(),
            (false, true) => "▶ [ Submit ]  (Enter)".to_string(),
            (false, false) => "  [ Submit ]  (Ctrl-S)".to_string(),
        };
        let submit_width = u16::try_from(submit_text.chars().count()).unwrap_or(u16::MAX);
        f.render_widget(Paragraph::new(submit_text), submit);
        if let Some(e) = &c.error {
            f.render_widget(
                Paragraph::new(format!("error: {}", s(e))).wrap(Wrap { trim: false }),
                message,
            );
        } else if let Some(text) = c.notice.as_ref().or(v.saved.as_ref()).or(v.note.as_ref()) {
            f.render_widget(Paragraph::new(s(text)).wrap(Wrap { trim: false }), message);
        }
        let help_text = if focus(Field::Model) {
            "↑↓ pick model · d save as default · Tab next field · Ctrl-S submit · Esc cancel"
        } else if focus(Field::Backend) {
            "←→ pick backend · Tab next field · Shift-Tab previous · Ctrl-S submit · Esc cancel"
        } else {
            "Tab next field · Shift-Tab previous · Ctrl-S submit · Esc cancel"
        };
        help_line(f, help, help_text);
        {
            // Each box takes the focus on a press; the model rows pick their model.
            let mut map = self.hits.borrow_mut();
            map.push(prompt, Target::Box(BoxId(BOX_PROMPT)), "Tab");
            map.push(target, Target::Box(BoxId(BOX_TARGET)), "Tab");
            if c.kind != TargetKind::S3Only {
                map.push(repo, Target::Box(BoxId(BOX_REPO)), "Tab");
            }
            map.push(base_a, Target::Box(BoxId(BOX_BASE)), "Tab");
            map.push(options_a, Target::Box(BoxId(BOX_OPTIONS)), "Tab");
            if backends.len() >= 2 {
                map.push(backend, Target::Box(BoxId(BOX_BACKEND)), "Tab");
            }
            map.push(models, Target::Box(BoxId(BOX_MODEL)), "Tab");
            // The Submit line is a button over its own words only (not the blank to its right), and not while the run is being started.
            if !c.submitting {
                map.push(
                    Rect::new(submit.x, submit.y, submit_width.min(submit.width), 1),
                    Target::Box(BoxId(BOX_SUBMIT)),
                    "Ctrl-S",
                );
            }
            let first = models.y.saturating_add(1);
            for (i, r) in v.rows.iter().enumerate() {
                let y = first.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
                if y >= models.bottom().saturating_sub(1) {
                    break;
                }
                map.push(
                    Rect::new(models.x + 1, y, models.width.saturating_sub(2), 1),
                    Target::Row {
                        list: MODEL_LIST,
                        index: i,
                        id: stable_id(&r.id),
                    },
                    "↑↓",
                );
            }
        }
        self.push_chips(help_text, help);
    }

    /// The run screen is the chat view: the header, the conversation with the status, question, blocked and delivery cards at its
    /// end, the result panel and the preview of a command above the box, the pinned message box, the notice row and one
    /// footer line.
    fn draw_dashboard(&self, f: &mut Frame, split: &Split) {
        let Some(v) = self.cached.as_ref() else {
            return;
        };
        let area = split.area;
        let typing = self.mode() == Mode::Typing;
        let header = self.chat_header(v);
        let cards = self.chat_cards(v, ChatView::card_width(area.width));
        let title = if typing {
            // The marker shows where the focus is without colour or bold (the old dashboard's block did the same). When the question
            // the draft was for has closed, the box says so and what Enter does now.
            match &self.closed_banner {
                Some(how) => format!(
                    "▶ Message to Cline · the question closed ({}): Enter sends this as a message",
                    s(how)
                ),
                // The box names what Enter does with the text now.
                None => {
                    let base = "▶ Message to Cline".to_string();
                    match self.send_title() {
                        Some(what) => format!("{base} · {}", s(&what)),
                        None => base,
                    }
                }
            }
        } else if self.question().is_some() {
            "Message to Cline · i or Enter to answer".to_string()
        } else {
            "Message to Cline · i or Enter to type".to_string()
        };
        let footer = if typing {
            self.typing_help(area.width)
        } else if self.search.takes_text() {
            "Enter accept · Esc cancel · ↑↓ previous and next match".to_string()
        } else if self.search.walks_matches() {
            "n next · N previous · Esc clear the search".to_string()
        } else {
            self.dashboard_help(area.width)
        };
        // While a message can be withdrawn or confirmed the footer names the key.
        let footer = self.send_footer(footer, typing);
        // The overlay shows once the failures have lasted past the grace (1 s, or 5 s before the first good read).
        let banner = self.dashboard_error.as_ref().and_then(|e| {
            self.link_overlay().map(|o| {
                format!(
                    "⟳ {o}: {} · the run keeps going in AWS; showing the last state read",
                    s(e)
                )
            })
        });
        let notice = self.notice.as_deref().or(self.mouse_hint.as_deref()).map(s);
        // A preview that asks for a typed answer has the cursor; one that waits for Enter alone leaves it in the box.
        let answering = self.armed.as_ref().is_some_and(Armed::wants_cursor);
        let pending = self.pending_rows();
        // The run's name on the row under the header: dim while it is only the seed built from the prompt.
        let name = self.run_name_shown(&v.run_id);
        // The run's tags follow the name on that row: the effective tags of the held Runs row, read at every draw.
        let shown_tags = self.run_tags_shown(&v.run_id);
        let facts = ChatFacts {
            header: &header,
            banner: banner.as_deref(),
            notice: notice.as_deref(),
            pending: &pending,
            footer: &footer,
            box_title: &title,
            focused: typing && !answering,
            header_reserve: super::version_tag::reserve_at(area.width, split.left),
            name: name.as_ref().map(|(text, ai)| (text.as_str(), *ai)),
            tags: &shown_tags,
        };
        let empty = ChatBox::new(Kind::Multi);
        let input = MarkedBox {
            input: self.chat_box().unwrap_or(&empty),
            marks: self.paste_marks(),
        };
        // The result panel and a waiting preview take rows between the conversation and the box; the view keeps the box and the footer.
        let armed_h = self.armed.as_ref().map_or(0, |a| a.height(area.width));
        // The offer of a finished run takes the panel's place while the box holds text and no result is shown.
        let hint = self.follow_up_hint();
        let panel = self.panel.as_ref().or(hint.as_ref());
        let panel_h = panel.map_or(0, |p| p.height(area));
        // Ctrl-R takes two rows (the bar and the match); the search inside the chat takes one.
        let search_h = if self.hsearch.is_some() {
            2
        } else {
            u16::from(self.search.is_active())
        };
        // The chip that asks about a plain phrase and the command popup sit right above the box.
        let popup = self.popup_entries();
        let chip_h = if self.chip.is_some() {
            CHIP_ROWS + 2
        } else {
            0
        };
        let popup_h = if popup.is_empty() {
            0
        } else {
            u16::try_from(popup.len().min(POPUP_ROWS)).unwrap_or(0) + 2
        };
        let mut chat = self.chat.borrow_mut();
        chat.set_cards(&cards);
        // In the band the header goes to the band's title rectangle and the box keeps a third of the rows below the band.
        let band = split.band.then(|| Band {
            title: split.title,
            cap_base: split.cap_base(),
        });
        let areas = chat.draw_in(
            f,
            area,
            band,
            &facts,
            &input,
            armed_h + panel_h + search_h + chip_h + popup_h,
        );
        let room = areas.reserved;
        // What asks for a key keeps its rows first: the search bar, the preview, the chip, the popup; the result panel takes what is left.
        let search_h = search_h.min(room.height);
        let armed_h = armed_h.min(room.height - search_h);
        let chip_h = chip_h.min(room.height - search_h - armed_h);
        let popup_h = popup_h.min(room.height - search_h - armed_h - chip_h);
        let panel_h = panel_h.min(room.height - search_h - armed_h - chip_h - popup_h);
        if let (Some(p), true) = (panel, panel_h > 0) {
            p.draw(f, Rect::new(room.x, room.y, room.width, panel_h), typing);
        }
        if let (Some(a), true) = (&self.armed, armed_h > 0) {
            a.draw(f, Rect::new(room.x, room.y + panel_h, room.width, armed_h));
        }
        let bar = Rect::new(room.x, room.y + panel_h + armed_h, room.width, 1);
        if let (Some(hs), true) = (&self.hsearch, search_h > 0) {
            Self::draw_history_search(f, Rect::new(bar.x, bar.y, bar.width, search_h), hs);
        } else if search_h > 0 {
            f.render_widget(SearchBar::new(self.search.search()), bar);
            // While the query is typed the terminal's cursor sits right after it.
            if self.search.takes_text() {
                let typed = u16::try_from(cols(self.search.search().query())).unwrap_or(u16::MAX);
                f.set_cursor_position((
                    bar.x + 1u16.saturating_add(typed).min(bar.width.saturating_sub(1)),
                    bar.y,
                ));
            }
        }
        let chip_area = Rect::new(
            room.x,
            room.y + panel_h + armed_h + search_h,
            room.width,
            chip_h,
        );
        if let (Some(chip), true) = (&self.chip, chip_h > 0) {
            self.draw_chip(f, chip_area, chip);
        }
        let popup_area = Rect::new(room.x, chip_area.y + chip_h, room.width, popup_h);
        if popup_h > 0 {
            self.draw_popup(f, popup_area, &popup);
        }
        {
            // The conversation scrolls under the wheel and its fold heads toggle on a press; a press in the message box starts typing there;
            // what the app draws over the conversation (the result panel, a preview, the search bar) takes the press and the wheel itself.
            let mut map = self.hits.borrow_mut();
            map.push(areas.transcript, Target::Text(PaneId(0)), "PgUp PgDn");
            for (rect, block) in chat.fold_heads() {
                map.push(rect, Target::Fold { block }, "Space");
            }
            map.push(areas.input, Target::Box(BoxId(BOX_MESSAGE)), "i");
            if panel_h > 0 {
                map.push(
                    Rect::new(room.x, room.y, room.width, panel_h),
                    Target::Block,
                    "Esc",
                );
            }
            if armed_h > 0 {
                map.push(
                    Rect::new(room.x, room.y + panel_h, room.width, armed_h),
                    Target::Block,
                    "Esc",
                );
            }
            if search_h > 0 {
                map.push(bar, Target::Block, "Esc");
            }
            if chip_h > 0 {
                map.push(chip_area, Target::Block, "Esc");
            }
            if popup_h > 0 {
                map.push(popup_area, Target::Block, "Esc");
            }
        }
        self.push_chips(&footer, areas.footer);
    }

    /// The chip of a plain phrase: the command it would run, for which run, and what Enter and Esc do.
    fn draw_chip(&self, f: &mut Frame, area: Rect, chip: &super::app::Chip) {
        f.render_widget(Clear, area);
        let words = chip.argv.first().map_or(String::new(), |w| s(w));
        let for_run = chip
            .argv
            .get(1)
            .map_or(String::new(), |r| format!(" for {}", s(r)));
        let lines = vec![
            Line::from(Span::styled(
                format!("Run ham {words}{for_run}?"),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(
                "Enter = prepare it (preview and confirm follow), Esc = dismiss and keep the draft",
            ),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Command? ")),
            area,
        );
    }

    /// The command popup: the matching built commands with their arguments and one line of help, the marked one bold.
    fn draw_popup(
        &self,
        f: &mut Frame,
        area: Rect,
        entries: &[&'static ham_core::registry::Entry],
    ) {
        f.render_widget(Clear, area);
        let rows = usize::from(area.height.saturating_sub(2)).max(1);
        let sel = self.popup_sel.min(entries.len().saturating_sub(1));
        // The window follows the marker.
        let start = (sel + 1).saturating_sub(rows);
        let width = usize::from(area.width.saturating_sub(2));
        let lines: Vec<Line> = entries
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .map(|(i, e)| {
                let usage = if e.usage.is_empty() {
                    String::new()
                } else {
                    format!(" {}", e.usage)
                };
                let mark = if i == sel { "▶" } else { " " };
                let text = cut(&format!("{mark} /{}{usage}   {}", e.name, e.help), width);
                if i == sel {
                    Line::from(Span::styled(
                        text,
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(text)
                }
            })
            .collect();
        let title = format!(
            " Commands {}/{} · Tab completes · Esc closes ",
            sel + 1,
            entries.len()
        );
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    /// The key chips of a one-line key help (`n new run · Enter open · ↑↓ select`): each part that starts with a key's name is a region that
    /// presses that key. Parts such as `↑↓ select` or `Ctrl-J new line` name no single key and get none.
    pub(super) fn push_chips(&self, line: &str, row: Rect) {
        const SEP: &str = " · ";
        let mut map = self.hits.borrow_mut();
        let mut x = usize::from(row.x);
        for (n, part) in line.split(SEP).enumerate() {
            if n > 0 {
                x += Span::raw(SEP).width();
            }
            let width = Span::raw(part).width();
            if let Some((code, name)) = chip_key(part) {
                let rect = Rect::new(
                    u16::try_from(x).unwrap_or(u16::MAX),
                    row.y,
                    u16::try_from(width).unwrap_or(u16::MAX),
                    1,
                );
                map.push(rect.intersection(row), Target::Key(code), name);
            }
            x += width;
        }
    }

    /// Ctrl-R: the bar (`history search: hel  1/2`) with the terminal's cursor after the query, and under it the match, one line.
    fn draw_history_search(f: &mut Frame, area: Rect, search: &ReverseSearch) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let bar = display_line(&search.bar_text(width));
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                bar,
                Style::default().add_modifier(Modifier::BOLD),
            ))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let typed = cols("history search: ") + cols(search.query());
        f.set_cursor_position((
            area.x + u16::try_from(typed).unwrap_or(u16::MAX).min(area.width - 1),
            area.y,
        ));
        if area.height >= 2 {
            // The match on one row, cut around the first occurrence of the query; each occurrence is reversed and bold, so the marks show
            // without colour too.
            let plain = Style::default().fg(Color::Cyan);
            let mark = plain.add_modifier(Modifier::REVERSED | Modifier::BOLD);
            let spans: Vec<Span> = search.shown().map_or_else(Vec::new, |s| {
                matchrow::cut(&s.text, &s.marks, usize::from(area.width))
                    .into_iter()
                    .map(|p| Span::styled(p.text, if p.marked { mark } else { plain }))
                    .collect()
            });
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(area.x, area.y + 1, area.width, 1),
            );
        }
    }

    /// The header of the chat view from the run's view and detail.
    fn chat_header(&self, v: &RunView) -> Header {
        let state = if self.question().is_some_and(|q| q.blocking) {
            "needs you".to_string()
        } else {
            v.status
                .as_ref()
                .map_or_else(|| "unknown".to_string(), enum_text)
        };
        let detail = self.detail.as_ref();
        // The model calls are in trouble (a retry, a wait, an outage): the words sit next to the state, as the engine's capability words do.
        let state = match v.route_trouble() {
            Some(trouble) => format!("{state} \u{b7} {trouble}"),
            None => state,
        };
        Header {
            run_id: v.run_id.clone(),
            // The model the run is on now: the one the latest switch went to, else the one its session started on.
            model: v
                .route
                .model
                .as_deref()
                .or_else(|| v.direct_model_id())
                .or_else(|| self.model_fallback(&v.run_id))
                .map(ham_core::render::model_name)
                .filter(|m| !m.is_empty()),
            state: self.state_with_caps(state),
            elapsed_s: self.elapsed(),
            // The stage's own count when it has one; a stage that counts none (tokensUsed 0) still reports the usage of the run's turns as
            // `cost`, and the delivery card sums that, so the header does too.
            tokens: detail.and_then(|d| {
                Some(d.tokens_used)
                    .filter(|t| *t > 0)
                    .or_else(|| crate::pull::run_usage(d).map(|u| u.tokens))
            }),
            cost: detail
                .map(|d| d.cost_usd)
                .filter(|c| *c > 0.0)
                .map(|c| format!("${c:.2}")),
            as_of_turn: Some(self.turns()).filter(|t| *t > 0),
            updated_ago_s: self.updated_ago(),
        }
    }

    /// The cards at the end of the conversation, in order: the status, the agents (folded), the blocked panel, the question and the
    /// delivery card. `width` is the columns a card line gets before it wraps.
    fn chat_cards(&self, v: &RunView, width: usize) -> Vec<Card> {
        let delivered = v.zip.is_some();
        let mut cards = Vec::new();
        // The status block stays open (a few lines: the state, the model, where a run that switched models ended, the outcome, the pull
        // request); the agent table is folded, because a run can have hundreds of agents.
        cards.push(Card {
            id: "status".into(),
            title: "status".into(),
            lines: header_lines(v, width, self.model_fallback(&v.run_id))
                .iter()
                .map(line_text)
                .collect(),
            folded: false,
            alert: false,
        });
        cards.push(Card {
            id: "agents".into(),
            title: format!("agents {}", v.agents.len()),
            lines: self.agent_lines(v, width),
            folded: true,
            alert: false,
        });
        super::skills_card::push_card(&mut cards, v, width);
        super::engine_config_card::push_card(&mut cards, v, width);
        // A run that ended early (its root execution stopped, failed or timed out) shows why where a blocked run shows its reason.
        if !delivered {
            let (lines, title) = if v.aborted.is_some() {
                (aborted_lines(v), "ended early · the run was cleared")
            } else {
                let title = if self.run_is_blocked() {
                    "blocked · r retry"
                } else {
                    "blocked · c cancel"
                };
                (blocked_lines(v), title)
            };
            if !lines.is_empty() {
                cards.push(Card {
                    id: "blocked".into(),
                    title: title.into(),
                    lines: lines.iter().map(line_text).collect(),
                    folded: false,
                    alert: true,
                });
            }
        }
        if let Some(q) = v.input.open.as_ref() {
            let (title, lines) = self.card(q, width, usize::MAX);
            cards.push(Card {
                id: "question".into(),
                title,
                lines: lines.iter().map(line_text).collect(),
                folded: false,
                alert: q.blocking,
            });
        }
        super::goal_brake::push_card(&mut cards, v, width);
        if delivered {
            let text = match &self.download {
                DownloadState::Done(p) | DownloadState::Refresh(p) => {
                    let again = matches!(self.download, DownloadState::Refresh(_));
                    let shown = ham_core::render::FinalCard {
                        tests: self.tests.clone(),
                        flow: self.flow.clone(),
                        ..card(self.detail.as_ref(), Some(p))
                    };
                    let whole = self.with_clone_state(final_card_shown(v, &shown), &v.run_id);
                    self.with_link_state(&whole, p, again, width)
                }
                DownloadState::Failed(e) => format!(
                    "download failed: {}\nd try again, or run: ham pull {}",
                    s(e),
                    s(&v.run_id)
                ),
                DownloadState::Pending | DownloadState::None => "downloading the zip…".into(),
            };
            cards.push(Card {
                id: "delivery".into(),
                title: "delivery".into(),
                lines: text.lines().map(s).collect(),
                folded: false,
                alert: false,
            });
        }
        cards
    }

    /// The agent table as text rows: the column heads, then a heading for each group and a row for each agent.
    fn agent_lines(&self, v: &RunView, width: usize) -> Vec<String> {
        // The MODEL column needs the width of a 100-column terminal; a narrower one keeps the summary readable instead.
        let wide = width + 2 >= usize::from(WIDE_MIN_WIDTH);
        // The CTX column shows once a session reports its window.
        let ctx = !v.windows().is_empty();
        let mut cols: Vec<(&str, usize)> = if wide {
            vec![("AGENT", 14), ("ENGINE", 9), ("MODEL", 30), ("TURNS", 5)]
        } else {
            vec![("AGENT", 14), ("ENGINE", 9), ("TURNS", 5)]
        };
        if ctx {
            cols.insert(cols.len() - 1, ("CTX", 6));
        }
        let fixed: usize = cols.iter().map(|(_, w)| w + 1).sum();
        let last_w = width.saturating_sub(fixed).max(4);
        let mut head: String = cols.iter().map(|(h, w)| fit(h, *w) + " ").collect();
        head.push_str("LAST");
        let mut lines = vec![head];
        for line in &self.list {
            match line {
                ListLine::Group(g, n) => lines.push(format!("▾ {} ({n})", g.title())),
                ListLine::Agent(i) => {
                    let Some(r) = self.agent_rows.get(*i) else {
                        continue;
                    };
                    let dash =
                        |x: &Option<String>| x.as_deref().map(s).unwrap_or_else(|| "-".into());
                    let mut cells = vec![s(&r.agent_id), dash(&r.engine)];
                    if wide {
                        cells.push(dash(&r.model_id));
                    }
                    if ctx {
                        cells.push(self.ctx_cell(&r.agent_id));
                    }
                    cells.push(r.turns.to_string());
                    let mut text: String = cells
                        .iter()
                        .zip(&cols)
                        .map(|(c, (_, w))| fit(c, *w) + " ")
                        .collect();
                    text.push_str(&fit(&s(&r.last), last_w));
                    lines.push(text.trim_end().to_string());
                }
            }
        }
        lines
    }

    /// The dashboard's help line: the answer keys that are offered while a question is open, else the usual keys.
    fn dashboard_help(&self, width: u16) -> String {
        let Some(q) = self.question() else {
            return self.usual_help(width);
        };
        let wide = width >= 100;
        let mut parts: Vec<&str> = Vec::new();
        if wide {
            parts.push("↑↓ scroll");
        }
        let offered = |a| refusal(q, a).is_none();
        if offered(InstructAction::ContinueWithPrompt) {
            parts.push("p message");
        }
        if offered(InstructAction::NewSessionWithPrompt) {
            parts.push(if wide { "s fresh session" } else { "s fresh" });
        }
        if offered(InstructAction::TryModel) {
            parts.push("m model");
        }
        if offered(InstructAction::Finish) {
            parts.push("f f finish");
        }
        parts.extend(["a artifacts", "c c cancel", "q quit"]);
        parts.join(" · ")
    }

    /// The footer of the focused message box. A run that waits for the person adds the keys that answer the question, with the way to them:
    /// the box is focused when a run opens (plan 3.3) and the hotkeys work after Esc, so a stuck run must not hide them. A run that waits for
    /// nobody says the same of the keys that cancel it and open its files.
    fn typing_help(&self, width: u16) -> String {
        let usual = keys::typing_footer();
        let Some(q) = self.question().filter(|q| q.blocking) else {
            return self.typing_hint(&usual, width);
        };
        let offered = |a| refusal(q, a).is_none();
        let mut keys: Vec<&str> = Vec::new();
        if offered(InstructAction::ContinueWithPrompt) {
            keys.push("p message");
        }
        if offered(InstructAction::NewSessionWithPrompt) {
            keys.push("s fresh session");
        }
        if offered(InstructAction::TryModel) {
            keys.push("m model");
        }
        if offered(InstructAction::Finish) {
            keys.push("f f finish");
        }
        if keys.is_empty() {
            return usual;
        }
        format!(
            "Enter answer · Esc browse, then {} · Ctrl-J new line · Ctrl-C clear, then quit",
            keys.join(" · ")
        )
    }

    /// The usual typing footer with the hotkeys that follow Esc named in its `Esc browse`: a first `c`, `a`, `o` or `d` in the box is a
    /// letter, so the line says to leave the box first. The longest wording that fits `width` is used: `Esc browse, then o open PR, d download,
    /// a artifacts, c c cancel` (the first two once a link exists), `Esc browse, then c c cancel`, `Esc browse (c c cancel)`, else the usual line.
    /// The width counts the line as it is drawn: while a message can be withdrawn or confirmed [`App::send_footer`] adds ` · Esc, then w
    /// withdraw` (and `, x confirm, h choose`) to it, and a line too wide for the screen loses that ending to an ellipsis.
    /// The keys share the one part of the line that the chip for Esc covers, so no chip for a letter is made that would type it in the box.
    /// A blocked run has ended and has nothing to cancel: its wording leaves out `c c cancel` and keeps `Esc browse, then a artifacts` (or the
    /// usual `Esc browse`), and [`App::send_footer`] ends the line with `Esc, then r retry`.
    fn typing_hint(&self, usual: &str, width: u16) -> String {
        let keys = self.link_keys();
        let blocked = self.run_is_blocked();
        let mut then: Vec<&str> = Vec::new();
        if keys.open() {
            then.push(if keys.pr { "o open PR" } else { "o open" });
        }
        if keys.zip {
            then.push("d download");
        }
        then.push("a artifacts");
        if !blocked {
            then.push("c c cancel");
        }
        let mut wordings = vec![format!("Esc browse, then {}", then.join(", "))];
        if !blocked {
            wordings.push("Esc browse, then c c cancel".to_string());
            wordings.push("Esc browse (c c cancel)".to_string());
        }
        wordings
            .into_iter()
            .map(|words| usual.replacen("Esc browse", &words, 1))
            .find(|line| cols(&self.send_footer(line.clone(), true)) <= usize::from(width))
            .unwrap_or_else(|| usual.to_string())
    }

    /// The usual keys. Once a link exists, `o` and `d` come first, and the scroll keys give way when the line would not fit.
    fn usual_help(&self, width: u16) -> String {
        let keys = self.link_keys();
        let mut lead: Vec<&str> = Vec::new();
        if keys.open() {
            lead.push(if keys.pr { "o open PR" } else { "o open" });
        }
        if keys.zip {
            lead.push("d download");
        }
        if keys.pr {
            lead.push("y copy PR link");
        }
        let width = usize::from(width);
        let mut text = String::new();
        for scroll in [Some("↑↓ PgUp PgDn scroll"), Some("↑↓ scroll"), None] {
            let mut parts = lead.clone();
            parts.extend(scroll);
            // A blocked run has ended: `r retry` stands where `c cancel` stood.
            let ends = if self.run_is_blocked() {
                "r retry"
            } else {
                "c cancel"
            };
            parts.extend(["a artifacts", ends, "Esc runs", "n new run", "q quit"]);
            text = parts.join(" · ");
            // Before a link exists the line is the old one, whatever the width.
            if lead.is_empty() || text.chars().count() <= width {
                break;
            }
        }
        text
    }

    /// The delivery card with its `Link` line replaced by the one short line (the link itself is never drawn), and, while `d` fetches the
    /// zip again, its `Zip` line by what is happening.
    fn with_link_state(&self, card: &str, p: &Pulled, again: bool, width: usize) -> String {
        let expires_at = self.link_expires.as_deref().unwrap_or(&p.url.expires_at);
        let epoch = harness_protocol::common::parse_utc_unix(expires_at);
        let expires = epoch.map_or_else(|| s(expires_at), |e| self.clock_text(e));
        let expired = self.now.zip(epoch).is_some_and(|(now, e)| now >= e);
        let keys = self.link_keys();
        let key_text = match (keys.pr, expired) {
            (true, _) => "d download",
            (false, true) => "o new link · d download",
            (false, false) => "o open · d download",
        };
        let name = p
            .path
            .file_name()
            .map_or_else(String::new, |n| s(&n.to_string_lossy()));
        let host = link_host(&p.url.url);
        let line = link_line(
            &LinkLine {
                host: &host,
                name: &name,
                expires: Some(&expires).filter(|e| !e.is_empty()).map(String::as_str),
                expired,
                keys: key_text,
            },
            width,
        );
        card.lines()
            .map(|l| {
                if l.starts_with(LINK_LABEL) {
                    line.clone()
                } else if again && l.starts_with("Zip       ") {
                    format!("Zip       downloading {} bytes…", p.artifact.size_bytes)
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The card of an open question: its border title and its lines, at most `rows` of them (the keys line is the last, and kept).
    /// The words say what happened to the run. Nothing here says the work failed or is done.
    pub(crate) fn card(
        &self,
        q: &InputRequest,
        width: usize,
        rows: usize,
    ) -> (String, Vec<Line<'static>>) {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let (title, heading) = if q.blocking {
            (
                format!(
                    "Cline needs you · {}",
                    self.secs_left(q).map_or_else(
                        || "waiting for your answer".to_string(),
                        ham_core::input::left_text
                    )
                ),
                Some(s(&q.headline())),
            )
        } else {
            (s(&q.title), None)
        };
        // The numbers belong to the heading line (one line when they fit); a notice carries them in its text.
        let numbers = format!(
            "{}{}.",
            q.numbers(),
            if q.blocking { "" } else { " so far" }
        );
        let explain = if !q.blocking && q.reason == "provider_outage" {
            OUTAGE_TEXT.to_string()
        } else {
            s(&q.words)
        };
        // A code this client has no words for carries the event's title as its explanation: the heading says it already.
        let explain = if explain == s(&q.title) {
            String::new()
        } else {
            explain
        };
        let mut info = wrap(
            if q.blocking {
                explain
            } else {
                format!("{explain} {numbers}")
            }
            .trim(),
            width,
        );
        if !q.blocking
            && let Some(e) = q.evidence.last_error.as_deref().filter(|e| !e.is_empty())
        {
            info.extend(wrap(&format!("Last error: {}", s(e)), width));
        }
        // Cline's last words, on one paragraph.
        let last = q
            .last_message
            .as_deref()
            .map(|m| m.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|m| !m.is_empty())
            .map(|m| wrap(&format!("Last words: {}", s(&m)), width))
            .unwrap_or_default();
        let files = (!q.files.is_empty()).then(|| files_line(q, width));
        let keys = if !q.blocking {
            None
        } else if self.answer_waiting() {
            Some("answer sent · waiting for Cline to take it".to_string())
        } else {
            Some(keys_line(q))
        };
        let mut head: Vec<Line<'static>> = Vec::new();
        if let Some(h) = heading {
            if cols(&h) + 1 + cols(&numbers) <= width {
                head.push(Line::from(vec![
                    Span::styled(h, bold),
                    Span::raw(format!(" {numbers}")),
                ]));
            } else {
                head.push(Line::from(cut(&h, width)).style(bold));
                head.push(Line::from(cut(&numbers, width)));
            }
        }
        // What fits: the heading and the keys always; then the explanation (up to four lines), the files (when two rows are left)
        // and the last words with what remains. A cut paragraph ends with an ellipsis.
        let reserve = head.len() + usize::from(keys.is_some());
        let mut left = rows.saturating_sub(reserve);
        let info_n = info.len().min(4).min(left);
        left -= info_n;
        let files_n = usize::from(files.is_some() && left >= 2);
        let last_n = last.len().min(12).min(left - files_n);
        let mut lines: Vec<Line<'static>> = head;
        lines.extend(clipped(info, info_n, width));
        lines.extend(clipped(last, last_n, width));
        if files_n == 1
            && let Some(fl) = files
        {
            lines.push(Line::from(fl));
        }
        if let Some(k) = keys {
            lines.push(Line::from(cut(&k, width)).style(bold));
        }
        (title, lines)
    }

    /// The typing screen of an answer (`p`, `s`, or a model choice): the question in two lines, the text box, the state, the keys.
    fn draw_reply(&self, f: &mut Frame, split: &Split) {
        let Some(r) = &self.reply else {
            return;
        };
        let area = split.area;
        let card_h = if area.height >= 14 { 4 } else { 0 };
        let [title, card, body, status, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(card_h),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(area);
        let run = self.focused_run().unwrap_or_default();
        let what = match r.action {
            InstructAction::ContinueWithPrompt => "message to Cline",
            InstructAction::NewSessionWithPrompt => "fresh session",
            InstructAction::TryModel => "other model",
            InstructAction::Finish => "finish",
        };
        title_line(
            f,
            split.title_over(title),
            &format!("ham · Run {} · {what}", s(run)),
        );
        if card_h > 0 {
            let width = usize::from(card.width.saturating_sub(2));
            let mut lines = vec![
                Line::from(cut(&format!("Cline needs you: {}", s(&r.title)), width))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            ];
            if let Some(m) = r.last_words.as_deref() {
                let one = m.split_whitespace().collect::<Vec<_>>().join(" ");
                lines.push(Line::from(cut(&format!("Last words: {}", s(&one)), width)));
            }
            f.render_widget(Paragraph::new(lines).block(block("question", false)), card);
        }
        let box_title = match (&r.action, &r.model) {
            (InstructAction::ContinueWithPrompt, _) => {
                "Message to Cline, same session (Enter adds a line)".to_string()
            }
            (InstructAction::TryModel, Some((_, name))) => {
                format!("Switch to {} (Enter adds a line)", s(name))
            }
            _ => "Fresh session on the same files (Enter adds a line)".to_string(),
        };
        let inner_w = usize::from(body.width.saturating_sub(2)).max(1);
        let inner_h = usize::from(body.height.saturating_sub(2)).max(1);
        let mut shown: Vec<String> = Vec::new();
        for line in format!("{}{}", r.text, if r.sending { "" } else { "▌" }).split('\n') {
            shown.extend(wrap_hard(line, inner_w));
        }
        let skip = shown.len().saturating_sub(inner_h);
        let lines: Vec<Line> = shown.into_iter().skip(skip).map(Line::from).collect();
        f.render_widget(Paragraph::new(lines).block(block(&box_title, true)), body);
        let state = if let Some(why) = &r.closed {
            format!("this question is closed ({why}); your text is kept here. Esc goes back.")
        } else if r.sending {
            "sending…".to_string()
        } else if let Some(e) = &r.error {
            format!("error: {}", s(e))
        } else if r.action == InstructAction::ContinueWithPrompt {
            format!(
                "{} of {} bytes",
                group_digits(r.text.trim().len() as u64),
                group_digits(MAX_INPUT_TEXT_BYTES as u64)
            )
        } else {
            "Text is optional: Ctrl-S sends now, or type a message first.".to_string()
        };
        f.render_widget(Paragraph::new(state).wrap(Wrap { trim: false }), status);
        help_line(
            f,
            help,
            "Enter adds a line · Ctrl-S sends · Esc back; the question stays open",
        );
    }

    /// The models Cline may continue on (`m`): the models of the question, one highlighted.
    fn draw_models(&self, f: &mut Frame, split: &Split) {
        let Some(q) = self.question() else {
            return;
        };
        let area = split.area;
        let [title, body, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);
        title_line(
            f,
            split.title_over(title),
            &format!(
                "ham · Run {} · choose another model",
                s(self.focused_run().unwrap_or_default())
            ),
        );
        let width = usize::from(area.width.saturating_sub(2));
        let wide = area.width >= 100;
        let mut lines = vec![
            Line::from(cut(
                "Cline keeps its conversation and continues on the model you choose.",
                width,
            )),
            Line::from(""),
        ];
        for (i, m) in q.models.iter().enumerate() {
            let cursor = if i == self.model_cursor { "›" } else { " " };
            let id = if wide {
                format!("{:<36} ", s(&m.id))
            } else {
                String::new()
            };
            let text = format!(
                "{cursor} {:<20} {id}ctx {:>9}",
                s(&m.name),
                group_digits(u64::from(m.context_tokens))
            );
            let line = Line::from(cut(text.trim_end(), width));
            lines.push(if i == self.model_cursor {
                line.style(Style::default().add_modifier(Modifier::BOLD))
            } else {
                line
            });
        }
        f.render_widget(
            Paragraph::new(lines).block(block("Other model (↑↓ select · Enter choose)", true)),
            body,
        );
        help_line(f, help, "↑↓ select · Enter choose · Esc back");
    }

    /// The card with its `Clone` hint line replaced by what the clone is doing (`C` starts it).
    fn with_clone_state(&self, card: String, run_id: &str) -> String {
        let line = match &self.clone {
            CloneState::None => format!("{CLONE_LABEL}press C, or run: ham clone {run_id}"),
            CloneState::Pending => format!("{CLONE_LABEL}cloning…"),
            CloneState::Done(c) => format!(
                "{CLONE_LABEL}{} (branch {}, {} files, tree equals src/){}",
                c.dir.display(),
                c.branch,
                c.files,
                if c.partial { " PARTIAL delivery" } else { "" }
            ),
            CloneState::Failed(e) => format!("{CLONE_LABEL}failed: {e}"),
        };
        card.lines()
            .map(|l| {
                if l.starts_with(CLONE_LABEL) {
                    line.as_str()
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw_artifacts(&self, f: &mut Frame, split: &Split) {
        let run = self.focused_run().unwrap_or_default().to_string();
        let [title, body, status, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(split.area);
        title_line(
            f,
            split.title_over(title),
            &format!("ham · Artifacts {}", s(&run)),
        );
        let longest_id = match &self.artifacts {
            ArtifactList::Loaded(list) => list
                .iter()
                .map(|a| cols(&s(&a.artifact_id)))
                .max()
                .unwrap_or(0),
            _ => 0,
        };
        let (id_width, file_min) =
            artifact_columns(usize::from(body.width.saturating_sub(2)), longest_id);
        let (rows, count, note): (Vec<Row>, usize, String) = match &self.artifacts {
            ArtifactList::Loaded(list) => (
                list.iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let mark = if i == self.artifact_cursor {
                            "▶ "
                        } else {
                            "  "
                        };
                        Row::new(vec![
                            Cell::from(format!("{mark}{}", enum_text(&a.kind))),
                            Cell::from(cut_cols(&s(&a.artifact_id), id_width)),
                            Cell::from(size_text(a.size_bytes)),
                            Cell::from(s(&a.sha256).chars().take(12).collect::<String>()),
                            Cell::from(s(&local_name(&run, a))),
                        ])
                    })
                    .collect(),
                list.len(),
                if list.is_empty() {
                    "the run has no artifacts yet".into()
                } else {
                    String::new()
                },
            ),
            ArtifactList::Loading | ArtifactList::NotLoaded => {
                (vec![], 0, "loading the artifact list".into())
            }
            ArtifactList::Failed(e) => (vec![], 0, format!("cannot list artifacts: {}", s(e))),
        };
        f.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(12),
                    Constraint::Length(u16::try_from(id_width).unwrap_or(u16::MAX)),
                    Constraint::Length(10),
                    Constraint::Length(13),
                    Constraint::Min(u16::try_from(file_min).unwrap_or(u16::MAX)),
                ],
            )
            .header(Row::new(vec![
                "  KIND", "ARTIFACT", "SIZE", "SHA-256", "FILE",
            ]))
            .block(block(&format!("artifacts ({count})"), false)),
            body,
        );
        let line = match &self.artifact_download {
            Some((name, DownloadState::Pending)) => format!("downloading {}…", s(name)),
            Some((_, DownloadState::Done(p))) => {
                format!(
                    "saved {} (sha256 matched)",
                    s(&p.path.display().to_string())
                )
            }
            Some((_, DownloadState::Failed(e))) => format!("download failed: {}", s(e)),
            _ => note,
        };
        f.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), status);
        help_line(
            f,
            help,
            "↑↓ select · Enter download · r reload · Esc dashboard · q quit",
        );
    }
}

/// What the outage card says: it asks for nothing and has no keys.
const OUTAGE_TEXT: &str =
    "The run keeps probing until its time limit. c c stops it and delivers what exists.";

/// Terminal columns of `text`.
fn cols(text: &str) -> usize {
    Span::raw(text.to_string()).width()
}

/// `line` split into pieces of at most `width` columns, keeping its spaces (the typing screen).
fn wrap_hard(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for c in line.chars() {
        let w = Span::raw(c.to_string()).width();
        if used + w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(c);
        used += w;
    }
    out.push(cur);
    out
}

/// `text` wrapped at word boundaries to `width` columns; a word longer than a line is split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let sep = usize::from(!cur.is_empty());
        if cols(&cur) + sep + cols(word) <= width {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if cols(word) <= width {
            cur.push_str(word);
        } else {
            let mut pieces = wrap_hard(word, width);
            cur = pieces.pop().unwrap_or_default();
            out.extend(pieces);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The first `n` of `lines`; when some were left out the last one ends with an ellipsis.
fn clipped(lines: Vec<String>, n: usize, width: usize) -> Vec<Line<'static>> {
    let cut_short = lines.len() > n;
    let mut out: Vec<String> = lines.into_iter().take(n).collect();
    if cut_short && let Some(last) = out.last_mut() {
        *last = cut(&format!("{last}…"), width);
    }
    out.into_iter().map(Line::from).collect()
}

/// `Files: a b c (+N more)`: as many of the question's paths as fit, and how many more the run changed.
fn files_line(q: &InputRequest, width: usize) -> String {
    let total = q.files_total as usize;
    let mut line = String::from("Files:");
    let mut shown = 0;
    for p in &q.files {
        let next = format!("{line} {}", s(p));
        let rest = total.saturating_sub(shown + 1);
        let tail = if rest > 0 {
            format!(" (+{rest} more)")
        } else {
            String::new()
        };
        if shown > 0 && cols(&next) + cols(&tail) > width {
            break;
        }
        line = next;
        shown += 1;
    }
    let more = total.saturating_sub(shown);
    if more > 0 {
        line.push_str(&format!(" (+{more} more)"));
    }
    cut(&line, width)
}

/// The keys a question offers, as the card and the notice row print them.
fn keys_line(q: &InputRequest) -> String {
    let mut parts = Vec::new();
    for (action, text) in [
        (InstructAction::ContinueWithPrompt, "p message"),
        (InstructAction::NewSessionWithPrompt, "s fresh session"),
        (InstructAction::TryModel, "m other model"),
        (InstructAction::Finish, "f f finish"),
    ] {
        if refusal(q, action).is_none() {
            parts.push(text);
        }
    }
    parts.join(" · ")
}

/// The Runs screen's STATUS cell: a run that waits for an answer says so instead of "active".
/// The key a part of a key line names and the name to show for it: `Enter`, `Esc`, `Tab`, `Shift-Tab`, or one letter, digit or slash that
/// is followed by words (`a artifacts`).
fn chip_key(part: &str) -> Option<(KeyCode, &'static str)> {
    const NAMES: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/";
    let lead = part.split(' ').next()?;
    match lead {
        "Enter" => return Some((KeyCode::Enter, "Enter")),
        "Esc" => return Some((KeyCode::Esc, "Esc")),
        // The tag explorer's `Space pick`: the bar is a key that has a name and a word after it.
        "Space" if part.contains(' ') => return Some((KeyCode::Char(' '), "Space")),
        "Tab" => return Some((KeyCode::Tab, "Tab")),
        "Shift-Tab" => return Some((KeyCode::BackTab, "Shift-Tab")),
        _ => {}
    }
    let mut chars = lead.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !part.contains(' ') {
        return None;
    }
    let at = NAMES.find(c)?;
    Some((KeyCode::Char(c), &NAMES[at..at + 1]))
}

fn status_cell(
    r: &harness_protocol::api::v1::RunSummary,
    filter: &ham_core::runfilter::RunsFilter,
) -> Cell<'static> {
    if r.needs_input.is_some() {
        let text = "needs input";
        Cell::from(marked_line(text, &filter.highlight(text), 0))
            .style(Style::default().add_modifier(Modifier::BOLD))
    } else {
        let text = enum_text(&r.status);
        let cell = Cell::from(marked_line(&text, &filter.highlight(&text), 0));
        // The two stops that can be resumed carry their gutter's colour.
        match r.status {
            harness_protocol::goal::GoalStatus::Paused => {
                cell.style(Style::default().fg(ratatui::style::Color::Yellow))
            }
            harness_protocol::goal::GoalStatus::Stalled => {
                cell.style(Style::default().fg(ratatui::style::Color::Magenta))
            }
            _ => cell,
        }
    }
}

/// The Runs screen's NAME cell, `width` columns wide: the run's name cut by display width with an ellipsis, plain when a model wrote
/// it, dim when it is a seed (or the title of an older backend's row), and a dim `-` for a run with neither. The characters the filter found
/// are underlined, the ellipsis never.
fn name_cell(
    r: &ham_core::runfilter::RunRow,
    filter: &ham_core::runfilter::RunsFilter,
    width: usize,
) -> Cell<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    // A run that continues another starts with the mark ↳, which the filter never marks.
    let mark = if r.continue_from.is_some() {
        ham_core::retry::LINK_MARK
    } else {
        ""
    };
    let skip = mark.chars().count();
    let Some(label) = r.label().map(s).filter(|t| !t.trim().is_empty()) else {
        return Cell::from(format!("{mark}-")).style(dim);
    };
    let ranges = filter.highlight(&label);
    let text = format!("{mark}{label}");
    let drawn = cut_cols(&text, width);
    let ranges = if drawn == text {
        ranges
    } else {
        // The last character of a cut text is the ellipsis: no mark reaches it, and none lies beyond it. The ranges count from the label, after
        // the mark.
        let kept = drawn.chars().count().saturating_sub(1 + skip);
        ranges
            .into_iter()
            .filter(|m| m.start < kept)
            .map(|m| m.start..m.end.min(kept))
            .collect()
    };
    let cell = Cell::from(marked_line(&drawn, &ranges, skip));
    let by_a_model =
        r.name.is_some() && r.name_source.as_deref() == Some(ham_core::run_index::NAME_SOURCE_AI);
    if by_a_model { cell } else { cell.style(dim) }
}

/// The note under the Runs table for the columns the width leaves out: the selected run's target, phase and creation time, in
/// that order, each with its word, so nothing the wide table shows is lost on a narrow one.
fn hidden_columns(plan: &RunsColumns, r: &harness_protocol::api::v1::RunSummary) -> String {
    let mut out = String::new();
    if !plan.target {
        out.push_str(&format!(" · target {}", target_text(&r.target)));
    }
    if !plan.phase
        && let Some(phase) = phase_shown(Some(r.status), Some(r.phase))
    {
        out.push_str(&format!(" · phase {phase}"));
    }
    if !plan.created {
        out.push_str(&format!(" · created {}", s(&r.created_at)));
    }
    out
}

/// The one-character gutter of a run's row: `>` running, `!` waiting for the user (bold), `x` failed (red), `=` paused (yellow) and `~`
/// stalled (magenta); the last two are the runs `R` resumes.
fn gutter_cell(gutter: ham_core::runfilter::Gutter) -> Cell<'static> {
    use ham_core::runfilter::Gutter;
    let style = match gutter {
        Gutter::NeedsYou => Style::default().add_modifier(Modifier::BOLD),
        Gutter::Failed => Style::default().fg(ratatui::style::Color::Red),
        Gutter::Paused => Style::default().fg(ratatui::style::Color::Yellow),
        Gutter::Stalled => Style::default().fg(ratatui::style::Color::Magenta),
        Gutter::Running | Gutter::None => Style::default(),
    };
    Cell::from(gutter.glyph()).style(style)
}

/// `text` as one line with the characters inside `ranges` (offset by `skip` leading characters that are never marked) underlined.
fn marked_line(text: &str, ranges: &[std::ops::Range<usize>], skip: usize) -> Line<'static> {
    if ranges.is_empty() {
        return Line::from(text.to_string());
    }
    let mark = Style::default().add_modifier(Modifier::UNDERLINED);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_marked = false;
    for (i, c) in text.chars().enumerate() {
        let marked = i >= skip && ranges.iter().any(|r| r.contains(&(i - skip)));
        if marked != run_marked && !run.is_empty() {
            let style = if run_marked { mark } else { Style::default() };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_marked = marked;
        run.push(c);
    }
    if !run.is_empty() {
        let style = if run_marked { mark } else { Style::default() };
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

/// The words ` · tags a b c` for the tags of the selected run, each cleaned like every string from the API; empty without a tag.
fn runs_note_tags(tags: &[String]) -> String {
    let cleaned: Vec<String> = tags
        .iter()
        .map(|t| s(t))
        .filter(|t| !t.is_empty())
        .collect();
    if cleaned.is_empty() {
        String::new()
    } else {
        format!(" · tags {}", cleaned.join(" "))
    }
}

/// The Runs screen's note line: the count, how many runs wait for an answer, and what the selected one waits for.
fn runs_note(runs: &[harness_protocol::api::v1::RunSummary], selected: usize) -> String {
    let mut note = format!("{} runs", runs.len());
    let waiting = runs.iter().filter(|r| r.needs_input.is_some()).count();
    if waiting > 0 {
        let verb = if waiting == 1 { "needs" } else { "need" };
        note.push_str(&format!(" · {waiting} {verb} input"));
    }
    // The runs `R` resumes: paused at their total time or stalled after they lost their progress.
    let resumable = ham_core::resume::plan(runs).resume.len();
    if resumable > 0 {
        note.push_str(&format!(" · {resumable} to resume (R)"));
    }
    if let Some(n) = runs.get(selected).and_then(|r| r.needs_input.as_ref()) {
        note.push_str(&format!(" · needs input: {}", s(&n.title)));
    }
    // Why the selected run stopped, in the service's own words and the session it ended in.
    if let Some(r) = runs.get(selected)
        && matches!(
            r.status,
            harness_protocol::goal::GoalStatus::Paused
                | harness_protocol::goal::GoalStatus::Stalled
        )
    {
        let word = enum_text(&r.status);
        match r.status_reason.as_deref() {
            // The reason is `code: sentence`; the screen says the sentence.
            Some(reason) => {
                let sentence = reason.split_once(": ").map_or(reason, |(_, t)| t);
                note.push_str(&format!(" · {word}: {}", s(sentence)));
            }
            None => note.push_str(&format!(" · {word}")),
        }
        if let Some(n) = r.session.filter(|n| *n > 1) {
            note.push_str(&format!(" · session {n}"));
        }
    }
    note
}

/// `text` cut to `width` terminal columns and padded to it.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = Span::raw(c.to_string()).width();
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(width - used));
    out
}

/// The widths of the ARTIFACT and FILE columns of the Artifacts table, which is `inner` columns wide (inside its border) and whose longest
/// artifact id is `longest` columns. The ARTIFACT column fits the longest id (the packager's `evidence-` and 20 hex digits are 29
/// characters) and is never narrower than the 28 columns it had before. When the terminal has not the room the FILE column gives way
/// first, down to 4 columns, and only then the id is cut (with a `…`, see [`cut_cols`]).
fn artifact_columns(inner: usize, longest: usize) -> (usize, usize) {
    // KIND, SIZE and SHA-256 and the four gaps between the five columns.
    const FIXED: usize = 12 + 10 + 13 + 4;
    const ID_MIN: usize = 28;
    const ID_LEAST: usize = 8;
    const FILE_MIN: usize = 10;
    const FILE_LEAST: usize = 4;
    let want = longest.max(ID_MIN);
    let room = inner.saturating_sub(FIXED);
    if room >= want + FILE_MIN {
        return (want, FILE_MIN);
    }
    let file = room.saturating_sub(want).clamp(FILE_LEAST, FILE_MIN);
    let id = room.saturating_sub(file).clamp(ID_LEAST, want);
    (id, file)
}

/// `text` in at most `width` terminal columns: whole when it fits, else its start and a `…` as the last column, so a cut is never silent.
fn cut_cols(text: &str, width: usize) -> String {
    if cols(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = Span::raw(c.to_string()).width();
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn size_text(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let x = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if x < KIB * KIB {
        format!("{:.1} KiB", x / KIB)
    } else {
        format!("{:.1} MiB", x / (KIB * KIB))
    }
}

/// The ended-early panel: the root execution's end and its cause.
fn aborted_lines(v: &RunView) -> Vec<Line<'static>> {
    let Some(a) = &v.aborted else {
        return vec![];
    };
    let mut head = s(&a.status);
    if let Some(e) = &a.error {
        head.push(' ');
        head.push_str(&s(e));
    }
    if let Some(at) = &a.stopped_at {
        head.push_str(&format!(" at {}", s(at)));
    }
    let mut lines = vec![Line::from(head)];
    if let Some(c) = &a.cause {
        lines.push(Line::from(format!("cause: {}", s(c))));
    }
    lines
}

/// The blocked panel: the reason the harness gave.
fn blocked_lines(v: &RunView) -> Vec<Line<'static>> {
    let Some(b) = &v.blocked else {
        return vec![];
    };
    vec![Line::from(format!("reason: {}", s(&b.reason)))]
}

fn header_lines(v: &RunView, width: usize, fallback: Option<&str>) -> Vec<Line<'static>> {
    // A direct run has no gates, tests or verdicts: its status block names the harness's label.
    let status = v
        .status
        .as_ref()
        .map_or_else(|| "unknown".into(), enum_text);
    // A run that is over names its status alone: its phase is only the last one the service wrote (`phase_shown`).
    let mut first = match v.phase_now() {
        Some(phase) => format!("{status} ({phase})"),
        None => status,
    };
    if v.untrusted > 0 {
        first.push_str(&format!("  ignored {} forged events", v.untrusted));
    }
    let mut lines = vec![Line::from(first)];
    if let Some(id) = v.direct_model_id().or(fallback) {
        lines.push(Line::from(cut(
            &format!("Model  {}", s(&ham_core::render::model_name(id))),
            width,
        )));
    }
    // While the run goes on, a switch or a trouble with the model calls says where it is, under the model the user asked for.
    if let Some(route) = v.route_now() {
        lines.push(Line::from(format!("Route  {}", s(&route))));
    }
    // A run that switched model or route says where it ended, under the model the user asked for.
    if let Some(e) = ham_core::render::ended_on(v) {
        lines.push(Line::from(cut(&s(&e.row()), width)));
    }
    // A run that started a fresh Cline session says so: the work tree is the same, the conversation is new.
    if v.sessions > 1 {
        let line = v.restarted.as_ref().map_or_else(
            || format!("Session {} · restarted from the work tree", v.sessions),
            ham_core::input::RestartNote::line,
        );
        lines.push(Line::from(cut(&s(&line), width)));
    }
    if let Some(r) = &v.result {
        // The whole label: a harness stop names its reason in it, and the card wraps the line.
        lines.push(Line::from(format!("Outcome {}", s(&r.label))));
        // The facts the harness read at the end of the run, one line each directly under the label; the warning marker for a
        // red last log entry.
        for o in &v.observations {
            let mark = if o.warns() { "! " } else { "" };
            lines.push(Line::from(format!("  {mark}{}", s(&o.text))));
        }
    }
    lines.push(Line::from(format!(
        "PR     {}",
        v.pr_url.as_deref().map_or_else(|| "none yet".into(), s)
    )));
    let windows = v.windows();
    if !windows.is_empty() {
        let text = windows
            .iter()
            .map(|(m, t)| format!("{} {}", s(m), group_digits(u64::from(*t))))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(Line::from(cut(&format!("window  {text}"), width)));
    }
    lines
}
