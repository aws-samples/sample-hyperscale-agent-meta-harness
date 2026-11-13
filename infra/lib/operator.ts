// The operator identity of an accounts.json entry: the optional field `operator` names the one IAM role (an IAM Identity Center role included) or IAM
// user that runs setup and later operates the stage. Pure rules, no read of the accounts file: lib/config.ts parses the field and the stacks ask these functions
// for the patterns they write. scripts/lib/aws_target.py applies the same rules to the same words (infra/test/fixtures/operator-identity.json is their one table).
//
//   "operator": {"kind": "role", "name": "<role name>"}   the name as it appears in the assumed-role ARN of the caller (arn:aws:sts::<account>:assumed-role/<name>/<session>);
//                                                          an IAM Identity Center role is named AWSReservedSSO_<permission set>_<16 hex>; no path
//   "operator": {"kind": "user", "name": "<user name>"}   the IAM user's name; no path
//
// A name has 1 to 64 letters, digits or the characters + = . @ _ - (no path, no wildcard, no comma: control-api's OPERATOR_PRINCIPALS setting is comma separated).
// An entry without the field keeps the IAM role `Admin` the stage used before the field existed.

export type OperatorKind = "role" | "user";
export interface Operator {
  kind: OperatorKind;
  name: string;
}

/**
 * The role of the account-local runner that `setup.sh` installs (infra/setup-runner.yaml, `RoleName`). The runner deploys the stage and fills the Go module store
 * with its own role, so a stage generated for a person names it next to that person where the deploying principal acts (the Go module store); nowhere else.
 */
export const SETUP_RUNNER_ROLE = "hamh-setup-runner-role";

// A literal class and a bounded repeat: one split, no backtracking.
const NAME_RULE = /^[A-Za-z0-9+=.@_-]{1,64}$/;

/** The operator an entry names, or a refusal that starts with `where` (the place in the file). */
export function parseOperator(raw: unknown, where: string): Operator {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error(where + ': operator must be an object with kind (role or user) and name (an IAM name), like {"kind": "role", "name": "Admin"}');
  }
  const o = raw as Record<string, unknown>;
  if (o.kind !== "role" && o.kind !== "user") throw new Error(`${where}: operator.kind must be role or user`);
  if (typeof o.name !== "string" || !NAME_RULE.test(o.name)) {
    throw new Error(`${where}: operator.name must be 1 to 64 letters, digits or + = . @ _ - (a name, not a path, a pattern or an ARN)`);
  }
  if (Object.keys(o).length !== 2) throw new Error(`${where}: operator has the keys kind and name only`);
  return { kind: o.kind, name: o.name };
}

/** `role/Admin` or `user/alice`: the identity as one word for messages. */
export function operatorLabel(op: Operator): string {
  return `${op.kind}/${op.name}`;
}

/**
 * The patterns of the caller ARN that API Gateway hands control-api (`requestContext.identity.userArn`) for this identity: the session ARN of a role, the user ARN
 * of a user (with and without a path). They are control-api's OPERATOR_PRINCIPALS (`*` matches any run of characters).
 */
export function callerArnPatterns(account: string, op: Operator): string[] {
  if (op.kind === "role") return [`arn:aws:sts::${account}:assumed-role/${op.name}/*`];
  return [`arn:aws:iam::${account}:user/${op.name}`, `arn:aws:iam::${account}:user/*/${op.name}`];
}

/**
 * The values of `aws:PrincipalArn` that match this identity in a trust or bucket policy: for a role the role's ARN, which has the role's path (an IAM Identity Center
 * role lives under /aws-reserved/sso.amazonaws.com/) and so is written without and with any path; for a user the user's ARN in the same two forms. A role
 * name is unique in an account whatever its path, so the two patterns name one role.
 */
export function principalArnPatterns(account: string, op: Operator): string[] {
  return [`arn:aws:iam::${account}:${op.kind}/${op.name}`, `arn:aws:iam::${account}:${op.kind}/*/${op.name}`];
}

/** The ARN of the setup runner's role in the account (no path: infra/setup-runner.yaml creates it without one). */
export function runnerRoleArn(account: string): string {
  return `arn:aws:iam::${account}:role/${SETUP_RUNNER_ROLE}`;
}

/**
 * Whether the caller ARN of `sts get-caller-identity` may deploy the stage of an entry that names `op`: that identity, or the setup runner of the account. A federated
 * user, the root user, another role, a user of another name and another account are not. The check is by identity, not by pattern: no wildcard in either side.
 */
export function operatorMayDeploy(account: string, op: Operator, callerArn: string): boolean {
  const sts = `arn:aws:sts::${account}:assumed-role/`;
  const iam = `arn:aws:iam::${account}:user/`;
  if (callerArn.startsWith(sts)) {
    const [role, session, ...rest] = callerArn.slice(sts.length).split("/");
    if (!role || !session || rest.length > 0) return false;
    return role === SETUP_RUNNER_ROLE || (op.kind === "role" && role === op.name);
  }
  if (callerArn.startsWith(iam)) {
    const parts = callerArn.slice(iam.length).split("/");
    return op.kind === "user" && parts.every((p) => p !== "") && parts[parts.length - 1] === op.name;
  }
  return false;
}
