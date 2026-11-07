import { createHash } from "node:crypto";
import { describe, expect, test } from "vitest";
import { resourcesOf, synth, template } from "./helpers.ts";
import { ACCOUNT, names, REGION, stageConfig } from "../lib/config.ts";

// The private /internal API: IAM auth, worker-api Lambda.
const ROUTES = ["creds", "heartbeat", "turn-complete", "events", "message-session", "message-claim", "message-seal", "message-outcome"];
// The research route adds a `research` resource and five more POST routes below it (infra/test/research.test.ts).
const RESEARCH_ROUTES = ["search", "fetch", "repo", "job", "limits"];

describe("control stack", () => {
  const t = template("direct", "control");

  const apiIds = () => {
    const ids: Record<string, string> = {};
    for (const [id, r] of Object.entries(resourcesOf(t, "AWS::ApiGateway::RestApi"))) ids[(r.Properties as any).Name] = id;
    return ids;
  };
  const onApi = (type: string, name: string) =>
    Object.values(resourcesOf(t, type)).map((r) => r.Properties as any).filter((p) => p.RestApiId?.Ref === apiIds()[name]);

  test("two REST APIs: the private /internal one and the public /v1 one", () => {
    expect(Object.keys(apiIds()).sort()).toEqual(["hamh-direct-internal", "hamh-direct-v1"]);
  });

  test("a private REST API reachable only through the worker-VPC execute-api endpoint", () => {
    const apis = Object.values(resourcesOf(t, "AWS::ApiGateway::RestApi")).map((r) => r.Properties as any)
      .filter((a) => a.Name === "hamh-direct-internal");
    expect(apis).toHaveLength(1);
    const api = apis[0];
    expect(api.Name).toBe("hamh-direct-internal");
    expect(api.EndpointConfiguration.Types).toEqual(["PRIVATE"]);
    expect(api.EndpointConfiguration.VpcEndpointIds).toHaveLength(1);
    const stmts = api.Policy.Statement;
    const allow = stmts.find((s: any) => s.Effect === "Allow");
    const deny = stmts.find((s: any) => s.Effect === "Deny");
    expect(allow.Action).toBe("execute-api:Invoke");
    expect(JSON.stringify(allow.Condition)).toContain("aws:SourceVpce");
    expect(JSON.stringify(allow.Condition)).toContain("aws:PrincipalArn");
    expect(JSON.stringify(allow.Condition)).toContain("harness-worker-session");
    expect(Object.keys(deny.Condition)).toEqual(["StringNotEquals"]);
    expect(JSON.stringify(deny.Condition)).toContain("aws:SourceVpce");
  });

  test("thirteen POST routes under /internal with IAM auth and Lambda proxy integration", () => {
    const resources = onApi("AWS::ApiGateway::Resource", "hamh-direct-internal").map((p) => p.PathPart);
    expect(resources.sort()).toEqual(["internal", ...ROUTES, "research", ...RESEARCH_ROUTES].sort());
    const methods = onApi("AWS::ApiGateway::Method", "hamh-direct-internal");
    expect(methods).toHaveLength(ROUTES.length + RESEARCH_ROUTES.length);
    for (const m of methods) {
      expect(m.HttpMethod).toBe("POST");
      expect(m.AuthorizationType).toBe("AWS_IAM");
      expect(m.Integration.Type).toBe("AWS_PROXY");
      expect(m.Integration.IntegrationHttpMethod).toBe("POST");
    }
  });

  test("a deployment stage named after the stage with tracing and no account-level logging", () => {
    // API Gateway access and execution logs need the account-wide CloudWatch role, an account
    // setting forbids changing; worker-api writes its own access line instead.
    for (const name of ["hamh-direct-internal", "hamh-direct-v1"]) {
      const stages = onApi("AWS::ApiGateway::Stage", name);
      expect(stages, name).toHaveLength(1);
      expect(stages[0].StageName).toBe("direct");
      expect(stages[0].TracingEnabled).toBe(true);
      expect(stages[0].AccessLogSetting).toBeUndefined();
      expect(stages[0].MethodSettings).toBeUndefined();
      expect(onApi("AWS::ApiGateway::Deployment", name), name).toHaveLength(1);
    }
    t.resourceCountIs("AWS::ApiGateway::Account", 0);
  });

  // Access Analyzer ValidatePolicy rejects the `execute-api:/*` shorthand as
  // MISSING_ARN_FIELD (ERROR). A resource policy only governs its own API, so the API ID wildcard of
  // the full ARN matches exactly what the shorthand did: every stage, method and path of this API.
  test("both resource policies name their API by the full execute-api ARN", () => {
    for (const name of ["hamh-direct-internal", "hamh-direct-v1"]) {
      const api = Object.values(resourcesOf(t, "AWS::ApiGateway::RestApi")).map((r) => r.Properties as any).find((a) => a.Name === name);
      expect(api.Policy.Statement.length, name).toBe(2);
      for (const st of api.Policy.Statement) {
        expect(st.Resource, `${name} ${st.Sid}`).toBe(`arn:aws:execute-api:${REGION}:${ACCOUNT}:*/*`);
      }
    }
    expect(JSON.stringify(t.toJSON())).not.toContain("execute-api:/");
  });

  // API Gateway applies a changed resource policy only when the API is deployed again, so each
  // deployment's logical ID carries a hash of its API's policy (and routes): a policy edit makes
  // CloudFormation create a new deployment and repoint the stage.
  test("each API's deployment logical ID changes with its resource policy", () => {
    const hash = (x: unknown) => createHash("sha256").update(JSON.stringify(x)).digest("hex").slice(0, 8);
    const deploymentOf = (tpl: ReturnType<typeof template>, name: string) => {
      const apiId = Object.entries(resourcesOf(tpl, "AWS::ApiGateway::RestApi")).find(([, r]) => (r.Properties as any).Name === name)![0];
      const ids = Object.entries(resourcesOf(tpl, "AWS::ApiGateway::Deployment"))
        .filter(([, r]) => (r.Properties as any).RestApiId?.Ref === apiId).map(([id]) => id);
      expect(ids, name).toHaveLength(1);
      return { apiId, id: ids[0], policy: (resourcesOf(tpl, "AWS::ApiGateway::RestApi")[apiId].Properties as any).Policy };
    };
    const v1 = deploymentOf(t, "hamh-direct-v1");
    expect(v1.id).toBe(`V1Deployment${hash({ routes: ["ANY /v1/{proxy+}"], policy: v1.policy })}`);
    const internal = deploymentOf(t, "hamh-direct-internal");
    expect(internal.id).toMatch(/^InternalDeployment[0-9a-f]{8}$/);
    // The stages point at those deployments.
    for (const [name, d] of [["hamh-direct-v1", v1], ["hamh-direct-internal", internal]] as const) {
      const stages = onApi("AWS::ApiGateway::Stage", name);
      expect(stages[0].DeploymentId, name).toEqual({ Ref: d.id });
    }
  });

  test("worker-api: arm64 provided.al2023 Lambda with the role the worker session trusts", () => {
    const fns = Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as any);
    expect(fns.map((x) => x.FunctionName).sort()).toEqual(["hamh-direct-control-api", "hamh-direct-research-api", "hamh-direct-research-job", "hamh-direct-worker-api"]);
    const f = fns.find((x) => x.FunctionName === "hamh-direct-worker-api");
    expect(f.FunctionName).toBe("hamh-direct-worker-api");
    expect(f.Runtime).toBe("provided.al2023");
    expect(f.Architectures).toEqual(["arm64"]);
    expect(f.Handler).toBe("bootstrap");
    expect(Object.keys(f.Environment.Variables).sort()).toEqual(["HARNESS_BUCKET", "TABLE_NAME", "WORKER_SESSION_ROLE_ARN", "WORKER_SESSION_ROLE_NAME"]);
    expect(f.Environment.Variables.TABLE_NAME).toBe("hamh-direct-harness");
    // worker-api compares a turn-complete's bundle with the harness bucket's turn-bundle key (sweep SW-67).
    expect(f.Environment.Variables.HARNESS_BUCKET).toBe(`hamh-direct-harness-${ACCOUNT}`);
    expect(f.Environment.Variables.WORKER_SESSION_ROLE_NAME).toBe("hamh-direct-harness-worker-session");
    const roles = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any);
    const role = roles.find((r) => r.RoleName === "hamh-direct-worker-api");
    expect(role).toBeDefined();
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const allows = stmts.flatMap((s: any) => (Array.isArray(s.Action) ? s.Action : [s.Action]));
    // Query: /internal/events reads the agent's CTRL# items.
    for (const a of ["dynamodb:GetItem", "dynamodb:UpdateItem", "dynamodb:BatchWriteItem", "dynamodb:Query", "sts:AssumeRole",
      "sts:TagSession", "sts:SetSourceIdentity"]) {
      expect(allows, a).toContain(a);
    }
    for (const a of allows) expect(a).not.toMatch(/^(dynamodb|sts|iam|s3):\*$|^\*$/);
    const text = JSON.stringify(stmts);
    expect(text).toContain("table/hamh-direct-harness");
    expect(text).toContain("role/hamh-direct-harness-worker-session");
    expect(text).toContain("kms:ViaService");
    // Dispatched turns: the token decrypts only under a turn's context, and only this state
    // machine's tasks can be completed or kept alive.
    for (const a of ["states:SendTaskSuccess", "states:SendTaskHeartbeat", "kms:Decrypt"]) expect(allows, a).toContain(a);
    // The run machine dispatches the turn with its own task token, and it is the only machine of the stage.
    const callbacks = stmts.find((x: any) => x.Sid === "TurnCallbacks");
    expect(callbacks.Resource).toEqual([`arn:aws:states:${REGION}:${ACCOUNT}:stateMachine:hamh-direct-run`]);
    expect(text).not.toContain("worker-goal-loop");
    expect(text).not.toContain("hidden-admission");
    expect(text).toContain("kms:EncryptionContextKeys");
  });

  // Message control. The ledger writes with UpdateItem (worker-api, run-session) and PutItem (control-api creates a
  // message and the owner's counter): the agent's side can change a message's state and cannot make a message, and no role can delete one.
  test("message control: worker-api cannot create or delete an item, control-api writes messages only where it must", () => {
    const roles = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any);
    const statements = (name: string) =>
      roles.find((r) => r.RoleName === name).Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const actions = (s: any) => (Array.isArray(s.Action) ? s.Action : [s.Action]);
    const worker = statements("hamh-direct-worker-api");
    const workerActions = worker.flatMap(actions);
    for (const a of ["dynamodb:GetItem", "dynamodb:UpdateItem", "dynamodb:Query"]) expect(workerActions, a).toContain(a);
    // PutItem makes an item and DeleteItem removes one. worker-api needs neither: the first write of the control item is
    // an UpdateItem guarded by attribute_not_exists(PK).
    expect(workerActions).not.toContain("dynamodb:PutItem");
    expect(workerActions).not.toContain("dynamodb:DeleteItem");
    const control = statements("hamh-direct-control-api");
    const writes = (action: string) => control.filter((s: any) => actions(s).includes(action));
    // A message and the owner's counter are made with PutItem.
    expect(writes("dynamodb:PutItem").length).toBeGreaterThan(0);
    // Every UpdateItem (a withdrawal, a confirmation's superseded original, the control item) is on a named partition; DeleteItem only
    // on the allowlist's.
    for (const s of writes("dynamodb:UpdateItem")) {
      const keys: string[] = s.Condition?.["ForAllValues:StringLike"]?.["dynamodb:LeadingKeys"] ?? [];
      expect(keys.length, s.Sid).toBeGreaterThan(0);
      // OWNER# is the run index; the messages need only RUN#.
      for (const k of keys) expect(["RUN#", "OWNER#", "REPOALLOW#", "HEALTH#"].some((p) => k.startsWith(p)), `${s.Sid}: ${k}`).toBe(true);
    }
    for (const s of writes("dynamodb:DeleteItem")) {
      expect(s.Condition["ForAllValues:StringLike"]["dynamodb:LeadingKeys"], s.Sid).toEqual(["REPOALLOW#*"]);
    }
    // The message items and their control item are in the run's partition, which the RunControl statement covers.
    const runControl = control.find((s: any) => s.Sid === "RunControl");
    expect(runControl.Condition["ForAllValues:StringLike"]["dynamodb:LeadingKeys"]).toEqual(["RUN#*"]);
  });

  test("API Gateway may invoke each function only from its own API", () => {
    const perms = Object.values(resourcesOf(t, "AWS::Lambda::Permission")).map((r) => r.Properties as any);
    // worker-api and control-api, and research-api for the five research routes (infra/test/research.test.ts).
    expect(perms).toHaveLength(3);
    const ids = apiIds();
    for (const p of perms) {
      expect(p.Principal).toBe("apigateway.amazonaws.com");
      expect(JSON.stringify(p.SourceArn)).toContain("execute-api");
    }
    const byApi = (name: string) => perms.filter((p) => JSON.stringify(p.SourceArn).includes(ids[name]));
    expect(byApi("hamh-direct-internal")).toHaveLength(2);
    expect(byApi("hamh-direct-internal").map((p) => JSON.stringify(p.SourceArn).match(/\/POST\/internal\/(research\/)?\*/)?.[0]).sort())
      .toEqual(["/POST/internal/*", "/POST/internal/research/*"]);
    expect(byApi("hamh-direct-v1")).toHaveLength(1);
    expect(JSON.stringify(byApi("hamh-direct-v1")[0].SourceArn)).toContain("/*/v1/*");
  });

  // The human API. IAM auth on every method, callers from
  // this account only, and an explicit Deny for harness-worker-session, whose sessions run agents.
  test("the public /v1 API: regional, IAM only, this account, never a worker session", () => {
    const api = Object.values(resourcesOf(t, "AWS::ApiGateway::RestApi")).map((r) => r.Properties as any)
      .find((a) => a.Name === "hamh-direct-v1");
    expect(api.EndpointConfiguration.Types).toEqual(["REGIONAL"]);
    const stmts = api.Policy.Statement;
    const allow = stmts.find((x: any) => x.Effect === "Allow");
    expect(allow.Action).toBe("execute-api:Invoke");
    expect(allow.Condition).toEqual({ StringEquals: { "aws:PrincipalAccount": `${ACCOUNT}` } });
    const deny = stmts.find((x: any) => x.Effect === "Deny");
    expect(deny.Action).toBe("execute-api:Invoke");
    expect(deny.Condition).toEqual({ ArnEquals: { "aws:PrincipalArn": `arn:aws:iam::${ACCOUNT}:role/hamh-direct-harness-worker-session` } });
    const resources = onApi("AWS::ApiGateway::Resource", "hamh-direct-v1").map((p) => p.PathPart).sort();
    expect(resources).toEqual(["v1", "{proxy+}"].sort());
    const methods = onApi("AWS::ApiGateway::Method", "hamh-direct-v1");
    expect(methods).toHaveLength(1);
    expect(methods[0].HttpMethod).toBe("ANY");
    expect(methods[0].AuthorizationType).toBe("AWS_IAM");
    expect(methods[0].Integration.Type).toBe("AWS_PROXY");
    const fnId = Object.entries(resourcesOf(t, "AWS::Lambda::Function"))
      .find(([, r]) => (r.Properties as any).FunctionName === "hamh-direct-control-api")?.[0];
    expect(JSON.stringify(methods[0].Integration.Uri)).toContain(`"Fn::GetAtt":["${fnId}","Arn"]`);
  });

  test("control-api: arm64 Lambda that starts the run execution and writes only run inputs", () => {
    const f = Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as any)
      .find((x) => x.FunctionName === "hamh-direct-control-api");
    expect(f.Runtime).toBe("provided.al2023");
    expect(f.Architectures).toEqual(["arm64"]);
    expect(f.Handler).toBe("bootstrap");
    const env = f.Environment.Variables;
    expect(Object.keys(env).sort()).toEqual(["ACCOUNT_ID", "BUCKET_NAME", "DEPLOYMENT_PARAMETER_PREFIX", "ENGINE",
      "ENGINE_POLICY_VERSION", "ENGINE_ROUTING", "ENGINE_SET", "HAMH_REPO_NAMES", "METRICS_NAMESPACE", "OPERATOR_PRINCIPALS", "ROUTE_LADDER",
      "ROUTE_LADDER_KEY", "RUN_STATE_MACHINE_ARN", "TABLE_NAME"]);
    // The direct stage routes every session to the run engine (no engine-policy route); the ladder switch is off.
    expect(env.ENGINE_ROUTING).toBe("run");
    expect(env.ROUTE_LADDER).toBe("off");
    expect(env.METRICS_NAMESPACE).toBe("HAMH/direct");
    // The repository names an operator may allow: the default rule when the account file names none (test/repo-names.test.ts).
    expect(env.HAMH_REPO_NAMES).toBe("hamh-e2e-*");
    // ham-admin: only the Admin role reaches /v1/admin/*.
    expect(env.OPERATOR_PRINCIPALS).toBe(`arn:aws:sts::${ACCOUNT}:assumed-role/Admin/*`);
    // Each new Run item names the deployment it runs on, read fresh from SSM.
    expect(env.DEPLOYMENT_PARAMETER_PREFIX).toBe("/hamh/direct");
    // ENGINE is the fallback where no policy route covers a role and model: Cline, the release's
    // only engine, and ENGINE_SET is the engines StartRun admits (engine-not-deployed outside it).
    expect(env.ENGINE).toBe("cline");
    expect(env.ENGINE_SET).toBe("cline");
    expect(env.TABLE_NAME).toBe("hamh-direct-harness");
    expect(env.BUCKET_NAME).toBe(`hamh-direct-harness-${ACCOUNT}`);
    expect(env.ACCOUNT_ID).toBe(`${ACCOUNT}`);
    expect(env.RUN_STATE_MACHINE_ARN).toBe(`arn:aws:states:${REGION}:${ACCOUNT}:stateMachine:hamh-direct-run`);
    expect(env.ENGINE_POLICY_VERSION).toMatch(/^\d+$/);
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const actions = stmts.flatMap((x: any) => (Array.isArray(x.Action) ? x.Action : [x.Action]));
    for (const a of ["dynamodb:PutItem", "dynamodb:GetItem", "dynamodb:Query", "s3:PutObject", "states:StartExecution"]) {
      expect(actions, a).toContain(a);
    }
    for (const a of actions) expect(a).not.toMatch(/^(dynamodb|s3|states|kms|iam):\*$|^\*$/);
    const allow = stmts.find((x: any) => x.Sid === "RepositoryAllowlist");
    expect([allow.Action].flat().sort()).toEqual(["dynamodb:BatchGetItem", "dynamodb:DeleteItem", "dynamodb:UpdateItem"]);
    expect(allow.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["REPOALLOW#*"] } });
    const text = JSON.stringify(stmts);
    expect(text).toContain("table/hamh-direct-harness/index/GSI1");
    expect(text).toContain(`arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/input/request.json`);
    expect(text).toContain("stateMachine:hamh-direct-run");
    // ham cancel: StopExecution on the run machine's executions only (`?discard=true`); no child execution exists to list or stop.
    const stop = stmts.filter((x: any) => [x.Action].flat().includes("states:StopExecution"));
    expect(stop).toHaveLength(1);
    expect(stop[0].Sid).toBe("CancelRun");
    expect([stop[0].Action].flat()).toEqual(["states:StopExecution"]);
    expect(stop[0].Resource).toEqual([`arn:aws:states:${REGION}:${ACCOUNT}:execution:hamh-direct-run:*`]);
    expect(stmts.filter((x: any) => [x.Action].flat().includes("states:ListExecutions"))).toEqual([]);
    // Run control updates only the fields it owns on run items (a whole-item Put erased run-tasks'
    // run machine on 2026-09-27); cancel clears the run's task goals the same way.
    const upd = stmts.find((x: any) => x.Sid === "RunControl");
    expect([upd.Action].flat()).toEqual(["dynamodb:UpdateItem"]);
    expect(upd.Resource).toBe(`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`);
    expect(upd.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["RUN#*"] } });
    expect(text).toContain("kms:ViaService");
    const ssm = stmts.filter((x: any) => JSON.stringify(x.Action).includes("ssm:"));
    // Two SSM statements: the deployment words every new Run item records, and the Instances runtime ARN, below.
    expect(ssm.map((x: any) => x.Sid).sort()).toEqual(["DeploymentParameters", "InstancesRuntimeArnParameter"]);
    const deployment = ssm.find((x: any) => x.Sid === "DeploymentParameters");
    expect(deployment.Action).toEqual(["ssm:GetParameters"]);
    expect(deployment.Resource).toEqual([
      `arn:aws:ssm:${REGION}:${ACCOUNT}:parameter/hamh/direct/deployment-digest`,
      `arn:aws:ssm:${REGION}:${ACCOUNT}:parameter/hamh/direct/deployed-source-tree`,
    ]);
  });

  // A request that names `--compute instances` makes control-api ask whether the stage has an Instances worker runtime, by reading that runtime's ARN
  // parameter with GetParameter (singular; the deployment words above are read with GetParameters). An absent parameter answers ParameterNotFound, which
  // control-api turns into a 422 the user can act on; a role without this grant answers AccessDenied, which is a stage that is set up wrongly. So the grant is
  // there whether or not the stage has the Instances part: it does not depend on `HAMH_COMPUTE_INSTANCES` (the control template is the same for off and on).
  test("control-api may read the Instances runtime ARN parameter with GetParameter, that one parameter only, whatever the Instances setting", () => {
    for (const computeInstances of [undefined, "on"]) {
      const control = synth("direct", { ...(computeInstances ? { computeInstances } : {}) }).templates.get("hamh-direct-control")!;
      const role = Object.values(resourcesOf(control, "AWS::IAM::Role")).map((r) => r.Properties as any)
        .find((r) => r.RoleName === "hamh-direct-control-api");
      const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
      const grant = stmts.find((x: any) => x.Sid === "InstancesRuntimeArnParameter");
      expect(grant, String(computeInstances)).toEqual({
        Sid: "InstancesRuntimeArnParameter", Effect: "Allow", Action: ["ssm:GetParameter"],
        Resource: `arn:aws:ssm:${REGION}:${ACCOUNT}:parameter/hamh/direct/worker-python-instances-runtime-arn`,
      });
      // It is the parameter the runtimes stack writes and control-api reads (`{DEPLOYMENT_PARAMETER_PREFIX}/worker-python-instances-runtime-arn`).
      const fn = Object.values(resourcesOf(control, "AWS::Lambda::Function")).map((r) => r.Properties as any).find((f) => f.FunctionName === "hamh-direct-control-api");
      expect(`${fn.Environment.Variables.DEPLOYMENT_PARAMETER_PREFIX}/worker-python-instances-runtime-arn`).toBe(names.runtimeArnParameter(stageConfig("direct"), "worker-python-instances"));
    }
  });

  // A download URL carries control-api's permissions, so the role reads the deliverable and
  // evidence objects it presigns, and never the hidden acceptance tests (verifier only). The transcript route adds one more read,
  // the chunk objects of the direct agent's first turn, in a statement of its own.
  test("control-api reads only the deliverable and evidence prefixes, to presign them", () => {
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const reads = stmts.filter((x: any) => [x.Action].flat().includes("s3:GetObject") && x.Sid === "PresignArtifacts");
    expect(reads).toHaveLength(1);
    expect(reads[0].Sid).toBe("PresignArtifacts");
    expect(reads[0].Effect).toBe("Allow");
    expect([reads[0].Action].flat()).toEqual(["s3:GetObject"]);
    expect([reads[0].Resource].flat().sort()).toEqual([
      `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/deliverable/*`,
      `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/evidence/*`,
    ]);
    const text = JSON.stringify(stmts);
    expect(text).not.toContain("/hidden");
    expect(text).not.toMatch(new RegExp(`runs/\\*"|runs/\\*/\\*"|harness-${ACCOUNT}/\\*"`));
  });

  // The engine can write the same prefix, so nothing wider and nothing that writes.
  test("control-api reads transcript chunks and lists only inside the transcript prefix", () => {
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const read = stmts.find((x: any) => x.Sid === "ReadTranscriptChunks");
    expect(read).toEqual({
      Sid: "ReadTranscriptChunks", Effect: "Allow", Action: ["s3:GetObject"],
      Resource: `${bucket}/runs/*/agents/direct/turns/1/transcript/*`,
    });
    // The follow-up run's listing has its own test below; this one pins the transcript's.
    const list = stmts.filter((x: any) => [x.Action].flat().includes("s3:ListBucket") && x.Sid !== "ListDeliveredHead");
    expect(list).toEqual([{
      Sid: "ListTranscriptPrefix", Effect: "Allow", Action: ["s3:ListBucket"], Resource: bucket,
      Condition: { StringLike: { "s3:prefix": ["runs/*/agents/direct/turns/1/transcript/*"] } },
    }]);
    // The role holds no S3 write, no delete, no version read and no other bucket-wide grant.
    const s3 = stmts.filter((x: any) => JSON.stringify(x.Action ?? "").includes("s3:"));
    const actions = s3.flatMap((x: any) => [x.Action].flat()).sort();
    // The presign statement, this one, the snapshot download, the read of a stored request and the request upload; two
    // ListBucket, the transcript's and the follow-up run's.
    expect(actions).toEqual(["s3:GetObject", "s3:GetObject", "s3:GetObject", "s3:GetObject", "s3:GetObjectVersion", "s3:ListBucket", "s3:ListBucket", "s3:PutObject"]);
    expect(stmts.find((x: any) => [x.Action].flat().includes("s3:PutObject")).Resource).toBe(`${bucket}/runs/*/input/request.json`);
    // Version reads belong to the snapshot download alone; nothing else in S3 may delete, read versions, list versions or touch the bucket.
    for (const x of s3.filter((y: any) => y.Sid !== "SnapshotDownload")) {
      expect(JSON.stringify(x)).not.toMatch(/Delete|GetObjectVersion|ListBucketVersions|PutBucket|"s3:\*"/);
    }
  });

  // `ham continue` reads the first bytes of the source run's verified bundle. A GET of an absent key is 403 (not 404) for a role that may
  // not list the bucket, so a run that delivered nothing would look like an outage. control-api therefore asks for the key by a listing whose
  // prefix is the key itself, and the role may list only prefixes of that one key.
  test("control-api may list the bucket only by the exact key of a delivered head", () => {
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const list = stmts.find((x: any) => x.Sid === "ListDeliveredHead");
    expect(list, "the statement exists").toBeDefined();
    expect(list.Effect).toBe("Allow");
    expect([list.Action].flat()).toEqual(["s3:ListBucket"]);
    // The bucket itself (ListBucket is a bucket-level action), narrowed by the prefix the listing carries.
    expect(list.Resource).toBe(`arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`);
    expect(list.Condition).toEqual({ StringLike: { "s3:prefix": ["runs/*/deliverable/verified.bundle"] } });
    // The only other listing is the transcript route's (its own test above), and none reads the hidden tests.
    expect(stmts.filter((x: any) => [x.Action].flat().includes("s3:ListBucket")).map((x: any) => x.Sid).sort()).toEqual(["ListDeliveredHead", "ListTranscriptPrefix"]);
    expect(JSON.stringify(list)).not.toMatch(/hidden|agents/);
  });

  // `POST /v1/runs/{runId}/retry` starts a blocked run again with the request it was started with, so the role reads the stored request
  // object of a run (control-api checks that the caller owns the run before it reads). The role already writes this key; it reads that key and
  // no other new one, and the statement has no condition that a plain GET would not carry.
  test("control-api may read the stored request of a run, to start a blocked run again", () => {
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const read = stmts.find((x: any) => x.Sid === "ReadRunRequests");
    expect(read, "the statement exists").toBeDefined();
    expect(read).toEqual({
      Sid: "ReadRunRequests", Effect: "Allow", Action: ["s3:GetObject"], Resource: `${bucket}/runs/*/input/request.json`,
    });
    // It reads the key the role writes, and the write stays the one statement it was.
    const write = stmts.filter((x: any) => [x.Action].flat().includes("s3:PutObject"));
    expect(write.map((x: any) => x.Sid)).toEqual(["RunRequests"]);
    expect(write[0].Resource).toBe(read.Resource);
    // Nothing else of the input prefix and nothing of the hidden tests.
    expect(JSON.stringify(read)).not.toMatch(/hidden|input\/\*|agents|deliverable|evidence/);
  });

  // The download of the work in progress. The route reads the record and the first bytes of the bundle and presigns the
  // bundle, so the role may get these two objects of the direct agent and nothing else under agents/ (the transcript, the summary,
  // the stop digests, the nonce and the final bundle stay closed to it).
  test("control-api may get the snapshot bundle and its record of the direct agent, and nothing else under agents/", () => {
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    const reads = stmts.filter((x: any) => [x.Action].flat().some((a: string) => a.startsWith("s3:Get")));
    expect(reads.map((x: any) => x.Sid).sort()).toEqual(["PresignArtifacts", "ReadRunRequests", "ReadTranscriptChunks", "SnapshotDownload"]);
    const snap = reads.find((x: any) => x.Sid === "SnapshotDownload");
    expect(snap.Effect).toBe("Allow");
    // GetObjectVersion on the same two keys only: the URL is pinned to the version whose header the route checked.
    expect([snap.Action].flat().sort()).toEqual(["s3:GetObject", "s3:GetObjectVersion"]);
    expect([snap.Resource].flat().sort()).toEqual([
      `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/agents/direct/turns/?/snapshot.bundle`,
      `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/agents/direct/turns/?/snapshot.json`,
      `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/agents/direct/turns/??/snapshot.bundle`,
      `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}/runs/*/agents/direct/turns/??/snapshot.json`,
    ]);
    expect(snap.Condition).toBeUndefined();
    expect(stmts.filter((x: any) => [x.Action].flat().includes("s3:GetObjectVersion")).map((x: any) => x.Sid)).toEqual(["SnapshotDownload"]);
    // The only listings are the transcript route's (inside the transcript prefix) and the follow-up run's (the exact key of a delivered head); each has
    // its own test.
    expect(stmts.filter((x: any) => [x.Action].flat().includes("s3:ListBucket")).map((x: any) => x.Sid).sort()).toEqual(["ListDeliveredHead", "ListTranscriptPrefix"]);
    expect(JSON.stringify(snap)).not.toMatch(/agents\/\*|turns\/\*|\/hidden/);
  });

  // The run index lives in the harness table under OWNER#<hash of the caller>, written by control-api (the start and the
  // backfill, both an UpdateItem that keeps the fields already there) and updated by run-session at the end of a run (it holds a
  // table-wide UpdateItem already). Reads and the page join use the table-wide GetItem and Query. The statement set of control-api is
  // pinned: a new grant is a decision.
  test("the run index adds one DynamoDB grant: UpdateItem on the OWNER# partition only", () => {
    const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
      .find((r) => r.RoleName === "hamh-direct-control-api");
    const stmts = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
    expect(stmts.map((x: any) => x.Sid).sort()).toEqual([
      "CancelRun", "DataKeyThroughDynamoDbAndS3", "DeploymentParameters", "HarnessTable", "InstancesRuntimeArnParameter", "ListDeliveredHead", "ListTranscriptPrefix", "OwnLogs",
      "PresignArtifacts", "ReadRunRequests", "ReadTranscriptChunks", "RepositoryAllowlist", "RouteHealthRead", "RouteHealthReset", "RunControl", "RunIndex", "RunRequests",
      "RunsByOwner", "SnapshotDownload", "StartRun", "Tracing", "VerifyRouteLadder",
    ]);
    const index = stmts.find((x: any) => x.Sid === "RunIndex");
    expect(index.Effect).toBe("Allow");
    expect([index.Action].flat()).toEqual(["dynamodb:UpdateItem"]);
    expect(index.Resource).toBe(`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`);
    expect(index.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["OWNER#*"] } });
    const table = stmts.find((x: any) => x.Sid === "HarnessTable");
    // The page join (a Query of the owner partition), the backfill's reads and the run list's own reads use these three.
    expect([table.Action].flat().sort()).toEqual(["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:Query"]);
    // No batch read outside the two partitions that already had one, and no delete anywhere.
    const batch = stmts.filter((x: any) => [x.Action].flat().includes("dynamodb:BatchGetItem"));
    expect(batch.map((x: any) => x.Sid).sort()).toEqual(["RepositoryAllowlist", "RouteHealthRead"]);
    expect(stmts.filter((x: any) => [x.Action].flat().includes("dynamodb:DeleteItem")).map((x: any) => x.Sid)).toEqual(["RepositoryAllowlist"]);
    // Every UpdateItem is scoped to a partition family; none is table-wide.
    for (const x of stmts.filter((y: any) => [y.Action].flat().includes("dynamodb:UpdateItem"))) {
      expect(x.Condition?.["ForAllValues:StringLike"]?.["dynamodb:LeadingKeys"], x.Sid).toBeDefined();
    }
  });

  // The answer of the download route holds a presigned URL, a bearer capability. The API Gateway stage logs no request or
  // response body: no data trace and no execution log, and no access log.
  test("the /v1 stage logs no bodies: no data trace, no logging level, no access log", () => {
    const stages = Object.values(resourcesOf(t, "AWS::ApiGateway::Stage")).map((r) => r.Properties as any);
    const v1 = stages.filter((s) => JSON.stringify(s.RestApiId).includes("V1Api"));
    expect(v1).toHaveLength(1);
    expect(v1[0].AccessLogSetting).toBeUndefined();
    for (const m of v1[0].MethodSettings ?? []) {
      expect(m.DataTraceEnabled ?? false).toBe(false);
      expect(m.LoggingLevel ?? "OFF").toBe("OFF");
    }
  });

  test("log groups have a retention", () => {
    const lgs = Object.values(resourcesOf(t, "AWS::Logs::LogGroup")).map((r) => r.Properties as any);
    expect(lgs.length).toBeGreaterThanOrEqual(1);
    for (const lg of lgs) expect(lg.RetentionInDays).toBeGreaterThan(0);
  });
});

describe("control stack outputs", () => {
  test("the internal API id and URL are stack outputs for the dispatcher and agentd", () => {
    const outs = template("direct", "control").toJSON().Outputs ?? {};
    expect(Object.keys(outs)).toEqual(expect.arrayContaining(["InternalApiId", "InternalApiUrl", "V1ApiUrl"]));
    expect(JSON.stringify(outs.InternalApiUrl.Value)).toContain(`.execute-api.${REGION}.amazonaws.com/direct`);
    expect(JSON.stringify(outs.V1ApiUrl.Value)).toContain(`.execute-api.${REGION}.amazonaws.com/direct`);
  });
});
