//! `npm install` writes the tarball URL of the packument into
//! `package-lock.json` (`resolved`). The proxy rewrote CodeArtifact's tarball links to `http://127.0.0.1:<port>/npm/...`, so two
//! runs committed a lockfile that no `npm ci` outside the session can use (211 and 285 entries). npm replaces the host of a
//! `registry.npmjs.org` URL with the configured registry when it fetches (`replace-registry-host`, default `npmjs`), so the npm
//! CLI is given tarball links on that host and fetches them through the loopback registry; every other client (pnpm, yarn,
//! bun, curl) gets the loopback links, which it must follow literally. The choice is the request's `User-Agent`, and an answer
//! that depends on it says so with `Vary`.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

const NPM_11: &str = "npm/11.19.0 node/v24.21.0 linux x64 workspaces/false";
const NPM_10: &str = "npm/10.9.9 node/v22.23.3 linux x64 workspaces/false";
const PNPM: &str = "pnpm/12.6.0 npm/? node/? linux x64";
const YARN: &str = "yarn/1.22.22 npm/? node/v24.21.0 linux x64";

fn packument() -> String {
    format!(
        r#"{{"name":"left-pad","versions":{{"1.0.0":{{"dist":{{"tarball":"https://{CA_HOST}/npm/packages/left-pad/-/left-pad-1.0.0.tgz","shasum":"x"}}}}}},"esc":"https:\/\/{CA_HOST}\/npm\/packages\/left-pad\/-\/left-pad-2.0.0.tgz","scoped":"https://{CA_HOST}/npm/packages/@lk/c/-/c-1.0.0.tgz","other":"https://github.com/example/left-pad"}}"#
    )
}

async fn rig_with_packument() -> Rig {
    rig(
        always(Reply::ok(packument()).header("Content-Type", "application/json")),
        nothing(),
        nothing(),
    )
    .await
}

fn assert_public(body: &str) {
    assert!(
        body.contains("\"https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz\""),
        "{body}"
    );
    assert!(
        body.contains("https:\\/\\/registry.npmjs.org\\/left-pad\\/-\\/left-pad-2.0.0.tgz"),
        "the JSON-escaped form is rewritten too: {body}"
    );
    assert!(
        body.contains("\"https://registry.npmjs.org/@lk/c/-/c-1.0.0.tgz\""),
        "a scoped name keeps its scope: {body}"
    );
    assert!(!body.contains("127.0.0.1"), "no loopback URL: {body}");
    assert!(!body.contains(CA_HOST), "no upstream host: {body}");
    assert!(
        body.contains("https://github.com/example/left-pad"),
        "another host is left alone: {body}"
    );
}

fn assert_loopback(body: &str, port: u16) {
    assert!(
        body.contains(&format!(
            "\"http://127.0.0.1:{port}/npm/left-pad/-/left-pad-1.0.0.tgz\""
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "http:\\/\\/127.0.0.1:{port}\\/npm\\/left-pad\\/-\\/left-pad-2.0.0.tgz"
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "\"http://127.0.0.1:{port}/npm/@lk/c/-/c-1.0.0.tgz\""
        )),
        "{body}"
    );
    assert!(!body.contains("registry.npmjs.org"), "{body}");
    assert!(!body.contains(CA_HOST), "{body}");
}

#[tokio::test]
async fn the_npm_cli_gets_tarball_links_on_the_public_registry_host() {
    let r = rig_with_packument().await;
    for ua in [NPM_11, NPM_10] {
        let resp = get_with(r.addr(), "/npm/left-pad", &[("User-Agent", ua)]).await;
        assert_eq!(resp.status, 200, "{ua}");
        let body = resp.text();
        assert_public(&body);
        assert_eq!(
            resp.header("content-length"),
            Some(body.len().to_string().as_str()),
            "the length is the rewritten one ({ua})"
        );
        assert!(resp.complete, "{ua}");
    }
}

#[tokio::test]
async fn every_other_client_keeps_the_loopback_tarball_links() {
    let r = rig_with_packument().await;
    let port = r.addr().port();
    for ua in [
        Some(PNPM),
        Some(YARN),
        Some("Bun/1.4.2"),
        Some("curl/8.5.0"),
        // The product token must be the first one: a client that only mentions npm is not the npm CLI.
        Some("Mozilla/5.0 npm/11.19.0"),
        Some("NPM/11.19.0 node/v24.21.0"),
        Some("npmx/1.0"),
        None,
    ] {
        let extra: Vec<(&str, &str)> = ua.map(|u| ("User-Agent", u)).into_iter().collect();
        let resp = get_with(r.addr(), "/npm/left-pad", &extra).await;
        assert_eq!(resp.status, 200, "{ua:?}");
        assert_loopback(&resp.text(), port);
    }
}

#[tokio::test]
async fn an_answer_that_depends_on_the_user_agent_says_so() {
    let r = rig_with_packument().await;
    for ua in [NPM_11, PNPM] {
        let resp = get_with(r.addr(), "/npm/left-pad", &[("User-Agent", ua)]).await;
        let vary: Vec<String> = resp
            .headers_named("vary")
            .iter()
            .map(|v| v.to_ascii_lowercase())
            .collect();
        assert!(
            vary.iter().any(|v| v.contains("user-agent")),
            "Vary names User-Agent for {ua}: {:?}",
            resp.headers
        );
    }
}

#[tokio::test]
async fn the_pypi_prefix_does_not_look_at_the_user_agent() {
    let page = format!(
        "<html><body><a href=\"https://{CA_HOST}/pypi/packages/simple/demo/demo-1.0-py3-none-any.whl#sha256=abc\">demo</a></body></html>"
    );
    let r = rig(
        always(Reply::ok(page).header("Content-Type", "text/html")),
        nothing(),
        nothing(),
    )
    .await;
    let port = r.addr().port();
    let resp = get_with(r.addr(), "/pypi/simple/demo/", &[("User-Agent", NPM_11)]).await;
    let body = resp.text();
    assert!(
        body.contains(&format!(
            "href=\"http://127.0.0.1:{port}/pypi/simple/demo/demo-1.0-py3-none-any.whl#sha256=abc\""
        )),
        "{body}"
    );
    assert!(!body.contains("registry.npmjs.org"), "{body}");
    assert!(
        resp.headers_named("vary").is_empty(),
        "the pypi answer does not depend on the client: {:?}",
        resp.headers
    );
}

#[tokio::test]
async fn a_tarball_download_is_not_touched_and_does_not_vary() {
    let tgz: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let body = tgz.clone();
    let r = rig(
        handler(move |_| {
            Reply::ok(body.clone()).header("Content-Type", "application/octet-stream")
        }),
        nothing(),
        nothing(),
    )
    .await;
    let resp = get_with(
        r.addr(),
        "/npm/left-pad/-/left-pad-1.0.0.tgz",
        &[("User-Agent", NPM_11)],
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, tgz);
    assert!(resp.headers_named("vary").is_empty(), "{:?}", resp.headers);
}

#[tokio::test]
async fn a_head_request_gets_the_same_headers_as_the_get_it_stands_for() {
    let r = rig_with_packument().await;
    let resp = send_raw(
        r.addr(),
        &request("HEAD", "/npm/left-pad", r.addr(), &[("User-Agent", NPM_11)]),
        true,
    )
    .await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
    assert!(
        resp.headers_named("vary")
            .iter()
            .any(|v| v.to_ascii_lowercase().contains("user-agent")),
        "{:?}",
        resp.headers
    );
}

#[tokio::test]
async fn env_tells_npm_which_host_to_replace_and_pnpm_which_registry_to_use() {
    let r = rig(nothing(), nothing(), nothing()).await;
    let p = r.addr().port();
    let env = r.proxy.env();
    // pnpm 12 ignores `npm_config_registry` (measured 2026-10-15 with pnpm 12.6.0: it asked registry.npmjs.org and got no answer
    // inside the session); it reads `pnpm_config_registry`.
    assert_eq!(
        env.get("pnpm_config_registry").map(String::as_str),
        Some(format!("http://127.0.0.1:{p}/npm/").as_str())
    );
    // The default is already `npmjs`; naming it keeps a user or project setting of another value from sending the tarball
    // requests of a public-host lockfile to the real registry.
    assert_eq!(
        env.get("npm_config_replace_registry_host")
            .map(String::as_str),
        Some("npmjs")
    );
}
