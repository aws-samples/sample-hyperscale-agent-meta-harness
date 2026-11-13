# Changelog

This file records notable features, architecture updates, and fixes for Hyperscale Agent Meta Harness, starting with version 4.29, the first public release. The layout follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), with Added, Changed, Deprecated, Removed and Fixed groups appearing only when they contain an entry. Versions appear newest first, with `## [Unreleased]` at the top for changes without a version and version headings dated when the version number increases.

The root `VERSION` file supplies the version shown on every terminal screen as `ham vN.NN | backend vN.NN`, where N is the release line's major number and NN is a two-digit counter. Commands and variables appear under their current names.

## [Unreleased]

## [5.18] - 2026-11-13

### Changed

- When an update stops for a stage commit that the clone cannot place, the message names the cause that fits: a clone older than the stage, the same version, or a version that cannot be read. `ham update` says whether running it again helps and names the folder of its clone.

### Fixed

- `./setup.sh --update` and `ham update` now update a stage that was deployed from a public commit that the repository history no longer holds. Setup compares the stage's version with the clone's, part by part as numbers, and goes ahead when the stage's version is lower.

## [5.17] - 2026-11-13

### Added

- `scripts/deploy-preflight.sh --stage direct` checks whether a deploy can start and changes nothing. It checks the account, the Availability Zone mapping and the VPC range, and ends with `READY` only when all three pass.
- The deploy confirms its control endpoint before it reports success. It does not print the address.

### Fixed

- `scripts/deploy.sh` ends with a clear message on a machine without Python 3.11 or later.

## [5.16] - 2026-11-12

### Fixed

- `./setup.sh --update` keeps the stage's value of a setting when the new default would replace or delete resources, and the update goes through. Later updates keep the value until you export a value or `default`.
- Setup's plan names each kept value before anything is installed, with the export that takes the new default, such as `export HAMH_INSTANCES_TYPES=default`.

## [5.15] - 2026-11-11

### Added

- `ham update` updates the backend and then `ham` to the newest version on the main branch of the public repository. It fetches a copy of that version and runs its `./setup.sh --update`.
- `ham update --check` shows your `ham` and backend versions, the newest version with its commit and the changes between. It changes nothing.
- `ham update` asks once and waits until your own active runs end. It never cancels a run and does not stop anyone from starting one. `--yes` skips the question, `--wait <minutes>` sets the wait and `--no-wait` stops at once.
- The setting `update_source` names the repository that `ham update` reads. The word `off` turns `ham update` off.
- For a run that agreed to provider data sharing, `ham` switches the account's Bedrock data retention mode from `default` or `inherit` to `provider_data_share` before the start. One line names the account, the Region, both modes and the command that sets the mode back.
- In the terminal app, a run on a model that needs provider data sharing asks before it starts while the setting is off. `y` agrees for that run only.
- The run screen keeps the whole line about a switched mode in a `provider data sharing` card.

### Changed

- The default of `HAMH_INSTANCES_TYPES` is `m7g.4xlarge,m6g.4xlarge`, so a run can use the second type when EC2 has no capacity of the first. Export `HAMH_INSTANCES_TYPES=m7g.4xlarge` to use one type only.
- An update of a stage that has the Instances part and never chose its machine types stops before any stack change. Export `HAMH_INSTANCES_TYPES=m7g.4xlarge` to keep the machine the stage has, or `HAMH_INSTANCES_TYPES=default` to take the new list.
- A model that needs provider data sharing also runs in an account whose Bedrock data retention mode is `aws_review`.
- The help, `/help` and usage errors say "the coding agent" where they mean no single engine.
- The clean-checkout test report finds the test command of a Python project without test settings: `python3 -m pytest -q` for pytest files and `python3 -m unittest discover -v` for unittest files.
- Lifecycle pack 1.15.0: the self-check prints the flow it checks and asks of the quick path only what that path asks. `ham rules init` replaces an unedited 1.14.0 copy and keeps an edited one.
- The default rules have a `Delivery` section. `ham rules init` replaces an unedited earlier copy of the default rules and keeps an edited one.

### Fixed

- A run that delivers only a zip (`--s3-only`) keeps a workflow file under `.github/workflows/` or a root `buildspec.yml` in the zip and names each such file in the run's detail.
- The skills card and the review-round notice name the engine of the run.
- A run refused because its repository is not on the stage's allowlist names the command that allows it, `ham-admin repos allow cc:<name>`.
- A `--compute instances` run that found no EC2 capacity says so and names two ways on: start it again later, or start it without `--compute instances`.
- The `Tests` line says when a test command was stopped at its limit while it was still building, and that the report then says nothing about the tests.
- When no test command is found, the sentence lists where the report looked and says that a `.ham/test-command` file names the command.
- The clean-checkout test report installs the project's packages before test commands from the lifecycle log or the goal card, using `npm ci` or `pnpm install --frozen-lockfile` from the lockfile, or the packages of `requirements.txt` for Python commands. A failed install is named on the `Tests` line, with the tests listed as `NOT RUN`.
- A goal-loop run that changed no file says that it delivered no code and gives the command to try again from the same start.
- A run that the harness ended shows the same outcome label in its last stop digest and in its summary.
- Two setups of one build that end at the same moment no longer each leave the runner to the other. The last one removes it.
- A setup that stops following a shared build takes itself out of the deploy lock, so the other setup removes the runner.
- When setup cannot check whether the runner is still in use, it keeps the runner, prints a `RETAINED` line with the reason and ends with exit 8.
- A deploy that fails right after it takes the deploy lock releases the lock.
- A deploy lock held by `scripts/deploy.sh` or by a setup with `--build-on local` tells a second setup to wait for it to end.
- `./setup.sh --full` on a deployed stage checks the client folder only when it installs a client.
- A first full setup whose build failed after its deploy names `./setup.sh --update` as the command that finishes it.
- The scripted run of a task list through the terminal app gives every engine the task text as written, with its line breaks, and scores a run that delivered nothing as failed.

## [5.14] - 2026-11-10

### Added

- A deploy prints one `DEPLOY-SETTING` line for each stage setting. A change that replaces resources, lowers log retention or removes something needs a value you export.
- One deploy of a stage runs at a time, whatever the route. A second update stops with exit 7 and names the setup that holds the stage.
- A deploy that refuses before it changes anything prints `DEPLOY-REFUSED` and exits 3. Setup then says "The stage is unchanged." and removes the runner it installed.

### Changed

- An update of the commit the stage already runs only installs the client. `--force-deploy` deploys anyway, and an older commit needs `--revision`.
- An update installs the new client where you run `ham` and keeps the previous one beside it.
- Setup checks before the confirmation that it can write to the client folder. If it cannot, it stops with exit 1 and names `--prefix`.

### Fixed

- `./setup.sh --update` keeps every stage setting a stage has, such as the Instances part, the tool layer, the utility model, the research provider and log retention. Export `HAMH_<NAME>=default` to return a setting to its default.
- An update keeps the stage's tag `auto-delete=no` and its kind, `official` or `test`, also through the account file that `setup.sh` generates. Export `HAMH_KEEP_TAG` or `HAMH_STAGE_KIND` to change them.
- An update keeps the stage's operator and zone, also on a stage that an earlier release deployed. An update by another person stops with exit 7 unless you add `--change-operator`.
- Setup reads a stage setting exported under its earlier variable name as the current name, checks its value, shows it in the plan and passes it to the deploy. Two spellings with different values stop setup with exit 2.

## [5.13] - 2026-11-09

### Added

- Claude Fable 5.2 runs on OpenCode as well as on Cline. It needs the same agreement to provider data sharing: `--engine opencode --model fable --provider-sharing on`.

## [5.12] - 2026-11-09

### Added

- Claude Fable 5.2 can be chosen as `fable`. It needs the setting `run.provider_sharing` and an AWS account in provider data sharing mode.
- Add the setting `run.provider_sharing` and the flag `--provider-sharing on|off`. With it on, the prompts and answers of a run on a model that needs it go to the model's provider, which may keep them up to 30 days. It is off by default.
- Mark the models that need provider data sharing in `ham models` and in the model list of the terminal app, and refuse them before anything is sent while the setting is off.
- Show the setting and the account's Bedrock data retention mode in `ham doctor`.

## [5.11] - 2026-11-09

### Changed

- OpenCode runs now let a shell command run up to ten minutes, as Cline's do, and `commands.timeout_seconds` sets it for OpenCode too.

## [5.10] - 2026-11-08

### Added

- The model list offers four models on OpenCode: Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol and GPT-6 Astra. GLM 5.3 is not offered on OpenCode yet.
- Both engines now ship in one worker image.
- `engine/PARITY.md` lists every protection and tool built for engines and how each applies to every engine that runs. A test holds each row for both engines or names the exception.
- `--engine <name>` and `default_engine` choose the engine for a run, including `ham continue`. A start request without an engine uses Cline and keeps the older request format.
- The New run form has an Engine box beside the Target box. Press Up or Down while the Target box is focused to change the engine.
- `/engine <name>` chooses the engine for runs started in the terminal session without changing the config file. `ham engine <name>` reports availability and explains how to choose the engine.
- The model list supports a row for each engine, with a separate smoke record. The service refuses an engine and model pair without a passing record and names the pair.
- OpenCode runs have a harness-driven goal loop. Follow-up prompts ask whether the goal is complete, and the loop has no round or token cap.
- `AGENTD_DIRECT_TOOL_IDLE_S` sets the tool-idle limit in seconds (off for Cline, 3600 for OpenCode, and 0 disables it). A tool call with no engine output past the limit cancels the prompt and restarts the engine in a fresh session.
- `engines/opencode.toml` is accepted for OpenCode. `ham engine opencode show|check` reads it, but it has no setting yet.
- OpenCode 2.2.6 is the pinned release of the OpenCode engine.
- OpenCode loads rules through `instructions` and skills through `skills.paths` in a session configuration written by the harness.

### Changed

- Outcome sentences name the engine of the run, such as "OpenCode reported done", while Cline's sentences stay unchanged. `EVIDENCE.md`, pull request text and service answers also name that engine.
- The app's words about a run name its engine. A follow-up of a finished run uses the engine of that run.
- `/engines` lists the available engines and marks the engine of the run on screen.
- Rules and skills are checked on disk before the first model call for both engines.

## [5.09] - 2026-11-07

### Added

- Set the most KiB one clipboard copy carries with `ui.copy_limit_kib`, default 64 KiB. Accept 1 to 1,048,576 KiB.
- Set immediate messages with `run.max_interrupts`, default 5. Set questions with `run.max_questions`, default 3. Set user-requested fresh sessions with `run.max_fresh_sessions`, default 3. These counts apply to one run. Allow 0 questions.
- Set research file sizes with `research.max_file_mib`, default 1 MiB. Set unpacked sizes with `research.max_unpacked_mib`, default 200 MiB. Set total run sizes with `research.max_run_mib`, default 300 MiB.
- Set container-starting file signals with `run.container_signals`. Use the built-in list by default. Accept `default`, `none`, or one line of semicolon-separated entries. Signals include a Testcontainers dependency and a docker compose file. Also recognize `.ham/test-command` when it mentions docker.
- Set one code search's wall time with `AGENTD_TOOLS_SEARCH_WALL_S`. Default to 10 seconds.
- Set log record retention with `HAMH_LOG_RETENTION_DAYS`, default 30 days.
- Set pull request check length with `HAMH_PR_CHECK_MINUTES`, default 60 minutes. Set its build machine with `HAMH_PR_CHECK_COMPUTE`, default `small`.
- Configure the Instances machine with `HAMH_INSTANCES_TYPES`, `HAMH_INSTANCES_ROOT_FREE_GIB`, `HAMH_INSTANCES_WORKSPACE_GIB`, `HAMH_INSTANCES_LIFETIME_S`, and `HAMH_INSTANCES_IDLE_S`. Keep the existing defaults.
- Set Cline's output silence limit with `HAMH_PROVIDER_SILENCE_S`, default 900 seconds. Accept 60 to 3,600 seconds. Cancel the prompt when this limit is reached.
- Set setup build length with `HAMH_SETUP_BUILD_MINUTES`. Set its machine with `HAMH_SETUP_BUILD_COMPUTE`. Use these for the build in your own account. Make the minimum `HAMH_SETUP_AUTHORITY_HOURS` follow the build length.
- Set sub-agent result silence with `AGENTD_GOAL_HELPER_SILENCE_S`, default 1500 seconds. Set oldest sub-agent wait with `AGENTD_GOAL_HELPER_WAIT_S`, default 4500 seconds. Accept 1 to 86,400 seconds for each. Let 0 switch either limit off.

### Changed

- Allow runs lasting 30 days (720 hours), raised from 7 days (168 hours). Allow 256 sessions, raised from 64. Keep the default run length at 24 hours. Accept 1 to 720 for `run.max_run_hours`.
- Allow 43,200 minutes for `--input-wait`, `HAMH_INPUT_WAIT`, and `input_wait_minutes`. Raise the limit from 240 minutes. This answer window covers 30 days, the longest run. Cut each wait to the session's remaining time.
- Allow a delivered changed file up to 2 GB. Raise the limit from 100 MiB.
- Allow research searches requesting 20 results and 300 domains. Accept 1 to 20 for `ham-research search --max`. Raise that command's limit from 10.
- Allow 20 web addresses per research fetch call, raised from 5.
- Allow a prompt to name 150 web addresses.
- Allow 20 messages waiting for running Cline, raised from 3. End the message box title with the waiting count. Show this count when more than 3 messages are open.
- Allow pull request checks to build for 60 minutes. Raise the limit from 20 minutes.
- Allow both API functions 28 seconds to answer, raised from 15.
- Allow 10 repository name rules per stage, raised from 8.
- Bound messages after Cline's first apparent finish by remaining run time. Remove the 8-turn limit. Remove the 30-minute limit on extra work.
- Cancel the prompt after 25 minutes without a sub-agent result. Also cancel when the oldest open sub-agent reaches 75 minutes. End every sub-agent when cancelling. Tell Cline in the same session that they did not return. Let Cline finish with its own tools. Allow this recovery at most twice per run. End the run on a third stalled wait.
- Wait for quiet engine output after cancelling a prompt. Then send the next prompt. Resend it once if the engine answers immediately without a model call.
- Keep repositories whose checks start containers on the microVM under `--compute auto`. The Instances machine cannot run containers.
- Reject `--compute instances` for repositories whose checks start containers. End the run before any machine starts. Name the signal and file. Show the ways to change the choice.
- Allow request member `compute` to carry `containerSignals` in protocol 1.57.0. Keep the previous request when a run sets no list.
- Add `--network-wait-minutes N|full` to `./setup.sh --teardown` and `scripts/teardown.sh`. Wait for AWS to release the interface held by AgentCore. Accept 0 to 480 minutes for `N`. Let `full` mean 480 minutes. Set the setup default with `HAMH_SETUP_NETWORK_WAIT_MINUTES`, default 0. Keep the script default at 60 minutes.
- Name the network interface held by AgentCore during teardown. Include its id. Explain that AWS releases it up to 8 hours after runtime deletion. Print the command for running teardown again.
- Exit 9 when only network resources held by that interface remain. Change `./setup.sh --teardown` from exit 8. Change `scripts/teardown.sh` from exit 1.

### Fixed

- Prevent completed code from hitting the two hour run limit. Fix waits where sub-agents started further sub-agents that kept working.
- Keep research enabled for the whole run. Remove the shutdown about nine hours into a run. A default 24-hour run reaches this in its second session.
- Read every answer when searching a long run. Remove the limit of the first 50 control items.
- Count run messages by their bytes in the reply. Keep the message list loading for a full run.

## [5.08] - 2026-11-05

### Added

- Reject start requests larger than 6,225,920 bytes before sending them, counting quotes, backslashes and line breaks twice.
- Report the request limit, the largest content part and the largest file when a start request is too large, including service rejections with HTTP 413 `payload-too-large`.
- Reject attachment, rule, skill, continue and review files larger than the whole-request limit before reading them.

### Changed

- Allow up to 100 rule files per run.
- Allow up to 32 skills per run, leaving room for 25 custom skills alongside the lifecycle pack's seven.
- Show up to 32 rows in the run screen's skills card.
- Allow rule file names, attachment names and skill file paths up to 255 bytes.
- Accept any whole number for `max_nudges` in `continue.md`.
- Allow up to 262,144 bytes of text in an answer to a question or a message to a running Cline.
- Show message length as "N of 262,144 bytes" on the typing screen.
- Allow up to 4 MiB of message text per run.
- Keep up to 256 KiB of each command's output in the clean-checkout test report, retaining the last bytes and marking truncated output as cut. Set `AGENTD_REPORT_TAIL_KIB` from 4 to 4096 to change the size.
- Keep up to 32 report steps, plan up to 12 test commands, and retain command lines of up to 2,000 characters and reasons of up to 1,000 characters.
- Allow reports up to 16 MiB, shortening the longest outputs first when combined steps exceed the limit. Older `ham` clients show no test line for reports over 64 KiB.
- Allow each report command to run for up to 465 minutes while its session lives, without exceeding the run's own time limit. Set `run.report_command_minutes` from 0 to 465 minutes, or `AGENTD_REPORT_STEP_S` and `AGENTD_REPORT_BUDGET_S` up to 27900 seconds.
- Keep Cline's complete final message, up to 200,000 characters, in the run summary and `evidence/direct.json`, and show it whole in `ham`.
- Include the start and end of final messages over 32 KiB in `EVIDENCE.md` and pull requests, with a marker counting omitted characters and naming `evidence/direct.json`.
- Split pull request comment text into parts of at most 10,240 characters to fit the repository service limit.
- Keep up to 1,000 characters in the detail beneath the outcome label.
- Keep up to 96 tries, 96 model switches and 32 interventions in the run summary.
- Pass a brief of up to 16 KiB to the next slice.
- Keep event payloads of up to 16 KiB and post backlogs of large events in batches of at most 4 MiB.
- Search back to the first run unless `since` is given.
- List every plan entry and every tool-call edit in the chat view.
- Show chat blocks of up to 200,000 characters (100,000) and complete stored records of up to 2865 KiB.
- Store transcript records of up to 256 KiB by default, with `AGENTD_TRANSCRIPT_RECORD_CAP_BYTES` allowing up to 440 KiB.
- List up to 2,000 changed file paths.
- Give fresh sessions the user's complete task.
- Allow task notes of up to 200,000 bytes, include up to 24,000 bytes in the prompt, and keep a stop digest of up to 256 KiB.

### Removed

- Remove separate size limits for prompts, rule files, `continue.md`, `review.md`, attachments and skill files.
- Remove the limit on the number of attachments.
- Remove limits on the number of files in a skill and the depth of skill paths.
- Remove the 8,000-character limit on answers and messages to a running Cline.

### Fixed

- Return the whole prompt in run details when it exceeds 256 KiB.

## [5.07] - 2026-11-05

### Added

- Test steps marked NOT RUN for the container runtime show socket diagnostics and up to 2,048 bytes of runtime output, prefixed with `RUNTIME: `.

### Changed

- Write the DOX starter to root `AGENTS.md` before the first prompt, adding `THIRD-PARTY-NOTICES.md` when it contains the DOX framework.
- Commit the DOX starter before reading the run's base commit, excluding it from engine changes and the "no changes" label.
- Deliver the DOX starter even when a run ends early. Later sessions restore it from the saved bundle without another commit.
- Update the lifecycle pack to 1.14.0. The delivery skill completes and corrects the seeded tree at run end and removes the starter for a missed documentation ban.
- `ham rules init` replaces unedited lifecycle pack 1.13.0 copies and preserves edited copies.
- Remove the never-enforced 8 KB (before 5.06) and 9 KB (5.06) size advice from default rules; the opening now asks only for no secrets, with limits unchanged at 64 KiB per file and 32 files per run. `ham rules init` replaces unedited 5.06 defaults and preserves edited copies, where users may delete the size advice.

### Fixed

- Preserve container runtime processes and their child processes when the engine stops. Test containers remain available until `ham-containers down` after the report.
- Read gotestsum summary counts in Go test reports, calculating passed tests as total tests minus skipped tests and failures.
- Recognize `go run` and `go tool` invocations of gotestsum in test reports.
- Select the last command run even with `timeout` or leading environment settings, so a later passing run replaces an earlier failure in the report.

## [5.06] - 2026-11-05

### Added

- Configure multiple backends with `[[backend]]` tables containing `alias`, `apiEndpoint`, `region`, `awsProfile` and `customerContent`.
- Add top-level `defaultBackend` for commands that name no backend, and `customerContent` to declare whether the default backend is safe for customer content.
- Reject `--customer-content` starts unless the selected backend declares `customerContent = true`; customer content is disallowed by default.
- Add root `CHANGELOG.md` in Keep a Changelog format for every public version. Contributor rules require an `Unreleased` entry in the same change for each user- or maintainer-visible change.

### Changed

- Keep the three existing top-level configuration keys as the `default` backend. Clients with one backend behave as before.
- Read all backends concurrently in the Runs list with per-backend deadlines; merge runs newest first and show a backend column when two or more backends are configured.
- Show a separate banner for each unavailable, signed-out or slow backend without stopping reads from the others.
- Route commands for one run to its backend using the plain run ID; `--backend <alias>` skips the lookup.
- Require `--backend <alias>` or the start screen's Backend field when starting a run, unless `defaultBackend` is set.
- Check all backends concurrently with `ham doctor`.
- Make backends with versions older than the client supports read-only; show `mixed` in the title when backend versions differ.
- Make setup-wizard tests wait for the first status call or process exit instead of fixed time limits.
- Add `HAMH_TEST_WAIT_SCALE` (1-8) to extend remaining test time limits on slow or busy machines; the parallel scripts runner defaults to 4. Hung tests still fail.
- Default rules gain a "Dated rules" section for rule stamps in `YYYY-MM-DD HH:MM TZ` format. The agent flags stale or conflicting rules and requires user confirmation to change them.

## [5.05] - 2026-11-04

### Fixed

- Fixed PostgreSQL 16 startup on long session paths: it starts without a Unix socket, using TCP only. Early exits show the log tail and FATAL message; shutdown does not wait for open connections.

## [5.04] - 2026-11-03

### Added

- The `ham resume` and `/resume` commands start follow-up runs from paused or stalled work; `/resume --all` resumes all, and `R` in run lists asks for confirmation.
- The `run.max_run_hours` setting accepts 1 to 168 whole hours (default 24 hours) and takes effect from the next run; the `--max-run-hours <hours>` flag also sets it.
- New run marks: a yellow `=` for `paused` runs, which used their total time, and a magenta `~` for `stalled` runs, which lost their progress; both delivered their work.

### Changed

- Unfinished runs nearing the 8-hour microVM session limit continue from saved work in a new session under the same run id, until their total time is used up.

## [5.03] - 2026-11-03

### Changed

- Clean-checkout test reports use the repository's own test scripts and state what each script ran without.
- Lifecycle pack 1.13.0: run the failing test with the repository's test command before other edits. Full and auto commit the test and red record alone before code; quick: one commit, no record.

### Fixed

- Fixed finished runs showing `complete (delivering)` in `ham runs`, `ham status` and `ham watch`; they now show `complete (delivered)`. JSON keeps the stored phase.
- Fixed helper-agent reviews causing completed work to end as a stall; the run uses the reported-done label.

## [5.02] - 2026-11-03

### Changed

- The `ham` title band shows flame and animated smoke in five rows at 33 or more terminal rows; `ui.mascot_motion = false` stops motion in the band and empty Runs table.

## [5.01] - 2026-11-02

### Added

- The `ham config migrate [--dry-run]` command copies earlier settings into `~/.config/ham`, resumes interrupted copies and refuses differing files or symbolic links without overwriting them.
- The `setup.sh` installation keeps a relative link with the earlier command name beside `ham`.
- The `ui.deprecation_notices` setting controls earlier-name notices, defaults to on and takes effect after restart; `HAMH_NO_DEPRECATION_NOTICES=1` forces it off.

### Changed

- The empty Runs list mascot gains rocket flames and moving smoke; `ui.mascot_motion` defaults to on and takes effect immediately.
- The terminal app reads earlier settings read-only when `~/.config/ham` has no user content; writes exit 2 with migration guidance, and the current folder wins when both hold content.
- The terminal app, setup and deployment accept previous variable names; non-empty `HAMH_` values win, and setup and deployment refuse conflicting selectors.
- Agentd and lifecycle tools use `.ham` or an earlier repository folder for lifecycle pack records, refuse state in both and name that folder in default goals; user-written goals stay unchanged.
- The `./setup.sh --teardown` command covers both naming generations with one plan and confirmation; `--only legacy|current` limits coverage, and repositories remain in place.
- The `setup.sh --status` command reports `state=legacy` with exit 7 or `state=legacy-retained` with exit 8; `--doctor` fails with exit 3 and a FAIL line about the previous names.

## [5.00] - 2026-10-31

### Changed

- The project becomes Hyperscale Agent Meta Harness and the terminal app becomes `ham`.
- The variable prefix becomes `HAMH_`.
- AWS resource names use the `hamh-direct-*` prefix, and the project tag becomes `hyperscale-agent-meta-harness`.

## [4.42] - 2026-10-31

### Added

- Stages accept 1 to 8 repository names or wildcard rules through `./setup.sh --repo-name`, `repoNames` and `HAMH_REPO_NAMES`; updates preserve rules unless replacements are supplied.
- Full setup builds, checks and installs `ham-admin` alongside `ham`.

### Changed

- Updates refuse active runs before building with exit 7; `./setup.sh --update --allow-active-runs` defers the check but still refuses worker runtime changes while runs are active.
- Setup reuses a matching Go module store with a passed offline check, skipping the fill and full check.
- When no stage URL is set, `ham doctor` and `ham models --server` name the `api_endpoint` setting and say the URL ends in `/direct`.
- Unresolved stage addresses suggest `./setup.sh --status` or `ham config set api_endpoint <url>`. The Runs screen wraps long errors.

### Fixed

- Setup and deployment keep the stage's `owner` tag. Deployment refuses a differing `HAMH_OWNER` or account-file `owner` before building.
- `./setup.sh --build-on local` refuses a differing `HAMH_OWNER` with exit 7. On the default runner route, setup warns and continues with the stage's owner; the runner does not receive `HAMH_OWNER`.
- When the stage owns the destination, `./setup.sh --teardown` restores prior Bedrock invocation logging or unsets it; other destinations stay unchanged. Failures keep the record and exit 8.
- Teardown deletes queued secrets after their stacks are gone and exits 8 for incomplete deletion; runner cleanup retries caller IAM denials with the CloudFormation execution role.
- Teardown reports RETAINED only after CloudFormation accepts retention, shows AgentCore endpoint deletion progress, and explains retained network resources when a rerun holds only a VPC.
- Worker runtimes omit descriptions to prevent unintended runtime versions. A stage with an existing description gets one more runtime version at its next update.

## [4.41] - 2026-10-30

### Added

- Command-line help works before setup with `ham --help`, `ham -h`, `ham help`, `ham help <command>` and `ham <command> --help`.

### Changed

- `./setup.sh --teardown` empties the stage's two versioned buckets and installs no runner; it needs neither git nor a clone.
- Teardown limits deletion to stage-owned resources in the confirmed plan; resources that appear later remain untouched and make teardown incomplete.
- Noninteractive teardown requires `--account-id` with `--yes --acknowledge-teardown`; a missing ID exits 4 and a mismatched account exits 7.
- Stage and delivery repository owner tags and run-session's `TENANT` use the selected `accounts.json` entry's `owner`, falling back to the operator's `name`, then `ham-user`.

### Fixed

- Harness-protocol 1.52.0 groups test steps by runner, takes each group's largest counts and adds distinct count triples; counts stay absent if a test step lacks them.
- Fixed interactive client-only setup failing after four answers when `ham` is missing. The wizard asks for its source first; `./setup.sh --help` says at most six questions.
- Fixed setup modifying an existing `CDKToolkit` stack with a custom qualifier; it stops with exit 7 before creating resources and explains the required qualifier.
- Fixed full setup deployment failing when unresolved paths prevented the container from mounting `accounts.json`.
- Fixed setup marking stages complete without a deployment record when all stacks exist; they are partial, `--full` and `--update` redeploy, and `--status` prints state=partial (exit 5).
- Fixed local setup spinning: without `flock` or with bash older than 4.1, builds start without a slot; `flock` output or exit 126 or more stops the run with exit 2.

## [4.40] - 2026-10-30

### Added

- `setup.sh` defaults to full setup: it installs a CodeBuild runner, builds and deploys the stage, then downloads and configures the client; client-only installation is also available.
- `setup.sh` supports resumable operations, `--status`, `--doctor`, `--update` and `--teardown`, with runner or local builds and a read-only preflight.
- Client installation supports Apple Silicon Macs and Linux amd64 or arm64, checks downloads with SHA-256 and refuses Intel Macs with a reason.
- `HAMH_ACCOUNTS_FILE` selects one file for build and deploy tools without editing `accounts.json`; relative paths, missing files and directories are refused.

### Changed

- Default rules require the scanner kit before the last commit, give its command, and require the agent to run it in the background and save output to a file.
- The scanner kit reports its path and scan scope, includes uncommitted and untracked files, and limits fallback scopes to the 300 newest files.

### Fixed

- Fixed setup re-runs to check all nine stage stacks and current inputs before skipping deployment, check stored client bytes and complete interrupted Rust toolchain installs.
- Setup runner tries bucket writes up to three times with five-second pauses, so a completed build survives an S3 throttle.
- Setup preflight rejects `--invocation-logging enable-and-restore` unless `--build-on local` is selected, before creating configuration or infrastructure.

## [4.39] - 2026-10-28

### Added

- The scanner kit sorts findings by file and line, shows at most 40, and reports missing checks with `KIT-NOT-RUN <part>` and `missing=<parts>` while other checks continue.

## [4.38] - 2026-10-28

### Added

- Lifecycle pack 1.10.0 adds semgrep 1.78.0 scans of changed Python and shell files with pinned rules. The kit cannot scan when the rules package is unpublished.
- With `HAMH_SCANNER_KIT=off`, the kit reports `KIT-OFF`. Install or run failures emit `KIT-NOT-RUN` with a reason and exit 0; Cline logs NOT RUN and copies the line into its last message.

### Changed

- Default rules ask the agent with a scanner kit to scan changed files before the last commit. Findings must be fixed or explained as false positives in the repository's own form.

## [4.37] - 2026-10-28

### Changed

- Default rules tell the agent to compare failing tests' first error lines and read every differing error: tests already failing at the base because of this session can fail for a new reason.

## [4.36] - 2026-10-28

### Changed

- Default rules ask the agent before the last commit to list and read repository suppressions and reasons, check its diff for matches and add matching suppressions in the same form with its own reason.

## [4.35] - 2026-10-28

### Changed

- Default rules ask the agent before the last commit to list repository-suppressed IDs, check its diff for matches and, on a match, add the same suppression in the same form with its own reason.

## [4.34] - 2026-10-28

### Changed

- Default rules require checks after every change, even docs or comments, retries with redirect settings for missing paths or unwritable folders, and NOT RUN with a reason if still blocked.
- Default rules require suppression comments to match repository forms, rule ids and reasons; `ham rules init` replaces unedited installed copies and keeps edited copies.

## [4.33] - 2026-10-28

### Changed

- The lifecycle pack moves to 1.9.0 and the harness protocol to 1.50.0 with wording-only changes. Behaviour is unchanged.

## [4.32] - 2026-10-28

### Changed

- Lifecycle pack 1.8.0 generalizes contract and loop wording; `ham rules init` updates unedited 1.7.0 rules and loop skills, leaving edited files unchanged.

### Fixed

- Fixed slow process startup by capping the soft open-files limit for agentd and its child processes at 65536 by default, configurable with `AGENTD_OPEN_FILES_CAP`.

## [4.31] - 2026-10-27

### Added

- Run summaries record the engine identity under protocol 1.49.0; older summaries remain unchanged when identity is absent.

## [4.30] - 2026-10-27

### Added

- Agentd allows new successful tool calls before stall limits via `run.stall_new_calls`: 0 to 1000 calls between file changes, default 100; 0 disables the allowance, effective next run.
- Run evidence records the declared flow as `quick`, `full` or `none`; `ham pull`, `ham watch` and delivery cards show it, and `ham pull --json` includes it.
- Settings `run.cargo_no_debug_info` and `run.cargo_no_incremental` default to on for the next run; turning them off leaves the corresponding Cargo environment overrides unset.
- Settings `run.exclude_target_dir` and `run.outside_checkout_note` default to on for the next run; turning them off stops adding `target/` to the checkout's local exclude list and removes the outside-checkout goal note.

### Changed

- Reasoning defaults to `xhigh` for GPT-6.1 Sol and GPT-6 Astra, and `high` for Claude Opus 5.5, Claude Sonnet 5.5 and GLM 5.3; explicit levels override these defaults.
- Explicit reasoning levels a model does not support are rejected.
- Session check text marks unavailable checks `NOT RUN`, explains root, Docker socket and SysV shared-memory limits, and tells the agent to leave code unchanged for them.

### Fixed

- Fixed missing worker tools: every worker image family now includes `diffutils`, `patch` and `jq`, and a `pip` wrapper running `/usr/bin/python3.12 -m pip` was added.

## [4.29] - 2026-10-27

### Added

- The `ham` terminal app starts remote Cline runs on Amazon Bedrock AgentCore from Mac or Linux; runs continue with the laptop closed, and `ham watch` rebuilds run state when you return.
- Runs return an S3 zip with source, a git bundle and checksums; `ham pull` resumes downloads and checks SHA-256, while `ham clone` builds a local git repository from the bundle.
- `ham pr` prints the link to the CodeCommit pull request of a run.
- Bedrock is the only provider: choose Claude Opus 5.5, Claude Sonnet 5.5 (default), GPT-6.1 Sol, GPT-6 Astra or GLM 5.3 via `--model`, `HAMH_MODEL` or `ham config set default_model`.
- The `ham rules init` command installs a lifecycle pack with a rules file and seven skills; `ham rules edit` and `ham rules show` manage rules, with edits applying to the next run.
- Runs end with one of 15 fixed stop outcomes, without a quality verdict; "Cline reported done" requires a final Cline message and at least one changed file.
- Failed recovery shows "Cline needs you" and rings once: `p` messages, `s` starts fresh, `m` tries a model, `f` twice finishes. Runs ask up to 3 questions; unanswered runs deliver existing work.
