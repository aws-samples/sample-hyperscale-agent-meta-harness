import { describe, expect, test } from "vitest";
import { buildApp } from "../lib/app.ts";
import { entryOptions } from "../lib/entry.ts";
import { template } from "./helpers.ts";
import { ACCOUNT, REGION } from "../lib/config.ts";

// The CLI entry is the only place the app reads the environment. A deploy without the real
// cargo-lambda output once shipped the 191-byte fixture as a worker-api; it must fail instead.
const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;
const IMAGE = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-worker-python@sha256:` + "c".repeat(64);
const DELIVERY = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-delivery@sha256:` + "d".repeat(64);

describe("CLI entry options", () => {
  test("HAMH_LAMBDA_DIR is required", () => {
    expect(() => entryOptions({})).toThrow(/HAMH_LAMBDA_DIR/);
  });

  test("the directory must hold every Lambda bootstrap", () => {
    const noBootstrap = new URL("./fixtures", import.meta.url).pathname;
    expect(() => entryOptions({ HAMH_LAMBDA_DIR: noBootstrap })).toThrow(/worker-api\/bootstrap/);
    const workerOnly = new URL("./fixtures/lambda-worker-only", import.meta.url).pathname;
    expect(() => entryOptions({ HAMH_LAMBDA_DIR: workerOnly })).toThrow(/control-api\/bootstrap/);
  });

  test("stage from the CDK context, the lambda directory and the worker image from the environment", () => {
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_IMAGE: IMAGE, CDK_CONTEXT_JSON: JSON.stringify({ stage: "direct" }) }))
      .toEqual({ stage: "direct", lambdaDir: FIXTURE, workerImage: IMAGE, workerRevision: 0 });
  });

  test("stage defaults to direct and an empty worker image means phase 1", () => {
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_IMAGE: "" }))
      .toEqual({ stage: "direct", lambdaDir: FIXTURE, workerImage: undefined, workerRevision: 0 });
  });

  test("the delivery image comes from HAMH_DELIVERY_IMAGE; unset or empty means its phase 1", () => {
    const opts = entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_IMAGE: IMAGE, HAMH_DELIVERY_IMAGE: DELIVERY });
    expect(opts.deliveryImage).toBe(DELIVERY);
    expect(opts.workerImage).toBe(IMAGE);
    for (const env of [{}, { HAMH_DELIVERY_IMAGE: "" }]) {
      const none = entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, ...env });
      expect("deliveryImage" in none && none.deliveryImage !== undefined, JSON.stringify(env)).toBe(false);
    }
  });

  test("the retired runtime image variables are not read: only the worker and delivery images reach the app", () => {
    const old = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-x@sha256:` + "e".repeat(64);
    const opts = entryOptions({
      HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_IMAGE: IMAGE, HAMH_DELIVERY_IMAGE: DELIVERY, HAMH_EVALUATOR_IMAGE: old, HAMH_VERIFIER_IMAGE: old,
      HAMH_WORKER_GO_IMAGE: old, HAMH_VERIFIER_GO_IMAGE: old, HAMH_WORKER_JVM_IMAGE: old, HAMH_VERIFIER_JVM_IMAGE: old,
      HAMH_EVALUATOR_MODEL_SETTINGS: "{}",
    });
    expect(Object.keys(opts).sort()).toEqual(["deliveryImage", "lambdaDir", "stage", "workerImage", "workerRevision"]);
  });

  test("HAMH_WORKER_REVISION is a non-negative integer", () => {
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_REVISION: "4" }).workerRevision).toBe(4);
    for (const bad of ["-1", "x", "1.5", " 2", ""]) {
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_REVISION: bad }), bad).toThrow(/HAMH_WORKER_REVISION/);
    }
  });

  // The utility model of the run names. Unset or blank keeps the catalog default and adds no option; off and the name of a row pass through
  // (the app resolves them again); anything else stops the deploy here, before a stack is built.
  test("HAMH_UTILITY_MODEL: unset or blank adds no option, off and a row's name pass through, a bad word throws", () => {
    for (const env of [{}, { HAMH_UTILITY_MODEL: "" }, { HAMH_UTILITY_MODEL: "   " }]) {
      expect("utilityModel" in entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, ...env }), JSON.stringify(env)).toBe(false);
    }
    for (const setting of ["off", "OFF", "haiku", "HAIKU", "us.anthropic.claude-haiku-5-5"]) {
      expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_UTILITY_MODEL: setting }).utilityModel, setting).toBe(setting);
    }
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_UTILITY_MODEL: "  off " }).utilityModel).toBe("off");
    for (const bad of ["sonnet", "nope", "global.anthropic.claude-haiku-5-5"]) {
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_UTILITY_MODEL: bad }), bad).toThrow(/HAMH_UTILITY_MODEL/);
    }
  });

  test("buildApp ignores the environment: only explicit options change the app", () => {
    const names = ["HAMH_WORKER_IMAGE", "HAMH_DELIVERY_IMAGE"] as const;
    const saved = names.map((n) => process.env[n]);
    process.env.HAMH_WORKER_IMAGE = IMAGE;
    process.env.HAMH_DELIVERY_IMAGE = DELIVERY;
    try {
      template("direct", "runtimes").resourceCountIs("AWS::BedrockAgentCore::Runtime", 0);
      template("direct", "delivery").resourceCountIs("AWS::Lambda::Function", 0);
      expect(() => buildApp({ stage: "direct" })).not.toThrow();
    } finally {
      names.forEach((n, i) => {
        if (saved[i] === undefined) delete process.env[n];
        else process.env[n] = saved[i];
      });
    }
  });
});
