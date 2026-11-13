//! Exported contract artifacts: insta snapshots, committed `schemas/*.json` equality,
//! and OpenAPI reference integrity.

use harness_protocol::PROTOCOL_VERSION;
use harness_protocol::api::{INTERNAL_ROUTES, V1_ROUTES};
use harness_protocol::export::{SCHEMA_FILES, all_schemas, render};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas")
}

#[test]
fn the_nine_contract_files_are_exported() {
    let names: Vec<&str> = all_schemas().iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        [
            "event-envelope.schema.json",
            "openapi-v1.json",
            "openapi-internal.json",
            "items.schema.json",
            "evidence.schema.json",
            "engine-profile.schema.json",
            "runtime.schema.json",
            "orchestration.schema.json",
            "tools.schema.json",
        ]
    );
    assert_eq!(SCHEMA_FILES, names.as_slice());
}

#[test]
fn schemas_match_their_snapshots() {
    for (name, value) in all_schemas() {
        insta::assert_json_snapshot!(name.trim_end_matches(".json"), value);
    }
}

#[test]
fn committed_schema_files_equal_fresh_output() {
    for (name, value) in all_schemas() {
        let path = schemas_dir().join(name);
        let committed =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(
            committed,
            render(&value),
            "{name} is stale: run `cargo run -p harness-protocol --bin export-schemas`"
        );
    }
}

#[test]
fn json_schemas_declare_draft_2020_12() {
    for (name, value) in all_schemas() {
        if name.starts_with("openapi") {
            continue;
        }
        assert_eq!(
            value["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{name}"
        );
    }
}

fn refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == "$ref"
                    && let Value::String(s) = x
                {
                    out.push(s.clone());
                }
                refs(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
        _ => {}
    }
}

#[test]
fn openapi_documents_cover_every_route_and_resolve_every_ref() {
    let all = all_schemas();
    for (name, routes) in [
        ("openapi-v1.json", V1_ROUTES),
        ("openapi-internal.json", INTERNAL_ROUTES),
    ] {
        let doc = &all.iter().find(|(n, _)| *n == name).unwrap().1;
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(
            doc["jsonSchemaDialect"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        for r in routes {
            let op = &doc["paths"][r.path][r.method.to_ascii_lowercase()];
            assert_eq!(
                op["operationId"], r.operation_id,
                "{name} {} {}",
                r.method, r.path
            );
        }
        let mut found = Vec::new();
        refs(doc, &mut found);
        assert!(!found.is_empty());
        for r in found {
            let key = r
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("{name}: foreign ref {r}"));
            assert!(
                doc["components"]["schemas"].get(key).is_some(),
                "{name}: dangling {r}"
            );
        }
    }
    let v1 = &all[1].1;
    assert!(
        v1["paths"]["/v1/direct/runs"]["post"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Idempotency-Key"
                && p["in"] == "header"
                && p["required"] == true)
    );
    assert_eq!(
        v1["components"]["securitySchemes"]["sigv4"]["type"],
        "apiKey"
    );
}

fn is_key_sorted(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            let keys: Vec<&String> = m.keys().collect();
            keys.windows(2).all(|w| w[0] <= w[1]) && m.values().all(is_key_sorted)
        }
        Value::Array(a) => a.iter().all(is_key_sorted),
        _ => true,
    }
}

// serde_json preserve_order (enabled workspace-wide by a dev-dependency elsewhere) must not change
// the contract files: objects are emitted with sorted keys whatever the build graph.
#[test]
fn every_exported_schema_is_key_sorted() {
    for (name, value) in all_schemas() {
        assert!(is_key_sorted(&value), "{name} is not key-sorted");
    }
}

#[test]
fn canonical_sorts_keys_whatever_the_insertion_order() {
    let mut inner = serde_json::Map::new();
    inner.insert("y".into(), Value::from(1));
    inner.insert("b".into(), Value::from(2));
    let mut outer = serde_json::Map::new();
    outer.insert("zeta".into(), Value::from(1));
    outer.insert("alpha".into(), Value::Array(vec![Value::Object(inner)]));
    let c = harness_protocol::export::canonical(&Value::Object(outer));
    assert!(is_key_sorted(&c));
    let out = render(&c);
    assert!(out.find("alpha") < out.find("zeta"));
    assert!(out.find("\"b\"") < out.find("\"y\""));
}

/// Each released protocol version and the SHA-256 of its contract files, in `SCHEMA_FILES` order.
/// Pin a new version by appending
/// it; never edit a released line.
const FROZEN: &[(&str, &str)] = &[
    (
        "1.0.0",
        "39715d749f217df709b292b2426ba31e1f0dfd838c7f450c0da3d3a0d5b72e40",
    ),
    // Lane B: GatePlan.lane, TaskContext.lane, the LaneBPool item and the staged
    // LaneBHiddenSet.
    (
        "1.1.0",
        "ce75bb54493b0a349fae600462a27cf292645b03edcc65cbd323639bda504e0e",
    ),
    // Multi-task runs: PlanTask.requirements, the Task item's context, attempt and merged head, the
    // DAG item, TaskContext attempt, baseBundleUri, hiddenScope and evidence, GatePlan.hiddenScope,
    // and the verifier's merge op.
    (
        "1.2.0",
        "8e4d1af044c9d3231cb7f21da4e787d1299a270ae02c414debdd9401ae4d1dd2",
    ),
    // The repo-read gateway tools (RepoReadContract in the orchestration contract) and the run
    // cost by model and engine on RunDetail (R4 gateway stack, W10).
    (
        "1.3.0",
        "4b40cb08af423fc0172e82e2dfbf0b808614e0e8166bf6f39ae4321b55d5c1ab",
    ),
    // Session lifecycle: generation, session, runtime version, base head and resume path on the
    // Turn item; generation, start, runtime and policy versions on the Agent item; the Run item's
    // pinned runtime version; TurnPlan.baseSha; the goal step's execution and restart.
    (
        "1.4.0",
        "04c20497065ecb5ad6c7313cb356fabee967b1e88185ba5987c20f0743cdaea4",
    ),
    // The Lane B merge mode: GateMode::Merge, GatePlan.merges and the staged
    // LaneBHiddenSet.scope.
    (
        "1.5.0",
        "23850a5e988a586b79b58d7f52245b622d1162c3a45eac55df652ad2487a4f3e",
    ),
    // runtimeEndpoint on the Run, Agent and Turn items: the qualifier a run's sessions use is an
    // endpoint name.
    (
        "1.6.0",
        "a0a8ff28e030e073ea37678ac87f9567bc89735b5e5d73074c0130219d4ddf84",
    ),
    // W8 delivery variants: the partial and log artifact kinds and their keys, the allowlist
    // listing item, GET /v1/repos and POST /v1/admin/repos. m7o pinned an
    // all-zero placeholder; this is the hash of m7o's schemas (07def9e), never deployed as 1.7.0.
    (
        "1.7.0",
        "568a08ac9b20f066f33b6442b670f86b456ac732ccac010239ee6205a481e8e1",
    ),
    // Catalog rows: the fork's `mantle-chat` protocol, per-role maxTokens and the TDD
    // qualification.
    (
        "1.8.0",
        "7347697fa19209dd7e93ddd241886b5aca3364da02af7f8a0ecedd62efa3ffb6",
    ),
    // Roles, rules and modes: RolePolicy.writeRegex (the Roo edit
    // fileRegex), TaskContext.roles (the plan's phase roles), SessionStartedPayload.role, and the
    // turn's rulesDigest and rulesManifest on TurnCompleteRequest and the Turn item (with its role).
    (
        "1.9.0",
        "4b60d0205373c2ec66ab578343c10b6777711f38bd3d4a9b9e9983d67643471e",
    ),
    // Image families: ImageFamily, TurnPlan.family, and the Run item's runtimeVersions and
    // runtimeEndpoints (each other family's pin: version and qualifier endpoint).
    (
        "1.10.0",
        "64f5023c990acb81d9144eb776e324e14e7ba483c298b8547fe8bb69b89c2889",
    ),
    // The goal loop's gate feedback: GateReportRef.feedback, one bounded GateFeedback record per
    // failing or erroring visible gate.
    (
        "1.11.0",
        "cb3b73cc5db7a789343a2ae36bf935bee1b1686a798ad36ef0d20862c80869e2",
    ),
    // Engines as profile names: Engine is a string of the profile-name pattern, no longer the
    // cline/opencode enum, so an engine added as engine/profiles/<name>.toml needs no protocol
    // change; the wire form of every record is unchanged.
    (
        "1.12.0",
        "975d80e8541a10487ec73c71a9102b93c2a47916380663ab9b02d8b0c73e4c16",
    ),
    // TurnCompleteRequest.reverted, the
    // paths agentd restored outside the role's write globs, which fail the turn,
    // and RunItem.budgetWrapUp, a budget-limited run's one wrap-up turn and its stop.
    (
        "1.13.0",
        "e1d9935d7553b96299b6d00aa9b3122cc472efdb51298bb4ddaee56575b7efff",
    ),
    // read-only
    // tasks run the real path: GateScope on GatePlan and
    // VerifyRequest, workProduct on EvaluatePlan and EvaluationRequest, and the turn summary on
    // TurnCompleteRequest and InvocationResponse.
    (
        "1.14.0",
        "2d2a7492c5aa11c08bce559a5200690d9ae62c28168b4a596d502fa97ce1db26",
    ),
    // TurnCompleteRequest
    // .revertedTruncated (the restored-path list capped by cap_reverted) and
    // BudgetWrapUp.claimedAt, which bounds the other agents' wait for a wrap-up turn.
    (
        "1.15.0",
        "b2364fc7e2af7f64d4d02a3cf0953bee3669a28ceb5c6484f91c6ad2fb5dd3a6",
    ),
    (
        "1.16.0",
        "52841d6f8163e67bfafc565434bf5be64f985874da1a8298a878d10e16fae621",
    ),
    // InvocationRequest.packagesToken, the CodeArtifact token the dispatcher mints outside the VM
    // with its own role, so the worker session (the engine's credentials) holds no CodeArtifact
    // grant.
    (
        "1.17.0",
        "790162e690291429a69724f56a3903c20ae7b271cc3b4f54b99a0390924d00f9",
    ),
    // The
    // run.aborted event and its RunAbortedPayload (ExecutionEnd, bounded
    // error and cause), and RunItem.aborted and RunSummary.aborted, why a run whose root execution
    // ended early was cleared.
    (
        "1.18.0",
        "f29fb7c02a68ee15ee5c87d3795300d4f36b5dbcc4a75f2841d45d870494bc32",
    ),
    // The
    // RepoTarget description names no account or Region ("cc:<name> (CodeCommit in the deployment's
    // account and home Region)"), so the schemas are the same under every accounts.json selection;
    // no wire shape changes.
    (
        "1.19.0",
        "c4d023328bd880fdae1c642e524a36ebed285deb223396e2a56fb0d94b44c5c4",
    ),
    // TurnCompleteRequest.refused and .refusedTruncated (Refusal: tool, action, reason, count;
    // cap_refused) and InvocationResponse.refused, so the goal step names a turn's refused
    // requests in the agent's next prompt.
    (
        "1.20.0",
        "9f2da30fbbbaafb15dd7ca57b52ef480e5cbd1a8459d7a91ab2c55462075bf1e",
    ),
    // RunKind, RunItem.kind
    // and RunSummary.kind, `probe` on the runs hamh-dispatch writes, which their probes end
    // (`hamh-dispatch end`) and `ham runs` leaves out by default.
    (
        "1.21.0",
        "fa525065a0c16bf368b5a92e433920ac768fba1721146e169c986506046be19b",
    ),
    // The provider-fault ladder and the context
    // settings in one additive bump. ladder.rs (RouteLadder, RoleLadder, ModelRung,
    // RouteRef, FaultParams, RouteHealthItem, the RouteChanged, RunWaiting and RouteRestored
    // payloads), the ROUTELADDER and HEALTH items, the route.changed, route.restored and run.waiting
    // events (21 to 24), the Turn item's fault fields, TurnCompleteRequest.faultHint and
    // maxSilentGapS (also on InvocationResponse), TurnSpec.providerSilenceS and contextTokens,
    // StartRunRequest.residency and context, session.started's contextTokens, NextAction::Wait, the
    // goal step's delayS and waitReason and EvaluatePlan.writerFamilies. EnginePolicy, RouteAssignment,
    // CatalogRow, TaskContext and AdmissionInput do not move (policy.schema.json is the 1.21.0 file).
    (
        "1.22.0",
        "2597c94307cdbafc2bf8b1195bf4e0627a964c7b91284c87389e1f7070ccb686",
    ),
    // The
    // server side of the ladder and the context settings. FaultParams carries the
    // 22 sidecar numbers 1.22.0 had no field for (BackoffParams.stall, BreakerParams.probeRetryFactor,
    // WaitParams.jitter, engineRetriesExhaustedAfterS, providerSilenceLongContextS,
    // longContextFromTokens, HealthParams, UnpublishedPoolParams, ladderRequireTdd), RunItem.pins
    // (RunPins: ladder version and digest, residency, context), RouteLadderPointer
    // (`ROUTELADDER`/`ACTIVE`), GET /v1/routes (RoutesResponse) and POST /v1/admin/route-health
    // (RouteHealthAdminRequest and Response). All optional or new, so a 1.22.0 item, ladder or
    // request keeps its bytes.
    (
        "1.23.0",
        "8ff0a11165f68da811f2dfa081536a5fe5601ed0bc6693486b512e949e5c2550",
    ),
    // TurnItem.maxOutputTokens, the output-token cap dispatch resolved for the turn.
    // Only items.schema.json moves; gate feedback per stack (D6) changes behavior and no type.
    (
        "1.24.0",
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc",
    ),
    // GoalStepEvent::EvaluationUnavailable {error}, the event worker-goal-loop's EvaluationUnavailable state
    // sends, and TurnItem.contextTokens, the window dispatch resolved for the turn. The review fixes re-pin this
    // unreleased line: EventType::ContextChanged ("context.changed") with ContextChangedPayload, the dispatcher's
    // announcement that a continuing session runs another model or window, and TurnItem.transport and
    // TurnItem.enginePolicyVersion, recorded with the engine so a retried dispatch reads the three from one item,
    // and EvaluatorVerdictPayload.designerFamilyJudged (the verdict of the family that designed the hidden tests).
    // re-pins the line again: EventType::RungSkipped ("rung.skipped") with RungSkippedPayload, EscalationRung and
    // RungSkipReason, the goal step's note that the other-family rung was skipped by the family rule. All optional
    // or new: a 1.24.0 item keeps its bytes. re-pins it a third time for TurnItem.startedAtMs, the millisecond
    // dispatch's first attempt created the item (the context.changed id's time, so it sorts after the events
    // written before it). The fourth re-pin is a description: RungSkippedPayload's doc says the handler dates the
    // event's id with the millisecond it publishes at. re-pins it a fifth time (hidden tests are admitted before
    // they are enforced): Evidence::Review, FailureClass with HiddenOutcome.class, HiddenFailure and
    // GateReportRef.hiddenFailures, GateResultPayload.failureClass, GatePlan.recheck, VerifyOp::Witness with
    // VerifyRequest.hiddenRev and VerifyResponse.witness (WitnessOutcome), EventType::HiddenAdmission
    // ("hidden.admission") with HiddenAdmissionPayload, NextAction::Triage(TriageRequest),
    // GoalStepEvent::TriageDone(TriageResult), WaitReason::HiddenAdmission, AdmissionItem and
    // AdmissionRevisionItem (StoreItem admission and admissionRevision), RulingRequest and Ruling, and
    // StartRunRequest.hiddenInject. All optional or new: a 1.24.0 document keeps its bytes. re-pins it a sixth
    // time: RulingRequest gains the optional writerFamilies, designerFamily, acceptanceSha256 and
    // repairAcceptanceSha256, so the evaluator runtime can re-check the judge's family and bind the ruling to the
    // exact hidden sets.
    (
        "1.25.0",
        "da823fe03f32397e093027c45655a91d98b3f8e958e2f550b3c850115c923231",
    ),
    // Opens 1.26.0 after 1.25.0 shipped (image 14397bf3f12b, deploy A2): TurnSpec.compaction
    // (CompactionSetting {enabled}), what `hamh-dispatch invoke --no-compaction` puts on a K16 turn so the engine's
    // own context compaction (Cline fork patch 0012, on at 0.9 of the window) cannot summarise the prompt the
    // long-context row measures. The member is optional and left out when absent, so a turn written before keeps
    // its bytes; the 1.25.0 line above is released and does not move.
    (
        "1.26.0",
        "71c22c2d24c23138f44f44e552fb9855aac3f176fc1ec5331907afce66c7de39",
    ),
    // EvaluationRequest.roleRecord (RoleRecord {tasks, turns, omitted}, TaskRoles, RoleTurn, RoleRule), the
    // harness's record of which role ran each writer turn, which the evaluator shows as section 9,
    // and TaskContext's evidence and roles documentation. The member is
    // optional and left out when absent, so a request written before keeps its bytes; the 1.26.0 line does not move.
    // The fix-up after the branch's review re-pins this unreleased line (it was 2af87f7a...): the published RoleRecord,
    // TaskRoles and RoleTurn descriptions now say what the evaluate Lambda sends (merged tasks by task id, no role
    // sessions, a turn's head from the next turn's base); no shape changed.
    (
        "1.27.0",
        "f43ae16380c42a10f3edd27e1152899fd7242aeaf0e4f61d8f8847a0dbae2d91",
    ),
    // Every member is optional and left out when empty, so a record written before keeps its bytes; the 1.27.0 line
    // does not move.
    (
        "1.28.0",
        "db4636864fe24ad7486523cc66a710b5803380fa62af08df84809832a724b091",
    ),
    // Direct run (1.29.0): StartDirectRunRequest and
    // ContinueSettings, the route POST /v1/direct/runs (operation startDirectRun) with the same Idempotency-Key header and
    // the StartRunResponse answer. StartRunRequest and every other type keep their bytes: the multi-agent line's
    // 1.28.0 line above does not move; openapi-v1.json gains the path and two definitions and both OpenAPI documents carry
    // the new version in info.version.
    (
        "1.29.0",
        "57d4ba2990ad65d6fc920e04d07f7b3df36e35fd5ab3de6fd4d449295517675e",
    ),
    // direct-run contract commit C0: the InvokeAgentRuntime body, summary.json, the packager evidence, the outcome words
    // and the outcome, label and detail of run.completed (`direct` module, runtime and orchestration schemas).
    (
        "1.30.0",
        "c80ed6b349849e03897bb1f6cf9b7d4a253b527cfcc45247bd3465959ba0d1d7",
    ),
    // Direct run (1.31.0, the model-choice merge): `ModelList` and `ModelRow` and the route GET /v1/direct/models (operation
    // listDirectModels) in openapi-v1.json; both OpenAPI documents carry the new version in info.version. Nothing else moves.
    (
        "1.31.0",
        "cdf05ed6295e272520a10354e3bf9f6adb18eabcfe0207d9b153bbfde4bb92e9",
    ),
    // Direct run: `DirectAttempt`, `DirectAttemptKind` and `DirectAttemptResult`
    // and the optional `attempts` member of `DirectSummary` (runtime.schema.json); both OpenAPI documents carry the new version in
    // info.version. Nothing else moves.
    (
        "1.32.0",
        "d60a9d8f0d42529e8d1a82621543bc3f4a605ebd407aceeb6d9bb8792a3b77a9",
    ),
    // Direct run (1.33.0, shell target): the optional `runtimeEndpoint` member of `ShellTargetResponse` (openapi-v1.json); both
    // OpenAPI documents carry the new version in info.version. Nothing else moves.
    (
        "1.33.0",
        "f7177e6a1e2f22b334a00c87f8963d6bbc1254e5ea518da5f74a96ba0509d139",
    ),
    // Direct run: `DirectInvocationRequest.failover`, `DirectSwitch`, `DirectFailoverNote`,
    // `DirectSummary.switches`, `finalModel` and `failover`, `DirectAttempt.modelId`, `DirectEvidence.finalModelId` and `modelSwitches`
    // (runtime.schema.json). Both OpenAPI documents carry the new version in info.version.
    (
        "1.34.0",
        "2af0fc2ac9bfccac0196fbe1adbb26ff05ee4bb798def5f6e016fe1c69470782",
    ),
    // Direct run: drops the gate, evaluator, planner, route-ladder, hidden-test,
    // repo-read, modes, plan, policy and approval types and six contract files (`gates`, `verdict`, `modes`, `plan`, `policy`, `catalog`).
    (
        "1.35.0",
        "03a32e5aef49ed3ef5cd6e2a92b00e8b6da2326e93b54429b7eeebd8ff3fdb52",
    ),
    // Direct run (1.36.0, outcome row 16): the `stopped_no_changes` member of the `DirectOutcome` enum in runtime.schema.json and
    // orchestration.schema.json; both OpenAPI documents carry the new version in info.version. Nothing else moves.
    (
        "1.36.0",
        "7223f31f1b0f472e2789f515811868e0a185d8dc006d3a3417dd736915d31550",
    ),
    // Direct run: `PendingControl::Instruct` with `InstructAction` and `InstructOrigin`
    // (items.schema.json, openapi-internal.json), the route POST /v1/runs/{runId}/input (operation answerInput) with `InputAnswer`,
    // `InputAccepted` and `NeedsInputBrief` and the optional `RunSummary.needsInput` and `StartDirectRunRequest.inputWaitS`
    // (openapi-v1.json), `DirectInvocationRequest.inputWaitS`, the attempt kinds `fresh_session` and `instruction`, and the optional
    // `DirectSummary.interventions` and `freshSessions` (runtime.schema.json). Both OpenAPI documents carry the new version in info.version.
    (
        "1.37.0",
        "cce20fa1beec7e842ecf9e4cbfbd2b24341a846bb5ca2e04af1a9fad82155928",
    ),
    // Direct run.
    // Message control: the routes POST and GET /v1/runs/{runId}/messages, GET /v1/runs/{runId}/messages/{messageId} and
    // POST.../withdraw (sendMessage, listMessages, getMessage, withdrawMessage) with SendMessageRequest, SendMessageResponse, MessageView,
    // MessagesPage and their enums; the internal routes POST /internal/message-session, message-claim, message-seal and message-outcome;
    // `PendingControl::Message` (the notice of a pending message); the items `MSG#`, `MSGCTL` and `MSGRATE#` in items.schema.json; the optional
    // `AgentItem.harnessControls`. The research route: the five routes POST /internal/research/{search,fetch,repo,job,limits}
    // (researchSearch, researchFetch, researchRepo, researchJob, researchLimits) with their request and response bodies (openapi-internal.json),
    // the optional `StartDirectRunRequest.research` (openapi-v1.json) and `RuntimeContract.research` (the stored item manifest, audit object,
    // policy item, error words and limits in runtime.schema.json). The stored conversation and the capability event:
    // the agent event type `session.capabilities` with `SessionCapabilitiesPayload` (event-envelope.schema.json,
    // orchestration.schema.json), the optional `TurnCompleteRequest.transcript` with `TranscriptSummary`, `CompletenessEntry` and
    // `CompletenessKind` (openapi-internal.json) and the optional `AgentItem.features` (items.schema.json). The clean-checkout test report:
    // `TestReport`, `TestStep`, `TestCounts`, `LoggedRun`, `TestReportSummary` and the three small enums (runtime.schema.json,
    // member `directTestReport`), the optional `DirectEvidence.testReport` (runtime.schema.json) and the optional `StartDirectRunRequest.reportTests`
    // (openapi-v1.json). The skills transport and the follow-up run: the optional `StartDirectRunRequest.skills` and
    // `continueFrom` (openapi-v1.json) with `Skill`, `SkillFile` and `ContinueFrom`, and the optional `DirectSummary.skills` with `DirectSkillRef`
    // (runtime.schema.json). The recorded observations: `DirectObservation` and `DirectObservationKind` with the optional
    // `DirectSummary.observations` (runtime.schema.json) and the optional `RunCompletedPayload.observations` (orchestration.schema.json). The research counters of the summary: the optional
    // `DirectSummary.research` with `DirectResearchNote` (its `usage` now optional and the new `counts`) and `DirectResearchCounts`
    // (runtime.schema.json). The start-request additions: the optional `StartDirectRunRequest.goal`, `thirdParty` and `attachments`
    // (openapi-v1.json) with `GoalOptions`, `ThirdPartyMode` and `Attachment`. The follow-up run on the run list and run detail: the optional
    // `RunSummary.continueFrom` (openapi-v1.json, which `RunDetail` carries in its `summary`) and `RunItem.continueFrom` (items.schema.json). Both OpenAPI documents carry the new version in info.version.
    (
        "1.38.0",
        "2fa101ce63c0bc8a6cf65d1d4d08fbd25d18b19d5370451ea085278775da37c5",
    ),
    // Direct run: the optional `StartDirectRunRequest.engineConfig` with `EngineConfig {engine, schema,
    // settings}` (openapi-v1.json) and the optional `DirectSummary.engineConfig` with `DirectEngineConfigRef {engine, schema, sha256, keys}`
    // (runtime.schema.json). The check (`check_engine_config`), the key table and the denied classes carry no schema. Only these three of the eight files
    // change; openapi-internal.json changes its info.version alone.
    (
        "1.39.0",
        "2ad7248b09537f072a68f05b106d7b6cb99776e319e4f761e392757754d92303",
    ),
    // Direct run (1.40.0, wire B of the tool layer, trimmed to `code_search`): the ninth contract file `tools.schema.json` (a `tools/list` result with
    // the descriptor of each tool: name, description and `inputSchema` as constants, and the failure words); the optional `mcp` table of the engine
    // profile with `EngineMcp` and `McpVia` (engine-profile.schema.json); and the optional `DirectSummary.tools` with `DirectToolsNote` and
    // `DirectToolsCounts` (runtime.schema.json). Both OpenAPI documents change their info.version alone; event-envelope, items, evidence and
    // orchestration are byte for byte as they were. The hash covers the ninth file's name and bytes after the eight, in `SCHEMA_FILES` order.
    (
        "1.40.0",
        "befa647a283c5fe6983c1e8a879e212285fc2b904e318bf948548b2b44b67d8b",
    ),
    // Direct run: the optional `StartDirectRunRequest.reportStepS` (openapi-v1.json), the optional `TestStep.limitS`
    // and `TestReportSummary.timedOut` with `TimedOutCommand {command, afterS}` (runtime.schema.json, through `DirectEvidence.testReport`). The progress
    // payload of the `turn.progress` events of kind `report` (`ReportProgress`) is a plain serde type and carries no schema. Both OpenAPI documents change
    // their info.version; the other six files are byte for byte as they were.
    (
        "1.41.0",
        "a9063534d265afd8193db20e229355a0c4040665fdc59df51312408d2a75f694",
    ),
    // Direct run: five optional members of `GoalOptions`, the `goal` member of
    // `StartDirectRunRequest` (openapi-v1.json): `roundCalls`, `roundWindowS`, `roundMinCalls`, `commandGraceS` and `finalMessage`. The check
    // (`check_goal`) and the range constants carry no schema. Only openapi-v1.json changes its schema; openapi-internal.json changes its info.version
    // alone, and the other files are byte for byte as they were.
    (
        "1.42.0",
        "375f62689bce01b1673c04c78987d5d54a87d8097cee08423c313bdfa9a98282",
    ),
    // Direct run: the new provider word `agentcore` of `ResearchProvider`,
    // listed by `openapi-internal.json` (the research route bodies) and `runtime.schema.json` (the stored research objects). `supports_fetch`
    // and the stricter page-manifest rule carry no schema. Both OpenAPI documents change their info.version; `openapi-v1.json` changes
    // nothing else, and the other files are byte for byte as they were.
    (
        "1.43.0",
        "d03e9bbf22816be98e95bcaa9db98bb30ab7ab5eba51a3eaf74f3bdb0ff77592",
    ),
    // Direct run: the optional `StartDirectRunRequest.review` with `ReviewOptions` and `ReviewMode`
    // (openapi-v1.json), and the optional `DirectSummary.review` and `DirectEvidence.review` with `DirectReview` (runtime.schema.json). The
    // check (`check_review`), the default prompt and the reason words carry no schema. Both OpenAPI documents change their info.version; the
    // other six files are byte for byte as they were.
    (
        "1.44.0",
        "b3f14f32ad73e55072e6c03ba097250723e004c8b3c0423fa5f61da0941f5260",
    ),
    // Direct run: the optional `StartDirectRunRequest.compute` with `ComputeOptions` and `ComputeMode`
    // (openapi-v1.json), and the optional `AgentItem.compute` and `AgentItem.capacityProviderId` (items.schema.json). The words and
    // `ComputeOptions::for_mode` carry no schema. Both OpenAPI documents change their info.version; the other six files are byte for byte as they
    // were.
    (
        "1.45.0",
        "ec141b8cc25744e629479ce495755c4bb282e498bb5d092cc634e2f109232056",
    ),
    // Direct run (1.46.0, the allowance of calls that bring something new): the optional `GoalOptions.newCalls`, a member of the `goal` object
    // of `StartDirectRunRequest` (openapi-v1.json). The range `GOAL_NEW_CALLS` and the check carry no schema. Both OpenAPI documents change their
    // info.version; the other seven files are byte for byte as they were.
    (
        "1.46.0",
        "e6244ea8183e7225c944990224c87ab133d443f9eea71fb4e26f7cdfbdad3ee6",
    ),
    // Direct run: the optional `DirectModelSpec.defaultReasoningEffort` and the optional
    // `DirectSummaryEngine.reasoningEffort` and `reasoningSource` (runtime.schema.json). The levels of the user's key `reasoning.effort` are
    // not in a schema (the settings member is an open map). Both OpenAPI documents change their info.version; the other six files are byte
    // for byte as they were.
    (
        "1.47.0",
        "d5f3065afffe4663bdb230453bddea21dce63c591cd42b2bc40d177625879fd3",
    ),
    // Direct run: the optional `DirectSummary.flow` and `DirectEvidence.flow` with `DirectFlow`
    // (runtime.schema.json), the optional `StartDirectRunRequest.build` with `BuildOptions` and the optional `GoalOptions.outsideCheckoutNote`
    // (openapi-v1.json). The reading rules of the flow carry no schema. Both OpenAPI documents change their info.version; the other six files
    // are byte for byte as they were.
    (
        "1.48.0",
        "74840e7fcf7e6eaa26c80786bee969d7e583b287d06140554e7a2c0c2395544f",
    ),
    // Direct run: the optional `DirectSummary.engine.identity` (`DirectSummaryEngine`, runtime.schema.json). The check
    // (`DirectSummary::validate`, `DirectSummaryError::EngineIdentityInvalid`) carries no schema. Both OpenAPI documents change their info.version; the other
    // seven files are byte for byte as 1.45.0 has them.
    (
        "1.49.0",
        "5adf6757281f658149b232d13e26febc55afb065f80ee57f4ecb5ebc0c74e51d",
    ),
    // Direct run: the descriptions of `DirectObservationKind`, `DirectSummary.observations` and `ResearchProvider` no longer
    // name a release candidate or a test run (runtime.schema.json, orchestration.schema.json, openapi-internal.json). No member, type or rule changed. Both
    // OpenAPI documents change their info.version; the other five files are byte for byte as 1.49.0 has them.
    (
        "1.50.0",
        "a7f26ec7b1c1ff14dddadddee2125f3e398c784f19b2c3bfdcc83043f07a6368",
    ),
    // 1.51.0, wording only: the descriptions of the direct run request, the nudge file, the model list and the runtime contract say direct run and no longer use the
    // working name of the product line; the repository target says that only `cc:` targets are served; the goal status names its own values; an absent run outcome
    // is described by what the event records (openapi-v1.json, items.schema.json, orchestration.schema.json, runtime.schema.json). No member, type or rule changed.
    // Both OpenAPI documents change their info.version; the other four files are byte for byte as 1.50.0 has them.
    (
        "1.51.0",
        "e897e11ffe636245344461bbf64bb472264c909f6038915f0e52c1c3dbcadff9",
    ),
    // 1.52.0, wording only: the description of `TestReportSummary.counts` states how the counts of several test steps combine (runtime.schema.json). No member,
    // type or rule of the wire changed. Both OpenAPI documents change their info.version; the other files are byte for byte as 1.51.0 has them.
    (
        "1.52.0",
        "f939545cb67808cc0a562b6c81a508d398b3c55d99d508b1f449b5b5214705ce",
    ),
    // 1.53.0, the rename to Hyperscale Agent Meta Harness and the removal of the review-record rule: the descriptions say `ham` and `HAMH_` where they said the
    // earlier names; the repository allowlist request, view and item lose the retired member `audrecRecordId` (an older client that still sends it is accepted and the
    // member is ignored). The wire format of every other member is unchanged. Both OpenAPI documents change their info.version.
    (
        "1.53.0",
        "d60fb1bc63b5b69a1480db1eaa25759ff3a0b6b1f532c118e72c7ecb007781a3",
    ),
    // 1.54.0, two changes, neither released before the other: the completion key: the optional `CredsBinding.completionKeySha256` of the Agent item
    // (items.schema.json), the hash of the key that worker-api requires, in the header `x-hamh-completion-key`, on `/internal/events` and `/internal/turn-complete`
    // (the header is no schema member); and the test report: `TestReportSummary` gains the optional members `needs`, `servicesStarted` and `details`, each
    // left out when empty. Both OpenAPI documents change their info.version; the other files are byte for byte as 1.53.0 has them.
    (
        "1.54.0",
        "a875a95922cff4a14723a0cc24b3d307f0bad14b2887880993032348a5d251a8",
    ),
    // 1.55.0, a run continues past the 8-hour session limit and paused and stalled runs resume: the start request gains `maxRunS`; the invocation gains `slice`; the
    // turn-complete gains `handoff`; the run summary gains `statusReason`, `resumeMode` and `session`; the run item, the agent item and the goal status gain the matching
    // members and the status `stalled`; the resume route and its answer are new. Both OpenAPI documents change their info.version; the wire format of every other
    // member is unchanged.
    (
        "1.55.0",
        "9c89e5e64844f79f472041e10e95297db9728372ca9126add96d2c655cebfd45",
    ),
    // 1.56.0, the input caps of a start request are gone: the prompt, the rule files, `continue.md`, `review.md`, the attachments and the skills have no size of
    // their own, and the request as a whole is held to one bound (`MAX_REQUEST_BYTES`). No type changes shape; the descriptions of the `rules`, `skills` and
    // `attachments` members and the limits they name change, so both OpenAPI documents change (info.version included). The wire format of every member is unchanged.
    (
        "1.56.0",
        "94a4d1441eca3cfc7df9d6fb0f951eec1d0fb07456d9d3b8aa4f7090db93b2dd",
    ),
    // 1.57.0, three changes in one bump. The version records of the end-of-run objects: `TurnCompleteRequest.outputPins` (`OutputPins`: the SHA-256 and the version
    // id of agentd's own write of the bundle, `summary.json` and `test-report.json`) and `CredsBinding.outputPinsRequired`. And the `limits` member of a start request:
    // six optional numbers for the interrupts, questions and fresh sessions a run may have and the research material budgets; `StartDirectRunRequest` gains the
    // member and the schemas gain `RunLimits`. Every new member is optional and left out when absent, so a body or an item written before this version reads and writes
    // back unchanged. And the container signals of the compute choice: `StartDirectRunRequest.compute` (`ComputeOptions`) gains the optional
    // `containerSignals`, a list of lines of text. Both OpenAPI documents change their info.version, and the internal one and `items.schema.json` change a type.
    // The notices of a pending seal answer have `maxItems` 20 in the internal OpenAPI document (3 before), the count of messages that may wait for a running Cline.
    (
        "1.57.0",
        "ef5e98521bd07c076ed69d78d4b21a377d18495794a0ca9f0d67b92b897a9b8b",
    ),
    // 1.58.0, the engine of a run is on the wire: `StartDirectRunRequest.engine` (the name of an engine profile; absent means Cline), the optional `RunItem.engine`
    // that control-api stores with the run, the optional `ModelRow.engine` of a non-Cline row of the model list, and the optional `DirectEvidence.engine` that lets EVIDENCE.md
    // name the engine. Every new member is optional and left out when absent, so a body or an item written before this version reads and writes back unchanged, and the
    // request of a client that chooses no engine is byte for byte the request of 1.57.0. Both OpenAPI documents change their info.version.
    (
        "1.58.0",
        "b3f13ba3bd38d105bcf4d600ca8d890b738122ce270458abf699906679c6f087",
    ),
    // 1.59.0, provider data sharing: the start request gains the optional `providerSharing` (only `true` is ever sent) and the model list row gains the
    // optional `requiresProviderSharing` (left out when false). Both OpenAPI documents change their info.version; the wire format of every other member is
    // unchanged.
    (
        "1.59.0",
        "74632b655d4bacc9b02e587bfb33d18f07811cf3b322adde19b2e90fec8eaabe",
    ),
];

/// 1.26.0 adds one optional turn member after 1.25.0 shipped: the released line keeps its hash and
/// the new line differs from it.
#[test]
fn frozen_1_26_0_is_a_new_line_and_the_released_1_25_0_line_did_not_move() {
    let v125: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    let v126: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.26.0").collect();
    assert_eq!(
        (v125.len(), v126.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v125[0].1,
        "da823fe03f32397e093027c45655a91d98b3f8e958e2f550b3c850115c923231"
    );
    assert_ne!(v125[0].1, v126[0].1);
}

/// 1.27.0 adds the optional role record after 1.26.0 shipped: the released line keeps its hash
/// and the new line differs from it.
#[test]
fn frozen_1_27_0_is_a_new_line_and_the_released_1_26_0_line_did_not_move() {
    let v126: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.26.0").collect();
    let v127: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.27.0").collect();
    assert_eq!(
        (v126.len(), v127.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v126[0].1,
        "71c22c2d24c23138f44f44e552fb9855aac3f176fc1ec5331907afce66c7de39"
    );
    assert_ne!(v126[0].1, v127[0].1);
}

/// 1.28.0 adds the retired base tests after 1.27.0: `Requirement.retires`, `GatePlan.retire` and
/// `VerifyRequest.retire`, all optional and left out when empty. The 1.27.0 line keeps its hash, the new line differs
/// from it, and it is the newest.
#[test]
fn frozen_1_28_0_is_a_new_line_and_the_1_27_0_line_did_not_move() {
    let v127: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.27.0").collect();
    let v128: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.28.0").collect();
    assert_eq!(
        (v127.len(), v128.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v127[0].1,
        "f43ae16380c42a10f3edd27e1152899fd7242aeaf0e4f61d8f8847a0dbae2d91"
    );
    assert_ne!(v127[0].1, v128[0].1);
}

/// 1.29.0 adds the direct run request and its route after 1.28.0: the 1.28.0 line keeps its hash, the new line
/// differs from it, and it is the newest.
#[test]
fn frozen_1_29_0_is_a_new_line_and_the_1_28_0_line_did_not_move() {
    let v128: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.28.0").collect();
    let v129: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.29.0").collect();
    assert_eq!(
        (v128.len(), v129.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v128[0].1,
        "db4636864fe24ad7486523cc66a710b5803380fa62af08df84809832a724b091"
    );
    assert_ne!(v128[0].1, v129[0].1);
}

/// 1.30.0 (direct-run contract commit C0) adds the direct runtime contract after 1.29.0: the 1.29.0 line keeps its hash, the
/// new line differs from it, and it is the newest.
#[test]
fn frozen_1_30_0_is_a_new_line_and_the_1_29_0_line_did_not_move() {
    let v129: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.29.0").collect();
    let v130: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.30.0").collect();
    assert_eq!(
        (v129.len(), v130.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v129[0].1,
        "57d4ba2990ad65d6fc920e04d07f7b3df36e35fd5ab3de6fd4d449295517675e"
    );
    assert_ne!(v129[0].1, v130[0].1);
}

/// 1.31.0 (the model-choice merge) declares `GET /v1/direct/models` after 1.30.0: the 1.30.0 line keeps its hash, the new line
/// differs from it, and it is the newest.
#[test]
fn frozen_1_31_0_is_a_new_line_and_the_1_30_0_line_did_not_move() {
    let v130: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.30.0").collect();
    let v131: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.31.0").collect();
    assert_eq!(
        (v130.len(), v131.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v130[0].1,
        "c80ed6b349849e03897bb1f6cf9b7d4a253b527cfcc45247bd3465959ba0d1d7"
    );
    assert_ne!(v130[0].1, v131[0].1);
}

/// 1.32.0 (the first slice of provider failover) adds `DirectSummary.attempts` after 1.31.0: the 1.31.0 line keeps its hash, the new line
/// differs from it, and it is the newest.
#[test]
fn frozen_1_32_0_is_a_new_line_and_the_1_31_0_line_did_not_move() {
    let v131: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.31.0").collect();
    let v132: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.32.0").collect();
    assert_eq!(
        (v131.len(), v132.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v131[0].1,
        "cdf05ed6295e272520a10354e3bf9f6adb18eabcfe0207d9b153bbfde4bb92e9"
    );
    assert_ne!(v131[0].1, v132[0].1);
}

/// 1.33.0 (the shell target) adds `ShellTargetResponse.runtimeEndpoint` after 1.32.0: the 1.32.0 line keeps its hash and the new line
/// differs from it.
#[test]
fn frozen_1_33_0_is_a_new_line_and_the_1_32_0_line_did_not_move() {
    let v132: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.32.0").collect();
    let v133: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.33.0").collect();
    assert_eq!(
        (v132.len(), v133.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v132[0].1,
        "d60a9d8f0d42529e8d1a82621543bc3f4a605ebd407aceeb6d9bb8792a3b77a9"
    );
    assert_ne!(v132[0].1, v133[0].1);
}

/// 1.34.0 adds the failover request and summary members after 1.33.0: the 1.33.0 line keeps its hash and the
/// new line differs from it.
#[test]
fn frozen_1_34_0_is_a_new_line_and_the_1_33_0_line_did_not_move() {
    let v133: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.33.0").collect();
    let v134: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.34.0").collect();
    assert_eq!(
        (v133.len(), v134.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v133[0].1,
        "f7177e6a1e2f22b334a00c87f8963d6bbc1254e5ea518da5f74a96ba0509d139"
    );
    assert_ne!(v133[0].1, v134[0].1);
}

/// 1.35.0 (the old multi-agent line's removal) drops the gate, evaluator, planner, route-ladder, hidden-test, repo-read, modes, plan, policy
/// and approval types and six contract files (`gates`, `verdict`, `modes`, `plan`, `policy`, `catalog`): the 1.34.0 line keeps its hash, the new
/// line differs from it, and it is the newest.
#[test]
fn frozen_1_35_0_is_a_new_line_and_the_1_34_0_line_did_not_move() {
    let v134: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.34.0").collect();
    let v135: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.35.0").collect();
    assert_eq!(
        (v134.len(), v135.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v134[0].1,
        "2af0fc2ac9bfccac0196fbe1adbb26ff05ee4bb798def5f6e016fe1c69470782"
    );
    assert_ne!(v134[0].1, v135[0].1);
}

/// 1.36.0 (the `stopped_no_changes` outcome of a direct run, row 16) adds one member to the `DirectOutcome` enum after 1.35.0: the 1.35.0
/// line (the removal of the old multi-agent line) keeps its hash, the new line differs from it, and it is the newest.
#[test]
fn frozen_1_36_0_is_a_new_line_and_the_1_35_0_line_did_not_move() {
    let v135: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.35.0").collect();
    let v136: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.36.0").collect();
    assert_eq!(
        (v135.len(), v136.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v135[0].1,
        "03a32e5aef49ed3ef5cd6e2a92b00e8b6da2326e93b54429b7eeebd8ff3fdb52"
    );
    assert_ne!(v135[0].1, v136[0].1);
}

/// 1.37.0 adds the answer route, the `instruct` control, the `inputWaitS` request members, two attempt kinds
/// and the `interventions` and `freshSessions` summary members after 1.36.0: the 1.36.0 line keeps its hash, the new line differs from
/// it, and it is the newest.
#[test]
fn frozen_1_37_0_is_a_new_line_and_the_1_36_0_line_did_not_move() {
    let v136: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.36.0").collect();
    let v137: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.37.0").collect();
    assert_eq!(
        (v136.len(), v137.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v136[0].1,
        "7223f31f1b0f472e2789f515811868e0a185d8dc006d3a3417dd736915d31550"
    );
    assert_ne!(v136[0].1, v137[0].1);
}

/// 1.38.0 with the skills and follow-up members follows 1.37.0: the 1.37.0 line
/// keeps its hash and the new line differs from it. (Which line is the newest is pinned by the test of the newest version, below.)
#[test]
fn frozen_1_38_0_is_a_new_line_and_the_1_37_0_line_did_not_move() {
    let v137: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.37.0").collect();
    let v138: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.38.0").collect();
    assert_eq!(
        (v137.len(), v138.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v137[0].1,
        "cce20fa1beec7e842ecf9e4cbfbd2b24341a846bb5ca2e04af1a9fad82155928"
    );
    assert_ne!(v137[0].1, v138[0].1);
}

/// The skills and follow-up branch pinned two of its own (`edb45498`, then `8ede92a3` on top of the first batch) and pinned `db9de444` once merged; the goal-loop branch pinned `c9ab22e1` for the recorded
/// observations. The batch that merged the goal loop pinned `b6f17efc`; the research counters branch pinned `ae915f7b` on a base without the observations; batch D2 pinned `980a22f6`, and the start-request
/// branch pinned `2c147c64` and then `fd38f15d` before it met the batches that came before it; main pinned `4dc2e22a` for batch D3 before the follow-up run members joined the line. None of those hashes
/// was released, and none of them is the line now: the line covers every set of members. (1.39.0 is the next line, pinned by the test below.)
#[test]
fn frozen_1_38_0_replaces_the_per_branch_placeholders() {
    let line = FROZEN.iter().find(|(v, _)| *v == "1.38.0").unwrap().1;
    for placeholder in [
        "76fe62be7ef4ffa5de6fb2b65fc8983b740cd6f9fbcff74073e1e1822344d794",
        "d947c3aeff84bbdcddca7ce22a39c37c4c799843755ce7b7c2016e7b7cdf2b52",
        "a826a75d4350fd9ec01ba7d1aaeaede61b116798b2e007d86ded8c67ecad2943",
        "626775944746eb613ecfe90bbe251848865d93dc82be4cd1cd4c1c1baf8d9972",
        "edb45498e5d1bf6eb759468989042f55423b7e02ac15f75b83d79f21a5376fbe",
        "8ede92a364d4febabd2d9271ab73314710c9244f6ce69c4c6556787f3b6c6bea",
        "0f1b28edd92ddf5c6b30c41e633474478274f7df66f21b7e8095ffa561c9aa7f",
        "8da73bdbb27ef7926b4eb81d83ca57dc31fe9dce96cef889c1f0277efd408272",
        "9cb9f2d9a5c6c4ea96805165a8c09f1202e1273949d74a65b1ee2a646c746a32",
        "d26685a61322337d6775d41b99c2c748f3bf7ef5377f4157b788a7747e07e120",
        "db9de444638a8b81085a84f991ac624b5f2faf08228f4de639442c0c6f86d6eb",
        "c9ab22e1392106d41f26cbd820362f960d86420d080270aaced820bd9c479f92",
        "ae915f7b47fa343734e66d3bcc70d127a0f9ac156b96b58462384eb0777b33ac",
        "b6f17efc48f8ee6ee32af96e9eb7428ae2bfc6d3bf95dfd80760563639a59c2c",
        "2c147c64412ca69b41fb5454c0666f13e9f68005c0060821352846fff9247991",
        "fd38f15dfc9ddb38f62038a4f6c68aed19daf873bd185d54b5b52047aa1e625f",
        "980a22f65cb6c4a131f32caaaa08a8c3e72e62355abd2d58d88e90213fca02b5",
        "4dc2e22a0d3e0aa4da6ba5067228b23c8b1fed4229b588ea74f17f7059efb4da",
    ] {
        assert_ne!(
            line, placeholder,
            "a per-branch placeholder hash is not the release line"
        );
    }
}

/// 1.39.0 follows the released 1.38.0 line: that line keeps its hash, the new line differs from it, and it is the newest.
#[test]
fn frozen_1_39_0_is_a_new_line_and_the_released_1_38_0_line_did_not_move() {
    let v138: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.38.0").collect();
    let v139: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.39.0").collect();
    assert_eq!(
        (v138.len(), v139.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v138[0].1,
        "2fa101ce63c0bc8a6cf65d1d4d08fbd25d18b19d5370451ea085278775da37c5"
    );
    assert_ne!(v138[0].1, v139[0].1);
    assert_eq!(
        v139[0].1,
        "2ad7248b09537f072a68f05b106d7b6cb99776e319e4f761e392757754d92303"
    );
}

/// 1.40.0 (wire B of the tool layer, trimmed to `code_search`: the `[mcp]` table of the engine profile, `DirectSummary.tools`, and the ninth contract
/// file `tools.schema.json`) follows the released 1.39.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest
/// line: 1.41.0 follows it, so the test no longer says so.
#[test]
fn frozen_1_40_0_is_a_new_line_and_the_released_1_39_0_line_did_not_move() {
    let v139: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.39.0").collect();
    let v140: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.40.0").collect();
    assert_eq!(
        (v139.len(), v140.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v139[0].1,
        "2ad7248b09537f072a68f05b106d7b6cb99776e319e4f761e392757754d92303"
    );
    assert_ne!(v139[0].1, v140[0].1);
}

/// 1.41.0 follows the released 1.40.0 line:
/// that line keeps its hash and the new line differs from it. It is no longer the newest line: 1.42.0 follows it, so the test no longer says so.
#[test]
fn frozen_1_41_0_is_a_new_line_and_the_released_1_40_0_line_did_not_move() {
    let v140: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.40.0").collect();
    let v141: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.41.0").collect();
    assert_eq!(
        (v140.len(), v141.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v140[0].1,
        "befa647a283c5fe6983c1e8a879e212285fc2b904e318bf948548b2b44b67d8b"
    );
    assert_ne!(v140[0].1, v141[0].1);
}

/// 1.42.0 follows the
/// 1.41.0 line (the report card): that line keeps its hash and the new line differs from it. It is no longer the newest line: 1.43.0 follows it, so
/// the test no longer says so.
/// The test of 1.41.0 above no longer says that 1.41.0 is the last line: that stopped being true with this version.
#[test]
fn frozen_1_42_0_is_a_new_line_and_the_1_41_0_line_did_not_move() {
    let v141: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.41.0").collect();
    let v142: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.42.0").collect();
    assert_eq!(
        (v141.len(), v142.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v141[0].1,
        "a9063534d265afd8193db20e229355a0c4040665fdc59df51312408d2a75f694"
    );
    assert_ne!(v141[0].1, v142[0].1);
}

/// 1.43.0 follows the 1.42.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest line: 1.44.0 follows it, so the test no longer says so.
#[test]
fn frozen_1_43_0_is_a_new_line_and_the_1_42_0_line_did_not_move() {
    let v142: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.42.0").collect();
    let v143: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.43.0").collect();
    assert_eq!(
        (v142.len(), v143.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v142[0].1,
        "375f62689bce01b1673c04c78987d5d54a87d8097cee08423c313bdfa9a98282"
    );
    assert_ne!(v142[0].1, v143[0].1);
}

/// 1.44.0 follows the 1.43.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest line: 1.45.0 follows it.
#[test]
fn frozen_1_44_0_is_a_new_line_and_the_1_43_0_line_did_not_move() {
    let v143: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.43.0").collect();
    let v144: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.44.0").collect();
    assert_eq!(
        (v143.len(), v144.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v143[0].1,
        "d03e9bbf22816be98e95bcaa9db98bb30ab7ab5eba51a3eaf74f3bdb0ff77592"
    );
    assert_ne!(v143[0].1, v144[0].1);
}

/// 1.45.0 follows the 1.44.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest
/// line: 1.46.0 follows it.
#[test]
fn frozen_1_45_0_is_a_new_line_and_the_1_44_0_line_did_not_move() {
    let v144: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.44.0").collect();
    let v145: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.45.0").collect();
    assert_eq!(
        (v144.len(), v145.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v144[0].1,
        "b3f14f32ad73e55072e6c03ba097250723e004c8b3c0423fa5f61da0941f5260"
    );
    assert_ne!(v144[0].1, v145[0].1);
}

/// 1.46.0 (the allowance of calls that bring something new) follows the 1.45.0 line: that line keeps its hash and the new line differs from it. It is no
/// longer the newest line: 1.47.0 follows it.
#[test]
fn frozen_1_46_0_is_a_new_line_and_the_1_45_0_line_did_not_move() {
    let v145: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.45.0").collect();
    let v146: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.46.0").collect();
    assert_eq!(
        (v145.len(), v146.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v145[0].1,
        "ec141b8cc25744e629479ce495755c4bb282e498bb5d092cc634e2f109232056"
    );
    assert_ne!(v145[0].1, v146[0].1);
}

/// 1.47.0 follows the 1.46.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest
/// line: 1.48.0 follows it.
#[test]
fn frozen_1_47_0_is_a_new_line_and_the_1_46_0_line_did_not_move() {
    let v146: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.46.0").collect();
    let v147: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.47.0").collect();
    assert_eq!(
        (v146.len(), v147.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v146[0].1,
        "e6244ea8183e7225c944990224c87ab133d443f9eea71fb4e26f7cdfbdad3ee6"
    );
    assert_ne!(v146[0].1, v147[0].1);
}

/// 1.48.0 follows the 1.47.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest
/// line: 1.49.0 follows it.
#[test]
fn frozen_1_48_0_is_a_new_line_and_the_1_47_0_line_did_not_move() {
    let v147: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.47.0").collect();
    let v148: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.48.0").collect();
    assert_eq!(
        (v147.len(), v148.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v147[0].1,
        "d5f3065afffe4663bdb230453bddea21dce63c591cd42b2bc40d177625879fd3"
    );
    assert_ne!(v147[0].1, v148[0].1);
}

/// 1.49.0 follows the 1.48.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest line:
/// 1.50.0 follows it.
#[test]
fn frozen_1_49_0_is_a_new_line_and_the_1_48_0_line_did_not_move() {
    let v148: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.48.0").collect();
    let v149: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.49.0").collect();
    assert_eq!(
        (v148.len(), v149.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v148[0].1,
        "74840e7fcf7e6eaa26c80786bee969d7e583b287d06140554e7a2c0c2395544f"
    );
    assert_ne!(v148[0].1, v149[0].1);
}

/// 1.50.0 follows the 1.49.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest line:
/// 1.51.0 follows it.
#[test]
fn frozen_1_50_0_is_a_new_line_and_the_1_49_0_line_did_not_move() {
    let v149: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.49.0").collect();
    let v150: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.50.0").collect();
    assert_eq!(
        (v149.len(), v150.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v149[0].1,
        "5adf6757281f658149b232d13e26febc55afb065f80ee57f4ecb5ebc0c74e51d"
    );
    assert_ne!(v149[0].1, v150[0].1);
}

/// 1.51.0 (wording only) follows the 1.50.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest line: 1.52.0 follows it.
#[test]
fn frozen_1_51_0_is_a_new_line_and_the_1_50_0_line_did_not_move() {
    let v150: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.50.0").collect();
    let v151: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.51.0").collect();
    assert_eq!(
        (v150.len(), v151.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v150[0].1,
        "a7f26ec7b1c1ff14dddadddee2125f3e398c784f19b2c3bfdcc83043f07a6368"
    );
    assert_ne!(v150[0].1, v151[0].1);
}

/// 1.52.0 (wording only: how the counts of several test steps combine) follows the 1.51.0 line: that line keeps its hash and the new line differs from it. It is no
/// longer the newest line: 1.53.0 follows it.
#[test]
fn frozen_1_52_0_is_a_new_line_and_the_1_51_0_line_did_not_move() {
    let v151: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.51.0").collect();
    let v152: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.52.0").collect();
    assert_eq!(
        (v151.len(), v152.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v151[0].1,
        "e897e11ffe636245344461bbf64bb472264c909f6038915f0e52c1c3dbcadff9"
    );
    assert_ne!(v151[0].1, v152[0].1);
}

/// 1.53.0 (the rename and the removal of the review-record rule) follows the 1.52.0 line: that line keeps its hash and the new line differs from it. It is no
/// longer the newest line: 1.54.0 follows it.
#[test]
fn frozen_1_53_0_is_a_new_line_and_the_1_52_0_line_did_not_move() {
    let v152: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.52.0").collect();
    let v153: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.53.0").collect();
    assert_eq!(
        (v152.len(), v153.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v152[0].1,
        "f939545cb67808cc0a562b6c81a508d398b3c55d99d508b1f449b5b5214705ce"
    );
    assert_ne!(v152[0].1, v153[0].1);
}

/// 1.54.0 follows the 1.53.0 line: that line keeps its hash and the new line differs from it. It is
/// no longer the newest line: 1.55.0 follows it.
#[test]
fn frozen_1_54_0_is_a_new_line_and_the_1_53_0_line_did_not_move() {
    let v153: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.53.0").collect();
    let v154: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.54.0").collect();
    assert_eq!(
        (v153.len(), v154.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v153[0].1,
        "d60fb1bc63b5b69a1480db1eaa25759ff3a0b6b1f532c118e72c7ecb007781a3"
    );
    assert_ne!(v153[0].1, v154[0].1);
}

/// 1.55.0 (a run continues past the 8-hour session limit; paused and stalled runs resume) follows the 1.54.0 line: that line keeps its hash, the new line differs
/// from it. It is no longer the newest line: 1.56.0 follows it.
#[test]
fn frozen_1_55_0_is_a_new_line_and_the_1_54_0_line_did_not_move() {
    let v154: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.54.0").collect();
    let v155: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.55.0").collect();
    assert_eq!(
        (v154.len(), v155.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v154[0].1,
        "a875a95922cff4a14723a0cc24b3d307f0bad14b2887880993032348a5d251a8"
    );
    assert_ne!(v154[0].1, v155[0].1);
}

/// 1.56.0 (the input caps of a start request are gone) follows the 1.55.0 line: that line keeps its hash and the new line differs from it. It is no longer the newest line: 1.57.0 follows it.
#[test]
fn frozen_1_56_0_is_a_new_line_and_the_1_55_0_line_did_not_move() {
    let v155: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.55.0").collect();
    let v156: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.56.0").collect();
    assert_eq!(
        (v155.len(), v156.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v155[0].1,
        "9c89e5e64844f79f472041e10e95297db9728372ca9126add96d2c655cebfd45"
    );
    assert_ne!(v155[0].1, v156[0].1);
}

/// 1.57.0 follows the 1.56.0 line: that line keeps its hash, the new line differs from it, and it is the newest.
#[test]
fn b759_frozen_1_57_0_is_a_new_line_and_the_1_56_0_line_did_not_move() {
    let v156: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.56.0").collect();
    let v157: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.57.0").collect();
    assert_eq!(
        (v156.len(), v157.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v156[0].1,
        "94a4d1441eca3cfc7df9d6fb0f951eec1d0fb07456d9d3b8aa4f7090db93b2dd"
    );
    assert_ne!(v156[0].1, v157[0].1);
}

/// 1.58.0 follows the 1.57.0 line: that line keeps its hash, the new line differs
#[test]
fn b787_frozen_1_58_0_is_a_new_line_and_the_1_57_0_line_did_not_move() {
    let v157: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.57.0").collect();
    let v158: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.58.0").collect();
    assert_eq!(
        (v157.len(), v158.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v157[0].1,
        "ef5e98521bd07c076ed69d78d4b21a377d18495794a0ca9f0d67b92b897a9b8b"
    );
    assert_ne!(v157[0].1, v158[0].1);
}

/// 1.59.0 follows the 1.58.0 line: that line keeps its hash, the new line differs from it, and it is the newest.
#[test]
fn b803_frozen_1_59_0_is_a_new_line_and_the_1_58_0_line_did_not_move() {
    let v158: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.58.0").collect();
    let v159: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.59.0").collect();
    assert_eq!(
        (v158.len(), v159.len()),
        (1, 1),
        "one pinned line per version"
    );
    assert_eq!(
        v158[0].1,
        "b3f13ba3bd38d105bcf4d600ca8d890b738122ce270458abf699906679c6f087"
    );
    assert_ne!(v158[0].1, v159[0].1);
    assert_eq!(FROZEN.last().map(|(v, _)| *v), Some("1.59.0"));
}

/// The ninth file is appended after the eight: the order of `SCHEMA_FILES` is part of the hash, so the file joins at the end and no existing file moves.
#[test]
fn the_tools_schema_joins_the_contract_files_at_the_end() {
    assert_eq!(SCHEMA_FILES.len(), 9);
    assert_eq!(SCHEMA_FILES.last(), Some(&"tools.schema.json"));
    assert_eq!(SCHEMA_FILES[7], "orchestration.schema.json");
}

fn contract_sha256() -> String {
    let mut h = Sha256::new();
    for (name, value) in all_schemas() {
        h.update(name.as_bytes());
        h.update(b"\n");
        h.update(render(&value).as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_contract_is_frozen_at_its_protocol_version() {
    let sha = contract_sha256();
    let pinned = FROZEN
        .iter()
        .find(|(v, _)| *v == PROTOCOL_VERSION)
        .map(|(_, s)| *s);
    assert_eq!(
        pinned,
        Some(sha.as_str()),
        "harness-protocol {PROTOCOL_VERSION}: the contract files hash to {sha}; a changed contract \
         needs a new version pinned in FROZEN"
    );
}

/// re-pins the unreleased 1.25.0 line once more: the wire types of the
/// hidden-test admission (`Evidence::Review`, `FailureClass`, `hidden.admission`, `VerifyOp::Witness`,
/// the triage action and event, the admission items, `RulingRequest`, `StartRunRequest.hiddenInject`).
/// m8b's 1.25.0 hash was 0d80c931...; a released line never moves, and 1.25.0 keeps one line.
#[test]
fn frozen_1_25_0_line_repinned() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "0d80c931eb745dace5ae31c73d2e7605ad9b9b97ee37830b575c82363e156df5",
        "the contract files changed, so the 1.25.0 line must be re-pinned"
    );
    // The released line before it is untouched.
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}

/// re-pins the unreleased 1.25.0 line once more: `RulingRequest` gains the optional `writerFamilies`,
/// `designerFamily`, `acceptanceSha256` and `repairAcceptanceSha256`, so the evaluator runtime can
/// re-check the judge's family and bind the ruling to the exact hidden sets.
#[test]
fn frozen_1_25_0_line_repinned_for_the_ruling_binding() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "638a4f41c76d07a4b8712704a6fa9a00e51a6d5ec5261f7dfb5e5e0474e38498",
        "WP2b changes RulingRequest in orchestration.schema.json, so WP2's 1.25.0 line must be re-pinned"
    );
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}

/// re-pins the unreleased 1.25.0 line once more: `VerifyRequest` gains the optional `revises`
/// (`HiddenRevises`: `fromRev`, `requirementId`, `files`), so the verifier can check that a staged
/// revision changes one requirement only.
#[test]
fn frozen_1_25_0_line_repinned_for_the_revision_collect() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "7d545899eee932ada8d9fd492c8d6c627bbfa613ea5d3c43af044d95e176b020",
        "VerifyRequest.revises changes orchestration.schema.json, so WP2b's 1.25.0 line must be re-pinned"
    );
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}

/// re-pins the unreleased 1.25.0 line once more: `GatePlan` gains the optional
/// `hiddenRev`, the hidden revision the Lane B base check of a `hiddenProof` plan runs for, so a revised
/// set whose tests need a container runtime is proved red in a build that has one. The line before this
/// was 6237968e...; a released line never moves, and 1.25.0 keeps one line.
#[test]
fn frozen_1_25_0_line_repinned_for_the_base_check_revision() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "6237968e710e071bfddc63379274c67d07fa1a12a1ec6ddfe5879a83e494639a",
        "GatePlan.hiddenRev changes orchestration.schema.json, so the previous 1.25.0 line must be re-pinned"
    );
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}

/// The deterministic cheat suite re-pins the unreleased 1.25.0 line once more: `StartRunRequest.cheatPlant`
/// (`CheatPlant {case}`, drill stage and operator principals, admitted by control-api) and `TurnSpec.cheat`
/// (`TurnCheat {case, mode, overlaySha256}` with `CheatMode` `plant`, `typist` or `scripted-model`, which
/// the dispatcher puts on the writer turns of a planted run).
#[test]
fn frozen_1_25_0_line_repinned_for_the_cheat_plant() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "509e2648c31a92129ed140d1a7975f890b909b8447a00fc4bf8bc43db61890a4",
        "StartRunRequest.cheatPlant and TurnSpec.cheat change the contract files, so h-base-op's 1.25.0 line must be re-pinned"
    );
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}

/// Review of m8c re-pins the unreleased 1.25.0 line once more:
/// `InvocationRequest.packagesTokenExpiresAt`, the Unix second the dispatcher's package token ends by its
/// own count (taken before it asked CodeArtifact), so agentd's lock step measures its budget from the real
/// end of the token and not from the end of the turn. Optional and left out when absent, so an invocation
/// written before keeps its bytes. The cheat-wire line was ec1806f9...; a released line never moves, and
/// 1.25.0 keeps one line.
#[test]
fn frozen_1_25_0_line_repinned_for_the_package_token_expiry() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "ec1806f9ddad967a589de78c5b97c35679c4eed76d3b55fdfaf9f35cc7892b8b",
        "InvocationRequest.packagesTokenExpiresAt changes runtime.schema.json, so the cheat-wire 1.25.0 line must be re-pinned"
    );
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}

/// The cheat suite's layer 2 re-pins the unreleased 1.25.0 line once more: `CheatPlant.mode`, the route a
/// planted run's cheat takes into the tree (`plant`, the default when absent, or `scripted-model`, the engine's
/// own tools against a scripted model: the cheat-engine record), so `CheatMode` joins the openapi document.
#[test]
fn frozen_1_25_0_line_repinned_for_the_cheat_engine_route() {
    let lines: Vec<&(&str, &str)> = FROZEN.iter().filter(|(v, _)| *v == "1.25.0").collect();
    assert_eq!(lines.len(), 1, "one pinned line per version");
    assert_ne!(
        lines[0].1, "4f694567b205dc9d0372f8d2a4db6cde600900baae814fe2f150850b58b5da0e",
        "CheatPlant.mode changes openapi-v1.json, so h-fix-agentd-lock's 1.25.0 line must be re-pinned"
    );
    let v124 = FROZEN.iter().find(|(v, _)| *v == "1.24.0").unwrap();
    assert_eq!(
        v124.1,
        "690e3b383e596a4f68ea639eb534798e51a9004ba71e12ee3a6ce30b4cc320fc"
    );
}
