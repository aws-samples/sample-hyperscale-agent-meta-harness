import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { ACCOUNT, OWNER, REGION } from "../lib/config.ts";
import { phase2Images, resourcesOf, synth } from "./helpers.ts";

// Direct run: the orchestration stack of stage `direct`: one Lambda (run-session), the Standard
// state machine hamh-direct-run, the run-ended rule and the small Standard machine it starts (hamh-direct-run-ended, which keeps calling
// run-session until a run whose execution failed is cleared); and the least-privilege shape of every role around them.
const { templates } = synth("direct", phase2Images("direct"));
const orch = templates.get("hamh-direct-orchestration")!;
const runtimes = templates.get("hamh-direct-runtimes")!.toJSON();
const security = templates.get("hamh-direct-security")!;
const control = templates.get("hamh-direct-control")!;

type Stmt = Record<string, any>;

const rolePolicy = (t: { toJSON(): any }, roleName: string): Stmt[] => {
  const roles = Object.values((t.toJSON().Resources ?? {}) as Record<string, any>).filter((r) => r.Type === "AWS::IAM::Role");
  const role = roles.find((r) => r.Properties.RoleName === roleName);
  expect(role, roleName).toBeDefined();
  return role.Properties.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
};
const sids = (s: Stmt[]) => s.map((x) => x.Sid);
const byId = (s: Stmt[], sid: string): Stmt => {
  const x = s.find((y) => y.Sid === sid);
  expect(x, sid).toBeDefined();
  return x!;
};
const actions = (s: Stmt) => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const resources = (s: Stmt) => (Array.isArray(s.Resource) ? s.Resource : [s.Resource]);
const flat = (v: unknown) => JSON.stringify(v);

const runSession = rolePolicy(orch, "hamh-direct-run-session");

describe("the stack", () => {
  test("holds run-session, the run machine, the run-ended machine and rule, and nothing else that runs code", () => {
    const fns = Object.values(resourcesOf(orch, "AWS::Lambda::Function")).map((r) => r.Properties as any);
    expect(fns.map((f) => f.FunctionName)).toEqual(["hamh-direct-run-session"]);
    const [f] = fns;
    expect(f.Runtime).toBe("provided.al2023");
    expect(f.Architectures).toEqual(["arm64"]);
    expect(f.MemorySize).toBe(256);
    expect(f.Timeout).toBe(300);
    expect(f.TracingConfig).toEqual({ Mode: "Active" });
    const machines = Object.values(resourcesOf(orch, "AWS::StepFunctions::StateMachine")).map((r) => r.Properties as any);
    expect(machines.map((m) => [m.StateMachineName, m.StateMachineType]).sort()).toEqual([
      ["hamh-direct-run", "STANDARD"], ["hamh-direct-run-ended", "STANDARD"]]);
    for (const m of machines) expect(m.TracingConfiguration).toEqual({ Enabled: true });
    const rules = Object.values(resourcesOf(orch, "AWS::Events::Rule")).map((r) => r.Properties as any);
    expect(rules.map((r) => r.Name)).toEqual(["hamh-direct-run-ended"]);
    expect(Object.keys(orch.toJSON().Resources).some((k) => /EventSourceMapping/.test(k))).toBe(false);
  });

  test("the function's environment is exactly the seventeen settings the Lambda reads", () => {
    const f = Object.values(resourcesOf(orch, "AWS::Lambda::Function"))[0].Properties as any;
    const env = f.Environment.Variables;
    expect(Object.keys(env).sort()).toEqual([
      "BUCKET_NAME", "DATA_KEY_ARN", "HAMH_STAGE", "INTERNAL_API_URL", "PACKAGES_DOMAIN", "PACKAGES_OWNER", "PACKAGES_REPOSITORY",
      "RESEARCH_PROVIDER", "RESEARCH_TABLE_NAME", "RUNTIME_ARN_PARAMETER", "RUN_STATE_MACHINE_ARN", "TABLE_NAME", "TENANT",
      "UTILITY_MODEL", "UTILITY_PURPOSES", "WORKER_MAX_LIFETIME_S", "WORKER_SESSION_ROLE_ARN",
    ]);
    // The utility model's row id (the catalog default), which the Lambda reads to decide whether to name a run
    expect(env.UTILITY_MODEL).toBe("us.anthropic.claude-haiku-5-5");
    // The purposes the stage runs (the catalog default is every purpose the row has a smoke record for)
    expect(env.UTILITY_PURPOSES).toBe("run-name,run-tags");
    expect(env.HAMH_STAGE).toBe("direct");
    expect(env.TABLE_NAME).toBe("hamh-direct-harness");
    expect(env.BUCKET_NAME).toBe(`hamh-direct-harness-${ACCOUNT}`);
    expect(env.RUNTIME_ARN_PARAMETER).toBe("/hamh/direct/worker-python-runtime-arn");
    expect(env.WORKER_SESSION_ROLE_ARN).toBe(`arn:aws:iam::${ACCOUNT}:role/hamh-direct-harness-worker-session`);
    expect(env.WORKER_MAX_LIFETIME_S).toBe("28800");
    expect(env.TENANT).toBe(OWNER);
    expect(env.RUN_STATE_MACHINE_ARN).toBe(`arn:aws:states:${REGION}:${ACCOUNT}:stateMachine:hamh-direct-run`);
    expect(flat(env.INTERNAL_API_URL)).toContain("execute-api");
  });
});

describe("the state machines name themselves", () => {
  // The Comment of a definition is part of the stored definition (a rename moves the ASL version and the deployment digest): it starts with the machine's own physical name.
  test("each definition's Comment starts with the StateMachineName it is deployed under", () => {
    const machines = Object.values(resourcesOf(orch, "AWS::StepFunctions::StateMachine")).map((r) => r.Properties as any);
    expect(machines.map((m) => m.StateMachineName).sort()).toEqual(["hamh-direct-run", "hamh-direct-run-ended"]);
    for (const m of machines) {
      const comment = JSON.parse(m.DefinitionString).Comment as string;
      expect(comment.startsWith(`${m.StateMachineName}: `), m.StateMachineName).toBe(true);
    }
  });
});

describe("the run machine", () => {
  const machine = Object.values(resourcesOf(orch, "AWS::StepFunctions::StateMachine")).map((r) => r.Properties as any)
    .find((m) => m.StateMachineName === "hamh-direct-run");

  test("its definition is direct.asl.json with the four functions substituted", () => {
    expect(machine.DefinitionString).toBe(readFileSync(new URL("../../statemachines/direct.asl.json", import.meta.url), "utf8"));
    expect(Object.keys(machine.DefinitionSubstitutions).sort()).toEqual(["BundleValidatorArn", "PackagerArn", "RepoGatewayArn", "RunSessionArn"]);
    const subs = machine.DefinitionSubstitutions;
    expect(subs.RepoGatewayArn).toBe(`arn:aws:lambda:${REGION}:${ACCOUNT}:function:hamh-direct-repo-gateway`);
    expect(subs.BundleValidatorArn).toBe(`arn:aws:lambda:${REGION}:${ACCOUNT}:function:hamh-direct-bundle-validator`);
    expect(subs.PackagerArn).toBe(`arn:aws:lambda:${REGION}:${ACCOUNT}:function:hamh-direct-packager`);
    expect(flat(subs.RunSessionArn)).toContain("RunSessionFunction");
  });

  test("its role may invoke run-session and the three delivery Lambdas, and call nothing else", () => {
    const s = rolePolicy(orch, "hamh-direct-run");
    expect(sids(s).sort()).toEqual(["RunSteps", "Tracing"]);
    expect(actions(byId(s, "RunSteps"))).toEqual(["lambda:InvokeFunction"]);
    const invoked = flat(resources(byId(s, "RunSteps")));
    for (const fn of ["repo-gateway", "bundle-validator", "packager"]) {
      expect(invoked).toContain(`function:hamh-direct-${fn}`);
    }
    expect(invoked).toContain("RunSessionFunction");
    expect(resources(byId(s, "RunSteps")).length).toBe(8);
    for (const x of s) expect(flat(actions(x))).not.toMatch(/dynamodb|s3:|bedrock|states:|sts:|kms|codecommit|codebuild/);
  });

  test("the trust policy is Step Functions' alone", () => {
    const roles = Object.values(resourcesOf(orch, "AWS::IAM::Role")).map((r) => r.Properties as any);
    const run = roles.find((r) => r.RoleName === "hamh-direct-run");
    expect(run.AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "states.amazonaws.com" }, Action: "sts:AssumeRole" }]);
  });
});

describe("run-ended", () => {
  const reaper = () => Object.values(resourcesOf(orch, "AWS::StepFunctions::StateMachine")).map((r) => r.Properties as any)
    .find((m) => m.StateMachineName === "hamh-direct-run-ended");

  test("a status change of this machine that ends ABORTED, FAILED or TIMED_OUT starts the run-ended machine, not the function", () => {
    const rule = Object.values(resourcesOf(orch, "AWS::Events::Rule"))[0].Properties as any;
    expect(rule.EventPattern.source).toEqual(["aws.states"]);
    expect(rule.EventPattern["detail-type"]).toEqual(["Step Functions Execution Status Change"]);
    expect(rule.EventPattern.detail.status).toEqual(["ABORTED", "FAILED", "TIMED_OUT"]);
    expect(flat(rule.EventPattern.detail.stateMachineArn)).toContain("Run");
    expect(rule.Targets).toHaveLength(1);
    // An asynchronous Lambda target is retried twice within minutes and then dropped; a Standard machine retries for days.
    expect(flat(rule.Targets[0].Arn)).toContain("RunEndedMachine");
    expect(flat(rule.Targets[0].Arn)).not.toContain("RunSessionFunction");
    expect(flat(rule.Targets[0].RoleArn)).toContain("RunEndedEventsRole");
    expect(Object.values(resourcesOf(orch, "AWS::Lambda::Permission"))).toHaveLength(0);
  });

  test("the machine is direct-run-ended.asl.json with run-session substituted", () => {
    const m = reaper();
    expect(m.DefinitionString).toBe(readFileSync(new URL("../../statemachines/direct-run-ended.asl.json", import.meta.url), "utf8"));
    expect(Object.keys(m.DefinitionSubstitutions)).toEqual(["RunSessionArn"]);
    expect(flat(m.DefinitionSubstitutions.RunSessionArn)).toContain("RunSessionFunction");
  });

  test("its role may invoke run-session and nothing else; the rule's role may start this machine and nothing else", () => {
    const s = rolePolicy(orch, "hamh-direct-run-ended");
    expect(sids(s).sort()).toEqual(["RunSessionInvoke", "Tracing"]);
    expect(actions(byId(s, "RunSessionInvoke"))).toEqual(["lambda:InvokeFunction"]);
    expect(flat(resources(byId(s, "RunSessionInvoke")))).toContain("RunSessionFunction");
    expect(resources(byId(s, "RunSessionInvoke"))).toHaveLength(2);
    const events = rolePolicy(orch, "hamh-direct-run-ended-events");
    expect(sids(events)).toEqual(["StartReaper"]);
    expect(actions(byId(events, "StartReaper"))).toEqual(["states:StartExecution"]);
    expect(flat(resources(byId(events, "StartReaper")))).toContain("RunEndedMachine");
    const roles = Object.values(resourcesOf(orch, "AWS::IAM::Role")).map((r) => r.Properties as any);
    expect(roles.find((r) => r.RoleName === "hamh-direct-run-ended").AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "states.amazonaws.com" }, Action: "sts:AssumeRole" }]);
    expect(roles.find((r) => r.RoleName === "hamh-direct-run-ended-events").AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "events.amazonaws.com" }, Action: "sts:AssumeRole" }]);
  });
});

describe("the run-session role is least privilege", () => {
  test("exactly these statements", () => {
    expect(sids(runSession).sort()).toEqual([
      "AgentOutputs", "DataKeyThroughServices", "Evidence", "HarnessTable", "InvokeUtilityModel", "InvokeWorkers", "ListForAbsentKeys", "MintWorkerSessions", "OwnLogs",
      "PackageRead", "PackageToken", "PackageTokenBearer", "PinnedAgentOutputs", "ResearchPolicyRow", "RunCreatedEvent", "RunInputs", "RunMessages", "RuntimeArn",
      "SessionNonce", "SessionNonceKey", "SliceRecords", "TagVocabulary", "TestReportEvidence", "Tracing", "TurnTokens",
    ]);
  });

  // run-session writes the run's research policy row when the run starts (a PutItem of run#<run>/cfg into the research
  // table, by naming convention). Nothing else of that table: no read, no update, no delete, and the harness table grant is unchanged.
  test("the research policy row: PutItem on the research table, partition run# only", () => {
    const x = byId(runSession, "ResearchPolicyRow");
    expect(x.Effect).toBe("Allow");
    expect(actions(x)).toEqual(["dynamodb:PutItem"]);
    expect(resources(x)).toEqual([`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-research`]);
    expect(x.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["run#*"] } });
    const f = Object.values(resourcesOf(orch, "AWS::Lambda::Function"))[0].Properties as any;
    expect(f.Environment.Variables.RESEARCH_TABLE_NAME).toBe("hamh-direct-research");
    // The default research provider is auto: run-session writes the same word as research-api reads.
    expect(f.Environment.Variables.RESEARCH_PROVIDER).toBe("auto");
  });

  test("DynamoDB: get, put and update on the one table, and queries of the event partition, the run partitions and the tag attributes of the owner partitions only", () => {
    const t = byId(runSession, "HarnessTable");
    expect(actions(t).sort()).toEqual(["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem"]);
    expect(resources(t)).toEqual([`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`]);
    const q = byId(runSession, "RunCreatedEvent");
    expect(actions(q)).toEqual(["dynamodb:Query"]);
    expect(q.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["EVT#*"] } });
    // finalize settles a run's messages from the message items, which it finds with one query of the run's own partition
    const m = byId(runSession, "RunMessages");
    expect(actions(m)).toEqual(["dynamodb:Query"]);
    expect(resources(m)).toEqual([`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`]);
    expect(m.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["RUN#*"] } });
    // The tag branch reads the maintainers' newest search items for its vocabulary: a Query of the owner partitions that may read the tag
    // attributes (and the two key attributes) and nothing else, with the projection required (`Select`). It is the third query grant and the last.
    const v = byId(runSession, "TagVocabulary");
    expect(actions(v)).toEqual(["dynamodb:Query"]);
    expect(resources(v)).toEqual([`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`]);
    expect(v.Condition).toEqual({
      "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["OWNER#*"] },
      "ForAllValues:StringEquals": { "dynamodb:Attributes": ["PK", "SK", "runTags", "runTagsUser", "runTagsOff"] },
      StringEquals: { "dynamodb:Select": "SPECIFIC_ATTRIBUTES" },
    });
    expect(runSession.filter((x) => actions(x).includes("dynamodb:Query")).map((x) => x.Sid).sort()).toEqual(["RunCreatedEvent", "RunMessages", "TagVocabulary"]);
    for (const x of runSession) expect(actions(x)).not.toContain("dynamodb:DeleteItem");
  });

  // plan-delivery reads snapshot.json and checks snapshot.bundle exists before it names the snapshot as the held head.
  test("S3: the snapshot pair and the stop digest the direct agent writes are readable, another agent's outputs are not", () => {
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const pattern = resources(byId(runSession, "AgentOutputs"))[0];
    const matches = (value: string) => new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`).test(value);
    const run = "01M4ZX2QEMB4JFCA411Q5169AY";
    for (const name of ["snapshot.json", "snapshot.bundle", "summary.json", "bundle", "stop-digest-1.json", "fresh-session-2.md"]) {
      expect(matches(`${bucket}/runs/${run}/agents/direct/turns/1/${name}`), name).toBe(true);
    }
    expect(matches(`${bucket}/runs/${run}/agents/merge/turns/1/snapshot.bundle`)).toBe(false);
  });

  // plan-delivery reads the version agentd recorded for its bundle, summary.json and test-report.json. GetObjectVersion is its own action,
  // and it stays on those three keys of the direct agent: the snapshot pair, the stop digests and every other key are read at the current version only.
  test("b759: GetObjectVersion on the three end-of-run keys of the direct agent, and nowhere else", () => {
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const stmt = byId(runSession, "PinnedAgentOutputs");
    expect(stmt.Effect).toBe("Allow");
    expect(actions(stmt)).toEqual(["s3:GetObjectVersion"]);
    expect(resources(stmt).sort()).toEqual([
      `${bucket}/runs/*/agents/direct/turns/*/bundle`,
      `${bucket}/runs/*/agents/direct/turns/*/summary.json`,
      `${bucket}/runs/*/agents/direct/turns/*/test-report.json`,
    ]);
    expect(stmt.Condition).toBeUndefined();
    const withVersion = runSession.filter((x) => actions(x).includes("s3:GetObjectVersion"));
    expect(withVersion.map((x) => x.Sid)).toEqual(["PinnedAgentOutputs"]);
    // No delete of an object or a version, and no listing of versions.
    for (const x of runSession) {
      expect(actions(x).filter((a) => /^s3:(Delete|ListBucketVersions|PutBucketVersioning)/.test(a)), x.Sid).toEqual([]);
    }
  });

  test("S3: the role writes and reads the checked test report in the evidence, at that one key and nothing wider", () => {
    // plan-delivery copies the worker's report to runs/<run>/evidence/test-report.json (create-only) and the packager zips it into evidence/.
    // Without this grant the write is refused in a deployed stage and the zip lacks the file EVIDENCE.md names; the in-memory test stores
    // never refuse.
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const stmt = byId(runSession, "TestReportEvidence");
    expect(stmt.Effect).toBe("Allow");
    expect(resources(stmt)).toEqual([`${bucket}/runs/*/evidence/test-report.json`]);
    expect(actions(stmt).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    expect(stmt.Condition).toBeUndefined();
    // The decision file keeps its own statement, and no evidence wildcard appeared.
    expect(resources(byId(runSession, "Evidence"))).toEqual([`${bucket}/runs/*/evidence/direct.json`]);
    for (const x of runSession) {
      for (const r of resources(x).filter((v) => typeof v === "string")) {
        expect(r, x.Sid).not.toMatch(/\/evidence\/\*$/);
      }
    }
  });

  test("S3: reads of the run's request and the agent's outputs, one evidence key, and a bucket list for absent keys", () => {
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    expect(resources(byId(runSession, "RunInputs"))).toEqual([`${bucket}/runs/*/input/request.json`]);
    expect(actions(byId(runSession, "RunInputs"))).toEqual(["s3:GetObject"]);
    expect(resources(byId(runSession, "AgentOutputs"))).toEqual([`${bucket}/runs/*/agents/direct/turns/*`]);
    expect(actions(byId(runSession, "AgentOutputs"))).toEqual(["s3:GetObject"]);
    expect(resources(byId(runSession, "Evidence"))).toEqual([`${bucket}/runs/*/evidence/direct.json`]);
    expect(actions(byId(runSession, "Evidence")).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    // The session nonce (review S1): the run's refresh nonce, KMS-sealed, written once so a retried start sends the nonce the
    // running agentd holds; the key is outside agents/direct/turns/, which the microVM writes.
    expect(resources(byId(runSession, "SessionNonce"))).toEqual([`${bucket}/runs/*/agents/direct/nonce.enc`]);
    expect(actions(byId(runSession, "SessionNonce")).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    // The service's own records of a run's slices, outside agents/direct/ (which the microVM writes) and with no delete.
    expect(resources(byId(runSession, "SliceRecords"))).toEqual([`${bucket}/runs/*/slices/*`]);
    expect(actions(byId(runSession, "SliceRecords")).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    expect(byId(runSession, "SliceRecords").Condition).toBeUndefined();
    expect(actions(byId(runSession, "ListForAbsentKeys"))).toEqual(["s3:ListBucket"]);
    expect(resources(byId(runSession, "ListForAbsentKeys"))).toEqual([bucket]);
    for (const x of runSession) {
      expect(actions(x)).not.toContain("s3:DeleteObject");
      expect(actions(x)).not.toContain("s3:*");
    }
  });

  test("KMS: decrypt and data keys through DynamoDB and S3 only; direct encrypt under a turn-shaped context and decrypt for the nonce alone", () => {
    const k = byId(runSession, "DataKeyThroughServices");
    expect(actions(k).sort()).toEqual(["kms:Decrypt", "kms:DescribeKey", "kms:GenerateDataKey"]);
    expect(k.Condition.StringEquals["kms:ViaService"]).toEqual([`dynamodb.${REGION}.amazonaws.com`, `s3.${REGION}.amazonaws.com`]);
    const e = byId(runSession, "TurnTokens");
    expect(actions(e)).toEqual(["kms:Encrypt"]);
    expect(e.Condition).toEqual({ "ForAllValues:StringEquals": { "kms:EncryptionContextKeys": ["run", "agent", "turn"] } });
    // The session nonce (review S1) is read back with a direct Decrypt, allowed for the nonce's own context alone: a `ForAllValues` condition
    // passes a request with no context at all, so the statement also requires the agent word `direct-nonce`.
    const n = byId(runSession, "SessionNonceKey");
    expect(actions(n)).toEqual(["kms:Decrypt"]);
    expect(n.Condition).toEqual({
      StringEquals: { "kms:EncryptionContext:agent": "direct-nonce" },
      "ForAllValues:StringEquals": { "kms:EncryptionContextKeys": ["run", "agent", "turn"] },
    });
    expect(flat(n.Resource)).toBe(flat(k.Resource));
    expect(flat(e.Resource)).toBe(flat(k.Resource));
  });

  test("it may assume the worker-session role and invoke only the exact worker runtime and its endpoints", () => {
    const m = byId(runSession, "MintWorkerSessions");
    expect(actions(m).sort()).toEqual(["sts:AssumeRole", "sts:SetSourceIdentity", "sts:TagSession"]);
    expect(resources(m)).toEqual([`arn:aws:iam::${ACCOUNT}:role/hamh-direct-harness-worker-session`]);
    const w = byId(runSession, "InvokeWorkers");
    expect(actions(w).sort()).toEqual([
      "bedrock-agentcore:GetAgentRuntimeEndpoint", "bedrock-agentcore:InvokeAgentRuntime", "bedrock-agentcore:StopRuntimeSession"]);
    const rs = resources(w);
    expect(rs).toHaveLength(3);
    const text = flat(rs);
    expect(text).toContain("runtime-endpoint/live");
    expect(text).toContain("runtime-endpoint/v*");
    expect(text).not.toContain("runtime/*");
    expect(text).not.toContain("InvokeAgentRuntimeCommand");
    expect(resources(byId(runSession, "RuntimeArn"))).toEqual([
      `arn:aws:ssm:${REGION}:${ACCOUNT}:parameter/hamh/direct/worker-python-runtime-arn`]);
    // the runtime ARN is the runtimes stack's own
    const rt = Object.values(runtimes.Resources as Record<string, any>).find((r) => r.Type === "AWS::BedrockAgentCore::Runtime");
    expect(rt.Properties.AgentRuntimeName).toBe("hamh_direct_harness_worker_python");
  });

  test("the package token is minted for CodeArtifact only, from the stage's domain and repository", () => {
    const b = byId(runSession, "PackageTokenBearer");
    expect(b.Condition).toEqual({ StringEquals: { "sts:AWSServiceName": "codeartifact.amazonaws.com" } });
    expect(actions(byId(runSession, "PackageToken"))).toEqual(["codeartifact:GetAuthorizationToken"]);
    expect(flat(resources(byId(runSession, "PackageToken")))).toContain("domain/");
    expect(actions(byId(runSession, "PackageRead"))).toEqual(["codeartifact:ReadFromRepository"]);
    expect(flat(resources(byId(runSession, "PackageRead")))).toContain("repository/");
  });

  test("it holds no CodeBuild, CodeCommit, Step Functions, Lambda, IAM-user or wildcard-service grant, and its one Bedrock grant is the utility model's", () => {
    for (const x of runSession) {
      for (const a of actions(x)) {
        expect(a, `${x.Sid}: ${a}`).not.toMatch(/^(codebuild:|codecommit:|states:|lambda:|iam:|ec2:|secretsmanager:|ssm:(Put|Delete)|sqs:|sns:)/);
        expect(a, `${x.Sid}: ${a}`).not.toMatch(/:\*$/);
        expect(a).not.toBe("*");
        // InvokeUtilityModel (bedrock:InvokeModel on the utility model's ARNs) is the only statement that names a bedrock: action.
        if (x.Sid !== "InvokeUtilityModel") expect(a, `${x.Sid}: ${a}`).not.toMatch(/^bedrock:/);
      }
    }
    expect(actions(byId(runSession, "InvokeUtilityModel"))).toEqual(["bedrock:InvokeModel"]);
  });

  test("no statement but the logging, tracing and bearer ones has a wildcard resource", () => {
    for (const x of runSession) {
      if (["Tracing", "PackageTokenBearer"].includes(x.Sid)) continue;
      expect(resources(x), x.Sid).not.toContain("*");
    }
    expect(byId(runSession, "Tracing").Resource).toBe("*");
  });

  test("every Allow with a wildcard resource is a service that offers no narrower one", () => {
    const wild = runSession.filter((x) => x.Effect === "Allow" && resources(x).includes("*")).map((x) => x.Sid).sort();
    expect(wild).toEqual(["PackageTokenBearer", "Tracing"]);
  });
});

describe("who may mint worker sessions", () => {
  test("the worker-session role trusts Admin, worker-api and run-session, and not a dispatcher or an evaluator that does not exist", () => {
    const roles = Object.values((security.toJSON().Resources ?? {}) as Record<string, any>).filter((r) => r.Type === "AWS::IAM::Role");
    const ws = roles.find((r) => r.Properties.RoleName === "hamh-direct-harness-worker-session")!;
    const minters = ws.Properties.AssumeRolePolicyDocument.Statement[0].Condition.ArnEquals["aws:PrincipalArn"];
    expect(minters).toEqual([
      `arn:aws:iam::${ACCOUNT}:role/Admin`, `arn:aws:iam::${ACCOUNT}:role/hamh-direct-worker-api`, `arn:aws:iam::${ACCOUNT}:role/hamh-direct-run-session`]);
    expect(ws.Properties.AssumeRolePolicyDocument.Statement[1].Condition.ArnEquals["aws:PrincipalArn"]).toEqual(minters);
    const names = roles.map((r) => r.Properties.RoleName);
    expect(names).not.toContain("hamh-direct-harness-verifier-session");
    expect(names).not.toContain("hamh-direct-harness-judge-session");
  });

  test("the session has no merge-train read and keeps its guards", () => {
    const roles = Object.values((security.toJSON().Resources ?? {}) as Record<string, any>).filter((r) => r.Type === "AWS::IAM::Role");
    const ws = roles.find((r) => r.Properties.RoleName === "hamh-direct-harness-worker-session")!;
    const s: Stmt[] = ws.Properties.Policies[0].PolicyDocument.Statement;
    expect(sids(s)).not.toContain("ReadRunMergeBundles");
    expect(sids(s)).not.toContain("DrillFaultPlan");
    for (const guard of ["DenyWithoutRunTag", "DenyWithoutAgentTag", "DenyGitHostAndPrCheck", "NoPackageTokens", "DenyOutsideWorkerNetwork"]) {
      expect(sids(s)).toContain(guard);
    }
  });
});

describe("the callers around it", () => {
  test("worker-api completes and heartbeats only the run machine's tasks", () => {
    const s = rolePolicy(control, "hamh-direct-worker-api");
    const cb = byId(s, "TurnCallbacks");
    expect(actions(cb).sort()).toEqual(["states:SendTaskHeartbeat", "states:SendTaskSuccess"]);
    expect(resources(cb)).toEqual([`arn:aws:states:${REGION}:${ACCOUNT}:stateMachine:hamh-direct-run`]);
  });

  test("control-api starts and stops the run machine's executions and lists no goal-loop machine", () => {
    const s = rolePolicy(control, "hamh-direct-control-api");
    expect(resources(byId(s, "StartRun"))).toEqual([`arn:aws:states:${REGION}:${ACCOUNT}:stateMachine:hamh-direct-run`]);
    expect(resources(byId(s, "CancelRun"))).toEqual([`arn:aws:states:${REGION}:${ACCOUNT}:execution:hamh-direct-run:*`]);
    expect(sids(s)).not.toContain("CancelTaskLoops");
    expect(flat(s)).not.toContain("worker-goal-loop");
    const env = (Object.values(resourcesOf(control, "AWS::Lambda::Function")).find((r) => (r.Properties as any).FunctionName === "hamh-direct-control-api")!
      .Properties as any).Environment.Variables;
    expect(env.RUN_STATE_MACHINE_ARN).toBe(`arn:aws:states:${REGION}:${ACCOUNT}:stateMachine:hamh-direct-run`);
    expect(env.DEPLOYMENT_PARAMETER_PREFIX).toBe("/hamh/direct");
    expect(env.ROUTE_LADDER).toBe("off");
  });
});

describe("runs that outlive one microVM session", () => {
  test("the machine lives as long as the longest total a run may ask for plus ten hours for the stage preparation and the delivery", async () => {
    const mod = await import("../lib/stacks/orchestration-direct.ts");
    expect(mod.RUN_TIMEOUT_S).toBe(30 * 24 * 3600 + 36_000);
  });

  test("the slice deadline of a live proof is a validated setting, absent in production", async () => {
    const cfg = await import("../lib/config.ts");
    expect(cfg.sliceDeadlineS({})).toBeUndefined();
    expect(cfg.sliceDeadlineS({ HAMH_SLICE_DEADLINE_S: "  " })).toBeUndefined();
    expect(cfg.sliceDeadlineS({ HAMH_SLICE_DEADLINE_S: "600" })).toBe(600);
    for (const bad of ["59", "28801", "ten minutes", "6e2", "-600", "600.5"]) {
      expect(() => cfg.sliceDeadlineS({ HAMH_SLICE_DEADLINE_S: bad }), bad).toThrow(/HAMH_SLICE_DEADLINE_S/);
    }
  });
});
