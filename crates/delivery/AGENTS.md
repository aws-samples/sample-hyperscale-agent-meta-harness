# crates/delivery

## Purpose

The three delivery Lambdas of the CodeCommit path,
which turn the head Cline left into a pull request and a zip:

- `repo-gateway`, the only holder of git write access: `stage`, `new-repo`, `push-pr`.
- `bundle-validator` (no secrets): branch, history, path, protected-path, size and gitleaks checks; for a delivery that passes it also writes the record of what the delivery changed (`changes.rs`).
- `packager` (no secrets): the zip, `MANIFEST.sha256`, SHA-256 metadata, the `ART#` item.

## Ownership

Owned by `crates/AGENTS.md`. Deployed by the `hamh-<stage>-delivery` stack as three arm64
container-image functions from one image (`images/delivery/`); `scripts/deploy.sh` builds the
three bootstraps with one `cargo lambda` arm64 call for the package `delivery` (`scripts/lib/deploy.py`). `crates/run-session`
builds the payloads of all three.

## Local Contracts

- Handlers take typed requests and are independent of the Lambda event types; CodeCommit (in
  `gateway.rs`), S3 and DynamoDB (`ports.rs`) are traits; `aws.rs` implements them; `git` and
  `gitleaks` run as child processes. `lambda.rs` builds the AWS-backed handlers from the function
  environment and one SDK configuration pinned to the home Region (`E2E_REGION`), and holds the per-event handlers; each
  binary is configuration, handler and the Lambda loop. `HAMH_STAGE` is `live`, `drill` or `direct`. `HAMH_OWNER` is the stage's `owner` tag value. The CDK app sets it from the account file. It contains 1 to 64 letters, digits or `+ = . @ _ -`. A function with a missing or invalid `HAMH_OWNER` does not start. `repo-gateway` puts the value on the CodeCommit repositories it creates. The
  delivery stack's IAM condition `aws:RequestTag/owner` requires the same value.
- Large objects travel by S3 URI (Step Functions state caps at 256 KiB): every URI input is
  `s3://<harness bucket>/runs/<runId>/...` with no empty, `.` or `..` segment (`run_object_key`).
- Errors: `Refused` (the exact reason `ham` shows), `Invalid` (malformed input), `Unavailable`
  (transient) and `AccessDenied` become the Lambda `errorType`. `AccessDenied` is S3's only (`S3Blobs` get, put and
  list, matched on the SDK error code, never on the message): the role has no grant for the key or the action, the same
  answer on every retry, so the run machine catches it by name beside `Refused` and `Invalid` and does not retry it. Without `s3:ListBucket` S3 also answers it for a
  key that does not exist, so the message names the operation and the key (`access denied: s3 get <key>: ...`). Every other
  S3 failure stays `Unavailable`. A CodeCommit answer no retry can change is `Invalid`
  (`CodeCommit <operation>: <code>: <message>`), matched on the SDK's typed variants: the refusal of a
  comment on a pull request with no commits (`BeforeCommitIdAndAfterCommitIdAreSameException`) and each
  operation's input-validation `Invalid*Exception`. Every other CodeCommit error stays `Unavailable`
  and is retried: throttling, 5xx and transport errors, access and key errors, and the
  `...DoesNotExist` answers, which right after a push can name a reference the API does not see yet.
  One JSON log line per invocation names function, operation, run and outcome, never the request body.
- Target checks, in order, before any repository read or push: `cc:` scheme and the stage's repository-name rule (`Gateway::repo_names`, `Config::repo_names` from `HAMH_REPO_NAMES`, `repo_names::RepoNames`, `checks::codecommit_name_in`; default `hamh-e2e-*`, which keeps the text `outside the hamh-e2e- prefix`; the names `--new-repo` creates are covered under every rule; `tests/repo_names.rs`, `tests/gateway_repo_names.rs` and the shared table `infra/test/fixtures/repo-names.json`). A rule that lists the default entry `hamh-e2e-*`, alone or among other entries, also covers, for a compatibility window, names that start with `EARLIER_DEFAULT_PREFIX`, the repository prefix of the earlier release, for staging and pull request delivery (`RepoNames::allows_earlier_name`); a rule without that entry covers exactly its listed entries, and the allowlist entry and the repository ARN check still decide which repository a run reaches. `new-repo` still creates only the two new forms below and refuses an earlier-prefix name as "not a new-repo name"
  (`gh:` is refused with "needs the tenant GitHub App (app mode), which is not installed": GitHub delivery is not supported; `gl:` is refused with "is a GitLab target, which is not supported":
  GitLab delivery is not supported), the
  `REPOALLOW#cc:<home Region>:<name>` item naming the same target, then `GetRepository` with the ARN in
  the home account and Region (`E2E_ACCOUNT`, `E2E_REGION`: the `accounts.json` entry the build selected). The allowlist key is exact; no wildcard entry is read.
- Change record: when `validate` passes a delivery it writes `runs/<run>/deliverable/changes.json` (`changes_key`) beside the verified bundle, from the same `base..head` changes it checked (`diff-tree --no-renames`: a rename is a deletion and an addition): `{schemaVersion: 1, runId, headSha, base: {kind: "commit", sha} | {kind: "empty-tree"}, total, omitted, files: [{path, status A|M|D|T}]}`. At most `MAX_FILES` (200) paths are listed, in path order; a path longer than `MAX_PATH_BYTES` (1,024) or holding a NUL is counted in `omitted` with the rest, and `total` counts every changed path (an empty delivery is `total` 0 with no files, not a missing record). The record is an aid to the follow-up digest and never part of the verdict: it is written after the checks, only for a pass, with the content type application/json and its SHA-256, and a write that fails (the role has no grant, a throttle) is logged and ignored. `control-api`'s head route reads it and uses it only when it names the run and the delivered head (`ham_core::head::ChangeList::from_record`); `crates/ham-core/tests/fixtures/changes/changes-v1.json` is the file both sides are tested against (this crate does not depend on `ham-core`, and so no `Cargo.lock` edge or image key moves). The role's grant is `WriteChangeRecord` in `infra/lib/stacks/delivery.ts`.
- Pushes: only `refs/heads/harness/<runId>` (`HarnessRef` is built from a ULID run id only), never
  forced; any other requested ref is refused before any read.
- Git sandbox: empty environment plus `PATH`, a scratch `HOME`, no system or global config, no hooks
  or fsmonitor, no prompts, no redirects. The CodeCommit credential (git-remote-codecommit's SigV4
  scheme from the role's current credentials, `credential.rs`) reaches git only through a one-shot
  credential helper reading two variables of that child.
- `stage` and container signals: when the payload carries `containerSignals` (lines of `ham_core::container_signals`; run-session sends them only when the run could end up on Instances), `stage` first refuses a bad line (`Invalid`, before any read), then reads the tree of the base commit it bundles with `ls-tree` and `cat-file --batch` (never a checkout; every file whose content a signal asks for is read, in batches) and answers `containerCheck` in `StageOutput`: `{}` for nothing found, `{"found": {"signal", "path"}}` for the first signal that matched, signals in list order and paths in sorted order. The check runs for all three kinds of base (repository, follow-up, empty) and before the bundle is uploaded, so a failed read stores nothing. Without the member there is no check and no `containerCheck`. `tests/gateway_container_signals.rs`.
- `stage` takes a repository target string or the run target (`repo`/`new-repo` stage that
  repository; `s3-only` makes no CodeCommit call and no allowlist read and bundles one empty-tree
  root commit by `ham-harness` at 2020-01-01T00:00:00Z on `main`, byte-identical on a rerun; a `baseRef` other than `main` is `Invalid` there). With `fromRun`
  the new run starts from the head an earlier run delivered. The target is `s3-only` with no `baseRef`, or a repository
  (`cc:` and allowlisted, not `--new-repo`) with no `baseRef` or with `harness/<source run>` (also `refs/heads/harness/<source run>`), the branch that run delivered on; any
  other `baseRef` or target is `Invalid`. The request is checked before the first read (ULID source other than the run itself, 40 lowercase hex head, the target and its
  `baseRef`); a repository target then goes through the usual scheme, allowlist and `GetRepository` checks (`Refused` as for any stage) and is otherwise untouched: no branch is
  fetched from it and nothing is written to it, so whatever the user did to the source run's branch since (merged, deleted, pushed to) changes nothing about the base. The
  source's `verified.bundle` is loaded with `load_verified` (it must verify and hold the head at `refs/heads/harness/<source>`; each `BundleProblem` is `Invalid`), the
  `main` branch is set to the head and `publish_base` writes the new run's `input/repo.bundle`: base = the old head, full history, the source's branch does not travel
  (`tests/gateway_from_run.rs`). An older gateway ignores the member and would stage an empty base: run-session `start-session` refuses a follow-up whose `staged.baseSha` is not `continueFrom.headSha`, and agentd checks the restored HEAD against it again. It
  writes the full bundle of `baseRef` (default branch when absent) with `HEAD` to
  `runs/<run>/input/repo.bundle`, with its SHA-256 metadata, and answers `bundleKey`, `bundleUri`, `baseSha`, `baseRef`, `sizeBytes` and `sha256`. A repository with no default branch
  is refused ("start it with --new-repo").
- `new-repo`: exactly the named repository, which must be `hamh-e2e-new-<yyyymmdd>` or
  `hamh-e2e-ntier-<kind>-<yyyymmdd>` (`harness_protocol::target::new_repo_refusal`, the exact
  reason otherwise). An existing allowlist entry must name the target; then `CreateRepository`
  with description `ham --new-repo for run <runId>` and tags `project`, `stage`, `owner`,
  `managed-by=repo-gateway`. A repository with another description, or whose `main` is not this
  run's empty base, is refused before any allowlist write, branch or push. The harness then writes
  the target's `REPOALLOW#cc:<home Region>:<name>` entry itself (conditional put; `addedBy`
  `hamh-<stage>-repo-gateway run <runId>`), the one allowlist write outside `ham-admin repos`; an
  empty-tree commit by `ham-harness` goes to the harness ref and `main` is created on it with
  `CreateBranch` (the role may not push `main`), then `UpdateDefaultBranch main`. A retry of the
  same run finds its own repository and base and writes nothing new.
- The verified bundle: `runs/<runId>/deliverable/verified.bundle`, a full-history bundle of
  `refs/heads/harness/<runId>` at the delivered head (`store::s3::verified_bundle_key`). `bundle-validator` writes it
  from `sourceBundleUri` (a worker turn bundle holding `headSha`) before validating, and names it in
  `verifiedBundleUri`; a missing, unverifiable or headless source is a `bundle` violation and writes
  nothing. `sourceBundleVersionId` names the version of the source bundle agentd wrote and `sourceBundleSha256` its digest (`check_request`: a version needs its digest, either needs a source bundle). With a version
  id the validator downloads that version (`Blobs::get_version_to_file`; a client that cannot is unavailable, never the current object) and a version that does not exist is a `bundle` violation; with a digest it hashes
  what it downloaded and a bundle of another digest is a `bundle` violation. `tests/b759_pinned_source.rs`. All three functions refuse a verified bundle that does not verify or holds another head.
- Follow-up pull request: `PushPrRequest.followUp` (`{runId, headSha, pullRequestUrl?}`, unknown members refused) names the earlier run this
  run continues, the commit it delivered (the base this run started from) and the console link of its pull request as its `delivery.pr` event kept it. The member is checked before any read
  (a ULID other than the run itself, a 40-hex head, a link that is exactly `pr_console_url` of this repository, `checks::pull_request_id_in`: otherwise `Invalid`). After the target checks and
  the load of the verified bundle, and before anything is pushed, the gateway checks that the recorded head is in the history of the head this run delivers (`Invalid` otherwise: every list of changes starts there) and reads the facts: `GetPullRequest` of the old pull request (it must belong to this repository and have
  `refs/heads/harness/<old run>` as its source, else `Invalid`: the id is global to the account), the tip of `refs/heads/harness/<old run>` and of the old pull request's destination branch
  (`git ls-remote`, no new IAM), and whether the recorded head is in the destination's history (fetched into the scratch repository) or the old pull request reports `isMerged`. `follow_up::decide`
  then chooses: merged work (any strategy, or by git alone) or a deleted branch (never pushed, no pull request opened, or deleted) aims at the destination branch of the old pull request
  (the `main` branch when the old run opened none), which must still exist; a branch that holds exactly the recorded head is stacked on (the new pull request targets `refs/heads/harness/<old run>`,
  so the changes listed start at that commit); a branch that is alive and points anywhere else, or a destination branch that is gone, is `Refused("delivery conflict: ...")`. A conflict happens before
  the push: no branch, no pull request and no comment of the new run reach the repository, and the machine's `PrFailed` path keeps the run `complete` with the zip and "pull request not opened:
  delivery conflict: ..." in the detail of `run.completed`. Otherwise the pull request is made as below with that destination, then one comment on it (token `hamh-pr-<runId>-follow-up`; it
  names the old run and its pull request link, the branch it targets and why) and, last, one comment on the old pull request that links to the new one (token `hamh-pr-<runId>-link`).
  CodeCommit takes a comment only between two different commits, so the pair is the old pull request's destination commit and source commit (the pair proven on new pull requests), else its merge base and source commit; each pair
  has its own token (`hamh-pr-<runId>-link`, `-link-mb`); a pair CodeCommit refuses for good (`Invalid`) is followed by the next, and when no pair takes it the answer says `oldPullRequestLinked: false` and the new pull request stands (it names the old one). A transient failure
  is an `Unavailable` and the machine retries the whole delivery; every call is idempotent (the tokens above, `CreatePullRequest`'s token with an unchanged request). Known limits: after a squash merge the old commits are not in the destination's history, so the new pull request lists the old run's changes again (the note on it says so; the content merges cleanly); a branch that moves between the check and `CreatePullRequest` (seconds) is not noticed, and a retry after a transient failure decides again (a user action in those minutes can make `CreatePullRequest` answer `IdempotencyParameterMismatchException`, which is final: the pull request of the first attempt stays and the run says it opened none); a follow-up of a follow-up whose immediate destination branch is gone is a conflict, not a walk up the chain. The answer carries `followUp
  {destinationRef, aim: old-branch|merged|deleted, oldPullRequestId?, oldPullRequestLinked}` only for a follow-up run. The fake of CodeCommit in `tests/common/mod.rs` refuses what the API reference says
  CodeCommit refuses (the same source and destination, a reference outside the heads namespace, a token reused with a changed request, a comment on a pull request of another repository, on one
  that does not exist or on a commit that does not exist), and `tests/follow_up.rs` pins the fake to those answers.
- `push-pr`: one PR to the `main` branch (the destination of a follow-up run is chosen as above) (token `hamh-pr-<runId>`) with the rule `hamh-prcheck-required` (one
  approval, one pool member, the role `hamh-<stage>-prcheck-build`) when the delivered head has a root build file (next bullet); an existing rule with other content is refused; comments carry
  tokens `hamh-pr-<runId>-c<i>`. A pull request whose source and destination commits are equal (a run that
  committed nothing) gets no comment, since CodeCommit refuses every one on it; the returned commits decide, and `comments` in the answer counts the
  comments posted. Title 150 and description
  and comments 10,240 characters. The shared CodeCommit fake (`tests/common/mod.rs`) refuses a comment
  with equal commit ids as the service does.
- The pull-request check needs a build file.
  The CodeBuild project of the delivery stack runs the source commit's root `buildspec.yml` and approves on success, so a repository without one (a mirror repository; every `--new-repo` repository, because the validator refuses any change
  to the root `buildspec.yml` and a run can never add one) would get a rule that nothing could satisfy. `push-pr` therefore reads the committed root tree of the delivered head in the local verified bundle (`ls-tree --full-tree`, one exact name, no CodeCommit call and no
  IAM change) before it pushes anything. The build file is a regular file (mode 100644 or 100755) named exactly `buildspec.yml` at the root; a link (its target is not protected by the validator, so a run could write the check that approves its own pull request), a folder,
  a submodule, a nested file and another spelling are none. With a build file the delivery is as before. Without one it attaches no rule (`approvalRules` is empty), posts the note `NO_CHECK_NOTE` once after the table comments (token
  `hamh-pr-<runId>-nocheck`, counted in `comments`; skipped on a pull request with no commits like the tables), and still pushes and opens the pull request. The note says what was not attached and claims nothing about the work. A rule that an earlier delivery of the same
  pull request attached (an older gateway, a retry) is never removed: `GetPullRequest` finds it, it is kept and reported, and no note is posted. A lookup that fails is an error (`Unavailable`, retried by the machine) and never "no build file". The CodeBuild project still starts
  for such a pull request and fails in its source download; nothing on the pull request shows it, because CodeCommit has no build status. Without the rule nothing here blocks merging; the note says so. Known limit: the decision is made once per delivery,
  for the head delivered then.
- Validator protected paths: `.github/workflows/**`, root `buildspec.yml` and the engine configuration. `ValidateRequest.engineConfig` is `all`
  (the default, left off the wire: `.cline/**`, `.clinerules/**`, `opencode.json`, `opencode.jsonc`, `.opencode/**`) or `executable-only` (`validator::EngineConfigMode`, `protected_in`,
  `evaluate_in`): `executable-only` protects only the `hooks`, `plugins` and `agents` directories of `.cline`, the `hooks` directory of `.clinerules`,
  `opencode.json`, `opencode.jsonc` and the `.opencode` directory, so Cline's own `.clinerules/*.md` notes do not discard a finished direct run; workflow files and
  `buildspec.yml` stay protected in both modes. Globs: last match decides, `!` negates, none allows nothing. Default limits: 2 GB per
  file, 2 GiB per bundle. gitleaks runs with `--redact --ignore-gitleaks-allow` in an empty
  directory and environment; findings never carry the secret; a scanner failure is an error.
- The target of the delivery: `ValidateRequest.target` is `pull-request` (the default, left off the wire)
  or `zip`. A request without the member is a pull request, which is what a caller of an earlier release sends; any other word fails the request. A git host runs a workflow file and the
  pull-request check runs the root `buildspec.yml`, so a pull request refuses a change (A, M, D, T) to either, with a detail that says what the file is and that a delivery to a repository refuses
  the change. Nothing runs a zip: for `zip` such a change is held to the globs like any other path and, when the head holds the file (A, M or T; a deletion puts nothing in the zip), it is
  reported under `ValidateOutput.findings` with the rule `workflow-file` or `buildspec`, before the third-party findings. The findings are what the judged range changed, not a list of every
  such file in the zip. The engine configuration is refused for both targets, because a follow-up run starts an engine in the delivered tree. The validator judges from the `baseSha` it is
  given; run-session picks it (`crates/run-session/AGENTS.md`), so a follow-up of a run whose head was refused is judged from that run's base.
- Third-party copy rule: when the run stored items through the research route
  (manifests under `runs/<run>/agents/direct/research/items/<id>/manifest.json`), the validator reads each item (its body's SHA-256 must match its manifest; a repository
  archive is opened and each file of 1 KiB or more hashed raw and with line endings, trailing blanks and the final newline made alike) and compares every blob that
  `base..head` adds (`Git::added_blobs`, `Git::blobs`: also a file added and removed again inside the range, but not one the base already held) with them. A match is a `third-party-copy`
  violation naming the delivered path, the stored path, the origin, the item id and the licence (and, for a file that is in the history only, saying so); it ends the run blocked with no zip,
  as a secret does. Stored files under 1 KiB, and licence texts (`LICENSE*`, `COPYING*`, `NOTICE*`), `.gitignore`, `.editorconfig`, lockfiles and minified files on either side, never count.
  A stored item that cannot be read or does not match its manifest is a violation too (the rule could not be applied): a repository archive must hold every listed file once with the listed length and SHA-256 and nothing else (an unlisted file, a link, a repeated name or a missing file fail), and the exempt and small files are checked too. A stored file whose normalised form is under 512 bytes matches its exact bytes only (`COPY_NORMALISED_FLOOR_BYTES`); delivered blobs under 512 bytes or over 8 MiB are not read. The rule looks at blobs that `base..head` adds: a new path that holds bytes the base already had is not a copy the run brought. A run that stored nothing, or added no file, is not asked anything
  (no object is read; the listing of the run's items is made for every run). The role needs `s3:ListBucket` (limited by `s3:prefix` to `runs/*/agents/direct/research/items/*`) and `s3:GetObject` on that
  prefix and nothing wider. A refusal of either is never read as "no items": the listing's `AccessDenied` ends the step as the machine's `AccessDenied` catch (no retry) and the run is
  blocked with `access denied: s3 list <prefix>` as its reason; a manifest or body that cannot be read is the violation above (`tests/third_party_access.rs`). `ValidateRequest.thirdParty` is `block` (default, left off the wire) or `allow` (the user allowed intentional vendoring): then matches are `ValidateOutput.findings`
  and the validation passes. `ThirdPartyMode` is the type of `harness_protocol::research` (re-exported here), the same words as the start request's `thirdParty` member; run-session puts `"thirdParty": "allow"` in the payload when the stored request says so, and leaves it off for `block` or no member. Attachments (`ItemKind::Attachment`, the user's own files) never take part: they are not material the research route stored, so a delivered file equal to an attachment is not a third-party copy.
  The runs whose items are read: the validated run, then the runs it
  continues whose commits are in the judged range, which run-session names in `sourceRunIds` (at most 32 run ids, the nearest first; left off the wire when empty; anything that is not a list of
  run ids of that length fails the request). A follow-up of a run that was not delivered is judged from that run's base and inherits none of its stored items; without the member a copy the earlier
  run committed passed unseen. Each run is listed and read once, an item always under the run that stored it (item ids count from one in every run), and a violation for an item of another run
  names it `<id> of run <run>`. The grant above covers the items of every run. A request without the member reads the validated run alone, which is what a caller of an earlier release sends.
- Packager: `PackageRequest.direct` (`harness_protocol::direct::DirectEvidence`) is required and holds what Cline reported about its stop. `src/` is `git archive` of the head, verbatim: an attributes file in the scratch git directory unsets `export-ignore`,
  `export-subst` and every conversion (`-text -eol -ident -filter -working-tree-encoding`), so each entry's bytes are its committed
  blob (an in-tree `eol=crlf`, `ident` or UTF-16 rule changes nothing), and a symbolic link is a link entry (`add_symlink`: Unix
  mode 0120777, the target as content; only links are written again, every other entry is a raw copy that keeps git's Unix attributes
  as written: a plain file has none, so it has no mode and reads as 100644, and an executable keeps its mode).
  A link's MANIFEST.sha256 line covers the link text, which is the entry's content (read the zip, not an extracted tree: `sha256sum -c`
  follows links); a link target that is not UTF-8 is refused. `ham clone` compares `src/` with the bundle's tree
  by path, blob id and mode. Entries carry the head's commit time, so reruns are byte-identical and the artifact id
  `zip-<sha256[..20]>` is stable; a tree with no link and no attribute keeps `git archive`'s entry names, bytes, times and execute bits (tested). The zip is
  at `s3::deliverable_key` (artifact kind `zip`). Every package also writes the evidence archive
  (`<top>-evidence/`: EVIDENCE.md, `evidence/`, MANIFEST.sha256) at `s3::evidence_archive_key`
  (`evidence-<sha256[..20]>`), each with its `ART#` item. Every object under `runs/<run>/evidence/` goes into `evidence/`; unsafe object names are skipped.
- The engine in EVIDENCE.md: `PackageRequest.direct.engine` (absent for Cline) names the engine that ran. The rows `<Engine> stop reason` and `<Engine> version`, the heading `## <Engine>'s last message`, the no-message line, the review sentence (`DirectReview::sentence_for`) and the closing sentences of "What the harness did" take its display name. A Cline run writes the file it always wrote, byte for byte. `tests/b788_engine_evidence.rs`.
- Review round: when `PackageRequest.direct.review` is set, EVIDENCE.md has a section `## Review round` (the one sentence of `DirectReview::sentence`: whether the harness asked Cline once to review its own delivery or why it did not, the seconds, the tokens, whether the round changed files and the head commits before and after) between the clean-checkout section and "What the harness did". The sentence uses none of the words in `BANNED_CLAIM_WORDS` and the section never changes the outcome line. A run without the record renders as before. Tests: `tests/packager_review.rs`.
- Flow row: when `PackageRequest.direct.flow` is set, EVIDENCE.md has one row `| Flow | quick |` (`full` or `none`) after the tokens row and before `Delivered from`: the word agentd recorded from the `Flow:` trailer of the final task commit's message (else the first line of Cline's last message). It is a fact beside the outcome; the outcome row and the label are untouched, and no text says the harness checked the work (`tests/packager_flow.rs`).
- Clean-checkout test report: when `PackageRequest.direct.testReport` is set, EVIDENCE.md has a section `## Clean-checkout test report` (the one sentence of the summary, then
  the file `evidence/test-report.json` that holds the steps, and that it does not say the tests are meaningful) between Cline's last message and "What the harness did"; the
  contents list names the file unless the report was switched off; and when a step ran, "What the harness did" says the harness also ran the repository's own test commands in a clean
  checkout and made no judgment (without a step that ran, the old sentence "It ran no test and made no judgment on the work." stays). The file itself rides in `evidence/` like any other
  object under `runs/<run>/evidence/`. After the sentence the section can hold two more paragraphs from the summary: `plan_note` (the command was chosen from the manifest of a Cargo workspace and runs every member) and `limit_note` (a command stopped at its limit: the command, the limit in seconds and the settings `run.report_command_minutes` and `AGENTD_REPORT_STEP_S` that change it); the machine line `PLAN NOTE: ...` of the report never appears. A request without the member renders word for word as before. Tests: `tests/packager_report.rs`.
- EVIDENCE.md (`render_evidence`) holds the run, repository, base, head and pull
  request rows, then `Outcome` (the label), `Detail`, `Cline stop reason`, `Model` (the model the user chose), `Ran on`, `Cline version`, `Rules` (names and the first 12
  characters of each SHA-256), `Wall time`, `Tokens` and `Delivered from` (`final bundle` or `last snapshot`), then `## Objective` and
  `## Cline's last message` as block quotes (a message cannot start a heading or a table), `## What the harness did` (one sentence: it
  hosted the session, committed what Cline left, scanned the commits for secrets and packaged the zip, and made no judgment; without a test
  report it says it ran no test, with a report whose step ran it says it also ran the repository's own test commands in a clean checkout) and
  `## Contents`. No text of it for any outcome contains a word of
  `harness_protocol::direct::BANNED_CLAIM_WORDS` (`tests/packager_direct.rs`).
- The `Tokens` row reads `total <n> = input <n> (not cached) + output <n> + cache read <n> + cache write <n>`. The four figures are `DirectEvidence.usage` (the shared `Usage`), the
  total is their sum with saturating addition, the same sum `ham` prints on the run card and control-api prices, so the row and the card agree. A run with no usage has no row. Tests:
  `tests/packager_direct.rs`.
- S3 objects use the bucket-default encryption (SSE-KMS); puts send `x-amz-meta-sha256` and
  `x-amz-checksum-sha256`. Without `s3:ListBucket` a missing object reads as AccessDenied
  (`AccessDenied`).

## Work Guidance

- TDD with real temporary git repositories for bundle, push and archive logic; AWS adapters against
  recorded HTTP exchanges (`StaticReplayClient`); the credential signature against the botocore
  vector in `tests/credential.rs`. AKIA-format test keys are built at runtime, never committed.
- Tests need `git` and `gitleaks` on `PATH`.

## Verification

```bash
scripts/cargo-slot.sh cargo test -p delivery
```

## Child DOX Index

None.
