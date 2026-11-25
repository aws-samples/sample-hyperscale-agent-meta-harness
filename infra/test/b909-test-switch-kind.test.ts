// The kind of the stage decides who may turn the test switch of the storage notice on.
//
// `HAMH_TEST_STORAGE_REFUSAL=on` makes the worker runtimes of a stage carry `AGENTD_TEST_STORAGE_REFUSAL=1`. The word is taken only when the kind `test`
// is stated for the stage, with `HAMH_STAGE_KIND=test` in the environment the word is read from. The role of the account entry states no kind: an entry
// with the role `active`, which is what `setup.sh` generates, takes the word on a test stage, and an entry with the role `test` does not take it without the kind
// word. `off`, blank and unset never throw and never add the variable. The deploy engine hands the app the kind word it resolved from the export or from the
// stage's settings record, and refuses the same cases before the synth (scripts/lib/deploy.py `refuse_test_switch`).
//
// A synthesis shows what the template of a stage would hold. That a deployed runtime carries or loses the variable is shown by a read of the live runtime.
import { describe, expect, test, vi } from "vitest";
import { resolveTestStorageRefusal, statedStageKind, TEST_STORAGE_REFUSAL_VAR } from "../lib/config.ts";
import { EARLIER_PREFIX } from "../lib/env.ts";
import { withCdkJsonContext } from "./helpers.ts";
import { THREE_ACCOUNTS as DOC, withEnv, writeAccountsFile } from "./accounts-three.ts";

const ENTRY = DOC.accounts[1];
const OTHER = { ...DOC.accounts[0], role: "active", profile: "other-profile" };
const FILES = {
  active: writeAccountsFile({ ...DOC, accounts: [{ ...ENTRY, role: "active" }] }),
  history: writeAccountsFile({ ...DOC, accounts: [OTHER, { ...ENTRY, role: "history" }] }),
  test: writeAccountsFile({ ...DOC, accounts: [OTHER, { ...ENTRY, role: "test" }] }),
} as const;
type Role = keyof typeof FILES;
const ROLES = Object.keys(FILES) as Role[];
const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;
const REFUSED = /HAMH_TEST_STORAGE_REFUSAL=on is for a stage of the kind test/;

interface Built {
  /** The environment of each worker runtime of the stage. */
  envs: Record<string, string>[];
  /** The create and the update call of each custom resource that sends a runtime's configuration again. */
  calls: string;
  role: string;
  kind: string;
  /** Every other stack of the stage, as text. */
  all: string;
}

/** The worker runtimes of a stage of the entry with the role `role`, built with images, with `kind` as the word of HAMH_STAGE_KIND (undefined: not set). */
function stage(role: Role, kind: string | undefined, option?: string, instances = false): Promise<Built> {
  return withEnv({ HAMH_ACCOUNTS_FILE: FILES[role], HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: kind, HAMH_NO_DEPRECATION_NOTICES: "1" }, async () => {
    vi.resetModules();
    const cfg = await import("../lib/config.ts");
    const { buildApp } = await import("../lib/app.ts");
    const { Template } = await import("aws-cdk-lib/assertions");
    const image = (repo: string) => `${ENTRY.account}.dkr.ecr.${ENTRY.region}.amazonaws.com/hamh-direct-${repo}@sha256:${"a".repeat(64)}`;
    const app = withCdkJsonContext(() =>
      buildApp({
        stage: "direct", workerImage: image("worker-python"), deliveryImage: image("delivery"),
        ...(option === undefined ? {} : { testStorageRefusal: option }), ...(instances ? { computeInstances: "on" } : {}),
      }));
    const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
    const runtimes = Template.fromStack(stacks.find((s) => s.stackName === "hamh-direct-runtimes"));
    const envs = Object.values(runtimes.findResources("AWS::BedrockAgentCore::Runtime")).map((r: any) => r.Properties.EnvironmentVariables);
    const calls = Object.values(runtimes.findResources("Custom::AWS")).map((r: any) => JSON.stringify(r.Properties.Create) + JSON.stringify(r.Properties.Update)).join("\n");
    const all = stacks.filter((s) => s.stackName !== "hamh-direct-runtimes").map((s) => JSON.stringify(Template.fromStack(s).toJSON())).join("\n");
    return { envs, calls, role: cfg.TARGET.role, kind: cfg.STAGE_KIND, all };
  });
}

const count = (text: string, part: string) => text.split(part).length - 1;

describe("b909 the kind stated for a stage", () => {
  test("the word of HAMH_STAGE_KIND states the kind, and nothing else does", () => {
    expect(statedStageKind({})).toBeUndefined();
    expect(statedStageKind({ HAMH_STAGE_KIND: "" })).toBeUndefined();
    expect(statedStageKind({ HAMH_STAGE_KIND: "  " })).toBeUndefined();
    expect(statedStageKind({ HAMH_STAGE_KIND: " Test " })).toBe("test");
    expect(statedStageKind({ HAMH_STAGE_KIND: "OFFICIAL" })).toBe("official");
    expect(statedStageKind({ [`${EARLIER_PREFIX}STAGE_KIND`]: "test", HAMH_NO_DEPRECATION_NOTICES: "1" })).toBe("test");
    expect(() => statedStageKind({ HAMH_STAGE_KIND: "prod" })).toThrow(/HAMH_STAGE_KIND="prod" is not one of official, test/);
  });

  test("c4: on is taken exactly when the stated kind is test, and off is never refused", () => {
    for (const kind of ["test", "official", undefined] as const) {
      for (const word of [undefined, "", "  ", "off", " OFF "]) expect(resolveTestStorageRefusal(word, kind), `${kind} ${word}`).toBe(false);
      for (const word of ["on", " ON "]) {
        if (kind === "test") expect(resolveTestStorageRefusal(word, kind)).toBe(true);
        else expect(() => resolveTestStorageRefusal(word, kind), String(kind)).toThrow(REFUSED);
      }
      for (const bad of ["1", "true", "yes", "auto"]) expect(() => resolveTestStorageRefusal(bad, kind), bad).toThrow(/HAMH_TEST_STORAGE_REFUSAL=.* is not one of off, on/);
    }
    // The error names the kind that was stated and the variable that states it, and no role.
    expect(() => resolveTestStorageRefusal("on", "official")).toThrow(/The kind stated for this stage is official \(HAMH_STAGE_KIND\)/);
    expect(() => resolveTestStorageRefusal("on", undefined)).toThrow(/The kind stated for this stage is none \(HAMH_STAGE_KIND\)/);
    for (const kind of ["official", undefined] as const) {
      let text = "";
      try { resolveTestStorageRefusal("on", kind); } catch (e) { text = String(e); }
      expect(text).not.toMatch(/role/);
    }
  });
});

describe("b909 the worker runtimes of a stage", () => {
  test("c1: a stage of an active entry whose kind word is test carries the variable with 1, once, and nothing else of the stage changes", async () => {
    const off = await stage("active", "test");
    const on = await stage("active", "test", "on");
    expect([on.role, on.kind]).toEqual(["active", "test"]);
    expect(on.envs.length).toBe(1);
    const { [TEST_STORAGE_REFUSAL_VAR]: flag, ...rest } = on.envs[0];
    expect(flag).toBe("1");
    expect(rest).toEqual(off.envs[0]);
    // The custom resource sends the whole configuration: the variable is in its create and in its update call, so the change makes a new runtime version.
    expect(count(on.calls, TEST_STORAGE_REFUSAL_VAR)).toBe(2);
    expect(on.calls).toContain(`\\"${TEST_STORAGE_REFUSAL_VAR}\\":\\"1\\"`);
    expect(off.calls).not.toContain(TEST_STORAGE_REFUSAL_VAR);
    // No other stack of the stage differs: no grant, no setting of a function, no parameter.
    expect(on.all).toEqual(off.all);
    expect(on.all).not.toContain(TEST_STORAGE_REFUSAL_VAR);
  });

  test("c1: with the Instances part both worker runtimes carry the variable", async () => {
    const off = await stage("active", "test", undefined, true);
    const on = await stage("active", "test", "on", true);
    expect(on.envs.length).toBe(2);
    for (const env of on.envs) expect(env[TEST_STORAGE_REFUSAL_VAR]).toBe("1");
    for (const env of off.envs) expect(TEST_STORAGE_REFUSAL_VAR in env).toBe(false);
    // The custom resource sends the configuration of the first worker runtime again; the second runtime gets its environment from its own resource alone.
    expect(count(on.calls, TEST_STORAGE_REFUSAL_VAR)).toBe(2);
    expect(on.all).toEqual(off.all);
  });

  test("c2 and c4: for every role the variable is present exactly when the kind word is test and the word is on; any other target stops the synth", async () => {
    for (const role of ROLES) {
      for (const kind of ["test", "official", undefined]) {
        if (kind === "test") {
          const on = await stage(role, kind, "on");
          expect(on.role).toBe(role);
          expect(on.envs.map((env) => env[TEST_STORAGE_REFUSAL_VAR]), `${role} ${kind}`).toEqual(["1"]);
        } else {
          await expect(stage(role, kind, "on"), `${role} ${kind}`).rejects.toThrow(REFUSED);
        }
      }
    }
    // The role test gives the kind test and states none: the error says so and names no role.
    await expect(stage("test", undefined, "on")).rejects.toThrow(/The kind stated for this stage is none \(HAMH_STAGE_KIND\)/);
    await expect(stage("test", "official", "on")).rejects.toThrow(/The kind stated for this stage is official \(HAMH_STAGE_KIND\)/);
  });

  test("c3: off, blank and unset give no variable for every kind", async () => {
    for (const [role, kind] of [["active", "test"], ["active", "official"], ["active", undefined], ["test", undefined], ["history", "test"]] as const) {
      for (const option of [undefined, "off", "  "]) {
        const r = await stage(role, kind, option);
        for (const env of r.envs) expect(TEST_STORAGE_REFUSAL_VAR in env, `${role} ${kind} ${option}`).toBe(false);
        expect(r.calls, `${role} ${kind} ${option}`).not.toContain(TEST_STORAGE_REFUSAL_VAR);
      }
    }
  });
});

describe("b909 the CLI entry", () => {
  /** entryOptions for the entry with the role `role`, with `vars` as its environment and `outer` as the word of HAMH_STAGE_KIND in the process environment. */
  const entry = (role: Role, vars: Record<string, string | undefined>, outer?: string) =>
    withEnv({ HAMH_ACCOUNTS_FILE: FILES[role], HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: outer, HAMH_NO_DEPRECATION_NOTICES: "1" }, async () => {
      vi.resetModules();
      const { entryOptions } = await import("../lib/entry.ts");
      return entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_NO_DEPRECATION_NOTICES: "1", ...vars });
    });

  test("c5: the word and the kind word are read from the same environment, each under both of its names", async () => {
    expect((await entry("active", { HAMH_TEST_STORAGE_REFUSAL: " on ", HAMH_STAGE_KIND: "test" })).testStorageRefusal).toBe("on");
    await expect(entry("active", { HAMH_TEST_STORAGE_REFUSAL: "on" })).rejects.toThrow(REFUSED);
    await expect(entry("active", { HAMH_TEST_STORAGE_REFUSAL: "on", HAMH_STAGE_KIND: "official" })).rejects.toThrow(REFUSED);
    await expect(entry("test", { HAMH_TEST_STORAGE_REFUSAL: "on" })).rejects.toThrow(REFUSED);
    // A kind word of another environment than the one the word is read from states nothing.
    await expect(entry("active", { HAMH_TEST_STORAGE_REFUSAL: "on" }, "test")).rejects.toThrow(REFUSED);
    // The earlier spelling of each name is read the same way.
    const earlier = (suffix: string) => `${EARLIER_PREFIX}${suffix}`;
    expect((await entry("active", { [earlier("TEST_STORAGE_REFUSAL")]: "on", [earlier("STAGE_KIND")]: "test" })).testStorageRefusal).toBe("on");
    await expect(entry("active", { [earlier("TEST_STORAGE_REFUSAL")]: "on", [earlier("STAGE_KIND")]: "official" })).rejects.toThrow(REFUSED);
    // off and no word add no option for any kind.
    expect("testStorageRefusal" in (await entry("active", { HAMH_STAGE_KIND: "test" }))).toBe(false);
    expect((await entry("active", { HAMH_TEST_STORAGE_REFUSAL: "off", HAMH_STAGE_KIND: "official" })).testStorageRefusal).toBe("off");
  });
});
