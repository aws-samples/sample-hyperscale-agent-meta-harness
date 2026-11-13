# crates/dispatcher

## Purpose

The dispatcher library provides pure helpers and AgentCore calls. The `run-session` Lambda is built on these. The library holds the run, agent and runtime session ids. It holds the Agent item, its credential binding and first mint plan. It holds the inline session policy that leases a model and the routes a run may switch to. It holds the endpoint a run pins. It holds the home account and Region. It holds the `InvokeAgentRuntime` and `StopRuntimeSession` calls with their conflict retry. The crate has no binary.

## Ownership

`crates/AGENTS.md` owns this crate. The crate reuses `worker-api`'s `Caller`, `MintPlan` and `decide`. The first mint plan comes from the decision `/internal/creds` makes on refresh. `crates/run-session` is the consumer.

## Local Contracts

- The modules are `ids`, `items`, `policy`, `endpoints`, `stage` and `aws`. Wire types come only from `harness-protocol`.
- `ids`: `is_id` accepts letters, digits and hyphens. `role_session_name` is `<run>.<agent>`. Its limit is 64 characters, the STS limit. `runtime_session_id` is `<run>-<agent>-g<generation>`. Zero padding goes before the generation, up to 33 characters. AgentCore's minimum is 33 characters. Every turn of one agent generation therefore reuses one session. `new_ulid` and `ulid_from` make ULIDs. `ulid_from(ms, seed)` gets its 80 random bits from the SHA-256 of `seed`. A retrying writer therefore puts the same event item again. It does not put a second item.
- `items`: `agent_item` builds the Agent item of a `Dispatch`. The item has attempt 1, generation 1, transport ACP and status running. Its credential binding stores only the nonce's SHA-256, tenant, mode and session policy. `mint_plan` derives the first mint through worker-api's own refresh decision. A refresh therefore reproduces the session name, source identity and tags exactly. It also reproduces the session policy and duration exactly. `sha256_hex` gives the store's hash format. `rfc3339` gives the store's timestamp format.
- `items::agent_item` also sets `CredsBinding.completion_key_sha256` to the hash of `CredsBinding::completion_key(nonce)` for the run's first nonce. worker-api requires that key on `/internal/events` and `/internal/turn-complete`; the key is never stored. `crates/worker-api/tests/handler.rs` (`b593_*`).
- `items::agent_item` also sets `CredsBinding.output_pins_required` to `Some(true)`: the run's agentd sends the version records of its three end-of-run objects, worker-api keeps them from a call that proved the key, and run-session reads those versions and no other (`crates/run-session/AGENTS.md`, `plan-delivery`). A later slice's rotation keeps the member (`..binding.clone()`); an item written before it has none and is read as it was.
- `items::agent_item` also sets `AgentItem.features`, this run's worker image capabilities, to `HarnessCapabilities::this_build().cleaned().features`. Today, the list contains only `transcript.v1`. Runs built by an older worker image have no `features` member. The member is left out when empty. `tests/items.rs` has one new test.
- `endpoints`: AgentCore's qualifier is an endpoint name
  (`is_endpoint_name`: `[a-zA-Z][a-zA-Z0-9_]{0,47}`); a bare version such as `50` is refused
  ("No endpoint or agent found with qualifier '50'"). `version_endpoint` names `v<N>`, the endpoint
  deploy.sh keeps on version N; `choose_pin` pins a run's first dispatch to `v<N>` of the version
  `live` serves when it is READY on N, else to `live` with the reason (`v<N>-absent`,
  `v<N>-<status>`, `v<N>-serves-<M>`); `session_qualifier` is a session's recorded endpoint, else
  `live`.
- `aws`: `invoke_at` and `stop_session_at` call an endpoint `qualifier`. Both refuse a qualifier that is not an endpoint name. They return `AwsError::Qualifier` before any request. Both retry `RetryableConflictException` up to `CONFLICT_ATTEMPTS` 7 times. This is a 409 while AgentCore provisions or tears down a session. Waits double from `CONFLICT_BASE_MS` 1 s. `stop_session_at` returns `false` when AgentCore no longer has the session. The error in that case is ResourceNotFoundException. `delete_capacity_provider_session` calls `DeleteCapacityProviderSession(capacityProviderId, sessionId)`, the call that ends an Instances session, its machine and its billing (`StopRuntimeSession` stops only the agent in it): `true` when the service took the delete, `false` on ResourceNotFoundException, an error naming the operation otherwise. It refuses, before any request, a provider id that is not `ids::is_capacity_provider_id` (`<name>-<10 characters>`) and a session id with a character outside `[a-zA-Z0-9._:-]`. The SDK crate must be 1.77.0 or later (the call is not in 1.73.0); `Cargo.toml` pins `aws-sdk-bedrockagentcore =1.77.0` with the smithy crates it needs.
- `stage`: `ACCOUNT` is the account from the build-selected `accounts.json` entry. `REGION` is that entry's home Region. `INFERENCE_REGIONS` lists Regions a cross-Region inference profile may route to. Those Regions are `us-west-2`, `us-east-1` and `us-east-2`.
- The inline session policy (`policy`) has `RoleGrants` (the role's non-Bedrock actions on `*`: S3, the data key, `execute-api:Invoke`; no STS or
  CodeArtifact action, since the engine uses these credentials and the package token is minted outside the microVM) and
  `LeasedModel` (`bedrock:InvokeModel*` on the geo profile ARN in the home Region and the foundation model
  in us-west-2, us-east-1 and us-east-2, plus the Region-less ARN for `global.`) and, for
  `aws-mantle` models only, `LeasedMantle`. A system inference-profile ARN in the home account
  and Region or a foundation-model ARN in an inference Region leases exactly what its id leases; every
  other ARN, wildcards, and a `/` anywhere but after the `aws-mantle` prefix are refused (so the prefix alone
  is refused); the policy stays within 2,048 characters. `session_policy_in(model, region)` leases the geo profile ARN in that
  Region; the foundation-model ARNs cover every inference Region either way. `session_policy_for_routes(&[(id, region)])` is
  the same policy over several routes: one `inference-profile` ARN per geo route plus, per distinct base model, a foundation-model ARN with the wildcard
  Region segment `us-*` (`arn:aws:bedrock:us-*::foundation-model/<base>`, `policy::WILDCARD_REGION`) and the Region-less ARN for a `global.` route. It adds no model, action, account or resource
  type, because the effective permissions are the intersection with the role's own policy, whose Bedrock invoke ceiling names the three inference Regions and the
  Region-less foundation-model ARN (`infra/test/security.test.ts` pins it literal; the safety of the form depends on that). `us-*` matches no Region-less ARN,
  so that ARN is written out only for the bases a `global.` route serves; the policy check accepts exactly this wildcard and refuses a bare `*`. The nine routes of the five
  models take 1,675 of the 2,048 characters that STS allows (the five session tags pack the whole to 36 percent, measured against STS; all-literal Regions
  would take about 2,370); one route gives exactly the text of `session_policy_in`; an ARN, a wildcard or an odd
  character in an id is refused. `tests/policy_routes.rs`.
- Credentials and nonces are never printed or logged. The Agent item stores only the nonce SHA-256.
- Tests never call AWS. `tests/aws_calls.rs` drives the real SDK client over recorded HTTP exchanges using `StaticReplayClient`. It reads what each call put on the wire.

## Work Guidance

## Verification

```bash
cargo test -p dispatcher
```

## Child DOX Index

None.
