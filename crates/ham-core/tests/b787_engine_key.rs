//! The engine of a run as a client setting: the key default_engine of config.toml and the flag --engine <name> of a start. The engine list
//! lives in `ham_core::engines`; the key, the flag and the TUI choice agree because all three read that one list. A run that chooses no other
//! engine than the default sends no member, so the request is byte for byte what an older client sends.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{Config, KEYS, get_key, parse_config, set_key, unset_key};
use ham_core::engines;
use ham_core::request::{
    DirectInputs, Origin, SettingsError, direct_idempotency_key_on, resolve_run_settings,
};
use ham_core::settings::{DefaultValue, Group, Key, Kind, Order};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::Engine as WireEngine;
use serde_json::Value;

const PROMPT: &str = "Add a percent_change(a, b) function";

fn parsed(extra: &[&str]) -> Result<DirectArgs, String> {
    let mut words = vec![PROMPT, "--s3-only", "--direct"];
    words.extend_from_slice(extra);
    match parse(&words) {
        Ok(inv) => match inv.command {
            Command::StartDirect(a) => Ok(a),
            other => panic!("{words:?} is not a direct start: {other:?}"),
        },
        Err(e) => Err(e.to_string()),
    }
}

fn args(extra: &[&str]) -> DirectArgs {
    parsed(extra).unwrap()
}

fn cfg_of(text: &str) -> Config {
    parse_config(text).unwrap()
}

fn resolved(cfg: &Config, extra: &[&str]) -> ham_core::request::ResolvedRun {
    resolve_run_settings(cfg, &|_| None, &args(extra), false).unwrap()
}

fn body(cfg: &Config, extra: &[&str]) -> Value {
    let run = resolved(cfg, extra);
    let req = run.request(DirectInputs::default()).unwrap();
    serde_json::from_slice(&serde_json::to_vec(&req).unwrap()).unwrap()
}

#[test]
fn b787_the_default_engine_row_is_the_last_key_with_a_flag_and_a_cline_default() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(Key::ALL[44], Key::DefaultEngine);
    let r = Key::DefaultEngine.row();
    assert_eq!(r.name, "default_engine");
    assert_eq!(r.file, ("", "defaultEngine"));
    assert_eq!(r.file_alias, Some("default_engine"));
    assert_eq!(r.kind, Kind::Engine);
    assert_eq!(r.default, DefaultValue::Text("cline"));
    assert_eq!(r.default_text, "cline");
    assert_eq!(
        (r.wire, r.group, r.order, r.security),
        (Some("engine"), Group::Run, Order::FlagFile, false),
        "{r:?}"
    );
    assert_eq!(r.flag, "--engine <name>");
    assert!(r.env.is_empty(), "no variable: the flag and the file only");
    assert_eq!(Key::lookup("default_engine"), Some(Key::DefaultEngine));
    assert_eq!(Key::lookup("defaultEngine"), Some(Key::DefaultEngine));
    assert!(KEYS.contains(&("defaultEngine", "default_engine")));
    assert_eq!(KEYS.len(), 46);
    assert_eq!(Kind::Engine.type_text(), "an engine id");
}

#[test]
fn b787_the_key_names_every_available_engine_in_its_help_and_reference_row() {
    let r = Key::DefaultEngine.row();
    for e in engines::available() {
        assert!(
            r.help.contains(e.id) || r.help.contains("engines"),
            "{}",
            r.help
        );
    }
    let table = ham_core::settings::reference_markdown();
    let line = table.lines().find(|l| l.contains("defaultEngine")).unwrap();
    assert!(
        line.contains("--engine <name>") && line.contains("cline"),
        "{line}"
    );
}

#[test]
fn b787_ham_config_sets_prints_and_unsets_the_key_for_every_available_engine_and_refuses_the_rest()
{
    for e in engines::available() {
        let cfg = set_key(&Config::default(), "default_engine", e.id).unwrap();
        assert_eq!(
            get_key(&cfg, "default_engine").unwrap().as_deref(),
            Some(e.id)
        );
    }
    let upper = set_key(&Config::default(), "default_engine", "  OpenCode ").unwrap();
    assert_eq!(
        get_key(&upper, "default_engine").unwrap().as_deref(),
        Some("opencode"),
        "the id is stored in lower case"
    );
    let cfg = unset_key(&upper, "default_engine").unwrap();
    assert_eq!(get_key(&cfg, "default_engine").unwrap(), None);
    for (bad, want) in [
        ("pi", "not available"),
        ("nope", "unknown engine"),
        ("", "engine"),
    ] {
        let e = set_key(&Config::default(), "default_engine", bad)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("default_engine") && e.contains(want),
            "{bad}: {e}"
        );
        assert!(
            e.contains("cline") && e.contains("opencode"),
            "the choices are listed: {e}"
        );
    }
}

#[test]
fn b787_the_file_reader_takes_the_key_in_both_spellings_and_names_a_wrong_type() {
    assert_eq!(
        cfg_of("defaultEngine = \"opencode\"\n")
            .default_engine
            .as_deref(),
        Some("opencode")
    );
    assert_eq!(
        cfg_of("default_engine = \"cline\"\n")
            .default_engine
            .as_deref(),
        Some("cline")
    );
    let e = parse_config("defaultEngine = 3\n").unwrap_err().to_string();
    assert!(
        e.contains("defaultEngine") || e.contains("default_engine"),
        "{e}"
    );
}

#[test]
fn b787_the_flag_takes_one_available_engine_once_and_only_on_a_direct_start() {
    assert_eq!(
        args(&["--engine", "opencode"]).engine.as_deref(),
        Some("opencode")
    );
    assert_eq!(
        args(&["--engine", "Cline"]).engine.as_deref(),
        Some("cline")
    );
    assert_eq!(args(&[]).engine, None);
    for (bad, want) in [
        (vec!["--engine", "pi"], "pi is not available"),
        (vec!["--engine", "gemini"], "unknown engine"),
        (vec!["--engine"], "--engine"),
        (vec!["--engine", "cline", "--engine", "opencode"], "twice"),
    ] {
        let e = parsed(&bad).unwrap_err();
        assert!(e.contains(want), "{bad:?}: {e}");
    }
    let e = parsed(&["--engine", "pi"]).unwrap_err();
    assert!(
        e.contains("cline, opencode"),
        "the available engines are listed: {e}"
    );
    let e = parse(&[PROMPT, "--s3-only", "--engine", "opencode"])
        .unwrap_err()
        .to_string();
    assert!(e.contains("--engine") && e.contains("--direct"), "{e}");
}

#[test]
fn b787_the_flag_is_written_back_into_the_command_line_and_continue_takes_it_too() {
    let inv = parse(&[PROMPT, "--s3-only", "--direct", "--engine", "opencode"]).unwrap();
    let argv = inv.to_argv();
    let at = argv.iter().position(|w| w == "--engine").unwrap();
    assert_eq!(argv[at + 1], "opencode", "{argv:?}");
    let again = parse(&argv.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
    assert_eq!(again, inv);
    let inv = parse(&[
        "continue",
        "01M4ZHFBV5SDX7J92E6YRH4QTW",
        "go on",
        "--engine",
        "opencode",
    ])
    .unwrap();
    match &inv.command {
        Command::Continue(a, _) => assert_eq!(a.engine.as_deref(), Some("opencode")),
        other => panic!("{other:?}"),
    }
    let argv = inv.to_argv();
    assert!(
        argv.windows(2).any(|w| w == ["--engine", "opencode"]),
        "{argv:?}"
    );
}

#[test]
fn b787_a_run_with_no_choice_sends_no_member_and_the_bytes_are_those_of_an_older_client() {
    let run = resolved(&Config::default(), &[]);
    assert_eq!(run.engine.id, "cline");
    assert_eq!(run.engine_member(), None);
    let req = run.request(DirectInputs::default()).unwrap();
    assert_eq!(req.engine, None, "no engine chosen: the field is left out");
    // The request of a run with no choice is the request an older client builds from the same flags, byte for byte.
    let mut older = req.clone();
    older.engine = None;
    assert_eq!(
        serde_json::to_vec(&req).unwrap(),
        serde_json::to_vec(&older).unwrap()
    );
    assert!(
        !String::from_utf8(serde_json::to_vec(&req).unwrap())
            .unwrap()
            .contains("\"engine\":"),
        "no engine chosen: the body has no engine member"
    );
    assert!(body(&Config::default(), &[]).get("engine").is_none());
    // The default engine named out loud is still the request of before: a stage that predates the member keeps working.
    assert!(
        body(&Config::default(), &["--engine", "cline"])
            .get("engine")
            .is_none()
    );
    assert!(
        body(&cfg_of("defaultEngine = \"cline\"\n"), &[])
            .get("engine")
            .is_none()
    );
}

#[test]
fn b787_a_chosen_second_engine_adds_exactly_one_member_and_changes_nothing_else() {
    let plain = body(&Config::default(), &[]);
    let chosen = body(&Config::default(), &["--engine", "opencode"]);
    assert_eq!(chosen["engine"], "opencode");
    let mut without = chosen.clone();
    without.as_object_mut().unwrap().remove("engine");
    assert_eq!(without, plain, "only the engine member differs");
    let from_file = body(&cfg_of("default_engine = \"opencode\"\n"), &[]);
    assert_eq!(from_file["engine"], "opencode");
}

#[test]
fn b787_the_engine_travels_in_the_engine_field_of_the_request_and_is_written_once() {
    let run = resolved(&Config::default(), &["--engine", "opencode"]);
    let req = run.request(DirectInputs::default()).unwrap();
    // The member is the field of the protocol type (1.58.0), set by `ResolvedRun::request`; no second writer adds one beside it.
    assert_eq!(req.engine, Some(WireEngine::OPENCODE));
    let text = String::from_utf8(serde_json::to_vec(&req).unwrap()).unwrap();
    assert_eq!(text.matches("\"engine\":").count(), 1, "{text}");
    assert!(text.contains("\"engine\":\"opencode\""), "{text}");
    // It reads back as the same type, with the engine kept.
    let back: StartDirectRunRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(back.engine, Some(WireEngine::OPENCODE));
    assert_eq!(back, req);
    // `engine_member` stays the id for the screens that name the engine; it and the field agree.
    assert_eq!(run.engine_member(), Some("opencode"));
}

#[test]
fn b787_every_engine_of_the_table_reaches_the_request_by_its_id_and_the_default_by_no_member() {
    for e in engines::roadmap() {
        // An id the wire type would refuse would leave the member out and start the run on the default engine without a word.
        let wire = WireEngine::new(e.id).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(wire.as_str(), e.id);
    }
    for e in engines::available() {
        let run = resolved(&Config::default(), &["--engine", e.id]);
        let req = run.request(DirectInputs::default()).unwrap();
        if e.is_default() {
            assert_eq!(req.engine, None, "{}", e.id);
        } else {
            assert_eq!(req.engine, Some(WireEngine::new(e.id).unwrap()), "{}", e.id);
        }
    }
}

#[test]
fn b787_the_idempotency_key_includes_the_engine_and_is_the_old_key_without_one() {
    let default = resolved(&Config::default(), &[])
        .request(DirectInputs::default())
        .unwrap();
    let other = resolved(&Config::default(), &["--engine", "opencode"])
        .request(DirectInputs::default())
        .unwrap();
    // A run on the default engine hashes the request of before: the same bytes, so the same key.
    let mut older = default.clone();
    older.engine = None;
    let old = direct_idempotency_key_on(&older, "n1", None);
    assert_eq!(direct_idempotency_key_on(&default, "n1", None), old);
    let a = direct_idempotency_key_on(&other, "n1", None);
    assert_ne!(a, old, "a run on another engine is another start");
    assert_eq!(
        a,
        direct_idempotency_key_on(&other, "n1", None),
        "a retry sends the same key"
    );
    let on = direct_idempotency_key_on(&other, "n1", Some("work"));
    assert_ne!(on, a, "the backend alias still takes part");
}

#[test]
fn b787_the_layers_are_the_flag_then_the_file_then_the_default() {
    let file = cfg_of("defaultEngine = \"opencode\"\n");
    assert_eq!(resolved(&file, &[]).engine.id, "opencode");
    assert_eq!(resolved(&file, &["--engine", "cline"]).engine.id, "cline");
    assert_eq!(resolved(&Config::default(), &[]).engine.id, "cline");
    let origin = |cfg: &Config, extra: &[&str]| resolved(cfg, extra).origin(Key::DefaultEngine);
    assert_eq!(origin(&file, &["--engine", "cline"]), Some(Origin::Flag));
    assert_eq!(origin(&file, &[]), Some(Origin::File));
    assert_eq!(origin(&Config::default(), &[]), Some(Origin::Default));
}

#[test]
fn b787_a_value_that_is_set_and_wrong_stops_the_run_and_names_its_layer() {
    let cfg = Config {
        default_engine: Some("kiro".into()),
        ..Config::default()
    };
    let e = resolve_run_settings(&cfg, &|_| None, &args(&[]), false).unwrap_err();
    assert!(matches!(e, SettingsError::Engine(_)), "{e:?}");
    let text = e.to_string();
    assert!(
        text.contains("default_engine") && text.contains("kiro"),
        "{text}"
    );
    // A flag that is right does not hide a wrong file: the flag wins, and the file is not looked at.
    assert_eq!(
        resolve_run_settings(&cfg, &|_| None, &args(&["--engine", "opencode"]), false)
            .unwrap()
            .engine
            .id,
        "opencode"
    );
}
