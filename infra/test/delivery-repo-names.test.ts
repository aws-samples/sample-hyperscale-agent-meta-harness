import { readFileSync } from "node:fs";
import { describe, expect, test, vi } from "vitest";
import { ACCOUNT, REGION } from "../lib/config.ts";
import { grantedRepositoryNames } from "../lib/stacks/delivery.ts";
import { withEnv } from "./accounts-three.ts";
import { repositoryArns } from "./earlier-names.ts";

// The repository-name rule of a stage (`repoNames` of the account-file entry, HAMH_REPO_NAMES, `setup.sh --repo-name`; lib/repo-names.ts) decides which CodeCommit repositories
// the control-api and delivery functions accept. The functions reach a repository through IAM, so the grants of the delivery stack follow the rule: repo-gateway reads, pushes to
// and opens pull requests in the repositories the rule covers, the PR-check build role reads them and approves there, and the EventBridge rule that starts the check fires for
// their pull requests. One table, test/fixtures/repo-names.json (its `cover` rows), says which names a rule covers; this file checks that the synthesized grants cover the same names.
// The repositories `--new-repo` makes are covered whatever the rule says (crates/delivery/src/checks.rs), and a rule that lists the default entry also covers the repositories of the
// earlier prefix (COMPAT-CAH-DATA, test/earlier-names.ts).
const TABLE = JSON.parse(readFileSync(new URL("./fixtures/repo-names.json", import.meta.url), "utf8")) as {
  cover: { why: string; rule: string[]; allows: string[]; refuses: string[] }[];
};

const arn = (name: string) => `arn:aws:codecommit:${REGION}:${ACCOUNT}:${name}`;
const list = <T>(v: T | T[] | undefined): T[] => (v === undefined ? [] : Array.isArray(v) ? v : [v]);
/** IAM's match of a Resource pattern: `*` is any run of characters, `?` one character; case sensitive. */
const iamMatch = (pattern: string, value: string): boolean =>
  new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".")}$`).test(value);

type Stmt = { Sid?: string; Effect: string; Action: string | string[]; Resource: string | string[]; Condition?: Record<string, unknown> };
type Json = { Resources: Record<string, { Type: string; Properties: any }> };

/**
 * The delivery template of the app built in a fresh module graph, with the rule set the way an operator sets it for one run. The graph and the environment are shared
 * state, so the builds run one after the other, and a rule is built once.
 */
let queue: Promise<unknown> = Promise.resolve();
const memo = new Map<string, Promise<Json>>();
function deliveryUnder(rule: string[] | undefined): Promise<Json> {
  const key = JSON.stringify(rule ?? null);
  if (!memo.has(key)) {
    const built = queue.then(() => withEnv({ HAMH_REPO_NAMES: rule?.join(","), CAH_REPO_NAMES: undefined, HAMH_NO_DEPRECATION_NOTICES: "1" }, async () => { // COMPAT-CAH-DATA
      vi.resetModules();
      const { buildApp } = await import("../lib/app.ts");
      const { Template } = await import("aws-cdk-lib/assertions");
      const stack = buildApp({ stage: "direct" }).node.children.find((c) => (c as { stackName?: string }).stackName === "hamh-direct-delivery");
      return Template.fromStack(stack as any).toJSON() as Json;
    }));
    queue = built.catch(() => undefined);
    memo.set(key, built);
  }
  return memo.get(key)!;
}

const roleOf = (json: Json, roleName: string) => Object.values(json.Resources).find((r) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === roleName)!.Properties;
const statementsOf = (json: Json, roleName: string): Stmt[] => roleOf(json, roleName).Policies.flatMap((p: any) => p.PolicyDocument.Statement);
const sid = (st: Stmt[], s: string): Stmt => st.find((x) => x.Sid === s)!;
const grants = (s: Stmt, name: string): boolean => list(s.Resource).some((r) => iamMatch(r, arn(name)));
const ruleOf = (json: Json) => Object.values(json.Resources).find((r) => r.Type === "AWS::Events::Rule")!.Properties;
/** Whether the EventBridge `resources` filter of the PR-check rule matches the ARN of a repository: a literal ARN equals, a prefix object starts. */
const ruleFires = (json: Json, name: string): boolean =>
  (ruleOf(json).EventPattern.resources as (string | { prefix: string })[]).some((m) => (typeof m === "string" ? m === arn(name) : arn(name).startsWith(m.prefix)));

const NEW_REPO_NAMES = ["hamh-e2e-new-20261101", "hamh-e2e-ntier-web-20261101"];

describe.each(TABLE.cover)("the grants for the rule: $why", ({ rule, allows, refuses }) => {
  const json = () => deliveryUnder(rule);
  const gateway = async () => statementsOf(await json(), "hamh-direct-repo-gateway");
  const build = async () => statementsOf(await json(), "hamh-direct-prcheck-build");

  test.each(["ReadE2eRepositories", "PushHarnessBranches", "PullRequests"])("repo-gateway %s covers what the rule covers and nothing it refuses", async (id) => {
    const s = sid(await gateway(), id);
    for (const name of allows) expect(grants(s, name), `${id} ${name}`).toBe(true);
    for (const name of refuses) expect(grants(s, name), `${id} ${name}`).toBe(false);
  });

  test.each(["ReadE2eRepositories", "ApproveAndComment"])("the PR-check build role's %s covers what the rule covers and nothing it refuses", async (id) => {
    const s = sid(await build(), id);
    for (const name of allows) expect(grants(s, name), `${id} ${name}`).toBe(true);
    for (const name of refuses) expect(grants(s, name), `${id} ${name}`).toBe(false);
  });

  test("the rule that starts the PR check fires for the repositories the rule covers and for no other", async () => {
    const j = await json();
    for (const name of allows) expect(ruleFires(j, name), name).toBe(true);
    for (const name of refuses) expect(ruleFires(j, name), name).toBe(false);
  });

  test("the repositories --new-repo makes are reachable whatever the rule says, and are the only ones the creation statements name", async () => {
    const st = await gateway();
    for (const id of ["ReadE2eRepositories", "PushHarnessBranches", "PullRequests"]) for (const name of NEW_REPO_NAMES) expect(grants(sid(st, id), name), `${id} ${name}`).toBe(true);
    const j = await json();
    for (const name of NEW_REPO_NAMES) expect(ruleFires(j, name), name).toBe(true);
    for (const id of ["CreateNewRepositories", "NewRepositoryMain", "NewRepositoryDefaultBranch"]) {
      expect(list(sid(st, id).Resource), id).toEqual([arn("hamh-e2e-new-*"), arn("hamh-e2e-ntier-*")]);
    }
    expect(sid(st, "AllowlistNewRepositories").Condition).toEqual({
      "ForAllValues:StringLike": { "dynamodb:LeadingKeys": [`REPOALLOW#cc:${REGION}:hamh-e2e-new-*`, `REPOALLOW#cc:${REGION}:hamh-e2e-ntier-*`] },
    });
  });
});

describe("the repositories of the earlier prefix", () => {
  const EARLIER = "cah-e2e-x"; // COMPAT-CAH-DATA

  test.each([
    [undefined, true], [["hamh-e2e-*"], true], [["my-app", "hamh-e2e-*"], true], [["hamh-e2e-*", "team-*"], true], [["*"], true],
    [["my-app"], false], [["team-*"], false], [["hamh-e2e-new-*"], false],
  ] as [string[] | undefined, boolean][])("rule %j: the earlier prefix is reachable = %s", async (rule, reachable) => {
    const json = await deliveryUnder(rule);
    for (const [role, id] of [["hamh-direct-repo-gateway", "ReadE2eRepositories"], ["hamh-direct-repo-gateway", "PushHarnessBranches"], ["hamh-direct-prcheck-build", "ApproveAndComment"]]) {
      expect(grants(sid(statementsOf(json, role), id), EARLIER), `${role} ${id}`).toBe(reachable);
    }
    expect(ruleFires(json, EARLIER)).toBe(reachable);
  });

  test("the default rule is what it was: the current prefix and the earlier one, in that order, in every grant", async () => {
    const json = await deliveryUnder(undefined);
    for (const [role, id] of [["hamh-direct-repo-gateway", "ReadE2eRepositories"], ["hamh-direct-repo-gateway", "PushHarnessBranches"], ["hamh-direct-repo-gateway", "PullRequests"],
      ["hamh-direct-prcheck-build", "ReadE2eRepositories"], ["hamh-direct-prcheck-build", "ApproveAndComment"]]) {
      expect(sid(statementsOf(json, role), id).Resource, `${role} ${id}`).toEqual(repositoryArns(REGION, ACCOUNT));
    }
    expect(ruleOf(json).EventPattern.resources).toEqual(repositoryArns(REGION, ACCOUNT).map((a) => ({ prefix: a.slice(0, -1) })));
    expect(ruleOf(json).Description).toBe("hamh-direct: start hamh-direct-prcheck on hamh-e2e-* harness PRs into main");
  });
});

describe("the rule that starts the PR check", () => {
  test("an exact name stays a literal ARN, a prefix entry becomes a prefix and * becomes the account and Region", async () => {
    const mixed = await deliveryUnder(["my-app", "team-*"]);
    expect(ruleOf(mixed).EventPattern.resources).toEqual([arn("my-app"), { prefix: arn("team-") }, { prefix: arn("hamh-e2e-new-") }, { prefix: arn("hamh-e2e-ntier-") }]);
    expect(ruleOf(mixed).Description).toBe("hamh-direct: start hamh-direct-prcheck on harness PRs into main in the repositories of the stage's repository-name rule");
    const all = await deliveryUnder(["*"]);
    expect(ruleOf(all).EventPattern.resources).toEqual([{ prefix: `arn:aws:codecommit:${REGION}:${ACCOUNT}:` }]);
  });

  test("an exact name is not cut: the last character of my-app is still there", async () => {
    const json = await deliveryUnder(["my-app"]);
    expect(JSON.stringify(ruleOf(json).EventPattern.resources)).toContain(`${ACCOUNT}:my-app"`);
    expect(ruleFires(json, "my-app")).toBe(true);
    expect(ruleFires(json, "my-ap")).toBe(false);
    expect(ruleFires(json, "my-app2")).toBe(false);
  });
});

describe("grantedRepositoryNames", () => {
  test("lists the rule's entries as they are, then the earlier prefix when the default entry is among them, then the new-repository prefixes the rule does not cover", () => {
    expect(grantedRepositoryNames(["hamh-e2e-*"])).toEqual(["hamh-e2e-*", "cah-e2e-*"]); // COMPAT-CAH-DATA
    expect(grantedRepositoryNames(["my-app", "team-*"])).toEqual(["my-app", "team-*", "hamh-e2e-new-*", "hamh-e2e-ntier-*"]);
    expect(grantedRepositoryNames(["*"])).toEqual(["*"]);
    expect(grantedRepositoryNames(["my-app", "hamh-e2e-*"])).toEqual(["my-app", "hamh-e2e-*", "cah-e2e-*"]); // COMPAT-CAH-DATA
    expect(grantedRepositoryNames(["hamh-*"])).toEqual(["hamh-*"]);
    expect(grantedRepositoryNames(["hamh-e2e-new-*"])).toEqual(["hamh-e2e-new-*", "hamh-e2e-ntier-*"]);
  });
});

describe("the policies stay inside the IAM size limit with the longest rule", () => {
  test("eight entries of 100 characters: the inline policies of repo-gateway and the PR-check build role are under 10,240 characters", async () => {
    const longest = Array.from({ length: 8 }, (_, i) => `${String(i)}${"a".repeat(98)}*`);
    const json = await deliveryUnder(longest);
    for (const role of ["hamh-direct-repo-gateway", "hamh-direct-prcheck-build"]) {
      const text = JSON.stringify({ Version: "2012-10-17", Statement: statementsOf(json, role) }).replace(/\s/g, "");
      expect(text.length, role).toBeLessThan(10_240);
    }
    // The EventBridge pattern stays far below its 2,048 character limit.
    expect(JSON.stringify(ruleOf(json).EventPattern).length).toBeLessThan(2_048);
  });
});
