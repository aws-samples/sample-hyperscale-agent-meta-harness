<p align="center"><img src="docs/images/ham-mascot.svg" width="112" alt="The ham mascot"></p>

# Hyperscale Agent Meta Harness

*A sample project for multi-harness orchestration on Amazon Bedrock AgentCore: remote coding-agent runs, controlled from your terminal.*

You deploy this sample application into your own AWS account and drive it from the ham terminal app. It uses AWS services under your configuration and permissions.

> one prompt in the ham terminal app starts a remote loop of Cline coding agents on AgentCore that keeps running with the laptop closed and returns a pull request and an S3 zip of tested code

The quickstart run with `--s3-only` returns an S3 zip. [What "tested code" means](#what-tested-code-means) says what "tested" covers. A pull request comes back only when you target an allowlisted CodeCommit repository whose name the stage allows (default names start with `hamh-e2e-`, as in `--repo cc:hamh-e2e-<name>`); see [Use a repository instead](#use-a-repository-instead-optional).

ham is the terminal app; a harness is a coding-agent program. The project runs two engines, Cline and OpenCode; AgentCore is Amazon Bedrock AgentCore, the AWS service that hosts the agent sessions. AgentCore hosts one microVM (a small isolated virtual machine) per run; AWS Step Functions orchestrates each run. agentd, the supervisor program inside the microVM, drives the engine of the run.

> **Disclaimer.** This is sample/reference software. A deployed stage creates real AWS resources that **incur cost**, some of them even when no run is active. [Costs](#costs) lists them. Review it before you deploy.
>
> **This is sample code, for informational and educational purposes only — not for production usage.** You should work with your security and legal teams to meet your organizational security, regulatory, and compliance requirements before deployment. You are responsible for securing your own AWS account, credentials, IAM scoping, network egress, and budget caps. Review the [Security section](#security) before enabling any live (billable) leg.

## Table of Contents

- [What this is](#what-this-is)
  - [Key properties](#key-properties)
  - [Headline counts](#headline-counts)
  - [What "tested code" means](#what-tested-code-means)
- [Architecture](#architecture)
  - [How it works in 8 steps](#how-it-works-in-8-steps)
- [Feature and capability catalog](#feature-and-capability-catalog)
  - [Harnesses and engines](#harnesses-and-engines)
- [Quickstart](#quickstart)
  - [Which stage, and how you get in](#which-stage-and-how-you-get-in)
  - [Set up, step by step](#set-up-step-by-step)
- [Configuration](#configuration)
  - [Where ham keeps its files](#where-ham-keeps-its-files)
  - [Which value wins](#which-value-wins)
  - [Settings reference](#settings-reference)
  - [Choosing the model](#choosing-the-model)
  - [What setup.sh writes](#what-setupsh-writes)
- [Usage](#usage)
  - [Your first run](#your-first-run)
  - [Keys and commands of the terminal app](#keys-and-commands-of-the-terminal-app)
- [Frequently asked questions](#frequently-asked-questions)
- [Models and accounts](#models-and-accounts)
  - [The six models](#the-six-models)
  - [Pick a model](#pick-a-model)
  - [Accounts and Regions](#accounts-and-regions)
- [Costs](#costs)
  - [What the idle stage costs](#what-the-idle-stage-costs)
  - [What runs and storage cost](#what-runs-and-storage-cost)
  - [Cost guardrails](#cost-guardrails)
- [Deploy the AWS infrastructure](#deploy-the-aws-infrastructure)
  - [Setup reference](#setup-reference)
  - [What the deploy does, in order](#what-the-deploy-does-in-order)
  - [Commands and flags](#commands-and-flags)
  - [Prerequisites](#prerequisites)
  - [Setup configuration](#setup-configuration)
  - [How the Cline binary reaches a new account](#how-the-cline-binary-reaches-a-new-account)
  - [What gets deployed](#what-gets-deployed)
  - [Verify the deploy](#verify-the-deploy)
  - [Developer verification](#developer-verification)
  - [Teardown](#teardown)
  - [Credentials](#credentials)
- [Limits](#limits)
- [Project layout](#project-layout)
  - [Documentation map](#documentation-map)
  - [Contributing](#contributing)
- [Security](#security)
  - [Safety and secrets](#safety-and-secrets)
- [License and attribution](#license-and-attribution)

## What this is

Hyperscale Agent Meta Harness is a sample project for multi-harness orchestration. You deploy it into your own AWS account and drive it from a Mac or Linux terminal. A harness is a coding-agent program. The project starts one harness per run, and its design is meant for several of them. The project runs two engines, Cline and OpenCode. A run uses the engine you choose and Cline when you choose none. One prompt in ham, the terminal app, starts a remote run on Amazon Bedrock AgentCore, the AWS service hosting agent sessions. The run continues with your laptop closed. Work returns in an Amazon S3 zip with source and a git bundle with full history. When you target an AWS CodeCommit repository, work also returns as a pull request there.

### Key properties

- **One prompt, passed through.** agentd, the supervisor program, runs inside a microVM, a small isolated virtual machine. AgentCore starts one per run. agentd sends your prompt once, byte for byte, with your rules. The service adds no planner and no roles. Cline decides whether to start sub-agents, helpers Cline starts by itself. Start with `ham "<prompt>" --s3-only --direct`.

- **Laptop closed.** AWS Step Functions, the AWS workflow service, orchestrates each run. It and the AgentCore session keep the run in AWS. You can close your laptop and collect the result later. `ham watch` rebuilds the run state when you return.

- **Work comes back.** The Amazon S3 zip holds source, a git bundle (`repo.bundle`), EVIDENCE.md, the file recording Cline's stop, and checksums. EVIDENCE.md says the service made no judgment of the work. `ham pull` downloads, resumes stopped downloads, and checks SHA-256. `ham clone` builds a local git repository from the bundle. `ham pr` prints the pull request link. Only allowlisted CodeCommit repositories receive pull requests. Delivery scans commits for secrets; a bundle secret blocks delivery.

- **Short-lived credentials.** The service makes per-session AWS STS credentials outside the microVM. AWS STS is the AWS service issuing short-lived credentials. Cline gets them only through a `credential_process` helper. No long-lived AWS key or GitHub token enters the microVM. The package proxy, the microVM's local package-fetching program, fetches packages from AWS CodeArtifact, the AWS package service. The proxy holds the CodeArtifact token; Cline never sees it.

- **Bedrock only.** Amazon Bedrock is the only model provider. Choose among six models: Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra, GLM 5.3 and Claude Fable 5.2. Claude Sonnet 5.5 is the default. Claude Fable 5.2 runs only with provider data sharing on. Pick in the terminal app, with `--model`, with `HAMH_MODEL`, or with `ham config set default_model`.

- **Limited network.** AWS Network Firewall, the AWS managed host-list firewall, allows a short host list. It drops and logs every other outbound request. AWS services in the Region use VPC endpoints. The pinned Cline build has telemetry compiled out. Cline is pinned by hash.

- **Recovery and 'Cline needs you'.** Provider failure triggers retries, session recovery, and model-route switching. Failed automatic recovery makes ham show "Cline needs you" and ring once. Answer with `p` (send a message), `s` (start a fresh session), `m` (try another model), or `f` twice (finish with what exists). Runs ask at most 3 questions. Without answers, runs finish with existing work and deliver as usual. While the goal loop is on, the harness ends a run that makes 30 tool calls in a row with no changed file and no new text; read [Limits](#limits) before you leave a run unattended.

- **Fixed outcome labels.** Each run ends with one of 15 fixed stop outcomes. Labels are "Cline reported done", "Cline did not start", or "Cline stopped:" followed by a fixed reason. "Cline reported done" requires Cline to end its turn with a message and at least one changed file. Labels describe how Cline stopped and give no quality verdict. The service never calls the work verified or tested.

- **Your own editable rules.** Rules are markdown files in `~/.config/ham/rules/`. Manage them with `ham rules init`, `ham rules edit` and `ham rules show`. Edits apply to the next run. A run takes up to 100 rule files of any size. Rules steer Cline and enforce nothing. `ham rules init` installs the lifecycle pack, a rules file plus seven skills that steer Cline through a development flow. The rules file is `~/.config/ham/rules/10-lifecycle.md`. The skills go in `~/.config/ham/skills/`, and a run sends them: research, context, tdd, build-test, loop, delivery and check. `ham rules init --no-skills` installs the rules file alone.

### Headline counts

Each count comes from the source file in the last column. Message-box commands are slash commands in the run-screen box; key bindings are single keys and chords. Patches are changes to Cline's source; a crate is a Rust package; the state machine is the Step Functions run definition.

| Construct | Count | Source of truth |
| --- | --- | --- |
| Amazon Bedrock models you can pick | 6 | `engine/catalog/models.json` |
| `ham` subcommands | 21 | `crates/ham-core/src/command.rs` |
| Message-box commands | 34 | `crates/ham-core/src/registry.rs` |
| Key bindings in the terminal app | 48 (14 typing-mode keys and 34 browse-mode keys) | `crates/ham/src/tui/keys.rs` |
| Fixed stop outcomes | 15 | `crates/harness-protocol/src/direct.rs` |
| CloudFormation stacks in stage `direct` | 9 | `infra/lib/config.ts` |
| States in the run state machine `hamh-direct-run` | 23 (12 of them task states) | `statemachines/direct.asl.json` |
| Patches on the pinned Cline build | 15 | `engine/cline/lock.toml` |
| Rust workspace crates | 15 | `Cargo.toml` |

### What "tested code" means

"Tested code" in the tagline at the top of this README means this: Cline writes and runs its own tests under your rules. The harness does not judge those tests and records how Cline stopped. The clean-checkout test report reruns the repository's own test commands on a fresh copy of the delivered head and says what they did. While it runs the screen says `running the repository's tests`, and a command that goes over its limit is named in the Tests line with the seconds it had. It runs in a step of the end sequence that takes at most 8 minutes (more when you raise the limit of one command with `run.report_command_minutes`) and never more than what is left of the run's time limit less 3 minutes (with under 4 minutes of the run's time limit left it is skipped, and the run's detail says so); a run you cancelled or one that ran out of time makes no report. It never changes the label.

## Architecture

![Architecture of Hyperscale Agent Meta Harness. The ham terminal app on your Mac or Linux machine calls Amazon API Gateway and AWS Lambda in your AWS account. Step Functions start a session in an Amazon Bedrock AgentCore microVM, where agentd runs the engine of the run, Cline or OpenCode, and the engine calls Amazon Bedrock models. Results land in Amazon S3 and AWS CodeCommit. Eight numbered steps follow the picture.](docs/images/architecture.svg)

The picture uses the official AWS Architecture Icons. If it does not load, open the [PNG version](docs/images/architecture.png).

### How it works in 8 steps

The numbers 1 to 8 in the picture match the steps below.

1. The ham terminal app on your Mac or Linux machine sends one request, signed with your AWS credentials (SigV4), that carries your prompt, your rules files and your model choice.
2. Amazon API Gateway accepts the request after AWS IAM authorization, admits only callers from the same AWS account, and passes it to the control-api AWS Lambda function.
3. The control-api function checks the model, rules and size, records the run in Amazon DynamoDB and Amazon S3, and starts one AWS Step Functions run of the state machine hamh-direct-run. The prompt, rules, skills and attachments together must fit one request, which carries about 6 MiB.
4. AWS Step Functions runs the hamh-direct-run state machine: its run-session Lambda function mints short-lived AWS credentials and starts the session, the worker-api function takes the session's events and "done" signal over a private API, and hamh-direct-run-ended cleans up runs that end early.
5. Amazon Bedrock AgentCore Runtime starts one microVM (a small isolated virtual machine) per run in a private subnet of the VPC; inside it agentd drives the engine of the run, Cline or OpenCode, which calls Amazon Bedrock models.
6. The engine installs packages through a local package proxy on 127.0.0.1:3128 that reads AWS CodeArtifact and keeps its token away from the engine, and AWS Network Firewall on the outbound path allows a short list of hosts and drops and logs the rest (AWS services in the Region are reached through VPC endpoints).
7. When the engine stops, AWS Step Functions has Lambda functions check the work for secrets, protected paths and size, open an AWS CodeCommit pull request to main (when you targeted a repository) and build the zip in Amazon S3, and ham pull, ham clone and ham watch bring the zip, a local git clone and the run events back to your machine.
8. AWS IAM roles and AWS STS short-lived credentials serve every step with no long-lived key inside the microVM, Amazon ECR holds the worker and delivery container images, and Amazon CloudWatch collects Lambda, firewall and Bedrock logs.

Full service names (the picture uses the short forms): Amazon S3 = Amazon Simple Storage Service; AWS IAM = AWS Identity and Access Management; AWS STS = AWS Security Token Service; Amazon ECR = Amazon Elastic Container Registry; VPC = Amazon Virtual Private Cloud; ACP = Agent Client Protocol (agentd talks to Cline over it); TUI = text user interface.

A run that ends as `blocked` delivers nothing. A lost session is not always a blocked run: the last snapshot can be delivered, without the lost conversation.

## Feature and capability catalog

| Feature | What it does |
|---|---|
| Terminal app (TUI, a full-screen text interface) | Screens show runs, dashboard, artifacts and a new-run composer. `ham "<prompt>" --s3-only --direct` starts a run from the command line. |
| One prompt, passed through | Your prompt reaches Cline byte for byte. Cline plans and picks its own modes and sub-agents. |
| Sub-agents (several agents?) | Sub-agents are helpers Cline starts itself. Each run uses one Cline session. The harness starts no other agent. It runs no planner or roles. Cline can use `spawn_agent` to start a helper. The harness leaves this tool on by default; you can switch it off with `tools.disable` in the engine file. The harness does not start or steer helpers. Cline decides whether to use helpers, so a run may use none. See [Does a run use several agents?](docs/user-guide.md#does-a-run-use-several-agents). |
| Six Bedrock models | Claude Opus 5.5; Claude Sonnet 5.5 (default); GPT-6.1 Sol; GPT-6 Astra; GLM 5.3; Claude Fable 5.2 (needs provider data sharing). Choose through TUI, `--model`, `HAMH_MODEL`, or `ham config set default_model`. |
| Your own rules | Editable markdown rules accompany every run: `ham rules init`, `ham rules edit`, `ham rules show`. The optional `continue.md` nudge file is off by default. |
| Laptop-closed runs | Step Functions, the AWS workflow service, keeps runs going with AgentCore, Amazon Bedrock AgentCore hosting agent sessions. You can close your laptop. `ham watch` rebuilds state later. |
| Delivery | An S3 zip (source, a git bundle, EVIDENCE.md, checksums) and, for a CodeCommit target, a pull request. `ham pull` resumes downloads and checks SHA-256. `ham clone` builds a local git clone. `ham link` gives a 15-minute download link. `ham pr` prints the pull request link. |
| Links and push | The dashboard shows the download link as one short line. `o` opens the pull request or a fresh download link in your browser. `d` downloads the zip again. `ham push <runId> <git-url>` pushes the run's branch to another git repository with your own credentials. At 80 columns the notice after `d` is cut at the screen width. |
| Clean-checkout test report | After Cline ends, the worker clones the delivered head into a fresh directory and runs the repository's own test commands there as the unprivileged user: the project's `.ham/test-command`, else the test runs in the lifecycle log, else the goal card's checks, else a conservative reading of the files at the tree root. The zip carries `evidence/test-report.json` (each command, its exit code, the counts the runner printed, the end of its output). The only claim is "repository tests pass in a clean checkout: N of N". It never changes the label, the outcome or the exit code. Off with `ham config set report_tests off`. The Tests line shows on `ham pull`, `ham watch` and the delivery card; the time one command may run is the setting `run.report_command_minutes`. The report step runs at most 8 minutes for all commands with the default limit, less when the run's time limit is near. It is skipped, and the run's detail says so, when under 4 minutes of the run's time limit are left. A cancelled run and a run that ran out of time make no report. |
| Lifecycle pack and skills | `lifecycle/pack/` holds the development flow once: one rules file and seven skills (research, context, tdd, build-test, loop, delivery, check). The rules file comes in three texts: the eight steps, a quick path for a small change in an existing repository, or both behind a flow statement (the default, 9 KB). `ham config set run.lifecycle_flow auto|full|quick` picks the text. `ham rules init` installs the rules file `10-lifecycle.md` and writes the skills to `~/.config/ham/skills/`, which a run sends; `ham rules init --no-skills` installs the rules file alone; `ham rules init --pack none` installs neither. `ham lifecycle init <dir>` copies the pack into a repository. By default, every project Cline delivers starts with a short docs tree in the DOX layout for `AGENTS.md` files, and the delivery skill tells Cline to leave your own `AGENTS.md` as it is; to change or skip the tree, say so in your request, edit the starter file `dox-root.md` in your skills folder (`DOX off` on its first line switches the tree off for every new project), or run with `--no-skills`. The harness writes the starter as the root `AGENTS.md` before the first prompt, so Cline loads the DOX rules from its first request. Rules and skills steer Cline. They enforce nothing. |
| Goal loop | Cline's goal plugin runs the goal loop by default. After Cline's first run ends, it queues follow-up prompts until Cline calls its goal-complete tool and that round ends with a message. The harness ends a run itself only when the loop stalls (for example the same `BLOCKED:` line in two rounds, or 30 tool calls in a row with no changed file), never on a count of rounds or tokens. `ham ... --goal off` makes one run a plain run. |
| Research and context | Cline searches the web and downloads public GitHub repositories for context. A new stage searches through AgentCore Web Search, a search service of AWS that needs no key. If the stage owner stores a key for Tavily, a web search service, the stage uses Tavily first and can also fetch pages. The microVM's firewall allowlist stays unchanged, and the Tavily key never enters the session. Repository download needs no key. Fetched files are placed read-only outside your repository, with an index of origin, date, licence and SHA-256. Delivery stops (no zip) when your repository holds a copy of a stored third-party file of 1,024 bytes or more, unless the run started with `--allow-third-party`. `--research` and `--no-research` switch it for one run. `--attach <file>` sends files of your own for Cline to read. |
| Follow-up run | `ham continue <runId> '<prompt>'` starts a new run from the code a finished run delivered, with its full history. The new run goes where the old one went: the same CodeCommit repository, or a zip alone for an `s3-only` run. `--s3-only` or `--repo cc:<name>` on the line chooses the target, and `ham` prints the target before it starts the run. In the run screen of a finished run, type your text and press Enter: the box then holds your text and a digest of the run, and its title reads `Prepare follow-up from commit <7 characters>: new billable run` once the service has named the delivered commit. Ctrl-L adds Cline's last message to the box. Press Enter again to start the new run. From a finished CodeCommit run's screen, the follow-up uses the same repository and can open a pull request linked both ways to the old pull request. A follow-up on a CodeCommit repository run needs a CodeCommit repository in the stage account. |
| Engine settings | `engines/cline.toml` sets reasoning effort: how hard the model thinks. It also sets command time limits, compaction mode and compaction trigger. It also sets tools to switch off. Reasoning is on by default at the model's level. `off` turns reasoning off. Manage the file with `ham engine cline check` (prints problems with their lines), `edit` (opens your editor, then runs check), `unset <key>` (removes one setting) and `import <file>` (checks the file and copies it to `engines/cline.toml`; `--replace` overwrites an existing file). |
| Run tags | A small model gives each new run topic and tech tags a few seconds after it starts; a run on a repository shows its repository as a project tag at once; you add and remove tags with `/tag`; `t` on the Runs list opens the tag explorer; and a tag you remove stays removed. |
| Long runs and resume | A run is not cut off at the 8-hour session limit. When the prompt deadline of a session comes and the run has time left, the service saves the work and continues the run in a new session under the same run id, from the best saved head, with a new Cline chat that is told where the work stands. A run lasts 24 hours in all unless `run.max_run_hours` or `--max-run-hours` says otherwise (1 to 720 hours). A run that used its total time stops as `paused`. A run whose session stopped answering, whose Cline sent no output for the silence limit, or whose sessions were lost, could not restore the earlier work or changed nothing three times in a row, stops as `stalled`. Both deliver the work and store the reason, which `ham runs --json` shows. The Runs list marks them (`=` yellow for paused, `~` magenta for stalled) and the filter finds them by status. `R` on the Runs screen and on a run, `/resume --all` and `ham resume --all` start a follow-up run for each, from the code it delivered, after one confirmation that names the count, and report what each did; `ham resume <runId>` does one. Resuming twice never starts a second run for the same run. The exit codes are 11 for paused and 16 for stalled. |
| Fixed outcome labels | Every run ends with one fixed label: "Cline reported done", "Cline stopped: <reason>" or "Cline did not start". |
| Correction path | After automatic recovery fails, "Cline needs you" asks you. Use p, s, m, f twice; `ham say`, `ham new-session`, `ham try-model`, `ham finish`. |
| Snapshots | Snapshots run every 10 minutes while Cline works, when files changed. After a lost session, the service delivers the last snapshot. |
| Package proxy | A package proxy serves Python, npm and Maven packages through CodeArtifact, and Go modules from the stage's own Go module store. CodeArtifact is the AWS package service. Cline never holds the CodeArtifact token. |
| Limited network and isolation | AWS Network Firewall allows listed hosts. It drops and logs other outbound traffic. Home-Region AWS services use VPC endpoints; S3 uses a gateway endpoint. The microVM, a small isolated virtual machine, holds no long-lived keys or GitHub token. Cline is hash-pinned with telemetry off. |
| Provider failover | When providers fail, the harness resumes the same Cline session. Then it rotates routes and models. It waits and probes when every route is down. |
| How many runs at once | Each run gets its own microVM, a small isolated virtual machine, in Amazon Bedrock AgentCore. The product states no maximum number of runs at once; your account's AgentCore and Bedrock quotas apply. See [Limits](#limits). |

Fifteen stop outcomes use fixed labels: "Cline reported done", "Cline did not start" and the "Cline stopped: <reason>" forms. "Cline reported done" means Cline says it is finished; it appears only when Cline ends its turn with a message and the harness counted at least one changed file between the base and the delivered head (a tree it could not count keeps the label). It is not a test result. The harness records how Cline stopped and gives no quality verdict. When the clean-checkout test report ran, its one sentence, "repository tests pass in a clean checkout: N of N", is an observation of the exit codes and counts the repository's own test runner printed; it stands beside the label, never in it.

**Not supported:** containers beyond the one offline image `postgres:16`; GitHub and GitLab delivery (only CodeCommit repositories get pull requests); other coding engines.

### Harnesses and engines

A harness is a coding-agent program that reads a task, edits files and runs commands. This project starts one harness per run in its own microVM. It passes the prompt and your rules, gives the harness short-lived credentials and a limited network, saves snapshots and delivers the result. The project runs two engines, Cline and OpenCode. Pi, etc. are the harnesses the design is meant for. A run uses one engine from start to end. The engine-specific functions are in `crates/agentd/src/engine/`, keyed on the engine name; a general engine adapter for other harnesses is a design.

#### Choose the engine of a run

Choose in one of four ways, in order of precedence:

- Pass `--engine <name>` on the command line, including with `ham continue`.
- Use the Engine box in the New run form.
- Use `/engine opencode` in the terminal app for runs started in that session.
- Set `default_engine` in `config.toml`.

To choose OpenCode for one run, or make it the configured default:

```sh
ham "<prompt>" --direct --engine opencode
ham config set default_engine opencode
```

Cline is used when you choose none. `ham engine opencode` shows whether it is available. A model is offered on an engine only when it has a passing smoke record for that engine. The service names the engine and model pair when it refuses one.

#### What both engines share

- Your rules in `~/.config/ham/rules` and skills in `~/.config/ham/skills` go with each run.
- Both engines use the harness `code_search` tool to search the work tree with ripgrep.
- Bedrock is the only model provider.
- Delivery includes the S3 zip, the local git clone through `ham clone` and the CodeCommit pull request.
- Outcome sentences use the engine's name.
- The goal loop sends your first prompt as typed and uses follow-up prompts to ask whether the goal is complete. Both engines use the same stall rules: blocked-twice, text-only-stall, no-progress, dead-loop, unfinished-claim, goal-cleared and round-no-progress. The harness never counts rounds or tokens.
- Both engines share snapshots and recovery.
- Both engines receive short-lived credentials through the `agent` profile and `credential_process`.
- The same network allowlist applies to both engines.
- The package proxy and the recovery and correction tools work the same way for both engines.
- The ACP client, the event collector, the stop classifier and the session driver in agentd name no engine.

#### Where the engines differ

- Settings: `engines/cline.toml` has five settings. `engines/opencode.toml` has one, `commands.timeout_seconds`, the time one shell command may run (10 minutes unless you change it, as on Cline).
- Goal loop: Cline's goal plugin runs its loop. The harness drives OpenCode's loop with the claim line `GOAL COMPLETE <nonce>: <summary>`.
- Helper agents: Cline uses `spawn_agent`. OpenCode uses `subagent`.
- Tool-idle limit: it is off for Cline. It is 3600 seconds for OpenCode.
- Models: each engine has its own smoke records.
- Tool name: Cline shows `code_search` as `ham__code_search`. OpenCode shows it as `ham_code_search`.
- Pinned releases: Cline is the project's fork at version `3.0.71`, pinned by SHA-256 in `engine/lock.toml`. OpenCode is release `2.2.6`, pinned by URL and SHA-256 in `engine/sources/opencode.toml` and `engine/lock.toml`.

Engine profiles live under `engine/profiles/`: `cline.toml`, `opencode.toml` and the test profile `fake-acp.toml`.

## Quickstart

This is the fastest path from a fresh clone to a first run.
`setup.sh` sets up the backend in your own AWS account and the terminal app `ham`.

**What you need**

- The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.
- You need an AWS account you may deploy into, with credentials in an AWS CLI profile or in `AWS_*` environment variables. Supported profile kinds are static keys, `credential_process`, IAM Identity Center (SSO), and assume-role. SSO and assume-role profiles need AWS CLI version 2.9.0 or later. Your credentials must be allowed to create IAM roles, normally through administrator access. Use a dedicated sandbox account.
- You need a Mac with Apple silicon (macOS 13 or later), or a Linux x86_64 or arm64 machine, with a terminal.
- Your AWS account needs access to Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra, GLM 5.3 and Claude Fable 5.2 for runs in the stage's Region, `us-east-1` or `us-west-2`. Claude Fable 5.2 also needs the Bedrock data retention mode of your account in that Region to let it run (`provider_data_share` or `aws_review`); `ham` switches the mode when a run agrees to share and your credentials may, or the account owner sets it; see "Models that need provider data sharing" in docs/user-guide.md. In `us-east-1`, your account also needs access to Claude Haiku 5.5, which only writes run names and tags; a stage in `us-west-2` has run names off and never calls Claude Haiku 5.5. Your account needs a valid payment method for AWS Marketplace purchases. Anthropic requires a first-time use case form once per AWS account. A Marketplace model's first call starts a subscription; stage roles lack Marketplace permission, so you make that call with your own identity with `aws-marketplace:Subscribe` and `aws-marketplace:ViewSubscriptions`, for example in the Bedrock console playground in the stage Region. A model still returning `AccessDeniedException` can need additional account-level access that AWS grants on request. The run drops a refused model for its remainder and ends with "Cline stopped: the model provider failed" if every route is refused. See "Before you start" in [Set up, step by step](#set-up-step-by-step).

```bash quickstart
git clone <repository-url> hyperscale-agent-meta-harness
cd hyperscale-agent-meta-harness
./setup.sh
export PATH="$HOME/.local/bin:$PATH"
ham "Write a Python command-line tool that counts the words in a text file. Add tests." --s3-only --direct
RUN_ID=$(ham runs | head -n 1 | cut -d ' ' -f 1)
ham clone "$RUN_ID"
```

`setup.sh` installs `ham` plus a link named `cah`, which works until version 5.50. The alias `cah` prints one notice on standard error and then behaves exactly as `ham`. You silence every compatibility notice with `HAMH_NO_DEPRECATION_NOTICES=1`. See [Moving from the earlier command name](docs/user-guide.md#moving-from-the-earlier-command-name) for the rest.

**What setup asks and does**

- The first question asks you to choose full setup, the default, or client-only setup. `--full` and `--client-only` skip it. Full setup deploys the backend and installs the client. Client-only setup installs and configures `ham` for an existing stage. A stage is a deployed service copy. You need `ham` already installed, built with `--build`, or supplied with `--from <directory>`. On a terminal, setup asks where `ham` comes from when it finds none and you gave neither option.
- On a terminal, setup asks for the AWS profile and the Region. It asks where to build only during full setup when your machine already has the local toolchain: `cargo`, `cargo-zigbuild`, `zig`, `node`, and `docker`.
- Setup asks numbered questions, one after another. It asks at most six questions. It skips each question whose option you gave.
- Setup reads your caller identity, account id, Region, and Availability Zone names and ids.
- Setup names the cost items on screen. [Costs](#costs) lists them.
- Setup asks for one confirmation: `Create these resources and start the deploy? [y/N]`. For scripts, use `--yes --acknowledge-costs`. After confirmation, setup asks nothing more.
- Setup installs an encrypted bucket, a CodeBuild project, and a runner role in your account. The runner is that CodeBuild project with its role. Setup also installs the CDK bootstrap stack when the account has none. The bootstrap stack supplies roles for deployment. Setup removes the runner after a successful setup unless you give `--keep-runner`.
- The runner builds the backend, the client and the operator tool `ham-admin` in your account. The backend includes the Cline binary, which the runner builds from pinned source. Setup needs no read access to any other account. The runner builds the client for macOS on Apple silicon, Linux x86_64, and Linux arm64.
- Setup downloads your platform's client from your own bucket and checks its SHA-256 checksum.
- Setup writes the endpoint, AWS profile, and Region settings with `ham config set`. It then runs `ham models`, `ham rules init`, and `ham doctor`.
- The final summary lists the endpoint, the setup phases, what stays in your account, and the first command. Resume an interrupted setup by running `./setup.sh` again.

**What setup grants**

The runner can assume the CDK bootstrap roles. Their CloudFormation execution role holds `AdministratorAccess`. For 12 hours after each install, treat the runner as an administrator of the account. `HAMH_SETUP_AUTHORITY_HOURS` sets this duration from 5 to 72 hours. After that time, the runner role assumes nothing.

The bootstrap roles trust the whole account. They stay after the runner is removed because updates need them. Only the installer—the identity that runs `setup.sh`—and the runner role can write to the runner bucket.

Anyone who may start a build of the project (`codebuild:StartBuild`) acts with that authority until the time ends. Give that permission to no one else in the account while the runner exists. Use a dedicated sandbox account.

**Before you copy the block**

- Replace `<repository-url>` with the repository's address.
- Add `--profile <your-profile>` and `--region <stage-region>` to `./setup.sh` to skip those questions.
- For a script, run `./setup.sh --full --yes --acknowledge-costs`.
- `./setup.sh --preflight-only` reads your account and creates nothing.
- For a stage that someone else runs, ask that person for its URL, Region and a role. See [Which stage, and how you get in](#which-stage-and-how-you-get-in). The stage URL ends in `/direct`. Then run `./setup.sh --client-only --build --endpoint <stage-URL>`. You can use `--from <directory>` in place of `--build` if the directory holds `ham` and `SHA256SUMS`. Full setup needs neither `--build` nor `--from`.
- Run the last three commands where you want the zip and the clone. A configured download directory changes the zip location.
- The line `export PATH="$HOME/.local/bin:$PATH"` lets you run `ham` in this terminal. You add that line to your shell startup file to use `ham` in a new terminal. Setup prints an `export PATH=...` line when `~/.local/bin` is not on PATH.

Run `ham` to open the app; `ham rules edit` opens your rules.
See [Set up, step by step](#set-up-step-by-step) and [Keys and commands of the terminal app](#keys-and-commands-of-the-terminal-app).

**Cost and safety:** The stage and every run create real, billable AWS usage in the stage's account, and the idle stage costs money before any run. Read [Costs](#costs) before starting. A first full setup usually runs about 80 to 85 minutes, and the setup build stops after 4 hours. `./setup.sh --teardown` removes the stage.

### Which stage, and how you get in

A stage is one deployed copy of the service. The Quickstart calls it stage `direct`. Its API accepts only AWS identities from the stage's own AWS account. Identities from other AWS accounts get HTTP 403. The profile's name does not change this rule.

The profile name is free: any name works. ham, the terminal app, signs each call with the identity behind the profile. The stage sees that identity and never the profile's name. Pass the name with `setup.sh --profile NAME`. Or set it with `ham config set aws_profile NAME`.

Nothing in the repository grants access. The person who runs the stage is the person who deployed it. Ask that person for three things:

- A role in the stage's AWS account.
- The stage URL, which ends in `/direct`.
- The stage's Region.

ham signs with an ordinary AWS CLI profile. Four kinds of profile work: IAM Identity Center (SSO), assume-role, a program that prints credentials (`credential_process`), and static keys. Use the kind you already use for the AWS CLI. `my-stage` is only an example name. Replace each placeholder in angle brackets with the value the person who runs the stage gave you (the account id, the role name, the Region) or with your own value.

IAM Identity Center (SSO): make the profile with `aws configure sso` (see the [AWS documentation](https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html)). The `sso-session` section holds your sign-in portal address (`sso_start_url`) and the Region of your IAM Identity Center (`sso_region`). Before each working session, run `aws sso login --profile my-stage`, with your own profile name.

```ini
[profile my-stage]
sso_session = <sso-session-name>
sso_account_id = <stage-account-id>
sso_role_name = <role-name>
region = <stage-region>

[sso-session <sso-session-name>]
sso_start_url = <sso-start-url>
sso_region = <sso-region>
sso_registration_scopes = sso:account:access
```

Assume a role: this profile assumes a role in the stage's account from another profile. Replace `<role-arn>` with the ARN of the role you were given.

```ini
[profile my-stage]
role_arn = <role-arn>
source_profile = <profile-that-holds-credentials>
region = <stage-region>
```

On an Amazon EC2 instance, an Amazon ECS task or another place that has its own credentials, write `credential_source` in place of `source_profile`, for example `credential_source = Ec2InstanceMetadata`. If the source profile uses SSO, run `aws sso login --profile` with the source profile's name.

A program that prints credentials: a profile with `credential_process` runs a program that prints your credentials as JSON.

```ini
[profile my-stage]
region = <stage-region>
credential_process = <command that prints your credentials as JSON>
```

Static keys: a profile with an access key pair also works. Prefer credentials that expire.

```ini
[profile my-stage]
region = <stage-region>
aws_access_key_id = <access-key-id>
aws_secret_access_key = <secret-access-key>
```

The `AWS_*` environment variables work too.

For an SSO or assume-role profile, ham runs the AWS CLI itself. It runs `aws configure export-credentials --profile my-stage --format process`, which prints the credentials as JSON. ham reads them, keeps them in memory until about five minutes before they expire, and never prints them. The AWS CLI must be version 2.9.0 or later, the first version with this command. ham finds `aws` on your `PATH`. To use another copy, set the environment variable `HAMH_AWS_CLI` to its path. ham waits at most 30 seconds for the command. ham makes no network call of its own to get credentials. Static keys, `credential_process` profiles and the `AWS_*` variables never start the AWS CLI.

When your sign-in has ended, ham says what to run. For an SSO profile the message is `Run: aws sso login --profile my-stage`, with your profile name. If the AWS CLI is missing or older than 2.9.0, ham says so and points to the [AWS install page](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html).

ham picks the profile in this order: the profile in its config (`setup.sh --profile NAME` or `ham config set aws_profile NAME`); the `AWS_*` variables, when both the key id and the secret are set; the profile named by `AWS_PROFILE`; the default profile.

After the install, `ham doctor` prints `PASS identity` when the stage accepted your account. A 403 or `FAIL identity` can mean two things. The identity may be outside the stage's account. Or the credentials may have expired.

To use a stage in your own AWS account, run the full setup with `./setup.sh`. See [Quickstart](#quickstart). It deploys your own stage in that account. [Deploy the AWS infrastructure](#deploy-the-aws-infrastructure) describes what it deploys.

The full setup builds the Cline binary in your account from pinned source. It needs no read access to any other account. [How the Cline binary reaches a new account](#how-the-cline-binary-reaches-a-new-account) explains this. To use a stage that someone else runs, ask that person for the three things above. Run `./setup.sh --client-only --build --endpoint <stage-URL>`. You can use `--from <directory>` in place of `--build` if the directory holds `ham` and `SHA256SUMS`.

### Set up, step by step

`setup.sh` is the single entry point for Hyperscale Agent Meta Harness. Steps 1 to 4 run the full setup, which deploys the backend in your AWS account and installs the client. Step 5 installs the client against a stage that already exists. You run these steps on your own Mac or Linux machine.

#### Before you start

- [ ] The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.
- [ ] You need credentials for the AWS account you deploy into. Use an AWS CLI profile or `AWS_*` environment variables. Profiles can use static keys, `credential_process`, SSO, or assume-role. SSO and assume-role profiles need AWS CLI version 2.9.0 or later. Before an SSO session, run `aws sso login --profile NAME`. [Which stage, and how you get in](#which-stage-and-how-you-get-in) shows a profile of each kind.
- [ ] You need permission to create IAM roles, normally through administrator access. Use a dedicated sandbox account.
- [ ] Your AWS account needs access to invoke Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra, GLM 5.3 and Claude Fable 5.2 for runs in the stage's Region, `us-east-1` or `us-west-2`. Claude Fable 5.2 also needs the Bedrock data retention mode of your account in that Region to let it run (`provider_data_share` or `aws_review`); `ham` switches the mode when a run agrees to share and your credentials may, or the account owner sets it; see "Models that need provider data sharing" in docs/user-guide.md. In `us-east-1`, your account also needs access to invoke Claude Haiku 5.5, which only writes run names and tags; a stage in `us-west-2` has run names off and never calls Claude Haiku 5.5. Model access is separate for each AWS account. AWS requires the Anthropic first-time use case form once per AWS account, or once at the management account of an AWS organization, before the first call. You submit the form by selecting an Anthropic model in the Bedrock console model catalog or with `aws bedrock put-use-case-for-model-access`. Calls return `AccessDeniedException` while access remains pending, until the status is `AVAILABLE`. The first invocation of a model sold through AWS Marketplace starts its subscription. Your AWS account needs a valid payment method configured for AWS Marketplace purchases; see [Request access to models](https://docs.aws.amazon.com/bedrock/latest/userguide/model-access.html). The identity making that call needs `aws-marketplace:Subscribe` and `aws-marketplace:ViewSubscriptions`. After activation by one identity, every role in the account can invoke the model without those Marketplace permissions. Models that AWS does not sell through AWS Marketplace need no subscription. The stage's roles have no Marketplace permission. You make the first call with your own identity, for example in the Bedrock console playground in the stage Region. Some models need additional account-level access beyond the form and subscription. You contact AWS Sales for additional access if a model still returns `AccessDeniedException` after you meet the requirements. The run drops a refused model for the rest of the run and ends with "Cline stopped: the model provider failed" if every route is refused.
- [ ] You need a Mac with Apple silicon (macOS 13 or later) or a Linux machine with a terminal.
- [ ] You need the repository's address, the `<repository-url>` of Step 1. The full setup needs a git clone. A zip download supports the client-only setup.
- [ ] For Step 5 only, ask the person who runs the stage for its endpoint ending in `/direct`, its Region and a role in the stage's AWS account. For `--repo` runs, also ask for an allowlisted CodeCommit repository whose name the stage's repository names cover (by default a name that starts with `hamh-e2e-`). Runs with `--s3-only` need no repository.

#### Step 1. Get the code

Clone the repository, then go into it.

```bash
git clone <repository-url> hyperscale-agent-meta-harness
cd hyperscale-agent-meta-harness
```

You should see `README.md` and `setup.sh` at the folder's top.

#### Step 2. Read the setup options

`./setup.sh --help` lists every option. It changes nothing.

```bash quickstart
./setup.sh --help
```

You see the usage text, which states the prerequisite:

The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh).

It lists every option: `--full`, `--client-only`, `--yes`, `--acknowledge-costs`, `--preflight-only`, `--keep-runner`, `--build-on`, `--account-id`, `--stage`, `--azs`, `--invocation-logging`, `--repo-name`, `--status`, `--doctor`, `--update`, `--revision`, `--allow-active-runs`, `--change-operator`, `--force-deploy`, `--teardown`, `--only`, `--network-wait-minutes`, `--from`, `--build`, `--sha256`, `--endpoint`, `--profile`, `--region`, `--default-repo`, `--prefix`, `--no-input`, `--dry-run`, `--verbose` and `--help`. `--acknowledge-teardown` appears inside the `--teardown` entry.

#### Step 3. Preview what setup will do

`./setup.sh --preflight-only` reads your caller identity, account id, Region and zones. It checks the clean git clone and the stage's state. It creates nothing. It ends with "Preflight finished" and exit code 0, including when the stage is already deployed.

`./setup.sh --dry-run` prints the plan and what it would do. It writes nothing.

```bash
./setup.sh --preflight-only
./setup.sh --dry-run
```

Each missing prerequisite prints a `FAIL` line. The exit code is 1. Fix what the line names, then run it again.

#### Step 4. Run the full setup

Run:

```bash
./setup.sh
```

The first question is "What do you want to set up?" The default is "Full setup", which deploys the backend in your AWS account and installs and configures the client. Choose "Client only" for an existing stage, as described in Step 5. Each title reads `Question N: ...`, where N counts the questions this run asks, in order. The full setup asks at most four questions: the mode, the AWS profile, the Region and where to build. If you choose Client only at the mode question and then answer the backend question with `No, deploy a stage into my own AWS account for me (full setup)`, the full setup asks the backend question as well, so it asks at most five. `--full` skips the first question, and the AWS profile question becomes `Question 1` when you omit `--profile`.

On a terminal, setup asks for the AWS profile and Region when you omit their options. The profile menu comes from `~/.aws/config` and `~/.aws/credentials`. Setup offers `AWS_*` credentials when both credential variables are set. Press Enter to accept the answer in brackets, or type `q` to stop.

Setup asks where to build only when your machine has `cargo`, `cargo-zigbuild`, `zig`, `node` and `docker` on PATH. The recommended choice is the runner, a CodeBuild project in your AWS account. The other choice builds on your machine.

Setup reads your caller identity, account id, Region and Availability Zones through the AWS CLI. It accepts `us-east-1` and `us-west-2`. It checks that the source is a clean git clone and records `HEAD` as the commit to build. It also reads the stage's state.

The "What setup will do" screen shows your account, caller identity, operator, Region, zone, stage, source commit, build route, generated account file and client installation. The operator is the identity the stage trusts for its admin routes and deploys. The "What setup will do" screen also has a line that starts with `run names:`. It says whether Claude Haiku 5.5 writes a short name and tags for each run in your Region. It does so in `us-east-1` only. In `us-west-2` run names are off, and each run shows a short label cut from the prompt. When all of the stage's stacks exist but the deploy did not finish, the plan says the stage is only partly deployed. `./setup.sh --full` then deploys again, and the complete stacks stay as they are.

The "What this setup grants" screen describes the runner's authority. The runner can assume the CDK bootstrap roles for 12 hours after each install. The bootstrap's CloudFormation execution role has `AdministratorAccess`. Treat the runner as an administrator during that period. Give no one else permission to start its CodeBuild project while the runner exists.

The "What it costs" screen names every cost item of the stage without amounts. [Costs](#costs) lists the items and gives example prices. The setup build stops after 4 hours (240 minutes), and setup prints progress every two minutes while each of its nine phases runs.

The confirmation is `Create these resources and start the deploy? [y/N]`. For scripts, use `--yes --acknowledge-costs`. `--full --yes` alone stops with exit code 4 and creates nothing. Setup asks nothing more after the confirmation.

Setup then performs these steps in order:

1. Install the runner stack `hamh-setup-runner`.
2. Upload the git archive and the generated account file as separate objects to your runner bucket.
3. If the account has no CDK bootstrap stack, have the runner print the pinned bootstrap template. Setup checks its SHA-256 against the pin in your clone and creates `CDKToolkit` with your credentials.
4. Start the build in your account. Follow its nine phases: `inputs`, `fetch`, `toolchain`, `account-file`, `deploy`, `gomod`, `scanner-rules`, `client` and `outputs`.
5. Download your platform's client from your bucket. Check its SHA-256 and install it to `~/.local/bin/ham`. `--prefix DIR` installs it to `DIR/bin/ham`. Setup creates the relative link `cah` to `ham` beside the installed binary.
6. Write the endpoint, AWS profile and Region settings with `ham config set`. Setup runs `ham config migrate` when `~/.config/cah` holds settings and `~/.config/ham` holds none.
7. Run `ham models`, `ham rules init` and `ham doctor`.
8. Remove the runner unless you use `--keep-runner`. The CDK bootstrap stack stays.

Full setup in `us-east-1` takes about 80 to 85 minutes. The breakdown is about 4 to 5 minutes for runner installation and CDK bootstrapping, about 60 to 67 minutes for deployment (including about 21 to 27 minutes for the Cline engine), and about 10 minutes for filling the Go module store and building the client. Duration varies with the Region and load. Setup prints a progress line every two minutes during build phases.

The final "Setup summary" lists the endpoint, account, Region, stage, source commit, phases, client path and SHA-256. It also lists retained resources and the teardown command. Its first command is:

```bash
ham "Build a small command-line todo app in Python with tests" --s3-only --direct
```

If setup is interrupted, run `./setup.sh` again. It attaches to the build it started without starting a second build.

See [Setup reference](#setup-reference) for flags, environment variables and exit codes.

#### Step 5. Client only: install against an existing stage

Use this step to install and configure the client for a stage that someone else runs. You need a `ham` program that you build with `--build`, supply with `--from <directory>`, or have installed.

```bash
./setup.sh --client-only --build --endpoint <stage-URL>
```

A client-only setup installs a `ham` that you supply or build. The full setup builds and downloads it for you. Without `--build` or `--from`, client-only setup uses `ham` in the install directory or on PATH. If no `ham` source is available, on a terminal setup asks where `ham` comes from, as described below. With `--yes`, `--no-input`, no terminal or the Stop answer, setup stops with exit code 1 and the message `there is no ham binary to install` without changing anything.

To build `ham` yourself, install [rustup](https://rustup.rs) first. On first use, rustup installs Rust 1.98.1, pinned in `rust-toolchain.toml`. To skip the build, use a prebuilt directory containing `ham` and `SHA256SUMS`. `scripts/build-macos-driver.sh` produces one for the Mac. A Rust build also needs a C compiler and linker named `cc` on PATH. On macOS, `xcode-select --install` installs one. On Linux, install `gcc` with your package manager.

Preview the client-only setup with `--dry-run`. Replace `<your-profile>` with the name of your AWS profile.

```bash
./setup.sh --dry-run --build --profile <your-profile>
```

This dry run writes nothing and runs no `ham` command. It checks for git, a source of AWS credentials and cargo when you use `--build`. You see a plan with `ok` lines and `would run` lines. If your PATH lacks the install folder, you also see a `warn` line. It ends with "Dry run finished" and "Nothing was written". The exit code is 0.

Each missing prerequisite prints a `FAIL` line and sets the exit code to 1. Fix what the line names, then run it again.

Find the endpoint before installing. The endpoint is `V1ApiUrl`, an output of the CloudFormation stack `hamh-direct-control`. This command needs credentials for the stage's account and permission to read stack outputs. Replace `<stage-region>` with the stage's Region and `<your-profile>` with your AWS profile.

```bash
aws cloudformation describe-stacks --stack-name hamh-direct-control \
  --region <stage-region> --profile <your-profile> \
  --query "Stacks[0].Outputs[?OutputKey=='V1ApiUrl'].OutputValue" --output text
```

You see a URL that ends with `/direct`. The person who runs the stage can also give you this URL.

Pick one of the two install commands below and replace each placeholder. The endpoint, Region and AWS profile must belong to the same deployment. Use the stage's Region for `<stage-region>`.

Building from source needs Rust and this checkout. It runs `cargo build --release --locked -p ham --bin ham`. `ham` does not use `HAMH_AWS_PROFILE`, which picks the account entry for the deploy tools and the stage's service programs. If its exported value names no `accounts.json` entry, `cargo build` exits with 101. Run `unset HAMH_AWS_PROFILE` before `./setup.sh --build`.

```bash
unset HAMH_AWS_PROFILE
./setup.sh --build --profile <your-profile> --region <stage-region> --endpoint <stage-URL>
```

Or use a prebuilt directory:

```bash
./setup.sh --from <dir-with-ham-and-SHA256SUMS> --profile <your-profile> --region <stage-region> --endpoint <stage-URL>
```

`--from` also accepts one file with `--sha256 HEX`. A checksum that does not match is refused.

You see the plan, the question "Go ahead? [y/N]", then `ok` lines. Add `--yes` to skip the confirmation.

In order, `setup.sh` installs `ham` to `~/.local/bin/ham`. `--prefix DIR` installs it to `DIR/bin/ham`. Setup runs `ham models` and prints the installed version. It writes your config with `ham config set`, using only the options you give and the answers you type. It runs `ham rules init`. It runs `ham doctor` when you give an endpoint. It then prints the next steps. You can run setup again; finished steps are left alone.

On a terminal, `setup.sh --client-only` asks about the options you leave out. It numbers the questions from 1 and asks them in this order: the stage endpoint (`--endpoint`), where `ham` comes from (`--from`, `--build`), the AWS profile (`--profile`), the Region (`--region`) and an optional default repository (`--default-repo`). A question whose option you give takes no number. The source question appears only when setup finds no `ham` at `PREFIX/bin/ham` or on PATH and you give none of `--from`, `--sha256` or `--build`. It offers to build `ham` from this checkout (when `cargo` is on PATH), to read a directory that holds `ham` and `SHA256SUMS`, or to stop. The default repository question appears only when an endpoint is known. Without `--client-only`, and when no other option sets the mode, the question "What do you want to set up?" comes first and the numbers shift by one. After a typed `--client-only` the setup asks at most five questions, and at most six when the first question is asked too.

With no stored endpoint and `--client-only` typed, the endpoint question is a menu with two choices: the URL, or `not now`, the default. A stored endpoint gives you a single prompt where Enter keeps it.

The profile menu comes from `~/.aws/config` and `~/.aws/credentials`. Each profile appears with its kind. Press Enter to accept the answer in brackets, or type `q` to stop. Nothing is written until you confirm the plan.

SSO and assume-role profiles appear as usable. Choosing either kind checks for the AWS CLI on your PATH. `HAMH_AWS_CLI` can name the AWS CLI program. The check requires AWS CLI version 2.9.0 or later. For SSO, `setup.sh` prints `aws sso login --profile NAME`. This client-only profile check asks the AWS CLI for its version.

The plan ends with an `again:` line that repeats your answers as options plus `--yes`. `--yes`, `--no-input` and a missing terminal ask no questions.

Client-only exit codes: 0 done, 1 a prerequisite or step failed, 2 bad usage, 3 installed but `ham doctor` found a problem. The full list is in [Setup reference](#setup-reference).

If `~/.local/bin` is not on your PATH, the script prints an `export PATH=...` line. Add it to your shell startup file yourself. Without `--endpoint`, the script skips `ham doctor`. If you install without an endpoint, set it to the URL you found:

```bash
ham config set api_endpoint <the URL you found>
```

The config file is `~/.config/ham/config.toml` or `$XDG_CONFIG_HOME/ham/config.toml`. Unknown keys make the file unreadable. Use `ham config set` and avoid hand edits.

#### Step 6. Write your rules and look at the models

`ham rules init` writes `~/.config/ham/rules/00-my-rules.md`, `~/.config/ham/rules/10-lifecycle.md`, `~/.config/ham/continue.md`, `~/.config/ham/review.md` and the pack's seven skills in `~/.config/ham/skills/`. `~/.config/ham/rules/10-lifecycle.md` holds the lifecycle pack's rules: testing and completion rules. These rules say to test first. Never weaken a test. Run the whole suite before saying done. Complete one real end-to-end run before saying done. `ham rules init` installs the text of the rules file that the setting `run.lifecycle_flow` names (`auto`, the default, `full` or `quick`; see [Choosing a flow](docs/user-guide.md#choosing-a-flow)). `ham rules init` creates missing files, including skill files you deleted; the output says `created`. It replaces files that you did not edit only when they still hold exactly earlier default or pack text from ham; the output says `updated`. Every file you edited is kept; the output says `kept`. `ham rules init --no-skills` leaves the skills out. `ham rules init --pack none` writes only `~/.config/ham/rules/00-my-rules.md`, `~/.config/ham/continue.md` and `~/.config/ham/review.md`. A run sends the skills it finds in `~/.config/ham/skills/`, and it takes at most 8 in all, yours included. One skill of your own fits beside the pack's seven. With two or more of your own, `ham rules init` installs the contract, leaves the skills folder as it is and says why. Cline reads the name and description of every installed skill when a session starts, about 500 tokens for the seven, and opens a skill's instructions only when the request matches its description. `ham rules show` prints the rules your next run will send. It also prints the skills your next run will send. The output includes sizes and SHA-256 hashes. `ham models` lists the five models. It marks the model your next run uses. All these commands work offline. They need no credentials.

The default rules file points to the lifecycle pack. Edit your rules with `ham rules edit`. An edit applies to the next run. Rules are plain markdown files in `~/.config/ham/rules/`. ham, control-api and agentd check the same limits. The limit is 100 files, of any size. A rule file may not be named `AGENTS.md`. Rules steer Cline; they enforce nothing.

By default, every project Cline delivers starts with a short docs tree in DOX, an open-source layout for `AGENTS.md` files, and the delivery skill tells Cline to leave your own `AGENTS.md` as it is; you can change or skip the tree in three ways: edit the starter `~/.config/ham/skills/lifecycle-delivery/references/dox-root.md` to set your own default (`DOX off` on its first line switches the tree off), say in your request that you want no docs, or run with `--no-skills`. The harness writes the starter as the root `AGENTS.md` before the first prompt, so Cline loads the DOX rules from its first request. [DOX docs in every project](docs/user-guide.md#dox-docs-in-every-project) has the details.

After a run finishes and delivers code, `ham continue <runId> '<text for Cline>'` starts a new run from that code. `ham lifecycle init <dir>` copies the lifecycle pack into a repository.

`continue.md` holds an optional nudge. Its front matter has `max_nudges`, the nudge count limit. Any whole number is accepted. The default 0 sends no nudges.

`review.md` holds the review prompt. When Cline ends a turn with a message and a file has changed, the harness sends this prompt once, in the same session. It asks Cline to review its own delivery as the maintainer who must merge it: remove what the task did not need, search for stale copies of every changed name, compare failing test names against a clean copy of the starting commit, and prove each claim with a command. The harness fills in the starting commit and four facts from the work tree. The front matter key `guard_minutes` (1 to 480; the default 20) is the least time a run must have left for the round to start. Edit the file with `ham rules edit review`. The round does not run after a `BLOCKED:` line, a question for you or a stop by the harness. `ham config set run.review_round off` switches it off.

```bash quickstart
ham rules init
ham rules show
ham models
```

You should see `created` or `kept` lines. You should also see the rules text. The model list should contain the six models below, with Claude Sonnet 5.5 marked as the default.

| Alias | Name | Bedrock model id | Context window (tokens) | Default |
|---|---|---|---|---|
| `opus` | Claude Opus 5.5 | `us.anthropic.claude-opus-5-5` | 1,000,000 | no |
| `sonnet` | Claude Sonnet 5.5 | `global.anthropic.claude-sonnet-5-5` | 1,000,000 | yes |
| `sol` | GPT-6.1 Sol | `us.openai.gpt-6.1-sol` | 1,000,000 | no |
| `astra` | GPT-6 Astra | `us.openai.gpt-6-astra` | 1,050,000 | no |
| `glm` | GLM 5.3 | `us.zai.glm-5.3` | 1,048,576 | no |
| `fable` | Claude Fable 5.2 | `global.anthropic.claude-fable-5-2` | 1,000,000 | no |

A run takes its model from the first source that is set: the `--model <alias>` flag, the `HAMH_MODEL` variable, `ham config set default_model <alias>`, then the built-in default. The picker in the TUI composer also chooses a model.

#### Step 7. Check the setup

Run this command to check your setup.

```bash
ham doctor
```

You should see one PASS, WARN or FAIL line per check: identity, api, versions, catalog, git-host and allowlist. The `versions` line names your ham and the backend, for example `PASS versions ham v5.NN | backend v5.NN`. The exit code is 0 unless a check fails. A `WARN versions` line means the two differ or the backend reports no version; it is advice and the command still exits 0. The `catalog` line states how old the newest model smoke record is and passes whatever its age. To get a warning for old records, add a limit in whole hours: `ham doctor --catalog-max-age 24` prints `WARN catalog` when the newest record is more than 24 hours old, and still exits 0. If a check fails, use this table.

| Symptom | What to do |
|---|---|
| `FAIL identity` or HTTP 403 | Your credentials expired or belong to another account. Sign in again. For an SSO profile, run `aws sso login --profile NAME`. Then try again. Ask the operator whether your identity is in the stage's account. |
| `FAIL api` or `FAIL allowlist` | The endpoint is wrong or the stage is down. The endpoint must end with `/direct`. Check it with the operator. |
| "ham makes no network calls for credentials" | The profile has none of the four kinds of settings. It has no static keys, `credential_process`, SSO settings, or `role_arn`. Fix the profile or choose another. [Which stage, and how you get in](#which-stage-and-how-you-get-in) shows a profile of each kind. Then run `ham config set aws_profile <name>`. |
| `Run: aws sso login --profile <name>` | Your IAM Identity Center sign-in has ended. Run that command, then try again. |
| `the AWS CLI was not found` or `older than 2.9.0` | SSO and assume-role profiles need AWS CLI version 2.9.0 or later. [Install or upgrade the AWS CLI](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html). You can also set `HAMH_AWS_CLI` to a newer program's path. |
| `did not answer within 30 seconds` | The AWS CLI took too long. Run `aws sts get-caller-identity --profile <name>` in a terminal to see why. |
| `FAIL catalog` | No model has a passing smoke record. Tell the operator. |

Setup is done. For the full list of problems and fixes, read the troubleshooting tables in [docs/user-guide.md](docs/user-guide.md).

## Configuration

ham keeps your settings in the client file, `config.toml`, inside `~/.config/ham/`.
A command flag or an environment variable can also set a value.
The file holds no secret, because ham reads credentials from your AWS profile or `AWS_*` variables when signing calls.
You can change the file with the commands below, in the Settings screen, or in any text editor.
Each command or screen change rewrites only the line it names, so your comments and the order of your lines stay.

- `ham config` prints every setting, showing `(not set)` for unset keys.
- `ham config get <key>` reads one setting.
- `ham config set <key> <value>` writes one setting.
- `ham config unset <key>` removes one setting.
- `ham config show` prints every setting with its value in force, where the value comes from and when a change takes effect. It then lists the settings the harness locks.
- `ham config check` reads the file and sends nothing. It prints each problem with its line. Exit code 0 means clean, 1 means a problem and 2 means wrong usage. `--strict` counts a warning as a problem.
- `ham config edit` opens the file in `$VISUAL`, else `$EDITOR`, else `vi`. It then runs `check`.
- `ham config docs` prints the table of all keys. The user guide holds the same table.

The Settings screen of the terminal app shows and changes these settings in place. Press `s` on the Runs list to open it.

The commands accept both spellings, such as `api_endpoint` and `apiEndpoint`.
The [Your settings](docs/user-guide.md#your-settings) chapter of the user guide has the details.

Cline has its own optional file, `engines/cline.toml`, beside `config.toml`.
It sets five Cline settings: `reasoning.effort`, `commands.timeout_seconds`, `context.compaction`, `context.compaction_trigger` and `tools.disable`.
`ham engine cline init` writes a template with every setting commented out.
`ham engine cline show` and `ham engine cline check` show and check what a run would send.
The Settings screen has a group `Cline engine` for the same five settings.
A user with no engine file changes nothing.
The [Engine settings](docs/user-guide.md#engine-settings) section of the user guide has the details.

This example shows `config.toml` after you turn the goal loop off in the Settings screen.

```toml
# my ham settings
apiEndpoint = "https://abc123xyz.execute-api.us-east-1.amazonaws.com/direct"
region = "us-east-1"
awsProfile = "my-profile"
configVersion = 1

[run]
goal = false
```

### Where ham keeps its files

- The config file is `~/.config/ham/config.toml`. A nonempty `XDG_CONFIG_HOME` moves it to `$XDG_CONFIG_HOME/ham/config.toml`.
- `ham` reads the folder `~/.config/cah` read-only when it holds your content and `~/.config/ham` holds none of your files. This fallback ends with version 5.50. You copy your content with `ham config migrate`.
- You preview the copy with `ham config migrate --dry-run`. The preview changes nothing. Nothing writes to or changes the folder `~/.config/cah`.
- The optional engine file is `~/.config/ham/engines/cline.toml`, under `$XDG_CONFIG_HOME` when that is set. `ham engine cline init` writes it. A run sends its values and never its comments.
- The flat `rules/` folder beside the config holds `.md` files. Each run takes at most 100 files, of any size.
- ham sends rules with every new run. They steer Cline and enforce nothing.
- `ham rules init` writes `00-my-rules.md` and `10-lifecycle.md` into `rules/`. It writes `continue.md` and `review.md` beside the config file and the seven lifecycle skills into the skills folder beside it.
- It keeps every file you edited. It writes the default rules file only when `rules/` holds no file of yours, and replaces it when it still holds an earlier default you did not edit.
- `ham rules init --no-skills` leaves out the skills. `ham rules init --pack none` leaves out the skills and `10-lifecycle.md`, the file that holds the lifecycle rules.
- The lifecycle pack bundles one rules file and seven skills. These short guides describe a development flow for Cline.
- `continue.md` holds optional nudge text, a follow-up message for Cline. Its front matter key `max_nudges` accepts any whole number; default 0 sends no nudge.
- `review.md` holds the review prompt that the harness sends once after Cline reports done with a changed file. Its front matter key `guard_minutes` accepts 1 to 480. Without it, a run needs 20 minutes left.
- The skills folder, `~/.config/ham/skills`, sits beside the config file. `ham rules init` fills it with the seven lifecycle skills. `ham rules init --skills` does the same.
- Each run sends the skills it finds there. Rules changes apply from the next run.
- Started runs keep their original rule text. Downloaded zips go to the download directory in the table.
- The file has 46 keys: `apiEndpoint`, `region`, `awsProfile`, `defaultRepo`, `downloadDir`, `defaultModel`, `inputWaitMinutes`, `mouse`, `reportTests`, `run.goal`, `run.rules`, `run.skills`, `research.enabled`, `ui.notify`, `ui.bell`, `ui.mascot`, `run.report_command_minutes`, `run.stall_calls`, `run.stall_window_minutes`, `run.stall_min_calls`, `run.command_grace_minutes`, `run.final_message`, `run.lifecycle_flow`, `run.review_round`, `run.compute`, `run.stall_new_calls`, `run.cargo_no_debug_info`, `run.cargo_no_incremental`, `run.exclude_target_dir`, `run.outside_checkout_note`, `ui.deprecation_notices`, `ui.mascot_motion`, `run.max_run_hours`, `run.provider_sharing`, `ui.copy_limit_kib`, `run.max_interrupts`, `run.max_questions`, `run.max_fresh_sessions`, `research.max_file_mib`, `research.max_unpacked_mib`, `research.max_run_mib`, `run.container_signals`, `defaultEngine`, `defaultBackend`, `customerContent` and `updateSource`. The first nine and the last four are top-level keys in camelCase. The thirty-three between them sit in the `[run]`, `[research]`, and `[ui]` tables. Their names include the table name and a dot. A file holding any of these thirty-three, or any of the last four, also holds `configVersion = 1`. ham writes that line itself. `ham config` prints and takes the thirteen top-level keys in snake_case, like `api_endpoint` and `default_backend`. It prints and takes the thirty-three table keys with their dots. The next section lists all 46 keys.
- A change rewrites only the line it names. Comments, key order and every other byte stay. Setting a key to its current value writes nothing. ham writes through a temporary file and a rename. A new file is private, with mode 0600. If the file changed on disk after ham read it, ham stops and writes nothing. If two writers race, the last rename wins.
- An unknown key or a wrong value is an error that names its line. A file that is not valid TOML is never overwritten. `ham config get`, `show`, `check`, `edit`, `docs` and `ham doctor` still run on a file with a bad key, so you can repair it. `ham models`, `ham runs` and a new run refuse such a file and name the line.
- Setting one of the thirty-three table keys adds a table and `configVersion = 1` to the file. Older builds of ham refuse a file that holds a table. `ham config unset <key>` takes the key out again, and the table with it when the table is empty. The user guide explains this.

### Which value wins

For most settings, the first set source wins, in this order.

1. A start flag applies to one run, including `--model` and `--input-wait`. `--rules`, `--no-rules`, and `--continue` replace files for one run.
2. An environment variable supplies `HAMH_MODEL`, `HAMH_INPUT_WAIT`, or `HAMH_DOWNLOAD_DIR`.
3. The config file supplies the value.
4. The built-in default supplies the value.

Blank environment or config values count as unset.
Wrong model or answer-wait values cause errors that name their source.
Model and answer-wait errors never fall through to another source.
A typo in `HAMH_MODEL` cannot silently select another model.

For `region` and `aws_profile`, the file wins over the environment.
Region comes from `region`, then `AWS_REGION`, then the AWS profile.
When `aws_profile` is set, ham signs with that profile.
Otherwise, it reads `AWS_*` credentials first, then the default profile.

`HAMH_NO_MOUSE` accepts `1`, `true`, `yes`, or `on` in any letter case.
It keeps mouse input off, overriding the file and `/mouse on`.

`HAMH_DOWNLOAD_DIR` has no flag.
It wins over `download_dir`, followed by the current directory.

`--attach` applies only to one run and has no config key. The Settings reference table lists `run.goal` for `--goal` and `research.enabled` for `--research` and `--no-research`. [docs/user-guide.md](docs/user-guide.md) lists the options for a run.

### Settings reference

| Setting | Where to set it | Default | What it does |
|---|---|---|---|
| `api_endpoint` | `ham config set api_endpoint <url>` or `./setup.sh --endpoint <url>` | Not set. Commands calling the stage stop with an error until set. | The stage URL, which is the `V1ApiUrl` output of the CloudFormation stack `hamh-direct-control`. It ends in `/direct`. When the host of the URL does not resolve, `ham` says the stage may have been removed or replaced. `./setup.sh --status` shows the current endpoint, and `ham config set api_endpoint <url>` sets it. |
| `region` | `ham config set region <name>` or `./setup.sh --region <name>`; fallbacks are `AWS_REGION`, then the AWS profile's Region | Not set | The AWS Region of the stage. ham signs each call for this Region. The file wins over `AWS_REGION`. |
| `aws_profile` | `ham config set aws_profile <name>` or `./setup.sh --profile <name>` | Not set. ham reads `AWS_*` credentials, then the profile named by `AWS_PROFILE`, then the default profile. | The AWS profile that signs calls. It can use static keys, `credential_process`, IAM Identity Center (SSO), or assume-role. SSO and assume-role profiles need AWS CLI version 2.9.0 or later. |
| `default_repo` | `ham config set default_repo <repository>` where the repository looks like `cc:<name>` | Not set | Fills the Repository box on the new-run screen of the terminal app. A start typed on the command line ignores it. |
| `download_dir` | `HAMH_DOWNLOAD_DIR` or `ham config set download_dir <directory>` | The directory you run ham from | Where ham saves a finished run's zip. The variable wins over the file. |
| `default_model` | `--model <alias or id>`; `HAMH_MODEL`; `ham config set default_model <alias or id>`; the `d` key in the terminal app's Model field | Claude Sonnet 5.5 | The model a run uses. The next section gives the order and aliases. |
| `update_source` | `ham config set update_source <https URL or file:/// path>`; `ham config set update_source off` | The public repository | Where `ham update` reads the newest version: the head of its main branch. `off` turns `ham update` off, and it then makes no network call. |
| `default_engine` | `--engine <name>`; `/engine <name>` in the terminal app for one session; `ham config set default_engine <name>` | cline | The coding engine a run uses. A run on the default engine sends the request that an older ham sends. `ham engine <name>` says whether an engine is available. |
| `input_wait_minutes` | `--input-wait <minutes>`; `HAMH_INPUT_WAIT`; `ham config set input_wait_minutes <minutes>` | 30 minutes in the terminal app; 0, meaning no wait, in scripts | How long a run waits when Cline needs your answer. Whole minutes from 0 to 43200. With 0, the run asks no question. |
| `mouse` | `ham config set mouse <value>` with true, false or auto; `HAMH_NO_MOUSE`; `/mouse on` or `/mouse off` inside the app | `true` (on) | Whether the terminal app asks the terminal for mouse reports. The value auto enables them in terminals known to report clicks the way ham reads them. `/mouse` changes the running session and leaves the file alone. |
| `report_tests` | `ham config set report_tests off` (or `on`); `ham config unset report_tests` | On | Turns the clean-checkout test report on or off. The report reruns the repository's own test commands in a fresh copy of the delivered code. It never changes a run's outcome label. |
| `run.report_command_minutes` | `ham config set run.report_command_minutes 10`; `ham config unset run.report_command_minutes` | The stage decides (5 minutes) | The most minutes one command of the clean-checkout test report may run before it is stopped, 0 to 465. The sentence of a stopped command says which one and how long it had. 0 uses the stage's own limit. A longer limit lengthens the whole report by the same time. |
| `run.goal` | `ham config set run.goal on` (or `off`); `--goal on` or `--goal off` for one run | The stage decides (on) | Keeps a run going until it reports done. Off asks for no loop. The flag wins over the file. The file wins over the default. |
| `run.rules` | `ham config set run.rules on` (or `off`); `--no-rules` or `--rules <dir>` for one run | On | Sets whether a run sends your rules. Off works like `--no-rules`. Off also sends no skills. The flag wins over the file. The file wins over the default. |
| `run.skills` | `ham config set run.skills on` (or `off`); `--no-skills` or `--skills <dir>` for one run | On | Sets whether a run sends your skills. Off works like `--no-skills`. The flag wins over the file. The file wins over the default. |
| `research.enabled` | `ham config set research.enabled on` (or `off`); `--research` or `--no-research` for one run | The stage decides | Sets whether a run may search the web. The stage decides while the key is unset. The flag wins over the file. The file wins over the default. |
| `ui.notify` | `ham config set ui.notify on` (or `off`); environment variable `HAMH_NOTIFY` | Off | Sends a terminal notification when a run waits for you. The window must be behind. `HAMH_NOTIFY` wins when it is set. The change applies at once. |
| `ui.bell` | `ham config set ui.bell on` (or `off`) | On | Rings the terminal bell when a run starts waiting for you. The change applies at once. |
| `ui.mascot` | `ham config set ui.mascot auto` (or `ascii`, or `off`); environment variable `HAMH_NO_MASCOT=1` | `auto` | The character beside the title on a terminal more than 30 rows tall and at least 72 columns wide, and in the empty Runs list. `ascii` draws it in plain characters for a terminal that draws block characters double wide. `off` hides it. `HAMH_NO_MASCOT=1` turns it off whatever the file says. The change applies at once. |
| `ui.mascot_motion` | `ham config set ui.mascot_motion on` (or `off`) | On | Moves the smoke under the mascot's feet a little, one step each second, in the title band of a terminal 33 rows tall or more and in the empty Runs list. Off shows the still picture. `ui.mascot = off` hides the mascot whatever this says. The change applies at once. |
| `ui.copy_limit_kib` | `ham config set ui.copy_limit_kib 256` | 64 | The most KiB one copy to the clipboard carries (the `y` key, `/copy` and a mouse selection). A longer text is refused whole and never cut; `/export` keeps it. 1 to 1048576. The change applies at the next start. |
| `run.stall_calls` | `ham config set run.stall_calls 45` | 30 | Tool calls in a row, with no changed file and no new text, that end a run. A command that is running does not count. 5 to 1000. The change applies from the next run. |
| `run.stall_window_minutes` | `ham config set run.stall_window_minutes 30` | 20 | Minutes with no changed file that end a run when at least `run.stall_min_calls` calls came in them. The time a command ran is left out. 1 to 240. |
| `run.stall_min_calls` | `ham config set run.stall_min_calls 12` | 10 | The least number of tool calls inside the stall minutes for them to count. One long build counts as one or two calls. 1 to 100. |
| `run.command_grace_minutes` | `ham config set run.command_grace_minutes 180` | 120 | Minutes with no changed file that a running command may hold a run for. `0` does not look for running commands. 0 to 1440. |
| `run.final_message` | `ham config set run.final_message on` (or `off`) | On | Gives Cline one last turn to write its final message before a stall ends the run. Off ends the run at once. |
| `run.lifecycle_flow` | `ham config set run.lifecycle_flow auto` (or `full`, or `quick`), then `ham rules init` | `auto` | Which text of the lifecycle rules file `ham rules init` installs as `10-lifecycle.md`. `auto` holds the quick path for a small change in an existing repository and the eight steps for anything else, and Cline chooses; `full` holds the eight steps; `quick` holds the quick path. An edited rules file is kept. |
| `run.review_round` | `ham config set run.review_round off` (or `auto`) | `auto` | After Cline reports done with a changed file, the harness asks it once to review its own delivery, with the prompt in `review.md`. `off` never asks and sends no review file. The change applies from the next run. |
| `run.compute` | `ham config set run.compute instances` (or `microvm`, `auto`), or `--compute <word>` for one run | `auto` | Which machine the session runs on. `microvm` is the normal session with 2 vCPU. `instances` is one EC2 machine per session with many CPUs and a large disk, where the stage has them. `auto` lets the stage decide. The first call of an `instances` session takes about a minute and a half longer. The change applies from the next run. |
| `run.stall_new_calls` | `ham config set run.stall_new_calls 150` | 100 | Between two changed files, this many tool calls that bring something new (an input no earlier call had, at least 3 lines in its answer that no earlier answer had, and the call succeeded) do not count toward `run.stall_calls` and `run.stall_min_calls`, so a run that reads a large codebase before it edits is not ended as a stall. `0` counts every call. 0 to 1000. The change applies from the next run. |
| `run.cargo_no_debug_info` | `ham config set run.cargo_no_debug_info off` (or `on`) | On | On, the session builds cargo's dev and test profiles without debug information (`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`), which keeps `target/` small on a small machine. Off leaves the setting to cargo and the repository. The change applies from the next run. |
| `run.cargo_no_incremental` | `ham config set run.cargo_no_incremental off` (or `on`) | On | On, the session builds without incremental state (`CARGO_INCREMENTAL=0`). Off leaves the setting to cargo and the repository. The change applies from the next run. |
| `run.exclude_target_dir` | `ham config set run.exclude_target_dir off` (or `on`) | On | On, the harness lists `target/` in the repository's local exclude file, so the commit it makes at the end of a run never takes up build output. Off leaves the list to the repository's own ignore files. The change applies from the next run. |
| `run.outside_checkout_note` | `ham config set run.outside_checkout_note off` (or `on`) | On | On, the goal prompts tell Cline that work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest. Off leaves that sentence out. The change applies from the next run. |
| `run.max_run_hours` | `ham config set run.max_run_hours 48`, or `--max-run-hours <hours>` for one run | 24 | The most hours one run may take in all. A session lasts up to 8 hours; a run that is not finished when its session ends continues in a new session from the work saved so far, until this time is used up. The run then stops as paused and `ham resume` continues it. 1 to 720. The flag wins over the file. The file wins over the default. The change applies from the next run. |
| `run.provider_sharing` | `ham config set run.provider_sharing on`, or `--provider-sharing on` for one run | off | Whether the prompts and answers of a run on a model that needs provider data sharing go to the model's provider, which may keep them up to 30 days for safety monitoring. A model that needs it is marked in `ham models`. With it on, `ham` also switches the account's Bedrock data retention mode in the stage's Region to `provider_data_share` before such a run when the mode does not let the model run. With the setting off, `ham` refuses such a model on the command line before it sends anything, and the terminal app asks first. The flag wins over the file. The file wins over the default. The change applies from the next run. |
| `run.max_interrupts` | `ham config set run.max_interrupts 20` | 5 | How many immediate (`/now`) messages one run may make. Each one cuts the turn that is running, so the number guards against an interrupt storm. After the last one, a `/now` message waits for the turn to end and goes out next. 0 allows none. 0 to 1000. |
| `run.max_questions` | `ham config set run.max_questions 10` | 3 | How many times one run may stop to ask you what to do. It asks only when it has an answer window. 0 means the run never asks. 0 to 1000. |
| `run.max_fresh_sessions` | `ham config set run.max_fresh_sessions 6` | 3 | How many fresh sessions you may ask for in one run. The one automatic fresh session is counted apart. 0 refuses your fresh-session answer. 0 to 1000. |
| `research.max_file_mib` | `ham config set research.max_file_mib 4` | 1 | The most MiB one file of the research material may hold. 1 to 64. |
| `research.max_unpacked_mib` | `ham config set research.max_unpacked_mib 500` | 200 | The most MiB one repository of the research material may take once unpacked. The worker also keeps free disk space, so a repository that does not fit is not placed. 1 to 1024. |
| `research.max_run_mib` | `ham config set research.max_run_mib 900` | 300 | The most MiB of research material one run may hold in all. 1 to 2048. |
| `default_backend` | `ham config set default_backend <alias>` | Not set | The backend a new run starts on when you name none: `default`, or the alias of a `[[backend]]` table. With two or more backends and no default, a start names its backend with `--backend`. [docs/user-guide.md](docs/user-guide.md) has the section Several backends. |
| `customer_content` | `ham config set customer_content on` | Off | Declares that the default backend may receive a run marked as customer content. The same key in a `[[backend]]` table declares that backend. A start marked with `--customer-content` on a backend not declared safe is refused. |
| `run.container_signals` | `ham config set run.container_signals "default; Gemfile contains testcontainers"`, or the word `none` | The built-in list | The files that show a repository starts containers in its checks (a Testcontainers dependency, a docker compose file, a `.ham/test-command` that mentions docker). The Instances machine cannot run containers, so with a signal `auto` keeps the run on the microVM and `--compute instances` is refused. One line of entries separated by semicolons; the word `default` stands for the built-in list. |
| `ui.deprecation_notices` | `ham config set ui.deprecation_notices off` (or `on`); environment variable `HAMH_NO_DEPRECATION_NOTICES=1` | On | Turning it off silences every compatibility notice. `HAMH_NO_DEPRECATION_NOTICES=1` wins over the file. The change applies at the next start of `ham`. |
| `HAMH_NOTIFY` | Environment variable | Off | Set to `1`, `true` or `on` for a terminal notification when a run waits for you and the window is not in front. When it is set it wins over the config key `ui.notify`; when it is unset, `ui.notify` decides. |
| `HAMH_NO_DEPRECATION_NOTICES` | Environment variable | Notices on | You set `HAMH_NO_DEPRECATION_NOTICES=1` to silence every compatibility notice. The variable wins over the file. The change applies at the next start of `ham`. |
| `HAMH_OPEN_COMMAND` | Environment variable | `open` on macOS, `xdg-open` on Linux | The program the `o` key runs to open a link. It gets the link as its one argument. No shell reads it. |
| `HAMH_AWS_CLI` | Environment variable | `aws` on the PATH | The AWS CLI program `ham` runs for SSO or assume-role profiles. Set it to a path or command name. The program must be version 2.9.0 or later. `ham` stops it after 30 seconds. |
| `HAMH_RANGE_BYTES` | Environment variable | 16 MiB | One download range's size, from 1,024 bytes to 16 MiB. Rarely needed. |
| `XDG_CONFIG_HOME` | Environment variable | `~/.config` | Moves the whole ham folder: the config file, the rules folder, `continue.md`, `review.md` and the skills folder. |
| `VISUAL`, `EDITOR` | Environment variables | `vi` | The editor `ham rules edit` opens: `$VISUAL`, then `$EDITOR`, then `vi`. |

### Choosing the model

ham knows six models.

- `opus` selects Claude Opus 5.5.
- `sonnet` selects Claude Sonnet 5.5, the default.
- `sol` selects GPT-6.1 Sol.
- `astra` selects GPT-6 Astra.
- `glm` selects GLM 5.3.
- `fable` selects Claude Fable 5.2. It needs provider data sharing; see Models that need provider data sharing below.

`--model` accepts an alias or a full Bedrock model id.
Aliases match in any letter case; full ids must match exactly.
[Set up, step by step](#set-up-step-by-step) lists the ids.

The order is `--model`, `HAMH_MODEL`, `default_model`, then Claude Sonnet 5.5.
`ham config set default_model sol` checks the list and stores the full id.
`ham models` lists six models offline.
It marks your next run's model with `*`.
A start prints `model: <name> (<id>), from <source>` on standard error.

The new-run screen's Model field selects a model for one run.
The `d` key saves the shown model as your default.

Bedrock is the only model provider.
Selectable models need a recorded passing check in the list.
The service refuses every other id.

### What setup.sh writes

`./setup.sh --help` lists every option.

- It installs `ham` at `PREFIX/bin/ham`. The default `$HOME/.local` prefix gives `~/.local/bin/ham`; `--prefix DIR` requires an absolute path. The full setup builds the client in your account, downloads it from your own bucket, and installs it after a SHA-256 check. It builds, checks and installs the operator tool `ham-admin` the same way, in the same folder. Setup creates the relative link `cah` to `ham` beside the installed binary.
- It uses `ham config set` for your options and typed answers. `--endpoint` sets `api_endpoint`. `--profile` sets `aws_profile`. `--region` sets `region`. `--default-repo` sets `default_repo`. Setup runs `ham config migrate` when `~/.config/cah` holds settings and `~/.config/ham` holds none.
- It never invents an endpoint. Nothing is created before you confirm the plan.
- It writes default rules through `ham rules init`, as described above.
- It runs `ham models` to show the binary works. With an endpoint set, it runs `ham doctor`.
- It reads only AWS profile section names and key names. It reads no credential values and writes no secrets. The full setup also calls the AWS CLI with your credentials. `setup.sh` never reads those credential values.
- It edits no shell startup file and deletes none of your files. Repeat runs leave finished steps alone.
- It writes a receipt folder at `~/.config/ham/setup/<account>/<region>/`. A receipt records setup identifiers and state in `direct.json`. The folder contains one log per run and the checked client. It holds no credential.
- It generates an account file from your account, Region, zones, and credential identity. Setup uploads the file to your bucket. It never writes the generated file into the clone.
- It accepts `--full`, `--client-only`, `--yes`, `--acknowledge-costs`, `--keep-runner`, `--preflight-only`, `--build-on`, `--account-id`, `--stage`, `--azs`, `--invocation-logging`, `--repo-name`, `--status`, `--doctor`, `--update`, `--revision`, `--allow-active-runs`, `--change-operator`, `--force-deploy`, `--teardown`, `--acknowledge-teardown`, `--only`, `--network-wait-minutes`, `--build`, `--from`, `--sha256`, `--endpoint`, `--profile`, `--region`, `--default-repo`, `--prefix`, `--no-input`, `--dry-run`, `--verbose` and `--help`.
- Its exit codes are: 0 done; 1 a prerequisite or step failed; 2 bad usage; 3 installed but `ham doctor` found a problem; 4 cancelled or a confirmation is missing; 5 bootstrap, deploy or initialization failed; 6 an operation is still running and safe to resume; 7 a conflicting operation or a source or configuration mismatch; 8 cleanup is incomplete. See [Setup reference](#setup-reference).

## Usage

The terminal app `ham` is the main surface. Start a run from the command line or the full-screen app, watch it, and collect the result. Chapters below cover your first run, the run screen as a chat, what to do when Cline gets stuck, and every key and message-box command. [docs/user-guide.md](docs/user-guide.md) is the full guide.

### Your first run

#### Start it

You do not need a repository with `--s3-only`. The run starts from an empty repository. It delivers an S3 zip. The start form requires `--direct`.

```bash quickstart
ham "Write a Python command-line tool that counts the words in a text file. Add tests." --s3-only --direct
```

ham prints `started run <runId>`. It then watches the run.

In a terminal, ham opens a full-screen dashboard. The dashboard shows the run status, agents table and model. Without a terminal, ham prints one line for each change. It does the same with `--json`.

Every screen of the terminal app shows your ham version and the backend version (the stage ham talks to) at the right end of its first row, for example `ham v5.NN | backend v5.NN`. `ham --version` prints your ham version. You can use `ham --help`, `ham -h` or `ham help` to list the commands. You can use `ham help <command>` or `ham <command> --help` to see one command's options.

When the run completes, ham downloads the zip. It then prints a delivery card.

Example card. The values are made up to show the shape.

```text
Run       01JAB2CDEF3GH4JKMNPQRSTVWX complete (delivered)
Outcome   Cline reported done
Model     Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)
PR        n/a
Zip       /Users/you/Downloads/s3-only-01JAB2CDEF3GH4JKMNPQRSTVWX-1a2b3c4.zip (sha256 9f8e7d6c5b4a, 48213 bytes)
Link      https://<presigned-url> (expires <time 15 minutes later>)
Clone     ham clone 01JAB2CDEF3GH4JKMNPQRSTVWX
Push      ham push 01JAB2CDEF3GH4JKMNPQRSTVWX <git-url>
```

Read the Outcome line. "Cline reported done" appears only when Cline ends its turn with a message and at least one file changed; it is not a test result or a quality verdict. Other endings read "Cline stopped: <reason>", for example "Cline stopped: it ended its turn without changing a file".

The harness records how Cline stopped. It does not judge the work. ham exits 0 for complete status and 10 to 16 for other states; 12 means blocked, with nothing delivered, and 11 (paused) and 16 (stalled) mean the work was delivered and `ham resume` continues it. Exit 0 does not mean "Cline reported done".

#### Limits that matter before you leave a run unattended

- The goal loop ends a run that makes 30 tool calls in a row with no changed file and no new text, or that changes no file for 20 minutes over at least 10 calls. With `--goal off` no such brake applies. Cancel a run you suspect with `ham cancel <runId>`.
- One engine runs today: Cline. A run cannot choose another.
- A run waits 30 minutes for your answer when you use the terminal app, and scripts do not wait. A session lasts at most 8 hours. A run that is still working when its session ends continues in a new session by itself, until it has used its total time (24 hours unless you set `run.max_run_hours`). It then stops as paused with what exists, and `ham resume` continues it.

The full list is under [Limits](#limits).

#### Use a repository instead (optional)

Use `--repo cc:<name>` for an existing CodeCommit repository. Use `--new-repo cc:<name>` to create it first.

Pull requests go only to CodeCommit repositories whose names the stage's repository names cover. The default rule is the safety prefix `hamh-e2e-` (`e2e` is short for end to end). The operator must put the repository on the stage's allowlist; see [Allow a repository for pull requests](#allow-a-repository-for-pull-requests). For `--new-repo`, you must use the name form `hamh-e2e-new-<yyyymmdd>`.

`--model` takes an id or an alias. The aliases are opus, sonnet, sol, astra and glm.

```bash
ham "Add input validation to the signup handler" --repo cc:hamh-e2e-demo --direct
ham "Build a small URL shortener" --new-repo cc:hamh-e2e-new-20261014 --direct --model opus
```

#### Allow a repository for pull requests

A run opens a pull request only in a CodeCommit repository on the stage's allowlist.
The name must be covered by the stage's repository names (default: names that start with `hamh-e2e-`).
Under the default rule, the stage refuses other names with `refused: cc:<name> is outside the hamh-e2e- prefix`.
Under another rule, the stage refuses other names with `refused: cc:<name> is outside the repository names this stage allows (<rule>)`.
The repository must be in the stage's own AWS account and Region.

**Choose the repository names.** You set the rule during the full setup:

```bash
./setup.sh --full --repo-name my-app --repo-name 'team-*'
```

You repeat the option or separate entries with commas. The rule accepts 1 to 10 entries. An entry is a repository name, a prefix that ends in `*`, or `*` alone, which covers every name. `./setup.sh --update` keeps the stage's rule. `./setup.sh --update --repo-name ...` replaces it. Setup writes the rule in the `repoNames` field of the account file. `HAMH_REPO_NAMES` takes comma-separated entries and overrides the field for one run of the deploy scripts.

The operator is the identity that runs `./setup.sh` for the stage.
Only the operator may call `ham-admin`.
Another identity receives HTTP 403 `not-an-operator`.

You use `ham-admin repos allow cc:<name>` to add an entry.
You use `ham-admin repos deny cc:<name>` to remove an entry.
You use `ham-admin repos list` to print the entries.
The command `ham-admin --version` prints `ham-admin <version>`.

The command `ham-admin` reads the same `config.toml` as `ham` for the stage endpoint `apiEndpoint`, Region and AWS profile.
It signs calls with that profile's credentials.
If `apiEndpoint` is unset, it prints `No stage URL is set.` with the command `ham config set api_endpoint <url>` and the config path, and exits 1.
It exits 0 when done.
It exits 1 when the stage refuses or fails.
It exits 2 on a usage error.

The full setup, `./setup.sh --full`, installs `ham-admin` beside `ham`.
The folder is `~/.local/bin`, or `DIR/bin` when you give `--prefix DIR`.
Setup builds both programs for your platform and installs them together.
On the runner route, it checks the SHA-256 of each file before it installs either one.
You need no Rust toolchain for `ham-admin`.
`./setup.sh --status` and `./setup.sh --doctor` report it.
`./setup.sh --update` builds and installs it again, also on another machine.

`ham-admin` signs its calls with the AWS profile in `config.toml`.
You set the profile with `ham config set aws_profile <profile>`.
The profile must sign in as the operator.
When you give `--profile` to `./setup.sh`, setup stores that profile for you.

```bash
ham-admin repos allow cc:hamh-e2e-demo
ham-admin repos list
```

#### Close the laptop, then collect the result

You can close your laptop after the run starts. Later, any machine with the same config can watch the run, download its zip and clone it. Use the run id that ham printed as `started run <runId>`. If you did not save it, `ham runs` prints one line per run, newest first, and each line starts with the run id; the first command below takes the newest one.

```bash quickstart
RUN_ID=$(ham runs | head -n 1 | cut -d ' ' -f 1)
ham watch "$RUN_ID"
ham pull "$RUN_ID"
ham clone "$RUN_ID"
```

`ham watch` ends when the run completes. It also downloads the zip. `ham pull` is only needed to resume an interrupted download, and it checks SHA-256.

`ham clone` builds a new git repository from the zip's `repo.bundle`. It checks the files against the zip's `src/` folder. It sets no remote.

To put the code in a git repository on another server, run `ham push "$RUN_ID" <git-url>`. It builds the same repository in a temporary folder and pushes the run's branch, `harness/<runId>`, with your own git credentials: your SSH key or credential helper. `ham` holds no token. It never forces a push, and it refuses a URL that has a password or token in it. Use `--branch <name>` to pick the remote branch.

In the dashboard, once the run has delivered, the card shows the download link as one short line. Press `o` to open the pull request, or the download link, in your browser, and `d` to download the zip again.

The zip holds the source files, the git bundle `repo.bundle` with the full history, checksums and EVIDENCE.md. EVIDENCE.md records how Cline stopped and says the harness made no judgment on the work. Without a test report it says the harness ran no test; with one it says the harness also ran the repository's own test commands in a clean checkout and shows their result.

`ham pr <runId>` prints the pull request link. `ham link <runId>` prints a download URL valid for 15 minutes. `ham cancel <runId>` stops Cline within about 10 seconds. It keeps the work. There is no `ham stop` command.

#### The run screen as a chat

A run opens as a chat. The conversation shows your prompt, what Cline said, each tool call and the service's notices. A message box stays at the bottom. It has the keyboard when the run opens (typing mode). Esc switches to browse mode, where letters are hotkeys, and keeps your text.

Every key has a row in [Keys and commands of the terminal app](#keys-and-commands-of-the-terminal-app), in the typing mode table or the browse mode table. `Ctrl-O` works in either mode and on the Runs list. `Ctrl-L` repaints the screen in either mode and on every screen.

A line that starts with `/` and a command word, or with `ham` and a command word, is a command and not a message. Typing `/` opens the list. Every `ham` subcommand works this way, for example `/cancel`, `/pull`, `/clone` and `/push`, with two differences: `/watch` opens the run on the screen and does not follow it in lines, and `/continue` hands the terminal to `ham continue`, which follows the new run until it ends (see [Typing /continue](docs/user-guide.md#typing-continue)). The app adds `/help`, `/new`, `/open`, `/sessions`, `/engines`, `/send`, `/now`, `/tag`, `/copy`, `/export` and `/mouse`. A command that changes a run, sends data away or edits a setting first shows a preview and waits for a second Enter. That Enter cannot follow within 0.3 seconds, and a burst of keys never confirms. `/send <text>` sends a message to a working Cline. `/now <text>` interrupts its turn and asks for the second Enter.

The mouse is on by default; clicks do what their keys do, and the wheel scrolls. On the New run screen, clicking `[ Submit ]` starts the run like Ctrl-S; only the words respond, not the blank space to their right; without a prompt, it says `a prompt is required`; while starting, it reads `submitting…` and ignores clicks. Dragging over the conversation selects text, shown reversed; letting go copies exactly that text. Holding the mouse button with the pointer still above or below the conversation keeps scrolling 3 lines every 60 milliseconds, toward older rows above and newer rows below; the selection follows, selecting every row it passes. Scrolling stops when you let go, press a key, resize the window, or move the pointer back over the conversation. The keyboard does everything the mouse does and works the same with the mouse off. `HAMH_NO_MOUSE=1` or `ham config set mouse false` turns it off. `/mouse off` turns the mouse reports off for the running session and `/mouse on` turns them back on; the config file is not changed, and `HAMH_NO_MOUSE=1` keeps them off. The first screen of a session with the mouse on shows a one-line hint on selecting text with the terminal: hold Fn in Terminal.app, Option in iTerm2 and Shift elsewhere.

The chapters are [The message box](docs/user-guide.md#the-message-box) and [Commands in the message box](docs/user-guide.md#commands-in-the-message-box).

#### When Cline gets stuck

Automatic recovery runs first. It tries provider retries, route and model switches, and one resume.

Cline may still stop or give up after recovery cannot help. Then a panel titled "Cline needs you" appears under the status block. The terminal bell rings once.

The panel shows the reason, Cline's last words and first changed files. You can answer with a key or a command.

| Key | What it does | Command |
| --- | --- | --- |
| `p` | Send a message in the same chat | `ham say <runId> '<text>'` |
| `s` | Start a fresh chat on the same files | `ham new-session <runId> ['<text>']` |
| `m` | Try another model | `ham try-model <runId> <model> ['<text>']` |
| `f` then `f` | Finish with what exists | `ham finish <runId>` |
| `Esc` | Close the typing screen and keep the question open | none |

The terminal app asks the run to wait 30 minutes for your answer. Scripts do not wait. Change the wait with `--input-wait <minutes>`.

If nobody answers, the run finishes with what exists. It delivers as usual. A run asks at most 3 questions. The key `run.max_questions` changes the number.

Recovery can also start a fresh session by itself.

Read the full chapter, [When Cline gets stuck](docs/user-guide.md#when-cline-gets-stuck-notifications-and-your-answers).

### Keys and commands of the terminal app

Run `ham` with no arguments in a terminal to open the terminal app. A run opens on its chat screen. The conversation is above, and the message box is below it. The box title says "Message to Cline".

The app has 48 key bindings. A run opens in typing mode with the message box focused. The 14 typing-mode keys work while the box has the keyboard. In typing mode a letter is text and never a command. Esc leaves typing mode and keeps your text. The 34 browse-mode keys work after you press Esc. Press `i` or Enter to start typing again. The key table in `crates/ham/src/tui/keys.rs` has 48 rows. These tables list three more chords outside that key table: `Up` on the first row of the box and `Down` on the last row walk the history in typing mode, and `/` opens the conversation search in browse mode.

The app has 34 message-box commands. Start a command with a slash at the first character of the first line, for example `/cancel`. A leading space makes the line a message to Cline. `/send` sends the rest of the line exactly as typed. Typing `/` opens a list of the commands above the box. A command that only reads runs at once. A command that changes a run, sends data off your machine, changes a security setting, hands the terminal over or starts a billable run shows a preview first and waits for a second Enter. `/mouse` says whether the mouse reports are on, and `/mouse on` or `/mouse off` turns them on or off for the session. `/tag +project:billing -tech:s3 csv-export` adds and removes tags on the run on screen, after a preview and a second Enter. `t` on the Runs list opens the tag explorer. `/settings` opens the Settings screen, also available with `s` on Runs. A run that ends as `blocked` delivered nothing and takes no messages; its message box offers `/retry`, the `r` key and Enter on your text to start it again with the same request, after a preview and a second Enter.

The tables below list every key and every command. The chapter "The terminal app" of [docs/user-guide.md](docs/user-guide.md) describes each one in full.

#### Typing mode

| Key | What it does |
|---|---|
| `Enter` | Send the text. It answers Cline's open question, goes to a working Cline as a message, prepares a follow-up on a finished run, or offers to start a blocked run again. A line that starts with a slash is a command. |
| `Shift-Enter`, `Ctrl-J` | Add a new line. `Ctrl-J` works in every terminal. `Shift-Enter` arrives only from a terminal that reports it, such as one that speaks the kitty keyboard protocol; in a plain terminal it is `Enter` and sends the text (see [Keys that depend on your terminal](docs/user-guide.md#keys-that-depend-on-your-terminal)). |
| `Alt-Enter` | Send the whole box to Cline as chat, exactly as typed: no command is read from it and nothing is trimmed. It needs a terminal that reports the key (on a Mac, Option set as Meta in Terminal.app or iTerm2); `/send <text>` works everywhere. Inside a burst it adds a new line. |
| `Esc` | Leave typing mode and keep the text. |
| `Ctrl-C` | Clear the box when it holds text. With an empty box, quit. |
| `Ctrl-R` | Search the lines you sent before. |
| `Up` on the first row, `Down` on the last row | Walk through the lines you sent, newest first, then the earlier messages of this run, then the prompts of your earlier runs. `Down` past the newest gives your draft back. |
| `Tab` | Expand the `[Pasted n lines]` placeholder under the caret. |
| `Ctrl-G` | Edit the draft in your editor (`$VISUAL`, else `$EDITOR`). |
| `Ctrl-L` | Repaint the whole screen, on every screen and in either mode. Use it when part of the screen was not drawn again after you switched to another app and back. While you prepare a follow-up on a finished run, it also adds Cline's last message at the end of the box, marked as written by Cline and untrusted and cut to 1,500 characters. A second press adds nothing. |
| `Ctrl-Z`, `Alt-Z` | Undo and redo an edit. |
| `Ctrl-U` | Does nothing. |
| `Ctrl-O` | Open the box that searches your runs and their conversations. |

#### Browse mode

| Key | What it does |
|---|---|
| `i`, `Enter` | Start typing in the message box. |
| `Esc` | Go back to the Runs screen. |
| `r` | On a blocked run, offer to start it again with the request it was started with; press `r` again, 0.3 seconds or more later, to start it. On any other run, go back to the Runs screen. |
| `R` | Show the preview of `ham resume --all`, with the number of paused and stalled runs in your list, then press `R` again, 0.3 seconds or more later, to start a follow-up run for each of them. The panel reports what each run did. |
| `n` | Open the New run screen. |
| `q` | Quit. |
| `Up`, `Down`, `j`, `k` | Scroll the conversation one line. |
| `PgUp`, `PgDn` | Scroll one page. |
| `Home`, `g` | Go to the top. |
| `End`, `G` | Go to the bottom and follow the end. |
| `Tab`, `Shift-Tab` | Select the next or the previous block of the conversation. |
| `Space` | Open or fold the selected block. |
| `/` | Open the search bar for the conversation. After `Enter` accepts the search, `n` goes to the next match and `N` to the previous one; `Esc` clears the marks. |
| `a` | Open the Artifacts screen. |
| `c` | Show the preview of `/cancel`, then press `c` again, 0.3 seconds or more later, to cancel the run. A second `c` that comes sooner is ignored, and the preview says it came too soon. Any other key withdraws the preview. A run that has ended has nothing to cancel, and `c` says so. |
| `C` | Clone the run's repository, the same way as `ham clone <runId>`; it fetches the zip itself when it is not on this machine. |
| `o` | Open the pull request, or else the download link, in your browser. |
| `d` | Download the zip again. |
| `y` | Copy the selected block. With none selected, copy the last answer of Cline. |
| `p` | Send a message to the same Cline session while a "Cline needs you" question is open. |
| `s` | Start a fresh Cline session on the same files while a question is open. |
| `m` | Pick another model for Cline while a question is open. |
| `f` | Press it twice to finish the run with what exists, while a question is open. |
| `w` | Withdraw the chosen message. With none chosen, withdraw the newest message that nobody has claimed. |
| `x` | Show a preview that confirms a held message, or one that may have reached Cline. |
| `h` | Choose the next message that waits for confirmation. After the last one, go back to the first. |
| `Ctrl-O` | Open the box that searches your runs and their conversations. |

#### Commands in the message box

| Command | What it does |
|---|---|
| `/runs` | List your runs, newest first |
| `/watch` | Follow a run to its end (in the app: open the run) |
| `/cancel` | Ask Cline to stop; the work so far is kept |
| `/models` | List the models you can pick (--server: the stage's list) |
| `/pr` | Print the pull request link of a run |
| `/pull` | Download a run's result to this machine; a stopped download resumes |
| `/clone` | Make a local git repository from a run's zip |
| `/push` | Push a run's code to another git repository with your own git login; with no URL, the preview has a slot for it |
| `/link` | Make a fresh download link for a run's zip (15 minutes) |
| `/doctor` | Check your identity, the API, the model list and the allowlist |
| `/rules` | Your rules files: write the defaults, edit, show |
| `/config` | Show or change ham's settings |
| `/engine` | Show or change the settings file of an engine |
| `/say` | Answer Cline's open question with a message |
| `/new-session` | Answer with a fresh Cline session on the same files |
| `/try-model` | Answer by switching to another model the question offers |
| `/finish` | Answer by finishing the run with what exists |
| `/continue` | Start a new run from the code a finished run delivered. A second Enter hands the terminal to `ham continue`: your editor opens for the prompt when you typed none, and the run starts at once when you did. It goes where the old run went. Use `/continue --run <runId>` for another run (see [Typing /continue](docs/user-guide.md#typing-continue)). |
| `/resume` | Continue paused and stalled runs from the code they delivered: `/resume <runId>` or `/resume --all`; a preview comes first |
| `/lifecycle` | Copy the lifecycle pack (AGENTS.md and skills) into a repository |
| `/update` | Update the backend and ham to the newest version, after your runs end. It runs from a shell (`ham update`); the message box answers with that line |
| `/help` | List the commands, or explain one |
| `/new` | Open the New run form |
| `/open` | Open a run's chat view |
| `/send` | Send text to the engine as chat, exactly as typed |
| `/now` | Send a message that interrupts Cline's current turn |
| `/tag` | Add or remove tags on this run |
| `/retry` | Start the blocked run on screen again with the same request |
| `/sessions` | Pick one of your earlier runs from a list |
| `/engines` | List the engines and which one is selected |
| `/export` | Write the chat to a file |
| `/copy` | Copy the last answer of Cline |
| `/mouse` | Say whether the mouse reports are on, or turn them on or off for this session (`/mouse on`, `/mouse off`) |
| `/settings` | Open the Settings screen: every config.toml key, changed in place |

## Frequently asked questions

These answers explain the project's value, limits, features, and differences.

### Value

**Does setup deploy AWS infrastructure?**

Yes. Setup is the single entry point.
On a terminal, its first question asks whether you want the full setup (the backend and the client) or client-only setup, and `--full` or `--client-only` skips that question.
The full setup installs a runner, an AWS CodeBuild project, in your AWS account.
It builds and deploys the backend there with CDK and CloudFormation.
It builds the client there, downloads it from your own bucket and configures it.

The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.

`./setup.sh --teardown` removes the stage.
`--status` reports it.
`--update` deploys a new revision.
The other flags and the exit codes are in [Setup reference](#setup-reference).

**What do I get from one run?**

`ham`, the terminal app, sends your prompt unchanged to Cline, the coding engine.
You receive an S3 zip containing source, a git bundle, EVIDENCE.md, and checksums.
CodeCommit repositories also receive a pull request.

**Who is this for?**

- A developer with an AWS account who uses a Mac or Linux terminal.
- You want a coding task to keep running while your laptop is closed.
- You want Amazon Bedrock models and code returned as a zip or pull request. Only CodeCommit repositories receive pull requests.
- You run `./setup.sh`. The full setup builds the backend and `ham` in your AWS account. See [Set up, step by step](#set-up-step-by-step).
- The full setup deploys the stage for you. A stage that someone else runs works with `./setup.sh --client-only --build --endpoint <stage-URL>`.

**What does it cost?**

- Your AWS account pays for the stage and for every run. Part of the stage costs money even with no run active.
- See [Costs](#costs) for the items, example prices and the limits that bound a run.
- `./setup.sh --teardown` removes the stage.

**How long can a run last?**

A session lasts at most 8 hours, and its prompt has a deadline of 27,900 seconds, or 7 hours 45 minutes.
At that deadline a run that has time left continues in a new session from the work saved so far.
A run lasts at most 24 hours in all unless you set `run.max_run_hours` (1 to 720) or pass `--max-run-hours`.
At that time the service stops the run as paused and delivers existing work. `ham resume` continues a paused run.
A run waits 30 minutes for your answer in the terminal app.
Read [Limits](#limits) before leaving a run unattended.

**What is the license?**

The code in this repository is MIT-0, the MIT No Attribution license. Cline is not part of the repository: the build fetches it from upstream at a pinned commit and applies this project's patches. See [License](#license-and-attribution).

**Is it ready for production use?**

No. This is sample code for informational and educational purposes only, and the notice at the top of this README asks you to work with your security and legal teams before any deployment.
Start with [Set up, step by step](#set-up-step-by-step).

### Problems it solves

**Will closing my laptop stop work or lose my files?**

No. The run continues on AgentCore while your laptop is closed.
`ham watch` rebuilds the run state when you return.
Snapshots save changed files every 10 minutes while Cline works.
If a session is lost, the service delivers the last snapshot.

**Can the agent work without holding long-lived keys or package tokens?**

- STS, AWS short-lived credential service, supplies per-session credentials created outside the microVM. A microVM is a small isolated virtual machine.
- No long-lived key or GitHub token enters the microVM.
- A package proxy supplies Python, npm, and Maven packages through CodeArtifact, AWS package service. Go modules come from the stage's own Go module store.
- Cline never holds the CodeArtifact token.
- See [Safety and secrets](#security) for the boundaries.

**What does the stop message actually tell me?**

Each run ends with "Cline reported done", "Cline stopped: <reason>" or "Cline did not start".
"Cline reported done" means Cline ended its turn with a message.
The label also requires at least one changed file.
The harness records how Cline stopped; this gives no quality verdict.

**What happens when a run gets stuck or a provider fails?**

- The service retries timeouts and can switch to another model route.
- "Cline needs you" appears when automatic recovery cannot help. Press p to say something or s to start a fresh session. Press m to try another model or f twice to finish.
- The goal loop ends a run that makes 30 tool calls in a row with no changed file and no new text, or that changes no file for 20 minutes over at least 10 calls. With `--goal off` no such brake applies. Cancel a run you suspect with `ham cancel <runId>`.
- Read [Limits](#limits) before leaving a run unattended.

**Can Cline follow my engineering rules?**

Your editable markdown rules go with every run.
Use `ham rules init`, `ham rules edit`, and `ham rules show`.
Cline writes and runs its own tests under your rules.
`ham rules init` also installs the lifecycle pack: one rules file and seven skills for research, context, tdd, build-test, loop, delivery and check. `ham rules init --no-skills` installs the rules file alone.
Cline's goal loop is on by default and keeps queueing follow-up prompts until Cline calls its goal-complete tool; `--goal off` turns it off.

**What happens to my existing `cah` install, settings and stage?**

- `setup.sh` installs the alias `cah`. The alias prints one notice on standard error and behaves exactly as `ham`. Alias support ends with version 5.50.
- A documented variable that starts with `CAH_` remains a fallback when the variable with the same name and the prefix `HAMH_` is unset. Different values for both spellings of an account, account-file or owner selector stop the command with exit 2. `./setup.sh` treats the earlier spelling of a stage setting as the `HAMH_` name and also stops with exit 2 when you set both spellings to different values. The variable fallback ends with version 5.50. Use the `HAMH_` spelling.
- The folder `~/.config/cah` remains readable when it holds your content and `~/.config/ham` holds none. This fallback ends with version 5.50. You copy your settings with `ham config migrate`.
- Repositories with names starting with `cah-e2e-` remain accepted as pull request targets through their ownership checks; on a new stage you allow each one with `ham-admin repos allow cc:<name>`. A repository uses `.cah/` when it holds state and `.ham/` does not. A run refuses to start or continue when both folders hold state. These fallbacks end with version 6.00.
- A stage deployed under the earlier names keeps running. `ham` still talks to it. `./setup.sh --update` and `./setup.sh --full` refuse it with exit 7. You install the new client against its endpoint. You run `ham pull` for each run you want to keep. You run `./setup.sh --teardown`. You deploy a fresh stage with `./setup.sh --full`. The fresh stage starts empty. Teardown support for the earlier stage ends with version 6.00. See [Moving from the earlier command name](docs/user-guide.md#moving-from-the-earlier-command-name).

### Features

**Which models can I choose?**

Six Amazon Bedrock models: Claude Opus 5.5, Claude Sonnet 5.5 (the default), GPT-6.1 Sol, GPT-6 Astra, GLM 5.3 and Claude Fable 5.2.
Choose one in the terminal app, with `--model`, or with `HAMH_MODEL`. Use `ham config set default_model` to set your default.
Bedrock is the only model provider.

**How do I get the code back?**

- `ham pull` downloads the S3 zip, resumes downloads and checks SHA-256.
- `ham clone` builds a local git clone. `ham link` gives a 15-minute download link.
- `ham pr` prints the CodeCommit pull request link. Only CodeCommit repositories receive pull requests.
- GitHub and GitLab delivery are not supported. `ham push <runId> <git-url>` uses your credentials to push elsewhere.
- See [Features](#feature-and-capability-catalog) for each command.

**Can I steer a run or continue after it finishes?**

Use `ham say`, `ham new-session`, `ham try-model`, or `ham finish` during a run.
`ham continue <runId> '<prompt>'` starts a new run from delivered code with full history.
`ham resume <runId>` and `ham resume --all` continue paused and stalled runs from the code they delivered.

**Can Cline research the web?**

Cline can search the web and download public GitHub repositories.
A new stage needs no key: it searches through AgentCore Web Search, a search service of AWS.
A query can hold at most 200 characters, and page fetch answers `disabled`.
If the stage owner stores a Tavily key, the stage uses Tavily first, and page fetch works.
The stage notices a stored or cleared key within 5 minutes, with no deploy and no restart.
An error never sends a query to the other provider.
To return a stage to AgentCore Web Search, the operator runs `python3 scripts/research-key-put.py --stage direct --clear`.
The script takes the stage's account, Region and AWS CLI profile from an account file, as the deploy scripts do.
After a full setup on the default route your machine has no such file: setup uploads the generated one to the runner's bucket and removes that bucket with the runner unless you give `--keep-runner`.
Copy `accounts.json` and keep one entry for your AWS account, with `role` set to `active`, the fields described in [Accounts and Regions](#accounts-and-regions) and your AWS CLI profile as `profile`.
Run the script with `HAMH_ACCOUNTS_FILE=<absolute path of that file>`.
With `./setup.sh --build-on local`, the generated file stays in the receipt folder, `~/.config/ham/setup/<stage account>/<stage-region>/`, as `accounts-local.json`.
To use one provider only, the operator sets `HAMH_RESEARCH_PROVIDER` to `tavily` or `agentcore` for the deploy.
The key never enters the session.
The firewall allowlist stays unchanged.

**Does the Tavily key expire?**

A key made in the Tavily dashboard does not expire. It works until someone deletes it in the dashboard. Tavily advises rotating a key about every 90 days. The
harness keeps the key in AWS Secrets Manager. Only the research Lambda can read it. A stage that can call Tavily has four CloudWatch alarms on the stored key. The
default stage can. One fires when the key is 30 days old. The other three fire when Tavily refuses the key, limits the rate of calls, or reports that a usage limit
is reached. The deploy setting `HAMH_TAVILY_KEY_MAX_AGE_DAYS` changes the age. `HAMH_RESEARCH_ALARM_TOPIC_ARN` sends the alarms to an SNS topic.
`HAMH_TAVILY_ALARMS=off` builds none. A new stage searches through AgentCore Web Search until a Tavily key is stored. It needs no key, so no key can expire. It
searches only and cannot fetch a page. `python3 scripts/research-key-put.py --stage direct --clear` returns a stage to it. The old key stops working only when
someone deletes it in the Tavily dashboard. `python3 scripts/research-key-put.py --stage direct --rotate` checks a new key with Tavily, stores it only if Tavily
accepts it, and waits for the stage to use it. On an Enterprise plan, `--rotate-enterprise` makes the new key through Tavily and deactivates the previous one.

**How many runs can operate at once?**

- Each run gets its own microVM. The product states no maximum number of runs at once; your account's AgentCore and Bedrock quotas apply.
- AgentCore (Amazon Bedrock AgentCore, the AWS service that hosts the agent sessions) allows 5,000 active session workloads per AWS account.
- AgentCore also allows 25 new sessions per second. Both AgentCore limits are adjustable.
- Model token quotas bind first. Only the account owner changes a Bedrock quota. See [Limits](#limits) for the scale boundaries.

### How it differs

**How does this differ from running Cline in my terminal?**

The same pinned Cline build runs remotely, in its own isolated microVM for each run, in your AWS account.
You gain these things around it:
- The run continues with the laptop closed.
- The work comes back as an S3 zip with a git bundle and, for CodeCommit, a pull request.
- Snapshots, retries, model rotation and the "Cline needs you" correction keys cover failures.
- Credentials are short-lived and the network allows only listed hosts.

**How does this differ from a hosted coding-agent service?**

You deploy this service into your own AWS account.
Bedrock is the only model provider.
The pinned Cline build has telemetry compiled out.
The firewall allows only listed hosts.

**What does this add to AgentCore?**

- AgentCore hosts the sessions. Step Functions, the AWS workflow service, coordinates each run.
- The project adds per-session credentials and a network allowlist.
- It adds a package proxy and snapshots.
- Delivery includes a zip and CodeCommit pull request. Recovery and correction tools help with interrupted runs.
- The terminal app gives you access to these tools. See [Features](#feature-and-capability-catalog) for each feature.

**How does this differ from a multi-agent framework?**

One prompt reaches one Cline session unchanged.
The harness has no planner or roles.
Cline decides whether to start helpers itself.
Engines like Cline decide when the work is done.
The harness records how Cline stopped.

**Can it run other coding engines?**

Two engines run, Cline and OpenCode.
See [Harnesses and engines](#harnesses-and-engines) for the engine design.
## Models and accounts

Amazon Bedrock is the only model provider. A stage is one deployment of the service, whose AWS account pays the bill. This section covers the six models and the account a stage uses. [Costs](#costs) covers what the stage and its runs cost, and the limits that bound a run.

### The six models

All six models use the Amazon Bedrock Converse API. Their inference Region is `us-east-1`. A call can also reach the other Regions named under [Accounts and Regions](#accounts-and-regions).

A second route is the model's other inference profile (the `us.` or the `global.` kind).

| Alias | Name | Bedrock model id | Context window (tokens) | Max output (tokens) | Second route | Default |
|---|---|---|---|---|---|---|
| `opus` | Claude Opus 5.5 | `us.anthropic.claude-opus-5-5` | 1,000,000 | 128,000 | `global.anthropic.claude-opus-5-5` | no |
| `sonnet` | Claude Sonnet 5.5 | `global.anthropic.claude-sonnet-5-5` | 1,000,000 | 128,000 | `us.anthropic.claude-sonnet-5-5` | yes |
| `sol` | GPT-6.1 Sol | `us.openai.gpt-6.1-sol` | 1,000,000 | 131,072 | `global.openai.gpt-6.1-sol` | no |
| `astra` | GPT-6 Astra | `us.openai.gpt-6-astra` | 1,050,000 | 128,000 | `global.openai.gpt-6-astra` | no |
| `glm` | GLM 5.3 | `us.zai.glm-5.3` | 1,048,576 | 128,000 | none | no |
| `fable` | Claude Fable 5.2 | `global.anthropic.claude-fable-5-2` | 1,000,000 | 128,000 | `us.anthropic.claude-fable-5-2` | no |

A run uses Claude Sonnet 5.5 when you set nothing. GLM 5.3 defaults to high reasoning effort, like the three Claude models. GPT-6.1 Sol and GPT-6 Astra default to xhigh reasoning effort. `ham`, the terminal client, prints GLM 5.3's note under the `ham models` table.

Run names, the labels of 5 to 7 words in the Runs list, and run tags come from a separate small model, Claude Haiku 5.5, which is not one of the six and which you cannot choose; Cline keeps the model you chose.

The model list is `engine/catalog/models.json`. The list is compiled into `ham`. It is also compiled into the control API, the AWS Lambda function accepting your requests.

A smoke record is a stored result of short Bedrock checks. A model needs a passing smoke record before you can pick it. A model without one appears as `pending` in `ham models`. The maintainers record smoke results in their own AWS accounts, so a record does not show that your account can invoke the model. You follow the access steps in "Before you start" under [Set up, step by step](#set-up-step-by-step).

The service refuses an unlisted model with HTTP 422 and `unknown-model`. It refuses a listed model without a passing record with HTTP 422 and `model-not-smoked`.

Each second route has its own smoke record. A run never names that route. `agentd`, the agent supervisor inside the microVM, may switch to it after route failure. A microVM is a small isolated virtual machine. GLM 5.3 has no second route.

Provider failover resumes the same Cline session after provider failure. It then rotates routes and models. When every route is down, it waits and sends small probes. A run pays for tokens from each model it uses.

Changing the list requires edits to `engine/catalog/models.json` and the two tests that pin the list. The change also requires rebuilding `ham` and redeploying the control API. Users need the new `ham` build. `ham models --server` reports differences between stage and client defaults.

### Models that need provider data sharing

Some models run only for a run that agrees to share its data with the model's provider. Claude Fable 5.2 (`fable`) is one. `ham models` marks such a model with "needs provider data sharing".

With the setting `run.provider_sharing` on, the prompts and answers of a run on that model go to the model's provider, which may keep them up to 30 days for safety monitoring. The setting is off by default. While it is off, `ham` refuses the model on the command line before it sends anything, and the service refuses a start request that does not carry `providerSharing: true`. In the terminal app, a run on such a model with the setting off asks first: the question names the model, what sharing means, and the AWS account and Region it changes, and only `y` starts the run.

Turn it on for every run with `ham config set run.provider_sharing on`, in the Settings screen, or for one run with `--provider-sharing on`. The setting is your agreement: with it on, `ham` asks nothing.

The AWS account has its own half of the agreement. Its Bedrock data retention mode in the stage's Region must let the model run: `provider_data_share` or `aws_review`. In any other mode Bedrock refuses the call with a message that ends "data retention mode 'default' is not available for this model". When a run agrees and the mode is `default` or `inherit`, `ham` switches it to `provider_data_share` with your own credentials before it starts the run, reads it back, and says so in one line. The switch covers every Bedrock call of the account in that Region and stays after the run; the line names the command that sets it back. Your credentials need `bedrock:PutAccountDataRetention`. When the switch is denied or the mode does not read back as one that lets the model run, `ham` starts nothing and says why in one message. `ham` changes no other mode, and changes nothing when it cannot read the mode or when your credentials belong to another account than the stage. The account owner can also set the mode, for the whole account in that Region:

```bash
aws bedrock put-account-data-retention --region us-east-1 --mode provider_data_share
```

`ham doctor` shows the setting and the mode in its `provider-data-sharing` line. [docs/user-guide.md](docs/user-guide.md) has the details.

### Pick a model

A run uses the first value set in this list:

1. The `--model <alias|id>` flag.
2. The `HAMH_MODEL` environment variable.
3. `default_model` in the config file, set with `ham config set default_model <alias|id>`.
4. The built-in default, Claude Sonnet 5.5.

A blank value counts as unset. A wrong value produces an error naming its source. There is no fallback from a wrong value to another layer. Aliases ignore letter case. Full model ids must match exactly.

`ham models` lists the models offline without credentials. It marks the next run's model with `*`. `ham models --server` prints the stage's list.

The terminal app's New run screen has a Model field. Arrow keys pick a model. The key `d` saves the shown model as your default.

```bash
ham models
ham "Add input validation to the signup handler" --s3-only --direct --model opus
HAMH_MODEL=astra ham "Add input validation to the signup handler" --s3-only --direct
ham config set default_model sol
```

When Cline needs your answer, `ham try-model <runId> <model>` switches models. The terminal app's key `m` does the same.

### Accounts and Regions

The stage is named `direct`. It uses one AWS account, one Region, and one Availability Zone.

Workers call Amazon Bedrock with the stage's own AWS account. The six models are listed with `us-east-1`. A worker reaches Bedrock in the stage's own Region through an interface VPC endpoint, and in the other two of `us-west-2`, `us-east-1` and `us-east-2` through the Network Firewall allowlist. A stage deploys only in `us-east-1` or `us-west-2`.

The full setup leaves `accounts.json` unchanged. It writes an account file for your account. The environment variable `HAMH_ACCOUNTS_FILE` supplies its absolute path. The deploy scripts, the CDK app, the Rust build scripts, and the nested CodeBuild packaging all use that file in place of `accounts.json`. When `HAMH_ACCOUNTS_FILE` is unset, they read the committed file.

`accounts.json` at the repository root lists the accounts the project may use. Only one entry may have the role `active`. To use your own account, either change the active entry to your account, or add an entry for it and select it with `HAMH_AWS_PROFILE`.

| Field | Meaning |
|---|---|
| `account` | The 12-digit AWS account id, for example `111122223333` |
| `region` | The home Region of the stage, for example `us-east-1` |
| `profile` | The AWS CLI profile name for this account. It is also the value that selects the entry |
| `role` | `active` (exactly one entry), `history` (an account that holds records already written; chosen only on purpose) or `test` (an account for deploys and live tests; never the default and never the active entry) |
| `azs`, `azIds` | The Availability Zone names and the matching AZ ids, listed for each stage name in the file. Stage `direct` uses one zone. A zone name is a different physical zone in each account, and the AZ id is what AgentCore accepts, so the deploy checks the names against the ids |
| `invocationLogging` | `enable-and-restore` (the deploy turns Bedrock model invocation logging on when it is unset, and `./setup.sh --teardown` and `scripts/teardown.sh` put the saved setting back) or `read-only` (scripts read the setting and never write it) |
| `operator` | Optional: `{"kind": "role" \| "user", "name": "<IAM name>"}`. Names the one identity that runs setup and operates the stage. The name has 1 to 64 letters, digits, or `+ = . @ _ -`. Without this field, the stage keeps the IAM role named `Admin`. An IAM Identity Center role is named `AWSReservedSSO_<permission set>_<hash>` |
| `repoNames` | Optional list of 1 to 10 repository names, prefixes ending in `*`, or `*`. The default is `["hamh-e2e-*"]`. `setup.sh --repo-name` writes it. `HAMH_REPO_NAMES` overrides it for one run. |

The stage trusts exactly the operator identity wherever it otherwise trusts the role `Admin`: the operator API routes, the deploying principal, the session minters, and the Go module store. `setup.sh` writes the field from your credentials using `aws sts get-caller-identity`. A role session becomes a role. An IAM user becomes a user. The root user and federated users are refused. The deploy script's caller check accepts the operator identity and the setup runner's role. It refuses every other caller with `DEPLOY-REFUSED reason=caller`, which names the identity it expects. The operator needs `sts:AssumeRole` in its own policy.

`HAMH_AWS_PROFILE` selects an entry by its `profile` value. An unset or empty value selects the active entry. The committed active entry uses `us-east-1`.

`AWS_PROFILE` never selects the entry. Scripts set it from the selected entry. An unknown profile stops the script before any AWS call. A malformed file also stops it before any AWS call. Both errors use `aws_target: <reason>` and exit code 2.

This example uses a placeholder profile name:

```bash
HAMH_AWS_PROFILE=my-stage-profile scripts/deploy.sh --stage direct --azs 1
```

Deploy scripts, teardown scripts, and the CDK app read `accounts.json`. The stage's service programs are built with that selection. They carry that selection. Build the `ham` client with `HAMH_AWS_PROFILE` unset. If `HAMH_AWS_PROFILE` names no `accounts.json` entry, `cargo build` stops with exit 101.

The client ignores the selection used when it was built. It signs each call using its config file's endpoint, Region, and profile. A `--repo cc:<name>` target names an AWS CodeCommit repository. That repository uses the stage's own account and Region. The stage decides these, not the machine that built `ham`.

The caller must have credentials for the target account. Wrong-account deploy credentials cause `DEPLOY-REFUSED reason=account`. Wrong-account credentials cause `TEARDOWN-FAIL account` in `scripts/teardown.sh`. A zone name mismatching its AZ id causes `DEPLOY-REFUSED reason=azs`.

Scripts act only on resources with this project's names and tags. Another project's untagged resources are outside their scope. The API accepts only IAM identities from the stage's account.

The client setting `api_endpoint` is the stage's API address. `region` selects its Region. `aws_profile` selects the profile used to sign requests. Set these with `ham config set <key> <value>`. One example is `ham config set aws_profile my-profile`. All three settings must belong to the same deployment.

## Costs

Your AWS account pays for everything a stage creates and for every run. The full setup (`./setup.sh`) creates AWS resources after you confirm the deployment, and its "What it costs" screen lists the charges without amounts. Client-only setup creates no AWS resources. It installs `ham` and writes a config file. Billable runs start when someone submits a prompt.

### What the idle stage costs

These items cost money while the stage exists, even with no run active:

- NAT gateway: hourly and per GB.
- Network Firewall endpoint: hourly and per GB.
- Interface endpoints: hourly for each endpoint and zone. They give the sessions and Lambda functions private access to AWS.

The idle stage costs roughly $13 per day (about $390 per 30 days) in `us-east-1` or `us-west-2` before any run, mostly for the Network Firewall endpoint. In `us-east-1` and in `us-west-2` the prices are the same: 1 AWS Network Firewall endpoint at $0.395 per hour, 1 NAT gateway at $0.045 per hour, 10 interface VPC endpoints at $0.01 per hour each and the NAT gateway's public IPv4 address at $0.005 per hour. Together they cost $0.545 per hour, $13.08 per day or $392.40 per 30 days before any run. The Network Firewall pricing page says standard NAT gateway per-hour and data processing usage charges are waived when the NAT gateway sits next to a Network Firewall in the same Region and networking path, as in this stage. With the waiver the total is $0.50 per hour, $12.00 per day or $360.00 per 30 days, and the public IPv4 address stays in both totals as a separate item under Amazon VPC pricing. The stage has three customer-managed KMS keys at $1 per key per month. Setup creates a `CDKToolkit` bootstrap stack with a fourth customer-managed KMS key in an account that has no CDK bootstrap stack. Data processing, CloudWatch Logs, S3, DynamoDB, ECR, CodeBuild minutes, AgentCore sessions and Bedrock tokens are extra. These example prices come from October 2026 and change, so you check the AWS pricing pages. Tear the stage down when you do not need it.

### What runs and storage cost

Watch Amazon Bedrock model tokens first. Token use grows with run count and run length. Every run is one Cline session; [Cost guardrails](#cost-guardrails) gives the longest it can last. When all routes are down, each probe requests 16 output tokens.

The usage field in `summary.json` shows tokens. The final card shows tokens when the stage reports usage. Bedrock invocation logging records calls when enabled. The Bedrock invocation logs are the token record of a run.

The card's Tokens/USD line shows stage-reported tokens and US dollar cost. Its dollar figure uses a price table pinned in control API code. The table covers Claude Opus 5.5, GPT-6.1 Sol, GPT-6 Astra, Claude Fable 5.2, and Claude Sonnet 5.5. GLM 5.3 has no price row, so its USD figure reads `unknown`. The line is absent when the stage reports no usage. Read your AWS bill for the charged amount.

Amazon Bedrock AgentCore uses one microVM per run, alive for the whole session. Its idle timeout is 30 minutes.

**Storage and requests**

| Service | Cost driver |
|---|---|
| S3 | Bundles, snapshots, zips, evidence; object versions |
| DynamoDB | On-demand billing; point-in-time recovery |
| KMS | Keys and requests |
| CloudWatch Logs | 30-day retention |
| CloudTrail | Trail data events |

S3 stores files for every run. The bucket keeps object versions.

**Per-run and deploy charges**

| Service | Cost driver |
|---|---|
| Step Functions Standard | Up to 20 states per run |
| Lambda | Per-run service charges |
| CodeArtifact | Package storage and requests |
| CodeBuild | ARM image builds during deploy |

CodeArtifact repositories cache public sources for npm, PyPI, Maven and crates.

### Cost guardrails

**Runs have no per-run token or dollar limit, and `ham` has no budget option. The controls below bound runtime. Set budget caps and spend alerts in your AWS account.**

- **Run deadline.** Each prompt has a limit of 27,900 seconds, or 7 hours 45 minutes, and the microVM limit is 28,800 seconds, or 8 hours. When the limit comes and the run has time left, Cline stops, the service saves the work and starts a new session from it, with a new Cline chat that is told where the work stands. A run lasts 24 hours in all unless `run.max_run_hours` or `--max-run-hours` says otherwise, up to 720 hours; at that time the service delivers the saved work and the run is paused. The Step Functions workflow limit is 2,628,000 seconds, or 178 hours.
- **Silence limit.** After 900 seconds without Cline output, the prompt ends. The service delivers saved work. Waiting and probing during a full route outage keep this clock alive. The run deadline still ends that wait.
- **Answer window.** The terminal app requests up to 30 minutes for your answer. Scripts get 0 minutes. `--input-wait <minutes>` accepts 0 to 43200. You can also use `ham config set input_wait_minutes <n>`. The environment variable `HAMH_INPUT_WAIT` sets the same window. The window ends at least 15 minutes before the run deadline. A run asks at most 3 questions. Without an answer, the run finishes and delivers existing work.
- **Cancel.** `ham cancel <runId>` stops Cline within about 10 seconds. It keeps the work. In the dashboard, press Esc and then `c` twice.
- **Quotas.** Bedrock model throughput is an account setting. AgentCore session quotas are also account settings. Only the account owner changes a quota. Report measured throttling numbers to the account owner. The account owner decides what to change.
- **Account check.** `scripts/deploy.sh` and `scripts/teardown.sh` stop when credentials belong to another account. `./setup.sh` stops with exit 7 when `--account-id` names another account than your credentials. The teardown plan names the account before it deletes anything.
- **Teardown.** `./setup.sh --teardown` removes the stage and the runner with your own AWS credentials. Its `--dry-run` option previews removal. It asks once for confirmation; see [Teardown](#teardown). Every AWS resource has the tag `project=hyperscale-agent-meta-harness`. `scripts/teardown.sh` is the tag-based teardown program for people who work on the repository. Its `--dry-run` option previews removal with read calls, changes require `--yes`, and successful removal ends with a zero-resource inventory. A retained interface VPC endpoint is billed by the hour until you delete it; [Teardown](#teardown) says which network resources teardown can retain.
- **Goal loop.** The goal loop, Cline's follow-up prompts, ends a run when it stalls.

Concurrent runs are bounded by your account's Amazon Bedrock and AgentCore quotas and by the Region; see [Limits](#limits).

## Deploy the AWS infrastructure

`setup.sh` deploys the stage. Run `./setup.sh` for the full setup. It installs a runner in your AWS account that runs the deploy engine, `scripts/deploy.sh`. `./setup.sh --teardown` removes the stage with the AWS CLI on your machine. `scripts/teardown.sh` is a separate program for people who work on the repository.

A stage is a named set of AWS resources. The only stage is `direct`. The stage creates billable resources; [Costs](#costs) lists them. Tear it down when unused.

Both scripts first check the selected account and change nothing otherwise. The deploy ends in `DEPLOY-PASS` or `DEPLOY-FAIL`, or in `DEPLOY-REFUSED` when it refuses before its first stack change and leaves the stage unchanged.

The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.

The runner needs no additional toolchain installed on your machine. Run `./setup.sh --build-on local` to run the same engine on your machine when it has the toolchain listed under [Prerequisites](#prerequisites).

### Setup reference

**Commands**

Run `setup.sh` from the repository root.

| Command | What it does |
|---|---|
| `./setup.sh` | Asks whether to deploy the backend and configure the client, or configure only the client. Full setup is the default answer. |
| `./setup.sh --full` | Deploys the backend in your AWS account, then installs and configures `ham`. |
| `./setup.sh --client-only` | Installs and configures `ham` for an existing backend. |
| `./setup.sh --preflight-only` | Reads and reports the account, zone and source commit. Creates nothing. On a deployed stage, exits 0 without installing or configuring the client. |
| `./setup.sh --status` | Reports the deployed stage, running build, last setup result and whether the client fits the stage. Exits 0 when nothing is wrong, 5 when the stage is broken or incomplete or the last setup failed, 6 when a build is running or the stage is being changed, 7 when the stage was deployed under the earlier names, and 8 when resources were retained. Also exits 1 when the AWS CLI is missing or too old or the identity call fails, and 2 when no Region is found or the Region is neither `us-east-1` nor `us-west-2`. |
| `./setup.sh --doctor` | Reports PASS, WARN and FAIL checks for the AWS CLI, stage, endpoint, any remaining runner and the client's own doctor. Exits 0, or 3 when a check fails. Also exits 1 when the AWS CLI is missing or too old or the identity call fails, and 2 when no Region is found or the Region is neither `us-east-1` nor `us-west-2`. |
| `./setup.sh --update` | Updates the backend and the client. Builds and deploys the clone's `HEAD`. Use `--revision ID` to select a full 40-digit commit ID. An update keeps the stage's `owner` tag, its operator and every stage setting it has unless you export a new value. When the stage already runs the commit, the update only gets the client. Active runs stop `./setup.sh --update` with exit 7 before anything is installed or built; `--allow-active-runs` skips that check. An update takes about 20 to 25 minutes when it reuses the Cline binary from the earlier build, and about 21 to 27 minutes more when the Cline binary must be rebuilt. |
| `./setup.sh --teardown` | Deletes the stage and runner. The run exits 9 if only network resources held by an AgentCore network interface remain, and 8 if anything else remains or a read fails. `--network-wait-minutes` (0 to 480, or `full`) makes it wait for AWS to release that interface. |

**Options**

The runner is a CodeBuild project in your AWS account.

| Group | Option | Meaning |
|---|---|---|
| Choose the mode | `--full` | Select full setup without the mode question. |
| Choose the mode | `--client-only` | Select client-only setup without the mode question. |
| Full setup | `--keep-runner` | Keep the runner after successful setup. |
| Full setup | `--acknowledge-costs` | Acknowledge resource charges. Required with `--yes` for full setup. |
| Full setup | `--preflight-only` | Read and report setup inputs without creating resources. |
| Full setup | `--build-on WHERE` | Choose `runner` (default) or `local`. Local deployment needs cargo, cargo-zigbuild, zig, node and docker on PATH, and a C compiler (`cc`). |
| Full setup | `--account-id ID` | Require credentials for the specified account. A mismatch exits 7. `--teardown --yes --acknowledge-teardown` requires it. |
| Full setup | `--stage NAME` | Select the stage. Only `direct` is supported. |
| Full setup | `--azs N` | Select the number of Availability Zones. Only `1` is supported. |
| Full setup | `--invocation-logging MODE` | Choose `read-only` (default) or `enable-and-restore`. The latter requires `--build-on local`. |
| Full setup | `--repo-name NAME` | You choose the stage's repository names. You repeat the option or separate up to 10 entries with commas. The default is `hamh-e2e-*`. The option also applies to `--update`, where it replaces the rule. |
| Later life | `--status` | Report stage, build, setup and client state. Exit codes: 0, 5, 6 and 8 as listed under Commands. |
| Later life | `--doctor` | Run AWS and client diagnostic checks. |
| Later life | `--update` | Build and deploy a revision of this clone, and install its client. Runs in progress block the update: setup exits 7 and lists the runs. |
| Later life | `--revision ID` | Select a full 40-digit commit ID for `--update`. The default is `HEAD`. An update needs it to deploy a commit older than the one the stage runs, and to deploy any commit when the stage records a commit that your clone does not hold and whose `VERSION` is not lower than the clone's. |
| Later life | `--allow-active-runs` | Skip the early active-run check of `--update`. With active runs, the build refuses only an update that changes the worker runtime. |
| Later life | `--force-deploy` | With `--update`, deploy even when the stage already runs the commit. |
| Later life | `--change-operator` | With `--update`, make your identity the stage's operator. Without it, an update by an identity other than the recorded operator exits 7 before it creates anything. |
| Later life | `--teardown` | Delete the stage and runner. AgentCore network interfaces can retain network resources for up to 8 hours. |
| Later life | `--acknowledge-teardown` | With `--yes` and `--account-id ID`, skip the teardown confirmation. Without `--account-id`, the teardown exits 4. |
| Later life | `--only legacy\|current` | With `--teardown`, limit the plan to one generation: `legacy` is a stage deployed under the earlier names and `current` is a stage under the current names. The default covers every generation found. |
| Client without the full setup | `--from PATH` | Install from a directory containing `ham` and `SHA256SUMS`, or from a single `ham` file with `--sha256`. A checksum mismatch stops installation. |
| Client without the full setup | `--sha256 HEX` | Supply the checksum for a single client file. |
| Client without the full setup | `--build` | Compile `ham` from a checkout with `cargo build --release --locked -p ham --bin ham`. Requires Rust 1.98.1 through rustup. |
| General | `--prefix DIR` | Set an absolute installation prefix. The default is `$HOME/.local`, which installs `~/.local/bin/ham`. |
| General | `--endpoint URL` | Set the stage URL. Use the `V1ApiUrl` output of `hamh-direct-control`, which ends in `/direct`. |
| General | `--profile NAME` | Select the AWS profile. |
| General | `--region NAME` | Select the AWS Region. Full setup supports `us-east-1` and `us-west-2`. |
| General | `--default-repo REPO` | Set the optional default repository. |
| General | `--yes`, `-y` | Accept confirmation prompts. Full setup also requires `--acknowledge-costs`. This option alone selects client-only setup. |
| General | `--no-input` | Disable interactive input. Without `--yes`, the full setup exits 4 and creates nothing, and the client-only setup exits 2 unless you give `--dry-run`. |
| General | `--dry-run` | Show the intended actions. Creates and writes nothing, runs no `ham` command and uses read-only AWS calls. |
| General | `--verbose` | Show the whole build log while the build runs. Without it, the runner route shows the marker lines of the build, and the local route leaves out the CDK notices that repeat for every stack. The setup log keeps every line. |
| General | `--help`, `-h` | Show command help. |

With no `--from` or `--build`, client-only setup uses `ham` in `PREFIX/bin` or on PATH. Supplying `--from`, `--build` or `--endpoint` also selects client-only setup unless you choose a mode explicitly.

**Environment variables**

| Variable | Meaning |
|---|---|
| `HAMH_SETUP_AUTHORITY_HOURS` | Runner role assumption window in whole hours: 5 to 72, default 12. It must outlast the build by an hour (see `HAMH_SETUP_BUILD_MINUTES`). Invalid values exit 2 before resource creation. |
| `HAMH_SETUP_BUILD_MINUTES` | The longest the build in your account may run, in whole minutes from 30 to 480. Default: 240. A longer build raises the least value of `HAMH_SETUP_AUTHORITY_HOURS` to the build's whole hours plus one. Left out, a runner that is still installed keeps its value and a new runner uses the default; the plan shows the value. Invalid values exit 2 before resource creation. |
| `HAMH_SETUP_BUILD_COMPUTE` | The size of the machine that builds in your account: `large` (default), `xlarge` or `2xlarge`. A larger machine shortens the Rust compile. Left out, a runner that is still installed keeps its size and a new runner uses the default; the plan shows the size. Invalid values exit 2 before resource creation. |
| `HAMH_SETUP_STACK_LOOKS` | Number of checks of a stack being changed by another operation before exit 6. Default: 120, with one check every 15 seconds. |
| `HAMH_SETUP_POLL_SECONDS` | Pause between build checks. Default: 15 seconds. |
| `HAMH_SETUP_POLL_RETRIES` | Consecutive failed build checks before exit 6. Default: 20. |
| `HAMH_ACCOUNTS_FILE` | Absolute account-file path used by deploy scripts, CDK, Rust build scripts and nested CodeBuild packaging. When unset, readers use the committed `accounts.json`. |
| `HAMH_OWNER` | A nonempty value overrides the account entry for the stage's `owner` tags. Deploy scripts and the CDK app read it from their process environment. `scripts/node.sh` passes it into the CDK container. It accepts 1 to 64 letters, digits or `+ = . @ _ -`. An invalid value stops the CDK app and deploy scripts with a message naming `HAMH_OWNER`. A deployed stage keeps its owner. When neither `HAMH_OWNER` nor the entry's `owner` is set, a deploy uses the owner the stage has. A deploy that names a different owner stops before it changes anything. With `--build-on local`, `setup.sh` checks this before it creates anything and exits with code 7. The runner that `setup.sh` starts in your account does not receive `HAMH_OWNER`, and `setup.sh` warns when your value differs from the stage's owner. To use another owner, remove the stage with `./setup.sh --teardown` and set it up again. |
| `HAMH_REPO_NAMES` | A nonempty, comma-separated list overrides the `repoNames` of the account entry for one run of the deploy scripts or the CDK app. It sets the repository names in which a run may open a pull request. |
| `HAMH_AWS_CLI` | AWS CLI path for SSO and assume-role profiles. |

Setup reads `AWS_PROFILE`, `AWS_REGION` and AWS credential environment variables as the AWS CLI reads them.

**Exit codes**

| Code | Meaning |
|---|---|
| 0 | Complete. |
| 1 | A prerequisite or step fails. |
| 2 | Invalid usage. |
| 3 | The client is installed, but `ham doctor` reports a problem. |
| 4 | You cancel, or a required confirmation is missing. Examples: full setup with `--yes` without `--acknowledge-costs`, full setup with `--no-input` without `--yes`, or a teardown with `--yes --acknowledge-teardown` and no `--account-id`. |
| 5 | Bootstrap, deployment or initialization fails. After `The stage is unchanged.`, the deploy refused for a reason that waiting does not cure. |
| 6 | An operation continues and is safe to resume. `--status` also returns it while a build is running or the stage is being changed. |
| 7 | A conflicting operation, source mismatch or configuration mismatch blocks setup. Examples include a dirty clone, account mismatch, active runs during an update, a deploy lock that another setup holds, a commit older than the stage's, a stage commit that your clone does not hold and whose VERSION is not lower than yours, another operator without `--change-operator`, or a result or endpoint mismatch. |
| 8 | Cleanup is incomplete. A stack, a retained resource, the runner, another resource of the stage, a failed step or a failed read remains. |
| 9 | Teardown removed everything except the network resources that an AgentCore-managed network interface holds. Run it again after AWS releases the interface. |

**Local files**

The receipt is a local cache of setup identifiers and state.

| Location | Contents |
|---|---|
| `~/.config/ham/setup/<stage account>/<stage-region>/` | Receipt folder. Uses `$XDG_CONFIG_HOME` when set. |
| `direct.json` in the receipt folder | Receipt with mode `0600`. Contains the commit, build ID, endpoint and state. Contains no credentials or passwords. |
| `logs/setup-*.log` in the receipt folder | One log per setup run. |
| The folder `verified` inside the folder `download` of the receipt folder | Checked client and its `SHA256SUMS`. |
| `accounts-local.json` in the receipt folder | Generated account file for the local build route. |
| `aws-config` in the receipt folder | Local-route AWS configuration when credentials come only from AWS environment variables. |
| `config/accounts-*.json` in your runner bucket | Generated account file for the runner route. Its filename contains the first 12 hexadecimal characters of its SHA-256 digest. |
| `~/.config/ham/config.toml` | Client settings written through `ham config set`: `api_endpoint`, `aws_profile`, `region` and `default_repo`. |

**The generated account file and the operator**

Setup writes one active account entry with your account ID, Region, profile and operator identity. It includes `azs` and `azIds` for `live`, `drill` and `direct`, with up to three zones for `live` and one zone each for `drill` and `direct`. Invocation logging defaults to `read-only`. Setup adds `repoNames` only when you give `--repo-name`, and an update keeps the rule the stage has. An update takes the operator and the zone from the stage settings record. The runner profile is `hamh-setup`; the local route uses your profile. Setup supplies this file through `HAMH_ACCOUNTS_FILE`. The committed `accounts.json` remains unchanged. See [Accounts and Regions](#accounts-and-regions).

The optional `operator` field contains `kind` (`role` or `user`) and the IAM identity's `name`. Setup derives it from `aws sts get-caller-identity`. The stage trusts that identity for operator routes and deployment. The operator needs `sts:AssumeRole` in its own policy. An entry without `operator` uses the IAM role `Admin`. An update keeps the recorded operator, or the operator its control function trusts for a stage without a record; when you run it with another identity, it needs `--change-operator` and shows `operator: <old> -> <new>` in the plan, or exits 7 before creating anything.

**Updating the stage and the client**

`./setup.sh --update` is the one way to update the backend and the client, and it works from a fresh `git clone` of this repository. An update keeps every setting the stage has unless you export a new value. `HAMH_<NAME>=default` returns a setting to its default.

**What a second machine needs**

Run `./setup.sh --update` from a clean clone to get a client on another machine. When the stage already runs the clone's commit, the update only gets the client. A clone older than the stage's commit stops the update with exit 7, and the message names `git pull`; `--revision` deploys an older commit on purpose. A stage commit that your clone does not hold goes ahead when its `VERSION` is lower than the clone's. Otherwise the update stops with exit 7, and the message names a new clone or `git pull` when the stage runs a later release than your clone, and `--revision` when the VERSION is the same or cannot be read. An update takes about 20 to 25 minutes when it reuses the Cline binary from the earlier build, and about 21 to 27 minutes more when the Cline binary must be rebuilt. The receipt is a cache. AWS holds the setup state, so a fresh machine recovers without copying the receipt.

**Resuming**

An interrupted setup exits 6. Run `./setup.sh` again. Setup attaches to the build it started and never starts a second one.

When you run `./setup.sh` again on a deployed stage, it deploys nothing again. It points the client at the stage's endpoint. It installs the client it downloaded earlier. Use `--update` when that client copy does not exist on this machine. Setup removes a runner left behind unless you give `--keep-runner`.

On a deployed stage, `--preflight-only` prints what it found and exits 0 without installing or configuring anything.

### What the deploy does, in order

The deploy uses the repository's committed HEAD.
Its flow is in `scripts/lib/deploy.py`.

1. **Check flags and files.** Bad flags stop the deploy. Uncommitted changes to tracked files also stop it. Commit first; these checks happen before any AWS call.

2. **Check the account and guard.** The caller must match the selected `accounts.json` account, or the deploy refuses with `DEPLOY-REFUSED reason=account`. The Availability Zone must match its recorded AZ id, or the deploy refuses with `DEPLOY-REFUSED reason=azs`; `10.50.0.0/16` must overlap no account VPC, or the deploy refuses with `DEPLOY-REFUSED reason=vpc-cidr`. Nothing is built before these pass; a new worker version requires no RUNNING run on `hamh-direct-run`, the run machine, or the deploy refuses with `DEPLOY-REFUSED reason=guard wait=yes`. A refusal leaves the stage unchanged and exits 3.

3. **Build the code.** One `cargo lambda` call builds five Lambda programs and three delivery programs for arm64, in a separate account-and-Region build directory. These are `worker-api`, `control-api`, `run-session`, `research-api`, `research-job`, `repo-gateway`, `bundle-validator`, and `packager`. Before upload, each binary must match the selected account and Region, or the deploy stops with `DEPLOY-FAIL build-selection`; `pnpm install --frozen-lockfile` installs the CDK app's packages.

4. **Read the stack order.** The deploy runs `cdk synth`. It reads the stack order from the CDK manifest. It then checks the CDK bootstrap, described under Setup configuration.

5. **Create the image build resources.** A stage without a worker image repository needs a first pass. This pass deploys every stack without images. It creates repositories, roles, the harness bucket, and the CodeBuild project.

6. **Get the Cline binary.** The deploy checks the pinned Cline binary in the stage bucket. The binary must be available before the first image build. The subsection below explains the known blocker for new accounts.

7. **Build the images.** The worker program `agentd` is cross-built for arm64 with `cargo zigbuild` and uploaded to the stage bucket. CodeBuild ARM, an AWS build service on Arm machines, builds the worker image tagged `in-<image key>`. It builds the delivery image the same way, tagged `dl-<delivery key>`; existing registry tags are reused.

8. **Deploy each stack again.** The second pass runs `cdk deploy <stack> --exclusively` in dependency order. It uses worker and delivery images pinned by digest. Each stack publishes outputs that later stacks read.

9. **Check the worker runtime.** Its `live` endpoint must be READY, use VPC mode and platform V1, have `requireMMDSV2` true, and serve the new image. The deploy creates `v<N>`, the endpoint for version N; it removes older endpoints except the rollback target and endpoints used by running runs. It then handles Bedrock invocation logging as configured, without overwriting an existing destination.

10. **Record the result.** The deploy writes `/hamh/direct/deployment-digest`, `/hamh/direct/deployed-source-tree`, `/hamh/direct/image-digest`, and `/hamh/direct/worker-revision` to SSM Parameter Store, the AWS parameter service. It saves CDK asset objects in `s3://<harness bucket>/deploy/direct/cdk-assets.json` for teardown. It writes a deploy record file and prints `DEPLOY-PASS`.

The deploy stores the worker revision in SSM as `/hamh/direct/worker-revision`.
The worker revision is `HAMH_WORKER_REVISION`.
It starts at 0.
`--bump-worker-version` adds 1 to it.
That makes a new runtime version from the same source.

The deploy guard also applies when the worker image changes.
It compares that image with the one the `live` endpoint serves.
The rollback target is the highest `v<M>` endpoint below N.

A repeat deploy skips image builds when its images already exist.
A first deploy into an empty account takes longer.
It must build the images.

### Commands and flags

```bash
# Deploy: the engine that the runner runs; run it yourself with the tools on PATH (see Prerequisites)
scripts/deploy.sh --stage direct --azs 1

# Check the account and the deploy guard only; deploys nothing
scripts/deploy.sh --stage direct --azs 1 --check-guard

# Preview a teardown (reads only), then run it
scripts/teardown.sh --stage direct --dry-run
scripts/teardown.sh --stage direct --yes
```

The full usage lines are:

```text
scripts/deploy.sh --stage direct [--azs 1] [--bump-worker-version] [--check-guard] [--fork-source auto|build|copy]
scripts/teardown.sh --stage direct [--e2e-only] [--dry-run] [--yes]
```

| Script | Flag | What it does |
|---|---|---|
| `deploy.sh` | `--stage direct` | Required. `direct` is the only stage. |
| `deploy.sh` | `--azs 1` | Optional. The stage uses one Availability Zone, so `1` is the only accepted value. |
| `deploy.sh` | `--bump-worker-version` | Adds 1 to the stage's worker revision. A CloudFormation environment change then makes a new worker runtime version from the same source. The deploy guard applies. |
| `deploy.sh` | `--check-guard` | Checks the account and the deploy guard and stops. Prints `DEPLOY-GUARD PASS stage=direct running=0` and exits 0 when no run is running; otherwise prints the `DEPLOY-FAIL guard` line and exits 1. |
| `teardown.sh` | `--stage direct` | Required. |
| `teardown.sh` | `--dry-run` | Makes read calls only. Prints `teardown: plan ...` for each change a real run would make. |
| `teardown.sh` | `--yes` | Required for any change. Without it, and without `--dry-run`, teardown exits 2 before any AWS call. |
| `teardown.sh` | `--e2e-only` | Limits teardown to the stage's end-to-end test runs and their test CodeCommit repositories. It touches no stack. |

Both scripts use these exit codes:

- 0 means pass.
- 1 means a failed step.
- 2 means a bad flag or an unknown profile.
- Deploy also exits 2 for a dirty tree.
- Teardown also exits 2 for a change without `--yes`.

Any stage name other than `direct` is a usage error.
Both scripts exit 2 for that error.

### Prerequisites

**Where the deploy engine runs**

With the full setup, the deploy engine runs in a CodeBuild project (Linux x86_64) in your account.
This project is the setup runner.
You install no additional tools for this route.
The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.

With `--build-on local`, the deploy engine runs on your machine.
Your machine needs `cargo`, `cargo-zigbuild`, `zig`, `node`, and `docker` on PATH.
`setup.sh` names any missing programs and stops.
With `--build-on local`, your machine also needs a C compiler and linker named `cc` on PATH. On macOS, `xcode-select --install` installs one. On Linux, install `gcc` with your package manager. `setup.sh` does not check for `cc`, so a missing one appears as an error of the build.

The local route and the runner use the pinned toolchain.
`tools.lock` pins x86_64 Linux downloads.
The deploy engine needs outbound HTTPS.
Install the tools at the versions in `tools.lock` and `rust-toolchain.toml`:

- Rust 1.98.1, with the targets listed in `rust-toolchain.toml`.
- `cargo-lambda` 1.9.2.
- `cargo-zigbuild` 0.23.4.
- `zig` 0.16.0.
- AWS CLI 2.37.2.
- Docker.
- Python 3.11 or newer.
- `git`.

The `agentd` cross-build checks `zig`, `cargo-zigbuild`, and `rustc` versions.
A wrong version stops the deploy with exit 2.

`HAMH_PYTHON` selects the Python interpreter.
Otherwise, wrappers try `python3.12`, `python3.11`, and `python3`.

For the deploy, Docker runs only the pinned Node container.
`scripts/node.sh` runs Node, `pnpm`, and the CDK command line there.
`tools.lock` pins the `node:22` image by digest.
The first run installs `pnpm` 12.6.0 and `aws-cdk` 2.1143.0.
It installs them into the tools directory.
It checks their registry integrity values against `tools.lock`.
This needs outbound access to the npm registry.
Never run `npm` or `npx` directly on the host.

CodeBuild ARM in your account builds worker and delivery images.
Local Docker does not build those images.
`scripts/codebuild-run.sh` zips `git archive HEAD`.
It uploads that source zip to the stage bucket.
It then starts the build.

The tool, scratch, and record paths are default directories of the deploy engine.
For the local route, set these variables to directories you control:

- `HAMH_TOOLS`.
- `NODE_SH_TMP`.
- `CODEBUILD_TMP`.
- `DEPLOY_RECORD_DIR`.
- `TEARDOWN_EVIDENCE_DIR`.

The Setup configuration table explains each variable.

**AWS identity**

Use credentials for the target account's selected AWS CLI profile.
The `accounts.json` entry names that profile.
`scripts/node.sh` reads credentials with `aws configure export-credentials --profile <your-profile>`.
SSO and role profiles work after you sign in.

The deploying identity is the operator named in the account file.
See [Accounts and Regions](#accounts-and-regions).
Setup writes the `operator` field from your credentials.
The identity can be an IAM role, an IAM Identity Center role, or an IAM user.
Without an `operator` field, the deploying identity must be the IAM role `Admin`.

The Go module store permits only the operator identity to delete objects or change retention.
Teardown needs that access.
The security stack's second test principal trusts that identity.
The Go module filler role also trusts that identity.
Operator API routes accept only that identity's sessions.

The deploy creates these AWS resources:

- IAM roles and KMS keys.
- A VPC with Network Firewall.
- API Gateway, Lambda, and Step Functions.
- CodeBuild, CodeArtifact, and ECR.
- DynamoDB, S3, and SSM.
- CloudWatch Logs and Amazon Bedrock AgentCore resources.

The account and Region need a CDK bootstrap.
The deploy creates one when none exists.
Setup configuration explains how it handles an existing bootstrap.

The account needs no CloudTrail trail slot.

A stage deploys in `us-east-1` or `us-west-2`. The CDK app stops for any other Region. Amazon Bedrock AgentCore must be available in your chosen Region.

### Setup configuration

`accounts.json` and `HAMH_AWS_PROFILE` select the target account.
The committed file sits at the repository root.
The full setup generates an account file for your account.
`HAMH_ACCOUNTS_FILE` supplies its absolute path to the deploy scripts, the CDK app, Rust build scripts, and nested CodeBuild packaging.
When `HAMH_ACCOUNTS_FILE` is unset, these readers use the committed `accounts.json`.
Every Rust program reads the selected account file when built; a `HAMH_AWS_PROFILE` value naming no entry stops the build of `ham`.
`ham` ignores account selection when running; build it with `HAMH_AWS_PROFILE` unset.

`HAMH_AWS_PROFILE` selects an entry by its `profile`.
Unset or empty selects the entry whose role is `active`.
`AWS_PROFILE` never selects an entry.
An unknown profile or malformed file prints `aws_target: <reason>`.
The script then exits 2.

Keep exactly one `active` entry.
To run the deploy scripts yourself, the smallest edit adds your account to the committed `accounts.json` as a `test` entry.
Select it with `HAMH_AWS_PROFILE=<your-profile>`.
A `test` entry is selected only by name.
It is never the default.
Commit that change before deploying.
The deploy refuses a dirty tree.

The deploy tools use default directories and create the ones that are missing. To use other directories, the command below sets four variables to writable directories: `HAMH_TOOLS` for tools, `NODE_SH_TMP` for Node container scratch space, `CODEBUILD_TMP` for making the CodeBuild source zip, and `DEPLOY_RECORD_DIR` where deploy writes its record. The teardown command has one such variable: `TEARDOWN_EVIDENCE_DIR`, where teardown writes its record.

```bash
HAMH_AWS_PROFILE=<your-profile> \
HAMH_TOOLS=<tools-dir> NODE_SH_TMP=<scratch-dir> \
CODEBUILD_TMP=<source-zip-dir> DEPLOY_RECORD_DIR=<record-dir> \
scripts/deploy.sh --stage direct --azs 1
```

Editing `accounts.json` changes the worker image key.
You rewrite `engine/policy/image-key.pin` in the same commit with `scripts/image-key-guard.sh --update`.

Each account entry has these fields:

| Field | Meaning | Allowed values |
|---|---|---|
| `account` | The AWS account the stage deploys into. The caller must be signed in as this account's operator identity: the entry's `operator`, or the IAM role named `Admin` when `operator` is absent. | 12 digits |
| `operator` | Optional. The identity the stage trusts for its operator routes and deploys. Without this field, the stage keeps the IAM role named `Admin`. An IAM Identity Center role is named `AWSReservedSSO_<permission set>_<hash>`. The operator needs `sts:AssumeRole` in its own policy. | `{"kind": "role" \| "user", "name": "<IAM name>"}`. The name contains 1 to 64 letters, digits or `+ = . @ _ -`. |
| `owner` | Nonempty `HAMH_OWNER` overrides this optional stage label. A deployed stage keeps its `owner` tag. A deploy that names another owner stops before it changes anything, because a changed tag replaces the network stack's `AWS::Route53Resolver::ResolverQueryLoggingConfig` and the update rolls back. | A string of 1 to 64 letters, digits or `+ = . @ _ -`. |
| `repoNames` | Controls which repositories a run may open a pull request in. The operator still allows each repository. The default is `["hamh-e2e-*"]`. `HAMH_REPO_NAMES` overrides it for one run. An invalid value stops the deploy scripts with a message naming the entry. | A list of 1 to 8 entries: a repository name, a prefix that ends in `*`, or `*`. |
| `region` | The home Region of the stage. | `us-east-1` or `us-west-2`. The CDK app builds the templates for these two Regions only. It stops building the templates for any other Region. |
| `profile` | The AWS CLI profile that reaches the account. `HAMH_AWS_PROFILE` selects an entry by this name. | A profile name, unique in the file |
| `role` | `active`, `history` or `test`. Exactly one entry is `active`; it is the default. | `active`, `history`, `test` |
| `azs` | Availability Zone names for the stage `direct`, which is the stage the product deploys. The keys `live` and `drill` are optional. | Names in the entry's Region |
| `azIds` | The AZ ids that match `azs` one to one. AgentCore accepts AZ ids, and the same AZ name is a different physical zone in each account. The deploy checks the zone mapping in your account, and a mismatch refuses the deploy before any change with `DEPLOY-REFUSED reason=azs` and exit 3. | For example `use1-az1`, `usw2-az1` |
| `invocationLogging` | What the deploy may do about Amazon Bedrock model invocation logging in the account. `read-only`: read the setting and never write it. `enable-and-restore`: when logging is unset, the deploy saves the prior state in SSM and turns logging on for the stage's log group. `./setup.sh --teardown` and `scripts/teardown.sh` put the prior state back before the first stack is deleted, when the setting still names the stage's log group. A destination that is already set is never overwritten. | `read-only`, `enable-and-restore` |

The committed entry maps the stage's one AZ to `use1-az1` in `us-east-1`; look up your account's matching AZ id with `aws ec2 describe-availability-zones`.

**CDK bootstrap**

The deploy reads `/cdk-bootstrap/hnb659fds/version` from SSM.
A bootstrap meeting the synthesized app's required version stays unchanged.
A lower version stops the deploy before it changes anything, with `DEPLOY-REFUSED reason=bootstrap`.
An existing `CDKToolkit` is never modified.
When none exists, the deploy runs `cdk bootstrap`.
It uses the selected account and Region.
It adds `project=hyperscale-agent-meta-harness` and `stage=shared` tags.

**Script environment variables**

| Variable | Read by | Meaning |
|---|---|---|
| `HAMH_AWS_PROFILE` | every script | Selects the `accounts.json` entry by profile name. Unset selects the `active` entry. |
| `HAMH_PYTHON` | script wrappers | The Python interpreter, version 3.11 or newer. |
| `HAMH_AGENTCORE_STORAGE_ACCOUNTS` | `infra/lib/stacks/network.ts` | This variable is optional. Without it, the VPC's S3 gateway endpoint policy admits managed session-storage buckets in the home Region only for requests from the AgentCore service principal. When you set it, the policy uses bucket-owner account conditions for the Regions you name and keeps the default for other Regions. You use comma-separated entries in the format `<region>=<account>[+<account>]`. You find the account in the `acr-storage-<account>-<region>-an` bucket name in the worker VPC's Route 53 Resolver query log. `scripts/deploy.sh` passes the variable to the CDK app. `setup.sh` passes it to the deploy on either route when you export it, and an update keeps the value the stage has. |
| `HAMH_TOOLS` | `node.sh`, `cargo-slot.sh` | The tools directory holds the `pnpm` and CDK install and cargo build locks. An unset variable uses the default directory. Without `flock`, as on stock macOS, `cargo-slot.sh` runs the command at once and says so. |
| `CODEBUILD_MAX_WAIT_S` | `codebuild-run.sh` | The maximum build wait, in seconds. The default is 18000 (5 hours). When the wait ends, the script exits 1. The build keeps running in CodeBuild. |
| `CODEBUILD_MAX_ERRORS` | `codebuild-run.sh` | The number of consecutive failed status checks that ends the wait. The default is 30. |
| `NODE_SH_TMP` | `node.sh` | Scratch and cache directory for the Node container. A default directory is used when it is unset. |
| `CODEBUILD_TMP` | `codebuild-run.sh` | Where the source zip for a CodeBuild run is made. A default directory is used when it is unset. |
| `DEPLOY_RECORD_DIR` | `deploy.py` | Where the deploy writes its record files. A default directory is used when it is unset. |
| `TEARDOWN_EVIDENCE_DIR` | `teardown.py` | Where teardown writes its record. A default directory is used when it is unset. |

The scripts create missing directories named in the last four rows.
Point those variables at writable places.

### How the Cline binary reaches a new account

Every worker image carries one Cline binary.
`engine/lock.toml` pins the SHA-256 of the official build in `binarySha256` and names its `artifact` location.

Before the first image build, the deploy settles where the binary comes from.
`scripts/deploy.sh --fork-source auto|build|copy` chooses. The default is `auto`.

1. **Present.** `s3://<stage bucket>/engine/cline/<binarySha256>/cline` exists and its SHA-256 matches the pin (its object metadata, or else its bytes). The deploy uses it.

2. **Recorded.** An earlier source build in this stage left a record for the same Cline version, patch series and build tools (`engine/cline/by-tree/<tree>-<tools>.json` in the stage bucket). The deploy uses that build again. It never builds the same version twice in one stage.

3. **Copied.** The lock's `artifact` belongs to an account listed in `accounts.json`, and the machine that runs the deploy has the AWS CLI profile of that account. The deploy copies the object with that profile and checks its SHA-256. A copy that fails stops the deploy; it does not turn into a build.

4. **Built from source.** None of the above applies. CodeBuild ARM builds the Cline fork from the pinned upstream source and the patches in `engine/cline/patches` (`images/codebuild/cline-fork-arm64.yml`). Builds are not bit-for-bit reproducible, so the new binary has other bytes than the official one, and the deploy does not compare its hash with the pin. It checks what the build itself proved: the pinned Cline commit and patch series, the pinned build tools, an ARM binary, telemetry compiled out, the build's own tests passed, the start-up check reports the pinned version, the output belongs to the build the deploy started, and the hash the build computed equals the hash of the downloaded bytes. The deploy stores the binary under its own SHA-256, records it, and builds the image with it. The deploy record keeps the official hash as a reference next to it.

`--fork-source build` skips the first and third case. Use it to check that a deploy in a new account needs nothing from another account. `--fork-source copy` never builds.

The pin changes only through a promotion.
A promotion is a maintainers' code change.
It is not a deploy option.
"Promoting a series" in `engine/cline/AGENTS.md` describes it.
A fresh account does not need a promotion: the source build above gives it its own binary.

### What gets deployed

The deploy creates nine CloudFormation stacks named `hamh-direct-<name>`.
It deploys them in dependency order from the CDK manifest.
A fixed priority list settles ties.
The order is network, security, data, packages, control, runtimes, orchestration, delivery, observability.

Every taggable resource and every stack of the stage carries these four tags:
`project=hyperscale-agent-meta-harness`, `stage=direct`, `managed-by=cdk` and an `owner` tag.
The owner value is nonempty `HAMH_OWNER`, else the entry's `owner`, else the `name` of its `operator`, else `hamh-user`.
`setup.sh` writes `operator` from the caller identity of `aws sts get-caller-identity`.
`scripts/teardown.sh` finds resources by the tags `project=hyperscale-agent-meta-harness` and `stage=direct` together with the name prefix. `./setup.sh --teardown` finds the stage's stacks by the name prefix `hamh-direct-` together with those two tags, and a VPC left by an earlier teardown by those two tags.

| # | Stack | What it provides |
|---|---|---|
| 1 | `hamh-direct-network` | The VPC `10.50.0.0/16` in one Availability Zone, one NAT gateway, an AWS Network Firewall with an egress allowlist, interface VPC endpoints, an S3 gateway endpoint whose policy admits only the stage's own buckets and the AgentCore session storage buckets, and Route 53 Resolver query logging |
| 2 | `hamh-direct-security` | The KMS data key, the key that signs engine policy, the runtime base role, the `harness-worker-session` role that run-session turns into short-lived session credentials, the Bedrock logging role and a second test principal |
| 3 | `hamh-direct-data` | The `hamh-direct-harness` DynamoDB table (one global secondary index and a stream), the `hamh-direct-harness-<account id>` S3 bucket, and the Bedrock invocation log group `/hamh/direct/bedrock-invocations` with its own KMS key |
| 4 | `hamh-direct-packages` | A CodeArtifact domain with four store repositories (npm, PyPI, Maven and crates) behind one aggregate repository `packages`, and the Go module store, an S3 bucket `hamh-direct-gomod-<account id>` that starts empty |
| 5 | `hamh-direct-control` | The private `/internal` API behind `worker-api`, the public `/v1` REST API `hamh-direct-v1` with IAM authentication behind `control-api`, and the research route (a table, an empty Tavily secret, an AgentCore Gateway for web search, and the `research-api` and `research-job` Lambdas) |
| 6 | `hamh-direct-runtimes` | The worker image repository `hamh-direct-worker-python`, the CodeBuild ARM project `hamh-direct-image-build`, and, once an image exists, the AgentCore runtime with its `live` endpoint |
| 7 | `hamh-direct-orchestration` | The `run-session` Lambda, the Step Functions Standard machines `hamh-direct-run` and `hamh-direct-run-ended`, and the EventBridge rule that starts the second machine when a run's execution ends early |
| 8 | `hamh-direct-delivery` | The delivery image repository `hamh-direct-delivery`, the three container-image Lambdas `repo-gateway`, `bundle-validator` and `packager`, and the CodeCommit pull-request check |
| 9 | `hamh-direct-observability` | A Logs Insights saved query that sums token use from the Bedrock invocation log |

The Python worker is the stage's one AgentCore runtime.

### Verify the deploy

A good deploy ends with:
`DEPLOY-PASS stage=direct digest=<12 characters> sourceTree=<12 characters>`.
It exits 0.

Before that result, the deploy prints these lines:

- `deploy: runtime <runtime id> live=<version> vpc=ok platform=V1 requireMMDSV2=true`
- `deploy: endpoint v<N> on <runtime id> serves version <N> (created|present)`
- `GOMOD-STORE OK|EMPTY|UNKNOWN stage=direct bucket=<bucket> ...`, a report that never fails the deploy. `EMPTY` carries the command that fills the Go module store.

Every deploy step prints a line starting with `deploy:`. The Go module store line is a report and does not.
A failed deploy ends with `DEPLOY-FAIL <step> <detail>`.
It exits 1.

Check the guard without deploying with `scripts/deploy.sh --stage direct --azs 1 --check-guard`.
A passing check prints `DEPLOY-GUARD PASS stage=direct running=0`.

Read the stored deployment digest with this command.
Use the entry's profile and Region for `<profile>` and `<region>`.

```bash
aws ssm get-parameter --name /hamh/direct/deployment-digest --query Parameter.Value --output text --profile <profile> --region <region>
```

Read the API URL from the control stack.

```bash
aws cloudformation describe-stacks --stack-name hamh-direct-control --query "Stacks[0].Outputs[?OutputKey=='V1ApiUrl'].OutputValue" --output text --profile <profile> --region <region>
```

The URL ends in `/direct`.

With the full setup, `setup.sh` reads the endpoint.
It installs the client and sets the endpoint.
It runs `ham doctor` for you.

After a deploy you run yourself with `scripts/deploy.sh`, install the client with `./setup.sh --client-only --build --endpoint <url>` (see [Set up, step by step](#set-up-step-by-step)).
You can also set the endpoint by hand and check the client:

```bash
ham config set api_endpoint <url>
ham doctor
```

`ham doctor` prints one PASS, WARN, or FAIL line per check.
It exits 0 unless a check fails.
Users need an IAM identity in the stage's own account.
The API accepts only principals from that account.

The table's first column shows failed deploy output. A `DEPLOY-REFUSED` line ends a deploy that stopped before its first stack change (exit 3, the stage is unchanged); a `DEPLOY-FAIL` line is exit 1.

| Line | Meaning | Fix |
|---|---|---|
| `DEPLOY-REFUSED reason=account` | The caller is not the target account. | Use credentials for the account in `accounts.json`, or set `HAMH_AWS_PROFILE`. |
| `DEPLOY-REFUSED reason=azs ...` | An AZ name does not map to the recorded AZ id in this account. | Fix the `azs` and `azIds` of the entry. |
| `DEPLOY-REFUSED reason=vpc-cidr ...` | `10.50.0.0/16` overlaps another VPC. | Remove or retag the overlapping VPC. |
| `DEPLOY-REFUSED reason=bootstrap ...` | The CDK bootstrap is older than the app needs. | Upgrade the bootstrap yourself. The deploy never modifies an existing one. |
| `DEPLOY-FAIL build-selection ...` | A binary was built for another account. | Rebuild with the right `HAMH_AWS_PROFILE`. |
| `DEPLOY-FAIL fork-binary ...` | The Cline binary does not match the pin, or cannot be read. | See "How the Cline binary reaches a new account". |
| `DEPLOY-REFUSED reason=guard wait=yes ...` | Runs are running on `hamh-direct-run`. | Wait for them, or cancel them. |
| exit code 2 | A dirty tree, a bad flag or an unknown profile. | Commit your changes, or fix the flag. |

The full setup fills the Go module store. After you deploy with `scripts/deploy.sh`, a run that needs a Go module finds none until you fill the store; fill it and
check it on a machine with the tools on PATH:

```bash
scripts/gomod-store.sh fill --stage direct --modules engine/rules/deps/go-approved-modules.txt --tools scripts/gomod-tool-roots.txt --jobs 8
scripts/gomod-store.sh check --stage direct
```

Storing a Tavily search key is the other step after a deploy, and it is optional. Without a key, search works
through AgentCore Web Search and page fetch answers `disabled`. With a key, the stage uses Tavily first and page
fetch works.

### Developer verification

Developer checks run on a Linux build machine with the pinned tools.

These checks are not needed to run the installed service.

| Check | Command | Passing result |
|---|---|---|
| Documentation tree lint | `python3 scripts/dox_lint.py` | `DOX PASS` |
| Worker image key against its pin | `scripts/image-key-guard.sh` | `IMAGE-KEY PASS <key>` |

The infra checks run the CDK app's tests and type check. They use `scripts/node.sh`. Tests parse the synthesized templates.

Run the infra checks by hand from the `infra` directory:

```bash
cd infra
../scripts/node.sh pnpm install --frozen-lockfile
../scripts/node.sh pnpm test
../scripts/node.sh pnpm typecheck
```

`scripts/node.sh` runs Node in a container, so Docker or a compatible container engine must be installed.
It fetches AWS credentials before it starts, because the CDK app reads the account entry of `accounts.json`.
The infra tests and the type check make no AWS call.
To run the three commands above with your own AWS account, add an entry for your account to `accounts.json` and select it with `HAMH_AWS_PROFILE`.
You can also set `NODE_SH_CREDS_CMD` to a program that prints credentials as JSON in the `credential_process` format.

### Teardown

`./setup.sh --teardown` removes stage `direct` and the runner from your AWS account.
It runs on your machine with your own AWS credentials, with the profile and Region that setup uses.
It installs nothing in your account, starts no build, assumes no role and opens no authority window.
It needs the AWS CLI, version 2, with the AgentCore commands. An older AWS CLI stops it with exit 1 before it deletes anything.
It needs no git clone and no clean working tree. A zip download of the repository holds the helper files it reads.
It prints the plan and asks `Delete these resources? [y/N]`.
Use `--dry-run` to print the plan and change nothing.
Use `--yes --acknowledge-teardown --account-id ID` to skip the question. `ID` is the 12-digit id of the account you mean.
`--yes` alone does not confirm. `--yes --acknowledge-teardown` without `--account-id` stops with exit 4 before it reads or deletes anything.

Teardown stops the stage's sessions and running executions.
Do not run it while the stage has work you want.
Check first with `ham runs`.
Download the results you want first with `ham pull <runId>`.
Teardown deletes the stage's results bucket `hamh-direct-harness-<account id>`, which holds every run's zip, bundle and evidence, and its DynamoDB run table `hamh-direct-harness`.

`./setup.sh --teardown` makes one inventory and one plan for the stages it finds. The plan header names the generations it covers. By default, it covers every generation found. Teardown support for a stage deployed under the earlier names ends with version 6.00. Detection of that stage has no end version.

- You use `--only legacy` to limit the plan to a stage deployed under the earlier names. You use `--only current` to limit it to a stage under the current names.
- `./setup.sh --update` and `./setup.sh --full` refuse a stage deployed under the earlier names with exit 7. Their message says the earlier stage keeps running. `ham pull` works against its endpoint.
- You install the new client against the earlier endpoint. You run `ham pull` for each run you want to keep. You run `./setup.sh --teardown`. You run `./setup.sh --full` to deploy a fresh stage.
- No data moves to the fresh stage. Teardown deletes the earlier stage's run records, delivered zips, engine settings, Tavily key and table rows.

#### What belongs to the stage

A stack belongs to the stage when its name starts with `hamh-direct-` and it carries the tags `project=hyperscale-agent-meta-harness` and `stage=direct`. A stack with that name and without both tags stays, and so does a stack whose tags cannot be read. A `kept:` line names it.
The buckets, image repositories, state machines, EventBridge rules, tables, research secret and AgentCore runtimes of a stage stack come from that stack's own resource list.
A thing outside every stage stack belongs to the stage only when it carries the stage tags: the AgentCore runtimes named `hamh_direct_*`, the buckets `hamh-direct-harness-<account id>` and `hamh-direct-gomod-<account id>`, VPCs, and the SSM parameters under `/hamh/direct/`.
Log groups carry no stage tag. Teardown deletes a log group only when its name starts with `/aws/bedrock-agentcore/runtimes/hamh_direct_`, `/aws/lambda/hamh-direct-` or `/aws/codebuild/hamh-direct-`, and only when the function, build project or runtime that writes to it is gone or goes with a stage stack. Other log groups under those prefixes stay, and a `kept:` line counts them.

The plan prints the exact names of the stacks, buckets, image repositories, secret, rules, state machines, AgentCore runtimes, VPCs, log groups and SSM parameters it deletes.
The plan you confirm is the limit of what teardown deletes. A stack, log group, parameter, bucket or VPC that appears after you confirm is reported and left. Run teardown again to see it in a plan.

#### Order of work

1. It deletes the runner stack `hamh-setup-runner` and its bucket, when a runner is installed. The runner's authority ends before the long deletes begin. When CloudFormation cannot delete the runner stack with your own session (a refusal of an IAM action of yours, for example a session policy that denies deleting an IAM role that has been used), teardown prints the failed resource and CloudFormation's reason. It then deletes the stack again with the CDK execution role `cdk-hnb659fds-cfn-exec-role-<account id>-<Region>`, the role CloudFormation uses for the stage's stacks. The plan names that role, and you need permission to pass it. A refusal of another principal, a bucket policy or a service control policy is not worked around. If that attempt fails too, the output shows `RETAINED stack hamh-setup-runner (DELETE_FAILED)` with the cause and the command `aws cloudformation delete-stack --stack-name hamh-setup-runner --role-arn <ARN of a role that may delete the stack's IAM role>`. The result then has a `LEFT: stack hamh-setup-runner` line, and the exit code is 8. `./setup.sh --full` removes its own runner after a deploy the same way.
2. It disables the stage's EventBridge rules. It stops the stage's Step Functions executions and AgentCore sessions. Stopping a session on the Instances runtime also deletes its machine.
3. It deletes the `v<N>` endpoints that deploys made on the stage's AgentCore runtimes. A runtime that has them cannot be deleted.
4. It deletes the stage's stacks in sequence, in up to eight rounds. It deletes each importing stack before the stack that supplies the imported values; it reads that order from the exports and imports that CloudFormation holds. Before each stack, it empties the stack's S3 buckets, because CloudFormation cannot delete a bucket or an image repository that holds data. It deletes every object version and delete marker, with the Object Lock governance bypass for the Go module store, and the images in the stack's repositories. It deletes the research secret right after its stack.
5. When no stack of the stage is left, it deletes the log groups and the `/hamh/direct/` parameters of the plan, the tagged stage buckets that no stack listed, and the VPC that an earlier teardown retained.
6. It reads the account again and prints the result.

A stack stays when teardown cannot read its resource list, or cannot read the stacks that import its exports. The `STACKS LEFT:` line gives the reason, and the next run tries again.
CloudFormation deletes each stack with the role it stored with the stack, so you need no permission to pass that role. When CloudFormation refuses a delete with `invalid or cannot be assumed`, that stored role (the execution role of the CDK bootstrap) no longer exists or cannot be assumed. Teardown then prints `aws cloudformation delete-stack --stack-name <stack name> --role-arn <ARN of a role that CloudFormation can assume>`.
The Go module store lets only the operator (the identity that ran setup) and the runner's role delete its objects. Run teardown as the operator. Another identity leaves that bucket and the `packages` stack in place, and the output names the identity that can finish.

The research secret `hamh-direct-research-tavily-api-key-<suffix>` comes from the resource list of the control stack, and the plan shows it. Teardown deletes it right after its stack, without a recovery window, with `aws secretsmanager delete-secret --force-delete-without-recovery`. The stack goes first because CloudFormation deletes the secret's resource policy and cannot do that while the secret is marked for deletion. With CloudFormation's own recovery window, the secret's fixed name would stay reserved for 30 days, and a new deploy in the same account and Region could not create the secret. The secret stays marked for about 30 seconds after the delete. The result waits up to 5 minutes until the secret is not found and prints `deleted: secret <name> ...`. If it is still marked for deletion after that, the result prints a `LEFT:` line and the exit code is 8, because the secret's name is not free for a new deploy. When the delete fails, the output says so and prints `aws secretsmanager delete-secret --secret-id hamh-direct-research-tavily-api-key --force-delete-without-recovery`. Other secrets wait out the recovery window that CloudFormation gives them and appear on `pending:` lines.

A teardown takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface.
While a stack deletes, teardown prints `still waiting for CloudFormation to delete the stack <name> (<n> minutes so far)` every 3 minutes.
Before it deletes the first stack, teardown deletes the `v<N>` endpoints that deploys made on the stage's AgentCore runtimes. AgentCore deletes an endpoint in the background, which can take several minutes. While teardown waits for that, it prints `still waiting for AgentCore to delete the versioned runtime endpoints (<n> minutes so far)` every 3 minutes.
For each bucket it empties, it prints `emptied <bucket>: <n> versions and delete markers`, the runner's bucket included.

AgentCore can hold network interfaces in the subnets of the network stack for up to 8 hours after its runtime is deleted.
The first delete of the network stack then fails after about 20 minutes, and the stack stays in `DELETE_FAILED`. CloudFormation cannot delete the subnet and the security group, and so not the VPC.
Before it deletes the network stack, teardown reads the network interfaces in the stack's VPC. When AgentCore holds an interface of type `agentic_ai` there, teardown prints `AgentCore holds <n> network interface(s) in <vpc id> and may keep them for up to 8 hours after runtime deletion. CloudFormation can wait about 20 minutes and then report the network stack as DELETE_FAILED; teardown then retains and lists the network resources those interfaces hold.` The interfaces of the NAT gateway, the VPC endpoints and the firewall belong to the network stack and do not trigger that line.
Teardown then looks at the interfaces in the stack's VPC. When one is held, it says so and deletes the stack again at once, in one call, with the failed subnet, the failed security group and the VPC retained.
This first run ends about 50 minutes after it starts.
It lists each retained resource as `RETAINED` and never counts it as zero resources.
AWS releases the interface up to 8 hours after the runtime is deleted; measured releases took about 3 hours in one account and 30 to 60 minutes in another. `--network-wait-minutes N` (0 to 480, or `full`; 0 by default) makes teardown wait for the release. When only these network resources are left, the result names the interface, prints the command that runs teardown again, and the exit code is 9.
It also prints the commands that remove them by hand, in the order that works: `aws ec2 delete-subnet --subnet-id <id>`, `aws ec2 delete-security-group --group-id <id>` and `aws ec2 delete-vpc --vpc-id <id>`.
A retained VPC endpoint adds `aws ec2 delete-vpc-endpoints --vpc-endpoint-ids <id>` before the subnet command, and a retained route table adds `aws ec2 delete-route-table --route-table-id <id>` before the VPC command. A command appears only for a resource that is retained.
The retained subnet, security group and VPC have no charge of their own; see [Pricing for Amazon VPC](https://docs.aws.amazon.com/vpc/latest/userguide/what-is-amazon-vpc.html). The NAT gateway, the Network Firewall endpoint and the public IPv4 address belong to the network stack and are deleted with it. [Cost guardrails](#cost-guardrails) says what a retained interface VPC endpoint costs.
When teardown cannot read the interfaces, or finds none held, it deletes the stack again in the later rounds. It retains the network resources that still fail, in up to five rounds. If CloudFormation refuses to retain the VPC together with the failed resources, teardown retains the failed ones first and the VPC after them.
Run `./setup.sh --teardown` again after AWS releases the interfaces. The command finds the VPC by its project and stage tags and by the id of the stack that made it, also when you have deployed the stage again since and a stack of the same name exists. It removes the retained network resources that carry the tags. Its plan lists everything it would delete, the new stage included, and asks `Delete these resources? [y/N]`. Answer `n` to keep the new stage.
A plan that lists no stack and only a VPC that an earlier teardown retained does not state the teardown time. It prints `This run only retries removing network resources retained by an earlier teardown; resources AgentCore still holds stay and appear as RETAINED.` Such a run takes about a minute.
To count the interfaces, teardown asks for the ones that AWS services manage (`aws ec2 describe-network-interfaces --include-managed-resources`). An AWS CLI that does not know that option prints a warning that interfaces AgentCore holds may be hidden from the count.

#### What stays

- The CDK bootstrap stack `CDKToolkit`, which other CDK projects of the account may use, with its bucket and container repository. The bucket holds the files and the repository holds the images that the deploy published for the stage.
- The client on your machine.
- The receipt and the logs under `~/.config/ham/setup/<account>/<Region>/`.
- KMS keys and secrets that CloudFormation scheduled for deletion. Lines that start with `pending:` give their deletion dates as year, month and day. The stage's three KMS keys wait 30 days, the CloudFormation default, and the runner's key waits 7 days. AWS KMS allows a waiting period of 7 to 30 days.

A line that starts with `kept:` names each of these, and each stack, runtime or bucket that teardown found and did not touch.
A line that starts with `STACKS LEFT:` lists each stack that stays, with its reason.
A line that starts with `LEFT:` names something of the stage that is still there after the run: an AgentCore runtime, a bucket, a log group, an SSM parameter, a KMS key that nothing scheduled for deletion, or a secret that still exists.
A VPC that carries the stage tags and that teardown did not sweep also prints `LEFT:`, and the run exits 8. No CloudFormation tag shows which stack made it, or the stack that owns it is not a stack of the stage.

#### The result

After the deletes, `./setup.sh --teardown` reads the account again.
It reads the stage's CloudFormation stacks and their tags, the resource list of each stage stack, the runner stack, the VPCs that carry the stage tags, the AgentCore runtimes named `hamh_direct_*`, the two stage buckets, the log groups of the stage's functions, projects and runtimes, and the SSM parameters under `/hamh/direct/`.
It also lists the stage's tagged resources from the AWS tag index for information only. The index can lag behind a delete by hours and lists a key or secret until AWS deletes it.
The result line is one of these:

```text
TEARDOWN-INVENTORY 0 stacks stage=direct runner=none
TEARDOWN-INVENTORY RETAINED <n> resource(s) in stage direct; ...
TEARDOWN-INVENTORY INCOMPLETE stage=direct: see the lines above
```

The first line means nothing is left. It appears only when every read succeeded, nothing of the stage is left and no step failed. The second line means only retained network resources are left. The retained subnet, security group and VPC carry no charge, and the VPC stays until AWS releases AgentCore's network interface, up to 8 hours after the runtime is deleted. The third line means a stack, the runner or another resource is left, a step failed, or a read failed.
A line that starts with `checked:` follows the result line and names only the reads that were made and passed. After the first line it adds that teardown does not check untagged resources, IAM roles outside the stage's stacks, or the files and images in the CDK bootstrap bucket and repository.
When a read fails, teardown names it, does not print the first line, and exits 8. When the stage has no stack and no runner but a read failed, the output starts with `Teardown could not check the stage`.

#### Exit codes

Exit 0 means nothing is left or nothing exists.
Exit 1 means the AWS CLI is missing or lacks the AgentCore commands, or the identity call fails, or a call that reads the stage fails.
Exit 2 means no Region is found or the Region is unsupported.
Exit 4 means you do not confirm: `--yes` without `--acknowledge-teardown`, `--yes --acknowledge-teardown` without `--account-id`, `--no-input` without `--yes`, no terminal, or an answer other than yes.
Exit 6 means the state of the recorded setup build, or the lease that records it, cannot be read.
Exit 7 means a build recorded in the deploy lock is in progress, a recorded build has succeeded and a setup under the lock still renews it, the lock records no build and is younger than 30 minutes, a stack of the stage is changing, or `--account-id` names another account than your credentials.
Exit 8 means a stack, a retained resource, the runner or another resource of the stage is left, a step failed, or a read failed.
A run that stops with exit 1, 2, 4, 6 or 7 deletes nothing.
A rerun continues from what the account shows.

`./setup.sh --teardown` puts the Bedrock model invocation logging setting back to the state that a deploy with `--invocation-logging enable-and-restore` recorded in `/hamh/direct/invocation-logging-prior`. It does this in the stage's Region before it deletes the first stack, when the setting still names the stage's log group `/hamh/direct/bedrock-invocations`. It leaves the setting as it is when there is no record, when the record lacks the stage tags, when the setting names another destination, or when the setting is already the recorded state, and the output says which. A restore that fails ends the run with exit 8 and keeps the record, so a second `./setup.sh --teardown` can try again.

#### The teardown program of the repository

`scripts/teardown.sh` is a separate teardown program for people who work on the repository.
It needs Python, Node and the project's tools, and it writes an evidence record.
`./setup.sh --teardown` does not use it.

Always preview first with `scripts/teardown.sh --stage direct --dry-run`.
This makes read calls only.
It prints `teardown: plan ...` for each planned change.
Then run `scripts/teardown.sh --stage direct --yes`.

The teardown flow is in `scripts/lib/teardown.py`.
It removes resources in this order:

1. Read `s3://<harness bucket>/deploy/direct/cdk-assets.json` before emptying the bucket.
2. Stop recorded AgentCore sessions. Then stop every RUNNING execution of the `hamh-direct-*` state machines.
3. Restore Bedrock invocation logging when the deploy turned it on. The prior state is in `/hamh/direct/invocation-logging-prior`.
4. Run `cdk destroy` in reverse dependency order. Before each stack, stop CloudTrail trails, empty S3 versions and delete markers, and delete ECR images. CloudFormation cannot delete buckets or repositories while they hold data. Retry the network stack for up to 60 minutes while detached network interfaces are removed.
5. Remove AgentCore runtimes named `hamh_direct_*`, tagged log groups, and tagged secrets and SSM parameters. Schedule KMS keys for deletion in 7 days. Remove recorded CDK assets, unmanaged EventBridge rules, dashboards, and saved queries.
6. Delete the stage's IAM roles. An account may refuse roles used in the last 24 hours. The inventory gives “deletable after about” 24 hours after last use. Those roles stay until a later run.

Teardown matches the name prefix `hamh-direct` or `hamh_direct`.
It also requires `project=hyperscale-agent-meta-harness` and `stage=direct` tags.
Both the name match and tag pair must apply.
It never touches untagged resources or other projects' resources.

A bootstrap created by deploy carries `stage=shared`.
Teardown's inventory does not count that bootstrap.
Teardown destroys only the `hamh-direct-*` stacks.
The bootstrap stays.

Teardown looks at CodeCommit repositories whose names start with `hamh-e2e-`.
These repositories must also carry the stage's tags.

The Go module store uses S3 Object Lock in governance mode.
Teardown deletes the Go module store's object versions with the S3 governance-mode bypass.
The bucket policy allows this only for the identity that runs `setup.sh`, recorded as the operator in the generated account file, and the setup runner's role.
A stage whose `accounts.json` entry has no `operator` field uses the IAM role named `Admin` as the operator.

After removal, teardown reads the Resource Groups Tagging API, the AWS tag index.
It checks project and stage tags in three Regions.
It also checks VPCs, NAT gateways, VPC endpoints, and network interfaces.
Its checks include log groups, IAM roles, and other resources.
A full run passes after two empty reads in a row.

Each remaining resource prints `INVENTORY <region> <id>`.
An empty stage ends with:
`TEARDOWN-INVENTORY 0 resources stage=direct kms-pending=<n>`.
The script then exits 0.
`kms-pending` counts keys waiting through their deletion period.

Remaining resources print before `TEARDOWN-INVENTORY <k> resources`.
That result exits 1.
A failure prints `TEARDOWN-FAIL <why>` and exits 1.
A caller outside the target account gets `TEARDOWN-FAIL account`.
That call changes nothing.

### Credentials

Deploy and teardown use short-lived credentials from the selected AWS CLI profile.
`scripts/node.sh` passes them through a helper that reads a file in a private directory (mode 0700); it refreshes credentials every 900 seconds with `aws configure export-credentials` and deletes the directory on exit.
The stacks create no IAM user, access key, or persistent password.
Cline gets per-session STS (the AWS Security Token Service) credentials made outside the microVM, the small isolated virtual machine that runs a session.
Do not store a password or long-lived deploy key; stop and ask the account owner for another way if a task needs one.
See [Security](#security) for the full model.

## Limits

- A session ends at the AgentCore 8-hour limit; the prompt deadline is 27,900 seconds. A run with time left continues in a new session by itself and its work is not lost. A run stops as paused when it has used its total time (24 hours by default, up to 720, which is 30 days), and as stalled when the session stops answering, Cline sends no output for the silence limit, or three sessions in a row are lost, cannot restore the work of the one before or change nothing. Both deliver the work, and `ham resume` continues them.
- With the goal loop on, the default, the harness ends a run that makes 30 tool calls in a row with no changed file and no new text, or that changes no file for 20 minutes over at least 10 calls. Calls that bring something new, and the time a command spends using the CPU, do not count toward them. `run.stall_calls`, `run.stall_window_minutes` and `run.stall_min_calls` change these numbers.
- With `--goal off` those brakes are absent. The 900-second silence limit, Cline's loop guard (5 identical tool calls in a row), Cline's mistake limit (6 failed tool calls in a row) and the 27,900-second deadline still end a run.
- The project runs two engines, Cline and OpenCode. Cline is the default engine. The default model for your own runs is Claude Sonnet 5.5.
- GitHub and GitLab delivery are not supported. Delivery is the S3 zip, a local git clone and a CodeCommit pull request. Only CodeCommit repositories get pull requests.
- The stack runs in one AWS account and Region.
- On a stage with no Tavily key, page fetch is off and `ham-research fetch` exits with code 3.
- The session's Docker is a Podman socket with one offline image, `postgres:16`. Other images and docker compose do not work.
- The worker image supplies Go, Java and Maven.
- A lost session cannot be resumed. The stage delivers the last snapshot when it holds one.
- The project publishes no `ham` binary; full setup builds `ham` in your AWS account and downloads it from your own bucket. Client-only setup installs a `ham` that you build with `--build` or supply with `--from`; see [Set up, step by step](#set-up-step-by-step).
- The product states no maximum number of runs at once. AgentCore allows 5,000 active session workloads per AWS account in US East (N. Virginia) and US West (Oregon) and 25 new sessions per second per account; AWS lists both as adjustable (the AWS page *Quotas for Amazon Bedrock AgentCore*). The Amazon Bedrock model token quotas of the account bind first. Only the account owner changes a Bedrock quota.
- The sample is not a supported production service.

## Project layout

The repository holds ham, the Mac and Linux terminal app, and its AWS services. Images, tests, probes and scripts build, deploy and check these parts. Production code is Rust in a Cargo workspace, a set of crates. Infrastructure uses the AWS
Cloud Development Kit (CDK) in TypeScript. Probes and some scripts use Python. Each durable directory, such as `crates/`, `engine/` and `scripts/`, holds an `AGENTS.md` file that states its contract. The tree below leaves those files out. It
also leaves out git-ignored build output such as `target/` and `node_modules/`.

Words used in the tree:

- **ham**: the terminal app you run on your Mac or Linux machine.
- **Cline**: the coding engine. It writes code and runs its own tests.
- **AgentCore**: Amazon Bedrock AgentCore. This AWS service hosts the agent sessions.
- **microVM**: a small isolated virtual machine. AgentCore starts one per run.
- **agentd**: the supervisor program inside the microVM. It drives Cline.
- **Lambda**: an AWS Lambda function. AWS runs this code on demand.
- **stage `direct`**: one AWS account and Region deployment.
- **run machine**: the AWS Step Functions workflow `hamh-direct-run`. It carries one run from start to delivery.
- **CDK**: the AWS Cloud Development Kit. It describes AWS infrastructure in code.
- **crate**: the Rust word for a package.
- **DOX tree**: the directory documentation tree. It is the set of `AGENTS.md` files.

```text
hyperscale-agent-meta-harness/                          # repository root
├── README.md                                     # this file
├── LICENSE                                       # MIT No Attribution license (MIT-0)
├── NOTICE                                        # lists third-party and derived material, such as Cline
├── SECURITY.md                                   # how to report a security issue
├── CONTRIBUTING.md                               # how to propose a change
├── CHANGELOG.md                                  # what changed in each version
├── CODE_OF_CONDUCT.md                            # rules of conduct for contributors
├── AGENTS.md                                     # root work contract at the top of the DOX tree
├── CLAUDE.md                                     # a link to AGENTS.md: the engineering contract with fixed rules, change rules and check commands
├── setup.sh                                      # Single entry point: deploys the backend in your AWS account and installs ham on a Mac or Linux machine
├── Cargo.toml                                    # Rust workspace with 15 crates and shared lint rules
├── Cargo.lock                                    # pinned Rust dependency versions
├── rust-toolchain.toml                           # pinned Rust toolchain version
├── clippy.toml                                   # settings for clippy, the Rust linter
├── tools.lock                                    # pinned build toolchain versions and checksums
├── accounts.json                                 # AWS accounts and Regions the tools can target; HAMH_AWS_PROFILE picks one
├── .config/nextest.toml                          # test-runner settings to run every test and report every failure
├── .gitignore                                    # keeps build output and key or credential files out of git
├── .gitleaks.toml                                # secret-scan settings and accepted false positives
├── .bandit, .checkov.yaml, .semgrepignore        # three security scanners' ignore lists, each naming exact files
│
├── crates/                                       # Rust Cargo workspace containing all production code
│   ├── harness-protocol/                         # shared wire types that export the JSON schemas
│   ├── harness-core/                             # shared logic for engine profiles, exit diagnostics and secret redaction
│   ├── ham-core/                                 # ham client logic: run state, command parser, rules checks and model list
│   ├── ham/                                      # ham binary: subcommands, signed requests and terminal screens
│   ├── ham-admin/                                # operator command line that sets which repositories can receive pull requests
│   ├── ham-e2e/                                  # the local fake engine and its tests
│   ├── control-api/                              # public /v1 API Lambda: starts and lists runs, events and artifacts
│   ├── worker-api/                               # private /internal API Lambda: credential refresh, heartbeat and events
│   ├── run-session/                              # run machine Lambda steps: prepare, start session, plan delivery and finalize
│   ├── dispatcher/                               # library for run and session ids, session policy and AgentCore calls
│   ├── agentd/                                   # supervisor inside the microVM that drives Cline
│   ├── pkgproxy/                                 # local package proxy for Cline installs; Cline never holds the registry token
│   ├── delivery/                                 # delivery Lambdas: repo-gateway (pull requests), bundle-validator (checks), packager (the zip)
│   ├── message-ledger/                           # rules for moving a user's message through its states to a running Cline
│   └── research-api/                             # research route for Cline: web search and public repository downloads
│
├── engine/                                       # pinned Cline fork, engine profiles and model list
│   ├── lock.toml                                 # pins the Cline binary in every worker image by SHA-256
│   ├── cline/                                    # Cline fork lock, 15 patches and rebase script; no upstream source stored
│   ├── profiles/                                 # profiles for Cline, a fake-ACP test engine and OpenCode; both engines run today
│   ├── catalog/                                  # models.json: five supported Bedrock models with smoke records, saved live test results
│   ├── plugins/                                  # observer plugin shipped in the worker image and the lock of both plugin folders; the goal plugin is fetched at image build
│   ├── sources/                                  # pins, hashes and licences of every fetched engine and plugin
│   ├── policy/                                   # image-key.pin holds the pinned worker image key
│   └── rules/                                    # approved dependency versions for sample projects
│
├── images/                                       # container images and build specs
│   ├── worker-python/                            # each microVM's container: agentd, pinned Cline, Python, Node, Go and Java tool sets
│   ├── delivery/                                 # image of the delivery Lambdas
│   ├── codebuild/                                # AWS CodeBuild specs for linux-arm64 builds
│   ├── services/                                 # native worker image services: PostgreSQL, MariaDB, Valkey and mailpit
│   ├── tools/                                    # pinned installers for the tool sets
│   └── lang-test/                                # test container with language toolchains for the build host
│
├── infra/                                        # TypeScript CDK app for stage direct: nine stacks named hamh-direct-<name>
│   ├── bin/                                      # CDK app entry points
│   ├── lib/                                      # stack code; config.ts reads accounts.json
│   ├── test/                                     # stack tests using vitest
│   ├── cdk.json                                  # CDK settings
│   ├── setup-runner.yaml, setup-runner-policy.json  # Account-local runner (bucket, CodeBuild project and roles) installed by setup.sh, plus its permissions
│   └── package.json, pnpm-lock.yaml              # pinned Node dependencies
│
├── lifecycle/                                    # development flow Cline reads: a rules file and seven skills
│   ├── pack/                                     # shipped content: the rules file and the seven skills
│   ├── pack.lock.json                            # SHA-256 of every pack file
│   ├── pack.history.json                         # SHA-256 of every earlier text of a pack file
│   └── lock.py                                   # checks and rewrites the lock and the history
│
├── statemachines/                                # AWS Step Functions definitions in Amazon States Language
│   ├── direct.asl.json                           # the run machine hamh-direct-run
│   └── direct-run-ended.asl.json                 # clears a run whose execution ended early
│
├── schemas/                                      # contracts generated from harness-protocol
│   ├── *.schema.json                             # JSON Schema 2020-12: engine profile, event envelope, evidence, items, orchestration, runtime, tools
│   └── openapi-v1.json, openapi-internal.json    # OpenAPI 3.1 for the /v1 and /internal APIs
│
├── fixtures/                                     # shared test data
│   ├── engines/                                  # transcripts the fake engine replays in tests
│   ├── acp/                                      # recorded transcripts of real engines
│   ├── ntier/                                    # web, events and enterprise sample projects: a prompt and a specification for each
│   ├── python-calc/, fanout-kata-24/             # repositories and prompts for live acceptance runs
│   ├── rules-clash/                              # repositories for checks of test-first work and rules
│   ├── direct-probe/                             # engine isolation probe (sandbox_probe.py)
│   └── ...                                       # more test-data folders
│
├── security/                                     # advisory review and scanner data
│
├── scripts/                                      # repository tools
│   ├── dox_lint.py                               # checks the AGENTS.md tree; prints DOX PASS
│   ├── deploy.sh                                 # deploys committed HEAD to stage direct, stack by stack
│   ├── teardown.sh                               # removes a stage and prints an inventory of what remains
│   ├── build-cline-fork.sh                       # checks the lock, applies patches and builds the pinned Cline fork
│   ├── engine-fetch                              # fetches, verifies and builds the pinned engine and plugin sources
│   ├── node.sh                                   # runs Node, pnpm and the CDK CLI in a pinned container
│   ├── setup/                                    # setup.sh helpers for the account file, runner, receipt, status, update and teardown
│   ├── lib/                                      # Python helpers behind the shell wrappers
│   └── tests/                                    # tests for scripts and the lock files they read
│
└── docs/                                         # documentation set
    ├── user-guide.md                             # install ham, start runs, edit rules, pick models, collect results and fix problems
    └── images/                                   # architecture.svg (AWS Architecture Icons), architecture.png fallback, SOURCES.md
```

### Documentation map

The page in `docs/` covers install, use and operation.

| Page | Read it when you want to |
| --- | --- |
| [docs/user-guide.md](docs/user-guide.md) | install ham, start runs, edit rules, pick models, collect results, fix problems |

### Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) before you open a change. Read [CHANGELOG.md](CHANGELOG.md) for what changed in each version. Every durable directory has an `AGENTS.md` file, and together these files form the DOX tree that starts at the root `AGENTS.md`. When a change alters what a directory holds or how it works, update the nearest `AGENTS.md` in the same change. `python3 scripts/dox_lint.py` must print `DOX PASS` before a change is done.

## Security

Please **do not** open public issues for security vulnerabilities. See [SECURITY.md](SECURITY.md) for reporting privately to AWS Security. It also explains the project's security posture.

The sample-code notice at the top of this README sets your deployment responsibilities. You secure your own AWS account, credentials, IAM scoping, network egress and budget caps. See [Costs](#costs) for the budget side. The stage creates real, billable AWS resources.

### Safety and secrets

- run-session creates per-session STS (AWS short-lived credential service) credentials outside the microVM. No long-lived credentials or GitHub token enter it. agentd, the microVM supervisor program, holds package-registry tokens in memory.
- Cline cannot edit its own limits. root owns the read-only rules, Cline's settings, and its config directory. Cline runs as an unprivileged user.
- Outbound network: AWS Network Firewall allows the Bedrock runtime endpoints of the other inference Regions, the Bedrock Mantle endpoints in us-west-2, us-east-1 and us-east-2, and api.osv.dev. It drops and logs the rest. Home-Region AWS services (Bedrock, the private worker API, CloudWatch Logs, CodeArtifact, ECR) are reached through VPC endpoints, and S3 through a gateway endpoint. The source is `infra/lib/stacks/network.ts`.
- Delivery scans run commits for secrets with gitleaks. A secret in the bundle blocks delivery. Delivery also blocks protected paths and files over 2 GB.
- Bedrock is the only model provider. Every AWS resource carries the tag `project=hyperscale-agent-meta-harness`.
- The repository stores no persistent password. IAM roles and short-lived STS credentials replace passwords.

Callers are authenticated with SigV4, AWS request signing, from an IAM identity in the stage's own AWS account. ham takes the credentials from the profile in its config, else the `AWS_*` environment variables, else `AWS_PROFILE`, else the default profile. A profile may use static keys, `credential_process`, IAM Identity Center (SSO) or assume-role. For the last two, ham asks the AWS CLI (version 2.9.0 or later). ham makes no network call of its own to get credentials. The stage API uses AWS_IAM authorization. Its resource policy allows only callers from the stage's own account and denies the worker session role. A run is returned only to the caller that started it. The admin routes answer only operators named at deploy time. The worker API is private: only the worker session role can reach it, through the stage's VPC endpoint. A request without a signature is refused with HTTP 403. The only link that works without a signed request is an S3 presigned download link, which lasts 15 minutes. The policy trusts every authenticated principal in the stage account, unless an explicit Deny applies to it; it holds no allowlist of named roles. The control API writes its own log line for each request.

The full security posture, listed by area, is in [SECURITY.md](SECURITY.md).

## License and attribution

The code in this repository is released under the **MIT No Attribution license (MIT-0)**. The text is in [LICENSE](LICENSE).

Cline and other coding engines are not part of this repository. The build fetches Cline from its upstream repository at a pinned commit, checks it, and applies the patches in `engine/cline/patches`. Cline is Apache-2.0 software of Cline Bot Inc. The patches derive from it and keep its license. Cline's goal plugin and one function of its SDK are fetched from their upstream repositories at pinned commits when the worker image is built, bundled and checked against hashes, and are not stored here (`scripts/engine-fetch`, `engine/sources/`). [NOTICE](NOTICE) lists the patches and the fetched parts and records the attribution.
