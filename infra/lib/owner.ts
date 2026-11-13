// The `owner` tag of a stage. Every taggable resource of the stage carries `owner=<value>`, the repository gateway puts the same value on the CodeCommit
// repositories it creates (the delivery stack's IAM condition requires it), and the worker sessions carry it as `tenant`. The value is a label of the deployed
// stage, not a credential and not the identity that deploys. Pure rules, no read of the accounts file: lib/config.ts parses the field and the stacks ask for
// the value. scripts/lib/aws_target.py applies the same rules to the same words.
//
// The value, in order:
//   1. the environment variable `HAMH_OWNER`, when it is set and not empty (one run of a script or of the CDK app; scripts/node.sh passes it into the container);
//   2. the entry's own `"owner": "<value>"` (1 to 64 letters, digits or the characters + = . @ _ -, the tag-value characters an IAM name may hold);
//   3. the name of the entry's `operator` (lib/operator.ts), which the file that `setup.sh` writes for a person always carries;
//   4. `hamh-user`.
// A stage that exists keeps its owner: a changed `owner` tag is a changed tag on every resource, and CloudFormation replaces the DNS query logging configuration of the
// network stack when its tags change, which fails. scripts/lib/deploy.py `settle_owner` reads the owner tag of the stage and sets this variable to it before any cdk call.

import { resolveEnv } from "./env.ts";
import type { Operator } from "./operator.ts";

/** The owner of a stage whose account file names neither an `owner` nor an `operator`. */
export const DEFAULT_OWNER = "hamh-user";

// A literal class and a bounded repeat: one split, no backtracking.
const OWNER_RULE = /^[A-Za-z0-9+=.@_-]{1,64}$/;

/** The owner an entry names, or a refusal that starts with `where` (the place in the file). */
export function parseOwner(raw: unknown, where: string): string {
  if (typeof raw !== "string" || !OWNER_RULE.test(raw)) {
    throw new Error(`${where}: owner must be 1 to 64 letters, digits or + = . @ _ - (a tag value: no path, no pattern, no ARN)`);
  }
  return raw;
}

/** The environment variable that sets the owner of one run of the CDK app or of a script, ahead of the account file. */
export const OWNER_ENV = "HAMH_OWNER";

/** The value of the `owner` tag of the stage of this entry; `env` is the environment of the run (`HAMH_OWNER`, else its earlier name, wins when set and not empty; two different values are a refusal, so is an invalid one). */
export function ownerOf(target: { owner?: string; operator?: Operator }, env: Record<string, string | undefined> = {}): string {
  const fromEnv = resolveEnv(env, "OWNER");
  if (fromEnv.value !== undefined) return parseOwner(fromEnv.value, fromEnv.name);
  return target.owner ?? target.operator?.name ?? DEFAULT_OWNER;
}
