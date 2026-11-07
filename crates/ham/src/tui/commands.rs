//! The command flow of the message box: the parts of it that hold no `App`. A line sent from the
//! box goes through `ham_core::router::route`; what comes back is acted on in `app.rs`, which keeps three pieces of state from here:
//!
//! - [`Armed`]: a preview waiting for its second Enter. It owns the router's `Confirmation` (so the argv that was previewed is the argv
//!   that runs), the time it was shown, the revision of the config file it was shown under, and it draws itself above the box.
//! - [`Panel`]: the text a command wrote (or why a line was not run), sanitized, scrolled with the arrow keys, closed with Esc. A
//!   bearer link that came with the text is kept apart in a [`Bearer`], never drawn, and shown to nobody but the user's own `o` key.
//! - [`config_revision`]: a number that changes when the config file does, so a preview can tell that the settings moved under it.
//!
//! Nothing here runs a command or touches the terminal modes. A command that needs the terminal (an editor, git's prompts) is run by the
//! event loop through [`handoff`](super::handoff), and what it wrote comes back here as a [`CmdOutput`] for a [`Panel`].

use super::input::{ChatBox, Kind};
use crate::output::CmdOutput;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham_core::command::{Command, ConfigCommand, Invocation};
use ham_core::push::default_branch;
use ham_core::router::{
    CONFIRM_DELAY_MS, ConfirmKey, ConfirmStep, Confirmation, Preview, RouteOutcome,
};
use ham_core::sanitize::sanitize_line;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::cell::Cell;

/// A bearer link (the presigned URL of `ham link`). It is held for the user's own `o` key and never drawn or printed: its `Debug` text
/// is a placeholder, so a dump of the app, an effect log or a failed assertion cannot carry it.
#[derive(Clone, PartialEq, Eq)]
pub struct Bearer(String);

impl Bearer {
    pub fn new(url: String) -> Self {
        Self(url)
    }

    /// The link, for the opener only.
    pub fn url(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Bearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bearer(<hidden>)")
    }
}

/// How the panel's command stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running,
    /// The command ended with this exit code.
    Done(i32),
    /// The line was not run (a refusal); the text says why.
    NotRun,
    /// Plain information (`/help`).
    Info,
}

/// The result panel above the message box.
#[derive(Clone, Debug)]
pub struct Panel {
    title: String,
    lines: Vec<String>,
    state: State,
    bearer: Option<Bearer>,
    /// The first row shown, in displayed rows.
    scroll: usize,
    /// Long lines wrap (a refusal) rather than being cut at the edge (a table).
    wrap: bool,
    /// The rows and the total at the last draw, for the scroll keys.
    rows: Cell<usize>,
    total: Cell<usize>,
}

/// The words of a command as a panel title shows them: `ham cancel 01K6…`.
pub fn command_title(argv: &[String]) -> String {
    let words: Vec<String> = argv.iter().map(|w| sanitize_line(w)).collect();
    format!("ham {}", words.join(" "))
}

impl Panel {
    fn new(title: String, lines: Vec<String>, state: State, wrap: bool) -> Self {
        Self {
            title,
            lines,
            state,
            bearer: None,
            scroll: 0,
            wrap,
            rows: Cell::new(0),
            total: Cell::new(0),
        }
    }

    /// A command that was sent and has not answered.
    pub fn running(argv: &[String]) -> Self {
        Self::new(command_title(argv), vec![], State::Running, false)
    }

    /// What a command wrote. Its lines are sanitized here; a bearer link is kept apart; a setting that changed says that the running
    /// app keeps the old one until it restarts.
    pub fn done(argv: &[String], out: &CmdOutput) -> Self {
        let mut lines: Vec<String> = out.lines().iter().map(|l| sanitize_line(l)).collect();
        if out.code == 0 && changes_a_setting(argv) {
            lines.push("The running app keeps its old settings until you restart ham.".to_string());
        }
        let mut panel = Self::new(command_title(argv), lines, State::Done(out.code), false);
        panel.bearer = out.bearer.clone().map(Bearer::new);
        panel
    }

    /// A line that was not run, and why.
    pub fn not_run(title: &str, reason: &str) -> Self {
        let lines = reason.lines().map(sanitize_line).collect();
        Self::new(sanitize_line(title), lines, State::NotRun, true)
    }

    /// Text for the person to read, such as `/help`.
    pub fn info(title: &str, text: &str) -> Self {
        let lines = text.lines().map(sanitize_line).collect();
        Self::new(sanitize_line(title), lines, State::Info, false)
    }

    /// The panel's title, as drawn between the border's corners.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The bearer link of the command, for the `o` key.
    pub fn bearer(&self) -> Option<&Bearer> {
        self.bearer.as_ref()
    }

    /// True when the panel has more rows than it showed at the last draw, so the arrow keys scroll it.
    pub fn overflows(&self) -> bool {
        self.total.get() > self.rows.get()
    }

    /// Scrolls by `rows` (negative goes up), within the text.
    pub fn scroll_by(&mut self, rows: isize) {
        let last = self.total.get().saturating_sub(self.rows.get());
        let to = if rows < 0 {
            self.scroll.saturating_sub(rows.unsigned_abs())
        } else {
            self.scroll.saturating_add(rows.unsigned_abs())
        };
        self.scroll = to.min(last);
    }

    /// One page: the rows shown at the last draw.
    pub fn page(&self) -> isize {
        isize::try_from(self.rows.get().max(1)).unwrap_or(1)
    }

    /// The displayed rows at `width` columns: wrapped or cut.
    fn displayed(&self, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for line in &self.lines {
            if self.wrap {
                rows.extend(wrap_text(line, width));
            } else {
                rows.push(cut(line, width));
            }
        }
        rows
    }

    /// The height the panel asks for in a screen `area`: its rows and the border, at least 3, at most two fifths of the screen.
    pub fn height(&self, area: Rect) -> u16 {
        let inner = usize::from(area.width.saturating_sub(2)).max(1);
        let wanted = self.displayed(inner).len().max(1) + 2;
        let most = usize::from((area.height * 2 / 5).max(5));
        u16::try_from(wanted.clamp(3, most)).unwrap_or(3)
    }

    fn title_text(&self) -> String {
        match self.state {
            State::Running => format!(" {} · running… ", self.title),
            State::Done(0) | State::Info => format!(" {} ", self.title),
            State::Done(code) => format!(" {} · exit {code} ", self.title),
            State::NotRun => format!(" {} ", self.title),
        }
    }

    /// Draws the panel in `area`. `typing` says whether Esc first leaves typing (so closing takes two).
    pub fn draw(&self, f: &mut Frame, area: Rect, typing: bool) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        f.render_widget(Clear, area);
        let inner_w = usize::from(area.width.saturating_sub(2)).max(1);
        let rows = self.displayed(inner_w);
        let shown = usize::from(area.height.saturating_sub(2)).max(1);
        self.rows.set(shown);
        self.total.set(rows.len());
        let start = self.scroll.min(rows.len().saturating_sub(shown));
        let close = if typing {
            "Esc Esc closes"
        } else {
            "Esc closes"
        };
        // The arrow keys scroll the panel in browse mode; while the box has the focus they belong to the box.
        let scroll = if typing {
            "PgUp PgDn scroll"
        } else {
            "↑↓ PgUp PgDn scroll"
        };
        let hint = if self.bearer.is_some() {
            format!(" o opens the link · {close} ")
        } else if rows.len() > shown {
            format!(
                " {}-{} of {} · {scroll} · {close} ",
                start + 1,
                (start + shown).min(rows.len()),
                rows.len()
            )
        } else {
            format!(" {close} ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title_text())
            .title_bottom(hint);
        let body: Vec<Line> = rows
            .iter()
            .skip(start)
            .take(shown)
            .map(|r| Line::from(r.clone()))
            .collect();
        f.render_widget(Paragraph::new(body).block(block), area);
    }
}

/// True for `config set` or `config unset`: the running app read its settings when it started and keeps them until it restarts.
fn changes_a_setting(argv: &[String]) -> bool {
    matches!(
        (
            argv.first().map(String::as_str),
            argv.get(1).map(String::as_str)
        ),
        (Some("config"), Some("set" | "unset" | "edit"))
    )
}

/// The preview of a `config set` that has none of its own but would add the first table to `config.toml`: the one write that older builds
/// answer by refusing the file (the one-way door). The note is shown before the write, the person confirms with Enter, and nothing is written by
/// showing it. `None` for every other line: a legacy key, an unset, a value the command will refuse, a file it cannot edit, a table that is
/// already there, a key that has a preview already (a security setting), or a line that is not a `config set`.
pub fn door_preview(
    outcome: &RouteOutcome,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<RouteOutcome> {
    let RouteOutcome::Command {
        argv,
        invocation,
        class,
        preview: None,
    } = outcome
    else {
        return None;
    };
    let Command::Config(ConfigCommand::Set { key, value }) = &invocation.command else {
        return None;
    };
    let note = crate::config_cmd::door_note_for(env, key, value)?;
    Some(RouteOutcome::Command {
        argv: argv.clone(),
        invocation: invocation.clone(),
        class: *class,
        preview: Some(Preview {
            text: door_text(&note),
            confirm_with: None,
        }),
    })
}

/// The preview of a typed follow-up with the target the screen already knows: the source run's summary is on the screen (the run
/// on screen, or a row of the run list) and a run's target does not change. Only the words of the preview change: the argv stays what was
/// typed, so the child `ham continue` resolves the target as the shell does and prints the same lines. `None` for every other line, for a
/// follow-up whose line names its target, and when the screen does not know the run's target (the preview then says it is read when the
/// run starts).
pub fn name_continue_target(
    outcome: &RouteOutcome,
    known: &dyn Fn(&str) -> Option<harness_protocol::target::RunTarget>,
) -> Option<RouteOutcome> {
    let RouteOutcome::Command {
        argv,
        invocation,
        class,
        preview: Some(preview),
    } = outcome
    else {
        return None;
    };
    let Command::Continue(d, None) = &invocation.command else {
        return None;
    };
    let run = d.continue_from.as_deref()?;
    let target = ham_core::followup::follow_up_target(&known(run)?, run).ok()?;
    let text = ham_core::router::continue_preview_text(invocation, Some(&target))?;
    Some(RouteOutcome::Command {
        argv: argv.clone(),
        invocation: invocation.clone(),
        class: *class,
        preview: Some(Preview {
            text,
            confirm_with: preview.confirm_with.clone(),
        }),
    })
}

/// The line of a preview that shows the one-way-door note (`ham_core::config_write::door_note`): the message box and the Settings screen show
/// the same words before the first write that adds a table to `config.toml`.
pub fn door_text(note: &str) -> String {
    format!("This adds the first table to config.toml. {note}")
}

/// `text` cut to `width` columns, with an ellipsis when it was cut.
fn cut(text: &str, width: usize) -> String {
    if Span::raw(text).width() <= width {
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

/// `text` wrapped at word boundaries to `width` columns; a word longer than a row is split at the edge. Leading spaces stay as the
/// indent of every row. Always at least one row.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let body = text.trim_start_matches(' ');
    let indent = text.len() - body.len();
    // An indent that leaves no room is dropped.
    let indent = if indent + 1 < width { indent } else { 0 };
    let pad = " ".repeat(indent);
    wrap_words(body, width.saturating_sub(indent))
        .into_iter()
        .map(|r| format!("{pad}{r}"))
        .collect()
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for word in text.split(' ') {
        let w = Span::raw(word).width();
        let sep = usize::from(!cur.is_empty());
        if used + sep + w <= width {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            used += sep + w;
            continue;
        }
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            used = 0;
        }
        if w <= width {
            cur.push_str(word);
            used = w;
            continue;
        }
        for c in word.chars() {
            let cw = Span::raw(c.to_string()).width();
            if used + cw > width {
                rows.push(std::mem::take(&mut cur));
                used = 0;
            }
            cur.push(c);
            used += cw;
        }
    }
    rows.push(cur);
    rows
}

/// What a preview waits for.
#[derive(Clone, Debug)]
enum What {
    /// A second key. `gesture` is the hotkey that gives it when the preview came from one (`c` of `c c`): the typed line takes a second
    /// Enter, the hotkey takes the same key again, and nothing else confirms.
    Confirm {
        confirmation: Box<Confirmation>,
        gesture: Option<Gesture>,
    },
    /// The one argument the line lacked, typed into a slot: the git URL of `push`.
    Ask(Box<Ask>),
}

/// The hotkey of a preview made by a key press, and the words that tell the person what the key does.
#[derive(Clone, Debug)]
pub struct Gesture {
    /// The key that confirms (`c`).
    pub key: char,
    /// The last line of the preview: `press c again to cancel run <id>`.
    pub line: String,
}

/// The line a hotkey preview adds when its key came again before the preview was [`CONFIRM_DELAY_MS`] old: the press is ignored on
/// purpose (a key held down must not confirm), and the person is told so instead of finding the press gone.
fn too_soon_line(key: char) -> String {
    let tenths = CONFIRM_DELAY_MS / 100;
    format!(
        "too soon: a second {key} within {}.{} s of the first is ignored; press {key} again",
        tenths / 10,
        tenths % 10
    )
}

/// A preview waiting for its second key, or for the argument it lacks.
#[derive(Clone, Debug)]
pub struct Armed {
    what: What,
    /// The revision of the config file the preview was made under.
    rev: u64,
    /// A hotkey's second press came before the preview was [`CONFIRM_DELAY_MS`] old and was ignored: the preview says so.
    early: bool,
}

impl Armed {
    /// A preview for a [`RouteOutcome::Command`] that has one; `None` for any other outcome.
    pub fn new(outcome: &RouteOutcome, shown_ms: u64, rev: u64) -> Option<Self> {
        Confirmation::new(outcome, shown_ms).map(|confirmation| Self {
            what: What::Confirm {
                confirmation: Box::new(confirmation),
                gesture: None,
            },
            rev,
            early: false,
        })
    }

    /// The preview a hotkey makes: the same prepared command as the typed line, confirmed by the same key again once
    /// 300 ms have passed.
    pub fn hotkey(
        outcome: &RouteOutcome,
        shown_ms: u64,
        rev: u64,
        gesture: Gesture,
    ) -> Option<Self> {
        Confirmation::new(outcome, shown_ms).map(|confirmation| Self {
            what: What::Confirm {
                confirmation: Box::new(confirmation),
                gesture: Some(gesture),
            },
            rev,
            early: false,
        })
    }

    /// The preview of a command that lacks `missing` (`git-url`): a slot to type it in. `argv` is what is known.
    pub fn ask(argv: Vec<String>, missing: &'static str, rev: u64) -> Self {
        Self {
            what: What::Ask(Box::new(Ask::new(argv, missing))),
            rev,
            early: false,
        }
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// True while the preview waits for an argument typed into its slot.
    pub fn is_ask(&self) -> bool {
        matches!(self.what, What::Ask(_))
    }

    /// True when the terminal's cursor belongs to the preview: it waits for typed characters (a slot, or the last four characters of a push).
    pub fn wants_cursor(&self) -> bool {
        self.is_ask() || self.expected().is_some()
    }

    /// The hotkey that confirms the preview, if a hotkey made it.
    pub fn gesture_key(&self) -> Option<char> {
        match &self.what {
            What::Confirm { gesture, .. } => gesture.as_ref().map(|g| g.key),
            What::Ask(_) => None,
        }
    }

    /// What must be typed before Enter, if anything.
    pub fn expected(&self) -> Option<&str> {
        match &self.what {
            What::Confirm { confirmation, .. } => confirmation.expected(),
            What::Ask(_) => None,
        }
    }

    pub fn argv(&self) -> &[String] {
        match &self.what {
            What::Confirm { confirmation, .. } => confirmation.argv(),
            What::Ask(ask) => &ask.argv,
        }
    }

    /// The command that was previewed; `None` while the preview still waits for its argument.
    pub fn invocation(&self) -> Option<&Invocation> {
        match &self.what {
            What::Confirm { confirmation, .. } => Some(confirmation.invocation()),
            What::Ask(_) => None,
        }
    }

    /// One key at `now_ms` on the app's clock. A preview that waits for an argument takes no confirming key.
    pub fn press(&mut self, key: ConfirmKey, now_ms: u64) -> ConfirmStep {
        let (step, hotkey) = match &mut self.what {
            What::Confirm {
                confirmation,
                gesture,
            } => (confirmation.press(key, now_ms), gesture.is_some()),
            What::Ask(_) => (ConfirmStep::Waiting(ham_core::router::Wait::Idle), false),
        };
        // The preview of a hotkey says that its second press came too soon; the preview of a typed line is as it was.
        if hotkey && step == ConfirmStep::Waiting(ham_core::router::Wait::TooSoon) {
            self.early = true;
        }
        step
    }

    /// An editing key for the slot. False when the preview has no slot.
    pub fn slot_key(&mut self, key: KeyEvent) -> bool {
        match &mut self.what {
            What::Ask(ask) => {
                ask.edit(key);
                true
            }
            What::Confirm { .. } => false,
        }
    }

    /// Text pasted into the slot (white space is dropped by the slot). False when the preview has no slot.
    pub fn slot_paste(&mut self, text: &str) -> bool {
        match &mut self.what {
            What::Ask(ask) => {
                ask.slot.insert_str(text);
                ask.refusal = None;
                true
            }
            What::Confirm { .. } => false,
        }
    }

    /// The argv that is known and what was typed in the slot, for [`ham_core::router::complete`].
    pub fn answer(&self) -> Option<(&[String], String)> {
        match &self.what {
            What::Ask(ask) => Some((&ask.argv, ask.slot.text())),
            What::Confirm { .. } => None,
        }
    }

    /// Why the last Enter on the slot was refused; the slot stays open and the reason is drawn in it.
    pub fn refuse(&mut self, reason: &str) {
        if let What::Ask(ask) = &mut self.what {
            ask.refusal = Some(sanitize_line(reason));
        }
    }

    fn lines(&self, width: usize) -> Vec<String> {
        match &self.what {
            What::Ask(ask) => ask.lines(width),
            What::Confirm {
                confirmation,
                gesture,
            } => {
                let mut lines = wrap_text(&sanitize_line(&confirmation.preview().text), width);
                match (confirmation.expected(), gesture) {
                    (Some(want), _) => lines.extend(wrap_text(
                        &format!(
                            "Type {}, then press Enter · Esc cancels",
                            sanitize_line(want)
                        ),
                        width,
                    )),
                    (None, Some(g)) => {
                        lines.extend(wrap_text(
                            &format!("{} · Esc withdraws it", sanitize_line(&g.line)),
                            width,
                        ));
                        if self.early {
                            lines.extend(wrap_text(&too_soon_line(g.key), width));
                        }
                    }
                    (None, None) => lines.push("Press Enter to run it · Esc cancels".to_string()),
                }
                lines
            }
        }
    }

    /// The height the preview asks for at `width` columns: its rows, the answer line (or the slot) when one is asked for, and the border.
    pub fn height(&self, width: u16) -> u16 {
        let inner = usize::from(width.saturating_sub(2)).max(1);
        match &self.what {
            What::Ask(ask) => {
                let rows = ask.lines(inner).len() + SLOT_ROWS;
                u16::try_from((rows + 2).min(11)).unwrap_or(11)
            }
            What::Confirm { confirmation, .. } => {
                let rows = self.lines(inner).len() + usize::from(confirmation.expected().is_some());
                u16::try_from((rows + 2).min(8)).unwrap_or(8)
            }
        }
    }

    /// Draws the preview in `area`. When it asks for a typed answer, the terminal's cursor goes to the end of what was typed (the slot: to
    /// its caret).
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        f.render_widget(Clear, area);
        let inner_w = usize::from(area.width.saturating_sub(2)).max(1);
        let rows: Vec<Line> = self
            .lines(inner_w)
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 0 {
                    Line::from(Span::styled(
                        l,
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(l)
                }
            })
            .collect();
        let block = Block::default().borders(Borders::ALL).title(" Confirm ");
        let inner = block.inner(area);
        if let What::Ask(ask) = &self.what {
            f.render_widget(block, area);
            let slot_h = u16::try_from(SLOT_ROWS).unwrap_or(3).min(inner.height);
            let text_h = inner.height - slot_h;
            f.render_widget(
                Paragraph::new(rows),
                Rect::new(inner.x, inner.y, inner.width, text_h),
            );
            ask.slot.render(
                f,
                Rect::new(inner.x, inner.y + text_h, inner.width, slot_h),
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", ask.label())),
                true,
            );
            return;
        }
        let mut rows = rows;
        let typed_row = rows.len();
        if let What::Confirm { confirmation, .. } = &self.what
            && confirmation.expected().is_some()
        {
            rows.push(Line::from(format!(
                "> {}",
                sanitize_line(confirmation.typed())
            )));
        }
        f.render_widget(Paragraph::new(rows).block(block), area);
        if let What::Confirm { confirmation, .. } = &self.what
            && confirmation.expected().is_some()
            && inner.height > 0
        {
            let row = u16::try_from(typed_row).unwrap_or(u16::MAX);
            let col = u16::try_from(2 + Span::raw(sanitize_line(confirmation.typed())).width())
                .unwrap_or(u16::MAX);
            f.set_cursor_position((
                inner.x + col.min(inner.width.saturating_sub(1)),
                inner.y + row.min(inner.height - 1),
            ));
        }
    }
}

/// The rows the slot of a preview takes: a line in a border.
const SLOT_ROWS: usize = 3;

/// A command that lacks one argument (`push` without its URL): what is known, and the slot the person types the rest in.
#[derive(Clone, Debug)]
pub struct Ask {
    argv: Vec<String>,
    /// The router's name of the missing argument (`git-url`).
    missing: &'static str,
    slot: ChatBox,
    /// Why the last Enter was refused.
    refusal: Option<String>,
}

impl Ask {
    fn new(argv: Vec<String>, missing: &'static str) -> Self {
        Self {
            argv,
            missing,
            slot: ChatBox::new(Kind::Token),
            refusal: None,
        }
    }

    /// The missing argument as a person says it: `git URL`.
    fn label(&self) -> String {
        match self.missing {
            "git-url" => "git URL".to_string(),
            other => other.replace('-', " "),
        }
    }

    /// The value of `--branch` on the line, else the run's own branch.
    fn branch(&self) -> String {
        let named = self
            .argv
            .windows(2)
            .find(|w| w[0] == "--branch")
            .map(|w| w[1].clone());
        named.unwrap_or_else(|| default_branch(self.argv.get(1).map_or("", String::as_str)))
    }

    /// What the preview says: the command, the run, the branch, and what is asked of the person.
    fn lines(&self, width: usize) -> Vec<String> {
        let run = self.argv.get(1).map_or("", String::as_str);
        let head = if self.argv.first().map(String::as_str) == Some("push") {
            format!(
                "Push run {} as branch {}: your own git login sends the run's code to the repository you name.",
                sanitize_line(run),
                sanitize_line(&self.branch())
            )
        } else {
            format!("{} needs more.", command_title(&self.argv))
        };
        let mut lines = wrap_text(&head, width);
        lines.extend(wrap_text(
            &format!(
                "Type or paste the {}, then press Enter · Esc cancels",
                self.label()
            ),
            width,
        ));
        if let Some(why) = &self.refusal {
            lines.extend(wrap_text(why, width));
        }
        lines
    }

    /// An editing key for the slot: letters, Backspace, Delete, the arrows, Home and End and the editor's own word keys. Enter, Esc, Tab and
    /// the arrows up and down are the preview's, not the slot's.
    fn edit(&mut self, key: KeyEvent) {
        let reserved = matches!(
            key.code,
            KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Up
                | KeyCode::Down
        );
        if reserved
            || key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
        {
            return;
        }
        let before = self.slot.text();
        self.slot.handle_key(key);
        if self.slot.text() != before {
            self.refusal = None;
        }
    }
}

/// A number that changes when the config file does (0 when there is none): a preview freezes it, and a command whose preview was made
/// under another revision does not run. FNV-1a over the bytes of the file.
pub fn config_revision() -> u64 {
    let env = ham_core::envnames::os_env;
    let Some(path) = ham_core::config::config_path(&env) else {
        return 0;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // A file whose content hashes to 0 must still differ from "no file".
    hash | 1
}
