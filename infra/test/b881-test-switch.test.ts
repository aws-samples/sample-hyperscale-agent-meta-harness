// The deploy side of the switch of the live test row of the storage notice.
//
// A session's storage can refuse writes while its files leave room, and a run then says so. A real refusal cannot be made on demand, so one live test row
// uses a switch of the worker: `AGENTD_TEST_STORAGE_REFUSAL=1` makes a worker answer one test write with a test answer. This file holds the deploy side: the
// runtime carries the variable only when the deploy of a test stage asked for it,
// no settings record keeps it, and for a stage whose stated kind is not `test` the word stops the synth.
import { describe, expect, test, vi } from "vitest";
import { resolveTestStorageRefusal, TEST_STORAGE_REFUSAL_VAR } from "../lib/config.ts";
import { VARIABLES } from "../lib/env.ts";
import { withCdkJsonContext } from "./helpers.ts";
import { THREE_ACCOUNTS as DOC, withEnv, writeAccountsFile } from "./accounts-three.ts";

const ENTRY = DOC.accounts[1];
const OTHER = { ...DOC.accounts[0], role: "active", profile: "other-profile" };
const AS_TEST = writeAccountsFile({ ...DOC, accounts: [OTHER, { ...ENTRY, role: "test" }] });
const AS_ACTIVE = writeAccountsFile({ ...DOC, accounts: [{ ...ENTRY, role: "active" }] });
const AS_HISTORY = writeAccountsFile({ ...DOC, accounts: [OTHER, { ...ENTRY, role: "history" }] });
const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;

/**
 * The environment of the worker runtime of a stage of the entry in `file`, built with images, and the two calls of the custom resource that re-sends it. `kind` is
 * the word of HAMH_STAGE_KIND (undefined: not set, so no kind is stated for the stage).
 */
function runtime(file: string, option?: string, kind?: string): Promise<{ env: Record<string, string>; calls: string; role: string; all: string }> {
  return withEnv({ HAMH_ACCOUNTS_FILE: file, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: kind, HAMH_NO_DEPRECATION_NOTICES: "1" }, async () => {
    vi.resetModules();
    const cfg = await import("../lib/config.ts");
    const { buildApp } = await import("../lib/app.ts");
    const { Template } = await import("aws-cdk-lib/assertions");
    const image = (repo: string) => `${ENTRY.account}.dkr.ecr.${ENTRY.region}.amazonaws.com/hamh-direct-${repo}@sha256:${"a".repeat(64)}`;
    const app = withCdkJsonContext(() =>
      buildApp({ stage: "direct", workerImage: image("worker-python"), deliveryImage: image("delivery"), ...(option === undefined ? {} : { testStorageRefusal: option }) }));
    const stacks = app.node.children.filter((c) => "templateOptions" in c) as any[];
    const runtimes = Template.fromStack(stacks.find((s) => s.stackName === "hamh-direct-runtimes"));
    const env = (Object.values(runtimes.findResources("AWS::BedrockAgentCore::Runtime"))[0] as any).Properties.EnvironmentVariables;
    const cr = (Object.values(runtimes.findResources("Custom::AWS"))[0] as any).Properties;
    const all = stacks.filter((s) => s.stackName !== "hamh-direct-runtimes").map((s) => JSON.stringify(Template.fromStack(s).toJSON())).join("\n");
    return { env, calls: JSON.stringify(cr.Create) + JSON.stringify(cr.Update), role: cfg.TARGET.role, all };
  });
}

describe("b881 the words of HAMH_TEST_STORAGE_REFUSAL", () => {
  test("unset, blank and off leave the runtime alone for every stated kind; on is for the stated kind test alone; another word names the setting", () => {
    expect(TEST_STORAGE_REFUSAL_VAR).toBe("AGENTD_TEST_STORAGE_REFUSAL");
    for (const kind of ["official", "test", undefined] as const) {
      for (const word of [undefined, "", "  ", "off", " OFF "]) expect(resolveTestStorageRefusal(word, kind), `${kind} ${word}`).toBe(false);
    }
    expect(resolveTestStorageRefusal("on", "test")).toBe(true);
    expect(resolveTestStorageRefusal(" ON ", "test")).toBe(true);
    for (const kind of ["official", undefined] as const) {
      expect(() => resolveTestStorageRefusal("on", kind), String(kind)).toThrow(/HAMH_TEST_STORAGE_REFUSAL=on is for a stage of the kind test/);
      expect(() => resolveTestStorageRefusal("on", kind), String(kind)).toThrow(new RegExp(`The kind stated for this stage is ${kind ?? "none"}`));
    }
    for (const bad of ["1", "true", "yes", "auto"]) {
      for (const kind of ["official", "test"] as const) expect(() => resolveTestStorageRefusal(bad, kind), bad).toThrow(/HAMH_TEST_STORAGE_REFUSAL=.* is not one of off, on/);
    }
  });

  test("the variable of the deploy is read like every documented stage setting: an empty value is not set, and it chooses no account", () => {
    expect(VARIABLES.TEST_STORAGE_REFUSAL).toEqual(VARIABLES.TOOLS_MODE);
    expect(VARIABLES.TEST_STORAGE_REFUSAL.emptyIsUnset).toBe(true);
    expect(VARIABLES.TEST_STORAGE_REFUSAL.selector).toBe(false);
  });
});

describe("b881 the worker runtime of a stage", () => {
  test("a test stage whose deploy did not ask has no such variable, in the runtime and in the custom resource that re-sends its configuration", async () => {
    for (const option of [undefined, "off", ""]) {
      const r = await runtime(AS_TEST, option);
      expect(r.role).toBe("test");
      expect(TEST_STORAGE_REFUSAL_VAR in r.env, String(option)).toBe(false);
      expect(r.calls, String(option)).not.toContain(TEST_STORAGE_REFUSAL_VAR);
    }
  });

  test("a test stage whose deploy asked carries the variable with the value 1, once, and nothing else of the stage changes", async () => {
    const off = await runtime(AS_TEST);
    const on = await runtime(AS_TEST, "on", "test");
    expect(on.role).toBe("test");
    const { [TEST_STORAGE_REFUSAL_VAR]: flag, ...rest } = on.env;
    expect(flag).toBe("1");
    expect(rest).toEqual(off.env);
    // The custom resource sends the whole configuration: the variable is in its create and in its update call, so the change makes a new runtime version.
    expect(on.calls.split(TEST_STORAGE_REFUSAL_VAR).length - 1).toBe(2);
    expect(on.calls).toContain(`\\"${TEST_STORAGE_REFUSAL_VAR}\\":\\"1\\"`);
    // No other stack of the stage differs: no grant, no setting of a function, no parameter.
    expect(on.all).toEqual(off.all);
    expect(on.all).not.toContain(TEST_STORAGE_REFUSAL_VAR);
  });

  test("without the kind test stated for the stage the word stops the synth, and such a stage never carries the variable", async () => {
    for (const file of [AS_ACTIVE, AS_HISTORY]) {
      await expect(runtime(file, "on")).rejects.toThrow(/HAMH_TEST_STORAGE_REFUSAL=on is for a stage of the kind test/);
      const r = await runtime(file);
      expect(r.role).not.toBe("test");
      expect(TEST_STORAGE_REFUSAL_VAR in r.env).toBe(false);
      expect(r.calls).not.toContain(TEST_STORAGE_REFUSAL_VAR);
    }
  });

  test("the CLI entry reads the word of the deploy and refuses it for a stage whose stated kind is not test", async () => {
    const entry = (file: string, vars: Record<string, string | undefined>) =>
      withEnv({ HAMH_ACCOUNTS_FILE: file, HAMH_AWS_PROFILE: ENTRY.profile, HAMH_KEEP_TAG: undefined, HAMH_STAGE_KIND: undefined, HAMH_NO_DEPRECATION_NOTICES: "1" }, async () => {
        vi.resetModules();
        const { entryOptions } = await import("../lib/entry.ts");
        return entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, ...vars });
      });
    expect("testStorageRefusal" in (await entry(AS_TEST, {}))).toBe(false);
    expect("testStorageRefusal" in (await entry(AS_TEST, { HAMH_TEST_STORAGE_REFUSAL: "  " }))).toBe(false);
    expect((await entry(AS_TEST, { HAMH_TEST_STORAGE_REFUSAL: " on ", HAMH_STAGE_KIND: "test" })).testStorageRefusal).toBe("on");
    await expect(entry(AS_ACTIVE, { HAMH_TEST_STORAGE_REFUSAL: "on" })).rejects.toThrow(/HAMH_TEST_STORAGE_REFUSAL=on is for a stage of the kind test/);
    await expect(entry(AS_TEST, { HAMH_TEST_STORAGE_REFUSAL: "maybe" })).rejects.toThrow(/HAMH_TEST_STORAGE_REFUSAL/);
    // No other variable of the deploy turns it on.
    expect("testStorageRefusal" in (await entry(AS_TEST, { HAMH_TOOLS_MODE: "on", AGENTD_TEST_STORAGE_REFUSAL: "1" }))).toBe(false);
  });
});
