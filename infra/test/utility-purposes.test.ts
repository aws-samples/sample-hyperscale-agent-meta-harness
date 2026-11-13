import { readFileSync } from "node:fs";
import { describe, expect, test, vi } from "vitest";
import { buildApp } from "../lib/app.ts";
import {
  ACCOUNT, REGION, resolveUtilityModel, resolveUtilityPurposes, type ResolvedUtilityModel, type UtilityCatalogSource, UTILITY_PURPOSE_WORDS,
} from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { HISTORY, TEST_ENTRY, underEntry } from "./accounts-three.ts";
import { phase2Images, resourcesOf, synth } from "./helpers.ts";

// The utility model serves more than one purpose. Each purpose has its own smoke record in the catalog row
// (`purposes`; the top-level record is the run name's), the deploy setting HAMH_UTILITY_PURPOSES chooses which ones the stage runs, and the
// Lambda reads the result as UTILITY_PURPOSES. The run-tags purpose reads the owner's newest search items for its vocabulary, which needs one
// IAM statement, TagVocabulary: a Query on the owner partitions that can read the tag attributes and nothing else.
type Stmt = Record<string, any>;

const HAIKU = "us.anthropic.claude-haiku-5-5";
const BASE = "anthropic.claude-haiku-5-5";
const NAME_RECORD = `smoke-utility-${HAIKU}.json`;
const TAGS_RECORD = `smoke-utility-tags-${HAIKU}.json`;
const TABLE = `arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`;

const TAG_VOCABULARY: Stmt = {
  Sid: "TagVocabulary", Effect: "Allow", Action: ["dynamodb:Query"], Resource: TABLE,
  Condition: {
    "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["OWNER#*"] },
    "ForAllValues:StringEquals": { "dynamodb:Attributes": ["PK", "SK", "runTags", "runTagsUser", "runTagsOff"] },
    StringEquals: { "dynamodb:Select": "SPECIFIC_ATTRIBUTES" },
  },
};

const tagsPurpose = { smokeRecordId: `s6:${HAIKU}:us-east-1:converse+u2:2026-10-21/${TAGS_RECORD}`, smokedAt: "2026-10-21T18:03:57Z" };
const row = (over: Record<string, unknown> = {}): Record<string, unknown> => ({
  id: HAIKU, name: "Claude Haiku 5.5", vendor: "anthropic", alias: "haiku", baseModelId: BASE, protocol: "converse", region: "us-east-1",
  additionalModelRequestFields: { thinking: { type: "disabled" } },
  smokeRecordId: `s6:${HAIKU}:us-east-1:converse+u1:2026-10-20/${NAME_RECORD}`, smokedAt: "2026-10-20T18:03:30Z",
  purposes: { "run-tags": tagsPurpose }, ...over,
});
const source = (models: unknown[], has: (file: string) => boolean = () => true): UtilityCatalogSource => ({
  catalog: { schemaVersion: 1, default: "haiku", models }, hasRecord: has,
});
const modelOf = (src: UtilityCatalogSource): ResolvedUtilityModel => resolveUtilityModel("haiku", REGION, src);

const actions = (s: Stmt): string[] => (Array.isArray(s.Action) ? s.Action : [s.Action]);
const resources = (s: Stmt): string[] => (Array.isArray(s.Resource) ? s.Resource : [s.Resource]);
type HasJson = { toJSON(): any };
const policyOf = (t: HasJson, roleName: string): Stmt[] => {
  const role = Object.values(t.toJSON().Resources as Record<string, any>).find((r) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === roleName);
  expect(role, roleName).toBeDefined();
  return role.Properties.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
};
const functionOf = (t: HasJson, name: string) => Object.values(t.toJSON().Resources as Record<string, any>)
  .filter((r) => r.Type === "AWS::Lambda::Function").map((r) => r.Properties).find((p) => p.FunctionName === name);
const RUN_SESSION = "hamh-direct-run-session";

const built = (opts: Record<string, unknown> = {}) => synth("direct", { ...phase2Images("direct"), ...opts }).templates;
const { templates } = synth("direct", phase2Images("direct"));
const orch = templates.get("hamh-direct-orchestration")!;

describe("the words", () => {
  test("the purposes the infra knows are the purposes the Rust build knows, in the same order", () => {
    const rust = readFileSync(new URL("../../crates/ham-core/src/utility.rs", import.meta.url), "utf8");
    const words = [...rust.matchAll(/UtilityPurpose::\w+ => "([a-z-]+)"/g)].map((m) => m[1]);
    expect(words).toEqual(["run-name", "run-tags"]);
    expect([...UTILITY_PURPOSE_WORDS]).toEqual(words);
  });
});

describe("resolveUtilityPurposes, against a catalog it is given", () => {
  test("unset, empty and blank mean every purpose the row holds a record for, in the catalog's order", () => {
    const src = source([row()]);
    for (const setting of [undefined, "", "   ", "\t"]) {
      expect(resolveUtilityPurposes(setting, modelOf(src), src), JSON.stringify(setting)).toEqual({ value: "run-name,run-tags", purposes: ["run-name", "run-tags"] });
    }
  });

  test("a subset is allowed; case and spaces do not matter; the order is the catalog's; a repeat counts once", () => {
    const src = source([row()]);
    const cases: [string, string][] = [
      ["run-name", "run-name"], [" RUN-NAME ", "run-name"], ["run-tags", "run-tags"], ["run-tags, run-name", "run-name,run-tags"],
      ["run-name,run-name", "run-name"], ["Run-Tags,RUN-NAME,run-tags", "run-name,run-tags"],
    ];
    for (const [setting, value] of cases) {
      const got = resolveUtilityPurposes(setting, modelOf(src), src);
      expect(got.value, setting).toBe(value);
      expect(got.purposes.join(","), setting).toBe(value);
    }
  });

  test("a row without a purposes object serves the run name only, and an explicit request for tags is refused for want of a record", () => {
    const src = source([row({ purposes: undefined })]);
    expect(resolveUtilityPurposes(undefined, modelOf(src), src)).toEqual({ value: "run-name", purposes: ["run-name"] });
    expect(() => resolveUtilityPurposes("run-tags", modelOf(src), src)).toThrow(/HAMH_UTILITY_PURPOSES.*run-tags.*smoke record/);
    expect(() => resolveUtilityPurposes("run-name,run-tags", modelOf(src), src)).toThrow(/HAMH_UTILITY_PURPOSES.*run-tags/);
  });

  test("a purpose whose record id is blank is not served: the default leaves it out and an explicit request is refused", () => {
    for (const blank of [{ smokeRecordId: "", smokedAt: "x" }, { smokeRecordId: "   ", smokedAt: "x" }, { smokedAt: "x" }]) {
      const src = source([row({ purposes: { "run-tags": blank } })]);
      expect(resolveUtilityPurposes(undefined, modelOf(src), src), JSON.stringify(blank).slice(0, 40)).toEqual({ value: "run-name", purposes: ["run-name"] });
      expect(() => resolveUtilityPurposes("run-tags", modelOf(src), src), JSON.stringify(blank)).toThrow(/HAMH_UTILITY_PURPOSES.*run-tags.*smoke record/);
    }
  });

  test("a record file that is not beside the catalog refuses the purpose, by file name only; the other purpose still works alone", () => {
    const asked: string[] = [];
    const src = source([row()], (file) => { asked.push(file); return file === NAME_RECORD; });
    const model = modelOf(src);
    asked.length = 0;
    expect(() => resolveUtilityPurposes(undefined, model, src)).toThrow(new RegExp(`HAMH_UTILITY_PURPOSES.*${TAGS_RECORD.replaceAll(".", "\\.")}`));
    expect(() => resolveUtilityPurposes("run-tags", model, src)).toThrow(/HAMH_UTILITY_PURPOSES.*run-tags/);
    expect(asked.every((f) => f === TAGS_RECORD), JSON.stringify(asked)).toBe(true);
    expect(resolveUtilityPurposes("run-name", model, src)).toEqual({ value: "run-name", purposes: ["run-name"] });
  });

  test("a record id whose file part is no plain .json file name is refused before any lookup", () => {
    for (const tail of ["..", "", "a b.json", "x.txt", ".", "x/.."]) {
      const bad = { smokeRecordId: `s6:${HAIKU}:us-east-1:converse+u2:2026-10-21/${tail}`, smokedAt: "x" };
      const probe = source([row()]);
      const model = modelOf(probe);
      const src = source([row({ purposes: { "run-tags": bad } })], () => { throw new Error(`looked up ${tail}`); });
      expect(() => resolveUtilityPurposes("run-tags", model, src), JSON.stringify(tail)).toThrow(/HAMH_UTILITY_PURPOSES/);
    }
  });

  test("a purpose key the build does not know is refused, naming the catalog file", () => {
    const src = source([row({ purposes: { "run-tags": tagsPurpose, "run-summary": tagsPurpose } })]);
    expect(() => resolveUtilityPurposes(undefined, modelOf(src), src)).toThrow(/utility-models\.json.*run-summary/);
  });

  test("a model that names no row of the catalog is an error that names the catalog", () => {
    const src = source([row()]);
    expect(() => resolveUtilityPurposes(undefined, { value: "us.anthropic.claude-nope-9", resources: [] }, src)).toThrow(/utility-models\.json/);
  });

  test("an unknown word, an empty element or a bare comma is refused; the text names the setting and the words that exist", () => {
    const src = source([row()]);
    const model = modelOf(src);
    for (const setting of ["nope", "run-summary", "run_tags", "run-name,nope", "run-name,", ",run-name", ",", " , ", "run-name run-tags", "off"]) {
      expect(() => resolveUtilityPurposes(setting, model, src), JSON.stringify(setting)).toThrow(/HAMH_UTILITY_PURPOSES.*run-name.*run-tags/);
    }
  });
});

describe("resolveUtilityPurposes, with the model off", () => {
  const OFF: ResolvedUtilityModel = { value: "off", resources: [] };
  const trap = {
    get catalog(): unknown { throw new Error("the catalog was read"); },
    hasRecord: (): boolean => { throw new Error("a record file was looked up"); },
  } as UtilityCatalogSource;

  test("the value is off with no purpose, whatever the setting says, and the catalog is not read", () => {
    for (const setting of [undefined, "", "run-name", "run-tags", "run-name,run-tags"]) {
      expect(resolveUtilityPurposes(setting, OFF, trap), JSON.stringify(setting)).toEqual({ value: "off", purposes: [] });
    }
  });

  test("a word that is no purpose is still refused, so a typo in a deploy is not hidden by the other setting", () => {
    for (const setting of ["nope", "run-name,nope", ","]) {
      expect(() => resolveUtilityPurposes(setting, OFF, trap), JSON.stringify(setting)).toThrow(/HAMH_UTILITY_PURPOSES/);
    }
  });
});

describe("resolveUtilityPurposes, against the committed catalog", () => {
  test("the default is both purposes; the run name alone is allowed", () => {
    const model = resolveUtilityModel(undefined, REGION);
    expect(resolveUtilityPurposes(undefined, model)).toEqual({ value: "run-name,run-tags", purposes: ["run-name", "run-tags"] });
    expect(resolveUtilityPurposes("run-name", model)).toEqual({ value: "run-name", purposes: ["run-name"] });
  });
});

describe("the TagVocabulary statement", () => {
  test("statement_is_query_on_owner_partitions_for_tag_attributes_only", () => {
    const stmts = policyOf(orch, RUN_SESSION).filter((s) => s.Sid === "TagVocabulary");
    expect(stmts).toHaveLength(1);
    const s = stmts[0];
    expect(Object.keys(s).sort()).toEqual(["Action", "Condition", "Effect", "Resource", "Sid"]);
    expect(s.Effect).toBe("Allow");
    expect(actions(s)).toEqual(["dynamodb:Query"]);
    // the table ARN only: no index ARN, no wildcard
    expect(resources(s)).toEqual([TABLE]);
    for (const r of resources(s)) expect(r).not.toMatch(/\*|\/index\//);
    // the three conditions equal the block of the package exactly: the attribute list in this order and no other name
    expect(s.Condition).toEqual(TAG_VOCABULARY.Condition);
    expect(Object.keys(s.Condition).sort()).toEqual(["ForAllValues:StringEquals", "ForAllValues:StringLike", "StringEquals"]);
    expect(s.Condition["ForAllValues:StringEquals"]["dynamodb:Attributes"]).toEqual(["PK", "SK", "runTags", "runTagsUser", "runTagsOff"]);
    expect(s).toEqual(TAG_VOCABULARY);
  });

  test("no attribute the tag branch does not need can be read through it", () => {
    const attrs: string[] = policyOf(orch, RUN_SESSION).find((s) => s.Sid === "TagVocabulary")!.Condition["ForAllValues:StringEquals"]["dynamodb:Attributes"];
    for (const never of ["promptHead", "outcome", "lastActivity", "runName", "runNameSource", "runNameModel", "title", "model", "repository", "status", "body", "prompt", "*"]) {
      expect(attrs, never).not.toContain(never);
    }
    expect(attrs).toHaveLength(5);
  });

  test("the test entry's stage gets the statement, with its own account in the table ARN", async () => {
    await underEntry(TEST_ENTRY.profile, async () => {
      vi.resetModules();
      const appModule = await import("../lib/app.ts");
      const configModule = await import("../lib/config.ts");
      const assertions = await import("aws-cdk-lib/assertions");
      expect(configModule.REGION).toBe("us-east-1");
      const stack = appModule.buildApp({ stage: "direct" }).node.children.find((c: any) => c.stackName === "hamh-direct-orchestration") as any;
      const t = assertions.Template.fromStack(stack);
      const s = policyOf(t, RUN_SESSION).find((x) => x.Sid === "TagVocabulary")!;
      expect(resources(s)).toEqual([`arn:aws:dynamodb:us-east-1:${configModule.ACCOUNT}:table/hamh-direct-harness`]);
      expect(configModule.ACCOUNT).toBe(TEST_ENTRY.account);
      expect(configModule.ACCOUNT).not.toBe(ACCOUNT);
    });
  });

  test("statement_is_absent_when_the_model_is_off_or_purposes_lack_run_tags", () => {
    for (const setting of ["off", "OFF", " Off "]) {
      const t = built({ utilityModel: setting }).get("hamh-direct-orchestration")!;
      expect(policyOf(t, RUN_SESSION).map((s) => s.Sid), setting).not.toContain("TagVocabulary");
      expect(functionOf(t, RUN_SESSION).Environment.Variables.UTILITY_PURPOSES, setting).toBe("off");
      // off in any case with an explicit purposes setting is still off
      const both = built({ utilityModel: setting, utilityPurposes: "run-name,run-tags" }).get("hamh-direct-orchestration")!;
      expect(policyOf(both, RUN_SESSION).map((s) => s.Sid), setting).not.toContain("TagVocabulary");
      expect(functionOf(both, RUN_SESSION).Environment.Variables.UTILITY_PURPOSES, setting).toBe("off");
    }
    for (const setting of ["run-name", " RUN-NAME "]) {
      const t = built({ utilityPurposes: setting }).get("hamh-direct-orchestration")!;
      const sids = policyOf(t, RUN_SESSION).map((s) => s.Sid);
      expect(sids, setting).not.toContain("TagVocabulary");
      expect(sids, "the run name keeps its model grant").toContain("InvokeUtilityModel");
      expect(functionOf(t, RUN_SESSION).Environment.Variables.UTILITY_PURPOSES, setting).toBe("run-name");
    }
    // the statement does not hide in any other form
    const t = built({ utilityPurposes: "run-name" }).get("hamh-direct-orchestration")!;
    expect(JSON.stringify(t.toJSON())).not.toContain("TagVocabulary");
    expect(JSON.stringify(t.toJSON())).not.toContain("runTagsUser");
  });

  test("tags alone, without the run name, still get the statement and the model grant", () => {
    const t = built({ utilityPurposes: "run-tags" }).get("hamh-direct-orchestration")!;
    const sids = policyOf(t, RUN_SESSION).map((s) => s.Sid);
    expect(sids).toContain("TagVocabulary");
    expect(sids).toContain("InvokeUtilityModel");
    expect(functionOf(t, RUN_SESSION).Environment.Variables.UTILITY_PURPOSES).toBe("run-tags");
  });

  test("the history entry (us-west-2), where the model is off for want of a smoke record in that Region, has neither the statement nor a purpose", async () => {
    await underEntry(HISTORY.profile, async () => {
      vi.resetModules();
      const appModule = await import("../lib/app.ts");
      const assertions = await import("aws-cdk-lib/assertions");
      for (const utilityPurposes of [undefined, "run-name", "run-tags"]) {
        const stack = appModule.buildApp({ stage: "direct", utilityPurposes }).node.children.find((c: any) => c.stackName === "hamh-direct-orchestration") as any;
        const t = assertions.Template.fromStack(stack);
        expect(policyOf(t, RUN_SESSION).map((s) => s.Sid), String(utilityPurposes)).not.toContain("TagVocabulary");
        expect(functionOf(t, RUN_SESSION).Environment.Variables.UTILITY_PURPOSES, String(utilityPurposes)).toBe("off");
      }
    });
  });
});

describe("UTILITY_PURPOSES", () => {
  test("purposes_variable_is_on_run_session_only", () => {
    const withVariable: string[] = [];
    for (const t of templates.values()) {
      for (const r of Object.values(resourcesOf(t, "AWS::Lambda::Function"))) {
        const p = r.Properties as any;
        if (p.Environment?.Variables && "UTILITY_PURPOSES" in p.Environment.Variables) withVariable.push(p.FunctionName);
      }
    }
    expect(withVariable).toEqual([RUN_SESSION]);
    expect(functionOf(orch, RUN_SESSION).Environment.Variables.UTILITY_PURPOSES).toBe("run-name,run-tags");
    // it sits beside UTILITY_MODEL and does not replace it
    expect(functionOf(orch, RUN_SESSION).Environment.Variables.UTILITY_MODEL).toBe(HAIKU);
  });

  test("an explicit list of both purposes, in any case and order, synthesizes the same stack as the default", () => {
    const base = JSON.stringify(built().get("hamh-direct-orchestration")!.toJSON());
    for (const opts of [{ utilityPurposes: "run-name,run-tags" }, { utilityPurposes: " run-tags , RUN-NAME " }, { utilityPurposes: "" }, { utilityPurposes: undefined }]) {
      expect(JSON.stringify(built(opts).get("hamh-direct-orchestration")!.toJSON()), JSON.stringify(opts)).toBe(base);
    }
  });
});

describe("a bad value stops the synthesis", () => {
  test("a_bad_purposes_value_throws_at_synth", () => {
    for (const setting of ["nope", "run-name,nope", "run-name,", "run_tags", "run-summary"]) {
      expect(() => buildApp({ stage: "direct", utilityPurposes: setting }), setting).toThrow(/HAMH_UTILITY_PURPOSES/);
      // also when the model is off: a typo is not hidden by the other setting
      expect(() => buildApp({ stage: "direct", utilityModel: "off", utilityPurposes: setting }), setting).toThrow(/HAMH_UTILITY_PURPOSES/);
    }
  });

  test("a purpose with no record is refused, naming the setting", () => {
    const src = source([row({ purposes: undefined })]);
    expect(() => resolveUtilityPurposes("run-tags", modelOf(src), src)).toThrow(/HAMH_UTILITY_PURPOSES/);
  });

  test("the CLI entry reads the setting like the model's: blank adds no option, a good value passes through trimmed, a bad one throws before any stack is built", () => {
    const lambdaDir = new URL("./fixtures/lambda", import.meta.url).pathname;
    for (const env of [{}, { HAMH_UTILITY_PURPOSES: "" }, { HAMH_UTILITY_PURPOSES: "   " }]) {
      expect("utilityPurposes" in entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, ...env }), JSON.stringify(env)).toBe(false);
    }
    expect(entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, HAMH_UTILITY_PURPOSES: "run-name" }).utilityPurposes).toBe("run-name");
    expect(entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, HAMH_UTILITY_PURPOSES: "  run-name,run-tags " }).utilityPurposes).toBe("run-name,run-tags");
    for (const bad of ["nope", "run-name,nope", "run-name,"]) {
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, HAMH_UTILITY_PURPOSES: bad }), bad).toThrow(/HAMH_UTILITY_PURPOSES/);
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: lambdaDir, HAMH_UTILITY_MODEL: "off", HAMH_UTILITY_PURPOSES: bad }), bad).toThrow(/HAMH_UTILITY_PURPOSES/);
    }
  });
});

describe("who gains what", () => {
  test("no_other_role_gains_query_on_owner_partitions", () => {
    const without = built({ utilityPurposes: "run-name" });
    // every template but the orchestration stack is the same text with and without the tag purpose: the control role, the worker session role,
    // the delivery roles and the rest gain nothing
    for (const [name, t] of templates) {
      if (name === "hamh-direct-orchestration") continue;
      expect(JSON.stringify(t.toJSON()), name).toBe(JSON.stringify(without.get(name)!.toJSON()));
    }
    // in the orchestration stack only the run-session role differs, and it differs by exactly the one statement
    const withRoles = Object.values(orch.toJSON().Resources as Record<string, any>).filter((r) => r.Type === "AWS::IAM::Role");
    const withoutRoles = Object.values(without.get("hamh-direct-orchestration")!.toJSON().Resources as Record<string, any>).filter((r) => r.Type === "AWS::IAM::Role");
    expect(withRoles.map((r) => r.Properties.RoleName)).toEqual(withoutRoles.map((r) => r.Properties.RoleName));
    for (const r of withRoles) {
      const other = withoutRoles.find((o) => o.Properties.RoleName === r.Properties.RoleName)!;
      if (r.Properties.RoleName === RUN_SESSION) continue;
      expect(JSON.stringify(r), r.Properties.RoleName).toBe(JSON.stringify(other));
    }
    const gained = policyOf(orch, RUN_SESSION);
    const base = policyOf(without.get("hamh-direct-orchestration")!, RUN_SESSION);
    expect(gained.filter((s) => !base.some((b) => JSON.stringify(b) === JSON.stringify(s))).map((s) => s.Sid)).toEqual(["TagVocabulary"]);
    expect(base.every((b) => gained.some((s) => JSON.stringify(s) === JSON.stringify(b)))).toBe(true);
    expect(gained).toHaveLength(base.length + 1);
  });

  test("TagVocabulary exists once in the nine templates and no role but run-session's names the tag attributes", () => {
    let count = 0;
    for (const [name, t] of templates) {
      const text = JSON.stringify(t.toJSON());
      count += text.split('"TagVocabulary"').length - 1;
      if (name !== "hamh-direct-orchestration") expect(text, name).not.toContain("runTagsUser");
    }
    expect(count).toBe(1);
  });

  test("the control role's query grants are the ones control.test.ts lists: the statement list of the control role is unchanged by the tag purpose", () => {
    const control = templates.get("hamh-direct-control")!;
    const without = built({ utilityPurposes: "run-name" }).get("hamh-direct-control")!;
    expect(policyOf(control, "hamh-direct-control-api")).toEqual(policyOf(without, "hamh-direct-control-api"));
    expect(policyOf(control, "hamh-direct-control-api").map((s) => s.Sid)).not.toContain("TagVocabulary");
  });

  test("the run-session inline policy, with the new statement, is under 10,240 characters", () => {
    const text = JSON.stringify({ Version: "2012-10-17", Statement: policyOf(orch, RUN_SESSION) }).replace(/\s/g, "");
    expect(text.length).toBeLessThan(10_240);
  });
});
