# engine/catalog

## Purpose

The static model list of a direct run (invariant 6): `models.json` names the five supported models, the one Bedrock
route each runs on, its context window, its output limit and its smoke record. A model is selectable only with a passing smoke
record. `smoke-us.zai.glm-5.3.json` is the committed smoke record behind the GLM 5.3 row. `reasoning-levels.json` is the committed live
table behind the `reasoning` object of every row.

## Ownership

Owned by `engine/AGENTS.md`. `crates/ham-core/src/models.rs` compiles `models.json` in (`include_str!`) and is the one parser:
`crates/control-api/src/direct_models.rs` re-exports it for control-api, and `ham` and the TUI read the same list.

## Local Contracts

- `models.json` lists exactly five models, in this order, each with one Cline route: Claude Opus 5.5 (`us.anthropic.claude-opus-5-5`),
  Claude Sonnet 5.5 (`global.anthropic.claude-sonnet-5-5`), GPT-6.1 Sol (`us.openai.gpt-6.1-sol`), GPT-6 Astra
  (`us.openai.gpt-6-astra`) and GLM 5.3 (`us.zai.glm-5.3`). All five run on Bedrock Converse. The GLM 5.3 profile exists only as `us.`
  and `global.`, and the Mantle list has no GLM 5.3. Ids outside the five (for example `zai.glm-5` and `us.zai.glm-5`) are refused.
- Each row has `id`, `name`, `vendor`, `alias`, `baseModelId`, `protocol`, `region`, `contextTokens`, `maxOutputTokens`,
  `smokeRecordId` and `smokedAt`. `contextTokens` is required because the Cline fork compacts context only when the window is known
  (patch 0010).
- `alias` (`opus`, `sonnet`, `sol`, `astra`, `glm`) is the short name that `ham --model`, `HAMH_MODEL`, `ham config set default_model` and
  the TUI accept. `crates/ham-core/tests/models.rs` pins the aliases and the default.
- A row may carry `note`, one line that `ham models` prints under the table and `GET /v1/direct/models` returns. Only the GLM 5.3 row has
  one: it reasons at high effort by default, a long reasoning turn can take many minutes at high and can end without an answer at xhigh, and it
  stays selectable but is not recommended for a first run. `crates/ham-core/tests/models.rs` pins which rows have a note and what it says.
- control-api refuses every id that is not listed (422 `unknown-model`) and a listed id without a smoke record (422 `model-not-smoked`).
  `ham` and the TUI show a row without a smoke record as pending and cannot pick it.
- `smoke-us.zai.glm-5.3.json` is a copy of the audit record: Converse, `mantle: false`, `us-east-1` and `us-west-2` pass,
  window 1,048,576 tokens, 128,000 output tokens. `crates/control-api/tests/direct_models.rs` compares the GLM 5.3 row with the file.
- A row of Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol or GPT-6 Astra also carries `alternateRoutes`: the model's other
  inference profile (`global.` for a `us.` model, `us.` for Sonnet's `global.`), each with its own `id`, `region`, `smokeRecordId` and `smokedAt`. A run never
  names it; the control plane lists it after the chosen route in the start request, the session credentials cover it, and agentd switches to it when the model's
  own route fails. Its window and output limit are the model's. GLM 5.3 has none (no smoked second profile). `crates/control-api/tests/direct_models.rs`
  compares each alternate with this file.
- Every row carries `reasoning`: `default`, the launch level the model reasons at when the user keeps `reasoning.effort = default`
  (`low`, `medium`, `high` or `xhigh`); `accepted`, the launch levels whose coding turn through the pinned engine passed; `smokeRecordId` and `smokedAt`, the
  live table it stands on. `default` is one of `accepted`; `Models::parse` refuses a row that breaks that, an unknown or repeated level or a blank record.
  run-session copies `default` into `DirectModelSpec.defaultReasoningEffort` of the model and of its routes; control-api refuses an explicit level a row does
  not accept (422 `reasoning-level-not-accepted`). Today: GPT-6.1 Sol and GPT-6 Astra `xhigh`; Claude Opus 5.5, Claude Sonnet 5.5 and GLM 5.3 `high`, because
  xhigh can run into the engine's idle limit on the two Claude models and can end without an answer on GLM 5.3. A default moves with a one-line
  edit and a deploy of the Lambdas, not of the worker image.
- `reasoning-levels.json` is the table: `cells` of kind `route` (one Converse call on each route at low, medium, high, xhigh, max, minimal and none, with the
  request fields the pinned engine sends) and of kind `launch` (one coding turn through the engine launched with `--thinking <level>`, with the word it sent),
  `hardRuns` (one long no-tool prompt at xhigh and high, with the longest silent stretch against `idleLimitS`) and `earlierRuns` (an earlier sample). It names no
  account, profile or host. `crates/ham-core/tests/model_reasoning.rs` holds the rows of `models.json` to it.
- Engine facts behind the defaults. agentd launches the engine with `--thinking <level>` per run; the levels per model are in `engine/catalog/models.json` (`reasoning`) and the live table behind them in `engine/catalog/reasoning-levels.json`. The engine ends a turn after about 360 s without a stream event ("The operation timed out."); a Claude route sends no event while it thinks at the wire word `max` (what `--thinking xhigh` sends), which is why the two Claude models default to `high`. A direct streaming probe of the two Claude models showed that a `thinking` block with `display: "summarized"` streams reasoning text continuously (gaps of a few seconds), while the block the engine sends today can stay silent for the whole idle limit at native xhigh. A fork change that sends `display: "summarized"` for Claude is therefore the lever for a Claude xhigh default; it moves the fork series and the worker image (not built).
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
