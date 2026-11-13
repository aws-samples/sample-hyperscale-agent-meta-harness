// The repository-name rule of a stage: which CodeCommit repository names a run may open a pull request in (and an operator may allow). The rule is a list of 1 to 10
// entries. An entry is a repository name (1 to 100 letters, digits, `.`, `_` or `-`), such a prefix of up to 99 characters followed by one `*`, or `*` alone. A name
// is covered when it equals an exact entry, starts with the prefix of a prefix entry, or an entry is `*`; names are case sensitive. Pure rules, no read of the
// accounts file: lib/config.ts parses the field and the stacks hand the list to the control-api and delivery functions as `HAMH_REPO_NAMES`, which their Rust code
// reads by the same rules. scripts/lib/aws_target.py applies the same rules to the same words (infra/test/fixtures/repo-names.json is their one table).
//
// The rule, in order:
//   1. the environment variable `HAMH_REPO_NAMES` (comma separated, spaces around an entry are dropped), when it is set and not empty (one run of a script or of the CDK app;
//      scripts/node.sh passes it into the container);
//   2. the entry's own `"repoNames": [...]`;
//   3. the default `["hamh-e2e-*"]`, the rule the product had before it was a setting.

import { resolveEnv } from "./env.ts";

/** The rule of a stage whose account file and environment name none. */
export const DEFAULT_REPO_NAMES: readonly string[] = ["hamh-e2e-*"];

/** The environment variable that sets the rule of one run of the CDK app or of a script, ahead of the account file (lib/env.ts also reads its earlier name). */
export const REPO_NAMES_ENV = "HAMH_REPO_NAMES";

const MAX_ENTRIES = 10;
// Literal classes and bounded repeats: one split, no backtracking.
const ENTRY_RULE = /^(?:[A-Za-z0-9._-]{1,100}|[A-Za-z0-9._-]{1,99}\*|\*)$/;

/** The rule an entry (or `HAMH_REPO_NAMES`) names, or a refusal that starts with `where` (the place in the file). */
export function parseRepoNames(raw: unknown, where: string): string[] {
  if (!Array.isArray(raw) || raw.length < 1 || raw.length > MAX_ENTRIES) {
    throw new Error(`${where}: repoNames must be a list of 1 to 10 repository names, like ["my-app", "team-*"]`);
  }
  for (const e of raw) {
    if (typeof e !== "string" || !ENTRY_RULE.test(e)) {
      throw new Error(`${where}: repoNames entry ${JSON.stringify(e)} must be a repository name (1 to 100 letters, digits, '.', '_' or '-'), such a prefix followed by '*', or '*'`);
    }
  }
  raw.forEach((e, i) => {
    if (raw.indexOf(e) !== i) throw new Error(`${where}: repoNames lists ${JSON.stringify(e)} twice`);
  });
  return [...raw];
}

/** The rule of the stage of this entry; `env` is the environment of the run (`HAMH_REPO_NAMES` wins when set and not empty, else a refusal when invalid). */
export function repoNamesOf(target: { repoNames?: string[] }, env: Record<string, string | undefined> = {}): string[] {
  const fromEnv = resolveEnv(env, "REPO_NAMES");
  if (fromEnv.value !== undefined) return parseRepoNames(fromEnv.value.split(",").map((e) => e.trim()), fromEnv.name);
  return target.repoNames ? [...target.repoNames] : [...DEFAULT_REPO_NAMES];
}

/** Whether the rule covers a repository name: an exact entry equals it, a prefix entry (`team-*`) starts it, `*` covers every name. */
export function repoNameAllowed(rule: readonly string[], name: string): boolean {
  return rule.some((e) => (e.endsWith("*") ? name.startsWith(e.slice(0, -1)) : name === e));
}
