import { App, Stack, Tags } from "aws-cdk-lib";
import {
  ACCOUNT, checkResearchProvider, DEFAULT_RESEARCH_PROVIDER, KEEP_TAG_EXCEPTIONS, KEEP_TAGS, OWNER, PROJECT, REGION, type ResearchProvider, resolveComputeInstances, resolveToolsMode, resolveUtilityModel,
  resolveResearchAlarms, resolveUtilityPurposes, STAGE_KIND, type Stage, stageConfig,
} from "./config.ts";
import { ControlStack } from "./stacks/control.ts";
import { DataStack } from "./stacks/data.ts";
import { DeliveryStack } from "./stacks/delivery.ts";
import { NetworkStack } from "./stacks/network.ts";
import { ObservabilityStack } from "./stacks/observability.ts";
import { DirectOrchestrationStack } from "./stacks/orchestration-direct.ts";
import { PackagesStack } from "./stacks/packages.ts";
import { RuntimesStack } from "./stacks/runtimes.ts";
import { SecurityStack } from "./stacks/security.ts";

export interface BuildOptions {
  stage: Stage;
  outdir?: string;
  /** cargo-lambda output directory; defaults to the test fixture (the CLI entry never relies on it). */
  lambdaDir?: string;
  /** `<repo>@sha256:<digest>` of the python worker image; without it only the ECR repositories deploy. */
  workerImage?: string;
  /** Runtime environment revision; `deploy.sh --bump-worker-version` adds 1. Default 0. */
  workerRevision?: number;
  /** `<repo>@sha256:<digest>` of the delivery image; without it only the ECR repository and roles deploy. */
  deliveryImage?: string;
  /** The research route's search provider; default `tavily`. */
  researchProvider?: ResearchProvider;
  /** `HAMH_TAVILY_ALARMS`: `on` (default) or `off`; the four alarms on the stored Tavily key exist only on a `tavily` stage with `on`. */
  tavilyAlarms?: string;
  /** `HAMH_TAVILY_KEY_MAX_AGE_DAYS`: an integer from 1 to 365, default 30; the key-age alarm fires at this age. */
  tavilyKeyMaxAgeDays?: string;
  /** `HAMH_RESEARCH_ALARM_TOPIC_ARN`: an SNS topic in the stage Region for the alarms; unset means the alarms have no actions. */
  researchAlarmTopicArn?: string;
  /**
   * The utility model of the run names, as `HAMH_UTILITY_MODEL` states it: an id or alias of a row of utility-models.json, or `off`. Unset or
   * blank means the catalog default. A word that names no row, or a row that cannot be granted in this stage's Region, throws (`resolveUtilityModel`).
   */
  utilityModel?: string;
  /**
   * The purposes of the utility model the stage runs, as `HAMH_UTILITY_PURPOSES` states it: a comma list of `run-name` and `run-tags`. Unset or blank
   * means every purpose the row holds a smoke record for. A word that is no purpose, or a purpose with no record, throws (`resolveUtilityPurposes`).
   */
  utilityPurposes?: string;
  /**
   * The stage setting of the tool layer, as `HAMH_TOOLS_MODE` states it: `off` or `on` in any letter case. Unset or blank means `off`. Any other word
   * throws (`resolveToolsMode`). The worker runtime's environment carries the resolved word as `AGENTD_TOOLS_MODE`, with a worker image only.
   */
  toolsMode?: string;
  /**
   * The stage setting of the Instances compute type, as `HAMH_COMPUTE_INSTANCES` states it: `off` or `on` in any letter case. Unset or blank means `off`. Any other word throws
   * (`resolveComputeInstances`). On, the stage also holds an Instances capacity provider with its operator role, a second worker runtime on the same image and run-session's grants for it.
   */
  computeInstances?: string;
}

const FIXTURE_LAMBDA_DIR = new URL("../test/fixtures/lambda", import.meta.url).pathname;

/**
 * Builds the nine stacks of stage `direct` (`STACK_NAMES`), with the four project tags on everything, each stack included, and on the test entry
 * (accounts.json role `test`) the keep tag `auto-delete=no` as well, except where `KEEP_TAG_EXCEPTIONS` says otherwise.
 */
export function buildApp(opts: BuildOptions): App {
  const cfg = stageConfig(opts.stage);
  const researchProvider = opts.researchProvider ?? DEFAULT_RESEARCH_PROVIDER;
  checkResearchProvider(researchProvider, STAGE_KIND);
  // The Tavily alarm settings, resolved before any stack exists for the same reason.
  const researchAlarms = resolveResearchAlarms({
    alarms: opts.tavilyAlarms, maxKeyAgeDays: opts.tavilyKeyMaxAgeDays, topicArn: opts.researchAlarmTopicArn,
  });
  // Resolved here, before any stack exists, so a bad word stops the synth by name.
  const utilityModel = resolveUtilityModel(opts.utilityModel, REGION);
  // The purposes, resolved against that model for the same reason.
  const utilityPurposes = resolveUtilityPurposes(opts.utilityPurposes, utilityModel);
  // The tool layer's stage setting, resolved before any stack exists for the same reason.
  const toolsMode = resolveToolsMode(opts.toolsMode);
  // The Instances compute type's stage setting, resolved before any stack exists for the same reason.
  const computeInstances = resolveComputeInstances(opts.computeInstances) === "on";
  const app = new App(opts.outdir ? { outdir: opts.outdir } : {});
  const env = { account: ACCOUNT, region: REGION };
  const stack = (name: string) => ({ env, stackName: `${cfg.prefix}-${name}`, cfg });
  const network = new NetworkStack(app, `${cfg.prefix}-network`, stack("network"));
  const security = new SecurityStack(app, `${cfg.prefix}-security`, { ...stack("security"), network, computeInstances });
  const data = new DataStack(app, `${cfg.prefix}-data`, { ...stack("data"), security });
  const packages = new PackagesStack(app, `${cfg.prefix}-packages`, { ...stack("packages"), security, network });
  const control = new ControlStack(app, `${cfg.prefix}-control`, {
    ...stack("control"), network, security, data, lambdaDir: opts.lambdaDir ?? FIXTURE_LAMBDA_DIR, researchProvider, researchAlarms,
  });
  const runtimes = new RuntimesStack(app, `${cfg.prefix}-runtimes`, {
    ...stack("runtimes"), network, security, workerImage: opts.workerImage, workerRevision: opts.workerRevision ?? 0, packages, toolsMode, computeInstances,
  });
  // The runtimes name the Go module store (a bucket of the packages stack) by convention, with no CloudFormation reference,
  // so the assembly needs the edge scripts/lib/deploy.py orders by.
  runtimes.addStackDependency(packages, "the Go module store the runtimes name in AGENTD_GOPROXY");
  // Direct run: the run machine and run-session; the delivery functions are the shared delivery image's.
  new DirectOrchestrationStack(app, `${cfg.prefix}-orchestration`, {
    ...stack("orchestration"), security, data, packages, internalApiUrl: control.internalApiUrl, lambdaDir: opts.lambdaDir ?? FIXTURE_LAMBDA_DIR,
    runtimes: { workers: runtimes.workerRuntimeArns, computeInstances }, researchProvider, utilityModel, utilityPurposes,
  });
  new DeliveryStack(app, `${cfg.prefix}-delivery`, { ...stack("delivery"), security, data, deliveryImage: opts.deliveryImage });
  new ObservabilityStack(app, `${cfg.prefix}-observability`, stack("observability"))
    .addStackDependency(data, "the harness bucket its trail records and the invocation log group it reads");
  // The four tags go on every taggable resource and on each stack itself. cdk.json sets the feature flag
  // `@aws-cdk/core:explicitStackTags`, under which `Tags.of(app).add` skips the stack (it adds `aws:cdk:stack` to
  // the excluded resource types), so a stack needs its own add that includes only that type. The tags of a stack
  // are in the manifest artifact, not the template; `cdk deploy` passes them to CloudFormation.
  for (const [key, value] of Object.entries({ project: PROJECT, stage: cfg.stage, owner: OWNER, "managed-by": "cdk" })) {
    Tags.of(app).add(key, value);
    Tags.of(app).add(key, value, { includeResourceTypes: ["aws:cdk:stack"] });
  }
  // On the test entry the keep tag goes the same two ways, minus what KEEP_TAG_EXCEPTIONS names (a type whose tag update replaces it, and the
  // stack that holds it, because CloudFormation copies a stack tag to the resources of the stack). `cdk deploy` passes a stack tag to CloudFormation,
  // which also copies it to the resources that take tags.
  const keepStacks = app.node.children.filter((c): c is Stack => Stack.isStack(c))
    .filter((c) => !KEEP_TAG_EXCEPTIONS.stacks.some((n) => c.stackName === `${cfg.prefix}-${n}`));
  for (const [key, value] of Object.entries(KEEP_TAGS)) {
    Tags.of(app).add(key, value, { excludeResourceTypes: [...KEEP_TAG_EXCEPTIONS.resourceTypes] });
    for (const keepStack of keepStacks) Tags.of(keepStack).add(key, value, { includeResourceTypes: ["aws:cdk:stack"] });
  }
  return app;
}
