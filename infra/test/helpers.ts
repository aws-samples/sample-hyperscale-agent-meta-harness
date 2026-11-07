import { readFileSync } from "node:fs";
import { App } from "aws-cdk-lib";
import { Template } from "aws-cdk-lib/assertions";
import { buildApp, type BuildOptions } from "../lib/app.ts";
import { ACCOUNT, REGION } from "../lib/config.ts";

export type Stage = "direct";

/** Representative digest-pinned images: with them every phase-2 resource synthesizes (the worker runtime, the delivery functions). */
export function phase2Images(stage: Stage = "direct"): Omit<BuildOptions, "stage"> {
  const image = (repo: string) => `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-${stage}-${repo}@sha256:${"a".repeat(64)}`;
  return { workerImage: image("worker-python"), deliveryImage: image("delivery") };
}

export function synth(stage: Stage, opts: Omit<BuildOptions, "stage"> = {}): { app: App; templates: Map<string, Template> } {
  const app = buildApp({ stage, ...opts });
  const templates = new Map<string, Template>();
  for (const stack of app.node.children) {
    if ("templateOptions" in stack) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      templates.set((stack as any).stackName, Template.fromStack(stack as any));
    }
  }
  return { app, templates };
}

export function template(stage: Stage, name: string, opts: Omit<BuildOptions, "stage"> = {}): Template {
  const t = synth(stage, opts).templates.get(`hamh-${stage}-${name}`);
  if (!t) throw new Error(`stack hamh-${stage}-${name} not synthesized`);
  return t;
}

export function resourcesOf(t: Template, type: string): Record<string, { Properties?: Record<string, unknown> }> {
  return t.findResources(type) as Record<string, { Properties?: Record<string, unknown> }>;
}

/** Runs `fn` with cdk.json's `context` in CDK_CONTEXT_JSON, the way the CDK CLI runs the app (the variable is restored after). */
export function withCdkJsonContext<T>(fn: () => T): T {
  const context = JSON.parse(readFileSync(new URL("../cdk.json", import.meta.url), "utf8")).context;
  const before = process.env.CDK_CONTEXT_JSON;
  process.env.CDK_CONTEXT_JSON = JSON.stringify(context);
  try {
    return fn();
  } finally {
    if (before === undefined) delete process.env.CDK_CONTEXT_JSON;
    else process.env.CDK_CONTEXT_JSON = before;
  }
}
