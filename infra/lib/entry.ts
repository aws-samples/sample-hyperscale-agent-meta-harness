// Options for the CLI entry (bin/app.ts), the only place the app reads the environment.
// scripts/deploy.sh sets HAMH_LAMBDA_DIR to the cargo-lambda output, HAMH_WORKER_IMAGE and
// HAMH_DELIVERY_IMAGE to the digest-pinned images (unset or empty: that image's phase 1) and
// HAMH_WORKER_REVISION (default 0) to the stage's worker revision; synth checks pass test/fixtures/lambda.
// HAMH_RESEARCH_PROVIDER, HAMH_UTILITY_MODEL, HAMH_UTILITY_PURPOSES, HAMH_TOOLS_MODE and HAMH_COMPUTE_INSTANCES are optional (below).
// Every variable is read through lib/env.ts, which also answers to the name it had before the rename for the compatibility window (COMPAT-CAH).
// There is no silent fallback: a deploy without the real Lambda directory once shipped the fixture as a worker-api.
import { existsSync } from "node:fs";
import { join } from "node:path";
import type { BuildOptions } from "./app.ts";
import { lookupEnv, resolveEnv } from "./env.ts";
import {
  LAMBDAS, REGION, RESEARCH_PROVIDERS, type ResearchProvider, resolveComputeInstances, resolveResearchAlarms, resolveToolsMode, resolveUtilityModel, resolveUtilityPurposes,
  type Stage, trimSetting,
} from "./config.ts";

export function entryOptions(env: Record<string, string | undefined>): BuildOptions {
  const lambda = resolveEnv(env, "LAMBDA_DIR");
  const lambdaDir = lambda.value;
  if (!lambdaDir) {
    throw new Error("HAMH_LAMBDA_DIR is required: the cargo-lambda output directory (scripts/deploy.sh sets it; synth checks use infra/test/fixtures/lambda)");
  }
  const workerRevision = resolveEnv(env, "WORKER_REVISION");
  const revision = workerRevision.value ?? "0";
  if (!/^(0|[1-9][0-9]*)$/.test(revision)) {
    throw new Error(`${workerRevision.name} must be a non-negative integer, got "${revision}"`);
  }
  const context = JSON.parse(env.CDK_CONTEXT_JSON ?? "{}") as { stage?: Stage };
  // The binaries of the stage: worker-api, control-api, run-session and the two research functions.
  for (const name of LAMBDAS) {
    if (!existsSync(join(lambdaDir, name, "bootstrap"))) {
      throw new Error(`${lambda.name} ${lambdaDir} has no ${name}/bootstrap`);
    }
  }
  // The research route's search provider; absent or empty keeps the default. Validated here by name; whether the stage may
  // use it (fixture is for test stages only) is judged when the app is built.
  const providerSetting = resolveEnv(env, "RESEARCH_PROVIDER");
  const provider = providerSetting.value ?? "";
  if (provider !== "" && !(RESEARCH_PROVIDERS as readonly string[]).includes(provider)) {
    throw new Error(`${providerSetting.name} must be one of ${RESEARCH_PROVIDERS.join(", ")}, got "${provider}"`);
  }
  // The utility model of the run names (crates/ham-core/assets/utility-models.json). Absent or blank keeps the catalog default, `off` removes
  // the grant, an id or alias selects a row. Resolved here for the stage Region so that a bad word stops the deploy by name before any stack is
  // built; the app resolves the same word again.
  const utility = (lookupEnv(env, "UTILITY_MODEL") ?? "").trim();
  const utilityModel = resolveUtilityModel(utility, REGION);
  // The purposes the utility model serves (a comma list of run-name and run-tags). Absent or blank means every purpose the catalog row has a smoke
  // record for; a word that is no purpose, or a purpose with no record, stops the deploy by name here as well.
  const purposes = (lookupEnv(env, "UTILITY_PURPOSES") ?? "").trim();
  resolveUtilityPurposes(purposes, utilityModel);
  // The stage setting of the tool layer. Absent or blank keeps the default (off, no option); `off` or `on` in any letter case selects it; any other word
  // stops the deploy here by name, before a stack is built. The app resolves the same word again.
  const tools = trimSetting(lookupEnv(env, "TOOLS_MODE") ?? "");
  resolveToolsMode(tools);
  // The Tavily key alarms. Absent or empty keeps the default; a bad value stops the deploy here by name.
  const alarms = trimSetting(lookupEnv(env, "TAVILY_ALARMS") ?? "");
  const maxAge = trimSetting(lookupEnv(env, "TAVILY_KEY_MAX_AGE_DAYS") ?? "");
  const topic = trimSetting(lookupEnv(env, "RESEARCH_ALARM_TOPIC_ARN") ?? "");
  resolveResearchAlarms({ alarms, maxKeyAgeDays: maxAge, topicArn: topic });
  // The Instances compute type is an opt-in of the deploy. Absent or blank keeps it off; `off` or `on` in any letter case selects it; any other word stops the deploy
  // here by name. The app resolves the same word again.
  const instances = trimSetting(lookupEnv(env, "COMPUTE_INSTANCES") ?? "");
  resolveComputeInstances(instances);
  return {
    stage: context.stage ?? "direct", lambdaDir, workerImage: lookupEnv(env, "WORKER_IMAGE") || undefined, workerRevision: Number(revision),
    deliveryImage: lookupEnv(env, "DELIVERY_IMAGE") || undefined,
    ...(provider === "" ? {} : { researchProvider: provider as ResearchProvider }),
    ...(utility === "" ? {} : { utilityModel: utility }),
    ...(purposes === "" ? {} : { utilityPurposes: purposes }),
    ...(tools === "" ? {} : { toolsMode: tools }),
    ...(alarms === "" ? {} : { tavilyAlarms: alarms }),
    ...(maxAge === "" ? {} : { tavilyKeyMaxAgeDays: maxAge }),
    ...(topic === "" ? {} : { researchAlarmTopicArn: topic }),
    ...(instances === "" ? {} : { computeInstances: instances }),
  };
}
