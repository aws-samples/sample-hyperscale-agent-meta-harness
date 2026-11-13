// COMPAT-CAH COMPAT-CAH-DATA: the earlier names that the infra tests refer to. A test reaches an earlier name through this file, so the names are in one place
// when the compatibility window ends (VERSION 5.50 for the variables, 6.00 for the repositories): delete the file with the tests that import it.

/** The earlier spelling of a variable of the CDK app: `earlier("OWNER")` is `CAH_OWNER`. */
export const earlier = (suffix: string): string => `CAH_${suffix}`;

/** The current spelling of a variable of the CDK app: `current("OWNER")` is `HAMH_OWNER`. */
export const current = (suffix: string): string => `HAMH_${suffix}`;

/** The selector word that an accounts file generated before the rename carries. */
export const EARLIER_SELECTOR = earlier("AWS_PROFILE");

/** The prefix of the CodeCommit repositories that a stage deployed before the rename created. */
export const EARLIER_REPOSITORY_PREFIX = "cah-e2e-";

/** The ARN pattern of those repositories: the stage still reads, pushes to and opens pull requests in them (COMPAT-CAH-DATA). */
export const earlierRepositoryArn = (region: string, account: string): string => `arn:aws:codecommit:${region}:${account}:${EARLIER_REPOSITORY_PREFIX}*`;

/** The ARN patterns of every repository a stage reaches: the current prefix first, then the earlier one. */
export const repositoryArns = (region: string, account: string): string[] => [`arn:aws:codecommit:${region}:${account}:hamh-e2e-*`, earlierRepositoryArn(region, account)];
