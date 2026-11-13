# engine/catalog

## Purpose

The static model list of a direct run (invariant 6): `models.json` names the owner's supported models, the one Bedrock
route each runs on, its context window, its output limit and its smoke record. A model is selectable only with a passing smoke
record. `smoke-us.zai.glm-5.3.json` and `smoke-global.anthropic.claude-fable-5-2.json` are the committed smoke records behind the GLM 5.3 and Claude Fable 5.2 rows.
`reasoning-levels.json` is the committed live table behind the `reasoning` object of every row.

## Ownership

Owned by `engine/AGENTS.md`. `crates/ham-core/src/models.rs` compiles `models.json` in (`include_str!`) and is the one parser:
`crates/control-api/src/direct_models.rs` re-exports it for control-api, and `ham` and the TUI read the same list.

## Local Contracts

- `models.json` lists exactly six Cline models (the rows with no `engine`), in this order, each with one Cline route: Claude Opus 5.5 (`us.anthropic.claude-opus-5-5`),
  Claude Sonnet 5.5 (`global.anthropic.claude-sonnet-5-5`), GPT-6.1 Sol (`us.openai.gpt-6.1-sol`), GPT-6 Astra
  (`us.openai.gpt-6-astra`), GLM 5.3 (`us.zai.glm-5.3`) and Claude Fable 5.2 (`global.anthropic.claude-fable-5-2`). All six run on Bedrock Converse. The GLM 5.3 profile exists only as `us.`
  and `global.`, and the Mantle list has no GLM 5.3. Ids outside the six (for example `zai.glm-5` and `us.zai.glm-5`) are refused.
- Each row has `id`, `name`, `vendor`, `alias`, `baseModelId`, `protocol`, `region`, `contextTokens`, `maxOutputTokens`,
  `smokeRecordId` and `smokedAt`. `contextTokens` is required because the Cline fork compacts context only when the window is known
  (patch 0010).
- Claude Fable 5.2 carries `requiresProviderSharing: true`, an optional boolean that is false when absent. A model that needs it runs only in an account whose Bedrock data retention mode is `provider_data_share` and only for a run that agrees to share its prompts and answers with the model's provider: `ham` refuses it while `run.provider_sharing` is off, control-api refuses a request without `providerSharing: true` (400 `provider-sharing-required`), and a run on any other model never gets it as a failover route. The row has its live record (the S6 smoke of both routes and the reasoning table, run in one test account and Region while its data retention mode was `provider_data_share` for the length of the calls and `inherit` again afterwards), so it is selectable; a row without a record stays pending and is refused (422 `model-not-smoked`, invariant 6). Its id is written once, as the row's `id` in `models.json`; the second profile is its one `alternateRoutes` entry. `crates/ham-core/tests/b803_catalog_sharing.rs` pins the row.
- `alias` (`opus`, `sonnet`, `sol`, `astra`, `glm`, `fable`) is the short name that `ham --model`, `HAMH_MODEL`, `ham config set default_model` and
  the TUI accept. `crates/ham-core/tests/models.rs` pins the aliases and the default.
- A row may carry `note`, one line that `ham models` prints under the table and `GET /v1/direct/models` returns. Only the GLM 5.3 row has
  one: it reasons at high effort by default, a long reasoning turn can take many minutes at high and can end without an answer at xhigh, and it
  stays selectable but is not recommended for a first run. `crates/ham-core/tests/models.rs` pins which rows have a note and what it says.
- A row may carry `engine`: the name of the engine profile it was smoked with, `cline` when absent. The same `id` may be listed once for each engine, each row with its own route, window, reasoning record and smoke record; aliases and the `default` row are per engine (`ham_core::models::Catalog`; `Models::parse` reads the Cline rows only, so a caller that predates the dimension keeps its list). A start request is accepted for an (engine, model) pair only with a row of that engine that has a passing smoke record. Rows of another engine are added by copying the Cline row of the model, setting `engine`, removing `default` and `alternateRoutes` unless that engine has smoked them, and replacing the smoke record ids with that engine's own; no existing row changes. Rows of another engine are added in the release that deploys that engine's worker image and Lambdas: until a row with a passing smoke record exists, no request can name the engine, so the catalog is the gate that keeps a stage with older readers from accepting a run it cannot start. `crates/ham-core/tests/b787_catalog.rs` pins the rules.
- control-api refuses every id that is not listed (422 `unknown-model`) and a listed id without a smoke record (422 `model-not-smoked`).
  `ham` and the TUI show a row without a smoke record as pending and cannot pick it.
- `smoke-us.zai.glm-5.3.json` is a copy of the audit record: Converse, `mantle: false`, `us-east-1` and `us-west-2` pass,
  window 1,048,576 tokens, 128,000 output tokens. `crates/control-api/tests/direct_models.rs` compares the GLM 5.3 row with the file.
- A row of Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra or Claude Fable 5.2 also carries `alternateRoutes`: the model's other
  inference profile (`global.` for a `us.` model, `us.` for Sonnet's `global.`), each with its own `id`, `region`, `smokeRecordId` and `smokedAt`. A run never
  names it; the control plane lists it after the chosen route in the start request, the session credentials cover it, and agentd switches to it when the model's
  own route fails. Its window and output limit are the model's. GLM 5.3 has none (no smoked second profile). `crates/control-api/tests/direct_models.rs`
  compares each alternate with this file.
- Every row with a smoke record carries `reasoning`: `default`, the launch level the model reasons at when the user keeps `reasoning.effort = default`
  (`low`, `medium`, `high` or `xhigh`); `accepted`, the launch levels whose coding turn through the pinned engine passed; `smokeRecordId` and `smokedAt`, the
  live table it stands on. `default` is one of `accepted`; `Models::parse` refuses a row that breaks that, an unknown or repeated level or a blank record.
  run-session copies `default` into `DirectModelSpec.defaultReasoningEffort` of the model and of its routes; control-api refuses an explicit level a row does
  not accept (422 `reasoning-level-not-accepted`). Today: GPT-6.1 Sol and GPT-6 Astra `xhigh`; Claude Opus 5.5, Claude Sonnet 5.5, Claude Fable 5.2 and GLM 5.3 `high`, because
  xhigh can run into the engine's idle limit on the two older Claude models and can end without an answer on GLM 5.3, and none of the xhigh long-reasoning turns of Claude Fable 5.2 finished (see below). A default moves with a one-line
  edit and a deploy of the Lambdas, not of the worker image.
- `reasoning-levels.json` is the table: `cells` of kind `route` (one Converse call on each route at low, medium, high, xhigh, max, minimal and none, with the
  request fields the pinned engine sends) and of kind `launch` (one coding turn through the engine launched with `--thinking <level>`, with the word it sent),
  `hardRuns` (one long no-tool prompt at xhigh and high, with the longest silent stretch against `idleLimitS`) and `earlierRuns` (an earlier sample). It names no
  account, profile or host. `crates/ham-core/tests/model_reasoning.rs` holds the rows of `models.json` to it.
- Engine facts behind the defaults. agentd launches the engine with `--thinking <level>` per run; the levels per model are in `engine/catalog/models.json` (`reasoning`) and the live table behind them in `engine/catalog/reasoning-levels.json`. The engine ends a turn after about 360 s without a stream event ("The operation timed out."); a Claude route sends no event while it thinks at the wire word `max` (what `--thinking xhigh` sends), which is why the two Claude models default to `high`. A direct streaming probe of the two Claude models showed that a `thinking` block with `display: "summarized"` streams reasoning text continuously (gaps of a few seconds), while the block the engine sends today can stay silent for the whole idle limit at native xhigh. A fork change that sends `display: "summarized"` for Claude is therefore the lever for a Claude xhigh default; it moves the fork series and the worker image (not built).
- `smoke-opencode-<model id>.json` is the smoke record of one model on the OpenCode engine (a model is offered on an engine only with a passing record for that engine). A
  record is `pass` when two prompts in one session both end `end_turn`, complete a tool call and end with a message, and the file they edit holds the two asked lines. Present
  for Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra and Claude Fable 5.2 on the pinned release. GLM 5.3 has none: Bedrock answered every call to it with a
  throttling error (`Too many tokens`) in all test accounts and routes at the time of the run, so it is not offered on OpenCode until a record passes.
- `models.json` lists a row with `"engine": "opencode"` for each of those five records, after the Cline rows: the route facts of the Cline row of the same model, the
  `smokeRecordId`, `smokedAt` and Region of the record file, no `alternateRoutes` and no `reasoning` (OpenCode has smoked no second route and has no reasoning record). The Claude Sonnet 5.5 row is `default` for OpenCode, as it is for the default engine: the default is per engine and the live drivers read the default of the engine they run.
  GLM 5.3 has no OpenCode row. The start check reads these rows: a pair without one is 422 `model-not-smoked` with the pair named.
  `crates/ham-core/tests/b787_opencode_rows.rs` pins the rows to the files.
- The Claude Fable 5.2 row of OpenCode carries `requiresProviderSharing: true` like its Cline row: the agreement is about the model and its provider, so the
  start check (400 `provider-sharing-required`) and `ham` read the same mark on every engine. `Catalog::parse` refuses a list whose rows of one model disagree on it, and
  `crates/ham-core/tests/b805_opencode_catalog_row.rs` and `b805_opencode_sharing.rs` pin the row and the rule. OpenCode takes no reasoning level (its rows carry no
  `reasoning` object and agentd places no `--thinking` argument), so the row has none of the Cline row's reasoning table or alternate route.
- Claude Fable 5.2 (measured with the same pinned engine, 5 launch samples per level). All four launch levels ended `end_turn` with the file written in every sample (low 6.5 to 9.8 s, medium 9.7 to 10.3 s, high 7.8 to 12.5 s, xhigh 25.7 to 58.5 s; `--thinking xhigh` reaches the wire as `max`, as on the other Claude models), and its two Converse routes take `low` to `max` and refuse `minimal` and `none` with the same message as Claude Opus 5.5. Its reasoning is not streamed: a direct ConverseStream of the long counting prompt at `high` and at `max` sent one reasoning block in two events and then nothing for 306 s and 273 s, until the 32,000-token cap with no answer. Through the engine the 8 by 8 form of that prompt ended in the provider's content filter ("Model returned no content because the response was blocked by a content filter") in all six runs, after 17 to 67 s. The 6 by 6 form finished at `high` in 73 s with a longest silent stretch of 60.5 s, and ended in the filter at `xhigh` after 25 s. So no xhigh long-reasoning turn of the row finished, and `default` is `high`, the level of the other Claude rows; `hardRuns` in `reasoning-levels.json` holds the eight runs (rows of model `fable`, with their `prompt`).
- To add a route, copy a passing row and its smoke id; do not invent either.
- This directory is outside the worker image key (`scripts/lib/digests.py` `IMAGE_EXCLUDED`): the image never reads it.

## Work Guidance

- Edit `models.json` by hand only to add a smoked route or to refresh a smoke record. Change the list and the pins in
  `crates/ham-core/tests/models.rs` and `crates/control-api/tests/direct_models.rs` in one change.

## Verification

```bash
scripts/cargo-slot.sh cargo nextest run -p ham-core --test models --test model_reasoning
scripts/cargo-slot.sh cargo nextest run -p control-api --test direct_models --test direct
```

## Child DOX Index

None.
