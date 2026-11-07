# crates/ham-e2e

## Purpose

`ham-e2e` holds the local fake engine of the project: the `fake-acp-engine` program that stands in for a coding engine in tests, and the library behind it. A test run
that starts it needs no real engine and makes no model call. `crates/agentd` starts it in its tests, and the tests of this crate play it in process and as a child process.

## Ownership

Owned by `crates/AGENTS.md`. The transcripts it replays are the data of `fixtures/engines/fake-acp/transcripts/`; `engine/profiles/fake-acp.toml` is the engine profile that names the program.

## Local Contracts

- `fake-acp-engine` speaks newline-delimited ACP JSON-RPC over stdio. It replays the transcripts of the fixture folder. The library holds the same
  replay as an in-process driver (`acp`) so a test can run an engine and the code that talks to it in one process.
- With `FAKE_ACP_CLINE=1` the program replays nothing. It is a small ACP agent that copies the behaviour of the pinned Cline engine that message control depends on (`fake_cline`): a
  second prompt is queued inside the engine and a later cancel does nothing, a cancel kills the shell tool and leaves its orphans, an engine can hang, die or answer a provider's 503.
  A prompt scripts its turn with one `@fake{...}` JSON object; the table of directives is the header of `src/fake_cline.rs`. Its shell tools are plain `sleep` processes started without a
  shell. `tests/fake_cline_inproc.rs` serves the same engine in process so a coverage run sees its lines.
- `goal_sim` plays the goal plugin of the engine and the observer of the harness: a scenario folder with a `goal.json` makes the engine claim the goal that agentd wrote to the state
  file, go on with the scripted rounds by itself, write the rounds log, ignore `session/cancel`, and refuse the completion call before a verification round.
- `fake_mcp` is the engine's MCP client for the tests of agentd's tool hook: it reads tool servers from a settings file and calls them as a transcript says.

## Work Guidance

- Build the program with `cargo build -p ham-e2e --bin fake-acp-engine`. The tests of `crates/agentd` build it themselves when `HAMH_FAKE_ENGINE_DIR` does not name a directory that holds it.
- A new directive of the fake engine goes into the table in the header of `src/fake_cline.rs`, with a test in `tests/fake_acp_*.rs` or `tests/fake_cline_inproc.rs`.

## Verification

```bash
cargo build -p ham-e2e --bin fake-acp-engine
cargo nextest run -p ham-e2e
```

## Child DOX Index

None.
