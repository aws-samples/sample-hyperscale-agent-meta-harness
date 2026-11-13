# fixtures/ntier

## Purpose

Three sample projects (`web`, `events`, `enterprise`): the one-prompt request for each and the clean-room specification that scores the tests of the delivered zip. `<kind>/prompt.md` and `<kind>/cleanroom.json` hold them.

## Ownership

Owned by `fixtures/AGENTS.md`.

## Local Contracts

- `<kind>/` for `web` (React and TypeScript UI, FastAPI API, PostgreSQL), `events` (Go HTTP service and queue worker, RabbitMQ, PostgreSQL) and `enterprise` (Spring Boot web UI and REST API, PostgreSQL through Testcontainers). The events prompt asks for Go 1.27 and a `golang:1.27` clean room: the module store's pgx v5.11.0 needs go 1.25.0 and its golang.org/x pins go 1.26.0, so the lock step's `go mod tidy` raises any lower `go` line (Go orders 1.26 below 1.26.0), and the builds use the toolchain the approved list names (go1.27.1, its `# go:` header) with GOTOOLCHAIN=local (`scripts/tests/test_ntier_events_prompt.py` holds the prompt to the measured
  directives and the approved list's `# go:` header); the unit, integration and e2e tier images of `events/cleanroom.json` run that Go version (`golang:1.27-bookworm` by digest; the official golang images set GOTOOLCHAIN=local, and the same test checks every tier image against the prompt's version). The prompt's test scripts run `go run gotest.tools/gotestsum@v1.13.0`: v1.12.0, the earlier pin, does not build on Go 1.27.1 (golang.org/x/tools v0.19.0, internal/tokeninternal: invalid array length -delta * delta), so a script that followed the prompt failed the unit tier in the clean room too. `scripts/tests/test_ntier_events_prompt.py` class
  ToolVersions refuses a tool version known not to build and requires every tool the prompt runs to be a tool root of the direct module store (`scripts/gomod-tool-roots.txt`); with `HAMH_FIXTURE_TOOLCHAIN=1` it also builds each named tool with the real Go.
- `<kind>/prompt.md`: the prompt, typed into the composer as one line (its words joined by single
  spaces). It asks for unit tests per tier, integration tests against the real data tier and broker
  from the project's own root `compose.yaml`, and at least one end-to-end test, and names each test
  script and the directory its JUnit XML goes to.
- `<kind>/cleanroom.json` (`live::spec::TierSpec`, unknown keys refused): the Compose file names,
  the services the file must define, `runtimeSecrets` (names of per-run random secrets the clean room makes, passes to Compose
  and the tiers by name and scrubs; `events` declares `BROKER_PASSWORD`, and an `env` value references it as `${BROKER_PASSWORD}`;
  `tier_spec` refuses an `env` value that holds a password), the Compose CLI image, the `docker:<version>-dind-rootless`
  image of the clean room's unprivileged Docker daemon for Testcontainers (`enterprise` only), the
  environment of every tier command (service URLs on the
  Compose network), each tier command (`name`, `tier` of `unit`, `integration` or `e2e`, toolchain
  `image`, `command`, `junit` directory), the `egress` allowlist of the clean room's proxy (the
  exact registry hosts the kind's toolchains need: npm and PyPI for `web`, the Go module proxy and
  checksum database for `events`, Maven Central and Docker Hub for `enterprise`) and the
  per-command timeout. Every image is pinned by
  digest (Docker Hub index digests); every command and JUnit directory must appear in
  the prompt, and each of the three tiers must appear once or more.

## Work Guidance

- Change a prompt and its `cleanroom.json` together; `live_spec` checks that they agree.

## Verification

```bash
python3 -m unittest
```

## Child DOX Index

None.
