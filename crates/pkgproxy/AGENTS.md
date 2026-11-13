# crates/pkgproxy

## Purpose

The loopback package proxy of a direct run. Cline installs what its
tests need (pip, npm, Maven, Go, cargo) while the CodeArtifact token stays out of its reach: agentd starts the proxy on `127.0.0.1`,
the engine's package tools point at it through `Handle::env()`, the Maven `settings.xml` and the cargo `config.toml`, and the proxy adds
the credential to the upstream request.

## Ownership

Owned by `crates/AGENTS.md`. `agentd` depends on this crate (the direct driver starts it); this crate depends on nothing in
the workspace.

## Local Contracts

- The public API is the contract: `Secret`, `Prefix` (`Pypi`, `Npm`, `Maven`, `Go`, `Cargo`), `Auth` (`None`, `Basic`, `Bearer`, and `Raw`, the secret as the whole
  `Authorization` value), `Upstream`, `Log`, `Config`, `start`, `Handle`
  (`addr`, `env`, `maven_settings_xml`, `cargo_config_toml`, `snapshot`, `stop`), `StatsSnapshot`, `codeartifact_upstreams`, `go_upstream`.
  `tests/api_surface.rs` pins every signature. A signature change needs a contract amendment, not an edit. The `testing`
  feature adds `pkgproxy::testing` (a fixed DNS table, private trusted roots, a fake clock) for the integration tests; no
  production code calls it, and `start` always resolves through the system resolver and verifies against the webpki roots.
- The token lives in agentd's memory only: no file, no argument, no log line, no error text. A `Secret` prints `<redacted>`
  through `Debug` and `Display` and overwrites its own buffer with zeros on drop (a test-only probe reads the bytes at release). The `Authorization` value is built per request from the `Secret`
  and marked sensitive; the proxy holds one `Secret` per credentialed upstream (`codeartifact_upstreams` makes four), each zeroized on
  drop, and no other long-lived copy; the token is never in the engine's environment, in a file or in `summary.json`. The client's `Authorization`, `Cookie` and `Proxy-*`
  never reach an upstream, and no upstream header other than the listed ones reaches a client. The engine uid can use the
  proxy (any process on 127.0.0.1 can, by design) and cannot read the token: it is in no file, and the kernel gives
  `/proc/<pid>/{environ,mem}` of agentd to root alone, with no group or other permission bit (a metadata check in
  `tests/uid_split.rs`; nothing is read as the engine uid). The proxy opens no unix socket and writes no
  file, so there is no socket or directory mode to get wrong.
- `start` refuses (InvalidInput, no secret in the text): a listen address other than `127.0.0.1`, an upstream base that is not
  `https://`, has userinfo, a query, a fragment or a bad host, two upstreams on one prefix, a credential that cannot be a header
  value, zero `max_connections` or a zero timeout. A busy port is an `AddrInUse` error, not a silent rebind: the caller retries
  with port 0 (agentd tries 3128, then 0) and reads the real port from `Handle::addr`.
- Request order and statuses: method (405, `Allow: GET, HEAD`; `CONNECT` is a 405), origin-form target (400), `Host` exactly
  `127.0.0.1:<port>` and once (400), no request body (400), path rules (400: percent-decoded once for checking only, a `.` or
  `..` segment or one made of dots only, an empty segment, a `%2f` run, any decoded byte outside letters, digits and `- . _ ~ ! $ & ' ( ) * + , = : @ /` (so no NUL, backslash, `;`, `%25`, blank, control, non-ASCII, `?` or `#`), a bad escape, over 8,192 path bytes or 8,192 query bytes, any query on the `go` prefix), prefix
  one of the five configured names compared as sent (403), on `/cargo` a path cargo asks for and no query (400, below), token not expired (401, credentialed prefixes
  only; `/go` is unaffected; the token is expired from the second `expires_at_unix` itself; on `/cargo/config.json` alone the answer is 403, below), then the upstream request. The raw (still encoded) path and query go upstream verbatim, so `@scope%2fname` keeps
  its `%2f`.
- Upstream: connect 10 s (`net.rs` pins it), `idle_timeout` between bytes (also the wait for each response head, so every redirect
  hop gets a fresh wait), `request_timeout` per request, a redirect `Location` of at most 8,192 bytes,
  at most `max_redirects` redirects (`301 302 303 307 308`) to `https` on port 443 at a host that ends `.amazonaws.com` (or back
  to the upstream's own origin, and then only under its configured base path with no dot or empty segment: the token covers the whole CodeArtifact domain), credential only to the upstream's own origin, otherwise `502 redirect refused`; an upstream
  failure is `502 upstream error: <class>` (`dns`, `blocked`, `tls`, `timeout`, `reset`, `connect`, `protocol`) with no URL or
  host in it. Resolved loopback, link-local, unspecified and multicast addresses are refused.
- Retry: GET and HEAD are the only methods the proxy accepts, so every upstream request is idempotent. A hop is sent again, at most twice (`UPSTREAM_RETRIES`),
  after 250 ms and then 500 ms plus up to half of each as jitter, when the connection was reset, closed or refused (`reset`, `connect`) or the upstream answered a head of
  429, 502, 503 or 504; only while the request deadline leaves room for the wait. A wait for the head that timed out is not retried (it would triple a 60 s wait), and neither
  are a name that did not resolve, a blocked address, an untrusted certificate or any other status (500, 404, 403 and the rest pass through). When the tries run out the last
  upstream answer goes through as it is, and a failed connection is the same `502 upstream error: <class>`. The credential goes to the upstream's own origin on every try and
  to no redirect target. A retry is one log line, `pkgproxy <prefix> retry <n> <class or status>`, with no host and no token; the counters count the client's request once.
  An answer that is `response too large to rewrite` is deterministic and a retry cannot help.
- Rewrite: on the `pypi` and `npm` prefixes only (Maven and Go answers are checksummed by their clients and pass byte for byte), a
  textual answer (`text/*`, an `application` type of JSON or XML, `application/vnd.pypi*`, `application/vnd.npm*`)
  that is a full GET answer has every absolute URL that starts with an upstream base (plain and JSON-escaped, at a URL
  boundary) rewritten to `http://127.0.0.1:<port>/<prefix>`. Over 64 MiB or with a `Content-Encoding` the proxy cannot undo it
  answers 502; a `206`, a `Content-Range`, a HEAD and every other type pass unrewritten. At most 4 answers are held for rewriting at once, each slot rides on the rewritten `Bytes` and is released when hyper has written them or the connection is cut (the connection writes vectored so hyper queues the bytes instead of copying them); `Accept-Ranges` is dropped from a rewritten answer; a fifth waits one idle period, then gets `503 proxy busy`. Upstream trailers are dropped. All other bodies stream: never held
  whole, cut with an error (a short body, never a padded one) when the upstream is idle or the request deadline passes. `tests/rewrite.rs` `a_rewritten_answer_keeps_its_slot_until_it_has_been_sent` runs with an idle time of 8 s and blocks every worker of its runtime for 2.6 s while the four answers are on their way: a stall of the process shorter than the idle time must not fail an answer (an idle time of 2 s failed it once in CI, 502 "upstream error: timeout", which frees a slot).
- Cargo: `/cargo` is a CodeArtifact cargo repository (`https://<host>/cargo/<repo>`, external connection `public:crates-io`) behind the credential
  shape `Auth::Raw`: CodeArtifact takes the token as the WHOLE `Authorization` value and answers 401 to `Bearer <token>` (measured on stage direct). The engine's
  cargo is pointed at it by the file `Handle::cargo_config_toml()` returns (agentd writes it as `$CARGO_HOME/config.toml`): `[source.crates-io] replace-with = "hamh-proxy"`,
  `[registries.hamh-proxy] index = "sparse+http://127.0.0.1:<port>/cargo/"` (a sparse URL ends in a slash), `[net] retry = 3`, `[term] color = "never"`; no token, no
  credential provider, no secret. `Handle::env()` is unchanged.
  * Paths (`cargo.rs`): only what cargo asks for is served, read only: `/config.json`, the index file of a crate in lower case (`/1/<n>`, `/2/<n>`, `/3/<c>/<n>`, `/<aa>/<bb>/<n>`,
    with the directories belonging to the name) and `/crates/<name>/<version>`. A name starts with a letter or a digit (a download name keeps its case, an index name is lower case),
    a version with a digit (so never `.` or `..`). Any other path, a name over 64 bytes, a percent escape in a name and any query are 400 before the upstream. The shared rules
    still apply (methods, path rules, expiry, redirects only to https `*.amazonaws.com` and never with the credential, retry, streaming).
  * Index and crate answers are opaque and streamed whatever their `Content-Type` says (the rewrite of pypi and npm never runs for cargo; measured: the real service sends
    an octet-stream type, chunked for an index). Only `Content-Type`, `ETag` and `Last-Modified` come back (the framing is the proxy's); `Cache-Control`, `Content-Disposition`,
    `Accept-Ranges`, `Content-Range`, cookies, a challenge and every other header of the upstream do not. `If-None-Match` and `If-Modified-Since` go up and a 304 stays a 304;
    `Range` and `If-Range` do not go up (cargo never sends them), and an answer that is a 206 or names a `Content-Range` is `502 unexpected answer from the package registry`.
  * `config.json` is built new by the proxy on each request (CodeArtifact's names its own host in `dl` and `api` and says `auth-required: true`): exactly
    `{"auth-required": false, "dl": "http://127.0.0.1:<the Host's port>/cargo/crates/{crate}/{version}"}`. No `api` (the engine cannot publish) and no other key: nothing the upstream
    sent reaches cargo. Only an unencoded, whole 200 that is a JSON object of at most 64 KiB (`MAX_CONFIG_BYTES`) is taken; it is read and parsed under a rewrite slot (the admission
    control of the pypi and npm rewrites: a fifth waits one idle period and gets `503 proxy busy`). Every other answer is a fixed text of the proxy and never the upstream's body or
    headers, for HEAD as for GET: `502 response too large to rewrite` (over 64 KiB), `502 encoded text cannot be rewritten`, `502 registry config is not a JSON object`, `502 unexpected
    answer for the registry config` (a 206, a `Content-Range`, a 2xx but 200, a 3xx), `502 package registry has no cargo config` (a 404: cargo would read it as "no matching package"),
    `403 package registry refused the token` (a 401), and a 4xx or 5xx keeps its number (cargo keys its retries on it) with `package registry answered <n> for the registry config`.
    The answer carries `Content-Type: application/json` and its own `Content-Length` (none for HEAD) and no validator, caching rule or range; the client's `If-None-Match`,
    `If-Modified-Since`, `Range` and `If-Range` are not sent up for this path, and `Accept-Encoding: identity` is, whatever the client offers (the real service answers plain text to
    `gzip, deflate, br`). Nothing is kept between requests, so a failed fetch is never remembered.
  * 401 on `/cargo/config.json` is answered 403 (the proxy's own expiry refusal `package token expired`, and a 401 from the registry as `package registry refused the token`, its
    challenge is not passed): cargo 1.98.1 reads a 401 for config.json as "this registry needs a credential" and prints advice about credential providers that no package token can
    act on, while a 403 prints `got 403 body: <text>`. Every other `/cargo` path keeps 401 for expiry and the registry's own status otherwise (cargo prints the body of a 401 for an
    index file).
  * Trust boundary: CodeArtifact is trusted not to reflect the credential it was sent. The proxy filters the representation (status, headers, the content of the config), not the bytes
    of an index or crate body, and does not look for the token in them. The engine cannot reach the token through the proxy's own answers.
  * `git = "..."` dependencies, other registries, `cargo publish`, vendoring and a toolchain download are out of scope; a git dependency fails at the egress list.
- Lockfile links: what `npm install` writes into `package-lock.json` (`resolved`) is the tarball link of the packument, so a loopback link
  there is a file no one outside the session can use (runs have committed hundreds of such entries). On the `npm` prefix the
  rewrite therefore depends on the request's `User-Agent`: a client whose first product token is `npm/` (the npm CLI: `npm/10.9.9 node/v22...`,
  `npm/11.19.0 node/v24...`) gets the tarball links of the upstream rewritten to `https://registry.npmjs.org/<name>/-/<file>.tgz` and fetches
  them through the loopback registry by npm's own `replace-registry-host` (default `npmjs`, named in `Handle::env()`); every other client
  (pnpm, yarn, bun, curl, none) keeps the `http://127.0.0.1:<port>/npm/...` links, which it must follow literally. Every textual answer of the
  `npm` prefix carries `Vary: User-Agent`. Measured with real clients: npm 10.9.9 and 11.19.0 write the link they were given;
  pnpm 12.6.0 writes no URL into `pnpm-lock.yaml` and fails on a `registry.npmjs.org` link (no host replacement); yarn 1.22.22 writes
  `resolved` links and fails on a `registry.npmjs.org` link; `uv lock` writes the index and file URLs it fetched from into `uv.lock`, and no
  setting hides them. So yarn.lock and uv.lock made in a session are not portable; the rules forbid committing them, and a check of the
  committed tree is backlog. pip, `pip freeze`, `go.mod`, `go.sum` and `uv pip compile` (without `--emit-index-url`) write no URL.
  The `ETag` stays the upstream's: a `304` revalidates the variant the client's own cache holds, and one client keeps one `User-Agent`. A
  project or user `.npmrc` that overrides `user-agent` makes npm look like another client and brings the loopback links back.
- Connections: at most `max_connections` (the next ones get `503` and are closed; at most 64 of those at once, then dropped), a
  client has `idle_timeout`, at most 30 s, to send a request head, a connection with no byte moving for `idle_timeout` and nothing
  in flight is closed (twice that with a request in flight, so a client that stopped reading frees its slot), a peer that is not
  loopback is closed, `stop` aborts every connection at once (it never waits for an idle time) and frees the port.
- Counters (`StatsSnapshot`): `requests` and `statuses` count every answer; `by_prefix` counts requests that reached a configured
  prefix; `refused` counts answers the proxy decided itself (400, 401, 403, 405, 503, a refused redirect, text it cannot rewrite);
  `bytes` counts body bytes sent to clients. Log lines are `pkgproxy <prefix|-> <status> <bytes> <ms>`, written when the body ends.
- Modules: `server.rs` (listener, connection loop, request path, the upstream retry), `cargo.rs` (the paths of the cargo prefix and the `config.json` rewrite), `target.rs` (request-target checks), `redirect.rs`
  (Location policy), `rewrite.rs` (link rewrite), `net.rs` (resolver, TLS, pooled client, error classes), `body.rs` (bodies,
  per-request bookkeeping), `stats.rs`.

## Work Guidance

- No dependency on `agentd`, `harness-core` or any AWS SDK crate.
- `tests/cargo_client.rs` runs the cargo that runs the test (`CARGO`, a scrubbed environment) against the proxy with the exact `config.toml`, a fake registry over HTTPS and a `.crate` made in the
  test (`cargo fetch --locked`, then `cargo build --locked --offline`); it needs cargo and nothing else. A change to the registry config or to the headers is run through it first.
- Tests run against fake HTTPS upstreams: `tests/common/mod.rs` makes a certificate authority per run with the `openssl` command
  (only options OpenSSL 1.0.2 takes) and never commits a key. Test tokens are built at run time (`security/AGENTS.md`).

## Verification

```bash
scripts/cargo-slot.sh cargo nextest run -p pkgproxy
# real npm (10 and 11), pnpm 12 (installed from the public registry) and Go clients in containers (Docker, --network host), and the case that runs as root in a container:
scripts/cargo-slot.sh cargo nextest run -p pkgproxy --test clients --run-ignored ignored-only
docker run --rm --user 0 -v <uid_split test binary>:/t:ro --tmpfs /tmp node:24-bookworm /t --ignored --nocapture
```

`PKGPROXY_REQUIRE_CLIENTS=1` turns a missing `python3` or `uv` (the host pip and uv tests of `tests/clients.rs`) into a failure
instead of a skip line.

## Child DOX Index

None.
