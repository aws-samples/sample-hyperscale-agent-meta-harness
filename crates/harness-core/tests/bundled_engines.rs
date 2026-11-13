//! Every engine is a profile: the build bundles each
//! `engine/profiles/*.toml` and `known_engine` refuses a name without one.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_core::engine_profile::{
    UnknownEngine, bundled_names, bundled_profile, known_engine, load_profile,
};
use harness_protocol::common::Engine;
use std::path::PathBuf;

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/profiles")
}

fn stems() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(profiles_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn every_profile_file_is_bundled_and_is_a_known_engine() {
    let stems = stems();
    assert!(stems.len() >= 3, "{stems:?}");
    assert_eq!(bundled_names(), stems);
    for s in &stems {
        let e = known_engine(s).unwrap();
        assert_eq!(e.as_str(), s);
        let bundled = bundled_profile(e).unwrap().unwrap();
        assert_eq!(
            bundled,
            load_profile(&profiles_dir().join(format!("{s}.toml"))).unwrap()
        );
    }
}

#[test]
fn a_name_without_a_profile_or_of_the_wrong_shape_is_unknown() {
    let e = known_engine("pi").unwrap_err();
    assert!(matches!(e, UnknownEngine::NoProfile { .. }), "{e:?}");
    let text = e.to_string();
    assert!(text.contains("engine/profiles/pi.toml"), "{text}");
    assert!(text.contains("cline, fake-acp, opencode"), "{text}");
    assert!(matches!(known_engine("Bad"), Err(UnknownEngine::Name(_))));
    assert!(matches!(
        known_engine("../cline"),
        Err(UnknownEngine::Name(_))
    ));
    assert!(bundled_profile(Engine::new("pi").unwrap()).is_none());
}
