// Engine routing as stage `direct` carries it: control-api names the run engine, the engine set and the
// supported models; a session runs on the engine the start request names (Cline when it names none). RUN_ENGINE is the engine of a session no route covers.
import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import * as config from "../lib/config.ts";
import { RUN_ENGINE, RUN_ENGINE_SET, RUN_EVALUATOR_MODELS, RUN_SUPPORTED_MODELS, stageConfig } from "../lib/config.ts";
import { resourcesOf, synth, template, type Stage } from "./helpers.ts";

const envOf = (stage: Stage, stack: string, fn: string) => {
  const f = Object.values(resourcesOf(template(stage, stack), "AWS::Lambda::Function")).map((r) => r.Properties as any)
    .find((x) => x.FunctionName === fn);
  return f.Environment.Variables;
};
const configText = () => readFileSync(new URL("../lib/config.ts", import.meta.url), "utf8");

describe("run engine", () => {
  test("the run engine, the engine of a session no route covers, is Cline, and control-api carries it", () => {
    expect(RUN_ENGINE).toBe("cline");
    expect(envOf("direct", "control", "hamh-direct-control-api").ENGINE).toBe(RUN_ENGINE);
  });

  // The list is declared once here, on one line, because scripts, probes and Rust tests read it by that shape
  // (crates/control-api/tests/cost.rs, crates/ham-e2e/tests/live_scope.rs and live_e2e9_models.rs).
  test("the supported models are the five, declared once on one line", () => {
    expect([...RUN_SUPPORTED_MODELS]).toEqual([
      "us.openai.gpt-6.1-sol",
      "us.openai.gpt-6-astra",
      "us.anthropic.claude-opus-5-5",
      "global.anthropic.claude-sonnet-5-5",
      "zai.glm-5",
    ]);
    expect(/^export const RUN_SUPPORTED_MODELS = \[[^\]\n]*\];$/m.test(configText())).toBe(true);
  });

  test("the calibrated evaluators stay declared on one line for the readers that price them", () => {
    // crates/control-api/tests/cost.rs (every evaluator has a price row) and crates/ham-e2e/tests/live_e2e9_models.rs read the list by this shape.
    expect(RUN_EVALUATOR_MODELS).toEqual(["us.openai.gpt-5.5", "us.anthropic.claude-opus-5-5", "us.xai.grok-4.7"]);
    expect(/^export const RUN_EVALUATOR_MODELS = \[[^\]\n]*\];$/m.test(configText())).toBe(true);
  });

  test("the deployed engine set is Cline and OpenCode, carried by control-api", () => {
    // ENGINE_SET is the engines this release ships, the profiles a start request may name. The run engine is a member, and the Lambda names
    // the set explicitly (the Rust default, [cline], is for local runs and tests).
    expect(RUN_ENGINE_SET).toEqual(["cline", "opencode"]);
    expect(RUN_ENGINE_SET).toContain(RUN_ENGINE);
    const control = envOf("direct", "control", "hamh-direct-control-api");
    expect(control.ENGINE).toBe("cline");
    expect(control.ENGINE_SET).toBe(RUN_ENGINE_SET.join(","));
  });

  test("only the engine set of control-api names OpenCode in a Lambda environment, and config.ts states the run engine on one line (scripts/lib/e2e_live.py reads it)", () => {
    for (const [stack, t] of synth("direct").templates) {
      for (const r of Object.values(resourcesOf(t, "AWS::Lambda::Function"))) {
        const props = r.Properties as { FunctionName?: string; Environment?: { Variables?: Record<string, unknown> } };
        for (const [k, v] of Object.entries(props.Environment?.Variables ?? {})) {
          const names = String(v).split(",").includes("opencode");
          const isTheSet = props.FunctionName === "hamh-direct-control-api" && k === "ENGINE_SET";
          expect(names, `${stack} ${props.FunctionName} ${k}=${String(v)}`).toBe(isTheSet);
        }
      }
    }
    const text = configText();
    expect(/^export const RUN_ENGINE = "cline";$/m.test(text)).toBe(true);
    // scripts/lib/e2e_live.py infra_consts takes the FIRST match of this pattern anywhere in the file, comments included: a
    // comment that quotes the declaration would be read as the value (PRECHECK FAIL RUN_ENGINE='<engine>').
    expect(/export const RUN_ENGINE\s*=\s*("[^"]*"|\[[^\]]*\])/.exec(text)?.[1]).toBe('"cline"');
    expect(/^export const RUN_ENGINE_SET = \["cline", "opencode"\] as const;$/m.test(text)).toBe(true);
  });

  test("control-api takes the Admin role of the selected account as its operator principals", () => {
    const operators = envOf("direct", "control", "hamh-direct-control-api").OPERATOR_PRINCIPALS;
    // The selected accounts.json entry's Admin role: ham-acct-b02 under HAMH_AWS_PROFILE=ham-acct-b02.
    expect(operators).toBe(`arn:aws:sts::${config.ACCOUNT}:assumed-role/Admin/*`);
  });

  test("the stage routes every session to the run engine and records the bootstrap policy version on new runs", () => {
    expect(stageConfig("direct").engineRouting).toBe("run");
    const c = envOf("direct", "control", "hamh-direct-control-api");
    expect(c.ENGINE_ROUTING).toBe("run");
    expect(c.ENGINE_POLICY_VERSION).toBe("0");
    expect(c.ROUTE_LADDER).toBe("off");
  });
});
