//! The `cargo` prefix: the paths a cargo client asks a sparse registry for, and the rewrite of the registry's `config.json`.
//!
//! Cargo (`sparse+http://127.0.0.1:<port>/cargo/`) asks for three kinds of path below the prefix:
//!
//! * `/config.json`, once per cargo run;
//! * the index file of a crate: `/1/<name>`, `/2/<name>`, `/3/<first letter>/<name>` or `/<first two letters>/<next two letters>/<name>`,
//!   with the name in lower case;
//! * the crate file: `/crates/<name>/<version>` (the `dl` template of the config that [`rewrite_config`] writes).
//!
//! Anything else (the registry's API base, a listing, a path of another shape) is refused before the upstream is asked, so the prefix
//! is a read-only window on crates and nothing else of the repository.

/// The path of the registry configuration, below the prefix.
pub(crate) const CONFIG_PATH: &str = "/config.json";
/// The most bytes of the upstream's `config.json` that are read to be rewritten (the real one is under 300).
pub(crate) const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Longest crate name crates.io accepts.
const MAX_NAME: usize = 64;
/// Longest version text taken (semantic versions with pre-release and build parts are short; the path limit is 2,048 bytes anyway).
const MAX_VERSION: usize = 128;

/// A crate name as it appears in a download path: letters, digits, `-` and `_`, starting with a letter or a digit.
fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A crate name as it appears in an index path: cargo writes it in lower case.
fn index_name_ok(name: &str) -> bool {
    name_ok(name) && !name.bytes().any(|b| b.is_ascii_uppercase())
}

/// A version as it appears in a download path: it starts with a digit (so it is never `.` or `..`), then letters, digits and `. - _ +`.
fn version_ok(version: &str) -> bool {
    version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && version.len() <= MAX_VERSION
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

/// Whether `rest` (the raw path below the prefix: empty, or starting with `/`) is one cargo asks for. The text is judged as sent: a
/// percent escape never matches a name, and a directory that does not belong to the name is not an index path.
pub(crate) fn path_ok(rest: &str) -> bool {
    let Some(path) = rest.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["config.json"] => true,
        ["crates", name, version] => name_ok(name) && version_ok(version),
        ["1", name] => index_name_ok(name) && name.len() == 1,
        ["2", name] => index_name_ok(name) && name.len() == 2,
        ["3", dir, name] => index_name_ok(name) && name.len() == 3 && name.get(..1) == Some(*dir),
        [first, second, name] => {
            // The name is ASCII once `index_name_ok` has passed, so the byte slices below fall on character boundaries.
            index_name_ok(name)
                && name.len() >= 4
                && name.get(..2) == Some(*first)
                && name.get(2..4) == Some(*second)
        }
        _ => false,
    }
}

/// The registry configuration the proxy hands to cargo, built new once `body` (the upstream's `config.json`) has been seen to be a JSON
/// object: `dl` points at the proxy (`local` is `http://127.0.0.1:<port>`) and keeps cargo's `{crate}` and `{version}` markers, and
/// `auth-required` is false so cargo looks for no token. There is no `api`, so `cargo publish`, `yank` and `owner` find no web API, and no
/// other key: nothing the upstream sent reaches cargo, whatever it is called and whatever cargo learns to read. `None` when `body` is not a
/// JSON object.
pub(crate) fn rewrite_config(body: &[u8], local: &str) -> Option<Vec<u8>> {
    let seen: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !seen.is_object() {
        return None;
    }
    serde_json::to_vec(&serde_json::json!({
        "dl": format!("{local}/cargo/crates/{{crate}}/{{version}}"),
        "auth-required": false,
    }))
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_shapes_cargo_asks_for_are_served() {
        for p in [
            "/config.json",
            "/1/a",
            "/2/ab",
            "/3/a/abc",
            "/it/oa/itoa",
            "/se/rd/serde",
            "/pr/oc/proc-macro2",
            "/ti/ny/tiny_http",
            "/ab/cd/abcd",
            "/12/34/1234",
            "/crates/serde/1.0.228",
            "/crates/Inflector/0.11.4",
            "/crates/openssl-src/300.5.4+3.5.4",
            "/crates/foo/1.0.0-rc.1",
            "/crates/7z/10",
            "/crates/a_b-c/0.1.0",
            "/7z/ip/7zip",
        ] {
            assert!(path_ok(p), "{p} should be served");
        }
    }

    #[test]
    fn everything_else_is_not() {
        for p in [
            "",
            "/",
            "config.json",
            "/config.json/",
            "/config.json/x",
            "/-/api/v1/crates/new",
            "/api/v1/crates",
            "/crates",
            "/crates/",
            "/crates/serde",
            "/crates/serde/1.0.0/download",
            "/crates/serde/",
            "/crates//1.0.0",
            "/1/ab",
            "/2/a",
            "/2/abc",
            "/3/a/bcd",
            "/3/ab/abc",
            "/3/a/ab",
            "/it/oa",
            "/it/oa/ito",
            "/xx/yy/itoa",
            "/It/oa/Itoa",
            "/it/oa/it%61",
            "/it/oa/it@a",
            "/it/oa/it;a",
            "/a/b/c/d/e",
            "/crates/it:oa/1.0.0",
            "/crates/itoa/1.0.0!",
            "/crates/ito%61/1.0.0",
            // a version starts with a digit: nothing like a dot segment, a sign or a word
            "/crates/serde/.",
            "/crates/serde/..",
            "/crates/serde/...",
            "/crates/serde/-1.0.0",
            "/crates/serde/+1.0.0",
            "/crates/serde/_1",
            "/crates/serde/v1.0.0",
            // a name starts with a letter or a digit
            "/crates/-x/1.0.0",
            "/crates/_x/1.0.0",
            "/-b/cd/-bcd",
            "/_a/bc/_abc",
            "/1/_",
            "/2/-a",
            "/3/-/-ab",
        ] {
            assert!(!path_ok(p), "{p:?} should be refused");
        }
    }

    #[test]
    fn the_config_points_at_the_proxy_asks_for_no_token_and_has_no_api() {
        let up = br#"{"dl":"https://h.example/cargo/r/crates/{crate}/{version}","api":"https://h.example/cargo/r/-","auth-required":true,"x":[1,{"y":null}]}"#;
        let out = rewrite_config(up, "http://127.0.0.1:3128").unwrap_or_default();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap_or_default();
        assert_eq!(
            v["dl"],
            "http://127.0.0.1:3128/cargo/crates/{crate}/{version}"
        );
        assert_eq!(v["auth-required"], false);
        // Built new: the upstream's `api` and its other keys are not in it.
        // The order of the keys is not part of the contract: the workspace build turns on serde_json's preserve_order (through aws-smithy),
        // and cargo reads the file as a map.
        let mut keys: Vec<&str> = v
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        keys.sort_unstable();
        assert_eq!(keys, ["auth-required", "dl"]);
    }

    #[test]
    fn what_is_not_a_json_object_is_refused() {
        for body in ["", "<html>", "[]", "7", "\"x\"", "null", "{\"dl\":"] {
            assert!(
                rewrite_config(body.as_bytes(), "http://127.0.0.1:1").is_none(),
                "{body:?}"
            );
        }
    }

    #[test]
    fn a_name_longer_than_crates_io_allows_is_refused() {
        let long = "a".repeat(65);
        assert!(!path_ok(&format!("/aa/aa/{long}")));
        assert!(!path_ok(&format!("/crates/{long}/1.0.0")));
        let edge = "a".repeat(64);
        assert!(path_ok(&format!("/aa/aa/{edge}")));
        assert!(path_ok(&format!("/crates/{edge}/1.0.0")));
        assert!(!path_ok(&format!("/crates/serde/{}", "1".repeat(129))));
    }
}
