// The accounts file that the infra tests use when they need every role: a history entry, the active entry and a test entry, with placeholder accounts and profiles.
// It is the same file as scripts/tests/fixtures/accounts-three.json (infra/test/accounts-three.test.ts compares them), because the committed accounts.json holds one
// neutral entry. A test passes the document to `selectTarget`, or writes it to a temporary file and names the file in HAMH_ACCOUNTS_FILE for a fresh module graph.
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface FixtureEntry {
  account: string;
  region: string;
  profile: string;
  role: "history" | "active" | "test";
}

/** The three entries, in file order: history, active, test. */
export const HISTORY: FixtureEntry = { account: "111111111111", region: "us-west-2", profile: "history-profile", role: "history" };
export const ACTIVE: FixtureEntry = { account: "222222222222", region: "us-east-1", profile: "my-profile", role: "active" };
export const TEST_ENTRY: FixtureEntry = { account: "333333333333", region: "us-east-1", profile: "test-profile", role: "test" };

/** The Availability Zones of the `direct` stage of each entry (the AgentCore VPC AZ IDs are use1-az1 and usw2-az1). */
export const DIRECT_AZS: Record<FixtureEntry["role"], string[]> = { history: ["us-west-2a"], active: ["us-east-1b"], test: ["us-east-1a"] };

export const THREE_ACCOUNTS = {
  schemaVersion: 1,
  selector: "HAMH_AWS_PROFILE",
  accounts: [
    {
      account: HISTORY.account, region: HISTORY.region, profile: HISTORY.profile, role: HISTORY.role,
      azs: { live: ["us-west-2a", "us-west-2b", "us-west-2c"], drill: ["us-west-2a"], direct: ["us-west-2a"] },
      azIds: { live: ["usw2-az1", "usw2-az2", "usw2-az3"], drill: ["usw2-az1"], direct: ["usw2-az1"] },
      invocationLogging: "read-only",
    },
    {
      account: ACTIVE.account, region: ACTIVE.region, profile: ACTIVE.profile, role: ACTIVE.role,
      azs: { live: ["us-east-1b", "us-east-1c", "us-east-1d"], drill: ["us-east-1b"], direct: ["us-east-1b"] },
      azIds: { live: ["use1-az1", "use1-az2", "use1-az4"], drill: ["use1-az1"], direct: ["use1-az1"] },
      invocationLogging: "enable-and-restore",
    },
    {
      account: TEST_ENTRY.account, region: TEST_ENTRY.region, profile: TEST_ENTRY.profile, role: TEST_ENTRY.role,
      azs: { live: ["us-east-1a", "us-east-1b", "us-east-1c"], drill: ["us-east-1a"], direct: ["us-east-1a"] },
      azIds: { live: ["use1-az1", "use1-az2", "use1-az4"], drill: ["use1-az1"], direct: ["use1-az1"] },
      invocationLogging: "read-only",
    },
  ],
};

/** Writes `doc` as accounts.json in a fresh directory under the temp dir and returns its absolute path. */
export function writeAccountsFile(doc: unknown = THREE_ACCOUNTS): string {
  const path = join(mkdtempSync(join(tmpdir(), "accounts-three-")), "accounts.json");
  writeFileSync(path, JSON.stringify(doc));
  return path;
}

let shared: string | undefined;
/** The path of one written copy of THREE_ACCOUNTS, made on first use and kept for the run of the test file. */
export function threeAccountsFile(): string {
  shared ??= writeAccountsFile();
  return shared;
}

/**
 * Runs `fn` with `vars` set in process.env (undefined deletes a variable) and puts every variable back afterwards. `config.ts` reads HAMH_ACCOUNTS_FILE and
 * HAMH_AWS_PROFILE when it loads, so a test that builds the app for one entry resets the module graph inside `fn` and imports the app there.
 */
export async function withEnv<T>(vars: Record<string, string | undefined>, fn: () => T | Promise<T>): Promise<T> {
  const before = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  for (const [k, v] of Object.entries(vars)) {
    if (v === undefined) delete process.env[k];
    else process.env[k] = v;
  }
  try {
    return await fn();
  } finally {
    for (const [k, v] of Object.entries(before)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  }
}

/** `withEnv` for the accounts file of THREE_ACCOUNTS and the entry with this profile (undefined: the active entry). */
export function underEntry<T>(profile: string | undefined, fn: () => T | Promise<T>): Promise<T> {
  return withEnv({ HAMH_ACCOUNTS_FILE: threeAccountsFile(), HAMH_AWS_PROFILE: profile }, fn);
}
