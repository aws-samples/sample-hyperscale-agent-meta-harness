//! The terminal session guard: one object turns on the alternate screen, bracketed paste, focus events and, when wanted, the
//! mouse, and writes the matching off-sequences on every way out. The event loop draws once for a burst of queued events, not once
//! for each.
#![allow(clippy::unwrap_used)]

mod support;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham::tui::session::{
    Modes, selection_hint, wants_mouse, write_enter, write_mouse, write_restore, write_start,
};
use ham::tui::{App, DirectSettings};
use ham_core::config::MouseMode;
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::{Fake, REGION, creds};

/// The DEC private modes a byte stream leaves on (`ESC [ ? n h` turns one on, `ESC [ ? n l` off; a list `n;m` sets each). Mode 25
/// (the cursor) is left out: it is on in a terminal's default state, and the tests below check it with its own assertion.
fn modes_on(bytes: &[u8]) -> BTreeSet<u16> {
    let mut on = BTreeSet::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' && bytes[i + 2] == b'?' {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'h' || bytes[j] == b'l') {
                for n in std::str::from_utf8(&bytes[i + 3..j])
                    .unwrap()
                    .split(';')
                    .filter_map(|n| n.parse::<u16>().ok())
                {
                    if n == 25 {
                        continue;
                    }
                    if bytes[j] == b'h' {
                        on.insert(n);
                    } else {
                        on.remove(&n);
                    }
                }
            }
            i = j;
        }
        i += 1;
    }
    on
}

fn enter(m: &Modes) -> Vec<u8> {
    let mut out = Vec::new();
    write_enter(&mut out, m).unwrap();
    out
}

fn restore(m: &Modes) -> Vec<u8> {
    let mut out = Vec::new();
    write_restore(&mut out, m).unwrap();
    out
}

/// Every combination of the five modes.
fn every_combination() -> Vec<Modes> {
    (0..32u8)
        .map(|b| Modes {
            raw: b & 1 != 0,
            alt_screen: b & 2 != 0,
            paste: b & 4 != 0,
            focus: b & 8 != 0,
            mouse: b & 16 != 0,
        })
        .collect()
}

const ALT_SCREEN: u16 = 1049;
const PASTE: u16 = 2004;
const FOCUS: u16 = 1004;
const MOUSE_PRESS: u16 = 1000;
const MOUSE_DRAG: u16 = 1002;
const MOUSE_SGR: u16 = 1006;

#[test]
fn the_session_turns_on_the_alternate_screen_paste_and_focus_and_the_mouse_only_when_asked() {
    assert_eq!(
        modes_on(&enter(&Modes::tui(false))),
        BTreeSet::from([ALT_SCREEN, PASTE, FOCUS])
    );
    // Press, drag and SGR coordinates only: no all-motion (1003) and no RXVT encoding (1015), which the TUI never uses.
    assert_eq!(
        modes_on(&enter(&Modes::tui(true))),
        BTreeSet::from([ALT_SCREEN, PASTE, FOCUS, MOUSE_PRESS, MOUSE_DRAG, MOUSE_SGR])
    );
}

#[test]
fn restore_writes_the_off_sequence_for_every_mode_that_was_turned_on() {
    for m in every_combination() {
        let on = modes_on(&enter(&m));
        let off = restore(&m);
        // Entered then restored: nothing stays on.
        let mut both = enter(&m);
        both.extend_from_slice(&off);
        assert!(
            modes_on(&both).is_empty(),
            "{m:?} left {:?} on",
            modes_on(&both)
        );
        // The restore stands alone (a panic hook has no memory of the enter): it names each mode it must turn off.
        for n in on {
            let want = format!("\x1b[?{n}l");
            assert!(
                String::from_utf8_lossy(&off).contains(&want),
                "{m:?}: restore lacks {want:?}: {off:?}"
            );
        }
    }
}

/// Every report of a mode in `bytes`, in order: `true` for `ESC [ ? n h`, `false` for `ESC [ ? n l`.
fn changes_of(bytes: &[u8], mode: u16) -> Vec<bool> {
    let text = String::from_utf8_lossy(bytes).to_string();
    let mut found: Vec<(usize, bool)> = Vec::new();
    for (suffix, on) in [("h", true), ("l", false)] {
        let needle = format!("\x1b[?{mode}{suffix}");
        found.extend(text.match_indices(&needle).map(|(i, _)| (i, on)));
    }
    found.sort_unstable();
    found.into_iter().map(|(_, on)| on).collect()
}

#[test]
fn focus_reports_are_asked_for_at_the_start_and_given_back_at_the_end_in_exactly_these_bytes() {
    // The bytes of a session without the mouse: the three modes in the order the restore undoes them in reverse.
    assert_eq!(
        String::from_utf8(enter(&Modes::tui(false))).unwrap(),
        "\x1b[?1049h\x1b[?2004h\x1b[?1004h"
    );
    assert_eq!(
        String::from_utf8(restore(&Modes::tui(false))).unwrap(),
        "\x1b[?1004l\x1b[?2004l\x1b[?1049l\x1b[0m\x1b[?25h"
    );
    // A start repairs a window that an earlier ham left in focus mode (pre-reset off), asks for it, and the end gives it back.
    let mut whole = Vec::new();
    write_start(&mut whole, &Modes::tui(true)).unwrap();
    assert_eq!(changes_of(&whole, FOCUS), vec![false, true]);
    write_restore(&mut whole, &Modes::tui(true)).unwrap();
    assert_eq!(changes_of(&whole, FOCUS), vec![false, true, false]);
    // The focus mode is undone before the alternate screen is left, so the shell never sees a stray report.
    let text = String::from_utf8_lossy(&whole).to_string();
    assert!(text.rfind("\x1b[?1004l").unwrap() < text.rfind("\x1b[?1049l").unwrap());
}

#[test]
fn restore_leaves_the_cursor_visible_even_when_nothing_else_was_on() {
    let nothing = Modes {
        raw: false,
        alt_screen: false,
        paste: false,
        focus: false,
        mouse: false,
    };
    assert!(String::from_utf8_lossy(&restore(&nothing)).contains("\x1b[?25h"));
    assert!(String::from_utf8_lossy(&restore(&Modes::tui(true))).contains("\x1b[?25h"));
}

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| {
        pairs
            .iter()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.clone())
    }
}

#[test]
fn the_mouse_is_on_by_default_and_off_by_config_or_by_ham_no_mouse() {
    // No setting means on; `mouse = false` and HAMH_NO_MOUSE=1 turn it off, and the variable wins over `true`.
    assert!(wants_mouse(&env(&[]), None));
    assert!(wants_mouse(&env(&[]), Some(MouseMode::On)));
    assert!(!wants_mouse(&env(&[]), Some(MouseMode::Off)));
    assert!(!wants_mouse(&env(&[("HAMH_NO_MOUSE", "1")]), None));
    assert!(!wants_mouse(
        &env(&[("HAMH_NO_MOUSE", "1")]),
        Some(MouseMode::On)
    ));
    assert!(!wants_mouse(
        &env(&[("HAMH_NO_MOUSE", "true")]),
        Some(MouseMode::On)
    ));
    // A zero or an empty value is not the switch.
    assert!(wants_mouse(
        &env(&[("HAMH_NO_MOUSE", "0")]),
        Some(MouseMode::On)
    ));
    assert!(wants_mouse(&env(&[("HAMH_NO_MOUSE", "")]), None));
}

#[test]
fn auto_is_on_for_a_listed_terminal_and_off_for_apple_terminal_and_unknown_ones() {
    let auto = Some(MouseMode::Auto);
    for term in ["iTerm.app", "ghostty", "WezTerm", "vscode"] {
        assert!(wants_mouse(&env(&[("TERM_PROGRAM", term)]), auto), "{term}");
    }
    // Terminal.app sets TERM=xterm-256color too: its name wins over the family.
    let apple = env(&[
        ("TERM_PROGRAM", "Apple_Terminal"),
        ("TERM", "xterm-256color"),
    ]);
    assert!(!wants_mouse(&apple, auto));
    assert!(!wants_mouse(&env(&[("TERM_PROGRAM", "tmux")]), auto));
    assert!(!wants_mouse(&env(&[]), auto));
    // Over ssh only TERM arrives: the xterm and kitty families are on.
    assert!(wants_mouse(&env(&[("TERM", "xterm-256color")]), auto));
    assert!(wants_mouse(&env(&[("TERM", "xterm-kitty")]), auto));
    assert!(!wants_mouse(&env(&[("TERM", "screen")]), auto));
    // `HAMH_NO_MOUSE` still wins.
    assert!(!wants_mouse(
        &env(&[("TERM_PROGRAM", "iTerm.app"), ("HAMH_NO_MOUSE", "1")]),
        auto
    ));
}

/// A backend that counts the times the terminal is drawn.
struct Counting {
    inner: TestBackend,
    draws: Arc<AtomicUsize>,
}

impl Backend for Counting {
    type Error = std::convert::Infallible;
    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.draws.fetch_add(1, Ordering::SeqCst);
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_1000_queued_events_causes_a_few_draws() {
    let fake = Fake::start().await;
    let api = Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let draws = Arc::new(AtomicUsize::new(0));
    let mut term = Terminal::new(Counting {
        inner: TestBackend::new(80, 24),
        draws: draws.clone(),
    })
    .unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    // Everything is queued before the loop starts: a paste that arrived as keys, or a burst of mouse reports.
    for _ in 0..1000 {
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    }
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    )))
    .unwrap();
    let app = App::new_direct("n".into(), None, DirectSettings::default());
    let dir = support::scratch("tui-term-burst");
    ham::tui::event_loop(
        &mut term,
        api,
        dir,
        app,
        vec![],
        rx,
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    let drew = draws.load(Ordering::SeqCst);
    assert!(
        (2..=4).contains(&drew),
        "1001 queued events drew {drew} times; one draw per event would be 1002"
    );
}

#[test]
fn a_signal_is_named_for_the_error_that_ends_the_app() {
    use ham::tui::session::signal_name;
    assert_eq!(signal_name(1), "SIGHUP");
    assert_eq!(signal_name(2), "SIGINT");
    assert_eq!(signal_name(15), "SIGTERM");
    assert_eq!(signal_name(99), "a signal");
}

// ---- the pre-reset at the start of a session -----------------------------------------------------

/// What an earlier ham that was killed with SIGKILL left on in the same terminal window: every mouse mode, focus reports and bracketed
/// paste. (A SIGKILL cannot be caught, so the next start has to repair it.)
fn left_by_a_killed_ham() -> Vec<u8> {
    b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1015h\x1b[?1004h\x1b[?2004h".to_vec()
}

#[test]
fn a_start_with_the_mouse_off_leaves_no_mouse_mode_on_that_an_earlier_ham_left_behind() {
    let mut bytes = left_by_a_killed_ham();
    write_start(&mut bytes, &Modes::tui(false)).unwrap();
    let on = modes_on(&bytes);
    for mode in [1000, 1002, 1003, 1006, 1015] {
        assert!(!on.contains(&mode), "mode {mode} is still on: {on:?}");
    }
    // The modes the session asked for are on, whatever the window had.
    assert_eq!(on, BTreeSet::from([1004, 1049, 2004]));
}

#[test]
fn a_start_with_the_mouse_on_leaves_exactly_the_three_modes_the_tui_uses() {
    let mut bytes = left_by_a_killed_ham();
    write_start(&mut bytes, &Modes::tui(true)).unwrap();
    // Motion tracking (1003) and the RXVT encoding (1015), which only a killed crossterm capture would have left, are off.
    assert_eq!(
        modes_on(&bytes),
        BTreeSet::from([1000, 1002, 1004, 1006, 1049, 2004])
    );
}

#[test]
fn the_reset_comes_before_the_modes_and_does_not_touch_the_screen_the_person_is_on() {
    let mut bytes = Vec::new();
    write_start(&mut bytes, &Modes::tui(true)).unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let alt = text.find("\x1b[?1049h").unwrap();
    // Nothing before the alternate screen is entered may leave or enter it, and nothing resets the text or moves the cursor.
    let before = &text[..alt];
    assert!(!before.contains("1049"), "{before:?}");
    assert!(
        !before.contains("\x1b[0m") && !before.contains("\x1b[H") && !before.contains("\x1b[2J"),
        "{before:?}"
    );
    // Every reset is a plain private-mode off sequence.
    assert!(
        before.contains("\x1b[?1003l") && before.contains("\x1b[?1015l"),
        "{before:?}"
    );
    // The restore at the end is the same as before the pre-reset existed.
    let mut end = Vec::new();
    write_restore(&mut end, &Modes::tui(true)).unwrap();
    assert!(modes_on(&[bytes.clone(), end].concat()).is_empty());
}

// ---- `/mouse on|off` and the native-selection hint ----------------------------------------------------------

#[test]
fn the_mouse_sequences_are_the_three_modes_the_tui_uses_and_nothing_else() {
    let mut on = Vec::new();
    write_mouse(&mut on, true).unwrap();
    assert_eq!(modes_on(&on), BTreeSet::from([1000, 1002, 1006]));
    let mut off = Vec::new();
    write_mouse(&mut off, false).unwrap();
    assert_eq!(modes_on(&off), BTreeSet::new(), "every mode ends off");
    // Off names the same three modes, so a terminal that was left in any of them is repaired by it.
    let all = |bytes: &[u8]| -> BTreeSet<u16> {
        let text = String::from_utf8_lossy(bytes);
        text.split("\u{1b}[?")
            .filter_map(|p| p.split(['h', 'l']).next()?.parse().ok())
            .collect()
    };
    assert_eq!(all(&off), all(&on));
    // 1003 (all motion) and 1015 are never written.
    assert!(!all(&on).contains(&1003) && !all(&on).contains(&1015));
}

#[test]
fn turning_the_mouse_off_and_on_again_on_a_session_matches_a_session_that_started_that_way() {
    // The bytes of a session that starts with the mouse on, then `/mouse off`, then `/mouse on`, then ends.
    let mut bytes = Vec::new();
    write_start(&mut bytes, &Modes::tui(true)).unwrap();
    write_mouse(&mut bytes, false).unwrap();
    assert_eq!(modes_on(&bytes), BTreeSet::from([1049, 2004, 1004]));
    write_mouse(&mut bytes, true).unwrap();
    assert_eq!(
        modes_on(&bytes),
        BTreeSet::from([1000, 1002, 1004, 1006, 1049, 2004])
    );
    write_restore(&mut bytes, &Modes::tui(true)).unwrap();
    assert_eq!(modes_on(&bytes), BTreeSet::new());
}

fn hint_for(pairs: &[(&str, &str)]) -> String {
    selection_hint(&env(pairs))
}

#[test]
fn the_hint_names_the_key_that_selects_text_in_that_terminal() {
    let apple = hint_for(&[("TERM_PROGRAM", "Apple_Terminal")]);
    assert!(apple.contains("Fn"), "{apple}");
    let iterm = hint_for(&[("TERM_PROGRAM", "iTerm.app")]);
    assert!(iterm.contains("Option"), "{iterm}");
    for other in [
        hint_for(&[("TERM_PROGRAM", "ghostty")]),
        hint_for(&[("TERM", "xterm-256color")]),
        hint_for(&[]),
    ] {
        assert!(other.contains("Shift"), "{other}");
    }
    // One line, and the way out is named.
    for h in [&apple, &iterm] {
        assert!(!h.contains('\n') && h.chars().count() <= 90, "{h}");
        assert!(h.contains("/mouse off"), "{h}");
    }
}
