// The alarms on the stored Tavily key (CloudWatch alarms, L1 only) and the settings that switch them. Four alarms exist only on a stage whose
// research provider is `tavily` or `auto`: the key's age, a refused key, a limited rate and a plan limit. They read the metrics research-api writes as
// Embedded Metric Format lines (crates/research-api/src/metrics.rs); the namespace, the dimension and the metric names are pinned here against that file.
import { readFileSync } from "node:fs";
import { App } from "aws-cdk-lib";
import { Template } from "aws-cdk-lib/assertions";
import { describe, expect, test, vi } from "vitest";
import {
  ACCOUNT, carriesTavily, DEFAULT_TAVILY_KEY_MAX_AGE_DAYS, names, REGION, RESEARCH_PROVIDERS, type ResearchProvider, resolveResearchAlarms, stageConfig,
  TAVILY_ALARM_WORDS, TAVILY_CAPABLE_PROVIDERS,
} from "../lib/config.ts";
import type { BuildOptions } from "../lib/app.ts";
import { entryOptions } from "../lib/entry.ts";
import { TEST_ENTRY, underEntry } from "./accounts-three.ts";
import { ControlStack } from "../lib/stacks/control.ts";
import { DataStack } from "../lib/stacks/data.ts";
import { NetworkStack } from "../lib/stacks/network.ts";
import {
  RESEARCH_METRICS_DIMENSION, RESEARCH_METRICS_NAMESPACE, TAVILY_METRIC,
} from "../lib/stacks/research-alarms.ts";
import { SecurityStack } from "../lib/stacks/security.ts";
import { resourcesOf, synth, template } from "./helpers.ts";

type Props = Record<string, any>;

const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;
const TOPIC = `arn:aws:sns:${REGION}:123456789012:ham-research-alarms`;
const cfg = stageConfig("direct");

const alarmsOf = (opts: Partial<BuildOptions> = {}): Props[] =>
  Object.values(resourcesOf(template("direct", "control", opts as Omit<BuildOptions, "stage">), "AWS::CloudWatch::Alarm")).map((r) => r.Properties as Props);
const alarm = (opts: Partial<BuildOptions>, name: string): Props => {
  const hits = alarmsOf(opts).filter((a) => a.AlarmName === name);
  expect(hits, name).toHaveLength(1);
  return hits[0];
};
const NAMES = {
  age: "hamh-direct-research-tavily-key-age",
  refused: "hamh-direct-research-tavily-key-refused",
  rate: "hamh-direct-research-tavily-rate-limited",
  plan: "hamh-direct-research-tavily-plan-limit",
};

describe("which stages get the four alarms", () => {
  test("the default provider, auto, has exactly the four alarms, named after the stage", () => {
    expect(alarmsOf().map((a) => a.AlarmName).sort()).toEqual(Object.values(NAMES).sort());
    expect(names.researchAlarm(cfg, "key-age")).toBe(NAMES.age);
    expect(names.researchAlarm(cfg, "key-refused")).toBe(NAMES.refused);
    expect(names.researchAlarm(cfg, "rate-limited")).toBe(NAMES.rate);
    expect(names.researchAlarm(cfg, "plan-limit")).toBe(NAMES.plan);
  });

  test("the agentcore and tavily-keyless providers have none, and nothing else of the control stack moves", () => {
    for (const researchProvider of ["agentcore", "tavily-keyless"] as const) {
      expect(alarmsOf({ researchProvider }), researchProvider).toEqual([]);
    }
  });

  test("the fixture provider, which a test stage may use, has none", async () => {
    // The test entry of the three-entry fixture, found by its role (a test stage may use the fixture provider; the active entry may not).
    await underEntry(TEST_ENTRY.profile, async () => {
      vi.resetModules();
      const { buildApp: build } = await import("../lib/app.ts");
      const { Template } = await import("aws-cdk-lib/assertions");
      const app = build({ stage: "direct", researchProvider: "fixture" });
      const stack = app.node.children.find((c) => (c as any).stackName === "hamh-direct-control");
      expect(Template.fromStack(stack as any).findResources("AWS::CloudWatch::Alarm")).toEqual({});
    });
  });

  test("the switch off builds none, and the other eight templates are the same with the alarms on or off", () => {
    expect(alarmsOf({ tavilyAlarms: "off" })).toEqual([]);
    const on = synth("direct").templates;
    const off = synth("direct", { tavilyAlarms: "off" }).templates;
    for (const [name, t] of on) {
      if (name === "hamh-direct-control") continue;
      expect(t.toJSON(), name).toEqual(off.get(name)!.toJSON());
    }
    // The control stack differs by the four alarms and by nothing else.
    const strip = (j: Props): Props => {
      const copy = JSON.parse(JSON.stringify(j));
      for (const [id, r] of Object.entries<Props>(copy.Resources)) if (r.Type === "AWS::CloudWatch::Alarm") delete copy.Resources[id];
      return copy;
    };
    expect(strip(on.get("hamh-direct-control")!.toJSON())).toEqual(off.get("hamh-direct-control")!.toJSON());
  });

  test("the switch accepts on and off in any letter case and spacing, and refuses any other word by name", () => {
    expect([...TAVILY_ALARM_WORDS]).toEqual(["on", "off"]);
    for (const word of ["off", "OFF", " Off\t", "\noff"]) expect(resolveResearchAlarms({ alarms: word }).enabled, word).toBe(false);
    for (const word of [undefined, "", "  ", "on", "ON", " On "]) expect(resolveResearchAlarms({ alarms: word }).enabled, String(word)).toBe(true);
    for (const word of ["no", "false", "0", "yes", "offf"]) {
      expect(() => resolveResearchAlarms({ alarms: word }), word).toThrow(/HAMH_TAVILY_ALARMS/);
      expect(() => resolveResearchAlarms({ alarms: word }), word).toThrow(/on, off/);
    }
  });
});

// The providers that can call Tavily with the stored key are one constant. `auto` is the provider that searches with AgentCore
// Web Search until a Tavily key is stored and with Tavily first once one is, so it carries the alarms too. The control stack is built here directly, without the
// rest of the app.
describe("the providers that carry Tavily", () => {
  const controlFor = (provider: ResearchProvider): Template => {
    const app = new App();
    const env = { account: ACCOUNT, region: REGION };
    const stack = (name: string) => ({ env, stackName: `${cfg.prefix}-${name}`, cfg });
    const network = new NetworkStack(app, `${cfg.prefix}-network`, stack("network"));
    const security = new SecurityStack(app, `${cfg.prefix}-security`, { ...stack("security"), network });
    const data = new DataStack(app, `${cfg.prefix}-data`, { ...stack("data"), security });
    const control = new ControlStack(app, `${cfg.prefix}-control`, {
      ...stack("control"), network, security, data, lambdaDir: FIXTURE, researchProvider: provider, researchAlarms: resolveResearchAlarms({}),
    });
    return Template.fromStack(control);
  };

  test("the constant names tavily and auto, and no other provider", () => {
    expect([...TAVILY_CAPABLE_PROVIDERS]).toEqual(["tavily", "auto"]);
  });

  test("carriesTavily follows the constant for every provider the stage accepts", () => {
    for (const p of [...RESEARCH_PROVIDERS] as ResearchProvider[]) {
      expect(carriesTavily(p), p).toBe((TAVILY_CAPABLE_PROVIDERS as readonly string[]).includes(p));
    }
    expect(["tavily", "auto"].map((p) => carriesTavily(p as ResearchProvider))).toEqual([true, true]);
    expect(["tavily-keyless", "agentcore", "fixture"].map((p) => carriesTavily(p as ResearchProvider))).toEqual([false, false, false]);
  });

  test("a control stack for tavily and for auto has the four alarms; one for any other provider has none", () => {
    for (const p of [...RESEARCH_PROVIDERS] as ResearchProvider[]) {
      const found = Object.values(controlFor(p).findResources("AWS::CloudWatch::Alarm")).map((r) => (r.Properties as Props).AlarmName).sort();
      expect(found, p).toEqual(carriesTavily(p) ? Object.values(NAMES).sort() : []);
    }
  });
});

describe("the four alarms", () => {
  const dim = [{ Name: "Stage", Value: "direct" }];
  const common = (a: Props) => {
    expect(a.Namespace).toBe("Hamh/Research");
    expect(a.Dimensions).toEqual(dim);
    expect(a.EvaluationPeriods).toBe(1);
    expect(a.DatapointsToAlarm).toBe(1);
    expect(a.ComparisonOperator).toBe("GreaterThanOrEqualToThreshold");
  };

  test("key age: the maximum age over an hour reaches 30 days; a silent hour keeps the last state", () => {
    const a = alarm({}, NAMES.age);
    common(a);
    expect(a.MetricName).toBe("TavilyKeyAgeDays");
    expect(a.Statistic).toBe("Maximum");
    expect(a.Period).toBe(3600);
    expect(a.Threshold).toBe(30);
    expect(a.TreatMissingData).toBe("ignore");
  });

  test.each([
    ["refused", "TavilyKeyRefused", 300],
    ["rate", "TavilyRateLimited", 900],
    ["plan", "TavilyPlanLimit", 3600],
  ] as const)("%s: a sum of 1 or more over its period, and a silent period is fine", (which, metric, period) => {
    const a = alarm({}, NAMES[which]);
    common(a);
    expect(a.MetricName).toBe(metric);
    expect(a.Statistic).toBe("Sum");
    expect(a.Period).toBe(period);
    expect(a.Threshold).toBe(1);
    expect(a.TreatMissingData).toBe("notBreaching");
  });

  test("each description says what to do, in words, and holds no secret or name of the team", () => {
    for (const a of alarmsOf()) {
      expect(typeof a.AlarmDescription, a.AlarmName).toBe("string");
      expect(a.AlarmDescription.length, a.AlarmName).toBeGreaterThan(80);
      expect(a.AlarmDescription.length, a.AlarmName).toBeLessThanOrEqual(1024);
      expect(a.AlarmDescription, a.AlarmName).toMatch(/Tavily/);
    }
    expect(alarm({}, NAMES.age).AlarmDescription).toMatch(/rotate/i);
    expect(alarm({}, NAMES.age).AlarmDescription).toMatch(/30 days/);
    expect(alarm({}, NAMES.refused).AlarmDescription).toMatch(/research-key-put\.py/);
    expect(alarm({}, NAMES.rate).AlarmDescription).toMatch(/rate/i);
    expect(alarm({}, NAMES.plan).AlarmDescription).toMatch(/plan|limit/i);
  });

  test("the alarms carry the project tags like every other resource of the stack", () => {
    for (const a of alarmsOf()) {
      const tags = Object.fromEntries((a.Tags as { Key: string; Value: string }[]).map((t) => [t.Key, t.Value]));
      expect(tags.project, a.AlarmName).toBe("hyperscale-agent-meta-harness");
      expect(tags.stage, a.AlarmName).toBe("direct");
    }
  });
});

describe("the age threshold and the topic", () => {
  test("the age threshold is HAMH_TAVILY_KEY_MAX_AGE_DAYS, 30 by default, and the description follows", () => {
    expect(DEFAULT_TAVILY_KEY_MAX_AGE_DAYS).toBe(30);
    const a = alarm({ tavilyKeyMaxAgeDays: "45" }, NAMES.age);
    expect(a.Threshold).toBe(45);
    expect(a.AlarmDescription).toMatch(/45 days/);
    expect(a.AlarmDescription).not.toMatch(/30 days/);
    for (const days of ["1", "365", " 90 "]) {
      expect(resolveResearchAlarms({ maxKeyAgeDays: days }).maxKeyAgeDays, days).toBe(Number(days.trim()));
    }
  });

  test.each(["0", "366", "-1", "1.5", "abc", "1e2", "0x10", "30d", "٣٠"])("HAMH_TAVILY_KEY_MAX_AGE_DAYS=%s is refused by name", (bad) => {
    expect(() => resolveResearchAlarms({ maxKeyAgeDays: bad })).toThrow(/HAMH_TAVILY_KEY_MAX_AGE_DAYS/);
    expect(() => resolveResearchAlarms({ maxKeyAgeDays: bad })).toThrow(/1 to 365/);
  });

  test("without a topic the alarms have no actions; with one, the alarm and the return to OK both go to it", () => {
    for (const a of alarmsOf()) {
      expect(a.AlarmActions, a.AlarmName).toBeUndefined();
      expect(a.OKActions, a.AlarmName).toBeUndefined();
    }
    const withTopic = alarmsOf({ researchAlarmTopicArn: TOPIC });
    expect(withTopic).toHaveLength(4);
    for (const a of withTopic) {
      expect(a.AlarmActions, a.AlarmName).toEqual([TOPIC]);
      expect(a.OKActions, a.AlarmName).toEqual([TOPIC]);
    }
  });

  test("the topic must be an SNS topic ARN of any Region, partition or kind, and the error does not repeat it", () => {
    // A topic in another Region, a FIFO topic and a topic of another partition are SNS topic ARNs and are accepted: CloudWatch publishes to standard and FIFO topics.
    const otherRegion = `arn:aws:sns:${REGION === "eu-west-1" ? "us-west-2" : "eu-west-1"}:123456789012:t`;
    for (const arn of [TOPIC, otherRegion, `${TOPIC}.fifo`, "arn:aws-cn:sns:cn-north-1:123456789012:t_1-2"]) {
      expect(resolveResearchAlarms({ topicArn: arn }).topicArn, arn).toBe(arn);
    }
    expect(resolveResearchAlarms({ topicArn: "" }).topicArn).toBeUndefined();
    expect(resolveResearchAlarms({ topicArn: `  ${TOPIC}\n` }).topicArn).toBe(TOPIC);
    const bad = [
      "not-an-arn", `arn:aws:s3:::${REGION}`, `arn:aws:sns:${REGION}:12345:t`, `arn:aws:sns:${REGION}:123456789012:`,
      `arn:aws:sns:${REGION}:123456789012:a b`, `${TOPIC}:extra`, `arn:aws:sns::123456789012:t`, `arn:aws:sqs:${REGION}:123456789012:t`,
    ];
    for (const arn of bad) {
      expect(() => resolveResearchAlarms({ topicArn: arn }), arn).toThrow(/HAMH_RESEARCH_ALARM_TOPIC_ARN/);
      let message = "";
      try {
        resolveResearchAlarms({ topicArn: arn });
      } catch (e) {
        message = String((e as Error).message);
      }
      expect(message, arn).not.toContain(arn);
    }
  });

  test("a topic with the alarms switched off, or on a stage without the alarms, is accepted and unused", () => {
    expect(alarmsOf({ tavilyAlarms: "off", researchAlarmTopicArn: TOPIC })).toEqual([]);
    expect(alarmsOf({ researchProvider: "agentcore", researchAlarmTopicArn: TOPIC })).toEqual([]);
  });
});

describe("the entry reads the three settings", () => {
  const base = { HAMH_LAMBDA_DIR: FIXTURE };

  test("nothing is added when they are unset or empty", () => {
    for (const env of [{}, { HAMH_TAVILY_ALARMS: "", HAMH_TAVILY_KEY_MAX_AGE_DAYS: "", HAMH_RESEARCH_ALARM_TOPIC_ARN: "" }]) {
      const o = entryOptions({ ...base, ...env });
      for (const key of ["tavilyAlarms", "tavilyKeyMaxAgeDays", "researchAlarmTopicArn"]) expect(key in o, key).toBe(false);
    }
  });

  test("the values reach the app options and the app builds the alarms from them", () => {
    const o = entryOptions({
      ...base, HAMH_TAVILY_ALARMS: "on", HAMH_TAVILY_KEY_MAX_AGE_DAYS: "60", HAMH_RESEARCH_ALARM_TOPIC_ARN: TOPIC,
    });
    expect([o.tavilyAlarms, o.tavilyKeyMaxAgeDays, o.researchAlarmTopicArn]).toEqual(["on", "60", TOPIC]);
    expect(alarmsOf(o).find((a) => a.AlarmName === NAMES.age)!.Threshold).toBe(60);
    expect(entryOptions({ ...base, HAMH_TAVILY_ALARMS: "off" }).tavilyAlarms).toBe("off");
  });

  test("a bad value stops the entry before any stack is built, and the message names the variable", () => {
    expect(() => entryOptions({ ...base, HAMH_TAVILY_ALARMS: "maybe" })).toThrow(/HAMH_TAVILY_ALARMS/);
    expect(() => entryOptions({ ...base, HAMH_TAVILY_KEY_MAX_AGE_DAYS: "0" })).toThrow(/HAMH_TAVILY_KEY_MAX_AGE_DAYS/);
    expect(() => entryOptions({ ...base, HAMH_TAVILY_KEY_MAX_AGE_DAYS: "400" })).toThrow(/1 to 365/);
    expect(() => entryOptions({ ...base, HAMH_RESEARCH_ALARM_TOPIC_ARN: "topic" })).toThrow(/HAMH_RESEARCH_ALARM_TOPIC_ARN/);
  });
});

describe("the strings the alarms and research-api share", () => {
  const rust = readFileSync(new URL("../../crates/research-api/src/metrics.rs", import.meta.url), "utf8");
  const constant = (name: string): string => {
    const m = new RegExp(`pub const ${name}: &str = "([^"]+)";`).exec(rust);
    expect(m, name).not.toBeNull();
    return m![1];
  };

  test("the namespace, the dimension and the four metric names equal the Rust constants", () => {
    expect(RESEARCH_METRICS_NAMESPACE).toBe(constant("NAMESPACE"));
    expect(RESEARCH_METRICS_DIMENSION).toBe(constant("DIMENSION"));
    expect(TAVILY_METRIC.keyRefused).toBe(constant("KEY_REFUSED"));
    expect(TAVILY_METRIC.rateLimited).toBe(constant("RATE_LIMITED"));
    expect(TAVILY_METRIC.planLimit).toBe(constant("PLAN_LIMIT"));
    expect(TAVILY_METRIC.keyAgeDays).toBe(constant("KEY_AGE_DAYS"));
  });

  test("the strings are written once in the app, in research-alarms.ts", () => {
    const here = readFileSync(new URL("../lib/stacks/research-alarms.ts", import.meta.url), "utf8");
    expect(here.split('"Hamh/Research"').length - 1).toBe(1);
    for (const lib of ["research.ts", "control.ts"]) {
      const other = readFileSync(new URL(`../lib/stacks/${lib}`, import.meta.url), "utf8");
      expect(other, lib).not.toContain("Hamh/Research");
    }
  });
});

describe("research-api knows the stage name for the Stage dimension", () => {
  test("its environment carries HAMH_STAGE, the stage, and research-job's does not", () => {
    const fns = Object.values(resourcesOf(template("direct", "control"), "AWS::Lambda::Function")).map((r) => r.Properties as Props);
    const api = fns.find((f) => f.FunctionName === "hamh-direct-research-api")!;
    const job = fns.find((f) => f.FunctionName === "hamh-direct-research-job")!;
    expect(api.Environment.Variables.HAMH_STAGE).toBe("direct");
    expect("HAMH_STAGE" in job.Environment.Variables).toBe(false);
  });
});
