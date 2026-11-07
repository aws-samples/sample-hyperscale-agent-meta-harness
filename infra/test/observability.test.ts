import { describe, expect, test } from "vitest";
import { names, stageConfig } from "../lib/config.ts";
import { resourcesOf, synth, template } from "./helpers.ts";
import { RUN_TOKENS_QUERY } from "../lib/stacks/observability.ts";

// CloudTrail allows five trails per Region and counts the shadow of every multi-Region trail (acct-a01 us-east-1 held five on 2026-10-14), so the stage builds no trail and no dashboard, and this stack no alarm (the only alarms
// of the stage are the four on the Tavily key, in the control stack).
describe("observability stack", () => {
  const t = template("direct", "observability");

  test("the stack holds one resource of its own: the run-tokens saved query", () => {
    const types = Object.values(t.toJSON().Resources ?? {}).map((r: any) => r.Type).filter((x) => x !== "AWS::CDK::Metadata");
    expect(types).toEqual(["AWS::Logs::QueryDefinition"]);
  });

  test("the invocation log group is read by name and never created or configured here", () => {
    // The invocation log group lives in the data stack; this stack reads it through the saved query.
    t.resourceCountIs("AWS::Logs::LogGroup", 0);
    const qd = Object.values(resourcesOf(t, "AWS::Logs::QueryDefinition")).map((r) => r.Properties as any);
    expect(qd).toHaveLength(1);
    expect(qd[0].Name).toBe("hamh-direct/run-tokens");
    expect(qd[0].LogGroupNames).toEqual(["/hamh/direct/bedrock-invocations"]);
    expect(qd[0].QueryString).toBe(RUN_TOKENS_QUERY);
    for (const needle of ["identity.arn", "inputTokenCount", "cacheReadInputTokenCount", "cacheWriteInputTokenCount", "outputTokenCount"]) {
      expect(qd[0].QueryString, needle).toContain(needle);
    }
    // No CDK path writes invocation logging or Bedrock retention settings, in any stack.
    for (const [name, tpl] of synth("direct").templates) {
      const text = JSON.stringify(tpl.toJSON());
      expect(text, name).not.toMatch(/PutModelInvocationLoggingConfiguration|DeleteModelInvocationLoggingConfiguration|put-model-invocation/i);
      expect(text, name).not.toMatch(/bedrock:Put|PutUseCaseForModelAccess|DataRetention/);
    }
  });

  test("no trail, trail bucket, trail role, composite alarm or dashboard in the stage, and no alarm but the four on the Tavily key in the control stack", () => {
    // A stage that uses Tavily has four CloudWatch alarms on the stored key (infra/test/research-alarms.test.ts holds them); the control stack
    // holds them and every other stack, the observability stack included, holds none.
    const tavilyAlarms = (["key-age", "key-refused", "rate-limited", "plan-limit"] as const).map((w) => names.researchAlarm(stageConfig("direct"), w)).sort();
    for (const [name, tpl] of synth("direct").templates) {
      for (const type of ["AWS::CloudTrail::Trail", "AWS::CloudWatch::Alarm", "AWS::CloudWatch::CompositeAlarm", "AWS::CloudWatch::Dashboard"]) {
        const found = Object.values(resourcesOf(tpl, type)).map((r) => (r.Properties as { AlarmName?: string }).AlarmName).filter((n): n is string => n !== undefined);
        if (type === "AWS::CloudWatch::Alarm" && name === "hamh-direct-control") {
          expect(found.sort(), `${name} ${type}`).toEqual(tavilyAlarms);
        } else {
          expect(Object.keys(resourcesOf(tpl, type)), `${name} ${type}`).toEqual([]);
        }
      }
    }
    t.resourceCountIs("AWS::S3::Bucket", 0);
    t.resourceCountIs("AWS::IAM::Role", 0);
  });

  test("the stack deploys after the data stack that holds the invocation log group", () => {
    const asm = synth("direct").app.synth();
    expect(asm.getStackByName("hamh-direct-observability").dependencies.map((d) => d.id)).toContain("hamh-direct-data");
  });
});
