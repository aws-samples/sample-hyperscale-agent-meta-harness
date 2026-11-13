import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { Template } from "aws-cdk-lib/assertions";
import {
  DEFAULT_LOG_RETENTION_DAYS, LOG_RETENTION_DAYS_ALLOWED, names, PR_CHECK_COMPUTE_WORDS, resolveStageSettings, stageConfig,
} from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { RUN_TIMEOUT_S } from "../lib/stacks/orchestration-direct.ts";
import { phase2Images, resourcesOf, synth, template } from "./helpers.ts";

// The second wave, the infra lane. Each limit below was a constant of the CDK app; each is now either raised for a reason or a stage setting
// read from an environment variable of the deploy (the same way HAMH_COMPUTE_INSTANCES is), with the old value as the default.
const FIXTURE_LAMBDA_DIR = new URL("./fixtures/lambda", import.meta.url).pathname;
const ENV = { HAMH_LAMBDA_DIR: FIXTURE_LAMBDA_DIR };
const DAY = 24 * 3600;

const lambdas = (t: Template) => Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as any);
const logGroups = (templates: Map<string, Template>) =>
  [...templates.values()].flatMap((t) => Object.values(resourcesOf(t, "AWS::Logs::LogGroup")).map((r) => r.Properties as any));

describe("D39: the two API Lambdas run until the gateway gives up", () => {
  const control = template("direct", "control", phase2Images("direct"));
  test("control-api and worker-api have a 28 s timeout, one second under the 29 s of the API Gateway integration", () => {
    const api = lambdas(control).filter((p) => /control-api|worker-api/.test(String(JSON.stringify(p.FunctionName))));
    expect(api.map((p) => p.Timeout)).toEqual([28, 28]);
  });
});

describe("D13, D14, E9: a run may take thirty days", () => {
  test("the machine timeout of the CDK app is thirty days plus the ten hours for staging and delivery, and the machine file says the same", () => {
    expect(RUN_TIMEOUT_S).toBe(30 * DAY + 36_000);
    const asl = JSON.parse(readFileSync(new URL("../../statemachines/direct.asl.json", import.meta.url), "utf8"));
    expect(asl.TimeoutSeconds).toBe(RUN_TIMEOUT_S);
    expect(RUN_TIMEOUT_S).toBeLessThan(365 * DAY);
  });

  test("the snapshot download grant names the turn directories of one, two and three digits (256 sessions)", () => {
    const control = template("direct", "control", phase2Images("direct"));
    const text = JSON.stringify(control.toJSON());
    for (const marks of ["?", "??", "???"]) {
      expect(text, marks).toContain(`/runs/*/agents/direct/turns/${marks}/snapshot.bundle`);
      expect(text, marks).toContain(`/runs/*/agents/direct/turns/${marks}/snapshot.json`);
    }
    expect(text).not.toContain("turns/????/");
  });
});

describe("D49: HAMH_LOG_RETENTION_DAYS", () => {
  test("30 days when unset, empty or blank", () => {
    expect(DEFAULT_LOG_RETENTION_DAYS).toBe(30);
    for (const s of [undefined, "", "  "]) expect(resolveStageSettings({ logRetentionDays: s }).logRetentionDays, JSON.stringify(s)).toBe(30);
  });

  test("a period CloudWatch Logs takes is accepted, with surrounding blanks ignored", () => {
    for (const days of LOG_RETENTION_DAYS_ALLOWED) expect(resolveStageSettings({ logRetentionDays: ` ${days} ` }).logRetentionDays).toBe(days);
  });

  test("any other word throws and names the setting and the periods", () => {
    for (const bad of ["45", "0", "-30", "30d", "1.5", "thirty", "1e2", "030"]) {
      expect(() => resolveStageSettings({ logRetentionDays: bad }), bad).toThrow(/HAMH_LOG_RETENTION_DAYS=".*" is not a number of days CloudWatch Logs keeps: 1, 3, 5, 7, 14, 30/);
    }
  });

  test("every log group of the stage keeps the chosen period, and none keeps 30 when 90 is chosen", () => {
    const base = synth("direct", phase2Images("direct")).templates;
    const ninety = synth("direct", { ...phase2Images("direct"), settings: { logRetentionDays: "90" } }).templates;
    const before = logGroups(base).map((g) => g.RetentionInDays);
    const after = logGroups(ninety).map((g) => g.RetentionInDays);
    expect(before.length).toBeGreaterThan(5);
    expect(after.length).toBe(before.length);
    expect(before.filter((d) => d === 30).length).toBeGreaterThan(5);
    expect(after.filter((d) => d === 30)).toEqual([]);
    expect(after.filter((d) => d === 90).length).toBe(before.filter((d) => d === 30).length);
  });
});

describe("D46, D47: the machine of the Instances part is a stage setting", () => {
  const set = { instancesTypes: "m7g.8xlarge", instancesRootFreeGiB: "200", instancesWorkspaceGiB: "400", instancesLifetimeS: "172800", instancesIdleS: "3600" };

  test("the defaults are the machine the spike proved", () => {
    const i = stageConfig("direct").instances;
    expect(i).toMatchObject({ instanceTypes: ["m7g.4xlarge", "m6g.4xlarge"], vcpus: 16, rootFreeGiB: 100, workspaceGiB: 150, maxLifetimeS: 28_800, idleInstanceTimeoutS: 1_800 });
  });

  test("a set value replaces its default and the vCPU count follows the smallest type", () => {
    const i = stageConfig("direct", resolveStageSettings(set)).instances;
    expect(i).toMatchObject({ instanceTypes: ["m7g.8xlarge"], vcpus: 32, rootFreeGiB: 200, workspaceGiB: 400, maxLifetimeS: 172_800, idleInstanceTimeoutS: 3_600 });
    const many = stageConfig("direct", resolveStageSettings({ instancesTypes: "m7g.8xlarge, c7g.4xlarge" })).instances;
    expect(many.instanceTypes).toEqual(["m7g.8xlarge", "c7g.4xlarge"]);
    expect(many.vcpus).toBe(16);
  });

  test("a type that is not Graviton, a size that does not exist or an empty element throws and names the setting", () => {
    for (const bad of ["m7i.4xlarge", "c5.xlarge", "m7g.huge", "m7g", "m7g.4xlarge,,c7g.large", "m7g.4xlarge,m7g.4xlarge"]) {
      expect(() => resolveStageSettings({ instancesTypes: bad }), bad).toThrow(/HAMH_INSTANCES_TYPES=/);
    }
  });

  test("the sizes of the disks are whole GiB from 1 to 16384 and the lifetimes stay in the range the service takes", () => {
    for (const bad of ["0", "16385", "1.5", "big", "-1"]) {
      expect(() => resolveStageSettings({ instancesRootFreeGiB: bad }), bad).toThrow(/HAMH_INSTANCES_ROOT_FREE_GIB=/);
      expect(() => resolveStageSettings({ instancesWorkspaceGiB: bad }), bad).toThrow(/HAMH_INSTANCES_WORKSPACE_GIB=/);
    }
    // The lifetime may not drop under the 28,800 s a session needs (27,900 s of prompt and a 900 s reserve) or pass the 14 days AgentCore takes.
    for (const bad of ["28799", "1209601", "8h", "0"]) expect(() => resolveStageSettings({ instancesLifetimeS: bad }), bad).toThrow(/HAMH_INSTANCES_LIFETIME_S=/);
    for (const bad of ["59", "1209601", "soon"]) expect(() => resolveStageSettings({ instancesIdleS: bad }), bad).toThrow(/HAMH_INSTANCES_IDLE_S=/);
    expect(resolveStageSettings({ instancesLifetimeS: "28800", instancesIdleS: "60" }).instances).toMatchObject({ maxLifetimeS: 28_800, idleInstanceTimeoutS: 60 });
    expect(resolveStageSettings({ instancesLifetimeS: "1209600", instancesIdleS: "1209600" }).instances).toMatchObject({ maxLifetimeS: 1_209_600, idleInstanceTimeoutS: 1_209_600 });
  });

  test("the defaults written out as settings are the defaults: the provider keeps its name, so an update with no setting replaces nothing", () => {
    const written = resolveStageSettings({
      instancesTypes: "m7g.4xlarge,m6g.4xlarge", instancesRootFreeGiB: "100", instancesWorkspaceGiB: "150", instancesLifetimeS: "28800", instancesIdleS: "1800", logRetentionDays: "30",
      prCheckMinutes: "60", prCheckCompute: "small",
    });
    expect(stageConfig("direct", written)).toEqual(stageConfig("direct"));
    expect(names.capacityProvider(stageConfig("direct", written))).toBe(names.capacityProvider(stageConfig("direct")));
  });

  test("an idle time longer than the machine lifetime throws and names both settings, whichever of the two is the default", () => {
    expect(() => resolveStageSettings({ instancesIdleS: "40000" })).toThrow(/HAMH_INSTANCES_IDLE_S=40000 is longer than the machine lifetime 28800 s \(HAMH_INSTANCES_LIFETIME_S\)/);
    expect(() => resolveStageSettings({ instancesLifetimeS: "28800", instancesIdleS: "28801" })).toThrow(/HAMH_INSTANCES_LIFETIME_S/);
    expect(resolveStageSettings({ instancesLifetimeS: "172800", instancesIdleS: "40000" }).instances).toMatchObject({ maxLifetimeS: 172_800, idleInstanceTimeoutS: 40_000 });
  });

  test("the capacity provider carries the settings, gets a name of its own, and the runtime keeps a session under 8 hours", () => {
    const def = synth("direct", { ...phase2Images("direct"), computeInstances: "on" }).templates.get("hamh-direct-runtimes")!;
    const big = synth("direct", { ...phase2Images("direct"), computeInstances: "on", settings: set }).templates.get("hamh-direct-runtimes")!;
    const provider = (t: Template) => Object.values(resourcesOf(t, "AWS::BedrockAgentCore::CapacityProvider")).map((r) => r.Properties as any)[0];
    const ec2 = provider(big).ComputeConfiguration.Ec2Configuration;
    expect(ec2.LaunchTemplateSource.LaunchParameters.InstanceRequirements.AllowedInstanceTypes).toEqual(["m7g.8xlarge"]);
    expect(ec2.RootVolume.FreeSpaceGiB).toBe(200);
    expect(ec2.Volumes[0].EbsConfiguration.SizeGiB).toBe(400);
    expect(ec2.LifecycleConfiguration).toEqual({ MaxLifetime: 172_800, IdleInstanceTimeout: 3_600 });
    expect(provider(big).Name).not.toBe(provider(def).Name);
    expect(provider(big).Name).toBe(names.capacityProvider(stageConfig("direct", resolveStageSettings(set))));
    const runtimes = Object.values(resourcesOf(big, "AWS::BedrockAgentCore::Runtime")).map((r) => r.Properties as any);
    const inst = runtimes.find((r) => r.EnvironmentVariables?.AGENTD_DIRECT_CARGO_JOBS !== undefined)!;
    expect(inst.EnvironmentVariables.AGENTD_DIRECT_CARGO_JOBS).toBe("32");
    // A longer machine lifetime does not lengthen a session: the runtime takes the smaller of the microVM limit and the machine's.
    expect(inst.LifecycleConfiguration).toEqual({ IdleRuntimeSessionTimeout: 1_800, MaxLifetime: 28_800 });
  });
});

describe("D53: the pull request check", () => {
  const project = (t: Template) => Object.values(resourcesOf(t, "AWS::CodeBuild::Project")).map((r) => r.Properties as any).find((p) => p.Name === "hamh-direct-prcheck")!;

  test("the build may take 60 minutes on the small ARM machine, and queue 60 more", () => {
    const p = project(template("direct", "delivery", phase2Images("direct")));
    expect(p.TimeoutInMinutes).toBe(60);
    expect(p.QueuedTimeoutInMinutes).toBe(60);
    expect(p.Environment.ComputeType).toBe("BUILD_GENERAL1_SMALL");
    expect(p.Environment.Type).toBe("ARM_CONTAINER");
  });

  test("HAMH_PR_CHECK_MINUTES and HAMH_PR_CHECK_COMPUTE set the build time and the machine", () => {
    const p = project(template("direct", "delivery", { ...phase2Images("direct"), settings: { prCheckMinutes: "240", prCheckCompute: "2XLarge" } }));
    expect(p.TimeoutInMinutes).toBe(240);
    expect(p.Environment.ComputeType).toBe("BUILD_GENERAL1_2XLARGE");
  });

  test("the minutes are whole numbers from 5 to 480 and the machine is one of the five ARM sizes", () => {
    expect([...PR_CHECK_COMPUTE_WORDS]).toEqual(["small", "medium", "large", "xlarge", "2xlarge"]);
    for (const bad of ["4", "481", "1.5", "an hour", "-5"]) expect(() => resolveStageSettings({ prCheckMinutes: bad }), bad).toThrow(/HAMH_PR_CHECK_MINUTES=".*" is not a whole number of minutes from 5 to 480/);
    for (const bad of ["huge", "gpu", "BUILD_GENERAL1_SMALL"]) expect(() => resolveStageSettings({ prCheckCompute: bad }), bad).toThrow(/HAMH_PR_CHECK_COMPUTE=".*" is not one of small, medium, large, xlarge, 2xlarge/);
    expect(resolveStageSettings({ prCheckMinutes: "5" }).prCheck.buildMinutes).toBe(5);
    expect(resolveStageSettings({ prCheckMinutes: "480" }).prCheck.buildMinutes).toBe(480);
  });
});

describe("E7: HAMH_PROVIDER_SILENCE_S, the silence limit of Cline", () => {
  const runSessionEnv = (settings?: { providerSilenceS?: string }) => {
    const t = template("direct", "orchestration", { ...phase2Images("direct"), ...(settings === undefined ? {} : { settings }) });
    const fn = Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as any).find((p) => p.FunctionName === "hamh-direct-run-session")!;
    return fn.Environment.Variables as Record<string, unknown>;
  };

  test("unset, the run-session function gets no variable, so agentd keeps its 900 s", () => {
    expect(runSessionEnv()).not.toHaveProperty("HAMH_PROVIDER_SILENCE_S");
    expect(resolveStageSettings({}).providerSilenceS).toBeUndefined();
  });

  test("set, the function carries the seconds as text", () => {
    expect(runSessionEnv({ providerSilenceS: "1800" })).toHaveProperty("HAMH_PROVIDER_SILENCE_S", "1800");
    expect(stageConfig("direct", resolveStageSettings({ providerSilenceS: " 60 " })).providerSilenceS).toBe(60);
  });

  test("whole seconds from 60 to 3600 at both ends; any other word throws and names the variable", () => {
    for (const ok of ["60", "3600"]) expect(resolveStageSettings({ providerSilenceS: ok }).providerSilenceS).toBe(Number(ok));
    for (const bad of ["0", "59", "3601", "15m", "1.5", "-1", "soon"]) {
      expect(() => resolveStageSettings({ providerSilenceS: bad }), bad).toThrow(/HAMH_PROVIDER_SILENCE_S=".*" is not whole seconds from 60 to 3600/);
    }
  });
});

describe("the entry reads the settings once, under both prefixes, and stops a bad value by name before any stack is built", () => {
  test("the settings reach the app as the words the deploy was given", () => {
    const o = entryOptions({ ...ENV, HAMH_LOG_RETENTION_DAYS: " 90 ", HAMH_PR_CHECK_MINUTES: "120", HAMH_INSTANCES_TYPES: "m7g.8xlarge" });
    expect(o.settings).toEqual({ logRetentionDays: "90", prCheckMinutes: "120", instancesTypes: "m7g.8xlarge" });
    expect(entryOptions(ENV).settings).toBeUndefined();
  });

  test("the previous prefix still answers for the window of the previous names", () => {
    expect(entryOptions({ ...ENV, CAH_LOG_RETENTION_DAYS: "60" }).settings).toEqual({ logRetentionDays: "60" }); // COMPAT-CAH: the earlier prefix answers until the window ends
  });

  test("a bad value throws and names the variable", () => {
    expect(() => entryOptions({ ...ENV, HAMH_LOG_RETENTION_DAYS: "45" })).toThrow(/HAMH_LOG_RETENTION_DAYS="45"/);
    expect(() => entryOptions({ ...ENV, HAMH_PR_CHECK_MINUTES: "3" })).toThrow(/HAMH_PR_CHECK_MINUTES="3"/);
    expect(() => entryOptions({ ...ENV, HAMH_INSTANCES_IDLE_S: "5" })).toThrow(/HAMH_INSTANCES_IDLE_S="5"/);
  });
});
