import { describe, expect, test } from "vitest";
import { buildApp } from "../lib/app.ts";
import {
  AGENTCORE_SEARCH_TOOL, carriesTavily, checkResearchProvider, DEFAULT_RESEARCH_PROVIDER, RESEARCH_PROVIDERS, type ResearchProvider, STAGE_KIND,
} from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { resourcesOf, synth, template } from "./helpers.ts";

// The research provider `auto` is the default of a stage. It searches with AgentCore Web Search (no key) until a Tavily key is stored in
// the stage's secret, and with Tavily first once one is; research-api makes that choice for every request. An `auto` stage therefore builds both backends: the
// AgentCore gateway with the Web Search Tool connector and the Tavily secret with its resource policy, and its research-api gets the settings and the grants of
// both. The four older words pin one provider and synthesize what they did before.
type Props = Record<string, any>;
type Stmt = Record<string, any>;
type Json = { Resources: Record<string, { Type: string; Properties?: Props }>; Outputs?: Record<string, unknown> };

const GATEWAY = "AWS::BedrockAgentCore::Gateway";
const TARGET = "AWS::BedrockAgentCore::GatewayTarget";
const SERVICE_ROLE = "hamh-direct-research-gateway";
const ALARM = "AWS::CloudWatch::Alarm";

const auto = template("direct", "control", { researchProvider: "auto" });
const byDefault = template("direct", "control");
const tavily = template("direct", "control", { researchProvider: "tavily" });
const agentcore = template("direct", "control", { researchProvider: "agentcore" });
const keyless = template("direct", "control", { researchProvider: "tavily-keyless" });

const fnProps = (t: typeof auto, name: string): Props => {
  const hits = Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as Props).filter((p) => p.FunctionName === name);
  expect(hits, name).toHaveLength(1);
  return hits[0];
};
const roleProps = (t: typeof auto, name: string): Props => {
  const hits = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as Props).filter((p) => p.RoleName === name);
  expect(hits, name).toHaveLength(1);
  return hits[0];
};
const statements = (t: typeof auto, role: string): Stmt[] => roleProps(t, role).Policies.flatMap((p: Props) => p.PolicyDocument.Statement);
const actions = (s: Stmt): string[] => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const idOfOnly = (t: typeof auto, type: string): string => {
  const ids = Object.keys(resourcesOf(t, type));
  expect(ids, type).toHaveLength(1);
  return ids[0];
};
const typesOf = (j: Json): string[] => Object.values(j.Resources).map((r) => r.Type);

describe("the setting and its default", () => {
  test("auto is a research provider, listed after the four older words", () => {
    expect([...RESEARCH_PROVIDERS]).toEqual(["tavily", "tavily-keyless", "fixture", "agentcore", "auto"]);
  });

  test("the default of a stage is auto", () => {
    expect(DEFAULT_RESEARCH_PROVIDER).toBe("auto");
  });

  test("auto is allowed on an official stage and on a test stage; the fixture is still refused on an official stage", () => {
    expect(() => checkResearchProvider("auto", "official")).not.toThrow();
    expect(() => checkResearchProvider("auto", "test")).not.toThrow();
    expect(() => checkResearchProvider("fixture", "official")).toThrow(/fixture.*official|official.*fixture/);
    expect(() => checkResearchProvider("fixture", "test")).not.toThrow();
  });

  test("an unknown word fails by name and lists the five words", () => {
    expect(() => checkResearchProvider("bing" as never, "test")).toThrow(/"bing" is not one of tavily, tavily-keyless, fixture, agentcore, auto/);
  });

  test("the CLI entry passes auto on, an unset or empty setting adds nothing, and a bad word stops the deploy by name", () => {
    const base = { HAMH_LAMBDA_DIR: new URL("./fixtures/lambda", import.meta.url).pathname };
    expect(entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "auto" }).researchProvider).toBe("auto");
    expect("researchProvider" in entryOptions(base)).toBe(false);
    expect("researchProvider" in entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: "" })).toBe(false);
    for (const bad of ["Auto", "AUTO", "auto ", "automatic", "bing"]) {
      expect(() => entryOptions({ ...base, HAMH_RESEARCH_PROVIDER: bad }), bad).toThrow(/HAMH_RESEARCH_PROVIDER must be one of tavily, tavily-keyless, fixture, agentcore, auto/);
    }
  });

  test("buildApp without a provider builds what auto builds, in all nine stacks", () => {
    const left = synth("direct").templates;
    const right = synth("direct", { researchProvider: "auto" }).templates;
    expect([...left.keys()]).toHaveLength(9);
    for (const [name, t] of left) expect(t.toJSON(), name).toEqual(right.get(name)!.toJSON());
    expect(byDefault.toJSON()).toEqual(auto.toJSON());
  });

  test("buildApp still refuses an unknown word", () => {
    expect(() => buildApp({ stage: "direct", researchProvider: "bing" as never })).toThrow(/research provider "bing"/);
  });
});

describe("an auto stage builds both backends", () => {
  test("it has the gateway, its target and its service role, like an agentcore stage", () => {
    expect(Object.keys(resourcesOf(auto, GATEWAY))).toHaveLength(1);
    expect(Object.keys(resourcesOf(auto, TARGET))).toHaveLength(1);
    expect(Object.values(resourcesOf(auto, "AWS::IAM::Role")).map((r) => (r.Properties as Props).RoleName)).toContain(SERVICE_ROLE);
    const out = Object.keys((auto.toJSON() as Json).Outputs ?? {});
    expect(out).toEqual(expect.arrayContaining(["ResearchGatewayId", "ResearchGatewayUrl", "ResearchGatewayTargetId"]));
  });

  test("it has the Tavily secret and its resource policy exactly as a tavily stage has them", () => {
    for (const type of ["AWS::SecretsManager::Secret", "AWS::SecretsManager::ResourcePolicy"]) {
      const a = Object.values(resourcesOf(auto, type));
      expect(a, type).toHaveLength(1);
      expect(a, type).toEqual(Object.values(resourcesOf(tavily, type)));
    }
  });

  test("only agentcore and auto build the gateway; no other word does", () => {
    const gateways = (t: typeof auto) => Object.keys(resourcesOf(t, GATEWAY)).length;
    expect(gateways(auto)).toBe(1);
    expect(gateways(agentcore)).toBe(1);
    expect(gateways(tavily)).toBe(0);
    expect(gateways(keyless)).toBe(0);
    if (STAGE_KIND === "test") expect(gateways(template("direct", "control", { researchProvider: "fixture" }))).toBe(0);
  });

  test("research-api gets the settings of both backends and RESEARCH_PROVIDER says auto", () => {
    const env = fnProps(auto, "hamh-direct-research-api").Environment.Variables;
    expect(Object.keys(env).sort()).toEqual([
      "AGENTCORE_GATEWAY_URL", "AGENTCORE_SEARCH_TOOL", "HAMH_STAGE", "HAMH_STAGE_KIND", "HARNESS_BUCKET", "RESEARCH_JOB_FUNCTION", "RESEARCH_PROVIDER",
      "RESEARCH_STAGE_RATE_PER_MIN", "RESEARCH_TABLE_NAME", "TAVILY_SECRET_ID", "WORKER_SESSION_ROLE_NAME",
    ]);
    expect(env.RESEARCH_PROVIDER).toBe("auto");
    expect(env.TAVILY_SECRET_ID).toBe("hamh-direct-research-tavily-api-key");
    expect(env.AGENTCORE_GATEWAY_URL).toEqual({ "Fn::GetAtt": [idOfOnly(auto, GATEWAY), "GatewayUrl"] });
    expect(env.AGENTCORE_SEARCH_TOOL).toBe(AGENTCORE_SEARCH_TOOL);
  });

  test("its role holds the grants of both backends and nothing wider: the same statements as an agentcore stage", () => {
    const st = statements(auto, "hamh-direct-research-api");
    expect(st.map((y) => y.Sid).sort()).toEqual(["DataKeyThroughDynamoDbAndS3", "InvokeSearchGateway", "OwnLogs", "ReadTavilyKey", "ResearchObjects", "ResearchTable", "StartResearchJob"]);
    expect(st).toEqual(statements(agentcore, "hamh-direct-research-api"));
    const invoke = st.find((y) => y.Sid === "InvokeSearchGateway")!;
    expect(actions(invoke)).toEqual(["bedrock-agentcore:InvokeGateway"]);
    expect(invoke.Resource).toEqual({ "Fn::GetAtt": [idOfOnly(auto, GATEWAY), "GatewayArn"] });
    const key = st.find((y) => y.Sid === "ReadTavilyKey")!;
    expect(actions(key)).toEqual(["secretsmanager:GetSecretValue"]);
    // The only statements that reach bedrock-agentcore or Secrets Manager are these two.
    expect(st.filter((y) => actions(y).some((a) => a.startsWith("bedrock-agentcore:"))).map((y) => y.Sid)).toEqual(["InvokeSearchGateway"]);
    expect(st.filter((y) => actions(y).some((a) => a.startsWith("secretsmanager:"))).map((y) => y.Sid)).toEqual(["ReadTavilyKey"]);
  });

  test("research-job and its role are what they are on every other stage, and both functions stay outside any VPC", () => {
    expect(fnProps(auto, "hamh-direct-research-job")).toEqual(fnProps(tavily, "hamh-direct-research-job"));
    expect(statements(auto, "hamh-direct-research-job")).toEqual(statements(tavily, "hamh-direct-research-job"));
    for (const name of ["hamh-direct-research-api", "hamh-direct-research-job"]) expect(fnProps(auto, name).VpcConfig, name).toBeUndefined();
  });

  test("the four Tavily key alarms exist on an auto stage, because it can call Tavily", () => {
    expect(carriesTavily("auto")).toBe(true);
    expect(Object.keys(resourcesOf(auto, ALARM))).toHaveLength(4);
  });
});

describe("what auto adds to a tavily stage and to an agentcore stage", () => {
  const a = auto.toJSON() as Json;

  test("compared with agentcore, auto differs by the provider word and the four alarms only", () => {
    const swapped = JSON.parse(JSON.stringify(a).replace('"RESEARCH_PROVIDER":"auto"', '"RESEARCH_PROVIDER":"agentcore"')) as Json;
    const g = agentcore.toJSON() as Json;
    const withoutAlarms = (j: Json): Json => ({ ...j, Resources: Object.fromEntries(Object.entries(j.Resources).filter(([, r]) => r.Type !== ALARM)) });
    expect(withoutAlarms(swapped)).toEqual(withoutAlarms(g));
    expect(typesOf(a).filter((x) => x === ALARM)).toHaveLength(4);
    expect(typesOf(g).filter((x) => x === ALARM)).toHaveLength(0);
  });

  test("compared with tavily, auto adds the gateway, its target and its role, three outputs, two settings and one statement, and removes nothing", () => {
    const t = tavily.toJSON() as Json;
    expect(Object.keys(t.Resources).filter((k) => !(k in a.Resources))).toEqual([]);
    expect(Object.keys(a.Resources).filter((k) => !(k in t.Resources)).map((k) => a.Resources[k].Type).sort()).toEqual([GATEWAY, TARGET, "AWS::IAM::Role"].sort());
    expect(Object.keys(a.Outputs ?? {}).filter((k) => !(k in (t.Outputs ?? {}))).sort()).toEqual(["ResearchGatewayId", "ResearchGatewayTargetId", "ResearchGatewayUrl"]);
    const envKeys = (tpl: typeof auto) => Object.keys(fnProps(tpl, "hamh-direct-research-api").Environment.Variables);
    expect(envKeys(auto).filter((k) => !envKeys(tavily).includes(k)).sort()).toEqual(["AGENTCORE_GATEWAY_URL", "AGENTCORE_SEARCH_TOOL"]);
    const sids = (tpl: typeof auto) => statements(tpl, "hamh-direct-research-api").map((y) => y.Sid);
    expect(sids(auto).filter((s) => !sids(tavily).includes(s))).toEqual(["InvokeSearchGateway"]);
  });

  test("seven stacks are identical to a tavily stage's, and the orchestration stack differs only in run-session's provider word", () => {
    const left = synth("direct", { researchProvider: "tavily" }).templates;
    const right = synth("direct", { researchProvider: "auto" }).templates;
    const same = [...left.keys()].filter((k) => k !== "hamh-direct-control" && k !== "hamh-direct-orchestration");
    expect(same).toHaveLength(7);
    for (const k of same) expect(right.get(k)!.toJSON(), k).toEqual(left.get(k)!.toJSON());
    const text = JSON.stringify(right.get("hamh-direct-orchestration")!.toJSON());
    expect(text).toContain('"RESEARCH_PROVIDER":"auto"');
    expect(JSON.parse(text.replace('"RESEARCH_PROVIDER":"auto"', '"RESEARCH_PROVIDER":"tavily"'))).toEqual(left.get("hamh-direct-orchestration")!.toJSON());
  });
});

describe("the four older words synthesize what they did before", () => {
  const providers: ResearchProvider[] = ["tavily", "tavily-keyless", "agentcore", ...(STAGE_KIND === "test" ? (["fixture"] as ResearchProvider[]) : [])];

  test.each(providers)("%s: the gateway, the alarms and research-api's environment follow the word and no other", (word) => {
    const t = template("direct", "control", { researchProvider: word });
    const env = fnProps(t, "hamh-direct-research-api").Environment.Variables;
    expect(env.RESEARCH_PROVIDER).toBe(word);
    const gateway = word === "agentcore";
    expect(Object.keys(resourcesOf(t, GATEWAY)).length).toBe(gateway ? 1 : 0);
    expect(Object.keys(env).filter((k) => k.startsWith("AGENTCORE_")).length).toBe(gateway ? 2 : 0);
    expect(statements(t, "hamh-direct-research-api").map((y) => y.Sid).includes("InvokeSearchGateway")).toBe(gateway);
    expect(Object.keys(resourcesOf(t, ALARM)).length).toBe(word === "tavily" ? 4 : 0);
    // The key's secret and its policy exist for every word.
    expect(Object.keys(resourcesOf(t, "AWS::SecretsManager::Secret"))).toHaveLength(1);
    expect(statements(t, "hamh-direct-research-api").map((y) => y.Sid)).toContain("ReadTavilyKey");
  });

  test("a tavily stage keeps the nine settings of research-api and run-session's tavily word", () => {
    const env = fnProps(tavily, "hamh-direct-research-api").Environment.Variables;
    expect(Object.keys(env).sort()).toEqual([
      "HAMH_STAGE", "HAMH_STAGE_KIND", "HARNESS_BUCKET", "RESEARCH_JOB_FUNCTION", "RESEARCH_PROVIDER", "RESEARCH_STAGE_RATE_PER_MIN", "RESEARCH_TABLE_NAME",
      "TAVILY_SECRET_ID", "WORKER_SESSION_ROLE_NAME",
    ]);
    const orch = JSON.stringify(synth("direct", { researchProvider: "tavily" }).templates.get("hamh-direct-orchestration")!.toJSON());
    expect(orch).toContain('"RESEARCH_PROVIDER":"tavily"');
  });
});
