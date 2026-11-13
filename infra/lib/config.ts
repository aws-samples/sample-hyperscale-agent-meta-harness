// Account, Region, stack names and the settings of stage `direct`.
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { isAbsolute } from "node:path";
import { fileURLToPath } from "node:url";
import { EARLIER_PREFIX, lookupEnv, resolveEnv } from "./env.ts";
import { KEEP_TAG } from "./keep-tag.ts";
import { callerArnPatterns, type Operator, parseOperator } from "./operator.ts";
import { ownerOf, parseOwner } from "./owner.ts";
import { parseRepoNames, repoNamesOf } from "./repo-names.ts";

/** One entry of the repository's `accounts.json`. */
export interface Target {
  account: string;
  region: string;
  profile: string;
  role: "active" | "history" | "test";
  /** Stage AZ names in the entry's Region, and the AZ IDs they map to in that account (AgentCore VPC IDs). */
  azs: Record<Stage, string[]>;
  azIds: Record<Stage, string[]>;
  invocationLogging: "enable-and-restore" | "read-only";
  /** The identity that runs setup and operates the stage. Absent: the IAM role `Admin`, as before the field existed. */
  operator?: Operator;
  /** The value of the `owner` tag of the stage (lib/owner.ts). Absent: the name of the `operator`, else `hamh-user`. */
  owner?: string;
  /** The CodeCommit repository names a run may open a pull request in (lib/repo-names.ts). Absent: `["hamh-e2e-*"]`. */
  repoNames?: string[];
}

const SELECTOR = "HAMH_AWS_PROFILE";
// COMPAT-CAH: an accounts file that setup.sh generated before the rename names the selector by its earlier word; both words are read, the file is never rewritten.
const SELECTOR_WORDS: readonly string[] = [SELECTOR, `${EARLIER_PREFIX}AWS_PROFILE`];
const STAGES = ["direct"] as const;

/**
 * The accounts.json entry `HAMH_AWS_PROFILE` names by its profile; unset or empty selects the one
 * `active` entry, and `AWS_PROFILE` never selects. The file rules are scripts/lib/aws_target.py's:
 * selector `HAMH_AWS_PROFILE` (or its earlier word, for a file generated before the rename), exactly one active entry, unique profiles, and per stage AZ names in
 * the entry's Region paired one to one with AZ IDs, the optional `operator` (an object with kind `role` or `user` and an IAM name; lib/operator.ts) and the optional `owner` (the value of the `owner` tag; lib/owner.ts). Anything else throws.
 */
export function selectTarget(doc: any, env: Record<string, string | undefined>): Target {
  if (doc?.schemaVersion !== 1 || !SELECTOR_WORDS.includes(doc?.selector) || !Array.isArray(doc?.accounts) || doc.accounts.length === 0) {
    throw new Error(`accounts.json: expected schemaVersion 1, selector ${SELECTOR_WORDS.join(" or ")} and a non-empty accounts list`);
  }
  const entries: Target[] = doc.accounts.map((e: any, i: number) => {
    const where = `accounts.json accounts[${i}]`;
    if (!/^[0-9]{12}$/.test(String(e?.account))) throw new Error(`${where}: account must be 12 digits`);
    // nosemgrep: detect-redos -- runs on one Region name of the committed accounts.json, each repeated group starts with a hyphen that the letter class cannot match, so the match has one split and cannot backtrack badly
    if (!/^[a-z]{2}(-[a-z]+)+-[0-9]$/.test(String(e?.region))) throw new Error(`${where}: region ${e?.region} is not a Region name`);
    if (typeof e.profile !== "string" || !/^[A-Za-z0-9._-]+$/.test(e.profile)) throw new Error(`${where}: profile must be a profile name`);
    if (e.role !== "active" && e.role !== "history" && e.role !== "test") throw new Error(`${where}: role must be one of active, history, test`);
    if (e.invocationLogging !== "enable-and-restore" && e.invocationLogging !== "read-only") {
      throw new Error(`${where}: invocationLogging must be enable-and-restore or read-only`);
    }
    for (const s of STAGES) {
      const names: unknown = e.azs?.[s];
      const ids: unknown = e.azIds?.[s];
      if (!Array.isArray(names) || !Array.isArray(ids) || names.length === 0 || names.length !== ids.length) {
        throw new Error(`${where}: azs.${s} and azIds.${s} must pair up one to one`);
      }
      for (const az of names) {
        // nosemgrep: detect-non-literal-regexp -- the Region name comes from the committed accounts.json and was checked as a Region name earlier in this function, so the pattern holds only lowercase letters, digits and hyphens
        if (typeof az !== "string" || !new RegExp(`^${e.region}[a-z]$`).test(az)) throw new Error(`${where}: ${az} is not in ${e.region}`);
      }
    }
    const operator = Object.prototype.hasOwnProperty.call(e, "operator") ? parseOperator(e.operator, where) : undefined;
    const owner = Object.prototype.hasOwnProperty.call(e, "owner") ? parseOwner(e.owner, where) : undefined;
    const repoNames = Object.prototype.hasOwnProperty.call(e, "repoNames") ? parseRepoNames(e.repoNames, where) : undefined;
    return { account: e.account, region: e.region, profile: e.profile, role: e.role, azs: e.azs, azIds: e.azIds,
      invocationLogging: e.invocationLogging, ...(operator ? { operator } : {}), ...(owner ? { owner } : {}), ...(repoNames ? { repoNames } : {}) };
  });
  const seen = new Set<string>();
  for (const t of entries) {
    if (seen.has(t.profile)) throw new Error(`accounts.json: profile ${t.profile} twice`);
    seen.add(t.profile);
  }
  if (entries.filter((t) => t.role === "active").length !== 1) throw new Error("accounts.json: exactly one active entry is required");
  const selected = resolveEnv(env, "AWS_PROFILE");
  const want = selected.value ?? "";
  if (!want) return entries.find((t) => t.role === "active")!;
  const hit = entries.find((t) => t.profile === want);
  if (!hit) throw new Error(`${selected.name}=${want} names no entry of accounts.json (profiles: ${entries.map((t) => t.profile).join(", ")})`);
  return hit;
}

/** The account-file override. */
export const ACCOUNTS_FILE_ENV = "HAMH_ACCOUNTS_FILE";

/**
 * The file the app reads its accounts from: `HAMH_ACCOUNTS_FILE` when it is set and not empty (an absolute path, else a refusal), else the committed
 * accounts.json. `setup.sh` generates a file for the account it deploys into, so the committed file is never edited. A set variable never falls
 * back to the committed file. scripts/node.sh mounts the file into the CDK container at this path.
 */
export function accountsFilePath(env: Record<string, string | undefined>): string {
  const file = resolveEnv(env, "ACCOUNTS_FILE");
  const value = file.value ?? "";
  if (value === "") return fileURLToPath(new URL("../../accounts.json", import.meta.url));
  if (!isAbsolute(value)) throw new Error(`${file.name} must be an absolute path (got ${JSON.stringify(value)})`);
  return value;
}

/** The parsed accounts file `accountsFilePath(env)` names; an unreadable or malformed file throws and the message names the path. */
export function loadAccounts(env: Record<string, string | undefined>): any {
  const path = accountsFilePath(env);
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (e: any) {
    throw new Error(`${path}: ${e?.code ?? e?.message ?? e}`);
  }
  try {
    return JSON.parse(text);
  } catch (e: any) {
    throw new Error(`${path}: ${e?.message ?? e}`);
  }
}

/** The selected entry: read once when the app loads (scripts/node.sh passes every HAMH_* variable). */
export const TARGET: Target = selectTarget(loadAccounts(process.env), process.env);
export const ACCOUNT = TARGET.account;
export const REGION = TARGET.region;
/** The AWS CLI profile of the selected account (scripts set AWS_PROFILE from it). */
export const PROFILE = TARGET.profile;
export const PROJECT = "hyperscale-agent-meta-harness";
/** The value of the `owner` tag of every taggable resource of the stage, of the repositories the repository gateway creates and of the worker sessions' `tenant`: `HAMH_OWNER` when it is set, else the entry's `owner`, else the name of its `operator`, else `hamh-user` (lib/owner.ts). */
export const OWNER = ownerOf(TARGET, process.env);
/** The repository-name rule of the stage: `HAMH_REPO_NAMES` when it is set, else the entry's `repoNames`, else `["hamh-e2e-*"]` (lib/repo-names.ts). The control-api and delivery functions read it as `HAMH_REPO_NAMES`. */
export const REPO_NAMES: string[] = repoNamesOf(TARGET, process.env);

/**
 * The characters a stage setting is trimmed of (`HAMH_TOOLS_MODE`, `HAMH_KEEP_TAG` and the others): space, tab, line feed, carriage return, form feed and vertical tab. Python's
 * `strip()` and JavaScript's `trim()` differ on U+001C to U+001F, U+0085 and U+FEFF, so neither resolver uses them and both accept the same words (test/fixtures/tools-mode.json,
 * scripts/lib/deploy.py `TOOLS_SPACE`). Declared before the first setting this module resolves when it loads.
 */
const TOOLS_SPACE = " \t\n\r\f\v";

/** `setting` without the ASCII whitespace around it. */
export function trimSetting(setting: string): string {
  let start = 0;
  let end = setting.length;
  while (start < end && TOOLS_SPACE.includes(setting.charAt(start))) start++;
  while (end > start && TOOLS_SPACE.includes(setting.charAt(end - 1))) end--;
  return setting.slice(start, end);
}

/**
 * A stage setting whose unset value is what the role of the selected entry gives: the keep tag and the stage kind. Unset, empty or blank is the role's word; otherwise
 * one of `words` in any letter case with the ASCII whitespace around it ignored. Any other word throws, names the setting and lists the role's words, so a synth stops.
 */
function roleWord<W extends string>(name: string, setting: string | undefined, words: readonly W[], ofRole: Readonly<Record<Target["role"], W>>,
  role: Target["role"]): W {
  const word = trimSetting(setting ?? "").toLowerCase();
  if (word === "") return ofRole[role];
  if ((words as readonly string[]).includes(word)) return word as W;
  const roles = (Object.keys(ofRole) as Target["role"][]).map((r) => `${ofRole[r]} for a ${r} entry`).join(", ");
  throw new Error(`${name}="${setting}" is not one of ${words.join(", ")} (unset or empty means what the role of the account entry gives: ${roles})`);
}

/** The words of `HAMH_KEEP_TAG`: on puts the keep tag on the stage, off leaves it off. */
export const KEEP_TAG_WORDS = ["on", "off"] as const;
export type KeepTagWord = (typeof KEEP_TAG_WORDS)[number];
/** What an unset `HAMH_KEEP_TAG` comes to for each role (scripts/lib/deploy.py `ROLE_DEFAULTS` holds the same table; scripts/tests/test_setup_deploy_settings.py pins it). */
export const KEEP_TAG_OF_ROLE: Readonly<Record<Target["role"], KeepTagWord>> = { active: "off", history: "off", test: "on" };

/** Resolves `HAMH_KEEP_TAG` for an entry of role `role` (`roleWord`). */
export function resolveKeepTag(setting: string | undefined, role: Target["role"]): KeepTagWord {
  return roleWord("HAMH_KEEP_TAG", setting, KEEP_TAG_WORDS, KEEP_TAG_OF_ROLE, role);
}

/**
 * The keep tag the app puts on the taggable resources and the stacks of the selected entry (lib/keep-tag.ts): `auto-delete=no` for the `test`
 * entry, an account whose cleanup service deletes an untagged resource about 7 days after it first sees it, and nothing for the
 * `active` and `history` entries, whose templates stay as they were. The role decides while `HAMH_KEEP_TAG` is unset: the profile name and the
 * account never do. `HAMH_KEEP_TAG` (on or off) decides when it is set: a stage keeps the word it was deployed with, whatever role the account file of a later
 * deploy gives its entry, and scripts/lib/deploy.py hands that word to every cdk call.
 */
export function keepTags(target: Pick<Target, "role">, env: Record<string, string | undefined> = process.env): Record<string, string> {
  return resolveKeepTag(lookupEnv(env, "KEEP_TAG"), target.role) === "on" ? { ...KEEP_TAG } : {};
}
export const KEEP_TAGS: Record<string, string> = keepTags(TARGET);

/**
 * What takes no keep tag, and why. The CloudFormation resource reference gives `AWS::Route53Resolver::ResolverQueryLoggingConfig` the update
 * behavior "Replacement" for `Tags`, and `cdk diff` against the base template reports the replacement of that resource and of its association (its
 * `ResolverQueryLogConfigId` changes). A VPC delivers DNS query logs to one CloudWatch Logs destination, so the new association would likely be refused
 * while the old one still exists and the network stack update would roll back. A stack tag reaches the same resource, so the `network` stack takes none
 * either. The configuration of the test stage carries `auto-delete=no` already.
 * Every other taggable type of the nine stacks and of the development builder updates its tags in place.
 */
export const KEEP_TAG_EXCEPTIONS: { resourceTypes: readonly string[]; stacks: readonly StackName[] } = {
  resourceTypes: ["AWS::Route53Resolver::ResolverQueryLoggingConfig"],
  stacks: ["network"],
};

export type StackName = "network" | "security" | "data" | "packages" | "control" | "orchestration" | "runtimes" | "delivery" | "observability";

/**
 * The nine stacks of stage `direct`, deployed as `hamh-direct-<name>`. Documentation checks read the
 * list and the next one by their names and their shape.
 */
export const DIRECT_STACKS: readonly StackName[] = [
  "network", "security", "data", "packages", "control", "orchestration", "runtimes", "delivery", "observability",
] as const;
export const STACK_NAMES = DIRECT_STACKS;

/**
 * The Rust Lambda binaries of the stage: worker-api, control-api and run-session, and the two functions of the research route.
 * scripts/deploy.sh builds each into
 * `<lambdaDir>/<name>/bootstrap`.
 */
export const DIRECT_LAMBDAS = ["worker-api", "control-api", "run-session", "research-api", "research-job"] as const;
export const LAMBDAS = DIRECT_LAMBDAS;

/**
 * The Go module store: per stage one S3 bucket `hamh-<stage>-gomod-<account>` holds an approved,
 * create-only GOPROXY tree. An operator fills it with `scripts/gomod-store.sh fill`; workers read it
 * anonymously through the stage's S3 gateway endpoint, so no credential enters an agent microVM and the egress allowlist is
 * unchanged. Object Lock is GOVERNANCE (a COMPLIANCE lock would stop teardown, invariant 7) with this many days of default
 * retention on every stored version; the real guard is the bucket policy (create-only puts, writes by the filler role alone,
 * deletes by the deploying principal alone: the IAM role `Admin`, or the entry's `operator`).
 */
export const GOMOD_RETENTION_DAYS = 365;
/** The two derived index objects of a GOPROXY tree per module: they change by compare-and-swap, every other object is create-only. */
export const GOMOD_INDEX_KEYS = ["*/@v/list", "*/@latest"] as const;

/** The delivery container-image functions. */
export const DELIVERY_FUNCTIONS = ["repo-gateway", "bundle-validator", "packager"] as const;
export type DeliveryFunction = (typeof DELIVERY_FUNCTIONS)[number];

/** The bootstrap engine-policy version: what a new Run item records when no signed
 * version routes it (an empty `ENGINEPOLICY` partition). control-api's `ENGINE_POLICY_VERSION`. */
export const ENGINE_POLICY_VERSION = 0;

/** The models the product supports: two from OpenAI, two from
 * Anthropic and one from Z.ai: GPT-6.1 Sol, GPT-6 Astra, Claude Opus 5.5, Claude Sonnet 5.5 and Z.ai GLM-5 (the stage account lists
 * no GLM 5.3; `zai.glm-5` is the nearest). The declaration stays on one line (scripts and Rust tests read the
 * list by that shape: crates/control-api/tests/cost.rs). */
export const RUN_SUPPORTED_MODELS = ["us.openai.gpt-6.1-sol", "us.openai.gpt-6-astra", "us.anthropic.claude-opus-5-5", "global.anthropic.claude-sonnet-5-5", "zai.glm-5"];

/** Evaluators with a passing calibration (0 false accepts on the 28-case corpus without worker summaries), in preference
 * order. The declaration stays on one line: crates/control-api/tests/cost.rs (every evaluator has a price row) reads it by
 * that shape. */
export const RUN_EVALUATOR_MODELS = ["us.openai.gpt-5.5", "us.anthropic.claude-opus-5-5", "us.xai.grok-4.7"];

/** The entry's operator identity, or undefined when it names none and the stage keeps the IAM role `Admin`. */
export const OPERATOR: Operator | undefined = TARGET.operator;

/** Operator principals (IAM ARN patterns, comma separated): `/v1/admin/*` and the owners of TDD-qualification
 * runs (control-api's OPERATOR_PRINCIPALS). The assumed-role sessions of the IAM role `Admin` unless the entry names an `operator`: then the sessions of
 * that role, or that IAM user. */
export const OPERATOR_PRINCIPALS = OPERATOR ? callerArnPatterns(ACCOUNT, OPERATOR).join(",") : `arn:aws:sts::${ACCOUNT}:assumed-role/Admin/*`;

/** The run engine: the engine of a session no engine-policy route covers. It is control-api's `ENGINE`. Cline, the fork, is
 * the default engine; OpenCode is the second, and a start request names the one a run uses (`RUN_ENGINE_SET` below). A script reads the declaration below by
 * pattern and takes the first match in the file, so the declaration stays on one line and no comment quotes it (`infra/test/engine.test.ts`
 * applies the same pattern). */
export const RUN_ENGINE = "cline";

/** The engines this release deploys, control-api's `ENGINE_SET`: the engine profiles a start request may name. `RUN_ENGINE` is a member (the module
 * fails to load otherwise, so a synth and a deploy stop). A run may use an engine only when the model catalog has a row with a passing smoke record for
 * the pair, so adding an engine here is not enough: it needs its K-suite and its rows in `engine/catalog/models.json`. */
export const RUN_ENGINE_SET = ["cline", "opencode"] as const;
if (!(RUN_ENGINE_SET as readonly string[]).includes(RUN_ENGINE)) {
  throw new Error(`RUN_ENGINE ${RUN_ENGINE} is not in RUN_ENGINE_SET ${RUN_ENGINE_SET.join(",")}`);
}

/** Bedrock inference Regions, in criteria order. */
export const INFERENCE_REGIONS = ["us-west-2", "us-east-1", "us-east-2"] as const;

/** Every inference Region except `home`, in criteria order: the Regions a worker reaches over the NAT
 * allowlist. */
export function remoteInferenceRegions(home: string): string[] {
  if (!(INFERENCE_REGIONS as readonly string[]).includes(home)) {
    throw new Error(`home Region ${home} is not an inference Region (${INFERENCE_REGIONS.join(", ")})`);
  }
  return INFERENCE_REGIONS.filter((r) => r !== home);
}
export const REMOTE_INFERENCE_REGIONS: readonly string[] = remoteInferenceRegions(REGION);

export type Stage = "direct";

/**
 * Whether the stage is the official one or a test stage. `HAMH_STAGE_KIND` (official or test) decides when it is set: a stage keeps its kind whatever role the account
 * file of a later deploy gives its entry. The research route's fixture search provider (canned answers, no Tavily key) is allowed on a test stage only: synthesis throws
 * otherwise, and the Lambda refuses it at run time from `HAMH_STAGE_KIND`.
 */
export const STAGE_KINDS = ["official", "test"] as const;
export type StageKind = (typeof STAGE_KINDS)[number];
/** What an unset `HAMH_STAGE_KIND` comes to for each role (scripts/lib/deploy.py `ROLE_DEFAULTS` holds the same table). */
export const STAGE_KIND_OF_ROLE: Readonly<Record<Target["role"], StageKind>> = { active: "official", history: "test", test: "test" };

/** Resolves `HAMH_STAGE_KIND` for an entry of role `role` (`roleWord`). */
export function resolveStageKind(setting: string | undefined, role: Target["role"]): StageKind {
  return roleWord("HAMH_STAGE_KIND", setting, STAGE_KINDS, STAGE_KIND_OF_ROLE, role);
}

/** The kind of the stage: `HAMH_STAGE_KIND` of `env` when it is set, else the role of `target`. */
export function stageKind(target: Pick<Target, "role">, env: Record<string, string | undefined> = process.env): StageKind {
  return resolveStageKind(lookupEnv(env, "STAGE_KIND"), target.role);
}
export const STAGE_KIND: StageKind = stageKind(TARGET);

/**
 * The search providers of the research route, chosen by the stage and never by a caller. `agentcore` searches AWS's own
 * web index through an AgentCore Gateway with the Web Search Tool connector: it needs no key and a query stays inside AWS. `auto`
 * builds both backends, the gateway and the Tavily secret, and research-api chooses for every request: Tavily when a key is stored in the stage's
 * secret, AgentCore Web Search when none is. The other four words pin one provider. Only `agentcore` and `auto` make the control stack create a gateway
 * (`lib/stacks/research-gateway.ts`); `tavily`, `tavily-keyless` and `fixture` synthesize what they did before.
 */
export const RESEARCH_PROVIDERS = ["tavily", "tavily-keyless", "fixture", "agentcore", "auto"] as const;
export type ResearchProvider = (typeof RESEARCH_PROVIDERS)[number];
export const DEFAULT_RESEARCH_PROVIDER: ResearchProvider = "auto";

/**
 * The providers that can call Tavily with the stored key: the four alarms on that key exist for these and for no other.
 * `auto` is the provider that searches with AgentCore Web Search until a Tavily key is stored and with Tavily first once one is, so it can call Tavily too. The set
 * is written once, here.
 */
export const TAVILY_CAPABLE_PROVIDERS: readonly ResearchProvider[] = ["tavily", "auto"];
export function carriesTavily(provider: ResearchProvider): boolean {
  return TAVILY_CAPABLE_PROVIDERS.includes(provider);
}

/** The providers whose research-api calls the AgentCore Gateway: the control stack builds the gateway, its target and its service role for these and for no other. */
export const GATEWAY_PROVIDERS: readonly ResearchProvider[] = ["agentcore", "auto"];
export function carriesGateway(provider: ResearchProvider): boolean {
  return GATEWAY_PROVIDERS.includes(provider);
}

/**
 * The Web Search Tool connector of the `agentcore` provider. The gateway target is named `web-search-tool`; a gateway publishes a target's tool as
 * `<target name>___<tool name>` (three underscores), and the connector's one tool is `WebSearch` (measured on a real gateway). research-api
 * calls the tool by this name (its `AGENTCORE_SEARCH_TOOL` setting), so the two constants move together.
 */
export const WEB_SEARCH_TARGET = "web-search-tool";
export const WEB_SEARCH_CONNECTOR = "web-search";
export const AGENTCORE_SEARCH_TOOL = `${WEB_SEARCH_TARGET}___WebSearch`;

/** The stage-wide Tavily calls a minute research-api allows: 80 percent of a development key's 100 requests a minute. */
export const RESEARCH_STAGE_RATE_PER_MIN = 80;

/** The four alarms on the stored Tavily key. */
export type TavilyAlarm = "key-age" | "key-refused" | "rate-limited" | "plan-limit";
/** `HAMH_TAVILY_ALARMS`: `on` (the default) builds the four alarms on a stage whose research provider is `tavily`, `off` builds none. */
export const TAVILY_ALARM_WORDS = ["on", "off"] as const;
/** `HAMH_TAVILY_KEY_MAX_AGE_DAYS`: the age of the stored key at which the key-age alarm fires. */
export const DEFAULT_TAVILY_KEY_MAX_AGE_DAYS = 30;

/** The settings of the Tavily alarms, resolved. */
export interface ResearchAlarmSettings {
  enabled: boolean;
  maxKeyAgeDays: number;
  /** The SNS topic that receives the alarm and the return to OK; none means the alarms only change state. */
  topicArn?: string;
}

/**
 * An SNS topic ARN: any partition and Region, a standard or a FIFO topic (CloudWatch publishes alarm notifications to both). The partition is one bounded class, not a
 * repeated group, so the pattern cannot backtrack badly.
 */
const SNS_TOPIC_ARN = /^arn:aws[a-z-]{0,20}:sns:[a-z0-9-]+:[0-9]{12}:[A-Za-z0-9_-]{1,256}(?:\.fifo)?$/;

/**
 * Resolves the three settings of the Tavily key alarms. Each word is the text of an environment variable, read by `lib/entry.ts` (the only place the
 * app reads the environment) and resolved again by `buildApp`, as the tool layer's word is. Unset, empty and blank mean the default. A bad value throws and names
 * its variable; the topic's error does not repeat the topic.
 *  - `HAMH_TAVILY_ALARMS`: `on` (default) or `off`, any letter case.
 *  - `HAMH_TAVILY_KEY_MAX_AGE_DAYS`: digits only, 1 to 365, default 30 (Tavily advises rotating about every 90 days; this stack asks earlier).
 *  - `HAMH_RESEARCH_ALARM_TOPIC_ARN`: an SNS topic ARN. Its shape is checked here; whether the topic exists and lets CloudWatch publish is the deploy's to find.
 */
export function resolveResearchAlarms(raw: { alarms?: string; maxKeyAgeDays?: string; topicArn?: string }): ResearchAlarmSettings {
  const word = trimSetting(raw.alarms ?? "").toLowerCase();
  if (word !== "" && !(TAVILY_ALARM_WORDS as readonly string[]).includes(word)) {
    throw new Error(`HAMH_TAVILY_ALARMS="${raw.alarms}" is not one of ${TAVILY_ALARM_WORDS.join(", ")} (unset or empty means on)`);
  }
  const days = trimSetting(raw.maxKeyAgeDays ?? "");
  const count = /^[0-9]{1,3}$/.test(days) ? Number(days) : Number.NaN;
  if (days !== "" && !(count >= 1 && count <= 365)) {
    throw new Error(`HAMH_TAVILY_KEY_MAX_AGE_DAYS="${raw.maxKeyAgeDays}" is not a whole number of days from 1 to 365 (unset or empty means ${DEFAULT_TAVILY_KEY_MAX_AGE_DAYS})`);
  }
  const topic = trimSetting(raw.topicArn ?? "");
  if (topic !== "" && !SNS_TOPIC_ARN.test(topic)) {
    throw new Error("HAMH_RESEARCH_ALARM_TOPIC_ARN is not an SNS topic ARN (arn:aws:sns:<Region>:<account>:<topic name>)");
  }
  return {
    enabled: word !== "off",
    maxKeyAgeDays: days === "" ? DEFAULT_TAVILY_KEY_MAX_AGE_DAYS : count,
    ...(topic === "" ? {} : { topicArn: topic }),
  };
}

export function checkResearchProvider(provider: ResearchProvider, kind: StageKind): void {
  if (!(RESEARCH_PROVIDERS as readonly string[]).includes(provider)) {
    throw new Error(`research provider "${String(provider)}" is not one of ${RESEARCH_PROVIDERS.join(", ")}`);
  }
  if (provider === "fixture" && kind === "official") {
    throw new Error("research provider fixture is refused on an official stage (the active accounts.json entry): canned answers are for test stages only");
  }
}

/**
 * The stage setting of the tool layer: `HAMH_TOOLS_MODE` becomes the worker runtime's `AGENTD_TOOLS_MODE`, the kill switch of agentd's tool server. The setting
 * is per deploy, like `HAMH_UTILITY_MODEL`, and it defaults to `off`: an optional addition never costs the required path, so the layer is on only where the
 * operator says so. The runtime always carries a concrete `off` or `on`, never a missing variable and never another word: the sibling research module reads its
 * mode leniently (anything but off, and an unset variable, mean on) and the tool server is specified to read like it.
 */
export const TOOLS_MODES = ["off", "on"] as const;
export type ToolsMode = (typeof TOOLS_MODES)[number];
export const DEFAULT_TOOLS_MODE: ToolsMode = "off";

/** Resolves `HAMH_TOOLS_MODE`: unset, empty or blank is the default; otherwise `off` or `on` in any letter case with surrounding ASCII whitespace ignored. Any other word throws and names the setting. */
export function resolveToolsMode(setting: string | undefined): ToolsMode {
  const word = trimSetting(setting ?? "").toLowerCase();
  if (word === "") return DEFAULT_TOOLS_MODE;
  if ((TOOLS_MODES as readonly string[]).includes(word)) return word as ToolsMode;
  throw new Error(`HAMH_TOOLS_MODE="${setting}" is not one of ${TOOLS_MODES.join(", ")} (unset or empty means ${DEFAULT_TOOLS_MODE})`);
}

/**
 * The stage setting of the Instances compute type: `HAMH_COMPUTE_INSTANCES`. Off (the default), the stage has the one microVM worker runtime
 * it always had. On, the runtimes stack also creates a capacity provider (EC2 machines in the stage's own VPC) and a second worker runtime on the same image that runs on it,
 * the security stack creates the provider's operator role, and run-session may start and end sessions on that runtime. Like `HAMH_TOOLS_MODE`, the word is per deploy and
 * trimmed of ASCII whitespace; `scripts/lib/deploy.py` (`resolve_compute_instances`) resolves the same words by the same table.
 */
export const COMPUTE_INSTANCES_MODES = ["off", "on"] as const;
export type ComputeInstancesMode = (typeof COMPUTE_INSTANCES_MODES)[number];
export const DEFAULT_COMPUTE_INSTANCES: ComputeInstancesMode = "off";

/**
 * A test knob: `HAMH_SLICE_DEADLINE_S` shortens the prompt deadline of every slice of a run (whole seconds, 60 to 28,800). Unset, empty or blank leaves the full slice, which is
 * every production deployment. Any other value throws and names the setting, so a typo in a live proof is never read as "no limit".
 */
export function sliceDeadlineS(env: NodeJS.ProcessEnv = process.env): number | undefined {
  const raw = trimSetting(lookupEnv(env, "SLICE_DEADLINE_S") ?? "");
  if (raw === "") return undefined;
  const n = Number(raw);
  if (!/^[0-9]+$/.test(raw) || n < 60 || n > 28_800) {
    throw new Error(`HAMH_SLICE_DEADLINE_S="${raw}" is not whole seconds from 60 to 28800 (unset or empty means the full slice)`);
  }
  return n;
}

/** Resolves `HAMH_COMPUTE_INSTANCES`: unset, empty or blank is `off`; otherwise `off` or `on` in any letter case with surrounding ASCII whitespace ignored. Any other word throws and names the setting. */
export function resolveComputeInstances(setting: string | undefined): ComputeInstancesMode {
  const word = trimSetting(setting ?? "").toLowerCase();
  if (word === "") return DEFAULT_COMPUTE_INSTANCES;
  if ((COMPUTE_INSTANCES_MODES as readonly string[]).includes(word)) return word as ComputeInstancesMode;
  throw new Error(`HAMH_COMPUTE_INSTANCES="${setting}" is not one of ${COMPUTE_INSTANCES_MODES.join(", ")} (unset or empty means ${DEFAULT_COMPUTE_INSTANCES})`);
}

/**
 * The utility model: the small model of the solution's own helper calls outside the engine, the 5 to 7 word run name first
 * (`name-run`, run-session). `crates/ham-core/assets/utility-models.json` describes it, Rust compiles that file in, and this module reads it
 * at synth, so the row's Region, the smoke record that admits it and the IAM grant come from one file. It is no coding model and no row of
 * `engine/catalog/models.json`; the file sits outside `engine/`, so a change to it does not move the worker image key.
 */
export const UTILITY_MODELS_URL = new URL("../../crates/ham-core/assets/utility-models.json", import.meta.url);

/** Where `resolveUtilityModel` reads the catalog and asks whether a smoke record file is beside it (tests pass their own). */
export interface UtilityCatalogSource {
  catalog: unknown;
  hasRecord(file: string): boolean;
}

/**
 * The catalog file as committed, with the record files that sit beside it. Read when called, never when the module loads. A record is looked up by
 * name in the listing of the catalog's own directory, so no caller-supplied path reaches the file system.
 */
export function committedUtilityCatalog(): UtilityCatalogSource {
  let catalog: unknown;
  let files: Set<string>;
  try {
    catalog = JSON.parse(readFileSync(UTILITY_MODELS_URL, "utf8"));
    files = new Set(readdirSync(new URL("./", UTILITY_MODELS_URL), { withFileTypes: true }).filter((d) => d.isFile()).map((d) => d.name));
  } catch (e) {
    throw new Error(`utility-models.json: ${(e as Error).message}`);
  }
  return { catalog, hasRecord: (file) => files.has(file) };
}

/**
 * What the deploy setting means: `value` is the row id the Lambda reads as `UTILITY_MODEL` (or `off`), `resources` the IAM resource ARNs of the
 * one Bedrock statement of run-session (none when off). `fallback` is set when an unset setting turned the feature off because the default row
 * is not smoked in the stage Region; the stack shows it as a synth note.
 */
export interface ResolvedUtilityModel {
  value: string;
  resources: string[];
  fallback?: string;
}

interface UtilityRoute { id: string; region: string; smokeRecordId?: string }
interface UtilityRow extends UtilityRoute { alias: string; baseModelId: string; alternateRoutes: UtilityRoute[]; purposes: unknown }

function utilityRows(catalog: unknown): { rows: UtilityRow[]; defaultName: string } {
  const fail = (why: string): never => { throw new Error(`utility-models.json: ${why}`); };
  const doc = catalog as { schemaVersion?: unknown; default?: unknown; models?: unknown } | null;
  if (typeof doc !== "object" || doc === null) return fail("not an object");
  if (doc.schemaVersion !== 1) return fail(`schemaVersion must be 1, got ${String(doc.schemaVersion)}`);
  if (typeof doc.default !== "string" || !Array.isArray(doc.models) || doc.models.length === 0) return fail("expected a default and a non-empty models list");
  const text = (v: unknown, what: string): string => (typeof v === "string" && v.trim() !== "" ? v : fail(`${what} must be a non-empty string`));
  const route = (r: any, where: string): UtilityRoute => ({
    id: text(r?.id, `${where} id`), region: text(r?.region, `${where} region`),
    ...(typeof r?.smokeRecordId === "string" ? { smokeRecordId: r.smokeRecordId } : {}),
  });
  const rows = (doc.models as any[]).map((m, i): UtilityRow => ({
    ...route(m, `models[${i}]`), alias: text(m?.alias, `models[${i}] alias`), baseModelId: text(m?.baseModelId, `models[${i}] baseModelId`),
    alternateRoutes: Array.isArray(m?.alternateRoutes) ? m.alternateRoutes.map((a: unknown, j: number) => route(a, `models[${i}].alternateRoutes[${j}]`)) : [],
    purposes: m?.purposes,
  }));
  return { rows, defaultName: doc.default.trim() };
}

/**
 * Resolves `HAMH_UTILITY_MODEL` against the utility-model catalog, for a stage in `region`. Unset, empty or blank is the catalog's default row;
 * `off` in any case is no grant; an id (exact) or an alias (any case) names a row. Anything else throws and names the setting: nothing falls through
 * to the default. A row is granted only with a smoke record (root invariant 6): its `smokeRecordId` must name a plain `.json` file that sits beside
 * the catalog, and so must the record of each alternate route. The row and its routes must be in the stage Region. The one exception is the unset
 * setting in another Region: the default row is smoked in its own Region only, an optional feature never stops a stage that cannot run it, so the
 * result is `off` with a `fallback` note. An explicit request in that Region still throws.
 */
export function resolveUtilityModel(setting: string | undefined, region: string, source?: UtilityCatalogSource): ResolvedUtilityModel {
  const want = (setting ?? "").trim();
  if (want.toLowerCase() === "off") return { value: "off", resources: [] };
  const src = source ?? committedUtilityCatalog();
  const { rows, defaultName } = utilityRows(src.catalog);
  const find = (name: string): UtilityRow | undefined => rows.find((m) => m.id === name) ?? rows.find((m) => m.alias === name.toLowerCase());
  const explicit = want !== "";
  const row = find(explicit ? want : defaultName);
  if (row === undefined) {
    if (!explicit) throw new Error(`utility-models.json: default "${defaultName}" names no row`);
    const choices = rows.filter((m) => (m.smokeRecordId ?? "").trim() !== "").map((m) => `${m.alias} = ${m.id}`);
    throw new Error(`HAMH_UTILITY_MODEL="${want}" names no utility model; use off or one of: ${choices.join(", ")}`);
  }
  // A record id ends in the name of a file beside the catalog; the name is checked as a plain .json name before any lookup.
  const admit = (r: UtilityRoute, what: string): void => {
    const id = (r.smokeRecordId ?? "").trim();
    if (id === "") throw new Error(`HAMH_UTILITY_MODEL: ${what} ${r.id} has no passing smoke record, so it is never granted (root invariant 6)`);
    const file = id.slice(id.lastIndexOf("/") + 1);
    if (!/^[A-Za-z0-9._-]+\.json$/.test(file)) throw new Error(`HAMH_UTILITY_MODEL: the smoke record id of ${what} ${r.id} does not end in a plain .json file name`);
    if (!src.hasRecord(file)) throw new Error(`HAMH_UTILITY_MODEL: the smoke record ${file} of ${what} ${r.id} is not beside utility-models.json`);
  };
  admit(row, "utility model");
  if (row.region !== region) {
    const why = `utility model ${row.id} is smoked in ${row.region} and the stage Region is ${region}`;
    if (explicit) throw new Error(`HAMH_UTILITY_MODEL: ${why}`);
    return {
      value: "off", resources: [],
      fallback: `HAMH_UTILITY_MODEL is unset: the default ${why}, so run names are off here; set HAMH_UTILITY_MODEL to a model smoked in this Region to turn them on`,
    };
  }
  const routes = [row, ...row.alternateRoutes];
  for (const alt of row.alternateRoutes) {
    admit(alt, "alternate route");
    if (alt.region !== region) throw new Error(`HAMH_UTILITY_MODEL: alternate route ${alt.id} of ${row.id} is smoked in ${alt.region} and the stage Region is ${region}`);
  }
  // The shape the dispatcher writes for a run's models (crates/dispatcher/src/policy.rs): per route the profile ARN in the stage account and Region,
  // the base model's foundation-model ARN in every inference Region, and for a `global.` profile the Region-less foundation-model ARN.
  const resources: string[] = [];
  const add = (arn: string): void => { if (!resources.includes(arn)) resources.push(arn); };
  for (const r of routes) {
    add(`arn:aws:bedrock:${region}:${ACCOUNT}:inference-profile/${r.id}`);
    for (const inference of INFERENCE_REGIONS) add(`arn:aws:bedrock:${inference}::foundation-model/${row.baseModelId}`);
    if (r.id.startsWith("global.")) add(`arn:aws:bedrock:::foundation-model/${row.baseModelId}`);
  }
  return { value: row.id, resources };
}

/**
 * The purposes of the utility model, in the order of `UtilityPurpose::ALL` (`crates/ham-core/src/utility.rs`; `test/utility-purposes.test.ts` holds the two
 * lists equal). Each purpose has its own prompt and so its own smoke record: the top-level record of a catalog row is the run name's, and the
 * `purposes` object of the row holds the others.
 */
export const UTILITY_PURPOSE_WORDS = ["run-name", "run-tags"] as const;
export type UtilityPurposeWord = (typeof UTILITY_PURPOSE_WORDS)[number];

/**
 * What `HAMH_UTILITY_PURPOSES` means for the stage: `value` is what the run-session function reads as `UTILITY_PURPOSES` (a comma list in catalog order,
 * or `off` when the utility model is off) and `purposes` the same as a list (empty when off).
 */
export interface ResolvedUtilityPurposes {
  value: string;
  purposes: UtilityPurposeWord[];
}

/** The words of the setting, in the order given, or undefined when the setting is unset, empty or blank (every purpose the row has a record for). */
function purposeWords(setting: string | undefined): UtilityPurposeWord[] | undefined {
  const want = (setting ?? "").trim();
  if (want === "") return undefined;
  const given = want.split(",").map((w) => w.trim().toLowerCase());
  const known: readonly string[] = UTILITY_PURPOSE_WORDS;
  const bad = given.filter((w) => !known.includes(w));
  if (bad.length > 0) {
    throw new Error(`HAMH_UTILITY_PURPOSES="${want}" names no purpose (${bad.map((w) => JSON.stringify(w)).join(", ")}); use a comma list of: ${UTILITY_PURPOSE_WORDS.join(", ")} (HAMH_UTILITY_MODEL=off stops them all)`);
  }
  return given as UtilityPurposeWord[];
}

/**
 * Resolves `HAMH_UTILITY_PURPOSES` for the utility `model` that `resolveUtilityModel` returned. Unset, empty or blank means every purpose the
 * model's catalog row holds a smoke record for; otherwise a comma list of purpose words (any case, spaces allowed, a repeat counts once), answered in
 * the catalog's order. A word that is no purpose, an empty element or a bare comma throws and names the setting, also when the model is off, so a typo
 * is never hidden by the other setting. A purpose is served only with a smoke record (root invariant 6): the row's `purposes` entry must carry a record
 * id that ends in a plain `.json` file name beside the catalog; the run name has the row's top-level record, which `resolveUtilityModel` has admitted.
 * With the model off the value is `off`, no purpose, and the catalog is not read.
 */
export function resolveUtilityPurposes(setting: string | undefined, model: ResolvedUtilityModel, source?: UtilityCatalogSource): ResolvedUtilityPurposes {
  const words = purposeWords(setting);
  if (model.value === "off") return { value: "off", purposes: [] };
  const src = source ?? committedUtilityCatalog();
  const row = utilityRows(src.catalog).rows.find((m) => m.id === model.value);
  if (row === undefined) throw new Error(`utility-models.json: the utility model ${model.value} names no row`);
  const records: Record<string, { smokeRecordId?: unknown } | undefined> =
    row.purposes === undefined ? {} : typeof row.purposes === "object" && row.purposes !== null && !Array.isArray(row.purposes)
      ? row.purposes as Record<string, { smokeRecordId?: unknown } | undefined>
      : (() => { throw new Error(`utility-models.json: model ${row.id}: purposes must be an object`); })();
  const known: readonly string[] = UTILITY_PURPOSE_WORDS;
  for (const key of Object.keys(records)) {
    if (!known.includes(key)) throw new Error(`utility-models.json: model ${row.id}: purposes.${key} is not a purpose this build knows (${UTILITY_PURPOSE_WORDS.join(", ")})`);
  }
  const recordOf = (w: UtilityPurposeWord): string => (typeof records[w]?.smokeRecordId === "string" ? (records[w]!.smokeRecordId as string).trim() : "");
  const has = (w: UtilityPurposeWord): boolean => w === "run-name" || recordOf(w) !== "";
  const wanted = UTILITY_PURPOSE_WORDS.filter((w) => (words === undefined ? has(w) : words.includes(w)));
  for (const w of wanted) {
    if (!has(w)) throw new Error(`HAMH_UTILITY_PURPOSES: purpose ${w} has no passing smoke record on utility model ${row.id}, so it is never enabled (root invariant 6)`);
    if (w === "run-name") continue;
    // A record id ends in the name of a file beside the catalog; the name is checked as a plain .json name before any lookup.
    const id = recordOf(w);
    const file = id.slice(id.lastIndexOf("/") + 1);
    if (!/^[A-Za-z0-9._-]+\.json$/.test(file)) throw new Error(`HAMH_UTILITY_PURPOSES: the smoke record id of purpose ${w} on ${row.id} does not end in a plain .json file name`);
    if (!src.hasRecord(file)) throw new Error(`HAMH_UTILITY_PURPOSES: the smoke record ${file} of purpose ${w} on ${row.id} is not beside utility-models.json`);
  }
  return { value: wanted.join(","), purposes: [...wanted] };
}

/**
 * The stage settings of the second limits wave. Each was a constant of the app; each is now an environment variable of the deploy, read once by `lib/entry.ts`, with
 * the old value as the default (unset, empty and blank mean the default). A value outside its range throws and names its variable, so a typo never reads as "no limit".
 *  - `HAMH_LOG_RETENTION_DAYS`: how long the CloudWatch log groups of the stage keep their records; a period CloudWatch Logs takes (default 30).
 *  - `HAMH_PROVIDER_SILENCE_S`: how long Cline may go without output before the harness cancels the prompt, 60 to 3,600 s. Unset leaves agentd's own 900 s; the value goes to
 *    the run-session function as `HAMH_PROVIDER_SILENCE_S`, which puts `providerSilenceS` into every session's invocation.
 *  - `HAMH_PR_CHECK_MINUTES`: the build time of the pull request check, 5 to 480 minutes (default 60); `HAMH_PR_CHECK_COMPUTE`: its ARM machine, one of
 *    small, medium, large, xlarge, 2xlarge (default small).
 *  - `HAMH_INSTANCES_TYPES` (comma list of Graviton types, default `DEFAULT_INSTANCE_TYPES`, m7g.4xlarge and m6g.4xlarge), `HAMH_INSTANCES_ROOT_FREE_GIB` (default 100), `HAMH_INSTANCES_WORKSPACE_GIB` (default 150),
 *    `HAMH_INSTANCES_LIFETIME_S` (28,800 to 1,209,600, default 28,800) and `HAMH_INSTANCES_IDLE_S` (60 to 1,209,600, default 1,800): the machine of the Instances part. They
 *    matter only with `HAMH_COMPUTE_INSTANCES=on`; a changed value is a different capacity provider (`names.capacityProvider`).
 */
export const LOG_RETENTION_DAYS_ALLOWED = [1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192, 2557, 2922, 3288, 3653] as const;
export const DEFAULT_LOG_RETENTION_DAYS = 30;
/** The ARM sizes of `BUILD_GENERAL1_*` the pull request check may use (the CodeBuild guide lists them for the ARM_CONTAINER environment type). */
export const PR_CHECK_COMPUTE_WORDS = ["small", "medium", "large", "xlarge", "2xlarge"] as const;
export type PrCheckCompute = (typeof PR_CHECK_COMPUTE_WORDS)[number];
export const DEFAULT_PR_CHECK_COMPUTE: PrCheckCompute = "small";
/** The pull request check ran 20 minutes, which stopped a repository whose own build takes longer from ever being approved. */
export const DEFAULT_PR_CHECK_MINUTES = 60;
export const MIN_PR_CHECK_MINUTES = 5;
export const MAX_PR_CHECK_MINUTES = 480;
/** The machine lifetime may not drop below a session (27,900 s of prompt and a 900 s reserve) and AgentCore takes 14 days at most. */
export const MIN_INSTANCES_LIFETIME_S = 28_800;
export const MAX_INSTANCES_LIFETIME_S = 1_209_600;
export const MIN_INSTANCES_IDLE_S = 60;
/**
 * The machines the Instances part may run on when `HAMH_INSTANCES_TYPES` is unset. AgentCore picks from the list, so one type that runs out of capacity in the stage's
 * zone does not stop a run (two runs ended blocked when EC2 had no m7g.4xlarge in the one zone of the stage). Both were offered in every zone of us-east-1 that the
 * stage subnets use. The capacity provider takes up to 30 types (the AgentCore API reference, InstanceRequirements.allowedInstanceTypes). A changed list is a
 * different capacity provider, so an update of a stage that kept the old default asks for consent (scripts/lib/deploy.py, C12) and never replaces the machines on its
 * own.
 */
export const DEFAULT_INSTANCE_TYPES = ["m7g.4xlarge", "m6g.4xlarge"] as const;
/** An EBS volume holds 16 TiB at most. */
export const MAX_INSTANCES_DISK_GIB = 16_384;

/** The words of the settings as the deploy was given them. */
export interface StageSettingsRaw {
  logRetentionDays?: string;
  providerSilenceS?: string;
  prCheckMinutes?: string;
  prCheckCompute?: string;
  instancesTypes?: string;
  instancesRootFreeGiB?: string;
  instancesWorkspaceGiB?: string;
  instancesLifetimeS?: string;
  instancesIdleS?: string;
}

/** What the settings come to. `instances` holds only the members a setting changed. */
export interface PrCheckConfig {
  buildMinutes: number;
  compute: PrCheckCompute;
}
export interface StageSettings {
  logRetentionDays: number;
  /** Seconds of silence from Cline after which the prompt is cancelled; absent leaves agentd's own 900. */
  providerSilenceS?: number;
  prCheck: PrCheckConfig;
  instances: Partial<Pick<InstancesConfig, "instanceTypes" | "vcpus" | "rootFreeGiB" | "workspaceGiB" | "maxLifetimeS" | "idleInstanceTimeoutS">>;
}

/** vCPUs by size name. A `metal` size is left out: its count differs by family. */
const SIZE_VCPUS: Readonly<Record<string, number>> = {
  medium: 1, large: 2, xlarge: 4, "2xlarge": 8, "3xlarge": 12, "4xlarge": 16, "6xlarge": 24, "8xlarge": 32, "9xlarge": 36, "12xlarge": 48,
  "16xlarge": 64, "18xlarge": 72, "24xlarge": 96, "32xlarge": 128, "48xlarge": 192,
};
/** A Graviton family has a `g` after its generation (m7g, c7gn, r8g, x2gd, im4gn); the worker image is arm64, so no other family can run it. */
const GRAVITON_TYPE = /^[a-z][a-z0-9]{0,11}g[a-z]{0,2}\.([a-z0-9]{5,8})$/;

/** A setting that is a whole number in `lo` to `hi`: undefined when unset, a refusal that names the variable otherwise. */
function settingNumber(raw: string | undefined, variable: string, lo: number, hi: number, what: string): number | undefined {
  const text = trimSetting(raw ?? "");
  if (text === "") return undefined;
  const n = /^[1-9][0-9]{0,9}$/.test(text) ? Number(text) : Number.NaN;
  if (!(n >= lo && n <= hi)) throw new Error(`${variable}="${raw}" is not ${what} (unset or empty means the default)`);
  return n;
}

function instanceTypesOf(raw: string | undefined): { types: string[]; vcpus: number } | undefined {
  const text = trimSetting(raw ?? "");
  if (text === "") return undefined;
  const refuse = (why: string): never => { throw new Error(`HAMH_INSTANCES_TYPES="${raw}" ${why} (a comma list of Graviton instance types like m7g.4xlarge; the worker image is arm64)`); };
  const types = text.split(",").map((t) => trimSetting(t));
  let smallest = Number.POSITIVE_INFINITY;
  for (const t of types) {
    const m = GRAVITON_TYPE.exec(t);
    const vcpus = m === null ? undefined : SIZE_VCPUS[m[1]];
    if (vcpus === undefined) refuse(`names ${JSON.stringify(t)}, which is no Graviton instance type`);
    smallest = Math.min(smallest, vcpus as number);
  }
  if (new Set(types).size !== types.length) refuse("names a type twice");
  return { types, vcpus: smallest };
}

/** Resolves the settings above; see the comment on `LOG_RETENTION_DAYS_ALLOWED`. */
export function resolveStageSettings(raw: StageSettingsRaw = {}): StageSettings {
  const days = trimSetting(raw.logRetentionDays ?? "");
  const dayCount = /^[1-9][0-9]{0,4}$/.test(days) ? Number(days) : Number.NaN;
  if (days !== "" && !(LOG_RETENTION_DAYS_ALLOWED as readonly number[]).includes(dayCount)) {
    throw new Error(`HAMH_LOG_RETENTION_DAYS="${raw.logRetentionDays}" is not a number of days CloudWatch Logs keeps: ${LOG_RETENTION_DAYS_ALLOWED.join(", ")} (unset or empty means ${DEFAULT_LOG_RETENTION_DAYS})`);
  }
  const word = trimSetting(raw.prCheckCompute ?? "").toLowerCase();
  if (word !== "" && !(PR_CHECK_COMPUTE_WORDS as readonly string[]).includes(word)) {
    throw new Error(`HAMH_PR_CHECK_COMPUTE="${raw.prCheckCompute}" is not one of ${PR_CHECK_COMPUTE_WORDS.join(", ")} (unset or empty means ${DEFAULT_PR_CHECK_COMPUTE})`);
  }
  const minutes = settingNumber(raw.prCheckMinutes, "HAMH_PR_CHECK_MINUTES", MIN_PR_CHECK_MINUTES, MAX_PR_CHECK_MINUTES,
    `a whole number of minutes from ${MIN_PR_CHECK_MINUTES} to ${MAX_PR_CHECK_MINUTES}`);
  const silence = settingNumber(raw.providerSilenceS, "HAMH_PROVIDER_SILENCE_S", 60, 3_600, "whole seconds from 60 to 3600");
  const types = instanceTypesOf(raw.instancesTypes);
  const root = settingNumber(raw.instancesRootFreeGiB, "HAMH_INSTANCES_ROOT_FREE_GIB", 1, MAX_INSTANCES_DISK_GIB, `a whole number of GiB from 1 to ${MAX_INSTANCES_DISK_GIB}`);
  const workspace = settingNumber(raw.instancesWorkspaceGiB, "HAMH_INSTANCES_WORKSPACE_GIB", 1, MAX_INSTANCES_DISK_GIB, `a whole number of GiB from 1 to ${MAX_INSTANCES_DISK_GIB}`);
  const lifetime = settingNumber(raw.instancesLifetimeS, "HAMH_INSTANCES_LIFETIME_S", MIN_INSTANCES_LIFETIME_S, MAX_INSTANCES_LIFETIME_S,
    `whole seconds from ${MIN_INSTANCES_LIFETIME_S} (a session needs that) to ${MAX_INSTANCES_LIFETIME_S} (14 days, the most AgentCore takes)`);
  const idle = settingNumber(raw.instancesIdleS, "HAMH_INSTANCES_IDLE_S", MIN_INSTANCES_IDLE_S, MAX_INSTANCES_LIFETIME_S, `whole seconds from ${MIN_INSTANCES_IDLE_S} to ${MAX_INSTANCES_LIFETIME_S}`);
  // An idle machine may not outlive the machine: the service takes both ranges, but a pair where the idle time passes the lifetime is no setting anybody means.
  if ((idle ?? 1_800) > (lifetime ?? 28_800)) {
    throw new Error(`HAMH_INSTANCES_IDLE_S=${idle ?? 1_800} is longer than the machine lifetime ${lifetime ?? 28_800} s (HAMH_INSTANCES_LIFETIME_S): raise the lifetime or lower the idle time`);
  }
  return {
    logRetentionDays: days === "" ? DEFAULT_LOG_RETENTION_DAYS : dayCount,
    ...(silence === undefined ? {} : { providerSilenceS: silence }),
    prCheck: { buildMinutes: minutes ?? DEFAULT_PR_CHECK_MINUTES, compute: word === "" ? DEFAULT_PR_CHECK_COMPUTE : (word as PrCheckCompute) },
    instances: {
      ...(types === undefined ? {} : { instanceTypes: types.types, vcpus: types.vcpus }),
      ...(root === undefined ? {} : { rootFreeGiB: root }),
      ...(workspace === undefined ? {} : { workspaceGiB: workspace }),
      ...(lifetime === undefined ? {} : { maxLifetimeS: lifetime }),
      ...(idle === undefined ? {} : { idleInstanceTimeoutS: idle }),
    },
  };
}

/** The worker runtime's session lifecycle. */
export interface WorkerLifecycle {
  /** `maxLifetime`: AgentCore ends the session after this many seconds. */
  maxLifetimeS: number;
  /** `idleRuntimeSessionTimeout`, at most `maxLifetimeS`. */
  idleTimeoutS: number;
}

/**
 * The machine of the Instances part, used only when `HAMH_COMPUTE_INSTANCES=on`. `maxLifetimeS` is a run's longest life (the same 28,800 s as the microVM runtime: a runtime's own `maxLifetime` recycles its
 * container, which loses its processes and /tmp but keeps the volume); `idleInstanceTimeoutS` is how long an unused machine lives before AgentCore retires it (the runtime's idle timeout may not exceed it).
 */
export interface InstancesConfig {
  instanceTypes: readonly string[];
  /** The vCPU count of the machine (the smallest type in `instanceTypes`); the Instances runtime gives cargo one job per vCPU (`AGENTD_DIRECT_CARGO_JOBS`). Not a provider property. */
  vcpus: number;
  rootFreeGiB: number;
  workspaceGiB: number;
  volumeName: string;
  mountPath: string;
  maxLifetimeS: number;
  idleInstanceTimeoutS: number;
}

/**
 * What an Instances capacity provider is made of: the properties of `AWS::BedrockAgentCore::CapacityProvider` that CloudFormation marks "Update requires:
 * Replacement" (`Name`, `ComputeConfiguration`, `PermissionsConfiguration`; only `Description` and `Tags` update in place) and that the stage's configuration decides. A
 * capacity provider's name is unique in the account and nothing but its description can be edited (the AgentCore guide: "duplicate the capacity provider"), so a custom
 * name that stays the same across a replacement cannot be deployed. The runtimes stack therefore builds the provider from this object, and the provider's name ends in a
 * hash of it (`names.capacityProvider`): a changed configuration is a different name, which CloudFormation creates beside the old provider, moves the runtime to and then
 * deletes. The subnets and the security group of the VPC configuration are tokens of the network stack and are left out; `azs` stands for the subnets (one private
 * subnet per Availability Zone), so a new zone is a new provider as well.
 */
export interface InstancesProviderShape {
  operatingSystem: "LINUX_ARM64";
  allowedInstanceTypes: string[];
  monitoring: "BASIC";
  /** The tags AgentCore puts on the machines, volumes and network interfaces it launches (inside the compute configuration, so a change replaces the provider). */
  propagatedTags: Record<string, string>;
  rootVolume: { volumeType: "gp3"; encrypted: true; freeSpaceGiB: number };
  volumes: { name: string; sizeGiB: number; volumeType: "gp3"; iops: number; throughput: number; encrypted: true }[];
  lifecycle: { maxLifetime: number; idleInstanceTimeout: number };
  /** The name of the role AgentCore assumes to launch and clean up the machines (security stack, `names.role(cfg, "instances-operator")`). */
  operatorRole: string;
  azs: string[];
}

export function instancesProviderShape(cfg: StageConfig): InstancesProviderShape {
  const i = cfg.instances;
  return {
    operatingSystem: "LINUX_ARM64",
    allowedInstanceTypes: [...i.instanceTypes],
    monitoring: "BASIC",
    propagatedTags: { project: PROJECT, stage: cfg.stage, owner: OWNER, "managed-by": "cdk", ...KEEP_TAGS },
    rootVolume: { volumeType: "gp3", encrypted: true, freeSpaceGiB: i.rootFreeGiB },
    volumes: [{ name: i.volumeName, sizeGiB: i.workspaceGiB, volumeType: "gp3", iops: 3000, throughput: 125, encrypted: true }],
    lifecycle: { maxLifetime: i.maxLifetimeS, idleInstanceTimeout: i.idleInstanceTimeoutS },
    operatorRole: names.role(cfg, "instances-operator"),
    azs: [...cfg.azs],
  };
}

/** JSON with the keys of every object in sorted order, so a hash does not depend on the order a property was written in. */
function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  if (value !== null && typeof value === "object") {
    return `{${Object.entries(value as Record<string, unknown>).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([k, v]) => `${JSON.stringify(k)}:${stableJson(v)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

export interface StageConfig {
  stage: Stage;
  /** `hamh-<stage>`, the prefix of every stack and named resource. */
  prefix: string;
  /** The selected accounts.json entry's AZ names for the stage (direct 1). */
  azs: string[];
  /** VPC CIDR; /16 split into per-AZ private /20s, public /24s and firewall /28s. */
  vpcCidr: string;
  logRetentionDays: number;
  /** `HAMH_PROVIDER_SILENCE_S`: the run-session function's silence limit for Cline; absent leaves agentd's own 900 s. */
  providerSilenceS?: number;
  /** The pull request check (delivery stack): its build time and ARM machine; `HAMH_PR_CHECK_MINUTES` and `HAMH_PR_CHECK_COMPUTE`. */
  prCheck: PrCheckConfig;
  worker: WorkerLifecycle;
  /** The machine and disks of the Instances part; read only with `HAMH_COMPUTE_INSTANCES=on`. */
  instances: InstancesConfig;
  /** How a new session picks its engine: `run` (the run's engine, `RUN_ENGINE`) on the direct stage. control-api's `ENGINE_ROUTING`. */
  engineRouting: "run" | "policy";
  /** control-api's `ROUTE_LADDER` switch: `off` on the direct stage (no ladder is published or pinned). */
  routeLadder: "off" | "on";
}

export function stageConfig(stage: Stage, settings: StageSettings = resolveStageSettings()): StageConfig {
  switch (stage) {
    case "direct":
      // Direct run: its own network (10.50.0.0/16), one AZ, the full 8 h microVM lifetime with a 30 minute
      // idle timeout, one engine route (`run`), no ladder, and no CloudTrail trail of its own: CloudTrail allows five trails per Region and
      // counts the shadow of every multi-Region trail, so an account that holds five refuses a sixth.
      return {
        stage, prefix: "hamh-direct", azs: [...TARGET.azs.direct], vpcCidr: "10.50.0.0/16", logRetentionDays: settings.logRetentionDays,
        prCheck: { ...settings.prCheck },
        ...(settings.providerSilenceS === undefined ? {} : { providerSilenceS: settings.providerSilenceS }),
        worker: { maxLifetimeS: 28_800, idleTimeoutS: 1_800 },
        instances: {
          instanceTypes: [...DEFAULT_INSTANCE_TYPES], vcpus: 16, rootFreeGiB: 100, workspaceGiB: 150, volumeName: "workspace", mountPath: "/mnt/workspace",
          maxLifetimeS: 28_800, idleInstanceTimeoutS: 1_800,
          ...settings.instances,
        },
        engineRouting: "run", routeLadder: "off",
      };
    default:
      throw new Error(`unknown stage ${String(stage)}: expected direct`);
  }
}

/** Names shared across stacks by convention, so policies need no reference back to their resources. */
export const names = {
  bucket: (cfg: StageConfig) => `${cfg.prefix}-harness-${ACCOUNT}`,
  table: (cfg: StageConfig) => `${cfg.prefix}-harness`,
  role: (cfg: StageConfig, role: string) => `${cfg.prefix}-${role}`,
  bedrockInvocationLogGroup: (cfg: StageConfig) => `/hamh/${cfg.stage}/bedrock-invocations`,
  stateMachine: (cfg: StageConfig, name: string) => `${cfg.prefix}-${name}`,
  /** SSM parameters the runtimes stack writes and run-session reads when a session starts. */
  runtimeArnParameter: (cfg: StageConfig, runtime: string) => `/hamh/${cfg.stage}/${runtime}-runtime-arn`,
  /**
   * The Instances part: the capacity provider (its name ends in a hash of what replaces it, `instancesProviderShape`) and the second worker runtime (underscore form, as AgentCore names are: `[a-zA-Z][a-zA-Z0-9_]{0,47}`; the log groups of the
   * runtime start with the stage prefix in this form, which the runtime base role's log grant matches), the SSM parameter that holds the provider's id (run-session reads it to end a
   * session: the runtime ARN parameter is `runtimeArnParameter(cfg, "worker-python-instances")`) and the provider's operator role is `role(cfg, "instances-operator")`.
   */
  capacityProvider: (cfg: StageConfig) =>
    `${cfg.prefix.replace(/-/g, "_")}_instances_${createHash("sha256").update(stableJson(instancesProviderShape(cfg))).digest("hex").slice(0, 8)}`,
  instancesRuntime: (cfg: StageConfig) => `${cfg.prefix.replace(/-/g, "_")}_harness_worker_python_instances`,
  capacityProviderIdParameter: (cfg: StageConfig) => `/hamh/${cfg.stage}/instances-capacity-provider-id`,
  /** The CloudWatch namespace of control-api's metrics (its `METRICS_NAMESPACE`). */
  metricsNamespace: (cfg: StageConfig) => `HAMH/${cfg.stage}`,
  /** The Go module store bucket, the GOPROXY URL workers use for it and its operator role. */
  gomodBucket: (cfg: StageConfig) => `${cfg.prefix}-gomod-${ACCOUNT}`,
  gomodProxyUrl: (cfg: StageConfig) => `https://${cfg.prefix}-gomod-${ACCOUNT}.s3.${REGION}.amazonaws.com`,
  gomodFillerRole: (cfg: StageConfig) => `${cfg.prefix}-gomod-filler`,
  /** The engine-policy signing key's alias (security stack; control-api's `ROUTE_LADDER_KEY`). */
  enginePolicyKeyAlias: (cfg: StageConfig) => `alias/${cfg.prefix}-engine-policy`,
  /** The research route's table and the secret that holds the Tavily
   * key: created empty by the stack, filled by `scripts/research-key-put.py`, which pins this exact name. */
  researchTable: (cfg: StageConfig) => `${cfg.prefix}-research`,
  researchSecret: (cfg: StageConfig) => `${cfg.prefix}-research-tavily-api-key`,
  /** The AgentCore Gateway of the `agentcore` search provider. Lower case: the gateway id is `<name>-<10 characters>`. */
  researchGateway: (cfg: StageConfig) => `${cfg.prefix}-websearch`,
  /** The CloudWatch alarms on the stored Tavily key: `key-age`, `key-refused`, `rate-limited` and `plan-limit`. */
  researchAlarm: (cfg: StageConfig, which: TavilyAlarm) => `${cfg.prefix}-research-tavily-${which}`,
};
