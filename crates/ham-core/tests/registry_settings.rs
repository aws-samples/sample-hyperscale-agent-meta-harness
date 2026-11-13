//! `/settings`: a word of the terminal app that opens the Settings screen. It is a terminal-only entry,
//! built, with no effect of its own (the screen writes through the config verbs, each with its own preview), so it runs at once, shows in the
//! popup and in `/help`, and the router hands it to the app as a screen word.
#![allow(clippy::unwrap_used)]

use ham_core::registry::{self, Availability, EffectClass, Effects, Kind, RunArg, find, popup};
use ham_core::router::{RouteContext, RouteOutcome, route};

#[test]
fn settings_is_a_built_terminal_word_with_no_effect_of_its_own() {
    let e = find("settings").expect("the registry has /settings");
    assert_eq!(e.kind, Kind::TuiOnly);
    assert!(matches!(e.status, Availability::Built));
    assert_eq!(e.effects, Effects::NONE);
    assert_eq!(e.effects.class(), EffectClass::ReadOnly);
    assert_eq!(e.run_arg, RunArg::None);
    assert!(
        !e.help.is_empty() && e.help.chars().count() <= 80,
        "{:?}",
        e.help
    );
    // It is a word of the terminal app: the shell parser has no subcommand of that name.
    assert!(registry::subcommand("settings").is_none());
    assert!(!ham_core::command::SUBCOMMANDS.contains(&"settings"));
}

#[test]
fn the_popup_and_help_list_it_and_the_router_hands_it_to_the_app() {
    let names: Vec<&str> = popup("sett").iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["settings"]);
    assert!(popup("s").iter().any(|e| e.name == "settings"));
    let help = registry::help_text();
    assert!(help.contains("/settings"), "{help}");
    assert!(
        registry::command_help("settings")
            .is_some_and(|t| t.contains(find("settings").unwrap().help)),
        "the help of the word itself"
    );
    match route("/settings", &RouteContext::default()) {
        RouteOutcome::Ui { word, args } => {
            assert_eq!(word, "settings");
            assert!(args.is_empty(), "{args:?}");
        }
        other => panic!("not a screen word: {other:?}"),
    }
    // A tainted draft (pasted text) still only opens a screen that changes nothing by itself.
    let tainted = RouteContext {
        focused_run: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
        tainted: true,
    };
    assert!(matches!(
        route("/settings", &tainted),
        RouteOutcome::Ui {
            word: "settings",
            ..
        }
    ));
}
