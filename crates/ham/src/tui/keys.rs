//! The key model: typing mode and browse mode, and the one table (mode, key, action) that the app reads to
//! decide what a key does, the footer prints from, and a test checks. In **typing** mode letters are text, so no letter is a command; in
//! **browse** mode the hotkeys of the dashboard work. Esc leaves typing for browse, `i` or Enter returns.
//!
//! The table is consulted before the text box sees a key. Every key the editor crate would take for itself and the app uses has a row:
//! Ctrl-J (the crate deletes to the line start; here it is a new line), Ctrl-R (redo in the crate; here removed until history search
//! exists, and redo is Alt-Z), Ctrl-U (undo in the crate; here removed, and undo is Ctrl-Z), Ctrl-C (copy in the crate; here clear the
//! box, then quit). The module names actions as an enum and does not import `App`; `app.rs` maps actions to methods.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which keys are live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Letters are text; Enter sends.
    Typing,
    /// The dashboard's hotkeys work.
    Browse,
}

/// What a key does. Typing actions first, then the browse actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Send,
    /// Alt-Enter in the message box: the whole box is sent as chat exactly as typed, where the terminal reports the key.
    SendLiteral,
    NewLine,
    LeaveTyping,
    /// Clear the box when it has text; otherwise ask to quit.
    ClearOrQuit,
    /// The key is swallowed: the editor crate's use of it is gone and nothing replaces it yet.
    Removed,
    /// Go back one edit (the crate's Ctrl-U moved to Ctrl-Z).
    Undo,
    /// Go forward again after an undo (the crate's Ctrl-R moved to Alt-Z).
    Redo,
    StartTyping,
    Runs,
    /// `r` in browse mode: the offer to start a blocked run again; on any other run it opens the Runs list, as it always did.
    Retry,
    /// `R` in browse mode: resume every paused and stalled run. The preview names the count; the same key again confirms.
    ResumeAll,
    NewRun,
    Quit,
    ScrollDown,
    ScrollUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    /// Select the next block of the conversation.
    SelectNext,
    /// Select the previous block (the last one when none is selected).
    SelectPrev,
    /// Fold or open the selected block.
    ToggleFold,
    Artifacts,
    Cancel,
    Clone,
    Open,
    Download,
    Message,
    FreshSession,
    Model,
    Finish,
    /// Copy the selected block of the conversation, or the last answer of Cline when none is selected.
    Copy,
    /// Ctrl-R in the message box: search the history of what was sent.
    HistorySearch,
    /// Tab in the message box: expand the `[Pasted n lines]` placeholder under the caret.
    Expand,
    /// Ctrl-G in the message box: edit the draft in `$VISUAL` or `$EDITOR`.
    Edit,
    /// Ctrl-L in the message box while a follow-up is prepared: add the engine's last message to the draft, fenced as untrusted text.
    ClineMessage,
    /// `w` in browse mode: withdraw the chosen message, else the newest one nobody has claimed.
    Withdraw,
    /// `x` in browse mode: preview the confirmation of the chosen held message (or the only one).
    ConfirmHeld,
    /// `h` in browse mode: choose the next held message (one is chosen at a time).
    ChooseHeld,
    /// Ctrl-O: the open-anything box.
    OpenAnything,
}

/// A key and its modifiers (Control, Alt and Shift only; the case of a letter is in the letter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

const fn plain(code: KeyCode) -> Chord {
    Chord {
        code,
        mods: KeyModifiers::NONE,
    }
}

const fn ch(c: char) -> Chord {
    plain(KeyCode::Char(c))
}

const fn ctrl(c: char) -> Chord {
    Chord {
        code: KeyCode::Char(c),
        mods: KeyModifiers::CONTROL,
    }
}

/// One line of the table. `keys` and `label` make the footer: a row with an empty `label` is not printed there.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub mode: Mode,
    pub chord: Chord,
    pub action: Action,
    pub keys: &'static str,
    pub label: &'static str,
}

const fn row(
    mode: Mode,
    chord: Chord,
    action: Action,
    keys: &'static str,
    label: &'static str,
) -> Row {
    Row {
        mode,
        chord,
        action,
        keys,
        label,
    }
}

use Action as A;
use Mode::{Browse as B, Typing as T};

/// The table. No two rows have the same mode and chord (a test checks).
pub const TABLE: &[Row] = &[
    row(T, plain(KeyCode::Enter), A::Send, "Enter", "send"),
    row(
        T,
        Chord {
            code: KeyCode::Enter,
            mods: KeyModifiers::SHIFT,
        },
        A::NewLine,
        "Shift-Enter",
        "",
    ),
    row(
        T,
        Chord {
            code: KeyCode::Enter,
            mods: KeyModifiers::ALT,
        },
        A::SendLiteral,
        "Alt-Enter",
        "",
    ),
    row(T, ctrl('j'), A::NewLine, "Ctrl-J", "new line"),
    row(T, plain(KeyCode::Esc), A::LeaveTyping, "Esc", "browse"),
    row(T, ctrl('c'), A::ClearOrQuit, "Ctrl-C", "clear, then quit"),
    row(T, ctrl('r'), A::HistorySearch, "Ctrl-R", ""),
    row(T, ctrl('u'), A::Removed, "Ctrl-U", ""),
    row(T, plain(KeyCode::Tab), A::Expand, "Tab", ""),
    row(T, ctrl('g'), A::Edit, "Ctrl-G", ""),
    row(T, ctrl('l'), A::ClineMessage, "Ctrl-L", ""),
    // Open anything works in both modes and on the Runs screen; the line has no room for its label, `/open` and the guide name it.
    row(T, ctrl('o'), A::OpenAnything, "Ctrl-O", ""),
    row(T, ctrl('z'), A::Undo, "Ctrl-Z", ""),
    row(
        T,
        Chord {
            code: KeyCode::Char('z'),
            mods: KeyModifiers::ALT,
        },
        A::Redo,
        "Alt-Z",
        "",
    ),
    row(B, ch('i'), A::StartTyping, "i", "type"),
    row(B, plain(KeyCode::Enter), A::StartTyping, "Enter", ""),
    row(B, ch('q'), A::Quit, "q", "quit"),
    row(B, plain(KeyCode::Esc), A::Runs, "Esc", "runs"),
    row(B, ch('r'), A::Retry, "r", ""),
    row(B, ch('R'), A::ResumeAll, "R", ""),
    row(B, ch('n'), A::NewRun, "n", "new run"),
    row(B, plain(KeyCode::Down), A::ScrollDown, "↑↓", "scroll"),
    row(B, ch('j'), A::ScrollDown, "j", ""),
    row(B, plain(KeyCode::Up), A::ScrollUp, "", ""),
    row(B, ch('k'), A::ScrollUp, "k", ""),
    row(B, plain(KeyCode::PageDown), A::PageDown, "PgDn", ""),
    row(B, plain(KeyCode::PageUp), A::PageUp, "PgUp", ""),
    row(B, plain(KeyCode::Home), A::Top, "Home", ""),
    row(B, ch('g'), A::Top, "g", ""),
    row(B, plain(KeyCode::End), A::Bottom, "End", ""),
    row(B, ch('G'), A::Bottom, "G", ""),
    row(B, plain(KeyCode::Tab), A::SelectNext, "Tab", "select"),
    row(
        B,
        Chord {
            code: KeyCode::BackTab,
            mods: KeyModifiers::SHIFT,
        },
        A::SelectPrev,
        "Shift-Tab",
        "",
    ),
    row(B, ch(' '), A::ToggleFold, "Space", "fold"),
    row(B, ch('a'), A::Artifacts, "a", "artifacts"),
    row(B, ch('c'), A::Cancel, "c", "cancel"),
    row(B, ch('C'), A::Clone, "C", "clone"),
    row(B, ch('o'), A::Open, "o", "open"),
    row(B, ch('d'), A::Download, "d", "download"),
    row(B, ch('p'), A::Message, "p", "message"),
    row(B, ch('s'), A::FreshSession, "s", "fresh session"),
    row(B, ch('m'), A::Model, "m", "model"),
    row(B, ch('f'), A::Finish, "f", "finish"),
    row(B, ch('y'), A::Copy, "y", ""),
    // The message keys: their footer words appear only while a message can be acted on (`App::send_footer`), so no label here.
    row(B, ch('w'), A::Withdraw, "w", ""),
    row(B, ch('x'), A::ConfirmHeld, "x", ""),
    row(B, ch('h'), A::ChooseHeld, "h", ""),
    row(B, ctrl('o'), A::OpenAnything, "Ctrl-O", ""),
];

/// The keys the editor crate takes for itself and the app also uses: each has a row in typing mode.
pub const EDITOR_KEYS_OVERRIDDEN: &[char] = &['j', 'r', 'u', 'c'];

/// Alt+letter pairs the editor crate binds (word moves, word delete, paragraph moves): they stay the editor's in typing mode, and are
/// never read as Esc followed by a browse hotkey.
pub const EDITOR_ALT_LETTERS: &[char] = &['b', 'd', 'f', 'h', 'n', 'p', 'v'];

/// The chord of a key event: Shift is folded into a letter's case, and a release or repeat is the same key. `BackTab` is Shift-Tab: crossterm
/// reads `ESC [ Z` and the CSI-u forms of Shift-Tab as BackTab with Shift, so a BackTab without it (an event built by hand) is the same key and
/// gets the Shift, and the table has the one row (`Shift-Tab`) for both.
pub fn chord(key: &KeyEvent) -> Chord {
    let mut mods =
        key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
    if key.code == KeyCode::BackTab {
        mods.insert(KeyModifiers::SHIFT);
    }
    if let KeyCode::Char(c) = key.code {
        // The terminal reports Shift-g as `G` with Shift held, and Ctrl-J as `j` with Control held.
        if c.is_alphabetic() {
            mods.remove(KeyModifiers::SHIFT);
        }
    }
    Chord {
        code: key.code,
        mods,
    }
}

/// The action of `key` in `mode`, if the table has one. Browse mode also takes Alt+key as the key (crossterm reads Esc and a letter that
/// arrive in one read as Alt+letter, and the table's rows are written for the letter).
pub fn lookup(mode: Mode, key: &KeyEvent) -> Option<Action> {
    let mut c = chord(key);
    if mode == Mode::Browse && c.mods == KeyModifiers::ALT && matches!(c.code, KeyCode::Char(_)) {
        c.mods = KeyModifiers::NONE;
    }
    TABLE
        .iter()
        .find(|r| r.mode == mode && r.chord == c)
        .map(|r| r.action)
}

/// In typing mode, the browse action that Alt+letter stands for (Esc, then the letter, in one chunk). `None` for a letter the editor
/// owns or one the table does not know.
pub fn alt_as_browse(key: &KeyEvent) -> Option<Action> {
    let c = chord(key);
    let KeyCode::Char(letter) = c.code else {
        return None;
    };
    if c.mods != KeyModifiers::ALT || EDITOR_ALT_LETTERS.contains(&letter) {
        return None;
    }
    lookup(Mode::Browse, key)
}

/// Ctrl-O, the key of the open-anything box, in either mode.
pub fn opens_anything(key: &KeyEvent) -> bool {
    [Mode::Typing, Mode::Browse]
        .into_iter()
        .any(|m| lookup(m, key) == Some(Action::OpenAnything))
}

/// The footer of typing mode: the rows that have a label, in table order.
pub fn typing_footer() -> String {
    TABLE
        .iter()
        .filter(|r| r.mode == Mode::Typing && !r.label.is_empty())
        .map(|r| format!("{} {}", r.keys, r.label))
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builders of the table are `const fn`s, which run at compile time when they build `TABLE`; running them here pins what they make.
    #[test]
    fn the_row_builders_make_what_their_names_say() {
        let c = ch('x');
        assert_eq!((c.code, c.mods), (KeyCode::Char('x'), KeyModifiers::NONE));
        let j = ctrl('j');
        assert_eq!(
            (j.code, j.mods),
            (KeyCode::Char('j'), KeyModifiers::CONTROL)
        );
        let e = plain(KeyCode::Enter);
        assert_eq!((e.code, e.mods), (KeyCode::Enter, KeyModifiers::NONE));
        let r = row(Mode::Typing, j, Action::NewLine, "Ctrl-J", "new line");
        assert_eq!(r.mode, Mode::Typing);
        assert_eq!(r.chord, j);
        assert_eq!(r.action, Action::NewLine);
        assert_eq!((r.keys, r.label), ("Ctrl-J", "new line"));
    }
}
