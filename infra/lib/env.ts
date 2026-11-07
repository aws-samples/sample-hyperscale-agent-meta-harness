// COMPAT-CAH: the one reader of the environment variables of the CDK app.
//
// Each variable that has an earlier name has two spellings during the compatibility window: the current `HAMH_<X>` and the earlier `CAH_<X>`.
//   - the current name wins; the earlier name answers only when the current one is not set;
//   - the earlier name alone, or both names with one value, prints one notice per name per process on stderr:
//       CAH_X is the old name of HAMH_X. It still works and will stop working in a later release.
//   - both names with different values: the current name is used and one notice says so (`Both CAH_X and HAMH_X are set. HAMH_X is used.`), except for a selector;
//   - a selector (the variables that choose an account, an accounts file or an owner) with two different values is an error that names both variables. A selector is
//     never ignored and never falls through to the default account: this is the one place where the app refuses instead of choosing;
//   - `HAMH_NO_DEPRECATION_NOTICES=1` silences every notice, and nothing else.
// The table below holds the variables the app reads and the rule of each. A name that is not in the table cannot be read, so the next variable joins the table before
// it joins the code. `emptyIsUnset` is the variable's own meaning of an empty value: for all but the worker revision an empty value is "not set" and the other name
// can answer; the worker revision keeps its refusal of an empty text.

export type Env = Record<string, string | undefined>;

/** The prefix of a variable now, and the prefix it had before the rename. */
export const CURRENT_PREFIX = "HAMH_";
export const EARLIER_PREFIX = "CAH_";

/** The variable that silences the notices (a notice is never an error, and this switch never touches an error). */
export const NO_NOTICES_VARIABLE = "HAMH_NO_DEPRECATION_NOTICES";

export interface VariableRule {
  /** The variable's empty value means "not set". */
  emptyIsUnset: boolean;
  /** The variable chooses an account, an accounts file or an owner: two different values are an error. */
  selector: boolean;
  /** The earlier name still answers during the window; false: renamed with no fallback (Tier 2). */
  earlier: boolean;
}

const setting: VariableRule = { emptyIsUnset: true, selector: false, earlier: true };
const selector: VariableRule = { emptyIsUnset: true, selector: true, earlier: true };
const deployInput: VariableRule = { emptyIsUnset: true, selector: false, earlier: false };

/** Every variable the app reads, by the part after the prefix. lib/entry.ts reads twelve, lib/config.ts three, lib/repo-names.ts one, lib/stacks/network.ts one. */
export const VARIABLES: Readonly<Record<string, VariableRule>> = {
  // Where the app builds: the account, the accounts file and the owner tag.
  AWS_PROFILE: selector,
  ACCOUNTS_FILE: selector,
  OWNER: selector,
  REPO_NAMES: setting,
  // The inputs of one deploy (scripts/deploy.sh and scripts/lib/deploy.py set them).
  LAMBDA_DIR: deployInput,
  WORKER_IMAGE: deployInput,
  DELIVERY_IMAGE: deployInput,
  WORKER_REVISION: { emptyIsUnset: false, selector: false, earlier: true },
  // The optional settings of a stage.
  RESEARCH_PROVIDER: setting,
  UTILITY_MODEL: setting,
  UTILITY_PURPOSES: setting,
  TOOLS_MODE: setting,
  TAVILY_ALARMS: setting,
  TAVILY_KEY_MAX_AGE_DAYS: setting,
  RESEARCH_ALARM_TOPIC_ARN: setting,
  COMPUTE_INSTANCES: setting,
  SLICE_DEADLINE_S: setting,
  AGENTCORE_STORAGE_ACCOUNTS: setting,
};

/** What a read answers: the value (absent when no name is set) and the name of the variable that answered (the current name when none is set). */
export interface Resolved {
  value: string | undefined;
  name: string;
}

/** Writes each notice once per name. The default writes to stderr; a test passes its own writer. */
export class Notices {
  private readonly seen = new Set<string>();

  constructor(private readonly write: (line: string) => void) {}

  /** Writes `line` unless the same `key` was written before. */
  once(key: string, line: string): void {
    if (this.seen.has(key)) return;
    this.seen.add(key);
    this.write(line);
  }

  /** Forgets what was written, so the next read of a name writes its notice again (a test starts from here). */
  reset(): void {
    this.seen.clear();
  }
}

/** The notices of this process, on stderr only: stdout belongs to the CDK CLI. */
export const processNotices = new Notices((line) => { process.stderr.write(`${line}\n`); });

/** The answer for `suffix` (`OWNER` for `HAMH_OWNER` and `CAH_OWNER`) in `env`; a selector with two different values throws. */
export function resolveEnv(env: Env, suffix: string, notices: Notices = processNotices): Resolved {
  const rule = Object.hasOwn(VARIABLES, suffix) ? VARIABLES[suffix] : undefined;
  if (rule === undefined) throw new Error(`env: ${suffix} is not a variable of the app; add it to VARIABLES in lib/env.ts and to compat/env-names.json`);
  const current = `${CURRENT_PREFIX}${suffix}`;
  const earlier = `${EARLIER_PREFIX}${suffix}`;
  const read = (name: string): string | undefined => {
    const v = env[name];
    return v === undefined || (rule.emptyIsUnset && v === "") ? undefined : v;
  };
  const now = read(current);
  const before = rule.earlier ? read(earlier) : undefined;
  const quiet = env[NO_NOTICES_VARIABLE] === "1";
  if (!rule.earlier || before === undefined) return { value: now, name: current };
  if (now === undefined) {
    if (!quiet) notices.once(earlier, `${earlier} is the old name of ${current}. It still works and will stop working in a later release.`);
    return { value: before, name: earlier };
  }
  if (now === before) {
    if (!quiet) notices.once(earlier, `${earlier} is the old name of ${current}. It still works and will stop working in a later release.`);
    return { value: now, name: current };
  }
  if (rule.selector) {
    throw new Error(`${current} and ${earlier} are both set to different values (${JSON.stringify(now)} and ${JSON.stringify(before)}). ${earlier} is the old name of ${current}: unset ${earlier}, or give both the same value.`);
  }
  if (!quiet) notices.once(`${earlier}+${current}`, `Both ${earlier} and ${current} are set. ${current} is used.`);
  return { value: now, name: current };
}

/** The value for `suffix`, or undefined when no name is set. */
export function lookupEnv(env: Env, suffix: string, notices: Notices = processNotices): string | undefined {
  return resolveEnv(env, suffix, notices).value;
}
