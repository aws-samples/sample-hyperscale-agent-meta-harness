import { readFileSync } from "node:fs";
import { describe, expect, test, vi } from "vitest";
import { ACCOUNT, OWNER, REGION, STACK_NAMES, stageConfig, TARGET } from "../lib/config.ts";
import { phase2Images, synth, withCdkJsonContext } from "./helpers.ts";

// Direct run: the nine stacks of stage `direct`.
const STACKS = ["network", "security", "data", "packages", "control", "orchestration", "runtimes", "delivery", "observability"];
const REQUIRED_TAGS = { project: "hyperscale-agent-meta-harness", owner: OWNER, "managed-by": "cdk" };

function tagMap(tags: unknown): Record<string, string> {
  if (Array.isArray(tags)) return Object.fromEntries(tags.map((t: { Key: string; Value: string }) => [t.Key, t.Value]));
  if (tags && typeof tags === "object") return tags as Record<string, string>;
  return {};
}

describe("app", () => {
  test("the stack name set is the nine stacks of stage direct", () => {
    expect([...STACK_NAMES]).toEqual(STACKS);
  });

  // O4 of the live rehearsal of the public setup: every cdk call printed `[WARNING] aws-cdk-lib.CfnResource#addDependency is deprecated.` (three lines, 22 times) on the
  // screen of the person who ran setup. The app uses the API that replaced it, so a synth of either phase prints no deprecation warning.
  test("building the app prints no deprecation warning of aws-cdk-lib", () => {
    // The warning goes through console.error or process.stderr, depending on the jsii runtime; both are listened to, and the lines of every call are kept.
    const written: string[] = [];
    const keep = (...chunk: unknown[]): boolean => {
      written.push(chunk.map(String).join(" "));
      return true;
    };
    const spies = [
      vi.spyOn(process.stderr, "write").mockImplementation(keep as never),
      vi.spyOn(process.stdout, "write").mockImplementation(keep as never),
      vi.spyOn(console, "error").mockImplementation(keep as never),
      vi.spyOn(console, "warn").mockImplementation(keep as never),
      vi.spyOn(console, "log").mockImplementation(keep as never),
    ];
    const before = process.env.JSII_DEPRECATED;
    delete process.env.JSII_DEPRECATED;
    try {
      synth("direct");
      synth("direct", phase2Images());
    } finally {
      for (const spy of spies) spy.mockRestore();
      if (before !== undefined) process.env.JSII_DEPRECATED = before;
    }
    expect(written.filter((line) => /is deprecated/.test(line))).toEqual([]);
  });

  test("cdk.json's default stage is direct, the only stage", () => {
    const context = JSON.parse(readFileSync(new URL("../cdk.json", import.meta.url), "utf8")).context;
    expect(context.stage).toBe("direct");
  });

  test("synthesizes the nine stacks with pinned account and region", () => {
    const { app, templates } = synth("direct");
    expect([...templates.keys()].sort()).toEqual(STACKS.map((s) => `hamh-direct-${s}`).sort());
    for (const stack of app.node.children) {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const s = stack as any;
      if (!s.templateOptions) continue;
      expect(s.account).toBe(ACCOUNT);
      expect(s.region).toBe(REGION);
    }
  });

  test("every tagged resource carries the four project tags", () => {
    const { templates } = synth("direct");
    let tagged = 0;
    for (const [name, t] of templates) {
      for (const [id, r] of Object.entries(t.toJSON().Resources ?? {}) as [string, { Properties?: { Tags?: unknown } }][]) {
        if (!r.Properties || !("Tags" in r.Properties)) continue;
        tagged += 1;
        const tags = tagMap(r.Properties.Tags);
        for (const [k, v] of Object.entries({ ...REQUIRED_TAGS, stage: "direct" })) {
          expect(tags[k], `${name}/${id} tag ${k}`).toBe(v);
        }
      }
    }
    expect(tagged).toBeGreaterThan(10);
  });

  // Verify row R4 and teardown.sh find a deployment through tags. With the feature flag
  // `@aws-cdk/core:explicitStackTags` (cdk.json) `Tags.of(app).add` skips the stack itself, so a stack carried none.
  // The tags
  // of a stack are in the manifest artifact (`tags`), not in its template; `cdk deploy` passes them to CloudFormation.
  test("every stack carries the four project tags itself, as CloudFormation stack tags", () => {
    // Synthesize under cdk.json's context, as `cdk synth` and `cdk deploy` do (the CLI hands it to the app in
    // CDK_CONTEXT_JSON): the flag `explicitStackTags` is part of it, and the default App has it off.
    const { app } = withCdkJsonContext(() => synth("direct"));
    const stacks = app.synth().stacks;
    expect(stacks.map((s) => s.stackName).sort()).toEqual(STACKS.map((s) => `hamh-direct-${s}`).sort());
    for (const s of stacks) {
      const { "auto-delete": _keep, ...fourTags } = s.tags;
      expect(fourTags, `${s.stackName} stack tags`).toEqual({ ...REQUIRED_TAGS, stage: "direct" });
    }
  });

  test("stage config: direct spans 1 AZ, the selected accounts.json entry's", () => {
    expect(stageConfig("direct").azs).toEqual(TARGET.azs.direct);
    expect(stageConfig("direct").azs).toHaveLength(1);
    for (const az of stageConfig("direct").azs) expect(az.startsWith(REGION), az).toBe(true);
    expect(() => stageConfig("prod" as never)).toThrow(/stage/);
    expect(() => stageConfig("live" as never)).toThrow(/expected direct/);
  });
});
