# Security Policy

This file explains how to report security vulnerabilities in Hyperscale Agent Meta Harness, a sample project for multi-harness orchestration on Amazon Bedrock AgentCore. It lists which versions receive fixes. It describes the project's security controls. It is this repository's single entry point for the security process. The terminal app `ham` starts a remote loop of Cline coding agents. These agents run on Amazon Bedrock AgentCore in your own AWS account.

## Reporting a Vulnerability

Please do not disclose a security vulnerability in a public issue, pull request or commit. Public disclosure before a fix puts every user at risk.

Report suspected vulnerabilities to AWS Security. Use the AWS Vulnerability Reporting page:

https://aws.amazon.com/security/vulnerability-reporting/

That page names the AWS Vulnerability Disclosure Program on HackerOne. For reports outside that platform, email aws-security@amazon.com. Send questions to the same address.

Include these details:

- The affected file or component.
- The path and line, where possible.
- A description of the vulnerability.
- Its impact.
- Steps to reproduce it, or a minimal proof of concept.
- Any helpful tool output.
- A suggested fix, if you have one.

The AWS page says AWS sends a non-automated reply within 24 hours. That reply confirms receipt. AWS then provides timely updates and monthly check-ins.

The AWS page asks you to keep vulnerability details private until the vulnerability is addressed. It also asks you to keep AWS customer data private.

Test only against AWS accounts and resources you own or have explicit permission to use. That permission must come from the account holder. Stop and report if you encounter user data.

## Exposed credentials

Treat leaked credentials as urgent. These include AWS access keys, API keys, tokens and private keys. Rotate or revoke the credential first. Then report it. For an AWS access key, make it inactive in IAM. Create a new key only if you still need it.

Project controls keep long-lived keys out of the microVM, a small isolated virtual machine. They scan commits and images for secrets. See Security Posture below.

## Supported Versions

Fixes land on the default branch, `main`. Run the default branch. Earlier tags and commits receive no separate fixes.

| Version | Supported |
| --- | --- |
| `main` (default branch) | Yes |
| Any earlier tag or commit | No |

## Security Posture

The product includes `ham`, a Rust terminal app. It also includes a set of AWS services. Its Cline agent runs in an Amazon Bedrock AgentCore microVM.

The notes below summarize the security controls. These controls are in the code and the CDK app, the AWS infrastructure code.

### Secrets handling

No long-lived AWS key enters the microVM. No GitHub token enters it.

The stage's `run-session`, a Lambda function, creates short-lived STS credentials outside the microVM. STS means AWS Security Token Service. The function creates one credential set for each run.

Cline receives credentials only through `credential_process`, a credential helper. This helper asks `agentd`, the microVM supervisor program, over a Unix socket. AWS key variables are removed from Cline's environment. The Bedrock API key variable is also removed. Cline's profile has AWS instance metadata lookup switched off.

`agentd` starts Cline as an unprivileged user inside the microVM. When it cannot find that user, the run ends before Cline starts and shows "Cline did not start". Cline never runs as root.

Cline holds the same session credentials as `agentd`. Two routes of the private API, the one that completes a turn and the one that records events, also need a run key that only `run-session` and `agentd` can make. A process that has the session credentials and not the key cannot complete a turn or write events. A run that started before this check existed is accepted as it was. The key does not protect the files `agentd` uploads at the end of a run: the session credentials can still overwrite them until the delivery step reads them.

The CodeArtifact token, for the AWS package service, lives only in `agentd` memory. A local proxy adds it to requests. Cline never sees the token. The program wipes the token from memory when it is dropped.

Source: `crates/pkgproxy/src/lib.rs`, using `Secret` and zeroize on drop.

Output containing a session credential becomes `<redacted>` before leaving `agentd`. The same rule covers the refresh nonce and package token, and every secret a credential refresh creates later. Events, the stored conversation and the end-of-run notes all use that one set. Strings starting with `ASIA` or `AKIA` are scrubbed from details.

The Tavily web-search key sits in AWS Secrets Manager. The secret's resource policy denies GetSecretValue to every principal except the research Lambda's role, administrators included. An administrator who may change the secret's resource policy can remove the rule. The key never enters the microVM. A stage needs no key to search: it uses AgentCore Web Search, which authenticates with IAM, and only a stage whose owner stored a Tavily key sends queries to Tavily.

The research Lambda accepts an event as a worker's only when it has the whole shape that API Gateway gives a worker session and the account of the function. Anyone who may invoke the function directly (`lambda:InvokeFunction`: account administrators and any role you name for the operator's key check) can send any event, so they can act as any worker session on the research routes. Worker sessions hold no such right. Grant it narrowly.

A Tavily dashboard key does not expire; a leaked key stays valid until someone deletes it in the Tavily dashboard. Tavily advises rotating a key about every 90 days; a stage that has a Tavily key stored raises an alarm when that key is 30 days old (the deploy setting `HAMH_TAVILY_KEY_MAX_AGE_DAYS` changes the age) and when Tavily refuses the key. The metrics of the alarms hold the stage name and a number, never the key, a header, an answer of Tavily or an address. `python3 scripts/research-key-put.py --stage direct --rotate` checks the new key with Tavily before it stores it, waits until the research Lambda holds it, and prints nothing derived from the key: after the key is typed it prints only fixed messages, status codes and a short list of AWS error codes. It asks the AWS CLI whether its history is on and refuses to run when it is, or when the CLI cannot say, because that history would keep the key. On an Enterprise plan, `--rotate-enterprise` makes a key that Tavily deactivates by itself after the hours you choose; it deactivates the previous generation only when it is certain that the new key is stored, current and in use, and it keeps a ledger of every active generation in the secret's tags. Run one operator at a time. The wait uses the `key-state` question, which only a direct invoke of the research Lambda reaches; it is meant for the operator, the Lambda cannot tell an operator from another caller that sends the same event, and so no worker session role may hold `lambda:InvokeFunction` on the research Lambda (the worker session policy grants no Lambda action, and a test holds it to that). A Tavily key is optional: `python3 scripts/research-key-put.py --stage direct --clear` returns a stage to AgentCore Web Search, which needs no key, and deleting the key in the Tavily dashboard is what revokes it. The script reads an account file, as the deploy scripts do; the README item "Can Cline research the web?" says how to get one after a full setup.

The repository stores no persistent password. IAM roles and short-lived credentials replace passwords. Default rules from `ham rules init` carry this rule into every Cline session.

Tests build key-shaped stand-in values at run time. They do not store these values as literals.

`ham` keeps local state in `config.toml`, engine files in `engines/` beside `config.toml`, your rules and skills folders, and downloaded artifacts. It changes the config file only when you ask, and only the line concerned. It signs each request with your own AWS identity using SigV4, AWS request signing. It stores no secret. `setup.sh` writes no secret.

### Guardrails in the product

Cline cannot edit its own limits. On the default compute type (`microvm`), `agentd` runs as root and Cline runs as the unprivileged user `agent`. On the opt-in `instances` type there is no second user: Cline runs as the user nobody (65534) in user and IPC namespaces of its own, and a Landlock domain with a system-call filter bounds it.

On the `microvm` type root owns your rules. The rules have mode 0444. Their directory has mode 0555. Root also owns Cline's settings file and config directory. On the `instances` type the Landlock domain, not file ownership, keeps the rules and settings read-only.

A user can set five engine settings for Cline in the engine file. The key allow-list refuses every other key. Credentials, provider, telemetry, processes, approvals, network and owned elsewhere can never be set. Values are numbers in ranges or members of closed sets, so a value cannot hold free text. A value outside its range is refused and never clamped. `ham`, `control-api` and `agentd` run the same check. `agentd` sets the telemetry-off and no-auto-update locks after it loads the base settings; the user's tool list adds to the stage's list and cannot remove from it. File comments never leave the user's computer.

Cline runs with auto-approve on. It may run any command inside the microVM. The microVM limits what these commands can reach. The user split, STS-only credentials, network allowlist and package proxy also limit their reach.

AWS Network Firewall drops every outbound host outside its allowlist. It logs each drop. `infra/test/network.test.ts` checks that the firewall allowlist holds exactly the names the stack lists and that the firewall drops and alerts on everything the allowlist does not match.

Only harness-authored events move a run to complete. Only those events show links. Cline cannot write these events. The harness records how Cline stopped. It shows one fixed label. It does not judge the code.

Delivery runs gitleaks, a secret scanner, over the run's commits. It refuses bundles containing a secret. It also refuses changes to GitHub Actions workflow files. It refuses changes to the root `buildspec.yml`. It refuses changes to the engine's hooks, plugins and agents.

Delivery refuses files over 100 MiB. It refuses bundles over 2 GiB. A bundle that fails one of these checks, or the secret scan, ends the run as `blocked`: `ham` exits 12 and nothing is delivered.

Amazon Bedrock is the only model provider. The service accepts only models with a passing smoke record. It refuses every other model id.

A run ends at a deadline of 27,900 seconds. It delivers what it has. You can stop a run with `ham cancel <runId>`.

While the goal loop is on, which is the default, the harness ends a run that makes 30 tool calls in a row with no changed file and no new text, or that changes no file for 20 minutes over at least 10 calls. With `--goal off` those brakes are absent. The 900-second silence limit, Cline's loop guard (5 identical tool calls in a row), Cline's mistake limit (6 failed tool calls in a row) and the 27,900-second deadline still end a run. Cancel a run you suspect with `ham cancel <runId>`. See Limits in [README.md](README.md).

### Transport and storage encryption

The Rust AWS SDK clients use HTTPS. The `/v1` API requires a SigV4 signature. That signature must come from an IAM identity in the stage's account. `ham` calls this API.

The stage has two S3 buckets. They are the harness bucket and the Go module store. Both deny requests that do not use TLS.

The harness bucket uses SSE-KMS with the stage's customer-managed KMS data key. Versioning is on. All four Block Public Access settings are on. ACLs are disabled.

The DynamoDB table uses KMS encryption with the same data key. Point-in-time recovery is on. The data key has rotation on. The CodeArtifact domain uses this data key. The Bedrock invocation-log group's key also has rotation on.

The Go module store holds public module bytes. It uses SSE-S3. An anonymous reader cannot decrypt an SSE-KMS object. The store has Object Lock in governance mode. Its objects are create-only. Reads go only through the stage's S3 gateway endpoint.

The package proxy listens only on `127.0.0.1`. It accepts only GET and HEAD. It checks upstream TLS certificates. Any other method gets status 405. `crates/pkgproxy/tests/refusals.rs` sends POST, PUT, DELETE, PATCH, OPTIONS and CONNECT to the package proxy and expects status 405 with an `Allow` header.

Download links are S3 presigned URLs, links that carry temporary permission. They last 15 minutes.

The refresh nonce is encrypted with the data key under an encryption context. The Step Functions task token receives the same encryption.

Sources: `infra/lib/stacks/data.ts`, `packages.ts`, `security.ts` and `crates/pkgproxy`.

### Least-privilege IAM

The CDK app creates IAM roles only. It creates no IAM user. It creates no access key.

Every session uses the worker session role. `run-session` narrows this role with a session policy for each run. The role reads the run's own input. Its agent-folder access covers only that run's folder in the harness bucket. It can read and write that folder.

The role can call Bedrock to run models. It can call the private worker API. It can use the KMS data key only through S3.

A session without both a run tag and an agent tag is denied everything. Explicit denies cover CodeCommit. They also cover starting CodeBuild builds and getting a package-registry token. Any call from outside the worker network is explicitly denied.

The microVM's execution role can be read from the platform's metadata service. That role allows exactly eight actions: the image pull from Amazon ECR (`ecr:BatchGetImage`, `ecr:GetDownloadUrlForLayer` and `ecr:GetAuthorizationToken`), the runtime's log writes (`logs:CreateLogGroup`, `logs:CreateLogStream`, `logs:DescribeLogStreams` and `logs:PutLogEvents`) and metrics in the AgentCore namespace (`cloudwatch:PutMetricData`). The list is `RUNTIME_BASE_ALLOWS` in `infra/lib/stacks/security.ts`. `infra/test/security.test.ts` pins the execution role's exact list of eight allowed actions and fails when the role allows more or fewer.

The public `/v1` API admits only principals from the stage's account. It denies the worker session role. Each run route checks that the caller started the run.

The private `/internal` API admits only the worker session role. Access must pass through the stage's VPC endpoint.

### Supply chain

`Cargo.lock` pins Rust dependencies. `cargo build --locked` fails when `Cargo.lock` would need a change.

The file `infra/pnpm-lock.yaml` pins the CDK app's packages. You install them from `infra` with `../scripts/node.sh pnpm install --frozen-lockfile`, which fails when the lockfile needs a change.

`rust-toolchain.toml` pins the Rust toolchain. `tools.lock` pins build tools with SHA-256 values. It pins container images by digest.

The Cline binary is pinned by SHA-256. It is also pinned to an upstream commit. The build applies a pinned patch series. One patch compiles telemetry out. Cline never updates itself.

`agentd` hashes the Cline binary at start and records the hash. It does not refuse to start Cline on a different hash. The image build checks the hash, the image is pinned by digest, and the repository's tags are immutable, so the build check protects the run.

The worker image starts from a base image pinned by digest. Its binaries are checked against SHA-256 values at build time. These binaries are `agentd`, Cline, ripgrep, gitleaks and uv.

The ECR repositories use immutable tags. They scan images on push.

The gitleaks configuration of the repository is `.gitleaks.toml`.

`images/codebuild/image-secret-scan.yml` scans a worker image for secrets. It uses its own pinned gitleaks.

The project's code is scanned with bandit, semgrep, checkov, cfn-nag and gitleaks at pinned versions. A suppression comment must include a rule id and a reason.

The repository root holds the scanner configuration files `.bandit`, `.checkov.yaml`, `.semgrepignore` and `.gitleaks.toml`. You can use these files to run the scanners on a checkout.

## Disclaimer

This is sample software under the MIT No Attribution license, MIT-0. The license text is in [LICENSE](LICENSE). [NOTICE](NOTICE) lists third-party and derived parts. Those parts keep their own licenses.

This sample code is for information and learning. It is not for production use. Work with your security and legal teams before deploying it.

The stage creates real AWS resources. Those resources cost money while they exist.

One NAT gateway bills every hour. One AWS Network Firewall endpoint also bills every hour. Several interface VPC endpoints bill every hour. These charges continue when no run is active. Bedrock model use bills per run.

Tear down the stage when you do not need it.

You run `./setup.sh --teardown` to remove the stage and the runner and print what is left. You run `./setup.sh --teardown --dry-run` to print the plan without changing anything.

You are responsible for securing your AWS account and credentials. You are responsible for IAM scoping and network egress. You are responsible for budget caps. You are responsible for any data you process with this software.

See the sample-code notice in [README.md](README.md) for deployment responsibilities.
