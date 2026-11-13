//! The CodeArtifact token on the wire (docs-versus-code report 7.4 and 6.4): `PackagesToken` is the one type that carries it
//! into agentd. It is a plain string on the wire (the invocation member `packagesToken`), prints `<redacted>` through `Debug`
//! and `Display`, gives its text through `expose` alone, and wipes its buffer when it is dropped. The wipe itself is probed by a
//! unit test in `src/runtime.rs` (the bytes are private); here the type's claim to it is pinned at compile time, and the exported
//! schema is pinned to the released plain string.

#![allow(clippy::unwrap_used)]

use harness_protocol::export::all_schemas;
use harness_protocol::runtime::PackagesToken;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A token stand-in, built at run time so no token-shaped literal sits in the source.
fn token() -> String {
    ["tok", "test", "0123456789"].join("-")
}

/// The description of `$defs/PackagesToken` in the released `runtime.schema.json` (protocol 1.38.0). The schema keeps it as
/// released, like `RepoTarget`'s, so the type's own documentation can move without a version bump.
const RELEASED_DESCRIPTION: &str = "A CodeArtifact authorization token the dispatcher minted outside the VM with its own role; agentd writes it only to `package_index_path`, never where the\nengine can read it. `Debug` never shows it.";

#[test]
fn debug_and_display_print_redacted_and_never_the_token() {
    let t = PackagesToken::new(token());
    assert_eq!(format!("{t:?}"), "<redacted>");
    assert_eq!(format!("{t}"), "<redacted>");
    assert_eq!(t.to_string(), "<redacted>");
    let shown = format!("{t:?} {t} {:?} {:#?}", Some(&t), vec![&t]);
    assert!(!shown.contains(&token()), "{shown}");
}

#[test]
fn the_token_is_a_plain_string_on_the_wire_and_round_trips() {
    let t = PackagesToken::new(token());
    let wire = serde_json::to_string(&t).unwrap();
    assert_eq!(wire, format!("\"{}\"", token()));
    let back: PackagesToken = serde_json::from_str(&wire).unwrap();
    assert_eq!(back, t);
    assert_eq!(back.expose(), token());
    assert_eq!(t.clone(), t);
    assert_ne!(PackagesToken::new(["tok", "other"].join("-")), t);
}

/// A struct that carries the token under the member name of the invocation request.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Carrier {
    packages_token: PackagesToken,
}

#[test]
fn the_member_is_packages_token_and_holds_the_string_itself() {
    let c = Carrier {
        packages_token: PackagesToken::new(token()),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v, json!({ "packagesToken": token() }));
    let back: Carrier = serde_json::from_value(v).unwrap();
    assert_eq!(back, c);
    // A containing type's derived `Debug` shows the token redacted.
    assert_eq!(
        format!("{back:?}"),
        "Carrier { packages_token: <redacted> }"
    );
}

#[test]
fn expose_reads_the_text_and_shape_ok_is_unchanged() {
    let t = PackagesToken::new(token());
    assert_eq!(t.expose(), token());
    assert!(t.shape_ok());
    assert!(PackagesToken::new("a.b_c=d-E9".into()).shape_ok());
    assert!(!PackagesToken::new(String::new()).shape_ok());
    assert!(!PackagesToken::new("a/b".into()).shape_ok());
    assert!(!PackagesToken::new("a b".into()).shape_ok());
}

#[test]
fn the_token_is_zeroized_on_drop_by_contract() {
    fn wiped<T: zeroize::ZeroizeOnDrop>() {}
    wiped::<PackagesToken>();
}

#[test]
fn the_exported_schema_of_the_token_is_the_released_plain_string() {
    let runtime: Value = all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "runtime.schema.json")
        .unwrap()
        .1;
    let def = &runtime["$defs"]["PackagesToken"];
    // The released 1.38.0 definition, byte for byte: a change here moves the FROZEN hash and needs a version bump.
    assert_eq!(
        def,
        &json!({
            "description": RELEASED_DESCRIPTION,
            "type": "string"
        })
    );
    // The invocation member keeps its reference to that definition, so the request's schema does not move either.
    let member = &runtime["$defs"]["DirectInvocationRequest"]["properties"]["packagesToken"];
    assert_eq!(member["anyOf"][0]["$ref"], json!("#/$defs/PackagesToken"));
}
