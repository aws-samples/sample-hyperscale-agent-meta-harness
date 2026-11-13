import { describe, expect, test, vi } from "vitest";
import { Annotations, Match, Template } from "aws-cdk-lib/assertions";
import { buildApp } from "../lib/app.ts";
import { ACCOUNT, committedUtilityCatalog, REGION, resolveUtilityModel, type UtilityCatalogSource } from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { ACTIVE, HISTORY, TEST_ENTRY, underEntry } from "./accounts-three.ts";
import { phase2Images, resourcesOf, synth } from "./helpers.ts";

// The utility model is the small model of the solution's own helper calls outside the engine (the run name first). The
// deploy setting HAMH_UTILITY_MODEL picks its row in crates/ham-core/assets/utility-models.json (id, alias or off); the orchestration stack
// grants run-session one Bedrock statement for that row's profile and foundation-model ARNs, gives the function the row id as UTILITY_MODEL,
// and sets a one-retry, ten-minute event config for the asynchronous `name-run` call of the run machine.
type Stmt = Record<string, any>;

const HAIKU = "us.anthropic.claude-haiku-5-5";
const BASE = "anthropic.claude-haiku-5-5";
const RECORD_FILE = `smoke-utility-${HAIKU}.json`;

const GOLDEN = [
  `arn:aws:bedrock:${REGION}:${ACCOUNT}:inference-profile/${HAIKU}`,
  `arn:aws:bedrock:us-west-2::foundation-model/${BASE}`,
  `arn:aws:bedrock:us-east-1::foundation-model/${BASE}`,
  `arn:aws:bedrock:us-east-2::foundation-model/${BASE}`,
];

const row = (over: Record<string, unknown> = {}): Record<string, unknown> => ({
  id: HAIKU, name: "Claude Haiku 5.5", vendor: "anthropic", alias: "haiku", baseModelId: BASE, protocol: "converse", region: "us-east-1",
  additionalModelRequestFields: { thinking: { type: "disabled" } },
  smokeRecordId: `s6:${HAIKU}:us-east-1:converse+u1:2026-10-20/${RECORD_FILE}`, smokedAt: "2026-10-20T18:03:30Z", ...over,
});
const source = (models: unknown[], opts: { default?: string; has?: (file: string) => boolean; schemaVersion?: number } = {}): UtilityCatalogSource => ({
  catalog: { schemaVersion: opts.schemaVersion ?? 1, default: opts.default ?? "haiku", models },
  hasRecord: opts.has ?? (() => true),
});

describe("resolveUtilityModel, against the committed catalog", () => {
  test("unset, empty and blank mean the default row, with the profile ARN and the foundation models of the row", () => {
    for (const setting of [undefined, "", "   ", "\t"]) {
      const got = resolveUtilityModel(setting, REGION);
      expect(got.value, JSON.stringify(setting)).toBe(HAIKU);
      expect(got.resources, JSON.stringify(setting)).toEqual(GOLDEN);
      expect("fallback" in got, "the default in the row's own Region has nothing to explain").toBe(false);
    }
  });

  test("the alias in any case, with spaces around it, and the exact id name the same row", () => {
    for (const setting of ["haiku", "HAIKU", " Haiku ", HAIKU, `  ${HAIKU}  `]) {
      expect(resolveUtilityModel(setting, REGION), JSON.stringify(setting)).toEqual({ value: HAIKU, resources: GOLDEN });
    }
  });

  test("off in any case, with spaces around it, is the value off with no ARN", () => {
    for (const setting of ["off", "OFF", "Off", "  off  "]) {
      expect(resolveUtilityModel(setting, REGION), JSON.stringify(setting)).toEqual({ value: "off", resources: [] });
    }
  });

  test("an unknown word is refused, the text names HAMH_UTILITY_MODEL and the choices, and nothing falls through to the default", () => {
    for (const setting of ["sonnet", "nope", "us.anthropic.claude-sonnet-5-5", "global.anthropic.claude-haiku-5-5", "haiku-5-5", `${HAIKU}x`, "of"]) {
      expect(() => resolveUtilityModel(setting, REGION), setting).toThrow(/HAMH_UTILITY_MODEL.*haiku/);
    }
  });

  test("a stage Region other than the row's: an explicit request is refused, naming both Regions", () => {
    for (const setting of ["haiku", HAIKU]) {
      expect(() => resolveUtilityModel(setting, "us-west-2"), setting).toThrow(/HAMH_UTILITY_MODEL.*us-east-1.*us-west-2/);
    }
  });

  test("a stage Region other than the row's: an unset setting means naming off, with a note that names the setting and both Regions", () => {
    // The history entry of accounts.json (us-west-2) is synthesized by existing tests with no option; the row is smoked in us-east-1 only, and an
    // optional feature never stops a stage that cannot run it. An explicit request in that Region still throws (the test above).
    for (const setting of [undefined, "", "  "]) {
      const got = resolveUtilityModel(setting, "us-west-2");
      expect(got.value, JSON.stringify(setting)).toBe("off");
      expect(got.resources, JSON.stringify(setting)).toEqual([]);
      expect(got.fallback, JSON.stringify(setting)).toMatch(/HAMH_UTILITY_MODEL.*us-east-1.*us-west-2/);
    }
  });

  test("off in another Region is off without a note", () => {
    expect(resolveUtilityModel("off", "us-west-2")).toEqual({ value: "off", resources: [] });
  });
});

describe("committedUtilityCatalog", () => {
  test("holds the committed catalog and finds a record file beside it by plain name only", () => {
    const src = committedUtilityCatalog();
    expect((src.catalog as { models: { id: string }[] }).models[0].id).toBe(HAIKU);
    expect(src.hasRecord(RECORD_FILE)).toBe(true);
    for (const name of ["nope.json", "../accounts.json", "../../accounts.json", `${RECORD_FILE}/..`, ""]) expect(src.hasRecord(name), name).toBe(false);
  });
});

describe("resolveUtilityModel, against a catalog it is given", () => {
  test("off reads neither the catalog nor a record file", () => {
    const trap = {
      get catalog(): unknown { throw new Error("the catalog was read"); },
      hasRecord: (): boolean => { throw new Error("a record file was looked up"); },
    } as UtilityCatalogSource;
    expect(resolveUtilityModel("OFF", REGION, trap)).toEqual({ value: "off", resources: [] });
  });

  test("a row without a smoke record is refused, whether it is asked for by name or is the default (root invariant 6)", () => {
    for (const bad of [{ smokeRecordId: undefined }, { smokeRecordId: "" }, { smokeRecordId: "   " }]) {
      const src = source([row(bad)]);
      expect(() => resolveUtilityModel("haiku", REGION, src), JSON.stringify(bad)).toThrow(/HAMH_UTILITY_MODEL.*smoke record/);
      expect(() => resolveUtilityModel(undefined, REGION, src), JSON.stringify(bad)).toThrow(/smoke record/);
    }
  });

  test("a record file that is not beside the catalog refuses the row, and the lookup is by the file name alone", () => {
    const asked: string[] = [];
    const src = source([row()], { has: (file) => { asked.push(file); return false; } });
    expect(() => resolveUtilityModel("haiku", REGION, src)).toThrow(new RegExp(`HAMH_UTILITY_MODEL.*${RECORD_FILE.replaceAll(".", "\\.")}`));
    expect(asked).toEqual([RECORD_FILE]);
    const present = source([row()], { has: (file) => file === RECORD_FILE });
    expect(resolveUtilityModel("haiku", REGION, present).value).toBe(HAIKU);
  });

  test("a record id whose file part is no plain .json file name is refused before any lookup", () => {
    for (const tail of ["..", "", "a b.json", "x.txt", ".", "x/.."]) {
      const src = source([row({ smokeRecordId: `s6:${HAIKU}:us-east-1:converse+u1:2026-10-20/${tail}` })], {
        has: () => { throw new Error(`looked up ${tail}`); },
      });
      expect(() => resolveUtilityModel("haiku", REGION, src), JSON.stringify(tail)).toThrow(/HAMH_UTILITY_MODEL/);
    }
  });

  test("an alternate route adds its profile ARN, and a global route the Region-less foundation model, each ARN once and in route order", () => {
    const globalId = "global.anthropic.claude-haiku-5-5";
    const alt = { id: globalId, region: "us-east-1", smokeRecordId: `s6:${globalId}:us-east-1:converse+u1:2026-10-21/smoke-utility-${globalId}.json` };
    const got = resolveUtilityModel("haiku", REGION, source([row({ alternateRoutes: [alt] })]));
    expect(got.value).toBe(HAIKU);
    expect(got.resources).toEqual([...GOLDEN, `arn:aws:bedrock:${REGION}:${ACCOUNT}:inference-profile/${globalId}`, `arn:aws:bedrock:::foundation-model/${BASE}`]);
    expect(new Set(got.resources).size).toBe(got.resources.length);
  });

  test("an alternate route that has no record, no record file or another Region refuses the whole row", () => {
    const alt = (over: Record<string, unknown>) => ({
      id: "global.anthropic.claude-haiku-5-5", region: "us-east-1", smokeRecordId: "s6:x:us-east-1:converse+u1:2026-10-21/smoke-utility-global.json", ...over,
    });
    for (const bad of [alt({ smokeRecordId: undefined }), alt({ smokeRecordId: " " }), alt({ region: "us-west-2" })]) {
      expect(() => resolveUtilityModel("haiku", REGION, source([row({ alternateRoutes: [bad] })])), JSON.stringify(bad)).toThrow(/HAMH_UTILITY_MODEL/);
    }
    const missingFile = source([row({ alternateRoutes: [alt({})] })], { has: (file) => file === RECORD_FILE });
    expect(() => resolveUtilityModel("haiku", REGION, missingFile)).toThrow(/HAMH_UTILITY_MODEL.*smoke-utility-global\.json/);
  });

  test("a catalog the code cannot use is refused with the file named, for the default and for a named row", () => {
    const cases: [string, UtilityCatalogSource][] = [
      ["schema version", source([row()], { schemaVersion: 2 })],
      ["no rows", source([])],
      ["default names no row", source([row()], { default: "sonnet" })],
      ["a row without an id", source([row({ id: "" })])],
      ["a row without a base model", source([row({ baseModelId: undefined })])],
      ["not an object", { catalog: "text", hasRecord: () => true }],
    ];
    for (const [why, src] of cases) {
      expect(() => resolveUtilityModel(undefined, REGION, src), why).toThrow(/utility-models\.json/);
    }
  });

  test("the default may be named by its id", () => {
    expect(resolveUtilityModel(undefined, REGION, source([row()], { default: HAIKU })).value).toBe(HAIKU);
  });
});

// The default process selection is the active entry of accounts.json (us-east-1), where the row is smoked.
const { app, templates } = synth("direct", phase2Images("direct"));
const orch = templates.get("hamh-direct-orchestration")!;

type HasJson = { toJSON(): any };
const policyOf = (t: HasJson, roleName: string): Stmt[] => {
  const role = Object.values(t.toJSON().Resources as Record<string, any>).find((r) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === roleName);
  expect(role, roleName).toBeDefined();
  return role.Properties.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
};
const actions = (s: Stmt): string[] => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const resources = (s: Stmt): string[] => (Array.isArray(s.Resource) ? s.Resource : [s.Resource]);
const functionOf = (t: HasJson, name: string) => Object.values(t.toJSON().Resources as Record<string, any>)
  .filter((r) => r.Type === "AWS::Lambda::Function").map((r) => r.Properties).find((p) => p.FunctionName === name);
const withUtility = (setting: string | undefined) => synth("direct", { ...phase2Images("direct"), utilityModel: setting }).templates.get("hamh-direct-orchestration")!;

/**
 * Every role-like holder of an Allow that reaches Bedrock model invocation, by role name (or by resource id for a policy resource), across the
 * templates: the actions InvokeModel and InvokeModelWithResponseStream, a wildcard that matches them, or a NotAction.
 */
function bedrockInvokeHolders(all: Map<string, Template>): string[] {
  const glob = (pattern: string, text: string) =>
    new RegExp("^" + pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".") + "$", "i").test(text);
  const reaches = (s: Stmt) => s.Effect === "Allow"
    && (s.NotAction !== undefined || actions(s).some((a) => ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream"].some((i) => glob(a, i))));
  const holders = new Set<string>();
  for (const [stack, t] of all) {
    for (const [id, r] of Object.entries(t.toJSON().Resources as Record<string, any>)) {
      if (r.Type === "AWS::IAM::Role") {
        const statements: Stmt[] = (r.Properties.Policies ?? []).flatMap((p: any) => p.PolicyDocument.Statement);
        if (statements.some(reaches)) holders.add(r.Properties.RoleName);
        for (const arn of r.Properties.ManagedPolicyArns ?? []) expect(JSON.stringify(arn), `${stack}/${id}`).not.toMatch(/bedrock/i);
      } else if (r.Type === "AWS::IAM::Policy" || r.Type === "AWS::IAM::ManagedPolicy") {
        if ((r.Properties.PolicyDocument.Statement as Stmt[]).some(reaches)) holders.add(`${stack}/${id}`);
      }
    }
  }
  return [...holders].sort();
}

describe("the orchestration stack, default setting", () => {
  test("run-session holds one new statement, InvokeUtilityModel: InvokeModel on the four ARNs of the row, no condition, no wildcard", () => {
    const s = policyOf(orch, "hamh-direct-run-session").find((x) => x.Sid === "InvokeUtilityModel")!;
    expect(s, "InvokeUtilityModel").toBeDefined();
    expect(s.Effect).toBe("Allow");
    expect(actions(s)).toEqual(["bedrock:InvokeModel"]);
    expect([...resources(s)].sort()).toEqual([...GOLDEN].sort());
    expect(resources(s)).toHaveLength(4);
    expect(s.Condition).toBeUndefined();
    for (const r of resources(s)) expect(r).not.toContain("*");
    // exactly one statement of the role names a bedrock: action (bedrock-agentcore: is the runtime grant of InvokeWorkers)
    expect(policyOf(orch, "hamh-direct-run-session").filter((x) => actions(x).some((a) => a.startsWith("bedrock:"))).map((x) => x.Sid)).toEqual(["InvokeUtilityModel"]);
  });

  test("the function gets the row id as UTILITY_MODEL", () => {
    expect(functionOf(orch, "hamh-direct-run-session").Environment.Variables.UTILITY_MODEL).toBe(HAIKU);
  });

  test("the profile ARN names the stage's own account and Region", () => {
    const profile = resources(policyOf(orch, "hamh-direct-run-session").find((x) => x.Sid === "InvokeUtilityModel")!).find((r) => r.includes(":inference-profile/"))!;
    expect(profile).toBe(`arn:aws:bedrock:${REGION}:${ACCOUNT}:inference-profile/${HAIKU}`);
  });
});

describe("the orchestration stack, off", () => {
  test("off in any case removes the statement and sets the variable to off", () => {
    for (const setting of ["off", "OFF", " Off "]) {
      const t = withUtility(setting);
      expect(policyOf(t, "hamh-direct-run-session").map((x) => x.Sid), setting).not.toContain("InvokeUtilityModel");
      expect(JSON.stringify(t.toJSON()), setting).not.toContain("bedrock:InvokeModel");
      expect(functionOf(t, "hamh-direct-run-session").Environment.Variables.UTILITY_MODEL, setting).toBe("off");
    }
  });

  test("an alias and the exact id synthesize the same grant as the default", () => {
    const base = JSON.stringify(policyOf(orch, "hamh-direct-run-session"));
    for (const setting of ["haiku", HAIKU, "", undefined]) {
      expect(JSON.stringify(policyOf(withUtility(setting), "hamh-direct-run-session")), String(setting)).toBe(base);
    }
  });
});

describe("a bad value stops the synthesis", () => {
  test("an unknown word throws from buildApp, naming HAMH_UTILITY_MODEL", () => {
    for (const setting of ["sonnet", "nope", "us.anthropic.claude-sonnet-5-5"]) {
      expect(() => buildApp({ stage: "direct", utilityModel: setting }), setting).toThrow(/HAMH_UTILITY_MODEL/);
    }
  });

  test("the CLI entry resolves the setting too, so a bad value fails before any stack is built", () => {
    const lambdaDir = new URL("./fixtures/lambda", import.meta.url).pathname;
    expect(() => entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, HAMH_UTILITY_MODEL: "nope" })).toThrow(/HAMH_UTILITY_MODEL/);
    expect(() => entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, HAMH_UTILITY_MODEL: "sonnet" })).toThrow(/HAMH_UTILITY_MODEL/);
  });
});

describe("who may invoke a Bedrock model", () => {
  test("across the nine templates exactly harness-worker-session and the run-session role hold a bedrock:Invoke action", () => {
    expect(templates.size).toBe(9);
    expect(bedrockInvokeHolders(templates)).toEqual(["hamh-direct-harness-worker-session", "hamh-direct-run-session"]);
  });

  test("with the model off only the worker session holds one", () => {
    const off = synth("direct", { ...phase2Images("direct"), utilityModel: "off" }).templates;
    expect(bedrockInvokeHolders(off)).toEqual(["hamh-direct-harness-worker-session"]);
  });

  test("control-api, worker-api, the research functions and both Step Functions roles hold none", () => {
    const holders = bedrockInvokeHolders(templates);
    for (const name of ["hamh-direct-control-api", "hamh-direct-worker-api", "hamh-direct-research-api", "hamh-direct-research-job", "hamh-direct-run", "hamh-direct-run-ended"]) {
      expect(holders, name).not.toContain(name);
    }
  });

  test("UTILITY_MODEL is on run-session only", () => {
    const withVariable: string[] = [];
    for (const t of templates.values()) {
      for (const r of Object.values(resourcesOf(t, "AWS::Lambda::Function"))) {
        const p = r.Properties as any;
        if (p.Environment?.Variables && "UTILITY_MODEL" in p.Environment.Variables) withVariable.push(p.FunctionName);
      }
    }
    expect(withVariable).toEqual(["hamh-direct-run-session"]);
  });
});

describe("the asynchronous call of the run machine", () => {
  test("one event config for run-session: one retry, ten minutes, no destination, on $LATEST", () => {
    const cfgs = Object.values(resourcesOf(orch, "AWS::Lambda::EventInvokeConfig")).map((r) => r.Properties as any);
    expect(cfgs).toHaveLength(1);
    const fnId = Object.entries(resourcesOf(orch, "AWS::Lambda::Function")).find(([, r]) => (r.Properties as any).FunctionName === "hamh-direct-run-session")![0];
    expect(cfgs[0].FunctionName).toEqual({ Ref: fnId });
    expect(cfgs[0].Qualifier).toBe("$LATEST");
    expect(cfgs[0].MaximumRetryAttempts).toBe(1);
    expect(cfgs[0].MaximumEventAgeInSeconds).toBe(600);
    expect(cfgs[0].DestinationConfig).toBeUndefined();
  });

  test("the research job keeps its own config: two event configs in the nine templates", () => {
    let count = 0;
    for (const t of templates.values()) count += Object.keys(resourcesOf(t, "AWS::Lambda::EventInvokeConfig")).length;
    expect(count).toBe(2);
  });

  test("the setting does not touch the event config (the machine calls name-run whether or not a model is set)", () => {
    const off = withUtility("off");
    expect(Object.keys(resourcesOf(off, "AWS::Lambda::EventInvokeConfig"))).toHaveLength(1);
  });
});

describe("the role stays inside the IAM size limit", () => {
  test("the run-session inline policy, with the new statement, is under 10,240 characters", () => {
    // IAM counts the characters of an inline policy without white space; JSON text with the intrinsic objects unresolved is longer than that.
    const text = JSON.stringify({ Version: "2012-10-17", Statement: policyOf(orch, "hamh-direct-run-session") }).replace(/\s/g, "");
    expect(text.length).toBeLessThan(10_240);
  });
});

/** The profile of the entry of the three-entry fixture (infra/test/accounts-three.ts) with this role. */
const profileOfRole = (role: "history" | "active" | "test"): string => ({ history: HISTORY, active: ACTIVE, test: TEST_ENTRY })[role].profile;

// A fresh module graph per selection: config.ts reads HAMH_ACCOUNTS_FILE and HAMH_AWS_PROFILE when it loads.
async function underProfile(profile: string) {
  return underEntry(profile, async () => {
    vi.resetModules();
    const appModule = await import("../lib/app.ts");
    const configModule = await import("../lib/config.ts");
    const assertions = await import("aws-cdk-lib/assertions");
    return { buildApp: appModule.buildApp, config: configModule, assertions };
  });
}

describe("the other entries of the accounts file", () => {
  test("the test entry (us-east-1) gets the grant with its own account in the profile ARN", async () => {
    const { buildApp: build, config, assertions } = await underProfile(profileOfRole("test"));
    expect(config.REGION).toBe("us-east-1");
    const stack = build({ stage: "direct" }).node.children.find((c: any) => c.stackName === "hamh-direct-orchestration") as any;
    const t = assertions.Template.fromStack(stack);
    const s = policyOf(t, "hamh-direct-run-session").find((x) => x.Sid === "InvokeUtilityModel")!;
    expect(resources(s)).toContain(`arn:aws:bedrock:us-east-1:${config.ACCOUNT}:inference-profile/${HAIKU}`);
    expect(config.ACCOUNT).not.toBe(ACCOUNT);
  });

  test("the history entry (us-west-2) still synthesizes with no option: naming is off there, with a synth note, and an explicit request throws", async () => {
    const { buildApp: build, config, assertions } = await underProfile(profileOfRole("history"));
    expect(config.REGION).toBe("us-west-2");
    const built = build({ stage: "direct" });
    const stack = built.node.children.find((c: any) => c.stackName === "hamh-direct-orchestration") as any;
    const t = assertions.Template.fromStack(stack);
    expect(policyOf(t, "hamh-direct-run-session").map((x) => x.Sid)).not.toContain("InvokeUtilityModel");
    expect(functionOf(t, "hamh-direct-run-session").Environment.Variables.UTILITY_MODEL).toBe("off");
    const notes = assertions.Annotations.fromStack(stack).findInfo("*", assertions.Match.stringLikeRegexp("HAMH_UTILITY_MODEL"));
    expect(notes).toHaveLength(1);
    expect(() => build({ stage: "direct", utilityModel: "haiku" })).toThrow(/HAMH_UTILITY_MODEL/);
    expect(() => build({ stage: "direct", utilityModel: "off" })).not.toThrow();
  });

  test("the default entry gives no synth note", () => {
    const stack = app.node.children.find((c: any) => c.stackName === "hamh-direct-orchestration") as any;
    expect(Annotations.fromStack(stack).findInfo("*", Match.stringLikeRegexp("HAMH_UTILITY_MODEL"))).toHaveLength(0);
  });
});
