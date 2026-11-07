import { describe, expect, test, vi } from "vitest";
import { buildApp } from "../lib/app.ts";
import {
  ACCOUNT, AGENTCORE_SEARCH_TOOL, checkResearchProvider, DEFAULT_RESEARCH_PROVIDER, KEEP_TAGS, names, OWNER, PROJECT, REGION, RESEARCH_PROVIDERS, stageConfig,
  WEB_SEARCH_TARGET,
} from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { THREE_ACCOUNTS, underEntry } from "./accounts-three.ts";
import { resourcesOf, synth, template, withCdkJsonContext } from "./helpers.ts";

type Props = Record<string, any>;
type Stmt = Record<string, any>;
const GATEWAY = "AWS::BedrockAgentCore::Gateway";
const TARGET = "AWS::BedrockAgentCore::GatewayTarget";
const SERVICE_ROLE = "hamh-direct-research-gateway";
const FOUR_TAGS = { project: PROJECT, stage: "direct", owner: OWNER, "managed-by": "cdk" };

const withGateway = template("direct", "control", { researchProvider: "agentcore" });
// The baseline of the comparisons below is the tavily provider.
const byTavily = template("direct", "control", { researchProvider: "tavily" });
const only = (t: typeof withGateway, type: string): [string, Props] => {
  const hits = Object.entries(resourcesOf(t, type));
  expect(hits, type).toHaveLength(1);
  return [hits[0][0], (hits[0][1] as { Properties: Props }).Properties];
};
const roleProps = (t: typeof withGateway, name: string): Props => {
  const hits = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as Props).filter((p) => p.RoleName === name);
  expect(hits, name).toHaveLength(1);
  return hits[0];
};
const tagMap = (tags: unknown): Record<string, string> =>
  Array.isArray(tags) ? Object.fromEntries(tags.map((x: { Key: string; Value: string }) => [x.Key, x.Value])) : ((tags as Record<string, string>) ?? {});
const actions = (s: Stmt): string[] => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const resourcesOfStmt = (s: Stmt): unknown[] => (Array.isArray(s.Resource) ? s.Resource : [s.Resource]);

// The `agentcore` search provider of the research route. research-api signs a call to an AgentCore Gateway that carries the Web Search Tool
// connector, so the stage needs a gateway, its service role and one target, and research-api needs the gateway's URL and the right to invoke it.
// The gateway exists only for a stage that selects the provider (HAMH_RESEARCH_PROVIDER=agentcore) or leaves the choice to `auto`; the tavily and
// tavily-keyless words synthesize what they did before.

describe("the agentcore provider setting", () => {
  test("it is a research provider, and the default of a stage is auto", () => {
    expect([...RESEARCH_PROVIDERS]).toContain("agentcore");
    expect(DEFAULT_RESEARCH_PROVIDER).toBe("auto");
  });

  test("it is allowed on an official stage and on a test stage (it is keyless and stays inside AWS)", () => {
    expect(() => checkResearchProvider("agentcore", "official")).not.toThrow();
    expect(() => checkResearchProvider("agentcore", "test")).not.toThrow();
  });

  test("an unknown word fails by name and lists the five words", () => {
    expect(() => checkResearchProvider("bing" as never, "test")).toThrow(/"bing" is not one of tavily, tavily-keyless, fixture, agentcore, auto/);
  });

  test("the CLI entry passes the word on, and an unset or empty setting adds nothing", () => {
    const base = { HAMH_LAMBDA_DIR: new URL("./fixtures/lambda", import.meta.url).pathname };
    expect(entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "agentcore" }).researchProvider).toBe("agentcore");
    expect("researchProvider" in entryOptions(base)).toBe(false);
    expect("researchProvider" in entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "" })).toBe(false);
  });

  test("buildApp builds the nine stacks for it, and still refuses an unknown word", () => {
    const app = buildApp({ stage: "direct", researchProvider: "agentcore" });
    expect(app.node.children.filter((c) => "templateOptions" in c)).toHaveLength(9);
    expect(() => buildApp({ stage: "direct", researchProvider: "bing" as never })).toThrow(/research provider "bing"/);
  });
});

describe("the names of the gateway", () => {
  const cfg = stageConfig("direct");

  test("the gateway is hamh-direct-websearch, its service role hamh-direct-research-gateway, its target web-search-tool", () => {
    expect(names.researchGateway(cfg)).toBe("hamh-direct-websearch");
    expect(names.role(cfg, "research-gateway")).toBe("hamh-direct-research-gateway");
    expect(WEB_SEARCH_TARGET).toBe("web-search-tool");
  });

  test("the tool name is the target name, three underscores and the connector's tool name (measured on a real gateway)", () => {
    expect(AGENTCORE_SEARCH_TOOL).toBe("web-search-tool___WebSearch");
    expect(AGENTCORE_SEARCH_TOOL).toBe(`${WEB_SEARCH_TARGET}___WebSearch`);
  });

  test("the gateway and target names fit the service pattern ([0-9a-zA-Z][-]?){1,100}, and the gateway name is lower case", () => {
    expect(names.researchGateway(cfg)).toMatch(/^([0-9a-zA-Z]-?){1,100}$/);
    expect(WEB_SEARCH_TARGET).toMatch(/^([0-9a-zA-Z]-?){1,100}$/);
    // The gateway id is "<name>-<10 lower-case characters>", so a lower-case name keeps the service role's
    // ARN pattern gateway/<name>-* true for it.
    expect(names.researchGateway(cfg)).toBe(names.researchGateway(cfg).toLowerCase());
  });
});

describe("the gateway, its target and its service role (provider agentcore)", () => {
  test("a stage on the tavily provider has none of them, and no custom resource or extra function appears for them", () => {
    expect(resourcesOf(byTavily, GATEWAY)).toEqual({});
    expect(resourcesOf(byTavily, TARGET)).toEqual({});
    expect(Object.values(resourcesOf(byTavily, "AWS::IAM::Role")).map((r) => (r.Properties as Props).RoleName)).not.toContain(SERVICE_ROLE);
    // The target is CloudFormation's own resource type: no custom resource, no provider function.
    expect(resourcesOf(withGateway, "Custom::AWS")).toEqual({});
    const fnNames = (t: typeof withGateway) => Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => (r.Properties as Props).FunctionName).sort();
    expect(fnNames(withGateway)).toEqual(fnNames(byTavily));
  });

  test("the control stack holds exactly one MCP gateway with AWS_IAM authorization, named hamh-direct-websearch, on the service role", () => {
    const [, gw] = only(withGateway, GATEWAY);
    const [roleId] = Object.entries(resourcesOf(withGateway, "AWS::IAM::Role")).find(([, r]) => (r.Properties as Props).RoleName === SERVICE_ROLE)!;
    expect(gw.Name).toBe("hamh-direct-websearch");
    expect(gw.ProtocolType).toBe("MCP");
    expect(gw.AuthorizerType).toBe("AWS_IAM");
    expect(gw.AuthorizerConfiguration).toBeUndefined();
    expect(gw.RoleArn).toEqual({ "Fn::GetAtt": [roleId, "Arn"] });
    // No debug exception level, no custom key, no interceptor, no policy engine: the measured gateway had none and the default is the safe one.
    for (const key of ["ExceptionLevel", "KmsKeyArn", "InterceptorConfigurations", "PolicyEngineConfiguration", "WafConfiguration"]) {
      expect(gw[key], key).toBeUndefined();
    }
  });

  test("the target is the Web Search Tool connector with the gateway's own IAM role as its credential", () => {
    const [gwId] = only(withGateway, GATEWAY);
    const [, target] = only(withGateway, TARGET);
    expect(target.Name).toBe("web-search-tool");
    expect(target.GatewayIdentifier).toEqual({ "Fn::GetAtt": [gwId, "GatewayIdentifier"] });
    const connector = target.TargetConfiguration.Mcp.Connector;
    // Tripwire: CloudFormation's ConnectorSource has one property, ConnectorId (no version), so the target is created at the connector's default version
    // and keeps it (an update without a version is sticky; docs "Connector versions"). The version is read back after a deploy with get-gateway-target.
    // If CloudFormation ever adds a version property, pin 1.2.0 or later here and say so in infra/AGENTS.md.
    expect(connector.Source).toEqual({ ConnectorId: "web-search" });
    expect(connector.Configurations).toHaveLength(1);
    expect(connector.Configurations[0].Name).toBe("WebSearch");
    expect(connector.Enabled).toBeUndefined();
    expect(target.CredentialProviderConfigurations).toEqual([{ CredentialProviderType: "GATEWAY_IAM_ROLE" }]);
    for (const key of ["PrivateEndpoint", "MetadataConfiguration"]) expect(target[key], key).toBeUndefined();
  });

  test("the service role trusts AgentCore for this account and this Region's gateways only", () => {
    const role = roleProps(withGateway, SERVICE_ROLE);
    expect(role.AssumeRolePolicyDocument.Statement).toEqual([{
      Effect: "Allow", Principal: { Service: "bedrock-agentcore.amazonaws.com" }, Action: "sts:AssumeRole",
      Condition: {
        StringEquals: { "aws:SourceAccount": ACCOUNT },
        ArnLike: { "aws:SourceArn": `arn:aws:bedrock-agentcore:${REGION}:${ACCOUNT}:gateway/*` },
      },
    }]);
    expect(role.ManagedPolicyArns).toBeUndefined();
    expect(role.PermissionsBoundary).toBeUndefined();
  });

  test("the service role allows GetGateway on this gateway's name and InvokeWebSearch on the service-owned tool, and nothing else", () => {
    const role = roleProps(withGateway, SERVICE_ROLE);
    expect(role.Policies).toHaveLength(1);
    const st: Stmt[] = role.Policies[0].PolicyDocument.Statement;
    expect(st.map((x) => x.Sid).sort()).toEqual(["GetGateway", "InvokeWebSearch"]);
    const get = st.find((x) => x.Sid === "GetGateway")!;
    expect(get.Effect).toBe("Allow");
    expect(actions(get)).toEqual(["bedrock-agentcore:GetGateway"]);
    // The role cannot name the gateway's own ARN (the gateway needs the role first: a CloudFormation cycle), so it names the gateway by the id shape
    // <name>-<10 characters> that the service gives.
    expect(resourcesOfStmt(get)).toEqual([`arn:aws:bedrock-agentcore:${REGION}:${ACCOUNT}:gateway/hamh-direct-websearch-*`]);
    const search = st.find((x) => x.Sid === "InvokeWebSearch")!;
    expect(search.Effect).toBe("Allow");
    expect(actions(search)).toEqual(["bedrock-agentcore:InvokeWebSearch"]);
    expect(resourcesOfStmt(search)).toEqual([`arn:aws:bedrock-agentcore:${REGION}:aws:tool/web-search.v1*`]);
    for (const x of st) {
      expect(x.Condition, x.Sid).toBeUndefined();
      expect(JSON.stringify(x.Resource), x.Sid).not.toBe('"*"');
      for (const a of actions(x)) expect(a, x.Sid).toMatch(/^bedrock-agentcore:[A-Za-z]+$/);
    }
  });

  test("the gateway and the service role carry the four project tags (the target takes no tags)", () => {
    const [, gw] = only(withGateway, GATEWAY);
    expect(tagMap(gw.Tags)).toMatchObject(FOUR_TAGS);
    expect(tagMap(roleProps(withGateway, SERVICE_ROLE).Tags)).toMatchObject(FOUR_TAGS);
    const [, target] = only(withGateway, TARGET);
    expect(target.Tags).toBeUndefined();
    // On the maintainers' test account the keep tag rides along; on the others there is none.
    for (const props of [gw, roleProps(withGateway, SERVICE_ROLE)]) {
      const tags = tagMap(props.Tags);
      if (Object.keys(KEEP_TAGS).length > 0) expect(tags).toMatchObject(KEEP_TAGS);
      else expect(tags).not.toHaveProperty("auto-delete");
    }
  });

  test("the control stack prints the gateway id, its URL and the target id for the post-deploy check, only for this provider", () => {
    const out = (t: typeof withGateway) => Object.keys((t.toJSON() as { Outputs?: Record<string, unknown> }).Outputs ?? {});
    expect(out(withGateway)).toEqual(expect.arrayContaining(["ResearchGatewayId", "ResearchGatewayUrl", "ResearchGatewayTargetId"]));
    expect(out(byTavily).filter((k) => k.startsWith("ResearchGateway"))).toEqual([]);
  });
});

describe("research-api calls the gateway (provider agentcore)", () => {
  const fnProps = (t: typeof withGateway, name: string): Props => {
    const hits = Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as Props).filter((p) => p.FunctionName === name);
    expect(hits, name).toHaveLength(1);
    return hits[0];
  };
  const statements = (t: typeof withGateway, role: string): Stmt[] => roleProps(t, role).Policies.flatMap((p: Props) => p.PolicyDocument.Statement);
  const gatewayId = () => only(withGateway, GATEWAY)[0];

  test("its environment gains the gateway's URL and the search tool's name, and RESEARCH_PROVIDER says agentcore", () => {
    const env = fnProps(withGateway, "hamh-direct-research-api").Environment.Variables;
    expect(Object.keys(env).sort()).toEqual([
      "AGENTCORE_GATEWAY_URL", "AGENTCORE_SEARCH_TOOL", "HAMH_STAGE", "HAMH_STAGE_KIND", "HARNESS_BUCKET", "RESEARCH_JOB_FUNCTION", "RESEARCH_PROVIDER",
      "RESEARCH_STAGE_RATE_PER_MIN", "RESEARCH_TABLE_NAME", "TAVILY_SECRET_ID", "WORKER_SESSION_ROLE_NAME",
    ]);
    expect(env.RESEARCH_PROVIDER).toBe("agentcore");
    // The gateway's own URL attribute: it ends /mcp, the path research-api posts to (measured).
    expect(env.AGENTCORE_GATEWAY_URL).toEqual({ "Fn::GetAtt": [gatewayId(), "GatewayUrl"] });
    expect(env.AGENTCORE_SEARCH_TOOL).toBe(AGENTCORE_SEARCH_TOOL);
    expect(env.AGENTCORE_SEARCH_TOOL).toBe("web-search-tool___WebSearch");
  });

  test("its role may invoke this gateway and no other: bedrock-agentcore:InvokeGateway on the gateway's ARN", () => {
    const st = statements(withGateway, "hamh-direct-research-api");
    const x = st.find((y) => y.Sid === "InvokeSearchGateway")!;
    expect(x).toBeDefined();
    expect(x.Effect).toBe("Allow");
    expect(actions(x)).toEqual(["bedrock-agentcore:InvokeGateway"]);
    expect(resourcesOfStmt(x)).toEqual([{ "Fn::GetAtt": [gatewayId(), "GatewayArn"] }]);
    expect(x.Condition).toBeUndefined();
    // Nothing else of the role changed: the same six statements as before, plus this one.
    expect(st.map((y) => y.Sid).sort()).toEqual(["DataKeyThroughDynamoDbAndS3", "InvokeSearchGateway", "OwnLogs", "ReadTavilyKey", "ResearchObjects", "ResearchTable", "StartResearchJob"]);
    // Only this one statement reaches bedrock-agentcore, and it is not a wildcard action.
    const agentcore = st.filter((y) => actions(y).some((a) => a.startsWith("bedrock-agentcore:")));
    expect(agentcore.map((y) => y.Sid)).toEqual(["InvokeSearchGateway"]);
  });

  test("fetch and repository downloads stay on Tavily and GitHub: the Tavily key, its grant and the job function are unchanged", () => {
    const env = fnProps(withGateway, "hamh-direct-research-api").Environment.Variables;
    expect(env.TAVILY_SECRET_ID).toBe("hamh-direct-research-tavily-api-key");
    const st = statements(withGateway, "hamh-direct-research-api");
    const key = st.find((y) => y.Sid === "ReadTavilyKey")!;
    expect(actions(key)).toEqual(["secretsmanager:GetSecretValue"]);
    expect(fnProps(withGateway, "hamh-direct-research-job")).toEqual(fnProps(byTavily, "hamh-direct-research-job"));
    expect(statements(withGateway, "hamh-direct-research-job")).toEqual(statements(byTavily, "hamh-direct-research-job"));
  });

  test("the tavily provider's research-api has neither new setting nor the new statement", () => {
    const env = fnProps(byTavily, "hamh-direct-research-api").Environment.Variables;
    expect(Object.keys(env).filter((k) => k.startsWith("AGENTCORE_"))).toEqual([]);
    expect(env.RESEARCH_PROVIDER).toBe("tavily");
    expect(statements(byTavily, "hamh-direct-research-api").map((y) => y.Sid)).not.toContain("InvokeSearchGateway");
  });

  test("research-api stays outside any VPC, so it reaches the gateway's public host without an endpoint", () => {
    for (const name of ["hamh-direct-research-api", "hamh-direct-research-job"]) {
      expect(fnProps(withGateway, name).VpcConfig, name).toBeUndefined();
    }
  });
});

describe("nothing else of the stage changes for the agentcore provider", () => {
  const plain = synth("direct", { researchProvider: "tavily" }).templates;
  const agent = synth("direct", { researchProvider: "agentcore" }).templates;

  test("seven of the nine templates are identical to the tavily provider's, the network and security templates among them", () => {
    const same = [...plain.keys()].filter((k) => k !== "hamh-direct-control" && k !== "hamh-direct-orchestration");
    expect(same).toHaveLength(7);
    expect(same).toEqual(expect.arrayContaining(["hamh-direct-network", "hamh-direct-security"]));
    for (const k of same) expect(agent.get(k)!.toJSON(), k).toEqual(plain.get(k)!.toJSON());
  });

  test("the orchestration template differs in one word: run-session's RESEARCH_PROVIDER", () => {
    const text = JSON.stringify(agent.get("hamh-direct-orchestration")!.toJSON());
    expect(text).toContain('"RESEARCH_PROVIDER":"agentcore"');
    expect(JSON.parse(text.replace('"RESEARCH_PROVIDER":"agentcore"', '"RESEARCH_PROVIDER":"tavily"'))).toEqual(plain.get("hamh-direct-orchestration")!.toJSON());
  });

  test("the worker session role and the Network Firewall allowlist gain nothing: no gateway action, no gateway host", () => {
    // The base role of the runtime already DENIES InvokeGateway (BASE_ROLE_DENIES); no statement of the security template may Allow it or the search action.
    const allowed = (agent.get("hamh-direct-security")!.toJSON() as { Resources: Record<string, { Properties?: Props }> }).Resources;
    const stmts: Stmt[] = [];
    const walk = (o: unknown): void => {
      if (Array.isArray(o)) o.forEach(walk);
      else if (o && typeof o === "object") {
        const rec = o as Stmt;
        if (rec.Effect !== undefined && rec.Action !== undefined) stmts.push(rec);
        Object.values(rec).forEach(walk);
      }
    };
    walk(allowed);
    expect(stmts.length).toBeGreaterThan(10);
    for (const x of stmts.filter((y) => y.Effect === "Allow")) {
      for (const a of actions(x)) expect(a, JSON.stringify(x.Sid)).not.toMatch(/^bedrock-agentcore:(InvokeGateway|InvokeWebSearch)$/);
    }
    const network = JSON.stringify(agent.get("hamh-direct-network")!.toJSON());
    expect(network).not.toMatch(/websearch|InvokeWebSearch/);
    const groups = Object.values(resourcesOf(agent.get("hamh-direct-network")!, "AWS::NetworkFirewall::RuleGroup")).map((r) => JSON.stringify(r.Properties));
    expect(groups.length).toBeGreaterThan(0);
    for (const g of groups) expect(g).not.toMatch(/gateway\.bedrock-agentcore/);
  });

  test("the control stack differs from the tavily provider's only by the gateway's resources, outputs, one setting and one statement, and the four Tavily alarms the tavily provider has", () => {
    const a = agent.get("hamh-direct-control")!.toJSON() as { Resources: Record<string, { Type: string }>; Outputs?: Record<string, unknown> };
    const b = plain.get("hamh-direct-control")!.toJSON() as { Resources: Record<string, { Type: string }>; Outputs?: Record<string, unknown> };
    const added = Object.keys(a.Resources).filter((k) => !(k in b.Resources)).map((k) => a.Resources[k].Type).sort();
    expect(added).toEqual(["AWS::BedrockAgentCore::Gateway", "AWS::BedrockAgentCore::GatewayTarget", "AWS::IAM::Role"]);
    // Only a stage that can call Tavily has the four alarms on the stored key, so the agentcore stage lacks them.
    expect(Object.keys(b.Resources).filter((k) => !(k in a.Resources)).map((k) => b.Resources[k].Type)).toEqual(Array(4).fill("AWS::CloudWatch::Alarm"));
    expect(Object.keys(a.Outputs ?? {}).filter((k) => !(k in (b.Outputs ?? {}))).sort()).toEqual(["ResearchGatewayId", "ResearchGatewayTargetId", "ResearchGatewayUrl"]);
  });
});

// Each entry of the three-entry fixture (infra/test/accounts-three.ts) synthesizes the gateway with its own account and Region; only the entry with role "test" (an
// account whose cleanup service deletes an untagged resource after about 7 days) gets the keep tag auto-delete=no. A fresh module graph per entry: config.ts reads
// HAMH_ACCOUNTS_FILE and HAMH_AWS_PROFILE when it loads. The role decides, as in lib/config.ts keepTags.
const DOC = THREE_ACCOUNTS;
const ENTRIES: { profile: string; role: string }[] = DOC.accounts;
describe.each(ENTRIES.map((e) => [e.profile, e.role === "test"] as const))("the gateway synthesized for HAMH_AWS_PROFILE=%s", (profile, keeps) => {
  const built = async () => {
    const entry = DOC.accounts.find((e: { profile: string }) => e.profile === profile)!;
    return underEntry(profile, async () => {
      vi.resetModules();
      const { buildApp } = await import("../lib/app.ts");
      const { Template } = await import("aws-cdk-lib/assertions");
      const app = withCdkJsonContext(() => buildApp({ stage: "direct", researchProvider: "agentcore" }));
      const stack = app.node.children.find((c) => "templateOptions" in c && (c as { stackName?: string }).stackName === "hamh-direct-control") as any;
      return { entry, json: Template.fromStack(stack).toJSON() as { Resources: Record<string, { Type: string; Properties: Props }> }, stackTags: app.synth().stacks.find((x: any) => x.stackName === "hamh-direct-control")!.tags };
    });
  };

  test("it uses the entry's account and Region, in the role's trust, its two resources and the caller's grant", async () => {
    const { entry, json } = await built();
    const text = JSON.stringify(json);
    const role = Object.values(json.Resources).find((r) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === SERVICE_ROLE)!;
    const doc = JSON.stringify(role.Properties);
    expect(doc).toContain(`"aws:SourceAccount":"${entry.account}"`);
    expect(doc).toContain(`arn:aws:bedrock-agentcore:${entry.region}:${entry.account}:gateway/*`);
    expect(doc).toContain(`arn:aws:bedrock-agentcore:${entry.region}:${entry.account}:gateway/hamh-direct-websearch-*`);
    expect(doc).toContain(`arn:aws:bedrock-agentcore:${entry.region}:aws:tool/web-search.v1*`);
    // No other Region or account id appears in any gateway ARN of the template.
    for (const m of text.matchAll(/arn:aws:bedrock-agentcore:([a-z0-9-]+):([0-9a-z]+):/g)) {
      expect([m[1], m[2] === "aws" ? entry.account : m[2]], m[0]).toEqual([entry.region, entry.account]);
    }
  });

  test(keeps ? "the gateway, its role and the control stack carry the keep tag auto-delete=no" : "no keep tag appears on any of them", async () => {
    const { json, stackTags } = await built();
    const gw = Object.values(json.Resources).find((r) => r.Type === GATEWAY)!;
    const role = Object.values(json.Resources).find((r) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === SERVICE_ROLE)!;
    for (const [label, tags] of [["gateway", tagMap(gw.Properties.Tags)], ["role", tagMap(role.Properties.Tags)]] as const) {
      expect(tags, label).toMatchObject(FOUR_TAGS);
      if (keeps) expect(tags["auto-delete"], label).toBe("no");
      else expect(tags, label).not.toHaveProperty("auto-delete");
    }
    if (keeps) expect(stackTags["auto-delete"]).toBe("no");
    else expect(stackTags).not.toHaveProperty("auto-delete");
  });
});
