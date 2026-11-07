//! How a user's settings reach the mascot: `ui.mascot` in `config.toml` (`auto`, `ascii`, `off`), `ui.mascot_motion` (the smoke under the feet
//! moves unless it says `false`) and `HAMH_NO_MASCOT=1`. `cli::head_from` turns
//! the environment and the file into the `Head` the app is given; `cli::apply_ui_settings` gives it at start (the band and the picture in the
//! empty table are on for a person who changed nothing); a write from the Settings screen takes effect on the running app at once, like the bell.
#![allow(clippy::unwrap_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ham::cli::{apply_ui_settings, head_from};
use ham::tui::head::Head;
use ham::tui::mascot::Look;
use ham::tui::settings::{self, Job, Op};
use ham::tui::{App, DirectSettings, Effect, Msg};
use ham_core::config::{Config, set_key};
use ham_core::settings::Key;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn env_of(
    pairs: &[(&'static str, &'static str)],
) -> impl Fn(&str) -> Option<String> + Send + Sync + 'static {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn cfg(mode: &str) -> Config {
    set_key(&Config::default(), "ui.mascot", mode).unwrap()
}

const UTF8: (&str, &str) = ("LANG", "C.UTF-8");

fn head(env: &[(&'static str, &'static str)], cfg: &Config) -> Head {
    head_from(&env_of(env), cfg)
}

fn on(look: Look) -> Head {
    Head {
        band: true,
        empty_table: true,
        look,
    }
}

const OFF_BOTH: bool = false;

#[test]
fn a_person_who_changed_nothing_gets_the_band_and_the_picture_in_the_look_of_the_environment() {
    // No locale: the ASCII look. A UTF-8 locale: blocks.
    assert_eq!(head(&[], &Config::default()), on(Look::Ascii));
    assert_eq!(head(&[UTF8], &Config::default()), on(Look::Blocks));
    assert_eq!(
        head(&[("LC_ALL", "en_US.UTF-8")], &Config::default()),
        on(Look::Blocks)
    );
    // The environment's other asks for the plain look: NO_COLOR, TERM=dumb, a locale that is not UTF-8.
    assert_eq!(
        head(&[UTF8, ("NO_COLOR", "1")], &Config::default()),
        on(Look::Ascii)
    );
    assert_eq!(
        head(&[UTF8, ("TERM", "dumb")], &Config::default()),
        on(Look::Ascii)
    );
    assert_eq!(head(&[("LANG", "C")], &Config::default()), on(Look::Ascii));
    // `auto` is the same as no setting.
    assert_eq!(head(&[UTF8], &cfg("auto")), on(Look::Blocks));
}

#[test]
fn ascii_forces_the_ascii_look_and_keeps_the_geometry() {
    assert_eq!(head(&[UTF8], &cfg("ascii")), on(Look::Ascii));
    assert_eq!(head(&[], &cfg("ascii")), on(Look::Ascii));
}

#[test]
fn off_hides_the_band_and_the_picture_in_the_empty_table() {
    let h = head(&[UTF8], &cfg("off"));
    assert_eq!((h.band, h.empty_table), (OFF_BOTH, OFF_BOTH));
}

#[test]
fn ham_no_mascot_wins_over_the_file_and_only_switches_off() {
    for value in ["1", "true", "yes", "on", "TRUE"] {
        let static_value: &'static str = Box::leak(value.to_string().into_boxed_str());
        for file in [Config::default(), cfg("auto"), cfg("ascii"), cfg("off")] {
            let h = head(&[UTF8, ("HAMH_NO_MASCOT", static_value)], &file);
            assert!(!h.band && !h.empty_table, "{value}");
        }
    }
    // Empty, 0 and a word that is no switch leave the file's value in force.
    for value in ["", "0", "no", "false", "banana"] {
        let static_value: &'static str = Box::leak(value.to_string().into_boxed_str());
        assert_eq!(
            head(
                &[UTF8, ("HAMH_NO_MASCOT", static_value)],
                &Config::default()
            ),
            on(Look::Blocks),
            "{value:?}"
        );
        let h = head(&[UTF8, ("HAMH_NO_MASCOT", static_value)], &cfg("off"));
        assert!(!h.band && !h.empty_table, "{value:?}");
    }
}

#[test]
fn the_app_started_for_a_user_has_the_band_on_and_one_started_for_a_test_has_it_off() {
    // The constructor is neutral: the library's tests draw the screens they always drew.
    let test_app = App::new_direct("n".into(), None, DirectSettings::default());
    assert!(!test_app.head().band);
    assert!(test_app.head().empty_table);
    // `cli::tui_app` gives the head through `apply_ui_settings`: with nothing configured the band is on.
    let mut user = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(&mut user, &env_of(&[UTF8]), &Config::default());
    assert_eq!(user.head(), on(Look::Blocks));
    let mut off = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(
        &mut off,
        &env_of(&[UTF8, ("HAMH_NO_MASCOT", "1")]),
        &Config::default(),
    );
    assert!(!off.head().band && !off.head().empty_table);
}

// ---- the Settings screen ------------------------------------------------------------------------------------------------------------

struct World {
    root: PathBuf,
}

impl World {
    fn new(content: &str) -> World {
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ham-mascot-settings-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(root.join("cfg/ham")).unwrap();
        std::fs::write(root.join("cfg/ham/config.toml"), content).unwrap();
        World { root }
    }

    fn env(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |k: &str| match k {
            "XDG_CONFIG_HOME" => Some(self.root.join("cfg").display().to_string()),
            "LANG" => Some("C.UTF-8".to_string()),
            _ => None,
        }
    }

    fn file(&self) -> String {
        std::fs::read_to_string(self.root.join("cfg/ham/config.toml")).unwrap()
    }
}

fn settings_app(w: &World) -> App {
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(&mut app, &env_of(&[UTF8]), &Config::default());
    let effects = app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    for e in effects {
        if let Effect::Settings(job) = e {
            app.update(Msg::Settings(settings::perform(&w.env(), job)));
        }
    }
    app
}

fn write(app: &mut App, w: &World, key: Key, value: &str) {
    let done = settings::perform(
        &w.env(),
        Job::Apply {
            key,
            op: Op::Set(value.to_string()),
        },
    );
    app.update(Msg::Settings(done));
}

#[test]
fn a_write_from_the_settings_screen_takes_effect_on_the_running_app_at_once() {
    let w = World::new("region = \"us-east-1\"\n");
    let mut app = settings_app(&w);
    assert_eq!(app.head(), on(Look::Blocks));
    write(&mut app, &w, Key::UiMascot, "off");
    assert!(w.file().contains("mascot = \"off\""), "{}", w.file());
    assert!(
        !app.head().band && !app.head().empty_table,
        "off applies now"
    );
    write(&mut app, &w, Key::UiMascot, "ascii");
    assert_eq!(app.head(), on(Look::Ascii), "ascii applies now");
    write(&mut app, &w, Key::UiMascot, "auto");
    assert_eq!(
        app.head(),
        on(Look::Blocks),
        "auto goes back to the look of the environment"
    );
}

#[test]
fn the_settings_screen_lists_the_row_with_its_words() {
    let w = World::new("region = \"us-east-1\"\n");
    let app = settings_app(&w);
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let text: String = (0..40u16)
        .map(|y| {
            (0..120u16)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let line = text
        .lines()
        .find(|l| l.contains("Mascot"))
        .unwrap_or_else(|| panic!("no Mascot row:\n{text}"));
    assert!(line.contains("auto") && line.contains("now"), "{line}");
}

// ---- the motion of the smoke ------------------------------------------------------------------------------------------------------

fn motion_cfg(word: &str) -> Config {
    set_key(&Config::default(), "ui.mascot_motion", word).unwrap()
}

#[test]
fn the_smoke_moves_unless_ui_mascot_motion_is_false() {
    // The constructor and a person who changed nothing: the smoke moves.
    let test_app = App::new_direct("n".into(), None, DirectSettings::default());
    assert!(test_app.mascot_motion());
    let mut user = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(&mut user, &env_of(&[UTF8]), &Config::default());
    assert!(user.mascot_motion());
    for (word, want) in [
        ("on", true),
        ("true", true),
        ("off", false),
        ("false", false),
    ] {
        let mut app = App::new_direct("n".into(), None, DirectSettings::default());
        apply_ui_settings(&mut app, &env_of(&[UTF8]), &motion_cfg(word));
        assert_eq!(app.mascot_motion(), want, "{word}");
        // The key decides the motion only: the band and the picture are the ones `ui.mascot` gives.
        assert_eq!(app.head(), on(Look::Blocks), "{word}");
    }
}

#[test]
fn ui_mascot_off_and_ham_no_mascot_still_hide_everything_whatever_the_motion_says() {
    let mut cfg = motion_cfg("true");
    cfg = set_key(&cfg, "ui.mascot", "off").unwrap();
    let mut app = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(&mut app, &env_of(&[UTF8]), &cfg);
    assert!(!app.head().band && !app.head().empty_table);
    let mut forced = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(
        &mut forced,
        &env_of(&[UTF8, ("HAMH_NO_MASCOT", "1")]),
        &motion_cfg("true"),
    );
    assert!(!forced.head().band && !forced.head().empty_table);
    // ui.mascot = ascii keeps the motion: the ASCII look animates too.
    let mut ascii = App::new_direct("n".into(), None, DirectSettings::default());
    apply_ui_settings(
        &mut ascii,
        &env_of(&[UTF8]),
        &set_key(&Config::default(), "ui.mascot", "ascii").unwrap(),
    );
    assert!(ascii.mascot_motion());
}

#[test]
fn a_write_of_ui_mascot_motion_from_the_settings_screen_takes_effect_at_once() {
    let w = World::new("region = \"us-east-1\"\n");
    let mut app = settings_app(&w);
    assert!(app.mascot_motion());
    write(&mut app, &w, Key::UiMascotMotion, "off");
    assert!(w.file().contains("mascot_motion = false"), "{}", w.file());
    assert!(!app.mascot_motion(), "off applies now");
    assert_eq!(app.mascot_frame(), 0);
    write(&mut app, &w, Key::UiMascotMotion, "on");
    assert!(w.file().contains("mascot_motion = true"), "{}", w.file());
    assert!(app.mascot_motion(), "on applies now");
    // The look and the band are the ones they were.
    assert_eq!(app.head(), on(Look::Blocks));
}

#[test]
fn the_settings_screen_lists_the_motion_row_with_its_words() {
    let w = World::new("region = \"us-east-1\"\n");
    let app = settings_app(&w);
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    let text: String = (0..50u16)
        .map(|y| {
            (0..120u16)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let line = text
        .lines()
        .find(|l| l.contains("Mascot motion"))
        .unwrap_or_else(|| panic!("no Mascot motion row:\n{text}"));
    assert!(line.contains("on") && line.contains("now"), "{line}");
}
