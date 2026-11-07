# engine/profiles

## Purpose

One engine profile per engine, `<name>.toml`: launch argv per transport, environment, optional config
emitters, `hiddenPaths`, the binary pin, the qualification gate and its K rows with pass values. The
profiles here are `cline`, `fake-acp` and `opencode`.

## Ownership

Owned by `engine/AGENTS.md`. The schema is `schemas/engine-profile.schema.json`, generated from
`crates/harness-protocol` (`engine_profile`); `crates/harness-core` (`engine_profile`) loads and
validates profiles.

## Local Contracts

- The file stem equals the profile `name`, and the name is the engine (`harness_protocol::common::
  Engine`, `^[a-z][a-z0-9-]{0,31}$`). The build bundles every profile here (harness-core `build.rs`),
  and `known_engine` refuses a name with no profile, so a new engine is this file and
  `fixtures/engines/<name>/` with no Rust or protocol change (`crates/ham-e2e/tests/fake_acp_engine.rs`
  runs `fake-acp` this way).
- `transports.acp` is required (the ACP seam); `transports.cli` is optional. ACP launches read stdin
  from a pipe; CLI launches read `/dev/null`.
- Strings may use only the placeholders `{workspace}`, `{dataDir}`, `{configDir}`, `{model}`,
  `{region}`, `{systemPromptFile}`, `{promptFile}` and `{fixtures}` (= `fixtures/engines/<name>`).
- `env` never sets AWS credentials or a Bedrock API key (root invariant 2).
- `transports.<t>.modelEnv` names the variable the engine reads its model from at ACP session start
  (`CLINE_MODEL` for the fork, patch 0004; `FAKE_ACP_MODEL` for the fake engine); agentd sets it to
  the run's model id when it launches the engine. It follows the env-key rule and is never a
  credential.
- The `cline` profile's ACP launch is `cline --acp --config {configDir} --data-dir {dataDir}` and
  carries no `--auto-approve`. The direct run adds `--auto-approve true` after `--acp` to its own
  launch (`agentd::engine::direct::direct_launch_argv`; the microVM is the boundary, no permission
  request reaches the client). `crates/harness-core/tests/real_profiles.rs` pins the profile's argv
  and `crates/agentd/tests/direct_launch.rs` the direct launch.
- `crates/harness-core/tests/real_profiles.rs` also pins the binary hashes: the `cline` and
  `opencode` profiles carry the SHA-256 values of `engine/lock.toml`.
- `hiddenPaths` are repo-relative; `agentd` hides the union across every profile.
- The optional `[mcp]` table says how the engine learns of the harness's tool server, which serves the `ham` tools (`code_search`
  today). `via = "file"` names the settings file the engine reads its tool servers from (`path`, under `{dataDir}` or `{configDir}`), the
  registration `template` (under `templates/` here) that agentd fills with `{toolsName}` and `{toolsUrl}` once it has bound the port, `maxResultChars`
  (1,000 to 16,000) and `shadow`, the engine built-ins to switch off once their use is counted. `via = "none"` takes nothing else. A profile with no
  table gets no tools. `cline.toml` has the table: `{dataDir}/settings/cline_mcp_settings.json`, `templates/mcp-cline.json`, 7,500 characters (Cline
  cuts a result at 8,000), `shadow = []`. The template says `streamableHttp` in the nested `transport` form and a timeout of 120 seconds
  (`crates/harness-protocol/tests/mcp_template.rs`). `harness-core` checks the table (`crates/harness-core/tests/profile_mcp.rs`).
  agentd's `tools` module reads the table through `StartupCtx::mcp()` and does the rest: it binds the server's port, fills the template, writes the file with
  the engine's user as its owner, puts it back when the engine damages it, and judges whether the engine connected (`crates/agentd/src/direct/AGENTS.md`, section
  "The tools module"). A profile with no table, or `via = "none"`, leaves a run exactly as it was.
- Neutrality: a profile and its template are where an engine word lives. No tool name, description or input type names an engine
  (`crates/harness-protocol/src/tools.rs`), and no agentd source outside `crates/agentd/src/engine/` does
  (`crates/agentd/tests/engine_neutral.rs`). A second engine gets a table and a template here with no change to the tool table.
- `templates/` holds the registration templates. The loaders read `*.toml` files of this directory only, and the image copies the whole directory
  (`images/worker-python/Dockerfile`), so a template reaches `/opt/harness/engine/profiles/templates/`.
- The commit that adds a fake engine profile touches only `engine/profiles/**` and
  `fixtures/engines/**`.

## Work Guidance

## Verification

```bash
scripts/cargo-slot.sh cargo nextest run -p harness-core --test engine_profile --test real_profiles --test bundled_engines --test profile_mcp
scripts/cargo-slot.sh cargo nextest run -p harness-protocol --test mcp_template --test engine_profile
```

## Child DOX Index

None.
