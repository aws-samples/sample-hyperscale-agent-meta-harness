import { describe, expect, test } from "vitest";
import { resourcesOf, synth, template } from "./helpers.ts";
import { ACCOUNT, INFERENCE_REGIONS, OWNER, REGION } from "../lib/config.ts";

type Stmt = { Sid?: string; Effect: string; Action?: string | string[]; Resource?: unknown; Condition?: Record<string, unknown>; Principal?: unknown };
const actions = (s: Stmt) => (Array.isArray(s.Action) ? s.Action : s.Action ? [s.Action] : []);

function role(t: ReturnType<typeof template>, roleName: string) {
  const found = Object.values(resourcesOf(t, "AWS::IAM::Role")).filter((r) => JSON.stringify(r.Properties?.RoleName).includes(roleName));
  expect(found, roleName).toHaveLength(1);
  const props = found[0].Properties as any;
  const stmts: Stmt[] = (props.Policies ?? []).flatMap((p: any) => p.PolicyDocument.Statement);
  return { props, stmts, trust: props.AssumeRolePolicyDocument.Statement as Stmt[] };
}

const BASE_DENIES = ["sts:AssumeRole", "sts:TagSession", "sts:SetSourceIdentity", "sts:GetFederationToken", "bedrock:*",
  "bedrock-mantle:*", "s3:*", "dynamodb:*", "appsync:*", "secretsmanager:*", "ssm:GetParameter*", "kms:Decrypt", "lambda:*",
  "iam:*", "bedrock-agentcore:GetWorkloadAccessToken", "bedrock-agentcore:GetWorkloadAccessTokenForJWT",
  "bedrock-agentcore:GetWorkloadAccessTokenForUserId", "bedrock-agentcore:GetResourceOauth2Token",
  "bedrock-agentcore:GetResourceApiKey", "bedrock-agentcore:InvokeAgentRuntime", "bedrock-agentcore:InvokeAgentRuntimeCommand",
  "bedrock-agentcore:StopRuntimeSession", "bedrock-agentcore:InvokeGateway",
  // C17: no microVM identity reaches the E2E git host or its PR check.
  "codecommit:*", "codebuild:StartBuild"];

describe("security stack", () => {
  const t = template("direct", "security");

  test("KMS: a rotating data key and an ECC signing key for the engine policy", () => {
    const keys = Object.values(resourcesOf(t, "AWS::KMS::Key")).map((k) => k.Properties as any);
    const data = keys.filter((k) => k.KeySpec === undefined || k.KeySpec === "SYMMETRIC_DEFAULT");
    const sign = keys.filter((k) => k.KeySpec === "ECC_NIST_P256" && k.KeyUsage === "SIGN_VERIFY");
    expect(data).toHaveLength(1);
    expect(data[0].EnableKeyRotation).toBe(true);
    expect(sign).toHaveLength(1);
    const aliases = Object.values(resourcesOf(t, "AWS::KMS::Alias")).map((a) => (a.Properties as any).AliasName);
    expect(aliases.sort()).toEqual(["alias/hamh-direct-data", "alias/hamh-direct-engine-policy"]);
  });

  test("runtime base role: AgentCore trust, image pull and own logs only, explicit deny list", () => {
    const { props, stmts, trust } = role(t, "hamh-direct-runtime-base");
    expect(JSON.stringify(trust)).toContain("bedrock-agentcore.amazonaws.com");
    expect(JSON.stringify(trust)).toContain("aws:SourceAccount");
    expect(props.MaxSessionDuration ?? 3600).toBeLessThanOrEqual(3600);
    const allows = stmts.filter((s) => s.Effect === "Allow").flatMap(actions);
    for (const a of allows) {
      expect(/^(ecr:|logs:|xray:|cloudwatch:PutMetricData)/.test(a), `unexpected allow ${a}`).toBe(true);
    }
    const deny = stmts.filter((s) => s.Effect === "Deny").flatMap(actions);
    for (const a of BASE_DENIES) expect(deny, a).toContain(a);
  });

  test("worker session role: trust by principal ARN with required session tags", () => {
    const { props, trust } = role(t, "hamh-direct-harness-worker-session");
    expect(props.MaxSessionDuration).toBe(3600);
    expect(trust).toHaveLength(2);
    const minters = ["role/Admin", "hamh-direct-worker-api", "hamh-direct-run-session"];
    const s = trust.find((x) => actions(x).includes("sts:AssumeRole"))!;
    expect([...actions(s)].sort()).toEqual(["sts:AssumeRole", "sts:TagSession"]);
    const cond = JSON.stringify(s.Condition);
    for (const needle of ["aws:PrincipalArn", ...minters, "aws:RequestTag/run", "aws:RequestTag/agent", "aws:TagKeys"]) {
      expect(cond, needle).toContain(needle);
    }
    const keys = (s.Condition as any)["ForAllValues:StringEquals"]["aws:TagKeys"];
    expect([...keys].sort()).toEqual(["agent", "engine", "mode", "run", "tenant"]);
    // SetSourceIdentity is authorized on its own: its request context carries no request tags, so
    // the tag conditions above would deny it (the first mint of the multi-agent line, 2026-09-26).
    const si = trust.find((x) => actions(x).includes("sts:SetSourceIdentity"))!;
    expect(actions(si)).toEqual(["sts:SetSourceIdentity"]);
    const siCond = JSON.stringify(si.Condition);
    for (const needle of ["aws:PrincipalArn", ...minters, "sts:SourceIdentity"]) expect(siCond, needle).toContain(needle);
    expect(siCond).not.toContain("aws:RequestTag");
  });

  test("worker session policy: no other agent's bundle is readable, the merge train's included (stage direct has one agent)", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const gets = stmts.filter((s) => s.Effect === "Allow" && actions(s).includes("s3:GetObject"));
    const resources = gets.flatMap((s) => [s.Resource].flat()).map(String);
    for (const r of resources) expect(r, r).not.toMatch(/agents\/merge|agents\/\*\//);
  });

  // The S3 reads of a worker session are the run's input and the session's own agent prefix, nothing else.
  test("worker session policy: the S3 reads are the run's input and the agent's own prefix, nothing else", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const reads = stmts.filter((s) => s.Effect === "Allow" && actions(s).includes("s3:GetObject"))
      .flatMap((s) => [s.Resource].flat()).map(String).sort();
    expect(reads).toEqual([`${bucket}/runs/\${aws:PrincipalTag/run}/agents/\${aws:PrincipalTag/agent}/*`, `${bucket}/runs/\${aws:PrincipalTag/run}/input/*`]);
  });

  // Agentd writes a stop digest after every stop and an audit copy of every fresh-session digest under the direct agent's turn
  // prefix. The worker session's own-agent-prefix grant must cover both keys (and the snapshot pair it already wrote), for its own run only.
  test("worker session policy: the stop digest, the fresh-session audit copy and the snapshot are writable under the agent's own prefix", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    const put = stmts.filter((s) => s.Effect === "Allow" && actions(s).includes("s3:PutObject")).flatMap((s) => [s.Resource].flat()).map(String);
    const resolve = (pattern: string, run: string, agent: string) => pattern.replaceAll("${aws:PrincipalTag/run}", run).replaceAll("${aws:PrincipalTag/agent}", agent);
    const matches = (pattern: string, value: string) => new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`).test(value);
    const run = "01M4ZX2QEMB4JFCA411Q5169AY";
    const reach = (runTag: string, agentTag: string, key: string) => put.some((p) => matches(resolve(p, runTag, agentTag), `${bucket}/${key}`));
    for (const name of ["stop-digest-1.json", "stop-digest-7.json", "fresh-session-2.md", "snapshot.bundle", "snapshot.json", "summary.json"]) {
      expect(reach(run, "direct", `runs/${run}/agents/direct/turns/1/${name}`), name).toBe(true);
      expect(reach(run, "direct", `runs/OTHERRUN/agents/direct/turns/1/${name}`), `${name} of another run`).toBe(false);
      expect(reach(run, "direct", `runs/${run}/agents/merge/turns/1/${name}`), `${name} of another agent`).toBe(false);
    }
  });

  // research-api answers an operator's key-state question to a direct invoke of the function (crates/research-api/AGENTS.md, KEYSTATE). The
  // function cannot tell an operator from another caller that sends the same event, so no worker may hold lambda:InvokeFunction on it, or on anything.
  test("worker session policy grants no Lambda action: the key-state question of research-api is for an operator's direct invoke", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const allowed = stmts.filter((s) => s.Effect === "Allow").flatMap(actions);
    expect(allowed.length).toBeGreaterThan(0);
    for (const a of allowed) {
      expect(a === "*" || a.toLowerCase().startsWith("lambda:"), `the worker session allows ${a}`).toBe(false);
    }
    // The only Allow that reaches the research function is the internal API's execute-api call, which has no key-state method.
    const lambdaResources = stmts.filter((s) => s.Effect === "Allow" && JSON.stringify(s.Resource ?? "").includes(":function:"));
    expect(lambdaResources).toEqual([]);
  });

  test("worker session policy denies the E2E git host and its PR check", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const deny = stmts.filter((s) => s.Effect === "Deny").flatMap(actions);
    expect(deny).toContain("codecommit:*");
    expect(deny).toContain("codebuild:StartBuild");
  });

  test("worker session policy: tag-scoped S3, Bedrock ceiling, internal API; no state-store access", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const text = JSON.stringify(stmts);
    expect(text).toContain("runs/${aws:PrincipalTag/run}/input/*");
    expect(text).toContain("runs/${aws:PrincipalTag/run}/agents/${aws:PrincipalTag/agent}/*");
    const allows = stmts.filter((s) => s.Effect === "Allow").flatMap(actions);
    expect(allows).toContain("bedrock:InvokeModel");
    expect(allows).toContain("bedrock:InvokeModelWithResponseStream");
    expect(allows).toContain("bedrock-mantle:CreateInference");
    expect(allows).toContain("execute-api:Invoke");
    for (const a of allows) {
      expect(/^(dynamodb|appsync|states|iam|secretsmanager|ssm|lambda):/.test(a), `forbidden allow ${a}`).toBe(false);
      expect(a).not.toBe("*");
    }
    // No package-registry token:
    // the engine uses these credentials, so they grant no STS call and no CodeArtifact action; the
    // run-session mints the token with its own role and agentd receives it in the invocation.
    expect(allows.filter((a) => a.startsWith("sts:") || a.startsWith("codeartifact:"))).toEqual([]);
    const noTokens = stmts.filter((s) => s.Effect === "Deny" && s.Sid === "NoPackageTokens");
    expect(noTokens).toHaveLength(1);
    expect(actions(noTokens[0]).sort()).toEqual(["codeartifact:GetAuthorizationToken", "sts:GetServiceBearerToken"]);
    expect(noTokens[0].Resource).toBe("*");
    expect(noTokens[0].Condition).toBeUndefined();
    // Requests without both tags are denied outright.
    const tagDenies = stmts.filter((s) => s.Effect === "Deny" && JSON.stringify(s.Condition ?? {}).includes("Null"));
    expect(JSON.stringify(tagDenies)).toContain("aws:PrincipalTag/run");
    expect(JSON.stringify(tagDenies)).toContain("aws:PrincipalTag/agent");
  });

  test("worker session policy: the Bedrock invoke ceiling names literal Regions only, because the session policy's wildcard Region is bounded by it", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const invoke = ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream"];
    const glob = (pattern: string, text: string) =>
      new RegExp("^" + pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".") + "$", "i").test(text);
    const reaching = stmts.filter((s) => s.Effect === "Allow"
      && ((s as any).NotAction !== undefined || actions(s).some((a) => invoke.some((i) => glob(a, i)))));
    expect(reaching.map((s) => s.Sid), "only the ceiling statement can reach a Bedrock invoke action").toEqual(["InvokeModelsCeiling"]);
    const resources = reaching[0].Resource as string[];
    const expected = INFERENCE_REGIONS.flatMap((r) => [
      `arn:aws:bedrock:${r}::foundation-model/*`,
      `arn:aws:bedrock:${r}:${ACCOUNT}:inference-profile/*`,
      `arn:aws:bedrock:${r}:${ACCOUNT}:application-inference-profile/*`,
    ]).concat(["arn:aws:bedrock:::foundation-model/*"]);
    expect([...resources].sort()).toEqual([...expected].sort());
    for (const r of resources) {
      const [arn, aws, bedrock, region, account] = r.split(":");
      expect([arn, aws, bedrock], r).toEqual(["arn", "aws", "bedrock"]);
      expect(region === "" || (INFERENCE_REGIONS as readonly string[]).includes(region), `${r}: Region`).toBe(true);
      expect(account === "" || account === ACCOUNT, `${r}: account`).toBe(true);
      expect(r, r).not.toMatch(/\?|\$\{/);
    }
  });

  test("worker session policy: denies calls from outside the worker VPC endpoints and NAT IPs", () => {
    const { stmts } = role(t, "hamh-direct-harness-worker-session");
    const net = stmts.filter((s) => s.Effect === "Deny" && JSON.stringify(s.Condition ?? {}).includes("aws:SourceVpce"));
    expect(net).toHaveLength(1);
    const c = net[0].Condition as any;
    expect(Object.keys(c).sort()).toEqual(["BoolIfExists", "NotIpAddressIfExists", "StringNotEqualsIfExists"]);
    // Calls a service makes for the worker (S3 -> KMS Decrypt for an SSE-KMS object) carry neither
    // key; without this exemption the deny refused kms:Decrypt invoked by fas.s3.amazonaws.com.
    expect(c.BoolIfExists).toEqual({ "aws:ViaAWSService": "false" });
    expect(c.StringNotEqualsIfExists["aws:SourceVpce"].length).toBeGreaterThanOrEqual(8);
    expect(c.NotIpAddressIfExists["aws:SourceIp"]).toHaveLength(1);
    expect(actions(net[0])).toEqual(["*"]);
  });

  // Access Analyzer ValidatePolicy reports `?*` on these keys as
  // WILDCARD_USAGE_TOO_PERMISSIVE (ERROR). Null false plus StringNotEquals "" requires the same thing:
  // the key present and non-empty (session tag values may be empty; a source identity may not).
  test("session role trusts require the run and agent tags and a source identity without wildcards", () => {
    for (const name of ["hamh-direct-harness-worker-session"]) {
      const { trust } = role(t, name);
      const tagged = trust.find((x) => actions(x).includes("sts:AssumeRole"))!;
      expect((tagged.Condition as any).Null, name).toEqual({ "aws:RequestTag/run": "false", "aws:RequestTag/agent": "false" });
      expect((tagged.Condition as any).StringNotEquals, name).toEqual({ "aws:RequestTag/run": "", "aws:RequestTag/agent": "" });
      const si = trust.find((x) => actions(x).includes("sts:SetSourceIdentity"))!;
      expect((si.Condition as any).Null, name).toEqual({ "sts:SourceIdentity": "false" });
      expect((si.Condition as any).StringNotEquals, name).toEqual({ "sts:SourceIdentity": "" });
      expect(JSON.stringify(si.Condition)).not.toContain("aws:RequestTag");
    }
  });

  test("no trust policy in the app matches a condition value with a bare wildcard", () => {
    for (const stage of ["direct"] as const) {
      for (const [stack, tpl] of synth(stage).templates) {
        for (const r of Object.values(resourcesOf(tpl, "AWS::IAM::Role"))) {
          const stmts = ((r.Properties as any).AssumeRolePolicyDocument.Statement as Stmt[]);
          for (const st of stmts) {
            for (const [op, block] of Object.entries(st.Condition ?? {})) {
              if (!/StringLike/.test(op)) continue;
              for (const v of Object.values(block as Record<string, unknown>).flat()) {
                const lead = String(v).split(/[*?]/)[0];
                expect(lead.length, `${stack} ${op} ${String(v)}`).toBeGreaterThanOrEqual(6);
              }
            }
          }
        }
      }
    }
  });

  // The second IAM principal a stranger's calls are made as. An IAM
  // role, never a user or access key; only the deploying principal (the Testacct Admin role) may
  // assume it, and it may only say who it is and read /v1 as a stranger would.
  test("the E2E second principal: a role only the deploying principal assumes, with read-only /v1 calls", () => {
    for (const stage of ["direct"] as const) {
      const { props, stmts, trust } = role(template(stage, "security"), `hamh-${stage}-e2e-other-principal`);
      expect(props.RoleName).toBe(`hamh-${stage}-e2e-other-principal`);
      expect(props.MaxSessionDuration).toBe(3600);
      expect(props.ManagedPolicyArns).toBeUndefined();
      expect(props.PermissionsBoundary).toBeUndefined();
      const tags = Object.fromEntries((props.Tags as { Key: string; Value: string }[]).map((x) => [x.Key, x.Value]));
      expect(tags).toMatchObject({ project: "hyperscale-agent-meta-harness", stage, owner: OWNER, "managed-by": "cdk" });
      // Admin sessions carry a source identity, and chaining one into a role needs sts:SetSourceIdentity.
      expect(trust).toEqual([{
        Sid: "DeployingPrincipalOnly", Effect: "Allow", Principal: { AWS: `arn:aws:iam::${ACCOUNT}:role/Admin` },
        Action: ["sts:AssumeRole", "sts:SetSourceIdentity"],
      }]);
      expect(stmts).toEqual([
        { Sid: "WhoAmI", Effect: "Allow", Action: "sts:GetCallerIdentity", Resource: "*" },
        { Sid: "ReadV1AsAStranger", Effect: "Allow", Action: "execute-api:Invoke", Resource: [
          `arn:aws:execute-api:${REGION}:${ACCOUNT}:*/${stage}/GET/v1/*`,
          `arn:aws:execute-api:${REGION}:${ACCOUNT}:*/${stage}/POST/v1/runs/*/artifacts/*/download-url`,
        ] },
      ]);
    }
  });

  test("no stack creates an IAM user or an access key", () => {
    for (const stage of ["direct"] as const) {
      for (const [stack, tpl] of synth(stage).templates) {
        for (const type of ["AWS::IAM::User", "AWS::IAM::AccessKey", "AWS::IAM::UserToGroupAddition"]) {
          expect(Object.keys(resourcesOf(tpl, type)), `${stack} ${type}`).toEqual([]);
        }
      }
    }
  });

  test("bedrock logging role: Bedrock in this account and Region may write only the invocation log group", () => {
    // Invocation logging in the home Region.
    const { props, stmts, trust } = role(t, "hamh-direct-bedrock-logging");
    expect(props.RoleName).toBe("hamh-direct-bedrock-logging");
    expect(trust).toHaveLength(1);
    expect(trust[0].Principal).toEqual({ Service: "bedrock.amazonaws.com" });
    expect(actions(trust[0])).toEqual(["sts:AssumeRole"]);
    expect(trust[0].Condition).toEqual({
      StringEquals: { "aws:SourceAccount": `${ACCOUNT}` },
      ArnLike: { "aws:SourceArn": `arn:aws:bedrock:${REGION}:${ACCOUNT}:*` },
    });
    expect(stmts.flatMap(actions).sort()).toEqual(["logs:CreateLogStream", "logs:PutLogEvents"]);
    for (const st of stmts) {
      expect(st.Resource).toBe(`arn:aws:logs:${REGION}:${ACCOUNT}:log-group:/hamh/direct/bedrock-invocations:log-stream:*`);
    }
  });
});

// The platform's metadata service hands the runtime base role's credentials to the engine's uid and
// the microVM grants no CAP_NET_ADMIN, so the role itself is the control. It allows exactly the actions below on exactly these scopes.
describe.each(["direct"] as const)("runtime base role scope: stage %s", (stage) => {
  const t = template(stage, "security");
  const { stmts } = role(t, `hamh-${stage}-runtime-base`);
  const allows = stmts.filter((s) => s.Effect === "Allow");
  const u = `hamh_${stage}`;
  const group = `arn:aws:logs:${REGION}:${ACCOUNT}:log-group:/aws/bedrock-agentcore/runtimes/${u}_*`;
  const repos = [`arn:aws:ecr:${REGION}:${ACCOUNT}:repository/hamh-direct-worker-python`];
  const scopeOf = (action: string) => allows.filter((s) => actions(s).includes(action)).map((s) => [s.Resource].flat());

  test("log writes reach the stage's own runtime log groups and streams only", () => {
    expect(scopeOf("logs:CreateLogGroup")).toEqual([[group]]);
    expect(scopeOf("logs:DescribeLogStreams")).toEqual([[group]]);
    expect(scopeOf("logs:CreateLogStream")).toEqual([[`${group}:log-stream:*`]]);
    expect(scopeOf("logs:PutLogEvents")).toEqual([[`${group}:log-stream:*`]]);
    // No statement names every runtime of the account and Region.
    expect(JSON.stringify(allows)).not.toContain("/aws/bedrock-agentcore/runtimes/*");
  });

  test("it can neither list the account's log groups nor write traces", () => {
    const all = allows.flatMap(actions);
    expect(all).not.toContain("logs:DescribeLogGroups");
    expect(all.filter((a) => a.startsWith("xray:"))).toEqual([]);
  });

  test("image pull is limited to the stage's runtime repositories; the registry token needs *", () => {
    expect(scopeOf("ecr:BatchGetImage")).toEqual([repos]);
    expect(scopeOf("ecr:GetDownloadUrlForLayer")).toEqual([repos]);
    expect(scopeOf("ecr:GetAuthorizationToken")).toEqual([["*"]]);
  });

  test("metrics stay in the bedrock-agentcore namespace", () => {
    const m = allows.filter((s) => actions(s).includes("cloudwatch:PutMetricData"));
    expect(m).toHaveLength(1);
    expect(m[0].Condition).toEqual({ StringEquals: { "cloudwatch:namespace": "bedrock-agentcore" } });
  });

  test("the whole allow list is exactly this set of actions", () => {
    expect([...new Set(allows.flatMap(actions))].sort()).toEqual([
      "cloudwatch:PutMetricData", "ecr:BatchGetImage", "ecr:GetAuthorizationToken", "ecr:GetDownloadUrlForLayer",
      "logs:CreateLogGroup", "logs:CreateLogStream", "logs:DescribeLogStreams", "logs:PutLogEvents",
    ]);
  });
});
