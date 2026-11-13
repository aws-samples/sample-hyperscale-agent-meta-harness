// The keep tag and the stage kind as stage settings: `HAMH_KEEP_TAG` (on or off) and `HAMH_STAGE_KIND` (official or test).
//
// A stage keeps the two words it was deployed with, whatever role the account file of a later deploy gives its entry: the account file that `setup.sh` writes holds
// one `active` entry, and without the two settings an update of a stage that a `test` entry deployed would take the tag off every resource, turn the stage official
// and, because the name of the Instances capacity provider ends in a hash that covers the tags its machines carry, replace the provider and delete its machines.
// `scripts/lib/deploy.py` resolves the two words for the stage and hands them to every cdk call; the app reads them and falls back to the role. The cases build one
// account entry under both roles and compare what the app synthesizes.
import { describe, expect, test, vi } from "vitest";
import * as config from "../lib/config.ts";
import { THREE_ACCOUNTS as DOC, withEnv, writeAccountsFile } from "./accounts-three.ts";
import { withCdkJsonContext } from "./helpers.ts";

const KEEP = { "auto-delete": "no" };
const NO_KEEP_TYPE = "AWS::Route53Resolver::ResolverQueryLoggingConfig";
const NO_KEEP_STACK = "hamh-direct-network";
const ROLES = ["active", "history", "test"] as const;

// One account entry in two accounts files. `AS_ACTIVE` is the file as setup.sh generates it: that entry alone, role `active`. `AS_TEST` names the same account,
// Region, profile and zones with role `test`, beside another `active` entry, because a file holds exactly one active entry.
const ENTRY = DOC.accounts[1];
const AS_ACTIVE = writeAccountsFile({ ...DOC, accounts: [{ ...ENTRY, role: "active" }] });
const AS_TEST = writeAccountsFile({ ...DOC, accounts: [{ ...DOC.accounts[0], role: "active", profile: "other-profile" }, { ...ENTRY, role: "test" }] });

type Vars = Record<string, string | undefined>;
interface Built {
  keepTags: Record<string, string>;
  stageKind: string;
  role: string;
  providerName: string;
  /** The nine templates by stack name. */
  templates: Map<string, any>;
  /** The CloudFormation stack tags by stack name (they are in the manifest, not in the template). */
  stackTags: Map<string, Record<string, string>>;
}

// A fresh module graph per selection and setting: lib/config.ts reads the accounts file, the profile and the two settings when it loads.
const cache = new Map<string, Promise<Built>>();
function build(file: string, vars: Vars = {}): Promise<Built> {
  const key = JSON.stringify([file, vars]);
  if (!cache.has(key)) {
    cache.set(key, withEnv({
      HAMH_ACCOUNTS_FILE: file, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: undefined, HAMH_NO_DEPRECATION_NOTICES: "1", ...vars,
    }, async () => {
      vi.resetModules();
      const cfg = await import("../lib/config.ts");
      const { buildApp } = await import("../lib/app.ts");
      const { Template } = await import("aws-cdk-lib/assertions");
      const image = (repo: string) => `${ENTRY.account}.dkr.ecr.${ENTRY.region}.amazonaws.com/hamh-direct-${repo}@sha256:${"a".repeat(64)}`;
      // Both images and the Instances part, so that every resource of the stage synthesizes: the runtimes, the delivery functions and the capacity provider.
      const app = withCdkJsonContext(() => buildApp({ stage: "direct", workerImage: image("worker-python"), deliveryImage: image("delivery"), computeInstances: "on" }));
      const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
      const templates = new Map<string, any>(stacks.map((s) => [s.stackName, Template.fromStack(s).toJSON()]));
      const stackTags = new Map<string, Record<string, string>>(app.synth().stacks.map((s: any) => [s.stackName, s.tags]));
      return {
        keepTags: cfg.KEEP_TAGS, stageKind: cfg.STAGE_KIND, role: cfg.TARGET.role, providerName: cfg.names.capacityProvider(cfg.stageConfig("direct")), templates, stackTags,
      };
    }));
  }
  return cache.get(key)!;
}

/** Several builds, one after the other: a build sets process.env for its own length, so two never run at once. */
async function builds(...specs: [string, Vars?][]): Promise<Built[]> {
  const out: Built[] = [];
  for (const [file, vars] of specs) out.push(await build(file, vars));
  return out;
}

/** Loads lib/config.ts alone under the entry and the variables; a value that is no word of a setting rejects here, which is where a synth stops. */
function loadConfig(file: string, vars: Vars): Promise<typeof config> {
  return withEnv({ HAMH_ACCOUNTS_FILE: file, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: undefined, ...vars }, async () => {
    vi.resetModules();
    return await import("../lib/config.ts");
  });
}

function tagMap(tags: unknown): Record<string, string> {
  if (Array.isArray(tags)) return Object.fromEntries(tags.map((t: { Key: string; Value: string }) => [t.Key, t.Value]));
  if (tags && typeof tags === "object") return tags as Record<string, string>;
  return {};
}

/** Every resource that carries the project tag, as `<stack>/<logical id>` with its tags. */
function resourceTags(b: Built): Record<string, Record<string, string>> {
  const out: Record<string, Record<string, string>> = {};
  for (const [name, json] of b.templates) {
    for (const [id, r] of Object.entries(json.Resources ?? {}) as [string, { Properties?: { Tags?: unknown } }][]) {
      const tags = tagMap(r.Properties?.Tags);
      if (tags.project !== undefined) out[`${name}/${id}`] = tags;
    }
  }
  return out;
}

function provider(b: Built): any {
  const found = Object.values(b.templates.get("hamh-direct-runtimes").Resources).filter((r: any) => r.Type === "AWS::BedrockAgentCore::CapacityProvider") as any[];
  expect(found).toHaveLength(1);
  return found[0].Properties;
}

function researchKinds(b: Built): Record<string, string> {
  const fns = Object.values(b.templates.get("hamh-direct-control").Resources).filter((r: any) => r.Type === "AWS::Lambda::Function") as any[];
  return Object.fromEntries(fns.filter((f) => /-research-(api|job)$/.test(f.Properties.FunctionName))
    .map((f) => [f.Properties.FunctionName, f.Properties.Environment.Variables.HAMH_STAGE_KIND]));
}

describe("the two words and their fallback to the role", () => {
  test("unset, empty or blank, the role decides: the keep tag is on for a test entry alone, and the stage is official for the active entry alone", () => {
    expect(config.KEEP_TAG_OF_ROLE).toEqual({ active: "off", history: "off", test: "on" });
    expect(config.STAGE_KIND_OF_ROLE).toEqual({ active: "official", history: "test", test: "test" });
    for (const role of ROLES) {
      for (const unset of [undefined, "", " ", "\t\n"]) {
        expect(config.resolveKeepTag(unset, role), `${role} ${JSON.stringify(unset)}`).toBe(role === "test" ? "on" : "off");
        expect(config.resolveStageKind(unset, role), `${role} ${JSON.stringify(unset)}`).toBe(role === "active" ? "official" : "test");
      }
    }
  });

  test("a word that is set wins over every role, in any letter case and with ASCII whitespace around it", () => {
    expect([...config.KEEP_TAG_WORDS]).toEqual(["on", "off"]);
    expect([...config.STAGE_KINDS]).toEqual(["official", "test"]);
    for (const role of ROLES) {
      for (const [given, want] of [["on", "on"], ["off", "off"], ["ON", "on"], ["Off", "off"], [" on\n", "on"], ["\toFF ", "off"]] as const) {
        expect(config.resolveKeepTag(given, role), `${role} ${JSON.stringify(given)}`).toBe(want);
      }
      for (const [given, want] of [["official", "official"], ["test", "test"], ["OFFICIAL", "official"], ["Test", "test"], [" test\n", "test"]] as const) {
        expect(config.resolveStageKind(given, role), `${role} ${JSON.stringify(given)}`).toBe(want);
      }
    }
  });

  test("any other word throws a plain sentence that names the variable, the value and the words it takes", () => {
    for (const bad of ["yes", "true", "no", "1", "auto-delete", "on off", "default"]) {
      expect(() => config.resolveKeepTag(bad, "active"), bad).toThrow(`HAMH_KEEP_TAG="${bad}" is not one of on, off`);
    }
    for (const bad of ["prod", "active", "officially", "tests", "on", "default"]) {
      expect(() => config.resolveStageKind(bad, "test"), bad).toThrow(`HAMH_STAGE_KIND="${bad}" is not one of official, test`);
    }
    expect(() => config.resolveKeepTag("yes", "test")).toThrow(/unset or empty means what the role of the account entry gives/);
    expect(() => config.resolveStageKind("prod", "active")).toThrow(/unset or empty means what the role of the account entry gives/);
  });

  test("keepTags and stageKind read the variables of the environment they are given, and the role when it holds neither", () => {
    for (const role of ROLES) {
      const target = { role };
      expect(config.keepTags(target), role).toEqual(role === "test" ? KEEP : {});
      expect(config.keepTags(target, {}), role).toEqual(role === "test" ? KEEP : {});
      expect(config.keepTags(target, { HAMH_KEEP_TAG: "" }), role).toEqual(role === "test" ? KEEP : {});
      expect(config.keepTags(target, { HAMH_KEEP_TAG: "on" }), role).toEqual(KEEP);
      expect(config.keepTags(target, { HAMH_KEEP_TAG: "off" }), role).toEqual({});
      expect(config.stageKind(target), role).toBe(role === "active" ? "official" : "test");
      expect(config.stageKind(target, { HAMH_STAGE_KIND: "" }), role).toBe(role === "active" ? "official" : "test");
      expect(config.stageKind(target, { HAMH_STAGE_KIND: "test" }), role).toBe("test");
      expect(config.stageKind(target, { HAMH_STAGE_KIND: "official" }), role).toBe("official");
    }
    expect(() => config.keepTags({ role: "active" }, { HAMH_KEEP_TAG: "maybe" })).toThrow(/HAMH_KEEP_TAG/);
    expect(() => config.stageKind({ role: "active" }, { HAMH_STAGE_KIND: "staging" })).toThrow(/HAMH_STAGE_KIND/);
  });
});

describe("a value that is no word of the setting stops the synth when the app loads", () => {
  test("HAMH_KEEP_TAG", async () => {
    await expect(loadConfig(AS_ACTIVE, { HAMH_KEEP_TAG: "maybe" })).rejects.toThrow('HAMH_KEEP_TAG="maybe" is not one of on, off');
  });

  test("HAMH_STAGE_KIND", async () => {
    await expect(loadConfig(AS_TEST, { HAMH_STAGE_KIND: "staging" })).rejects.toThrow('HAMH_STAGE_KIND="staging" is not one of official, test');
  });

  test("the app loads with the words it takes, and with neither set", async () => {
    const both = await loadConfig(AS_ACTIVE, { HAMH_KEEP_TAG: "ON", HAMH_STAGE_KIND: " Test " });
    expect([both.KEEP_TAGS, both.STAGE_KIND]).toEqual([KEEP, "test"]);
    const neither = await loadConfig(AS_ACTIVE, {});
    expect([neither.KEEP_TAGS, neither.STAGE_KIND, neither.TARGET.role]).toEqual([{}, "official", "active"]);
  });
});

describe("unset, the role decides as before", () => {
  test("the active entry: no keep tag anywhere and an official stage", async () => {
    const b = await build(AS_ACTIVE);
    expect([b.role, b.keepTags, b.stageKind]).toEqual(["active", {}, "official"]);
    for (const [name, json] of b.templates) expect(JSON.stringify(json), name).not.toContain("auto-delete");
    for (const [name, tags] of b.stackTags) expect(tags, name).not.toHaveProperty("auto-delete");
    expect(Object.values(researchKinds(b))).toEqual(["official", "official"]);
  });

  test("the same entry with role test: the keep tag on every project-tagged resource and stack but the exception, and a test stage", async () => {
    const b = await build(AS_TEST);
    expect([b.role, b.keepTags, b.stageKind]).toEqual(["test", KEEP, "test"]);
    const tagged = resourceTags(b);
    expect(Object.keys(tagged).length).toBeGreaterThan(60);
    for (const [where, tags] of Object.entries(tagged)) {
      const type = b.templates.get(where.split("/")[0]).Resources[where.split("/")[1]].Type;
      if (type === NO_KEEP_TYPE) expect(tags, where).not.toHaveProperty("auto-delete");
      else expect(tags["auto-delete"], where).toBe("no");
    }
    for (const [name, tags] of b.stackTags) expect(tags["auto-delete"], name).toBe(name === NO_KEEP_STACK ? undefined : "no");
    expect(Object.values(researchKinds(b))).toEqual(["test", "test"]);
  });
});

describe("HAMH_KEEP_TAG=on on a role-active entry", () => {
  test("puts the keep tag on the resources and the stacks exactly as role test does", async () => {
    const [on, asTest, plain] = await builds([AS_ACTIVE, { HAMH_KEEP_TAG: "on" }], [AS_TEST], [AS_ACTIVE]);
    expect([on.role, on.keepTags]).toEqual(["active", KEEP]);
    expect(resourceTags(on)).toEqual(resourceTags(asTest));
    expect(Object.fromEntries(on.stackTags)).toEqual(Object.fromEntries(asTest.stackTags));
    expect(on.stackTags.get(NO_KEEP_STACK)).not.toHaveProperty("auto-delete");
    expect(on.stackTags.get("hamh-direct-runtimes")).toMatchObject(KEEP);
    // Not the same answer whatever the setting says: without it the entry carries no keep tag.
    expect(resourceTags(on)).not.toEqual(resourceTags(plain));
  });

  test("gives the Instances capacity provider the name it has under role test, so an update replaces no provider and deletes no machine", async () => {
    const [on, asTest, plain] = await builds([AS_ACTIVE, { HAMH_KEEP_TAG: "on" }], [AS_TEST], [AS_ACTIVE]);
    expect(on.providerName).toMatch(/^hamh_direct_instances_[0-9a-f]{8}$/);
    expect(on.providerName).toBe(asTest.providerName);
    expect(provider(on).Name).toBe(asTest.providerName);
    expect(provider(on)).toEqual(provider(asTest));
    // The name follows the tag: the same entry without the setting names another provider, which is the replacement the setting prevents.
    expect(plain.providerName).not.toBe(asTest.providerName);
  });

  test("leaves the stage kind to the role: the stage stays official", async () => {
    const on = await build(AS_ACTIVE, { HAMH_KEEP_TAG: "on" });
    expect(on.stageKind).toBe("official");
    expect(Object.values(researchKinds(on))).toEqual(["official", "official"]);
  });
});

describe("HAMH_KEEP_TAG=off on a role-test entry", () => {
  test("takes the keep tag off every resource and stack and names the provider as the entry without the tag does", async () => {
    const [off, plain] = await builds([AS_TEST, { HAMH_KEEP_TAG: "off" }], [AS_ACTIVE]);
    expect([off.role, off.keepTags, off.stageKind]).toEqual(["test", {}, "test"]);
    for (const [name, json] of off.templates) expect(JSON.stringify(json), name).not.toContain("auto-delete");
    for (const [name, tags] of off.stackTags) expect(tags, name).not.toHaveProperty("auto-delete");
    expect(off.providerName).toBe(plain.providerName);
  });
});

describe("HAMH_STAGE_KIND", () => {
  test("test on a role-active entry: the research functions read test, and the fixture provider passes as on a test stage", async () => {
    const b = await build(AS_ACTIVE, { HAMH_STAGE_KIND: "test" });
    expect([b.role, b.stageKind, b.keepTags]).toEqual(["active", "test", {}]);
    expect(researchKinds(b)).toEqual({ "hamh-direct-research-api": "test", "hamh-direct-research-job": "test" });
    await withEnv({ HAMH_ACCOUNTS_FILE: AS_ACTIVE, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: "test" }, async () => {
      vi.resetModules();
      const { buildApp } = await import("../lib/app.ts");
      expect(() => withCdkJsonContext(() => buildApp({ stage: "direct", researchProvider: "fixture" }))).not.toThrow();
    });
  });

  test("unset on a role-active entry: the fixture provider is refused, as before", async () => {
    await withEnv({ HAMH_ACCOUNTS_FILE: AS_ACTIVE, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: undefined }, async () => {
      vi.resetModules();
      const { buildApp } = await import("../lib/app.ts");
      expect(() => withCdkJsonContext(() => buildApp({ stage: "direct", researchProvider: "fixture" }))).toThrow(/fixture.*official|official.*fixture/);
    });
  });

  test("official refuses the fixture provider on any entry, a role-test entry included", async () => {
    const b = await build(AS_TEST, { HAMH_STAGE_KIND: "official" });
    expect([b.role, b.stageKind, b.keepTags]).toEqual(["test", "official", KEEP]);
    expect(researchKinds(b)).toEqual({ "hamh-direct-research-api": "official", "hamh-direct-research-job": "official" });
    for (const file of [AS_TEST, AS_ACTIVE]) {
      await withEnv({ HAMH_ACCOUNTS_FILE: file, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: "official" }, async () => {
        vi.resetModules();
        const { buildApp } = await import("../lib/app.ts");
        expect(() => withCdkJsonContext(() => buildApp({ stage: "direct", researchProvider: "fixture" })), file).toThrow(/fixture.*official|official.*fixture/);
      });
    }
  });
});

describe("the account file setup generates, with both settings kept from a stage that a test entry deployed", () => {
  test("synthesizes that stage again: the nine templates and the stack tags are the ones of role test, so the update changes no resource", async () => {
    const [kept, asTest] = await builds([AS_ACTIVE, { HAMH_KEEP_TAG: "on", HAMH_STAGE_KIND: "test" }], [AS_TEST]);
    expect([kept.role, kept.keepTags, kept.stageKind]).toEqual(["active", KEEP, "test"]);
    expect([...kept.templates.keys()].sort()).toEqual([...asTest.templates.keys()].sort());
    expect(kept.templates.size).toBe(9);
    for (const [name, json] of kept.templates) expect(json, name).toEqual(asTest.templates.get(name));
    expect(Object.fromEntries(kept.stackTags)).toEqual(Object.fromEntries(asTest.stackTags));
    expect(kept.providerName).toBe(asTest.providerName);
  });

  test("and the other way round: a test entry with the tag off and the kind official synthesizes the stage of the active entry", async () => {
    const [back, plain] = await builds([AS_TEST, { HAMH_KEEP_TAG: "off", HAMH_STAGE_KIND: "official" }], [AS_ACTIVE]);
    for (const [name, json] of back.templates) expect(json, name).toEqual(plain.templates.get(name));
    expect(Object.fromEntries(back.stackTags)).toEqual(Object.fromEntries(plain.stackTags));
  });
});
