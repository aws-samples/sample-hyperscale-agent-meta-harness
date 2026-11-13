import { describe, expect, test } from "vitest";
import { buildApp } from "../lib/app.ts";
import {
  ACCOUNT, checkResearchProvider, DIRECT_LAMBDAS, DIRECT_STACKS, names, REGION, RESEARCH_PROVIDERS, STAGE_KIND, stageConfig, TARGET,
} from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { phase2Images, resourcesOf, synth, template } from "./helpers.ts";

// The research route. Two Lambda functions (research-api: search, page fetch, job start and status, limits, the only
// reader of the Tavily key; research-job: the repository download, no key), one table, one empty secret and five routes under
// /internal/research/ on the existing private API. The VM firewall, the worker session policy and the network stack do not move. The
// resources are a construct of the control stack (no tenth stack), so the nine-stack contract of stage direct stays.
const BUCKET = `hamh-direct-harness-${ACCOUNT}`;
const RESEARCH_PREFIX = `arn:aws:s3:::${BUCKET}/runs/*/agents/direct/research/*`;
const SESSION_ROLE = `arn:aws:iam::${ACCOUNT}:role/hamh-direct-harness-worker-session`;
const ROUTES = ["search", "fetch", "repo", "job", "limits"];

type Stmt = Record<string, any>;
type Props = Record<string, any>;

// The assertions of this file were written for the tavily provider, which was the default of a stage made it auto: they build it by name.
// test/research-auto.test.ts holds the auto stage and the default.
const TAVILY = { researchProvider: "tavily" } as const;
const t = template("direct", "control", TAVILY);
const fnProps = (name: string): Props => {
  const hits = Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as Props).filter((p) => p.FunctionName === name);
  expect(hits, name).toHaveLength(1);
  return hits[0];
};
const idOf = (type: string, pick: (p: Props) => boolean): string => {
  const hits = Object.entries(resourcesOf(t, type)).filter(([, r]) => pick(r.Properties as Props)).map(([id]) => id);
  expect(hits, type).toHaveLength(1);
  return hits[0];
};
const roleProps = (name: string): Props => {
  const hits = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as Props).filter((p) => p.RoleName === name);
  expect(hits, name).toHaveLength(1);
  return hits[0];
};
const statements = (roleName: string): Stmt[] => roleProps(roleName).Policies.flatMap((p: Props) => p.PolicyDocument.Statement);
const byId = (s: Stmt[], sid: string): Stmt => {
  const x = s.find((y) => y.Sid === sid);
  expect(x, sid).toBeDefined();
  return x!;
};
const actions = (s: Stmt): string[] => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const resources = (s: Stmt): unknown[] => (Array.isArray(s.Resource) ? s.Resource : [s.Resource]);
const internalApiId = () => idOf("AWS::ApiGateway::RestApi", (p) => p.Name === "hamh-direct-internal");
const onInternal = (type: string) =>
  Object.values(resourcesOf(t, type)).map((r) => r.Properties as Props).filter((p) => p.RestApiId?.Ref === internalApiId());

describe("names and configuration", () => {
  const cfg = stageConfig("direct");

  test("the table, the secret, the functions and the roles are named after the stage prefix", () => {
    expect(names.researchTable(cfg)).toBe("hamh-direct-research");
    // scripts/research-key-put.py pins this exact name (scripts/tests/test_research_key_put.py reads it from this file).
    expect(names.researchSecret(cfg)).toBe("hamh-direct-research-tavily-api-key");
    expect(names.role(cfg, "research-api")).toBe("hamh-direct-research-api");
    expect(names.role(cfg, "research-job")).toBe("hamh-direct-research-job");
  });

  test("the stage binaries are the three of the control line and the two research functions", () => {
    expect([...DIRECT_LAMBDAS]).toEqual(["worker-api", "control-api", "run-session", "research-api", "research-job"]);
  });

  test("there is no tenth stack: the research resources belong to the control stack", () => {
    expect(DIRECT_STACKS).toHaveLength(9);
    const { templates } = synth("direct", TAVILY);
    expect(templates.size).toBe(9);
    const fnNames = (stack: string) => Object.values(resourcesOf(templates.get(`hamh-direct-${stack}`)!, "AWS::Lambda::Function"))
      .map((r) => (r.Properties as Props).FunctionName);
    expect(fnNames("control")).toEqual(expect.arrayContaining(["hamh-direct-research-api", "hamh-direct-research-job"]));
    for (const other of DIRECT_STACKS.filter((s) => s !== "control")) {
      expect(fnNames(other), other).not.toContain("hamh-direct-research-api");
      expect(fnNames(other), other).not.toContain("hamh-direct-research-job");
    }
  });

  test("the stage kind is official for the active accounts.json entry and test for any other", () => {
    expect(STAGE_KIND).toBe(TARGET.role === "active" ? "official" : "test");
  });

  // Adds `agentcore` as the fourth word (AWS's own web index through an AgentCore Gateway; infra/test/research-gateway.test.ts holds its resources) and
  // adds `auto` as the fifth (infra/test/research-auto.test.ts).
  test("a stage names one of five providers; fixture is refused on an official stage", () => {
    expect([...RESEARCH_PROVIDERS]).toEqual(["tavily", "tavily-keyless", "fixture", "agentcore", "auto"]);
    expect(() => checkResearchProvider("tavily", "official")).not.toThrow();
    expect(() => checkResearchProvider("tavily-keyless", "official")).not.toThrow();
    expect(() => checkResearchProvider("agentcore", "official")).not.toThrow();
    expect(() => checkResearchProvider("agentcore", "test")).not.toThrow();
    expect(() => checkResearchProvider("fixture", "test")).not.toThrow();
    expect(() => checkResearchProvider("fixture", "official")).toThrow(/fixture.*official|official.*fixture/);
    expect(() => checkResearchProvider("bing" as never, "test")).toThrow(/provider/);
  });

  test("synthesis throws for the fixture provider on an official stage and passes on a test stage", () => {
    if (STAGE_KIND === "official") {
      expect(() => buildApp({ stage: "direct", researchProvider: "fixture" })).toThrow(/fixture/);
    } else {
      expect(() => buildApp({ stage: "direct", researchProvider: "fixture" })).not.toThrow();
    }
  });

  test("the entry reads HAMH_RESEARCH_PROVIDER, validates it and adds nothing when it is unset", () => {
    const base = { HAMH_LAMBDA_DIR: new URL("./fixtures/lambda", import.meta.url).pathname };
    expect("researchProvider" in entryOptions(base)).toBe(false);
    expect("researchProvider" in entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "" })).toBe(false);
    expect(entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "tavily-keyless" }).researchProvider).toBe("tavily-keyless");
    expect(entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "fixture" }).researchProvider).toBe("fixture");
    expect(entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "agentcore" }).researchProvider).toBe("agentcore");
    expect(entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "auto" }).researchProvider).toBe("auto");
    expect(() => entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "bing" })).toThrow(/HAMH_RESEARCH_PROVIDER/);
    // The error names every word the setting accepts, the new one included.
    expect(() => entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "bing" })).toThrow(/tavily, tavily-keyless, fixture, agentcore, auto/);
  });

  test("the entry requires the bootstraps of both research functions", () => {
    const worker = new URL("./fixtures/lambda-worker-only", import.meta.url).pathname;
    expect(() => entryOptions({ HAMH_LAMBDA_DIR: worker })).toThrow(/bootstrap/);
    for (const name of ["research-api", "research-job"]) {
      expect(DIRECT_LAMBDAS).toContain(name);
    }
  });
});

describe("the five routes", () => {
  test("a research resource below internal holds search, fetch, repo, job and limits", () => {
    const parts = onInternal("AWS::ApiGateway::Resource").map((p) => p.PathPart);
    expect(parts.sort()).toEqual(["internal", "creds", "heartbeat", "turn-complete", "events", "message-session", "message-claim", "message-seal", "message-outcome", "research", ...ROUTES].sort());
    const research = idOf("AWS::ApiGateway::Resource", (p) => p.PathPart === "research" && p.RestApiId?.Ref === internalApiId());
    const internal = idOf("AWS::ApiGateway::Resource", (p) => p.PathPart === "internal" && p.RestApiId?.Ref === internalApiId());
    expect(Object.values(resourcesOf(t, "AWS::ApiGateway::Resource")).map((r) => r.Properties as Props)
      .find((p) => p.PathPart === "research")!.ParentId).toEqual({ Ref: internal });
    for (const route of ROUTES) {
      const hits = onInternal("AWS::ApiGateway::Resource").filter((p) => p.PathPart === route);
      expect(hits, route).toHaveLength(1);
      expect(hits[0].ParentId, route).toEqual({ Ref: research });
    }
  });

  test("each is a POST with IAM auth and a Lambda proxy integration with research-api, and none reaches worker-api", () => {
    const methods = onInternal("AWS::ApiGateway::Method");
    // Four calls of agentd (creds, heartbeat, turn-complete, events), four of message control, and the five research routes.
    expect(methods).toHaveLength(13);
    const researchFn = idOf("AWS::Lambda::Function", (p) => p.FunctionName === "hamh-direct-research-api");
    const workerFn = idOf("AWS::Lambda::Function", (p) => p.FunctionName === "hamh-direct-worker-api");
    const research = idOf("AWS::ApiGateway::Resource", (p) => p.PathPart === "research" && p.RestApiId?.Ref === internalApiId());
    const resourceIds = Object.fromEntries(Object.entries(resourcesOf(t, "AWS::ApiGateway::Resource"))
      .filter(([, r]) => (r.Properties as Props).ParentId?.Ref === research).map(([id, r]) => [id, (r.Properties as Props).PathPart]));
    const mine = methods.filter((m) => resourceIds[m.ResourceId?.Ref] !== undefined);
    expect(mine.map((m) => resourceIds[m.ResourceId.Ref]).sort()).toEqual([...ROUTES].sort());
    for (const m of mine) {
      expect(m.HttpMethod).toBe("POST");
      expect(m.AuthorizationType).toBe("AWS_IAM");
      expect(m.Integration.Type).toBe("AWS_PROXY");
      expect(m.Integration.IntegrationHttpMethod).toBe("POST");
      expect(JSON.stringify(m.Integration.Uri)).toContain(`"Fn::GetAtt":["${researchFn}","Arn"]`);
      expect(JSON.stringify(m.Integration.Uri)).not.toContain(workerFn);
    }
    const others = methods.filter((m) => resourceIds[m.ResourceId?.Ref] === undefined);
    expect(others).toHaveLength(8);
    for (const m of others) expect(JSON.stringify(m.Integration.Uri)).toContain(`"Fn::GetAtt":["${workerFn}","Arn"]`);
  });

  test("API Gateway may invoke research-api only for POST /internal/research/*, from the internal API", () => {
    const perms = Object.values(resourcesOf(t, "AWS::Lambda::Permission")).map((r) => r.Properties as Props);
    const researchFn = idOf("AWS::Lambda::Function", (p) => p.FunctionName === "hamh-direct-research-api");
    const mine = perms.filter((p) => JSON.stringify(p.FunctionName).includes(researchFn));
    expect(mine).toHaveLength(1);
    expect(mine[0].Action).toBe("lambda:InvokeFunction");
    expect(mine[0].Principal).toBe("apigateway.amazonaws.com");
    const src = JSON.stringify(mine[0].SourceArn);
    expect(src).toContain(internalApiId());
    expect(src).toContain("/POST/internal/research/*");
    // research-job has no API permission: only research-api starts it.
    const jobFn = idOf("AWS::Lambda::Function", (p) => p.FunctionName === "hamh-direct-research-job");
    expect(perms.filter((p) => JSON.stringify(p.FunctionName).includes(jobFn))).toEqual([]);
  });

  // The deployment's logical id carries a hash of the routes and the resource policy (control.ts `deploymentId`), so a new route makes
  // CloudFormation create a new deployment and repoint the stage. The policy holds a cross-stack token (the worker endpoint), whose
  // resolved form differs before and after synthesis, so the hash itself is not recomputed here (the v1 test does that for a token-free
  // policy); this test pins that the deployment exists once, is named by a hash, and depends on every one of the thirteen methods.
  test("the internal deployment is hashed, is created after all thirteen methods, and the stage points at it", () => {
    const deployments = Object.entries(resourcesOf(t, "AWS::ApiGateway::Deployment"))
      .filter(([, r]) => (r.Properties as Props).RestApiId?.Ref === internalApiId());
    expect(deployments).toHaveLength(1);
    const [id, resource] = deployments[0] as [string, any];
    expect(id).toMatch(/^InternalDeployment[0-9a-f]{8}$/);
    const methodIds = Object.entries(resourcesOf(t, "AWS::ApiGateway::Method"))
      .filter(([, r]) => (r.Properties as Props).RestApiId?.Ref === internalApiId()).map(([mid]) => mid);
    expect(methodIds).toHaveLength(13);
    expect([...resource.DependsOn].sort()).toEqual([...methodIds].sort());
    expect(onInternal("AWS::ApiGateway::Stage")[0].DeploymentId).toEqual({ Ref: id });
  });

  test("the internal API resource policy is unchanged: worker sessions through the worker endpoint only", () => {
    const api = resourcesOf(t, "AWS::ApiGateway::RestApi")[internalApiId()].Properties as Props;
    expect(api.Policy.Statement).toHaveLength(2);
    expect(api.Policy.Statement[0].Condition.ArnEquals["aws:PrincipalArn"]).toBe(SESSION_ROLE);
  });
});

describe("the two functions", () => {
  test("research-api: arm64 provided.al2023, 512 MB, 28 s, outside any VPC, no X-Ray", () => {
    const f = fnProps("hamh-direct-research-api");
    expect(f.Runtime).toBe("provided.al2023");
    expect(f.Architectures).toEqual(["arm64"]);
    expect(f.Handler).toBe("bootstrap");
    expect(f.MemorySize).toBe(512);
    expect(f.Timeout).toBe(28);
    expect(f.VpcConfig).toBeUndefined();
    expect(f.TracingConfig === undefined || f.TracingConfig.Mode === "PassThrough").toBe(true);
    expect(f.LoggingConfig.LogFormat).toBe("JSON");
    expect(f.LoggingConfig.LogGroup).toEqual({ Ref: idOf("AWS::Logs::LogGroup", (p) => p.LogGroupName === "/aws/lambda/hamh-direct-research-api") });
  });

  test("research-api: the environment is the nine settings the Lambda reads", () => {
    const env = fnProps("hamh-direct-research-api").Environment.Variables;
    // HAMH_STAGE is the value of the Stage dimension of the Tavily key metrics (crates/research-api/src/metrics.rs).
    expect(Object.keys(env).sort()).toEqual([
      "HAMH_STAGE", "HAMH_STAGE_KIND", "HARNESS_BUCKET", "RESEARCH_JOB_FUNCTION", "RESEARCH_PROVIDER", "RESEARCH_STAGE_RATE_PER_MIN", "RESEARCH_TABLE_NAME",
      "TAVILY_SECRET_ID", "WORKER_SESSION_ROLE_NAME",
    ]);
    expect(env.HAMH_STAGE).toBe("direct");
    expect(env.RESEARCH_TABLE_NAME).toBe("hamh-direct-research");
    expect(env.HARNESS_BUCKET).toBe(BUCKET);
    expect(env.TAVILY_SECRET_ID).toBe("hamh-direct-research-tavily-api-key");
    expect(env.RESEARCH_JOB_FUNCTION).toBe("hamh-direct-research-job");
    expect(env.RESEARCH_PROVIDER).toBe("tavily");
    expect(env.HAMH_STAGE_KIND).toBe(STAGE_KIND);
    expect(env.RESEARCH_STAGE_RATE_PER_MIN).toBe("80");
    // The handler parses the caller from the assumed-role ARN and checks this role name, exactly as worker-api does.
    expect(env.WORKER_SESSION_ROLE_NAME).toBe("hamh-direct-harness-worker-session");
  });

  test("research-job: arm64 provided.al2023, 2048 MB, 15 minutes, outside any VPC, no X-Ray", () => {
    const f = fnProps("hamh-direct-research-job");
    expect(f.Runtime).toBe("provided.al2023");
    expect(f.Architectures).toEqual(["arm64"]);
    expect(f.Handler).toBe("bootstrap");
    expect(f.MemorySize).toBe(2048);
    expect(f.Timeout).toBe(900);
    expect(f.VpcConfig).toBeUndefined();
    expect(f.TracingConfig === undefined || f.TracingConfig.Mode === "PassThrough").toBe(true);
    expect(f.LoggingConfig.LogGroup).toEqual({ Ref: idOf("AWS::Logs::LogGroup", (p) => p.LogGroupName === "/aws/lambda/hamh-direct-research-job") });
  });

  test("research-job: three settings, and no key, secret name or provider", () => {
    const env = fnProps("hamh-direct-research-job").Environment.Variables;
    expect(Object.keys(env).sort()).toEqual(["HAMH_STAGE_KIND", "HARNESS_BUCKET", "RESEARCH_TABLE_NAME"]);
    expect(env.RESEARCH_TABLE_NAME).toBe("hamh-direct-research");
    expect(env.HARNESS_BUCKET).toBe(BUCKET);
    expect(env.HAMH_STAGE_KIND).toBe(STAGE_KIND);
    expect(JSON.stringify(env)).not.toMatch(/tavily|secret/i);
  });

  test("research-job runs asynchronously with no retries and no destination", () => {
    const cfgs = Object.values(resourcesOf(t, "AWS::Lambda::EventInvokeConfig")).map((r) => r.Properties as Props);
    expect(cfgs).toHaveLength(1);
    const jobFn = idOf("AWS::Lambda::Function", (p) => p.FunctionName === "hamh-direct-research-job");
    expect(cfgs[0].FunctionName).toEqual({ Ref: jobFn });
    expect(cfgs[0].Qualifier).toBe("$LATEST");
    expect(cfgs[0].MaximumRetryAttempts).toBe(0);
    expect(cfgs[0].MaximumEventAgeInSeconds).toBe(120);
    expect(cfgs[0].DestinationConfig).toBeUndefined();
  });

  test("each function is created after the table, and research-api after the secret (explicit dependencies)", () => {
    const tableId = idOf("AWS::DynamoDB::Table", (p) => p.TableName === "hamh-direct-research");
    const secretId = idOf("AWS::SecretsManager::Secret", () => true);
    const dependsOn = (name: string): string[] => {
      const id = idOf("AWS::Lambda::Function", (p) => p.FunctionName === name);
      const d = (resourcesOf(t, "AWS::Lambda::Function")[id] as any).DependsOn ?? [];
      return Array.isArray(d) ? d : [d];
    };
    expect(dependsOn("hamh-direct-research-api")).toEqual(expect.arrayContaining([tableId, secretId]));
    expect(dependsOn("hamh-direct-research-job")).toContain(tableId);
    expect(dependsOn("hamh-direct-research-job")).not.toContain(secretId);
  });

  test("each function has a log group with the stage retention and no tracing statement in its role", () => {
    for (const name of ["research-api", "research-job"]) {
      const lg = Object.values(resourcesOf(t, "AWS::Logs::LogGroup")).map((r) => r.Properties as Props)
        .filter((p) => p.LogGroupName === `/aws/lambda/hamh-direct-${name}`);
      expect(lg, name).toHaveLength(1);
      expect(lg[0].RetentionInDays).toBe(30);
      expect(JSON.stringify(statements(`hamh-direct-${name}`))).not.toMatch(/xray/);
    }
  });
});

describe("the research-api role is least privilege", () => {
  const s = () => statements("hamh-direct-research-api");

  test("exactly these statements, and a Lambda trust", () => {
    expect(s().map((x) => x.Sid).sort()).toEqual(["DataKeyThroughDynamoDbAndS3", "OwnLogs", "ReadTavilyKey", "ResearchObjects", "ResearchTable", "StartResearchJob"]);
    expect(roleProps("hamh-direct-research-api").AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }]);
  });

  test("DynamoDB: five item actions on the research table only", () => {
    const x = byId(s(), "ResearchTable");
    expect(x.Effect).toBe("Allow");
    expect(actions(x).sort()).toEqual(["dynamodb:BatchGetItem", "dynamodb:BatchWriteItem", "dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem"]);
    expect(resources(x)).toEqual([`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-research`]);
  });

  test("S3: get and put under the research prefix only (no list, no delete)", () => {
    const x = byId(s(), "ResearchObjects");
    expect(actions(x).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    expect(resources(x)).toEqual([RESEARCH_PREFIX]);
    for (const st of s()) {
      for (const a of actions(st)) expect(a, st.Sid).not.toMatch(/^s3:(Get(?!Object$)|List|Delete|Abort|Put(?!Object$))/);
    }
  });

  test("KMS: the data key through DynamoDB and S3 only", () => {
    const x = byId(s(), "DataKeyThroughDynamoDbAndS3");
    expect(actions(x).sort()).toEqual(["kms:Decrypt", "kms:DescribeKey", "kms:GenerateDataKey"]);
    expect(x.Condition).toEqual({ StringEquals: { "kms:ViaService": [`dynamodb.${REGION}.amazonaws.com`, `s3.${REGION}.amazonaws.com`] } });
  });

  test("the Tavily secret: GetSecretValue on that one secret's ARN, and no other secrets action", () => {
    const x = byId(s(), "ReadTavilyKey");
    expect(actions(x)).toEqual(["secretsmanager:GetSecretValue"]);
    const secret = idOf("AWS::SecretsManager::Secret", () => true);
    expect(resources(x)).toEqual([{ Ref: secret }]);
    expect(JSON.stringify(s())).not.toContain('"*"');
    const secretActions = s().flatMap(actions).filter((a) => a.startsWith("secretsmanager:"));
    expect(secretActions).toEqual(["secretsmanager:GetSecretValue"]);
  });

  test("it may start research-job and nothing else of Lambda", () => {
    const x = byId(s(), "StartResearchJob");
    expect(actions(x)).toEqual(["lambda:InvokeFunction"]);
    const job = idOf("AWS::Lambda::Function", (p) => p.FunctionName === "hamh-direct-research-job");
    expect(resources(x)).toEqual([{ "Fn::GetAtt": [job, "Arn"] }]);
  });

  test("no statement uses a service wildcard action", () => {
    for (const st of s()) for (const a of actions(st)) expect(a, st.Sid).not.toMatch(/:\*$|^\*$/);
  });
});

describe("the research-job role holds no key", () => {
  const s = () => statements("hamh-direct-research-job");

  test("exactly these statements, and a Lambda trust", () => {
    expect(s().map((x) => x.Sid).sort()).toEqual(["DataKeyThroughDynamoDbAndS3", "OwnLogs", "ResearchObjects", "ResearchTable"]);
    expect(roleProps("hamh-direct-research-job").AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }]);
  });

  test("DynamoDB: read and update its job and counters on the research table; S3: PutObject under the research prefix", () => {
    const x = byId(s(), "ResearchTable");
    expect(actions(x).sort()).toEqual(["dynamodb:GetItem", "dynamodb:UpdateItem"]);
    expect(resources(x)).toEqual([`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-research`]);
    expect(actions(byId(s(), "ResearchObjects")).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    expect(resources(byId(s(), "ResearchObjects"))).toEqual([RESEARCH_PREFIX]);
  });

  test("it has no secrets, Lambda or wildcard grant", () => {
    const text = JSON.stringify(s());
    expect(text).not.toMatch(/secretsmanager|lambda:|"\*"/);
  });
});

describe("the research table", () => {
  const tables = () => Object.values(resourcesOf(t, "AWS::DynamoDB::Table")).map((r) => r.Properties as Props);

  test("on demand, PK and SK strings, a ttl attribute, the data key, no point-in-time recovery", () => {
    expect(tables()).toHaveLength(1);
    const tbl = tables()[0];
    expect(tbl.TableName).toBe("hamh-direct-research");
    expect(tbl.BillingMode).toBe("PAY_PER_REQUEST");
    expect(tbl.KeySchema).toEqual([{ AttributeName: "PK", KeyType: "HASH" }, { AttributeName: "SK", KeyType: "RANGE" }]);
    expect(tbl.AttributeDefinitions).toEqual(expect.arrayContaining([{ AttributeName: "PK", AttributeType: "S" }, { AttributeName: "SK", AttributeType: "S" }]));
    expect(tbl.AttributeDefinitions).toHaveLength(2);
    expect(tbl.TimeToLiveSpecification).toEqual({ AttributeName: "ttl", Enabled: true });
    expect(tbl.SSESpecification.SSEEnabled).toBe(true);
    expect(tbl.SSESpecification.SSEType).toBe("KMS");
    expect(tbl.SSESpecification.KMSMasterKeyId).toBeDefined();
    expect(tbl.PointInTimeRecoverySpecification === undefined || tbl.PointInTimeRecoverySpecification.PointInTimeRecoveryEnabled === false).toBe(true);
    expect(tbl.GlobalSecondaryIndexes).toBeUndefined();
    expect(tbl.StreamSpecification).toBeUndefined();
  });
});

describe("the Tavily secret", () => {
  const secrets = () => Object.entries(resourcesOf(t, "AWS::SecretsManager::Secret"));

  test("exists, named after the stage, created EMPTY and under the default key", () => {
    expect(secrets()).toHaveLength(1);
    const p = secrets()[0][1].Properties as Props;
    expect(p.Name).toBe("hamh-direct-research-tavily-api-key");
    for (const forbidden of ["SecretString", "GenerateSecretString", "SecretBinary", "KmsKeyId", "ReplicaRegions"]) {
      expect(p[forbidden], forbidden).toBeUndefined();
    }
    const resource = secrets()[0][1] as any;
    expect(resource.DeletionPolicy === undefined || resource.DeletionPolicy === "Delete").toBe(true);
  });

  test("no template of the stage holds a secret value, a secret parameter or an output of the secret", () => {
    const { templates } = synth("direct", { ...phase2Images("direct"), ...TAVILY });
    for (const [name, tpl] of templates) {
      const json = tpl.toJSON();
      const text = JSON.stringify(json);
      expect(text, name).not.toMatch(/SecretString|GenerateSecretString|SecretBinary|resolve:secretsmanager/);
      for (const [k, o] of Object.entries(json.Outputs ?? {}) as [string, any][]) {
        expect(JSON.stringify(o.Value), `${name}/${k}`).not.toContain("research-tavily");
      }
      for (const k of Object.keys(json.Parameters ?? {})) expect(k, name).not.toMatch(/secret|tavily|key/i);
    }
  });

  test("its resource policy denies GetSecretValue to every principal but the research-api role, and blocks public policies", () => {
    const pols = Object.values(resourcesOf(t, "AWS::SecretsManager::ResourcePolicy")).map((r) => r.Properties as Props);
    expect(pols).toHaveLength(1);
    const secret = secrets()[0][0];
    expect(pols[0].SecretId).toEqual({ Ref: secret });
    expect(pols[0].BlockPublicPolicy).toBe(true);
    const st = pols[0].ResourcePolicy.Statement as Stmt[];
    expect(st).toHaveLength(1);
    const role = idOf("AWS::IAM::Role", (p) => p.RoleName === "hamh-direct-research-api");
    expect(st[0]).toEqual({
      Sid: "OnlyResearchApiReadsTheKey", Effect: "Deny", Principal: "*",
      Action: ["secretsmanager:GetSecretValue", "secretsmanager:BatchGetSecretValue"], Resource: "*",
      Condition: { StringNotEquals: { "aws:PrincipalArn": { "Fn::GetAtt": [role, "Arn"] } } },
    });
    // The owner stores the key and checks that it is there: nothing else of the secret is denied.
    for (const a of ["secretsmanager:DescribeSecret", "secretsmanager:PutSecretValue", "secretsmanager:TagResource"]) {
      expect(JSON.stringify(st), a).not.toContain(a);
    }
  });
});

describe("the bucket policy closes the research prefix to the worker session", () => {
  const data = template("direct", "data");
  const stmts = () => (Object.values(resourcesOf(data, "AWS::S3::BucketPolicy"))[0].Properties as Props).PolicyDocument.Statement as Stmt[];

  test("a Deny of every write and delete action on runs/*/agents/direct/research/*, for the worker session role only", () => {
    const x = byId(stmts(), "WorkerSessionsCannotWriteResearchMaterial");
    expect(x.Effect).toBe("Deny");
    expect(x.Principal).toBe("*");
    expect([...actions(x)].sort()).toEqual([
      "s3:AbortMultipartUpload", "s3:DeleteObject", "s3:DeleteObjectTagging", "s3:DeleteObjectVersion", "s3:PutObject", "s3:PutObjectTagging"]);
    expect(JSON.stringify(x.Resource)).toContain("/runs/*/agents/direct/research/*");
    expect(x.Condition).toEqual({ ArnEquals: { "aws:PrincipalArn": SESSION_ROLE } });
  });

  test("the transport Deny stays, and nothing else was added", () => {
    expect(stmts().map((x) => x.Sid).sort()).toEqual(["DenyInsecureTransport", "WorkerSessionsCannotWriteResearchMaterial"]);
  });
});

describe("what does not move", () => {
  test("the worker session policy names no research resource", () => {
    const security = template("direct", "security").toJSON();
    const ws = Object.values(security.Resources as Record<string, any>).find(
      (r) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === "hamh-direct-harness-worker-session")!;
    expect(JSON.stringify(ws.Properties.Policies)).not.toMatch(/research/i);
  });

  test("the network stack names no research host or service, and the firewall allowlist is the recorded one", () => {
    const net = JSON.stringify(template("direct", "network").toJSON());
    expect(net).not.toMatch(/research|tavily|secretsmanager|codeload|github/i);
  });

  test("no function of an official stage has the fixture provider in its environment", () => {
    if (STAGE_KIND !== "official") return;
    const { templates } = synth("direct", { ...phase2Images("direct"), ...TAVILY });
    let functions = 0;
    for (const [name, tpl] of templates) {
      for (const r of Object.values(resourcesOf(tpl, "AWS::Lambda::Function"))) {
        functions += 1;
        const env = ((r.Properties as Props).Environment?.Variables ?? {}) as Record<string, unknown>;
        for (const [k, v] of Object.entries(env)) expect(String(v), `${name} ${k}`).not.toMatch(/fixture/i);
      }
    }
    expect(functions).toBeGreaterThanOrEqual(5);
  });

  test("run-session writes the policy row and knows the provider", () => {
    const orch = synth("direct", { ...phase2Images("direct"), ...TAVILY }).templates.get("hamh-direct-orchestration")!.toJSON();
    const fn = Object.values(orch.Resources as Record<string, any>).find((r) => r.Type === "AWS::Lambda::Function")!;
    expect(fn.Properties.Environment.Variables.RESEARCH_TABLE_NAME).toBe("hamh-direct-research");
    expect(fn.Properties.Environment.Variables.RESEARCH_PROVIDER).toBe("tavily");
  });
});
