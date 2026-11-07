// The stage setting of the tool layer, `HAMH_TOOLS_MODE`, and the worker runtime variable it becomes, `AGENTD_TOOLS_MODE`.
//
// The worker daemon (agentd) reads `AGENTD_TOOLS_MODE` to switch its tool server off or on. The sibling research module reads its
// `AGENTD_RESEARCH_MODE` leniently (anything but off means on, an unset variable means on), so the tool server may well treat an unset variable as
// on.
import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { buildApp } from "../lib/app.ts";
import { DEFAULT_TOOLS_MODE, TOOLS_MODES, resolveToolsMode } from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { phase2Images, resourcesOf, synth, template } from "./helpers.ts";

const TABLE = JSON.parse(readFileSync(new URL("./fixtures/tools-mode.json", import.meta.url), "utf8")) as {
  default: string;
  accept: { setting: string | null; mode: string }[];
  reject: string[];
};
const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;
const VAR = "AGENTD_TOOLS_MODE";

const runtimeEnv = (toolsMode?: string): Record<string, string> => {
  const t = template("direct", "runtimes", { ...phase2Images(), ...(toolsMode === undefined ? {} : { toolsMode }) });
  return (Object.values(resourcesOf(t, "AWS::BedrockAgentCore::Runtime"))[0].Properties as any).EnvironmentVariables;
};

/** The two calls of the custom resource that re-applies requireMMDSV2 (it sends the whole configuration, the environment included). */
const customResource = (toolsMode?: string): { Create: string; Update: string } => {
  const t = template("direct", "runtimes", { ...phase2Images(), ...(toolsMode === undefined ? {} : { toolsMode }) });
  const p = (Object.values(t.findResources("Custom::AWS"))[0] as any).Properties;
  return { Create: JSON.stringify(p.Create), Update: JSON.stringify(p.Update) };
};

describe("the words of HAMH_TOOLS_MODE", () => {
  test("the setting has two words and defaults to off", () => {
    expect([...TOOLS_MODES]).toEqual(["off", "on"]);
    expect(DEFAULT_TOOLS_MODE).toBe("off");
    expect(TABLE.default).toBe("off");
  });

  test.each(TABLE.accept.map((c) => [String(c.setting), c] as const))("resolveToolsMode(%s) is a concrete word", (_name, c) => {
    expect(resolveToolsMode(c.setting ?? undefined)).toBe(c.mode);
  });

  test.each(TABLE.reject.map((w) => [w] as const))("resolveToolsMode(%s) throws and names the setting and the words", (word) => {
    expect(() => resolveToolsMode(word)).toThrow(/HAMH_TOOLS_MODE/);
    expect(() => resolveToolsMode(word)).toThrow(/off, on/);
  });

  test("an unknown word never falls back to a mode", () => {
    for (const word of TABLE.reject) {
      let got: string | undefined;
      try {
        got = resolveToolsMode(word);
      } catch {
        got = undefined;
      }
      expect(got, word).toBeUndefined();
    }
  });
});

describe("CLI entry", () => {
  test("unset or blank adds no option; a word passes through as the operator typed it, trimmed; any other word stops the deploy by name", () => {
    for (const env of [{}, { HAMH_TOOLS_MODE: "" }, { HAMH_TOOLS_MODE: "   " }]) {
      expect("toolsMode" in entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, ...env }), JSON.stringify(env)).toBe(false);
    }
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_TOOLS_MODE: " ON " }).toolsMode).toBe("ON");
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_TOOLS_MODE: "off" }).toolsMode).toBe("off");
    for (const bad of TABLE.reject) {
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_TOOLS_MODE: bad }), bad).toThrow(/HAMH_TOOLS_MODE/);
    }
  });

  test("buildApp refuses a bad word too, so a caller that skips the entry is stopped", () => {
    expect(() => buildApp({ stage: "direct", toolsMode: "auto" })).toThrow(/HAMH_TOOLS_MODE/);
  });
});

describe("the worker runtime", () => {
  test("by default the runtime says AGENTD_TOOLS_MODE=off, so an agentd that reads a missing variable as on cannot turn the layer on", () => {
    expect(runtimeEnv()[VAR]).toBe("off");
    expect(runtimeEnv("off")[VAR]).toBe("off");
  });

  test("on and ON become on; the variable only ever holds off or on", () => {
    expect(runtimeEnv("on")[VAR]).toBe("on");
    expect(runtimeEnv(" ON ")[VAR]).toBe("on");
    for (const c of TABLE.accept) expect(["off", "on"], String(c.setting)).toContain(runtimeEnv(c.setting ?? undefined)[VAR]);
  });

  test("the mode is the only environment variable that changes", () => {
    const { [VAR]: off, ...restOff } = runtimeEnv("off");
    const { [VAR]: on, ...restOn } = runtimeEnv("on");
    expect([off, on]).toEqual(["off", "on"]);
    expect(restOn).toEqual(restOff);
  });

  test("the custom resource that sends the whole configuration carries the same mode in its create and its update call", () => {
    for (const mode of ["off", "on"]) {
      const cr = customResource(mode);
      for (const call of [cr.Create, cr.Update]) {
        expect(call, mode).toContain(`\\"${VAR}\\":\\"${mode}\\"`);
        expect(call.split(VAR).length - 1, `${mode}: once`).toBe(1);
      }
    }
    expect(customResource().Create).toContain(`\\"${VAR}\\":\\"off\\"`);
  });

  test("a change of the mode reruns the custom resource, as a change of the revision does (a new runtime version)", () => {
    expect(customResource("on").Create).not.toEqual(customResource("off").Create);
    expect(customResource("on").Update).not.toEqual(customResource("off").Update);
    expect(customResource().Create).toEqual(customResource("off").Create);
  });
});

describe("what the setting leaves alone", () => {
  test("with no worker image there is no runtime, so the setting changes nothing in phase 1", () => {
    const off = JSON.stringify(template("direct", "runtimes").toJSON());
    const on = JSON.stringify(template("direct", "runtimes", { toolsMode: "on" }).toJSON());
    expect(on).toEqual(off);
    expect(on).not.toContain(VAR);
  });

  test("with images, every stack but runtimes is byte for byte the same whatever the mode", () => {
    const off = synth("direct", phase2Images()).templates;
    const on = synth("direct", { ...phase2Images(), toolsMode: "on" }).templates;
    expect([...on.keys()].sort()).toEqual([...off.keys()].sort());
    for (const [name, t] of off) {
      if (name === "hamh-direct-runtimes") continue;
      expect(JSON.stringify(on.get(name)!.toJSON()), name).toEqual(JSON.stringify(t.toJSON()));
    }
    expect(JSON.stringify(on.get("hamh-direct-runtimes")!.toJSON())).not.toEqual(JSON.stringify(off.get("hamh-direct-runtimes")!.toJSON()));
  });

  test("the official stage may turn the layer on: the release gate decides when, not the app", () => {
    expect(() => buildApp({ stage: "direct", ...phase2Images(), toolsMode: "on" })).not.toThrow();
  });
});
