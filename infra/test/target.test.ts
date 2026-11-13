// The account, Region and AZs the app targets: the accounts.json entry HAMH_AWS_PROFILE selects.
// Unset or empty selects the one
// active entry (us-east-1); the history profile selects the history entry (us-west-2), whose deployed values stay exactly as they were;
// the test profile selects the test entry (us-east-1), whose us-east-1a is the AgentCore AZ ID use1-az1.
// The committed accounts.json holds one neutral entry, so the three roles come from the fixture of infra/test/accounts-three.ts.
import { describe, expect, test, vi } from "vitest";
import * as config from "../lib/config.ts";
import { egressDomains } from "../lib/stacks/network.ts";
import { ACTIVE, HISTORY, TEST_ENTRY, THREE_ACCOUNTS as DOC, underEntry } from "./accounts-three.ts";
const core = (t: config.Target) => ({ account: t.account, region: t.region, profile: t.profile, role: t.role });
const MANTLE_AND_OSV = ["bedrock-mantle.us-west-2.api.aws", "bedrock-mantle.us-east-1.api.aws", "bedrock-mantle.us-east-2.api.aws",
  "api.osv.dev"];

describe("selectTarget", () => {
  test("unset or empty HAMH_AWS_PROFILE selects the active entry; AWS_PROFILE never selects", () => {
    for (const env of [{}, { HAMH_AWS_PROFILE: "" }, { AWS_PROFILE: HISTORY.profile }]) {
      const t = config.selectTarget(DOC, env);
      expect(core(t)).toEqual(ACTIVE);
      // The active entry maps us-east-1b to use1-az1, an AgentCore VPC AZ ID (us-east-1a is use1-az6 in an account like it).
      expect(t.azs.direct).toEqual(["us-east-1b"]);
      expect(t.azIds.direct).toEqual(["use1-az1"]);
    }
  });

  test("the history profile selects the history entry with its deployed AZs", () => {
    const t = config.selectTarget(DOC, { HAMH_AWS_PROFILE: HISTORY.profile });
    expect(core(t)).toEqual(HISTORY);
    expect(t.azs.direct).toEqual(["us-west-2a"]);
  });

  test("the test profile selects the test entry; its us-east-1a is use1-az1 and no logging setting is written", () => {
    const t = config.selectTarget(DOC, { HAMH_AWS_PROFILE: TEST_ENTRY.profile });
    expect(core(t)).toEqual(TEST_ENTRY);
    expect(t.azs.direct).toEqual(["us-east-1a"]);
    expect(t.azIds.direct).toEqual(["use1-az1"]);
    expect(t.invocationLogging).toBe("read-only");
    // The test entry is never the default and never stands in for the active one.
    expect(core(config.selectTarget(DOC, {}))).toEqual(ACTIVE);
    expect(DOC.accounts.filter((e: any) => e.role === "active")).toHaveLength(1);
  });

  test("an unknown profile is refused with the known profiles named", () => {
    expect(() => config.selectTarget(DOC, { HAMH_AWS_PROFILE: "default" })).toThrow(new RegExp(`HAMH_AWS_PROFILE=default.*${HISTORY.profile}, ${ACTIVE.profile}, ${TEST_ENTRY.profile}`));
  });

  test("a test entry is accepted beside one active entry and is not an active one", () => {
    const entry = (over: Record<string, unknown> = {}) => ({ ...DOC.accounts[1], ...over });
    const doc = (accounts: unknown[]) => ({ ...DOC, accounts });
    const t = config.selectTarget(doc([entry(), entry({ role: "test", profile: "x", account: "111111111111" })]), { HAMH_AWS_PROFILE: "x" });
    expect(t.role).toBe("test");
    expect(() => config.selectTarget(doc([entry({ role: "test" })]), {})).toThrow(/exactly one active/);
    expect(() => config.selectTarget(doc([entry({ role: "primary" })]), {})).toThrow(/role must be one of active, history, test/);
  });

  test("a file without exactly one active entry, or with a repeated profile or an AZ outside its Region, is refused", () => {
    const entry = (over: Record<string, unknown> = {}) => ({ ...DOC.accounts[1], ...over });
    const doc = (accounts: unknown[]) => ({ ...DOC, accounts });
    expect(() => config.selectTarget(doc([entry(), entry({ profile: "x", account: "111111111111" })]), {})).toThrow(/exactly one active/);
    expect(() => config.selectTarget(doc([entry({ role: "history" })]), {})).toThrow(/exactly one active/);
    expect(() => config.selectTarget(doc([entry(), entry({ role: "history", account: "111111111111" })]), {})).toThrow(new RegExp(`profile ${ACTIVE.profile} twice`));
    expect(() => config.selectTarget(doc([entry({ azs: { direct: ["us-west-2a"] } })]), {})).toThrow(/us-west-2a is not in us-east-1/);
    expect(() => config.selectTarget(doc([entry({ azIds: { direct: ["use1-az1", "use1-az2"] } })]), {})).toThrow(/pair up/);
    // Stage direct: a file that names only the retired stages live and drill is refused, as the Rust and Python parsers refuse it.
    expect(() => config.selectTarget(doc([entry({ azs: { live: ["us-east-1b"], drill: ["us-east-1b"] },
      azIds: { live: ["use1-az1"], drill: ["use1-az1"] } })]), {})).toThrow(/azs.direct and azIds.direct must pair up/);
    expect(() => config.selectTarget({ ...DOC, selector: "AWS_PROFILE" }, {})).toThrow(/selector/);
  });
});

describe("the module values follow the process's selection", () => {
  test("ACCOUNT, REGION, PROFILE and the stage AZs are the selected entry's", () => {
    // The module reads the file the process names (the committed accounts.json unless HAMH_ACCOUNTS_FILE says otherwise), not the fixture.
    const t = config.selectTarget(config.loadAccounts(process.env), process.env);
    expect([config.ACCOUNT, config.REGION, config.PROFILE]).toEqual([t.account, t.region, t.profile]);
    expect(config.stageConfig("direct").azs).toEqual(t.azs.direct);
    expect(config.REMOTE_INFERENCE_REGIONS).toEqual(config.INFERENCE_REGIONS.filter((r) => r !== t.region));
  });

  test("the inference Regions stay the criteria set in criteria order", () => {
    expect([...config.INFERENCE_REGIONS]).toEqual(["us-west-2", "us-east-1", "us-east-2"]);
    expect(config.remoteInferenceRegions("us-west-2")).toEqual(["us-east-1", "us-east-2"]);
    expect(config.remoteInferenceRegions("us-east-1")).toEqual(["us-west-2", "us-east-2"]);
    expect(() => config.remoteInferenceRegions("eu-west-1")).toThrow(/inference Region/);
  });
});

describe("egress allowlist per home Region", () => {
  test("the history entry's list (us-west-2) is byte-identical to the list it deployed", () => {
    expect(egressDomains("us-west-2")).toEqual([
      "bedrock-runtime.us-east-1.amazonaws.com", "bedrock-runtime.us-east-2.amazonaws.com", ...MANTLE_AND_OSV]);
  });

  test("the active entry's list (us-east-1) names bedrock-runtime for the two other inference Regions; the home one uses its endpoint", () => {
    expect(egressDomains("us-east-1")).toEqual([
      "bedrock-runtime.us-west-2.amazonaws.com", "bedrock-runtime.us-east-2.amazonaws.com", ...MANTLE_AND_OSV]);
  });
});

// A fresh module graph per selection: config.ts reads HAMH_ACCOUNTS_FILE and HAMH_AWS_PROFILE when it loads.
async function underProfile(profile: string) {
  return underEntry(profile, async () => {
    vi.resetModules();
    const { buildApp } = await import("../lib/app.ts");
    const { Template } = await import("aws-cdk-lib/assertions");
    return { buildApp, Template };
  });
}

describe.each([
  [ACTIVE.profile, ACTIVE, ["us-east-1b"]],
  [HISTORY.profile, HISTORY, ["us-west-2a"]],
  [TEST_ENTRY.profile, TEST_ENTRY, ["us-east-1a"]],
])("synth with HAMH_AWS_PROFILE=%s", (profile, want, directAzs) => {
  test("every stack's env, the subnet AZs and the firewall allowlist follow the entry", async () => {
    const { buildApp, Template } = await underProfile(profile);
    for (const [stage, azs] of [["direct", directAzs]] as const) {
      const app = buildApp({ stage });
      const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
      expect(stacks).toHaveLength(9);
      for (const s of stacks) {
        expect([s.stackName, s.account, s.region]).toEqual([s.stackName, want.account, want.region]);
      }
      const net = Template.fromStack(stacks.find((s) => s.stackName === `hamh-${stage}-network`));
      const subnets = Object.values(net.findResources("AWS::EC2::Subnet")) as any[];
      const privateAzs = subnets.filter((r) => JSON.stringify(r.Properties.Tags).includes('"private"'))
        .map((r) => r.Properties.AvailabilityZone).sort();
      expect(privateAzs).toEqual([...azs].sort());
      const group = Object.values(net.findResources("AWS::NetworkFirewall::RuleGroup"))[0] as any;
      expect(group.Properties.RuleGroup.RulesSource.RulesSourceList.Targets).toEqual(egressDomains(want.region));
      const s3 = Object.values(net.findResources("AWS::EC2::VPCEndpoint"))
        .find((r: any) => r.Properties.VpcEndpointType === "Gateway") as any;
      expect(JSON.stringify(s3.Properties.PolicyDocument)).toContain(`prod-${want.region}-starport-layer-bucket`);
    }
  });

  test("the S3 gateway endpoint admits only this entry's own buckets and AgentCore's session storage of its Region", async () => {
    // The owner pin follows the entry for hamh-<stage>-* (the stack's account). AgentCore's session storage carries no account
    // number: the statement names this Region's account regional buckets and admits the AgentCore service principal only.
    const before = process.env.HAMH_AGENTCORE_STORAGE_ACCOUNTS;
    delete process.env.HAMH_AGENTCORE_STORAGE_ACCOUNTS;
    try {
      const { buildApp, Template } = await underProfile(profile);
      for (const stage of ["direct"] as const) {
        const stacks = buildApp({ stage }).node.children.filter((c) => "templateOptions" in c) as any[];
        const net = Template.fromStack(stacks.find((s) => s.stackName === `hamh-${stage}-network`));
        const gw = Object.values(net.findResources("AWS::EC2::VPCEndpoint")).filter((r: any) => r.Properties.VpcEndpointType === "Gateway") as any[];
        expect(gw).toHaveLength(1);
        const stmts = gw[0].Properties.PolicyDocument.Statement as any[];
        const by = (pattern: string) => stmts.filter((st) => [st.Resource].flat().includes(pattern));
        expect(by(`arn:aws:s3:::hamh-${stage}-*`).map((st) => st.Condition)).toEqual([{ StringEquals: { "aws:ResourceAccount": want.account } }]);
        expect(by(`arn:aws:s3:::acr-storage-*-${want.region}-an`).map((st) => st.Condition))
          .toEqual([{ StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } }]);
        expect(JSON.stringify(by(`arn:aws:s3:::acr-storage-*-${want.region}-an`))).not.toMatch(/\d{12}/);
        expect(stmts.filter((st) => !st.Condition).flatMap((st) => [st.Resource].flat())).toEqual([`arn:aws:s3:::prod-${want.region}-starport-layer-bucket/*`]);
      }
    } finally {
      if (before !== undefined) process.env.HAMH_AGENTCORE_STORAGE_ACCOUNTS = before;
    }
  });

  test("control-api takes the entry's Admin role as the operator principals", async () => {
    const { buildApp, Template } = await underProfile(profile);
    const app = buildApp({ stage: "direct" });
    const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
    const envOf = (stack: string, fn: string) => (Object.values(Template.fromStack(stacks.find((s) => s.stackName === `hamh-direct-${stack}`))
      .findResources("AWS::Lambda::Function")) as any[]).find((r) => r.Properties.FunctionName === fn).Properties.Environment.Variables;
    expect(envOf("control", "hamh-direct-control-api").OPERATOR_PRINCIPALS).toBe(`arn:aws:sts::${want.account}:assumed-role/Admin/*`);
  });
});
