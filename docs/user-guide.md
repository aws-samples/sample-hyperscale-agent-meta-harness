# ham user guide

Use `ham`, the terminal app, to send one prompt to Cline, a coding agent that runs on AWS, and to collect the result.

> one prompt in the ham terminal app starts a remote loop of Cline coding agents on AgentCore that keeps running with the laptop closed and returns a pull request and an S3 zip of tested code

The product hosts one Cline session, records how Cline stopped, and delivers what Cline left. It does not run or judge the code.
Labels say "Cline reported done" when Cline says so; otherwise, "Cline stopped: <reason>".
The quickstart run with `--s3-only` returns an S3 zip.
A pull request comes back only when you target an allowlisted CodeCommit repository whose name the stage allows (default: `--repo cc:hamh-e2e-<name>`).

Example output in this guide is illustrative; the values are made up.

- Before you start
- Install
- What a run does
- Your first run
- Your settings
- The terminal app
- Your rules
- Choosing a model
- Watching a run
- When Cline gets stuck
- Reading the outcome label
- Getting your code
- Stopping a run and partial results
- Limits
- Troubleshooting
- FAQ

## Before you start

`ham` is the terminal app. AgentCore is Amazon Bedrock AgentCore, the AWS service running each session.
Cline and OpenCode are the two coding engines.
A run uses the engine you choose from start to end.
Cline is the default when you choose none.
See "Choosing the engine of a run".

The run continues while your laptop is closed. `ham` sends the prompt, shows progress, and downloads the result.
Models come from Amazon Bedrock only.

You need:

- **An AWS account.**
  - With the full setup, you deploy the stage into your own AWS account. Your credentials must be allowed to create IAM roles, normally through administrator access. Use a dedicated sandbox account.
  - With the client-only setup, you use a deployed stage named `direct`. The person who runs the stage gives you its endpoint URL and Region. The endpoint looks like `https://<api-id>.execute-api.<region>.amazonaws.com/direct`. The last part, `/direct`, is the stage name.
- **Access to the models.** Your AWS account needs access to invoke Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra, GLM 5.3 and Claude Fable 5.2 for runs in the stage's Region, `us-east-1` or `us-west-2`. Claude Fable 5.2 also needs the Bedrock data retention mode of your account in that Region to let it run (`provider_data_share` or `aws_review`); `ham` switches the mode when a run agrees to share and your credentials may, or the account owner sets it; see "Models that need provider data sharing" in docs/user-guide.md. In `us-east-1`, your account also needs access to invoke Claude Haiku 5.5, which only writes run names and tags; a stage in `us-west-2` has run names off and never calls Claude Haiku 5.5. Model access is separate for each AWS account. AWS requires the Anthropic first-time use case form once per AWS account, or once at the management account of an AWS organization, before the first call. You submit the form by selecting an Anthropic model in the Bedrock console model catalog or with `aws bedrock put-use-case-for-model-access`. Calls return `AccessDeniedException` while access remains pending, until the status is `AVAILABLE`. The first invocation of a model sold through AWS Marketplace starts its subscription. Your AWS account needs a valid payment method configured for AWS Marketplace purchases; see [Request access to models](https://docs.aws.amazon.com/bedrock/latest/userguide/model-access.html). The identity making that call needs `aws-marketplace:Subscribe` and `aws-marketplace:ViewSubscriptions`. After activation by one identity, every role in the account can invoke the model without those Marketplace permissions. Models that AWS does not sell through AWS Marketplace need no subscription. The stage's roles have no Marketplace permission. You make the first call with your own identity, for example in the Bedrock console playground in the stage Region. Some models need additional account-level access beyond the form and subscription. You contact AWS Sales for additional access if a model still returns `AccessDeniedException` after you meet the requirements. The run drops a refused model for the rest of the run and ends with "Cline stopped: the model provider failed" if every route is refused.
- The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.
- **An IAM identity in the same AWS account.** The same person grants you a role in the stage's own account. The API accepts only principals from that account. A session from another account is refused. The profile name does not matter. The account behind it does. The next subsection, [Which stage, and how you get in](#which-stage-and-how-you-get-in), shows the steps.
- **Credentials from an AWS CLI profile.** `ham` signs calls with AWS SigV4. It first uses `awsProfile`, the config's AWS profile setting. Without that setting, it reads `AWS_*` environment variables first. Then it tries `AWS_PROFILE`. Then it tries the default profile. Profiles can use static keys, `credential_process`, SSO, or assume-role. SSO and assume-role profiles need AWS CLI version 2.9.0 or later. See [Which stage, and how you get in](#which-stage-and-how-you-get-in).
- **A Region.** `ham` first reads `region` from the config file. Next it tries `AWS_REGION`, then the profile's Region.
- **`git` to get `setup.sh` and for `ham clone` and `ham push`.** The full setup runs from a git clone.
- **An allowed repository, only for `--repo` runs.** You use a CodeCommit repository whose name the stage's repository names cover (default: a name that starts with `hamh-e2e-`). It must share the stage's AWS account and Region. It must also be on the stage's allowlist. `--s3-only` needs no repository.
- **A terminal.** With no arguments, `ham` opens the full-screen app. Both stdout and stdin must be terminals. Otherwise, it prints lines.

If the profile has none of the four kinds of settings, the error text is:

```text
ham makes no network calls for credentials: use a credential_process profile, an SSO or assume-role profile (ham asks the AWS CLI, version 2.9.0 or later), static keys or the AWS_* environment variables
```

### Which stage, and how you get in

A stage is one deployed copy of the service. The service has one stage, named direct. Its API accepts only AWS identities from the stage's own AWS account. Identities from any other AWS account receive HTTP 403. The profile's name does not change this rule.

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

After the install, `ham doctor` prints `PASS identity` when the stage accepted your account. A 403 or `FAIL identity` means one of two things. The identity is outside the stage's account, or the credentials expired.

To use a stage in your own AWS account, run the full setup (see [Install](#install)). It deploys your own stage there. It builds the Cline binary from pinned source in your account. It needs nothing from any other account.

## Install

`setup.sh` is the single entry point for Hyperscale Agent Meta Harness.
It sits at the repository root next to `README.md`.
Run it yourself on your Mac or Linux machine.
You can run it again; completed steps are left alone.
If you use the alias `cah`, read [Moving from the earlier command name](#moving-from-the-earlier-command-name) at the end of this chapter.

The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.

Setup uses the shell tools that ship with macOS and Linux.
Run the full setup from a clean git clone.
It builds the commit it records.
A zip download supports the client-only setup.
Setup refuses to run as root.

### Choose full or client only

The first question is `What do you want to set up?`

1. **Full setup**, the default, deploys the backend in your AWS account and installs and configures `ham`.
   It creates AWS resources that cost money.
2. **Client only** installs and configures `ham` for a backend that already exists.

`--full` and `--client-only` skip this question.
`--yes` alone never chooses the full setup.
A run with `--yes` and neither mode option uses client-only setup.
A run with `--from`, `--build`, or `--endpoint` also uses client-only setup.

### Run the full setup

Run this command from the repository root:

```bash
./setup.sh
```

For a scripted full setup, use:

```bash
./setup.sh --full --yes --acknowledge-costs --profile <your-profile> --region <stage-region>
```

On a terminal, setup asks one numbered question at a time for each answer whose option you omit.
The titles read `Question 1: ...`, `Question 2: ...` in the order asked.
With `--full`, the AWS profile question is `Question 1` when you omit `--profile`.
The full setup asks at most four questions: the mode, the AWS profile, the Region and where to build.
If you choose Client only first and then answer the backend question with `No, deploy a stage into my own AWS account for me (full setup)`, the full setup asks at most five.
Besides the mode question, it asks for these answers:

- **AWS profile.** A numbered menu reads `~/.aws/config` and `~/.aws/credentials`.
  It offers the `AWS_*` credential variables when both keys are set.
  It never asks for a key or secret.
- **Region.** The default comes from the `ham` config, then `AWS_REGION`, then the profile's region line.
  The supported Regions are `us-east-1` and `us-west-2`.
- **Where to build.** This question appears only when your machine has `cargo`, `cargo-zigbuild`, `zig`, `node`, and `docker`.
  Choice 1 is the runner in your AWS account, the recommended route.
  Choice 2 is your machine.

Press Enter to take the answer in brackets.
Type `q` to stop.
Nothing is created before confirmation.

The runner is a CodeBuild project that builds and deploys in your account.
`--build-on runner` selects it and is the default.
`--build-on local` runs `scripts/deploy.sh` on your machine.
The local route also needs `cargo`, `cargo-zigbuild`, `zig`, `node`, and `docker` on PATH.
With `--build-on local`, your machine also needs a C compiler and linker named `cc` on PATH. On macOS, `xcode-select --install` installs one. On Linux, install `gcc` with your package manager. `setup.sh` does not check for `cc`, so a missing one appears as an error of the build.

Setup reads your caller identity, account, Region, Availability Zone names, and AgentCore zone identifiers through read-only AWS CLI calls.
An Availability Zone is a zone within an AWS Region.
The stage uses one zone.
Setup also reads the source commit and the stage's state.
It refuses placeholder values, unsupported Regions, and a dirty clone.

Use `--preflight-only` to read and report the account, zone, and commit without creating anything.
For a deployed stage, it reports what it finds and exits 0 without installing or configuring anything.

Use `--dry-run` to show what would happen.
It creates and writes nothing.
It runs no `ham` command.
Its AWS calls are read-only.

The plan has one line that starts with `run names:`.
It says whether Claude Haiku 5.5 writes a short name and tags for each run in your Region.
It does so in `us-east-1` only.
In `us-west-2` run names are off, and each run shows a short label cut from the prompt.
When all of the stage's stacks exist but the deploy did not finish, the plan says the stage is only partly deployed.
`./setup.sh --full` then deploys again, and the complete stacks stay as they are.

Setup shows the plan, grants, and costs before asking its one confirmation:

`Create these resources and start the deploy? [y/N]`

For scripts, supply both `--yes` and `--acknowledge-costs`.
In full setup, `--yes` without `--acknowledge-costs` exits 4 and creates nothing.
In full setup, `--no-input` without `--yes` exits 4 and creates nothing.
After confirmation, setup asks no further questions.

### What the full setup does

1. **Checks your account.**
   The screen says `Checking your account`.
   Setup reads your caller identity with `sts get-caller-identity`.
   It checks the account, Region, zone, source, and stage state.
   The source must be a clean git clone.
   The source commit is `HEAD`.
   A conflicting operation or a broken stage stops setup with exit 7.

2. **Shows the plan, grants, and costs.**
   The screen says `What setup will do`.
   It shows the account, caller ARN, operator, Region, zone, stage, source commit, build route, generated account file, and client installation.
   The operator is the identity the stage trusts for its admin routes and deploys.
   The stage name is `direct`.
   The next screens are `What this setup grants` and `What it costs`.
   The `What it costs` screen names the cost items without amounts.
   In `us-east-1` and in `us-west-2` the prices are the same: 1 AWS Network Firewall endpoint at $0.395 per hour, 1 NAT gateway at $0.045 per hour, 10 interface VPC endpoints at $0.01 per hour each and the NAT gateway's public IPv4 address at $0.005 per hour.
   Together they cost $0.545 per hour, $13.08 per day or $392.40 per 30 days before any run.
   The Network Firewall pricing page says standard NAT gateway per-hour and data processing usage charges are waived when the NAT gateway sits next to a Network Firewall in the same Region and networking path, as in this stage.
   With the waiver the total is $0.50 per hour, $12.00 per day or $360.00 per 30 days, and the public IPv4 address stays in both totals as a separate item under Amazon VPC pricing.
   The stage has three customer-managed KMS keys at $1 per key per month.
   Setup creates a `CDKToolkit` bootstrap stack with a fourth customer-managed KMS key in an account that has no CDK bootstrap stack.
   Data processing, CloudWatch Logs, S3, DynamoDB, ECR, CodeBuild minutes, AgentCore sessions and Bedrock tokens are extra.
   These example prices come from October 2026 and change, so you check the AWS pricing pages.
   The setup build stops after 4 hours (240 minutes), and setup prints progress every two minutes while each of its nine phases runs.

3. **Asks you to confirm.**
   Setup asks `Create these resources and start the deploy? [y/N]`.
   Approval starts the resource creation and deploy.
   Right after you confirm, setup takes the deploy lock of the stage, so no other deploy of the stage can start until this one ends.

4. **Installs the runner.**
   Setup installs `hamh-setup-runner` from `infra/setup-runner.yaml`.
   The runner stack contains an encrypted, versioned bucket, a CodeBuild project, and separate roles.

5. **Uploads the source archive and generated account file.**
   Setup uploads a git archive of `HEAD` and the account file as separate objects.
   Both uploads use SHA-256 checksums.
   The generated file contains your account, Region, zone identifiers, and credential identity.
   It leaves the committed `accounts.json` unchanged.
   If the account has no CDK bootstrap stack, a first build of the runner prints the bootstrap template.
   Setup checks its SHA-256 against the pin in your clone and creates `CDKToolkit` with your credentials.
   The bootstrap stack supplies the roles that CDK deploys use.

6. **Builds and deploys the backend.**
   Setup starts the build and follows it phase by phase.
   The nine phases are `inputs`, `fetch`, `toolchain`, `account-file`, `deploy`, `gomod`, `scanner-rules`, `client`, and `outputs`.
   The build installs the pinned toolchain from `tools.lock`.
   It deploys with:

   ```bash
   scripts/deploy.sh --stage direct --azs 1 --fork-source build
   ```

   The Cline engine binary is compiled from pinned source.

7. **Fills the Go module store and publishes the scanner-kit rules.**
   The Go module store holds the modules used by the stage.
   The `gomod` phase fills it.
   The `scanner-rules` phase publishes the scanner-kit rules.

8. **Builds the client for three platforms.**
   The `client` phase compiles `ham` for `darwin-arm64`, `linux-amd64`, and `linux-arm64`.
   `darwin-arm64` is macOS on Apple silicon.
   The macOS executable has a valid code signature.
   The Linux executables are static.
   The `outputs` phase writes `SHA256SUMS` and `result.json` to your bucket.

9. **Downloads and checks the client.**
   Setup checks the SHA-256 of `result.json` against the value CodeBuild retains.
   It compares the result endpoint with the control stack's `V1ApiUrl` output.
   It downloads your platform's client from your bucket with `aws s3 cp` using your credentials.
   It checks the client's SHA-256 before installation.
   A result or endpoint mismatch exits 7 and installs nothing.
   The default installation path is `~/.local/bin/ham`.
   `--prefix DIR` installs to `DIR/bin/ham`.
   Setup creates the relative link `cah` to `ham` beside the installed binary.

10. **Writes the settings and runs the client checks.**
    Setup uses `ham config set` to write `api_endpoint`, `aws_profile`, and `region`.
    The settings file is `~/.config/ham/config.toml`.
    Setup runs `ham config migrate` when `~/.config/cah` holds settings and `~/.config/ham` holds none.
    Setup runs `ham models`, `ham rules init`, and `ham doctor`.
    These show the model list, initialize the client rules, and report client diagnostic results.

11. **Removes the runner.**
    After a successful setup, setup empties the runner bucket and deletes the runner stack.
    `--keep-runner` keeps the runner.
    The CDK bootstrap stack stays.

12. **Prints the summary.**
    The screen says `Setup summary`.
    It reports the endpoint, account/Region/stage, source commit, and each phase's state and seconds.
    It also reports the client path and SHA-256, retained resources, teardown command, and first command.

A full setup in `us-east-1` takes about 80 to 85 minutes. Runner installation and CDK bootstrapping account for about 4 to 5 minutes, deployment for about 60 to 67 minutes (including about 21 to 27 minutes for the Cline engine), and Go module store population plus client compilation for about 10 minutes. Timing depends on the Region and load.

The retained-resource field names `CDKToolkit` and the runner stack if you keep it.
The teardown command is `./setup.sh --teardown`.
Stage charges continue until teardown.

The summary gives this first command:

```bash
ham "Build a small command-line todo app in Python with tests" --s3-only --direct
```

### What the full setup grants and costs

The runner can assume the roles of the CDK bootstrap stack.
The bootstrap's CloudFormation execution role holds `AdministratorAccess`, the CDK default.
For 12 hours after each install, the runner role can assume those roles.
Treat the runner as an administrator of the account for that duration.
`HAMH_SETUP_AUTHORITY_HOURS` sets the duration to a whole number of hours from 5 to 72.
The build in your account may run 240 minutes. `HAMH_SETUP_BUILD_MINUTES` (30 to 480) changes that, and `HAMH_SETUP_BUILD_COMPUTE` (`large`, `xlarge` or `2xlarge`) changes the size of the machine. A longer build raises the least duration to the build's whole hours plus one.
After that duration, its role assumes nothing, whether the runner remains installed or is removed.
The bootstrap roles trust the whole account.
They stay in the account after the runner is removed because updates need them.

Only the installer and the runner role can write to the runner bucket.
The installer is the identity that runs `setup.sh`.
Anyone with `codebuild:StartBuild` permission for the runner's CodeBuild project acts with that authority until the duration ends.
Give that permission to no one else in the account while the runner exists.
Use a dedicated sandbox account.

The runner role writes its logs, build results, client files, stage objects, and stage parameters under `/hamh/direct/`.
It writes the stage's worker runtime endpoints and the scanner rules package in the stage's package repository.
It starts the stage's image-build project.
The runner role has no IAM permission of its own.
A permissions boundary limits the runner role itself.
That boundary does not limit what an assumed role does.

The deploy creates IAM roles, a VPC, a firewall, and the rest of the stage.
Your credentials must be allowed to create IAM roles.
This normally requires an administrator.
The local route runs the deploy on your machine with your credentials.
It installs no runner.

AWS bills these items to your account.
The idle cost of the default stage is in step 2 of [What the full setup does](#what-the-full-setup-does).

| Item | Charges |
| --- | --- |
| NAT gateway | Hourly and per GB while the stage exists. |
| Network Firewall endpoint | Hourly and per GB while the stage exists. |
| Interface endpoints | Hourly for each endpoint and zone. |
| AgentCore sessions | While a run's session is alive. |
| Bedrock calls | Tokens of the models a run uses. |
| CodeBuild | Build minutes for setup and every update. |
| Storage | S3, DynamoDB, KMS, and ECR. |
| Logs | CloudWatch Logs. |

Network resource charges continue while the stage sits idle.
Run `./setup.sh --teardown` to remove the stage.
Teardown takes about 20 to 40 minutes when AgentCore has no network interface in the stage's VPC, and about 50 minutes when AgentCore still holds its network interface.
Teardown prints a `still waiting` line every 3 minutes while a stack deletes.
AgentCore can hold network interfaces for up to 8 hours after its runtime is deleted.
Before it deletes the network stack, teardown says so when AgentCore holds an interface in the stack's VPC, because CloudFormation can wait about 20 minutes before it reports the network stack as failed.
Teardown then retains the network resources those interfaces hold, lists them as `RETAINED` with the commands that remove them by hand, and ends after about 50 minutes.
A second run, after AWS releases the interfaces, removes them.

### Resume, status, doctor, update and teardown

- **Resume:** Run `./setup.sh` again after Ctrl-C, a lost connection, or a closed laptop.
  An interrupted setup exits 6.
  Setup attaches to the build it starts and never starts a second one.
  The deploy lock, which records the build identifier, is stored in AWS, and the local receipt caches the identifier.
  For a deployed stage, setup points the client at its endpoint without deploying again.
  For a stage whose stacks all exist but whose deploy did not finish, `./setup.sh --full` deploys again.
  It installs the client from the downloaded copy if one is available.
  If no copy is available, use `--update`.
  It removes a leftover runner unless you use `--keep-runner`.

- **Status:** Run `./setup.sh --status`.
  It reports what is deployed, which build runs, the last setup result, and whether the client fits the stage.
  It exits 0 when nothing is wrong, 5 when the stage is broken or incomplete or the last setup failed, 6 when a build is running or the stage is being changed, 7 when the stage was deployed under the earlier names, and 8 when resources were retained.
  A stage whose stacks all exist but whose deploy did not finish reports `state=partial` and exits 5.
  A stage deployed under the earlier names reports `state=legacy` and exits 7, or `state=legacy-retained` and exits 8 when only a retained VPC or bucket of it remains.

- **Doctor:** Run `./setup.sh --doctor`.
  It reports `PASS`, `WARN`, and `FAIL` checks for the AWS CLI, stage, endpoint, leftover runner, and the client's own doctor.
  For a stage whose deploy did not finish, the stage check fails and names `./setup.sh --full` as the way to finish it.
  For a stage deployed under the earlier names, the stage check fails with the same sentence as `--status`.

- **Update:** Run `./setup.sh --update`. It is the one way to update both the backend and the client, and it works from a fresh clone. It builds and deploys `HEAD` from this clone. Use `--revision` to select a full 40-digit commit identifier. `./setup.sh --update --revision <sha>` exits 2 when the commit is older than the rename. It also lets an update go ahead when the stage records a commit that your clone does not hold and whose `VERSION` is not lower
  than the clone's. It also exits 2 for a commit whose release cannot keep the stage's settings. A release can change the default of a setting your stage follows. When taking the new default would replace the Instances machines, delete log events or remove something, the update keeps the value the stage runs and replaces nothing. The plan names such a setting before anything is installed, for example `setting: HAMH_INSTANCES_TYPES=m7g.4xlarge (the stage
  keeps m7g.4xlarge; this release's default is m7g.4xlarge,m6g.4xlarge, ...: export HAMH_INSTANCES_TYPES=default to take it)`. The stage keeps that value in later updates too, until you export another value or `HAMH_<NAME>=default`. A new default that replaces and deletes nothing is taken. A retry of an update that did not finish carries on what you exported for it, and the plan names what that replaces. When the stage's record was written by an earlier
  release and cannot say whether that update was given the new default, setup stops with exit 7 before it installs anything and names the exports that answer it. When the stage already runs the commit, the plan says `the stage already runs this commit`, setup only gets the client, and active runs do not stop it; `--force-deploy` deploys anyway. A commit older than the one the stage runs stops the update with exit 7, and the message names `git pull`;
  `--revision` deploys an older commit on purpose. When the stage records a commit that your clone cannot place (it does not hold the commit, for example because the repository's history was started again), setup compares the two `VERSION` values (`N.NN`, each part as a number): a lower recorded one goes ahead, and the plan says so (a VERSION orders releases only; if the stage was deployed from your own fork or from commits that you did not publish, check
  the `order:` line before you answer the question). A higher recorded one says that your clone is older than the stage and names a new clone or `git pull`. The same one, or one that cannot be read, stops the update with exit 7, and the message names `--revision` first and then a new clone of the repository. When you are not the operator that the stage records, the plan shows `operator: <old> -> <new>`, and the update needs `--change-operator`. One deploy
  of the stage runs at a time: an update that finds the deploy lock held by another setup stops with exit 7 before the confirmation and names the holder. Active runs stop `./setup.sh --update` before anything is installed or built. Setup stops with exit 7 and lists the active runs. The stage is unchanged and no runner was installed. If a run starts after setup's check and the update changes the worker runtime, the build refuses it with exit 7 too. Setup
  then removes the runner, and the stage is unchanged. Setup also removes the runner when the build fails in one of its first four phases (inputs, fetch, toolchain or account-file), which run before the deploy. If setup cannot remove the runner, it says so, and the runner stays installed. When the deploy refuses before it changes anything, setup prints `The stage is unchanged.` with the action, removes the runner it installed, and exits 7 when waiting cures
  the refusal or 5 otherwise. After a failure in the deploy or in a later phase, or when the build leaves no result, the stage may contain partial deployment changes. The runner then stays installed for the retry, and its role can assume the CDK roles until its authority window ends, 12 hours after the install by default. You cancel active runs with `ham cancel <runId>` or wait until they end, and then run `./setup.sh --update` again. Add
  `--allow-active-runs` to `./setup.sh --update` to skip the early active-run check. With active runs, the build refuses only an update that changes the worker runtime. This option belongs to `--update`; any other command stops with exit 2. Active runs keep the worker runtime version they started on. A second machine needs `--update` to obtain a client. When the stage already runs the commit of that clone, the update only gets the client. An update takes
  about 20 to 25 minutes when it reuses the Cline binary from the earlier build, and about 21 to 27 minutes more when the Cline binary must be rebuilt. `--update` and `--full` exit 7 before they create anything when the stage was deployed under the earlier names; the earlier stage keeps running. See [Moving from the earlier command name](#moving-from-the-earlier-command-name).

- **Teardown:** Run `./setup.sh --teardown` to delete the stage and runner.
  It runs on your machine with your own AWS credentials.
  It installs nothing, starts no build and opens no authority window.
  Download the results you want with `ham pull <runId>` first; teardown deletes the stage's results bucket and run table.
  It needs no git clone; the helper files of a zip download are enough.
  It needs an AWS CLI that has the AgentCore commands, and an older one stops it with exit 1 before it deletes anything.
  You confirm once at `Delete these resources? [y/N]`.
  `--yes --acknowledge-teardown --account-id <account id>` skips the question. `--dry-run` prints the plan and changes nothing.
  Without `--account-id`, the form `--yes --acknowledge-teardown` exits 4 before it reads or deletes anything.
  A stack belongs to the stage only when its name starts with `hamh-direct-` and it has both tags: `project=hyperscale-agent-meta-harness` and `stage=direct`.
  The plan names every stack, bucket, repository, log group and parameter it deletes. It deletes only what you confirm.
  It deletes the research secret right after its stack, without a recovery window. This lets a deploy in the same account and Region reuse its name.
  Run it as the identity that ran setup. Only that identity can empty the Go module store.
  Network resources held by AgentCore network interfaces are retained and listed as `RETAINED`, with the commands that remove them by hand.
  They are never counted as zero resources.
  AgentCore can hold the interfaces for up to 8 hours after its runtime is deleted. Run `./setup.sh --teardown` again after AWS releases them to remove those network resources.
  Measured releases took about 3 hours in one account and 30 to 60 minutes in another.
  `--network-wait-minutes N|full` makes teardown wait for the release instead. N is 0 to 480 minutes, `full` is 480, and 0 is the default; `HAMH_SETUP_NETWORK_WAIT_MINUTES` sets the default.
  When only those network resources are left, the run names the interface, prints the command that runs teardown again, and exits 9.
  If you deployed the stage again meanwhile, the plan of that run lists the new stage too and asks before it deletes: answer `n` to keep it.
  The CDK bootstrap stack with its bucket and repository, the client, the receipt and the setup logs stay and are named on `kept:` lines.
  KMS keys and secrets that CloudFormation scheduled for deletion are named on `pending:` lines with their dates (year, month and day).
  A stack that stays, a retained resource, any other resource of the stage that is left, a runner that cannot be removed, a failed step, or a failed read gives exit 8, unless nothing but the network resources held by AgentCore is left: that gives exit 9.
  Teardown makes one plan for every generation it finds, and the plan header names the generations it covers.
  `--only legacy` limits the plan to a stage deployed under the earlier names, and `--only current` limits it to a stage under the current names.
  A failed or denied read of either generation makes the run incomplete and gives exit 8.

### Update with ham update

`ham update` updates the backend and then `ham` to the newest version on the main branch of the public repository.
It runs the same `./setup.sh --update`, from a copy of that version that it fetches for you.

```bash
ham update --check   # your versions, the newest version and the changes between; changes nothing
ham update           # asks once, waits for your active runs to end, then updates
ham update --yes     # the same without the question (for scripts)
```

- `--check` prints `ham vX`, `backend vY`, the newest version with its commit, and the lines of `CHANGELOG.md` between. `--check --json` prints the same as one JSON object.
- The update asks one question. Without a terminal it needs `--yes`, else it stops with exit 4 and changes nothing.
- It never cancels a run. While one of your runs is active it waits and prints the run ids and the minutes left every 30 seconds. A run that waits for your answer is marked; answer it with `ham say`.
- The wait lasts at most 60 minutes. `--wait <minutes>` changes the limit (1 to 1440), and `--no-wait` does not wait at all. When the wait ends with runs still going, `ham update` stops with exit 7 and makes no deployment or client change. Ctrl-C during the wait stops it with exit 4.
- The wait covers only your own runs, because `ham` sees only yours. `ham update` does not stop you or anyone else from starting a run while it waits or while the stage updates. Setup checks the runs of everyone on the stage and stops with exit 7 while any of them is still going; run `ham update` again later.
- The copy goes into your download folder as `ham-update-<first 12 characters of the commit>`. `ham` checks that it holds exactly the commit it showed you, with no changed or extra file. A copy that fails the check is named and left alone; move it aside and run again.
- It then runs that copy's `setup.sh --update --prefix <folder> --yes`, where `<folder>/bin` holds the `ham` you ran. Setup deploys the backend first, then installs the new `ham` and keeps the previous one beside it as `ham.prev`. Setup's own exit code is `ham update`'s; on exit 6 it prints the command that follows the running update. Setup ends with exit 3 when `ham` is installed and `ham doctor` found a problem, and with exit 8 when it is done and could not remove its runner. The update itself took effect in both cases, so `ham update` says what setup reported and still checks both versions; the exit code stays setup's unless a check fails. When setup stops with exit 7 because the stage records a commit that this clone cannot place, `ham update` names the folder of the clone: run the command that setup's message names there, for example `./setup.sh --update --revision <commit>`; running `ham update` again fetches the same clone and stops the same way.
- Afterwards `ham` and the backend must report the new version. When the new `ham` reports another version, `ham update` puts the previous `ham` back and exits 3; the backend is not rolled back. `Updated: ham vN.NN | backend vN.NN.` is the line of a finished update.
- It never moves a version back: a backend or a `ham` newer than main stops it with exit 7. A backend that reports no version stops it too; use `./setup.sh --update` from a clone then.
- The setting `update_source` names the repository: an `https://` URL or a `file:///` path to a mirror. The word `off` turns `ham update` off; it then makes no network call and exits 2. `ham config unset update_source` returns to the public repository.
- `ham update` runs from a shell. With several backends in `config.toml` it stops; run `./setup.sh --update` for each account.
- With one backend written as a `[[backend]]` table and no top-level `api_endpoint`, `ham update --check` works and the update itself stops. Setup reads and writes `api_endpoint`, `aws_profile` and `region` at the top level of the file. Write the backend's three keys there instead of in the table, then run `ham update` again.

### Client only

The client-only setup installs and configures `ham` for a stage that already exists.
You can use a stage someone else runs or one you deployed earlier.
You run `./setup.sh --client-only` with a `ham` program built with `--build`, supplied with `--from <directory>`, or already installed in the install directory or on PATH.
This setup creates no AWS resource.
It needs the AWS CLI only for SSO and assume-role profiles.
The full setup builds the client inside your AWS account and downloads it for you.

#### Choose how to get the binary

A client-only setup installs a `ham` that you supply or build.
The full setup builds it in your account and downloads it for you.

| Option | Use it when |
| --- | --- |
| `--from DIR` | A directory holds `ham` and `SHA256SUMS`. |
| `--from FILE --sha256 HEX` | You have one `ham` file and its SHA-256. |
| `--build` | You want to compile from a repository checkout. |
| none | A `ham` is already on your PATH. |

Without these options, the script also looks in `PREFIX/bin` and on PATH.
If it finds no binary on a terminal, it asks where `ham` comes from, as the second step of "What setup.sh does" describes.
With `--yes`, `--no-input` or no terminal, it stops with exit code 1 and the message `there is no ham binary to install`, and lists your choices.
You cannot combine `--from` and `--build`.

The directory can contain output from `scripts/build-macos-driver.sh`, the macOS cross-build script.
That script builds `ham` on a Linux build host.
A checksum mismatch is refused.
For a single file, `--sha256 HEX` needs 64 hexadecimal characters.

`--build` needs the Rust toolchain.
The repository pins Rust 1.98.1 in `rust-toolchain.toml`.
rustup installs that version on first use.
The build also needs a C compiler and linker named `cc` on PATH. On macOS, `xcode-select --install` installs one. On Linux, install `gcc` with your package manager.
Run setup from a repository checkout when using `--build`.
`ham` does not use `HAMH_AWS_PROFILE`, which picks the account entry for the deploy tools and the stage's service programs. If its exported value names no `accounts.json` entry, `cargo build` exits with 101. Run `unset HAMH_AWS_PROFILE` before `./setup.sh --build`.
It compiles with:

```bash
cargo build --release --locked -p ham --bin ham
```

#### Run setup.sh

Start with a dry run:

```bash
./setup.sh --dry-run
```

This shows the plan without writing anything.
It runs no `ham` command.

Here are two setup examples:

```bash
./setup.sh --client-only --build --endpoint https://abc123xyz.execute-api.us-east-1.amazonaws.com/direct --profile my-profile --region us-east-1
./setup.sh --client-only --from /path/to/macos-driver --endpoint https://abc123xyz.execute-api.us-east-1.amazonaws.com/direct --profile my-profile
```

The example endpoint is made up.
Get the real endpoint from the person who deployed the stage.
It is `V1ApiUrl`, the stage URL output, from `hamh-direct-control`, the CloudFormation stack.
The endpoint must start with `https://`.

These options control setup:

- `--client-only` installs and configures the client for a stage that already exists.
- `--prefix DIR` installs to `DIR/bin/ham`.
  `DIR` must be an absolute path.
  The default is `$HOME/.local`, giving `~/.local/bin/ham`.
- `--endpoint URL` sets the stage URL.
- `--profile NAME` sets the AWS profile for `ham`.
  Static keys, `credential_process`, SSO, and assume-role profiles work.
  SSO and assume-role profiles need AWS CLI version 2.9.0 or later.
- `--region NAME` sets the region, such as `us-east-1`.
- `--default-repo REPO` sets the repository the terminal app preselects.
  You use a CodeCommit repository whose name the stage's repository names cover. The default argument form is `cc:hamh-e2e-NAME`. A stage can allow other names.
- `--yes` or `-y` skips the questions and the confirmation.
- `--no-input` skips the questions and the confirmation.
  Use it with `--yes` or `--dry-run`.
- `--dry-run` shows the plan without changing anything.
- `--help` or `-h` shows help.

#### What setup.sh does

1. **Checks the machine.**
   macOS and Linux are the targets.
   The script refuses to run as root.
   It checks that `git` runs.
   Git is needed for:

   ```bash
   ham clone
   ```

   It checks AWS credential sources in this order:
   - The profile named with `--profile NAME`.
   - The profile in `awsProfile`, the ham config profile setting.
   - `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
   - The profile named by `AWS_PROFILE`.
   - The default profile.

   The `--profile` option sets the profile in the ham config.
   For profiles, it reads only AWS section and key names.
   It reports `credential_process`, static keys, SSO, or a role.
   It reads no credential value.
   A failed profile check lists the profiles it found.
   It shows the option that picks each profile.
   SSO and assume-role profiles are accepted.
   For these profiles, setup checks the AWS CLI version.
   It uses `aws` on the PATH or the program `HAMH_AWS_CLI` names.
   The AWS CLI must be version 2.9.0 or later.
   A missing or older AWS CLI stops setup before anything is installed.
   Setup then gives the [AWS CLI install page](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html).
   For an SSO profile, setup prints `aws sso login --profile NAME`.
   Setup asks the AWS CLI only for its version.
   Setup never writes to your AWS files.
   An unreadable ham config file is reported as such.
   An endpoint given or stored means `ham doctor` will run.
   That requires a Region before setup installs anything.
   Setup checks `--region`, the ham config, `AWS_REGION`, then the AWS profile.
   Without a Region, setup stops before installing.
   Missing `unzip` causes a warning.
   You need `unzip` only to open a zip by hand.
   Failed checks list every problem and leave everything unchanged.

2. **Asks for what you left out.**
   On a terminal, setup asks at most six questions, one after another.
   Each title reads `Question N: ...`, where N counts the questions this run asks, in order.
   A question whose option you gave is not asked and takes no number.
   Setup shows no total.

   The questions come in this order:

   - **What to set up.** Full setup, the default, or client-only setup. `--full`, `--client-only`, and any option that implies a mode skip it.
   - **Stage endpoint (`--endpoint`).** The client-only setup only.
   - **Where `ham` comes from (`--from`, `--build`).** The client-only setup only. It appears when setup finds no `ham` at `PREFIX/bin/ham` or on PATH and you give none of `--from`, `--sha256` or `--build`. You can choose to build from this checkout, to supply a directory that holds `ham` and `SHA256SUMS`, or to stop. The build choice appears only when `Cargo.toml` and `crates/ham` are present and `cargo` is on PATH.
   - **AWS profile (`--profile`).** Both setups.
   - **Region (`--region`).** Both setups.
   - **Where to build (`--build-on`).** The full setup only. It appears only when your machine has `cargo`, `cargo-zigbuild`, `zig`, `node` and `docker` on PATH. Choice 1 is the runner in your AWS account, the default. Choice 2 is your machine.
   - **Default repository (`--default-repo`).** Optional, and the client-only setup only. It appears only when an endpoint is known: typed, answered above, or stored in the ham config. Press Enter to skip it. Runs then return a zip.

   Press Enter to take the answer shown in brackets.
   Type `q` or press Ctrl-D to stop with nothing changed.
   Your answers fill the same settings as the options.
   Nothing is written before you confirm the plan.
   `--yes`, `--no-input`, and a missing terminal ask no questions.
   On a terminal, `--dry-run` asks the due questions.
   `--dry-run` writes nothing.

   The endpoint question asks for the stage endpoint, matching `--endpoint`.
   It applies to the client-only setup only.
   You can give its URL.
   The URL must use `https`.
   It should end with `/direct`.
   A URL without `/direct` is accepted with a warning.
   Every call to that URL then returns HTTP 403.
   With no stored endpoint, the question is a menu.
   After you choose client only at the first question, or when `--from` or `--build` implies client only, the menu offers three choices: you have its URL; deploy a stage into your own AWS account, which switches to the full setup; or `not now`, the default.
   If you typed `--client-only`, the menu offers two choices: you have its URL, or `not now`, the default.
   The deploy choice is absent then, because `--client-only` promises that nothing is created in your AWS account.
   `not now` writes no endpoint and skips `ham doctor`.
   Setup then names two routes to an endpoint.
   With a stored endpoint, Enter keeps it.
   You can type another endpoint.

   The source question appears on a terminal for the client-only setup, when no `ham` is installed and you gave none of `--from`, `--sha256` or `--build`.
   Its prompt is `Source [1]: `, and Enter takes choice 1.
   The build choice compiles `ham` from this checkout when `Cargo.toml` and `crates/ham` are present and `cargo` is on PATH.
   The directory choice asks `Directory with ham and SHA256SUMS: ` for a directory that holds your built client.
   The stop choice prints `there is no ham binary to install` with the ways to get a client, and exits with code 1 and nothing changed.
   A person who answers "deploy one for me" at the endpoint question gets the full setup, which builds the client in the account, and is not asked this question.
   With `--endpoint` given, the source question is the first question of the run.
   With `--yes`, `--no-input` or no terminal, setup asks no source question and stops with exit code 1 and the message `there is no ham binary to install`.

   The profile question asks for the AWS profile, matching `--profile`.
   Its numbered menu reads `~/.aws/config` and `~/.aws/credentials`.
   Each row shows the profile's kind.
   Kinds include `credential_process`, static keys, SSO, and assuming a role.
   Row 0 offers AWS variables only when both keys are set.
   Those keys are `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
   The default comes from the ham config first.
   Next come the AWS keys, then `AWS_PROFILE`.
   Next comes the profile named `default`, then the only usable profile.
   Every kind of profile in the menu is usable.
   SSO and assume-role rows say AWS CLI version 2.9.0 or later is needed.
   Choosing either kind is accepted like any other profile.
   For SSO, setup prints `aws sso login --profile NAME`.
   Setup never edits your AWS files.
   Press `r` to read the files again after adding a profile.

   The Region question asks for the Region, matching `--region`.
   Its default comes from the ham config first.
   Next comes the Region in the endpoint's host name.
   Next come `AWS_REGION` and the chosen profile's region line.
   A Region differing from the endpoint's host Region gets a warning.
   Setup then asks `Keep ... anyway? [y/N]`.

   The default repository question asks for an optional default repository, matching `--default-repo`.
   It applies to the client-only setup only.
   This is a CodeCommit repository that ham preselects.
   Setup asks this question only when an endpoint is known.
   Press Enter to skip it.
   Without a default repository, runs return a zip.
   Setup never asks for a key, secret, or token.

3. **Shows the plan and asks for permission.**
   The question is `Go ahead? [y/N]`.
   `--yes` and `--dry-run` skip that question.
   `--no-input` also skips that question.
   `--no-input` needs `--yes` or `--dry-run`.
   Without either, the client-only setup exits 2.
   Without a terminal, setup needs `--yes` or `--dry-run`.
   Otherwise, it stops and tells you to add either option.
   After you answer a question, the plan ends with an `again:` line.
   That line gives a command repeating your answers as options.
   It includes `--yes`, so another run asks nothing.
   For `--build`, the plan says a first build takes about 11 minutes.
   It needs 3 GB of disk.
   It also needs `rustup`.

4. **Installs the binary to `PREFIX/bin/ham`.**
   The checksum is checked.
   Setup creates the relative link `cah` to `ham` beside the installed binary.
   If `PREFIX/bin` is missing from PATH, it prints:

   ```bash
   export PATH=...
   ```

   It never edits your shell startup file.

5. **Checks that the binary runs offline.**
   It runs:

   ```bash
   ham models
   ```

   It also prints the installed version with `ham --version`.

6. **Writes config values from your options.**
   It writes values from your options and your answers.
   It uses:

   ```bash
   ham config set
   ```

   The endpoint goes into `api_endpoint`, the endpoint setting.
   The AWS profile goes into `aws_profile`, the profile setting.
   The Region goes into `region`, the Region setting.
   The default repository goes into `default_repo`, the default repository setting.
   This keeps the config file valid.
   It never invents an endpoint.
   Without config options or answers, it leaves the config file untouched.

7. **Initializes the rules.**
   It runs:

   ```bash
   ham rules init
   ```

   It replaces only default or pack files holding earlier text.
   Those files must also be unedited.
   A file you edited is kept.

8. **Runs doctor when an endpoint is set.**
   It runs:

   ```bash
   ham doctor
   ```

   The client-only setup makes no AWS call of its own.
   `ham doctor` is its one network check.
   The catalog check states how old the newest model smoke record is and passes whatever its age.
   Setup runs doctor without an age limit, so a current ham shows no `WARN catalog` line.
   A `FAIL catalog` line means no model is selectable.
   Setup then exits 3.
   A `WARN versions` line means ham and the stage backend report different versions.
   That line is advice and does not stop the run.
   After doctor, setup repeats which account ham signs as.
   That must be the account that hosts the stage.
   A `FAIL identity` line gets an account hint.
   The stage accepts callers from its own AWS account only.
   Your profile must belong to that account.
   Without an endpoint, setup skips doctor.
   It names two routes to an endpoint.
   To get an endpoint, ask the person who runs the stage or run the full setup.
   Setup also prints what to run next.

9. **Prints the next steps.**

The script writes no secret.
It deletes nothing.

### Without setup.sh

You can do each step by hand.
Use the commands in "Your first run" below.

The credentials requirement matches "Before you start": a static-key, `credential_process`, SSO or assume-role profile, or `AWS_*` variables.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Done. |
| 1 | A prerequisite or step failed. |
| 2 | Bad usage. |
| 3 | Installed, but `ham doctor` found a problem. |
| 4 | You cancelled, or a required confirmation is missing. Examples: full setup with `--yes` without `--acknowledge-costs`, full setup with `--no-input` without `--yes`, or a teardown with `--yes --acknowledge-teardown` and no `--account-id`. |
| 5 | Bootstrap, deploy, or initialization failed. After `The stage is unchanged.`, the deploy refused for a reason that waiting does not cure. |
| 6 | An operation is running and is safe to resume. Run `setup.sh` again to attach. `--status` also returns this code while a build is running or the stage is being changed. |
| 7 | A conflicting operation, source mismatch, or configuration mismatch. Examples include a dirty clone, a different account from `--account-id`, a running build with other inputs, active runs blocking an update, a deploy lock that another setup holds, a commit older than the stage's, a stage commit that your clone does not hold and whose VERSION is not lower than yours, another operator without `--change-operator`, another operation changing the stage, or a result or endpoint mismatch. A result mismatch installs nothing. |
| 8 | Cleanup is incomplete. A stack, a retained resource, the runner, another resource of the stage, a failed step or a failed read remains. |
| 9 | Teardown removed everything except the network resources that an AgentCore-managed network interface holds. Run it again after AWS releases the interface, or add `--network-wait-minutes full`. |

### Moving from the earlier command name

You need this section if you use the alias `cah` or find the earlier names in your files or AWS account. Your files stay intact, and the earlier names keep working until the end versions listed below.

**The command**

- `setup.sh` installs `ham` and a relative symbolic link named `cah` that points to `ham`. Both live in `~/.local/bin`, or in `DIR/bin` with `--prefix DIR`.
- The alias `cah` prints one notice on standard error. Standard output matches `ham`. The exit code also matches.
- Building the client yourself or copying a binary by hand makes no alias. You can create one in the install folder with `ln -s ham cah`.
- Setup replaces an existing `cah` only if it is a link to `ham` or a recognised program from the earlier release. Recognition uses a SHA-256 hash in a setup receipt or the text `x-cah-backend-version` in the file. Setup keeps any other file named `cah` and prints a warning.
- `./setup.sh --client-only --from <dir>` exits 1 before changing anything if the directory holds only a `cah` program.
- The message box accepts `cah` as a command word alongside `ham`.
- A line starting with `ham` and an unknown lower-case word is refused with the closest command names. The hint is `To send it as chat, use /send <text>`.
- Pull request titles and message prefixes read `ham: ` even when you start the program with the alias `cah`.

**The notice and the switch**

- Compatibility notices are on by default. You can silence all of them with `HAMH_NO_DEPRECATION_NOTICES=1` or `ham config set ui.deprecation_notices off`.
- The setting `ui.deprecation_notices` is a true-or-false key in the `[ui]` table of `config.toml`. The environment variable forces notices off whatever the file says. The switch takes effect at the next start of `ham`.

**Settings and rules folder**

- `~/.config/ham` counts as populated only when it contains `config.toml`, `engines/`, `rules/`, `skills/`, `continue.md` or `review.md`. An empty folder does not count.
- If only `~/.config/cah` holds content, `ham` reads it read-only and prints one notice. A command that needs to write exits 2 and tells you to run `ham config migrate`. Nothing writes the earlier folder.
- If both folders hold content, `~/.config/ham` wins. `ham config show` says the earlier folder is not read.
- You can preview the copy, then copy your settings and rules.

```bash
ham config migrate --dry-run
ham config migrate
```

- Migration copies the listed content byte for byte and keeps file modes. It uses a temporary sibling folder and renames it into place to avoid a partial destination. It refuses symbolic links and exits 2 if a different file already exists at the destination.
- The earlier folder stays untouched. You can delete it when you no longer need it. Migration honours `XDG_CONFIG_HOME` for both folder names.
- `setup.sh` runs migration in its Config step when only the earlier folder exists. A plain `ham` never migrates by itself. Migration never copies `setup/`, the folder of the setup receipts.

**Environment variables**

- Each documented variable that starts with `CAH_` is read when the variable with the same name and the prefix `HAMH_` is unset. You see one notice per variable name per process on standard error before the terminal screen opens. The `HAMH_` value wins when both spellings are set, except for conflicting selectors.
- The selectors are `CAH_AWS_PROFILE`, `CAH_ACCOUNTS_FILE` and `CAH_OWNER`, with current names `HAMH_AWS_PROFILE`, `HAMH_ACCOUNTS_FILE` and `HAMH_OWNER`. Different values for both spellings of a selector make the command exit 2. A selector is never ignored and never falls through to the default account.
- Other test and development variable names carry only the new prefix. Commands you run never fail because of those names.
- Accounts-file readers accept both `CAH_AWS_PROFILE` and `HAMH_AWS_PROFILE` as the selector word. New accounts files carry `HAMH_AWS_PROFILE`.
- You can replace the `CAH_` spellings in your environment with their `HAMH_` spellings.

**Records folder in your repositories**

- A run chooses one records folder per repository once per run. If exactly one of `.ham/` and `.cah/` holds state, it uses that folder. If neither holds state, it uses `.ham/`.
- If both folders hold state, the run refuses to start or continue and names both. An empty `.ham/` never hides a populated `.cah/`.
- The first line of the prompt names the chosen folder. Every tool that writes records receives that folder. `.cah/test-command` is read when `.ham/test-command` is absent.
- A session sets both `HAMH_` and `CAH_` spellings of `CONTEXT_DIR`, `ATTACHMENTS_DIR`, `RESEARCH_URL`, `RESEARCH_WAIT_S` and `TOOLS_URL`. The session image keeps `cah-research` and `cah-services` as links to `ham-research` and `ham-services`. Your rules and skills that use those earlier names keep working.
- Stored-record readers accept earlier keys such as `cahSha256` and schema ids such as `cah-research-item/1`. New records carry the new names.

**Repositories and pull requests**

- You type the repository name for `--new-repo`. The client generates no repository name. A stage deployed under the current names accepts `hamh-e2e-new-<yyyymmdd>`. A stage deployed under the earlier names accepts `cah-e2e-new-<yyyymmdd>`. The stage refuses names outside its forms.
- For a stage that sends only `x-cah-backend-version`, the `versions` line of `ham doctor` shows a WARN. The warning says the stage is deployed under the earlier names. It says the stage still works with this `ham`. It advises that `--new-repo` takes names starting with `cah-e2e-new-`. A stage that sends the current header gets no such advice. The advice ends with version 5.50. After 5.50, you type an explicit `cah-e2e-new-<yyyymmdd>` name for `--new-repo` against a stage deployed under the earlier names.
- Existing repositories with the prefix `cah-e2e-` remain accepted as pull request targets by the delivery check, IAM patterns and teardown. Each applies its own ownership check. Teardown keeps `cah-e2e-*` repositories.
- A repository rule that lists the default entry `hamh-e2e-*` also covers names that start with `cah-e2e-`; a rule without that entry covers exactly its entries. The new stage has its own allowlist, so each earlier repository still needs `ham-admin repos allow cc:<name>`.

**A stage deployed under the earlier names**

- A stage uses the earlier names when its name prefix and project tag both belong to that generation. `./setup.sh --status` reports `state=legacy`, or `state=legacy-retained` when only a retained VPC or bucket remains. `./setup.sh --doctor` fails with the same sentence.
- `./setup.sh --update` and `./setup.sh --full` exit 7 before creating anything. The earlier stage keeps running. `ham` still talks to it, so `ham pull` works against its endpoint.
- No data moves to a new stage. A new stage starts empty.

To replace the stage, follow these steps:

1. You install the client against the endpoint with `./setup.sh --client-only --endpoint <old endpoint URL>`; on a machine without `ham`, you supply `--build` or `--from <directory>` or answer the prompt asking where `ham` comes from. You read the endpoint URL from the output `V1ApiUrl` of the CloudFormation stack `cah-direct-control` when you do not have it.
2. You run `ham pull <runId>` for each run you want to keep.
3. You run `./setup.sh --teardown`. It makes one plan for both generations by default. `--only legacy` or `--only current` limits the plan to that generation.
4. You run `./setup.sh --full`.

**When the earlier names stop working**

- Version 5.50 ends the alias `cah`, the environment-variable fallback, the config-folder fallback, the earlier backend-version header and the accounts-file selector word `CAH_AWS_PROFILE`.
- Version 6.00 ends support for the records folder `.cah/`, the earlier session helper commands and variables, repository names starting with `cah-e2e-`, earlier stored-record keys and schema ids, and teardown of a stage deployed under the earlier names.

After 5.50, the alias `cah` is gone without a message. The `CAH_` variable fallback ends without a message. The `~/.config/cah` fallback ends without a message. You copy settings from `~/.config/cah` with `ham config migrate` before 5.50. The fallback for the earlier header `x-cah-backend-version` ends without a message. The advice from `ham doctor` about repository names is gone.

After 6.00, the held-data fallbacks end. These fallbacks cover the records folder `.cah/`, session helper links and variables, repository names starting with `cah-e2e-`, earlier stored-record keys, and earlier schema ids. For held data that needs these fallbacks, `ham` refuses with a message that names the old folder, link or variable. Teardown of a stage deployed under the earlier names ends. Detection of a stage deployed under the earlier names never ends.

## What a run does, in plain words

1. `ham` sends your prompt, rules, and model name in one signed call.
2. The service checks the request before writing anything. It refuses an empty request, a request that is too large to send (see "Limits" at the end of this guide), unlisted models and invalid rules. The prompt itself has no size limit of its own.
3. A workflow prepares the base by copying your repository into S3. For `--s3-only`, it puts an empty base in S3.
4. One session of the engine of the run starts in an isolated AgentCore microVM, a virtual machine. The session uses short-lived credentials. Your prompt reaches the engine word for word. The engine works until it stops. If the model provider fails meanwhile, the session resumes the engine and may switch to another model; see "When the model provider fails".
5. Every 10 minutes, changed files trigger a snapshot saved to S3. When Cline stops, the session commits what the files hold. It then uploads the full git history.
6. The service checks the bundle for secrets, protected paths, and size limits. It opens a CodeCommit pull request when a repository is targeted. It builds the zip and records the outcome label.
7. `ham` shows progress and downloads the zip. It prints a card with the links.

The whole path has 23 workflow states; you do not need them to use `ham`.

The zip contains the code the engine left. It includes the full git history in `repo.bundle`.

The product does not run or judge the work.
It records how the engine stopped.

The label says "Cline reported done" or "OpenCode reported done" only when that engine says so.
Otherwise, it says "<Engine> stopped: <reason>", using the engine's name.

### Does a run use several agents?

The harness uses one Cline session and starts no other agent.
The harness has no planner and no roles.
A run on OpenCode is the same: one session, and OpenCode starts its helper agents itself with its `subagent` tool.

Cline's `spawn_agent` tool starts a sub-agent: a helper that Cline starts itself.
The harness leaves `spawn_agent` on by default.
You can switch it off for your runs with `tools.disable` in the engine file; see [Engine settings](#engine-settings).
The harness has no tool to start, steer or stop sub-agents.
Cline chooses whether to use sub-agents, and its choice differs by task.

A run waits for its sub-agents, and the wait has limits. Cline cannot go on with the step that started them until they return, and no file changes in the meantime. The harness cancels
the wait when no sub-agent has returned a result for 25 minutes, or when the oldest open sub-agent has run for 75 minutes. A sub-agent that starts further sub-agents does not restart the
25 minutes; only a result that comes back does. Cline has no way to cancel one sub-agent, so the harness cancels the whole prompt, and every sub-agent that is still running ends. It then
tells Cline, in the same session, that its sub-agents did not return, that their findings are missing and that it should finish with its own tools and not start sub-agents again. The run
goes on and can still end with Cline's own final message. The run's events show one `goal-loop` line that names the limit that was reached. The harness does this at most twice in a run.
A third wait that reaches a limit ends the run like the other stalls of the goal loop do. The two hour limit with no changed file stays the last limit. The person who runs the stage
changes the two limits with `AGENTD_GOAL_HELPER_SILENCE_S` and `AGENTD_GOAL_HELPER_WAIT_S`; `0` switches one off. The limits apply while the goal loop runs. With `--goal off` or
`AGENTD_GOAL=off`, a run has no such limit.

### Tools the harness gives Cline

The tool layer is off by default on a stage.

Cline, the coding agent, does the work of a run. Besides its own tools, it can call tools that the harness provides. A small tool server offers them. The server runs inside the run's microVM, the run's own small virtual machine. agentd, the program that supervises Cline there, starts it.

The server speaks MCP, the protocol Cline uses to call such tools. It listens on `127.0.0.1` inside the microVM. That is the loopback address: nothing outside the microVM can reach it. It uses port `3130`, or any free port when `3130` is taken.

The server offers one tool: `code_search`. Cline shows it as `ham__code_search`. That is the server name `ham`, two underscores and the tool name. The stored events of a run use the joined name. The run summary uses the bare name `code_search`. OpenCode shows the same tool as `ham_code_search`; the tool is the same for both engines.

Cline's own search tool, `search_codebase`, stays on. Cline decides which tool to use.

#### Who switches it

- The layer is off by default on a stage. A stage is one deployed copy of the harness in an AWS account.
- The person who runs the stage turns it on or off at deploy time with `HAMH_TOOLS_MODE`. The values are `on` and `off`.
- A change reaches new runs only. A run that is already running keeps the setting it started with.
- `ham` has no config key, flag or command for the tool layer. Cline reads the tool settings itself at the start of each session.
- You cannot switch the layer off for one run. The engine setting `tools.disable` takes only `spawn_agent` and `fetch_web_content`, so it does not touch `ham__code_search`. See [Engine settings](#engine-settings).
- With the layer off, no server starts and the run summary has no `tools` block.
- If the server cannot start, the run goes on without the tools and the block says why. The tool server never ends a run.

#### How to tell whether a run used it

A run that had the layer on has a `tools` block in its run summary. agentd writes the summary file `summary.json` when the run ends. The delivery step copies the summary into `evidence/direct.json`. The block is the member `summary.tools` of that file.

To read it, pull the zip with `ham pull <runId>` and unzip it. Or pull only the evidence zip with `ham pull <runId> --artifact evidence`. The zip holds one top folder, `<repo>-<runId>-<sha7>/` (`<repo>-<runId>-<sha7>-evidence/` in the evidence zip). The file `evidence/direct.json` sits inside it. After unzipping, change into that folder. There, `jq .summary.tools evidence/direct.json` prints the block.

A run without the block was started while the layer was off, or on a stage built before the layer existed. The block holds words and counts only. It never holds a search pattern, a path or a result.

| Member | Meaning |
| --- | --- |
| `enabled` | `true` when the server started and its tool list was registered. `false` when it did not start. |
| `port` | The loopback port the server bound. |
| `connect` | What the harness saw of Cline's connection. Left out when no session was judged. |
| `list` | The bare names of the tools the server offers. At most 16. Today `["code_search"]`. |
| `counts` | Left out when `enabled` is `false`. |
| `counts.calls` | Calls per tool, including calls that failed or were refused. |
| `counts.failed` | Calls that ran and failed: `error: unavailable`, `error: timeout` or an internal fault. |
| `counts.refused` | Calls the server did not run: `error: usage`, `error: busy` or `error: refused`. |
| `counts.truncated` | Calls whose answer was cut short. |
| `counts.restarts` | How often the server task ended and started again on the same port. At most 3. |
| `counts.p50Ms` | The median time of the calls that ran, in milliseconds. Left out when no call ran. |
| `counts.p95Ms` | The 95th-percentile time of the calls that ran, in milliseconds. Left out when no call ran. |

Cline connects to the server once per session. Thirty seconds after the first prompt of a session, the harness checks what the server saw. `connect` holds one of three words:

| Word | Meaning |
| --- | --- |
| `none` | Nothing arrived from Cline. |
| `handshake-only` | Cline said hello (`initialize`) and never asked for the tool list. |
| `ok` | Cline asked for the tool list. |

When a run has several sessions, `connect` shows the worst word of the sessions that were judged. A session that ends before its 30 seconds are over is judged only if the tool list had already arrived.

Cline skips a tool server it cannot read, and it does not say so. A block with `enabled` `true` and `connect` `none` or `handshake-only` therefore means the server ran and Cline did not use it. Use `counts.calls` to see how many calls Cline made.

This example comes from the fixture of the protocol tests. Its numbers are not from a run.

```json
{"enabled": true, "port": 3130, "connect": "ok", "list": ["code_search"], "counts": {"calls": {"code_search": 3}, "failed": 0, "refused": 1, "truncated": 1, "restarts": 0, "p50Ms": 181, "p95Ms": 318}}
```

#### What `ham__code_search` accepts

Cline calls the tool with a JSON object. The server refuses a member that is not in this table. A work tree is the run's repository. A glob is a file-name pattern such as `*.rs`.

| Argument | Type | Allowed values | Default | Meaning |
| --- | --- | --- | --- | --- |
| `pattern` | text | Required. 1 to 1,000 characters, one line, no NUL byte. | none | A regular expression (Rust syntax), or a literal when `fixed` is `true`. |
| `fixed` | true or false | | `false` | Treat `pattern` as literal text. |
| `paths` | list of text | At most 10 entries, each 1 to 1,024 bytes. | the whole work tree | Files or directories to search. |
| `glob` | list of text | At most 10 entries, each 1 to 1,024 bytes. | none | Include globs. A leading `!` excludes. |
| `maxResults` | whole number | 1 to 200 | `50` | Stop after this many matches. |
| `maxPerFile` | whole number | 1 to 50 | `10` | At most this many matches per file. |
| `filesOnly` | true or false | | `false` | Return only the names of files that match. |
| `count` | true or false | | `false` | Return match counts per file. `filesOnly` and `count` together are refused. |
| `context` | whole number | 0 to 5 | `0` | Lines of context around each match. |
| `includeHidden` | true or false | | `false` | Also search hidden files and directories. |
| `includeIgnored` | true or false | | `false` | Also search files that `.gitignore` excludes. |

- The search is case sensitive. A search of the whole work tree leaves out the `.git` folder. A `paths` entry inside `.git`, or a `glob` entry that matches it (`.git`, `*` or `**`), searches it. Files larger than 2,097,152 bytes (`--max-filesize=2M`) are skipped. Links are not followed.
- A relative entry in `paths` is read against the work tree. An absolute entry must lie inside the work tree or inside the run's context folder. The context folder sits outside the repository and holds the files that research places. An entry that leaves them, through `..` or a link, is refused. An entry that does not exist ends in `error: usage`.
- That check is the tool's own rule. It is a second line of defence and no security boundary: Cline's shell runs as the same user and can read the same files.
- The tool runs ripgrep, a fast text search program, as the same user as Cline. Its environment holds no AWS variables.
- Results come in path order, so the same call returns the same first matches.

#### What `ham__code_search` returns

| Answer | Shape |
| --- | --- |
| Matches | One `path:line:text` line per match. Lines of a file come in line order. |
| Matches with `context` above 0 | Context lines read `path-line-text`. A line with `--` separates the groups. |
| `filesOnly` | One file name per line. |
| `count` | One `path:number` line per file. A last line `total: <N> matching lines in <M> files` follows when `maxResults` or the size limit did not cut the answer short. A cut-short answer ends with a `[stopped at ...]` note and has no total. |
| Nothing found | `no matches` |

Paths in the work tree are relative to it. Paths in the context folder are absolute. A line of text over 300 characters is cut to 300 characters, counting the `…` marks. The cut keeps the first match in view and shows `…` at each end that was cut. Control characters other than tab show as the replacement character.

The text of an answer is held to 7,000 characters. The whole result, with its wrapping, is held to 7,500. Cline cuts a result at 8,000 characters, so the tool stays under that.

When an answer is short on purpose, its last lines are notes. A note starts with `[` and names the reason:

| Note | Meaning |
| --- | --- |
| `[stopped at 50 matches (maxResults); narrow the pattern, paths or glob, or raise maxResults]` | The call reached `maxResults`. The number is the value of the call. |
| `[maxPerFile 10 reached in 2 files; raise maxPerFile or narrow paths]` | Some files had more matches than `maxPerFile`. |
| `[stopped at the size limit of one answer after N matches; narrow the pattern, paths or glob]` | The answer reached its size limit. |
| `[N line(s) too long to show was/were skipped]` | The search skipped lines that were too long to read. |
| `[search incomplete: some results may be missing]` | The search could not read every file. |
| `[shortened: showing N of M characters]` | The server cut the text to fit the 7,500 characters. |

A failed call starts with `error:` and one of five words. The result is marked as an error. Cline still passes it to the model as a finished call, so the model reads the text.

| Word | Meaning |
| --- | --- |
| `usage` | The arguments are wrong. The text names the member and the rule and never repeats a value. A pattern or glob that ripgrep does not accept also ends here. The text advises to check the expression and the glob, and to set `fixed` to search for literal text. An entry of `paths` that does not exist ends here too. |
| `busy` | Four searches already run, or the server already holds eight calls. Retry shortly. |
| `refused` | A path lies outside the work tree and the context folder, or it exists and cannot be opened. |
| `timeout` | A search ran longer than 10 seconds. The server stops it. Narrow the paths or the pattern. |
| `unavailable` | ripgrep could not start or failed, the work tree is not available, or the search output could not be read. |

#### What it cannot do

- It does not search the web or the network. It runs no code and changes no file.
- It searches only the work tree and the context folder.
- It does not match across lines. A pattern is one line.
- It skips files larger than 2,097,152 bytes, and a call cannot raise that limit. It skips hidden and git-ignored files unless the call sets `includeHidden` or `includeIgnored`.
- Four searches run at once, and a search stops after 10 seconds. The server accepts a request body of at most 64 KiB and runs at most 8 calls at once.

## Your first run

If `setup.sh` already did some of these steps, skip them.

Set the endpoint URL supplied by your operator.

```bash
ham config set api_endpoint https://abc123xyz.execute-api.us-east-1.amazonaws.com/direct
```

Set the stage's Region.

```bash
ham config set region us-east-1
```

Set the profile that supplies your credentials.

```bash
ham config set aws_profile my-profile
```

The config file is `~/.config/ham/config.toml`. With `$XDG_CONFIG_HOME`, it is `$XDG_CONFIG_HOME/ham/config.toml`.

Print every config key.

```bash
ham config
```

The keys are `api_endpoint`, `region`, `aws_profile`, `default_repo`, `download_dir`, `default_model`, `input_wait_minutes`, `mouse`, `report_tests`, `run.goal`, `run.rules`, `run.skills`, `research.enabled`, `ui.notify`, `ui.bell`, `ui.mascot`, `run.report_command_minutes`, `run.stall_calls`, `run.stall_window_minutes`, `run.stall_min_calls`, `run.command_grace_minutes`, `run.final_message`, `run.lifecycle_flow`, `run.review_round`, `run.compute`, `run.stall_new_calls`, `run.cargo_no_debug_info`, `run.cargo_no_incremental`, `run.exclude_target_dir`, `run.outside_checkout_note`, `ui.deprecation_notices`, `ui.mascot_motion`, `run.max_run_hours`, `run.provider_sharing`, `default_backend`, `customer_content`, `ui.copy_limit_kib`, `run.max_interrupts`, `run.max_questions`, `run.max_fresh_sessions`, `research.max_file_mib`, `research.max_unpacked_mib`, `research.max_run_mib` and `run.container_signals`, `default_engine`, `update_source`.

Create your rules file and `continue.md`, the continuation file.

```bash
ham rules init
```

This command never overwrites a file you edited.

List the six models offline; `*` marks the next-run model.

```bash
ham models
```

Check identity, api, versions, catalog, git-host, allowlist and provider-data-sharing.

```bash
ham doctor
```

Each check prints one PASS, WARN or FAIL line. The exit code is 0 unless a check fails. A WARN line is advice. The `catalog` line states how old the newest model smoke record is and passes whatever its age. `ham doctor --catalog-max-age 24` prints `WARN catalog` when that record is more than 24 hours old; the value is a whole number of hours, 1 or more. The `git-host` line shows how each delivery target is reached: `cc:` (CodeCommit) uses the stage's role credentials and no git secret, and `gh:` (GitHub) and `gl:` (GitLab) are not supported. The `provider-data-sharing` line shows `run.provider_sharing` and the account's Bedrock data retention mode in the stage's Region; see "Models that need provider data sharing".

Start a run without a repository.

```bash
ham "Add a function slugify(text) to a new module slugify.py with pytest tests" --s3-only --direct
```

Example output (the values are illustrative):

```text
started run 01JAB2CDEF3GH4JKMNPQRSTVWX
run 01JAB2CDEF3GH4JKMNPQRSTVWX active (green) agents 1 model Claude Sonnet 5.5
run 01JAB2CDEF3GH4JKMNPQRSTVWX active (delivering) agents 1 model Claude Sonnet 5.5
run 01JAB2CDEF3GH4JKMNPQRSTVWX complete (delivered) agents 1 model Claude Sonnet 5.5 outcome: Cline reported done
Run       01JAB2CDEF3GH4JKMNPQRSTVWX complete (delivered)
Outcome   Cline reported done
Model     Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)
PR        n/a
Zip       /Users/you/Downloads/s3-only-01JAB2CDEF3GH4JKMNPQRSTVWX-1a2b3c4.zip (sha256 9f8e7d6c5b4a, 48213 bytes)
Link      https://<presigned-url> (expires <time 15 minutes later>)
Clone     ham clone 01JAB2CDEF3GH4JKMNPQRSTVWX
Push      ham push 01JAB2CDEF3GH4JKMNPQRSTVWX <git-url>
Tokens    1200000  USD 1.54
```

- The first line appears on standard output. `ham` also prints the model line on stderr. For example: `model: Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5), from built-in default`.
- On a terminal without `--json`, `ham` opens the full-screen dashboard. Lines appear with `--json` or when output is not a terminal.
- A direct run has no gates. Its status line shows no gate counts, and its card has no Evidence block.
- The card prints the outcome label on its Outcome line, and the model the run used on its Model line. The pull request description contains the label too. Inside the zip, `EVIDENCE.md` and `evidence/direct.json` contain it. The `run.completed` event also contains the label. See "Reading the outcome label".
- The card's Outcome line shows the label from `run.completed` (the harness's outcome event). The harness writes the zip announcement first. It writes `run.completed` right after that announcement. Once the run completes and announces its zip, `ham watch` keeps reading. It waits up to 30 seconds for `run.completed`. A cleared or aborted run skips this wait. If the event does not arrive, ham prints this on stderr: "ham: run.completed did not arrive within 30 s; the card has no outcome label". The Outcome line then reads "not received". The zip still downloads. The exit code still comes from the run status. `ham watch <runId>` shows the label once the harness writes it.
- A run that switched to another route or model can end on another model than the one you asked for. Then the card shows three lines. The Detail line starts "Ran on <model name> after <n> switches." It uses the model's display name. The cause follows as a sentence with a capital letter and full stop. The Model line shows the final model's name and id. The Requested line shows the requested model's name and id. The dashboard adds "Ran on <model name> after <n> switches" below Model. The line-mode status adds "ran on <model name> after <n> switches" after the model. Another route for the requested model has the same name. That route uses another inference profile. Its id appears beside the name. Runs without switches have no Requested line. Runs that switch back to their requested model have no Requested line. The `--json` output keeps the harness's detail text unchanged. This example requested Claude Sonnet 5.5 and ended on GLM 5.3. It switched 9 times.

  ```text
  Outcome   Cline stopped: the model provider failed
  Detail    Ran on GLM 5.3 after 9 switches. Engine error -32603: Internal error: The operation timed out.
  Model     GLM 5.3 (us.zai.glm-5.3)
  Requested Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)
  ```
- The zip goes into the download directory. `HAMH_DOWNLOAD_DIR` sets that directory first. Next comes `downloadDir` from the config, then the current directory.
- The exit code is 0 when the run status is complete. It remains 0 even if Cline stopped halfway, because the zip exists. Exit 0 does not mean "Cline reported done".

For a repository run, replace `--s3-only` with `--repo cc:hamh-e2e-<name>`. Add `--base <ref>` to start from another branch. You use `--new-repo cc:<name>` to create the repository first. You must use the name form `hamh-e2e-new-<yyyymmdd>`.

Start a run with this command form:

```bash
ham "<prompt>" ... --direct
```

Use `--prompt <text>` when the prompt starts with `-`. Also use it when the prompt equals a subcommand name. `--direct` is required for the argv form.

These commands do not exist: `ham login`, `ham logs`, `ham run`, `ham start`, `ham status`, and `ham stop`. You can use `ham --help`, `ham -h` or `ham help` to print the usage screen. You can use `ham help <command>` or `ham <command> --help` to see a command's usage forms, explanation and options. You get exit code 0 for help or 2 for `ham help <word>` when the word is not a command. You can request help before any setup. `ham --version` and `ham -V` print the version of ham (see "The version tag").

## Your settings

ham keeps your choices in a plain-text file.
The file uses TOML format.
You can edit the file by hand.
You can also use commands or the [Settings screen](#settings).

The client file is `~/.config/ham/config.toml`.
If `$XDG_CONFIG_HOME` is set and not empty, use `$XDG_CONFIG_HOME/ham/config.toml`.
The file holds no secret.
ham reads credentials from your AWS profile or `AWS_*` variables.
It reads those credentials when it signs a call.

Cline also has an engine file, `engines/cline.toml`, for the settings of the coding engine itself.
The section [Engine settings](#engine-settings) describes it.
The rest of this chapter is about the client file.

You need to change nothing to use the defaults.
With no file, a run sends the same request as before.
The file that `setup.sh` writes also leaves that request unchanged.

The file has 46 keys: `apiEndpoint`, `region`, `awsProfile`, `defaultRepo`, `downloadDir`, `defaultModel`, `inputWaitMinutes`, `mouse`, `reportTests`, `run.goal`, `run.rules`, `run.skills`, `research.enabled`, `ui.notify`, `ui.bell`, `ui.mascot`, `run.report_command_minutes`, `run.stall_calls`, `run.stall_window_minutes`, `run.stall_min_calls`, `run.command_grace_minutes`, `run.final_message`, `run.lifecycle_flow`, `run.review_round`, `run.compute`, `run.stall_new_calls`, `run.cargo_no_debug_info`, `run.cargo_no_incremental`, `run.exclude_target_dir`, `run.outside_checkout_note`, `ui.deprecation_notices`, `ui.mascot_motion`, `run.max_run_hours`, `run.provider_sharing`, `ui.copy_limit_kib`, `run.max_interrupts`, `run.max_questions`, `run.max_fresh_sessions`, `research.max_file_mib`, `research.max_unpacked_mib`, `research.max_run_mib`, `run.container_signals`, `defaultEngine`, `defaultBackend`, `customerContent` and `updateSource`.
The first nine and the last two are top-level keys in camelCase.
The thirty-three between them sit inside tables.
The `[run]` table holds `goal`, `rules`, `skills`, `report_command_minutes`, `stall_calls`, `stall_window_minutes`, `stall_min_calls`, `command_grace_minutes`, `final_message`, `lifecycle_flow`, `review_round`, `compute`, `stall_new_calls`, `cargo_no_debug_info`, `cargo_no_incremental`, `exclude_target_dir` and `outside_checkout_note` and `provider_sharing`.
The `[research]` table holds `enabled`.
The `[ui]` table holds `notify`, `bell`, `mascot`, `deprecation_notices` and `mascot_motion`.
Commands name a key of a table with the table name and a dot, as in the list above.

ham writes `configVersion = 1` with the first table key.
You do not type that line.

The file must be UTF-8 text.
Its size must be at most 64 KiB.
It must contain no NUL byte.
It must contain no byte-order mark.

Here is a file after the goal loop is turned off in the Settings screen.
Your own comments stay where you put them.

```toml
# my ham settings
apiEndpoint = "https://abc123xyz.execute-api.us-east-1.amazonaws.com/direct"
region = "us-east-1"
awsProfile = "my-profile"
configVersion = 1

[run]
goal = false
```

### The config commands

Use these commands to read, change, or check your settings.

| Command | What it does |
| --- | --- |
| `ham config` | Prints every setting; `--json` prints JSON that includes the file path |
| `ham config get <key>` | Prints one setting; an unset key exits 1 with a note |
| `ham config set <key> <value>` | Writes one setting; either spelling of a key works, such as `default_model` or `defaultModel`; a switch takes `on`, `off`, `true` or `false` |
| `ham config unset <key>` | Removes the key so its default applies again |
| `ham config show [--json]` | Shows every key, its value in force, its origin and when a change takes effect; the origin is `flag`, `env` with the variable name, `file` or `default`; then lists the locked settings with their reasons |
| `ham config check [--strict]` | Reads the file and sends nothing; prints each problem with its line; exits 0 when clean, 1 for a problem, or 2 for wrong usage; `--strict` also counts a warning as a problem; a clean file prints its path and `ok` |
| `ham config edit` | Opens the file in `$VISUAL`, else `$EDITOR`, else `vi`; then runs `check` |
| `ham config docs` | Prints the table of all keys; the next section holds the same table |

These commands need no network and no credentials.
`ham doctor` also prints one `config` line for each file problem.

### The settings, key by key

The table below is the output of `ham config docs`.
The Takes effect column says when a change applies.
`now` means at once.
`next run` means at the start of the next run.
`next composer` means at the next New run screen.
`restart` means after you restart ham.

<!-- ham-settings:begin -->
| Key | Type | Default | Flag | Environment variable | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `apiEndpoint` | text | none | none | none | restart |
| `region` | text | AWS chain | none | `AWS_REGION` (the file wins) | restart |
| `awsProfile` | text | AWS chain | none | `AWS_PROFILE` (the file wins) | restart |
| `defaultRepo` | text | none | `--repo` | none | next composer |
| `downloadDir` | text | current folder | none | `HAMH_DOWNLOAD_DIR` (wins over the file) | restart |
| `defaultModel` | model id or alias | Claude Sonnet 5.5 | `--model` | `HAMH_MODEL` | next run |
| `inputWaitMinutes` | whole minutes, 0 to 43200 | 30 (terminal), 0 (script) | `--input-wait` | `HAMH_INPUT_WAIT` | next run |
| `mouse` | true, false or "auto" | on | none | `HAMH_NO_MOUSE` (forces off) | restart |
| `reportTests` | true or false | on | none | none | next run |
| `run.goal` | true or false | stage default (on) | `--goal on\|off` | none | next run |
| `run.rules` | true or false | on | `--no-rules, --rules <dir>` | none | next run |
| `run.skills` | true or false | on | `--no-skills, --skills <dir>` | none | next run |
| `research.enabled` | true or false | stage default | `--research, --no-research` | none | next run |
| `ui.notify` | true or false | off | none | `HAMH_NOTIFY` (wins over the file) | now |
| `ui.bell` | true or false | on | none | none | now |
| `ui.mascot` | auto, ascii or off | auto | none | `HAMH_NO_MASCOT` (forces off) | now |
| `run.report_command_minutes` | whole minutes, 0 to 465 | 5 (the stage's own limit) | none | none | next run |
| `run.stall_calls` | whole calls, 5 to 1000 | 30 calls | none | none | next run |
| `run.stall_window_minutes` | whole minutes, 1 to 240 | 20 minutes | none | none | next run |
| `run.stall_min_calls` | whole calls, 1 to 100 | 10 calls | none | none | next run |
| `run.command_grace_minutes` | whole minutes, 0 to 1440 | 120 minutes | none | none | next run |
| `run.final_message` | true or false | on | none | none | next run |
| `run.lifecycle_flow` | auto, full or quick | auto | none | none | next run |
| `run.review_round` | auto or off | auto | none | none | next run |
| `run.compute` | auto, microvm or instances | auto | `--compute auto\|microvm\|instances` | none | next run |
| `run.stall_new_calls` | whole calls, 0 to 1000 | 100 calls | none | none | next run |
| `run.cargo_no_debug_info` | true or false | on | none | none | next run |
| `run.cargo_no_incremental` | true or false | on | none | none | next run |
| `run.exclude_target_dir` | true or false | on | none | none | next run |
| `run.outside_checkout_note` | true or false | on | none | none | next run |
| `ui.deprecation_notices` | true or false | on | none | `HAMH_NO_DEPRECATION_NOTICES` (forces off) | restart |
| `ui.mascot_motion` | true or false | on | none | none | now |
| `run.max_run_hours` | whole hours, 1 to 720 | 24 hours | `--max-run-hours <hours>` | none | next run |
| `run.provider_sharing` | true or false | off | `--provider-sharing on\|off` | none | next run |
| `defaultBackend` | text | none | none | none | restart |
| `customerContent` | true or false | no | none | none | restart |
| `ui.copy_limit_kib` | whole KiB, 1 to 1048576 | 64 KiB | none | none | restart |
| `run.max_interrupts` | whole interrupts, 0 to 1000 | 5 interrupts | none | none | next run |
| `run.max_questions` | whole questions, 0 to 1000 | 3 questions | none | none | next run |
| `run.max_fresh_sessions` | whole fresh sessions, 0 to 1000 | 3 fresh sessions | none | none | next run |
| `research.max_file_mib` | whole MiB, 1 to 64 | 1 MiB | none | none | next run |
| `research.max_unpacked_mib` | whole MiB, 1 to 1024 | 200 MiB | none | none | next run |
| `research.max_run_mib` | whole MiB, 1 to 2048 | 300 MiB | none | none | next run |
| `run.container_signals` | signals separated by semicolons, default or none | default | none | none | next run |
| `defaultEngine` | an engine id | cline | `--engine <name>` | none | next run |
| `updateSource` | text | https://github.com/aws-samples/sample-hyperscale-agent-meta-harness.git | none | none | now |
<!-- ham-settings:end -->

The key `ui.deprecation_notices` controls every compatibility notice that `ham` prints and defaults to on.
You turn notices off with `ham config set ui.deprecation_notices off`; `HAMH_NO_DEPRECATION_NOTICES=1` forces them off whatever the setting says.

### How the settings reach a run

For each setting, the first source that is set wins.
A flag comes first.
An environment variable comes next.
The file comes next.
The built-in default comes last.

`region` reads the file before `AWS_REGION`.
`awsProfile` reads the file before `AWS_PROFILE`.
`downloadDir` has no flag.
`HAMH_DOWNLOAD_DIR` wins over the file.
`HAMH_NO_MOUSE` keeps the mouse off whatever the file says.
`HAMH_NO_MASCOT` keeps the mascot off whatever the file says.
When set, `HAMH_NOTIFY` wins over `ui.notify`.

A blank value counts as unset.
A wrong value that is set causes an error.
The error names the source.
ham never falls through to a weaker source.

`run.goal` has the one-run flag `--goal on|off`.
`research.enabled` has the one-run flags `--research` and `--no-research`.
`run.rules` has the one-run flags `--no-rules` and `--rules <dir>`.
`run.skills` has the one-run flags `--no-skills` and `--skills <dir>`.
These flags win over the file.

`run.rules = false` works like `--no-rules`.
It sends no skills either.
`run.skills = false` works like `--no-skills`.
`--rules <dir>` names a folder and wins over `false`.
`--skills <dir>` names a folder and wins over `false`.

The five keys `run.stall_calls`, `run.stall_window_minutes`, `run.stall_min_calls`, `run.command_grace_minutes` and `run.final_message` have no flag.
They move the limits of the stall rule `round-no-progress` and the switch of the last turn that rule gives Cline for its final message (see “The goal loop”).
A key you leave out sends nothing, and the stage's own value applies.
A run with the goal loop switched off sends none of them.

`run.stall_new_calls` has no flag and no environment variable.
A model that reads and plans before it edits makes no file change for as long as the task is large, and the stall rule could not tell that from a loop.
So a tool call that brings something new is not counted toward `run.stall_calls` and `run.stall_min_calls`: its input is unlike that of every earlier call of the run, its answer has at least 3 lines that no earlier answer had, and it succeeded.
The key is how many such calls go uncounted between two changed files; a changed file gives the whole number back.
`0` counts every call, as the rule did before.
It is sent as the request member `goal.newCalls`; a key you leave out sends nothing, and the stage's own value, 100, applies.
A run with the goal loop switched off sends none of it.

`run.lifecycle_flow` has no flag and no environment variable, and a run does not send it.

`run.review_round` has no flag and no environment variable.
`off` sends the request member `review` as `{"mode":"off"}` and nothing else, so the review file is not sent.
`auto`, and no setting, send the review file when you have one and nothing when you have none (see “review.md and the review round”).

`run.compute` picks the machine the session runs on. It takes `auto` (the default), `microvm` or `instances`.
`microvm` is the normal session with 2 vCPU.
`instances` is one EC2 machine per session with many CPUs and a large disk, where the stage has them. The first call of such a session takes about a minute and a half longer.
Choose `microvm`, which runs as the user `agent`, when the repository's checks need Docker or need to switch users. With `instances`, the engine runs as the user nobody (65534): programs that refuse root work, it cannot switch users, and the session has no Docker socket. The Limits table lists more under "Checks a session cannot run".
`auto` lets the stage decide.
The flag `--compute auto|microvm|instances` sets it for one run and wins over the file.
`microvm` and `instances` are sent as the request member `compute`, as `{"mode":"instances"}`; `auto` and no setting send nothing, so the request is the one an older version of ham sends.
A stage without the kind you ask for refuses the run at the start and says so.
A repository whose checks start containers stays on the microVM.
The Instances machine cannot run containers, so when the session could land there, the stage looks in the repository the run starts from for a container signal: a Testcontainers dependency in a build file, a docker compose file, or a `.ham/test-command` that mentions docker.
With a signal, `auto` runs on the microVM whatever the stage prefers. A request for `instances` ends the run before any machine starts, with a message that names the signal and the file and the two ways out: `--compute microvm` (or `auto`), or a list without that signal.
A repository with no signal keeps the choice described above.
The check reads the files the run starts from, so a new repository, an empty start and anything the session adds later cannot trigger it.

`run.container_signals` is the list of those signals. It is one line of entries separated by semicolons. With no setting, the built-in list is in force.
An entry is one or more file names separated by commas, then optionally the word `contains` and a text, as in `pom.xml, build.gradle contains testcontainers`.
A name with no slash matches a file of that name at any depth, so `pom.xml` also names the `pom.xml` of a folder below the root. A name with a slash is read from the root of the repository (`.ham/test-command`, `deploy/**/*.yml`), and a name that starts with `./` names the root only.
`*` and `?` stand for characters inside one folder name, and `**/` for any number of folders.
The text is looked for in the file without regard to the case of ASCII letters. An entry with no `contains` matches by file name alone.
The word `default` as an entry stands for the built-in list, so `default; Gemfile contains testcontainers` adds one signal and keeps the later improvements of the list. The word `none`, alone, turns every signal off.
`ham config set run.container_signals "<line>"` checks the line before it writes it.
The list is sent in the request member `compute` as `containerSignals`, also when the mode is `auto`; with no setting nothing is sent, so the request is the one an older version of ham sends.
The built-in list, one entry per line:

```
pom.xml, *.gradle, *.gradle.kts, *.versions.toml, build.sbt contains testcontainers
go.mod contains testcontainers
requirements*.txt, requirements*.in, constraints*.txt, pyproject.toml, setup.py, setup.cfg, Pipfile, environment.yml contains testcontainers
package.json contains testcontainers
*.csproj, *.fsproj, *.vbproj, Directory.Packages.props, Directory.Build.props, packages.config, paket.dependencies contains testcontainers
Cargo.toml contains testcontainers
Gemfile, composer.json, mix.exs contains testcontainers
docker-compose.yml, docker-compose.yaml, docker-compose.*.yml, docker-compose.*.yaml, compose.yml, compose.yaml, compose.*.yml, compose.*.yaml
.ham/test-command contains docker
```

Lock files are not signals, because their entries are often transitive.
A signal is a hint: a compose file kept for local development also keeps a repository on the microVM, and container use that no file shows is not found. A run that must use `instances` for such a repository sets the list to `none` (or to a list without that signal) first.

`run.cargo_no_debug_info`, `run.cargo_no_incremental`, `run.exclude_target_dir` and `run.outside_checkout_note` turn off four defaults of the session.
Each one is on unless you set it to `false`, and none has a flag or an environment variable.
`run.cargo_no_debug_info = false` leaves cargo's debug-information setting to cargo and to the repository's own profiles: the session then does not set `CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0`.
With them set, cargo builds the dev and test profiles without debug information, which keeps `target/` small and links fast on a small machine.
`run.cargo_no_incremental = false` leaves `CARGO_INCREMENTAL` unset in the same way.
`run.exclude_target_dir = false` leaves `target/` out of the repository's local exclude list.
By default the harness lists `target/` there, so the commit it makes at the end of the run never takes up build output; a file the repository already tracks stays tracked.
`run.outside_checkout_note = false` leaves the sentence "Work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest." out of the goal text and out of the prompt that restarts a quiet loop.
A value of `false` is sent as the request member `build` (`{"cargoNoDebugInfo":false}`, `"cargoNoIncremental"`, `"excludeTargetDir"`) or as `outsideCheckoutNote` in the `goal` member; `true` and no setting send nothing, so the request is the one an older version of ham sends.
A run with the goal loop switched off sends no `outsideCheckoutNote`.
The machine is chosen when the session starts and does not change for the run.
`ham rules init` reads it and installs the matching text of the lifecycle contract as the rule `10-lifecycle.md` (see “Choosing a flow”).

The command line uses one function to choose run settings.
The New run screen uses that same function.
A follow-up run also uses that function.
A file key therefore reaches all three.

Follow-ups read the file fresh when they start.
That holds for `ham continue` and for the follow-up in the terminal app.
Follow-ups do not inherit flags from the run they continue.

`goal` stays unset in the request while its key is unset.
`research` also stays unset while its key is unset.
The stage then decides those settings.
The table calls this "stage default".

### What a change writes

`set` and `unset` rewrite only the line they name.
Your comments stay unchanged.
The order of your keys stays unchanged.
Every other byte stays unchanged.

Setting a key to its current value writes nothing.
The file's modification time also stays unchanged.

`unset` removes the key without writing its default.
A later change to that default then reaches you.
Removing a table's last key also removes the empty table.
When no table key remains, ham removes `configVersion` too.
Earlier ham builds can then read the file again.

The eleven top-level keys use `key = "value"` on one line.
This keeps `setup.sh` and other tools that read those lines working.

A new file is private, with mode 0600.
ham follows a symbolic link at the config path.
The link stays a link.
Its target must be a regular file.

ham writes a temporary file in the same folder.
A rename then replaces the file.
Just before renaming, ham reads the file again.
It compares those bytes with the bytes it first read.
If they differ, ham writes nothing and prints `config.toml changed on disk; reloaded; nothing written`.
Run the command again.

This check narrows a race between two writers.
It is not a lock.
When two writers rename nearly together, the last rename wins.
ham makes no lock file.

ham never overwrites a file that is not valid TOML.
For such files, `set` and `unset` exit 1 without writing.
For a bad key, ham patches around it.
ham then prints what is still wrong.

The composer's `d` key saves the shown model as default.
That key uses the same write method.
`/config set` in the message box uses that method too.
The Settings screen also uses that method.
All three keep your comments.

You can edit the file by hand.
Run `ham config check` afterwards.

### Older and newer versions of ham

Setting any of the thirty-three table keys adds a table.
It also adds `configVersion`, and so do `defaultBackend`, `customerContent` and a `[[backend]]` table.
Older ham builds refuse a file that holds a table.
They report `unknown field`.
Those builds also refuse `ham config unset`.
Fix the file by hand or with a current ham.

The Settings screen shows a note before adding the first table.
It then waits for a second Enter.
`ham config set` prints its note after writing, on standard error.
The real note carries your ham version number:

```text
ham: note: ham versions before v<this version> refuse this file. To go back, run `ham config unset <key>` or delete the `[table]` and `configVersion` lines.
```

To restore the older form, run `ham config unset <key>` for each table key.
Or delete the table and `configVersion` lines by hand.
A build from before the tables can read a file that holds only the nine top-level keys.

You avoid this problem if you never set a table key.
ham never rewrites your file on upgrade.

A file from a newer ham has a higher `configVersion`.
Your ham reads the keys it knows.
It warns about each key it does not know.
It changes nothing in that file.
A command that wants to change it prints `config.toml was written by a newer ham (configVersion 2); update ham to change it. Nothing was written`.

An unknown key or table outside `[ui]` may steer a run.
For such a file, your ham refuses to start a run.
It also refuses `ham models` and `ham runs`.
The message ends with `update ham or remove those keys`.
An unknown key inside `[ui]` causes only a warning.
`ham config get`, `show`, `check`, `edit` and `docs`, and `ham doctor`, still run on such a file.

### Settings that no file can change

The harness fixes some settings.
No key, file, or flag changes them.
`ham config show` lists seven classes with their reasons.
The Settings screen lists the same classes in this order.

| Class | Reason |
| --- | --- |
| Credentials and identity | The harness supplies credentials itself; this cannot be set |
| Provider, model, endpoint | The model, provider, and limits come from the run's model; use `--model` or `default_model` |
| Telemetry and update | Telemetry and updates are off for every run and stay off |
| Processes and paths | This would run code or move a protected folder; not available |
| Permissions | The harness sets the approval mode for the run |
| Network | The stage sets network access |
| Owned elsewhere | Set this in `config.toml` or with the flag |

### Engine settings

Each engine has its own file in `engines/`.
Cline's `engines/cline.toml` holds the settings described in the next sections.
`engines/opencode.toml` holds one setting, the command time limit.
'The OpenCode setting' describes it.
`ham engine opencode show`, `check`, `set` and `unset` work on it.
You need no engine file.
Apart from the OpenCode command time limit, a run with no engine file is the run you started before.

#### The engine file

Your engine file is `~/.config/ham/engines/cline.toml`.
If `$XDG_CONFIG_HOME` is set and not empty, use `$XDG_CONFIG_HOME/ham/engines/`.
That folder sits beside `config.toml`.

Your client file, `config.toml`, holds settings for ham itself.
ham keeps that file on your computer and does not send it.
Your engine file holds values sent with every run you start.
The stage places those values where Cline reads them.
Your comments stay on your computer.
Only the values are sent.

With no engine file, your run sends no engine settings.
An empty file has the same effect.
A file with only `schema = 1` has the same effect.
A file with only empty tables also has the same effect.
In each case, you get exactly the run you started before.

Once you add a setting, your file needs `schema = 1`.
Without that line, ham refuses the file.
The message tells you to add it as the first line.
A schema number above 1 is refused.
The message says your file uses a newer schema.
It tells you to update ham or change the file.

Your file must be UTF-8 text.
It can be at most 64 KiB.
It must contain no NUL byte.
It must have no byte-order mark.
Put your settings in `[reasoning]`, `[commands]`, `[context]`, and `[tools]` tables.

```toml
# ~/.config/ham/engines/cline.toml
schema = 1

[reasoning]
effort = "high"

[commands]
timeout_seconds = 900

[context]
compaction = "agentic"
compaction_trigger = 0.85

[tools]
disable = ["fetch_web_content"]
```

#### The five Cline settings

You can set the five settings in this table.
The Settings screen also has them in the `Cline engine` group.

| Key | Type | Allowed | Default | What it does |
| --- | --- | --- | --- | --- |
| `reasoning.effort` | one word | `default`, `off`, `low`, `medium`, `high`, `xhigh` | `default` | How hard the model thinks before it answers. |
| `commands.timeout_seconds` | whole seconds | 1 to 1800 | 600 | How long one shell command may run before Cline stops it. |
| `context.compaction` | one word | `agentic`, `basic`, `off` | `agentic` | How Cline shortens a long conversation. |
| `context.compaction_trigger` | share of the context window | 0.10 to 0.99 | 0.9 | The share of the context window at which a compaction starts. |
| `tools.disable` | list of words | `spawn_agent`, `fetch_web_content` | empty | Tools Cline may not use in the run. |

- `reasoning.effort` sets the model's reasoning level for each run.
  `default` uses the level chosen for the model.
  See the section 'Reasoning is on by default'.
  `off` passes no reasoning argument to Cline.
  GLM 5.3 still uses `low` when you choose `off`.
  `low`, `medium`, `high`, and `xhigh` set that level for the run.
  `none`, `minimal`, and `max` are refused.
- `commands.timeout_seconds` limits how long one shell command can run.
  Cline stops the command when that time runs out.
  Cline's own limit is 600 seconds.
- `context.compaction` sets how Cline shortens a long conversation.
  `agentic` summarizes it, `basic` trims it, and `off` ends a run that overflows its context.
- `context.compaction_trigger` sets the share of the context window that starts a compaction.
  The setting is unused while compaction is `off`.
- `tools.disable` lists tools Cline may not use in your run.
  Its members are `spawn_agent` and `fetch_web_content`, each at most once.
  Your list adds to the stage's own list of switched-off tools.
  It never removes a tool from that list.

Both ends of a range are allowed.
A value outside its range is refused, never clamped.
Each value is a number in a range, a member of a closed set, or a list of such members.
A value cannot hold free text.

These limits apply to the settings ham sends.

The member that ham sends, with its engine name, schema and settings, is at most 4,096 bytes.
Tables and lists nest at most 3 levels.
The settings hold at most 64 values.
A text value is at most 64 bytes.

#### The OpenCode setting

Your OpenCode engine file is `~/.config/ham/engines/opencode.toml`.
It sits beside the Cline file and follows the rules in 'The engine file'.
It holds one setting.

| Key | Type | Allowed | Default | What it does |
| --- | --- | --- | --- | --- |
| `commands.timeout_seconds` | whole seconds | 1 to 1800 | 600 | How long one shell command may run before OpenCode stops it. |

```toml
# ~/.config/ham/engines/opencode.toml
schema = 1

[commands]
timeout_seconds = 900
```

OpenCode stops a shell command after 120 seconds unless the harness says otherwise.
That is too short for most builds and test runs.
Every OpenCode run therefore starts with 600 seconds, the same limit a Cline run has.
You change nothing to get it.
Set `commands.timeout_seconds` to use another limit from 1 to 1800 seconds.
Use `ham engine opencode set commands.timeout_seconds <seconds>` or the `Command time limit` row of the Settings screen.
The Settings screen shows that row in the `OpenCode engine` group while `default_engine` is `opencode`.
The other four keys of the Cline file are not OpenCode settings.
ham refuses them in `engines/opencode.toml` and sends nothing.

#### Reasoning is on by default

Reasoning is on for every run.
You change no setting to get it.

| Model | Level when you keep `default` |
| --- | --- |
| GPT-6.1 Sol | `xhigh` |
| GPT-6 Astra | `xhigh` |
| Claude Opus 5.5 | `high` |
| Claude Sonnet 5.5 | `high` |
| GLM 5.3 | `high` |
| Claude Fable 5.2 | `high` |

Set `reasoning.effort` in the engine file `engines/cline.toml`.
Use `ham engine cline set reasoning.effort <word>` to change that setting.
You can also change it through the Settings screen.
The `Reasoning effort` row sets the reasoning level.
It shows `model default` when the file leaves the key unset.
Enter cycles through `default`, `off`, `low`, `medium`, `high`, and `xhigh`.
Then the cycle starts again.
`default` uses the model's level from the table.
You cannot set a level for one run from the command line.

`off` sends no reasoning request to Claude Opus 5.5 or Claude Sonnet 5.5.
It also sends no reasoning request to GPT-6.1 Sol or GPT-6 Astra.
GLM 5.3 cannot switch reasoning off.
With `off`, GLM 5.3 receives `low`.
Bedrock rejects `none` on every route.

The level stays the same for the whole run.
It is the level of the model the run started on, also after a switch to another model.

`xhigh` takes longer and costs more than `high`.
A long reasoning turn of Claude Opus 5.5 or Claude Sonnet 5.5 at `xhigh` can run into the engine's idle limit (it ends a turn after about six minutes without a stream event), and GLM 5.3 can end without an answer at `xhigh`. Claude Fable 5.2 thinks without a stream event for minutes on a long problem, and its long test turns ended in the provider's content filter. That is why these models default to `high`.
You can still request `xhigh` for these models in `engines/cline.toml`.

#### The engine commands

You can use these commands without a network or credentials.
They work with your local engine file.

| Command | What it does |
| --- | --- |
| `ham engine cline init` | Writes a template: `schema = 1` and every setting commented out. It never changes a file that exists. |
| `ham engine cline show` | Prints the path, the size, the file sha256, the settings hash and each setting with its line. |
| `ham engine cline check` | Reads the file and prints the problems it finds, each with its line. |
| `ham engine cline edit` | Opens the file in your editor, then runs `check`. |
| `ham engine cline set <key> <value>` | Writes one setting. A list takes words separated by commas. |
| `ham engine cline unset <key>` | Removes one setting. |
| `ham engine cline import <file>` | Checks the file you name and copies it to `engines/cline.toml`. Add `--replace` to replace a file that exists. |

A command ends with exit 0 when it did what you asked.
It ends with exit 2 when it refused: a file the check refuses (a syntax error included), a value out of range, a file you name that is not there, or an engine with no settings in this build.
`init`, `show`, `check`, `edit` and `import` end with exit 1 when they cannot read or write the file at all.
`set` and `unset` end with exit 2 for every error.
A refused `set` prints `Nothing was written.` and changes nothing.

For example, `ham engine cline set reasoning.effort high` writes `effort = "high"` under `[reasoning]`.
`ham engine cline set tools.disable spawn_agent,fetch_web_content` writes a list of two tools.

Add `--json` to print the result as JSON.
Only `cline` has settings. Any other engine name answers `no settings schema in this build yet`.

`set` and `unset` change only the line you name.
Both commands keep your comments.
On a missing file, `set` writes `schema = 1` first.
`unset` removes the key.
It does not write the default.

Before renaming, a write re-reads your file and compares its SHA-256.
If your file changes on disk, the command prints this message:
`engines/cline.toml changed on disk; reloaded; nothing written`
This check narrows a race.
It is not a lock.

`import` checks the file you name.
It then copies that file's bytes, including comments.
It refuses to replace an existing file unless you add `--replace`.
`init` writes a template with every setting commented out.
It never changes an existing file.
`edit` opens `$VISUAL`, else `$EDITOR`, else `vi`.
It then checks your file.

#### What a run does with your settings

ham reads your file fresh each time you start a run.
This includes command-line starts, including `ham continue`.
It includes starts from the New run screen.
It also includes follow-up runs in the terminal app.

ham uses the same file check as control-api and agentd.
A refused file stops your start.
Nothing is sent.
ham puts accepted values into one request member.
It sends that member with your run.
agentd checks the member again.

Each setting goes where Cline reads it:

- `reasoning.effort` becomes the launch argument `--thinking`.
- For `default` or no setting, the level is the model's own level.
- For `off`, the argument is not added.
- The run summary records the level and where it came from.
- `commands.timeout_seconds` becomes the limit Cline puts on one shell command.
- `context.compaction` goes into the compaction block of Cline's provider file.
- `context.compaction_trigger` goes into that same block.
- `tools.disable` joins the stage's own list of switched-off tools.
- That list is in a file only root can change.

On OpenCode, `commands.timeout_seconds` becomes the limit OpenCode puts on one shell command.
Every OpenCode run carries that limit: your value, or 600 seconds.
agentd reads it back before the first model call.
A mismatch ends the run with the label `OpenCode did not start`.

agentd sets the telemetry-off and no-auto-update locks after it loads the base settings, so no engine setting can change them.
Your `tools.disable` list joins `disabledTools` in that same root-owned file.
What agentd wrote is read back and compared with what your settings call for, including the parts the stage sets.
A mismatch ends your run before the first model call.
The run ends with the label `Cline did not start`.
The detail names the part that differs.
It never includes a value.

#### Placed, and the settings hash

Placed means agentd wrote a value where Cline reads it.
It also means agentd read that value back.
It does not mean Cline confirmed the value.

With no engine file, your run gets no engine settings.
It posts no engine settings event.
It has no engine settings card.

When settings are placed, your run screen shows a folded card.
Its title looks like this:
`engine settings placed · cline · 3 set · settings hash a1b2c3d4e5f6`
The number of settings and the hash vary.
Select the card and press Space to open it.
You see one `key = value` row per setting.
The conversation shows one line: `Engine settings: 3 placed (settings hash a1b2c3d4e5f6).`

`ham watch` prints the card once as `Engine settings placed for this run`, with the engine and the first 12 digits of the settings hash, and one `key = value` line per setting.
`ham watch --json` lists them in the `engineConfig` member, with `engine`, `sha256` (all 64 digits), `keys` and `settings`.
A run with no engine settings has no such member.
Your run's `engine-config` event lists each key with its value.
`summary.json` records the engine, the schema, the number of keys and the settings hash.
It holds the number of keys and the settings hash, not the key names or the values.

The settings hash is the SHA-256 of the settings as sent.
These settings use compact JSON with every key in sorted order.
Changing only comments leaves the hash the same.
Changing only key order also leaves it the same.
Number forms such as `0.90` and `0.9` give the same hash.
The settings hash is not the checksum of the file.
Your card shows the first 12 digits.

`ham engine cline show` prints your file's path and size.
It prints the `file sha256`.
It prints the `settings hash` with all 64 digits.
It also prints each setting with its line number.
Its hash line reads:
`settings hash: <64 digits>   (the run card shows the first 12 digits)`

#### Follow-up runs

Your follow-up run reads the engine file fresh.
It does not copy the settings of the run you continue.
It uses the same message box and New run screen as other starts.

The confirmation compares your file's settings hash with the earlier card's hash.
When they differ, the confirmation adds this line:
`Engine settings changed since run <id>: the new run sends the file as it is now (settings hash <12 digits>).`
When your file now sets nothing, it adds this line:
`Engine settings changed since run <id>: the file now sets none, so the new run uses the engine's own.`

#### A stage older than your ham

A stage deployed before engine settings existed refuses requests carrying them.
ham shows this message for that answer:
``this stage predates engine settings; run `ham engine cline unset <key>` or update the stage (keys set: <list>)``

ham never sends the run again without the settings.
A missing setting must not look as though your run received it.
You can remove the keys with `ham engine cline unset <key>`.
Or ask whoever runs your stage to update it.

An older ham build does not read the `engines/` folder.
It starts your runs without these settings.
The runs it starts carry no engine settings, so they have no engine settings card.

#### What is refused

ham, control-api, and agentd refuse the same things.
A key outside the five settings is refused as not an engine setting.

You cannot set keys in these classes:

- credentials
- provider
- telemetry
- processes
- approvals
- network
- owned elsewhere

A key naming one of these classes gets that class's refusal reason.
The section [Settings that no file can change](#settings-that-no-file-can-change) lists those reasons.

A value outside its range or allowed set is refused.
A value with the wrong type is refused.
A list naming the same member twice is refused.
A file over the limits is refused.

A refusal names the key and the rule.
It never repeats a value.
A plain lowercase key name is repeated in the message, so never put a secret in a key name.
A key with unusual characters is reported as a key with unusual characters.

On the command line, a refused file prints the problems.
It also prints `Nothing was sent.`
The command exits with code 2.
In the terminal app, you see the same text as a notice.

A request from another path can reach control-api with a refused member.
control-api answers 400 with the code `invalid-engine-config`.
It names the problem, the key, and the rule.
It never includes a value.
It writes nothing.

### Several backends

One `ham` can work with several stages, one in each AWS account, and show the runs of all of them in one list. This helps when one account's Bedrock or AgentCore limits slow your runs: you start the next run in another account. Each stage is a full setup in its own account, and a run stays in the account where it started. `ham` talks to each stage itself with its own AWS profile; no service sits between them.

A config with no `[[backend]]` table has one backend, called `default`, made of the top-level `apiEndpoint`, `region` and `awsProfile`. Nothing changes for you: no extra column, no extra line, the same output as before. To add a backend, add a table to `config.toml`, after the other keys:

```toml
configVersion = 1
defaultBackend = "work"

[[backend]]
alias = "work"
apiEndpoint = "https://<api-id>.execute-api.us-west-2.amazonaws.com/direct"
region = "us-west-2"
awsProfile = "work"
customerContent = true

[[backend]]
alias = "spare"
apiEndpoint = "https://<other-api-id>.execute-api.us-east-1.amazonaws.com/direct"
awsProfile = "spare"
```

| Key of a `[[backend]]` table | Meaning |
| --- | --- |
| `alias` | The short name you type after `--backend`: 1 to 32 lower-case letters, digits, `-` and `_`, starting with a letter or digit. `default` is taken by the top-level keys. |
| `apiEndpoint` | The stage address, as for the top-level key. Required. Two backends cannot share an address. |
| `region` | The Region that signs requests. Optional. |
| `awsProfile` | The AWS CLI profile of the account. Optional. Use one profile for each account. |
| `customerContent` | `true` declares that this backend may receive a run marked as customer content. Absent means no. |

`default_backend` (and `defaultBackend` in the file) names the backend a new run starts on when you name none. `customer_content` is the same declaration for the `default` backend.

**Choosing the backend of a run.** With one backend nothing needs choosing. With two or more:

- Adding `--backend work` to the start command starts the run on `work`. Without `--backend`, a start goes to `default_backend`; with neither, `ham` stops and lists the aliases.
- In the terminal app, the New run screen shows a Backend box. Pick with the arrow keys; Tab reaches it after the Options box. A start with no choice is refused on the screen.
- `ham continue <run id>` starts the new run on the backend that holds the old one. So does a follow-up typed on a finished run in the terminal app. If you name another backend for it, `ham` refuses when it already knows where the run is.
- Nothing picks a backend for you: `ham` never starts a run on another backend because the first one is slow or failing, and a start that is sent again after a lost answer goes to the same backend with the same key.

**Customer content.** `--customer-content` (in the terminal app, the same word in the Options box) marks a start as carrying customer content. `ham` refuses the start unless the backend is declared safe with `customerContent = true`. The declaration is your statement; `ham` does not check it against the data-retention mode of an account. It covers the start only.

**Reading and changing runs.** `ham runs` and the Runs screen list the runs of every backend, newest first, with a BACKEND column (and a `backend` member in `--json`). A backend that does not answer in time, is down, needs a new login, refuses your profile or answers with a server error does not hold up the others: its last good rows stay on screen and one line names it and the fix, for example `spare: sign in needed: aws sso login --profile spare`. A backend whose login has lapsed, or whose gateway refuses your profile (`spare: access refused: HTTP 403 ...`), is left alone by the 5-second refresh; `r` tries it again. `ham doctor` checks every backend at once and gives each 20 seconds, so one backend that does not answer costs 20 seconds and no more.

Commands about one run (`watch`, `cancel`, `say`, `pull`, `clone`) go to the backend that holds the run. You type the run id as before. If `ham` has not seen the run yet, it asks every backend; it says "not found" only when every backend has answered, and it refuses a run id that two backends hold. `--backend <alias>` skips the asking.

**Versions.** Each stage reports its version. When two stages report different versions, the tag at the top reads `backend mixed`. A stage older than this `ham` supports (before 5.00), one that reports no version at all, or one of a newer major version is read-only: `ham` reads from it, including the download of a finished zip with `ham pull` or `ham clone`, and refuses to change anything there until you update `ham` or the stage.

**What is not merged.** Search and the filtered reads of the Runs screen read the default backend only. A tag edit covers the runs of one backend at a time. `ham watch` without a run id needs `--backend`. `ham` keeps the list of which run is where in memory only; it writes no cache file.

**Sessions.** Ownership of a run is the exact AWS identity that started it, including the session name of an assumed role. To keep your earlier runs visible after each login, give an assumed-role profile a fixed `role_session_name`, and share one `sso_session` between the profiles of one person.

## The terminal app

The TUI is the full-screen terminal interface.
You start, watch and collect a run from one screen.

Open it with no arguments:

```bash
ham
```

The TUI opens when stdout and stdin are both terminals.
Otherwise, this command prints the usage line and exits 2.

There are seven screens:

- **Runs** lists your runs.
- **New run** is the composer. It starts a run.
- **Dashboard** shows one run.
- **Artifacts** lists one run's files.
- **Models** lists models Cline may continue on. Press `m` while a "Cline needs you" question is open.
- **Reply** lets you type an answer message. It opens with `p` or `s`. It also opens when you pick a model.
- **Settings** shows config.toml's settings and changes them.
  It opens with `s` on Runs or with `/settings`.

Ctrl-C clears the text box that has the keyboard when it holds text.
The app stays open.
When that box is empty, Ctrl-C requests a quit.
Ctrl-C also requests a quit when the screen has no text box.

Unsent text changes how quitting works.
This includes Prompt and Base ref, both text boxes.
It also includes a changed Repository in New run.
It includes an answer you are typing.

With unsent text, the first quit request only shows this line:

> unsent text is lost when ham ends: press Ctrl-C or q again to quit

That request can be Ctrl-C or q.
Press Ctrl-C or q again to quit.
Any other key between those requests starts the warning over.

### The version tag

ham is the terminal app. Its backend is the stage ham talks to.
Every screen shows the tag at the first row's right end. The screen title stays at the left, after the mascot's head when the terminal is tall enough for it (see "The mascot").
The tag reads `ham vN.NN | backend vN.NN`, for example `ham v5.NN | backend v5.NN`.
The first number comes from `VERSION` at the repository root. It has the form N.NN: a major number and a two-digit counter.
The backend sends its number in `x-hamh-backend-version`, the version header. Every answer includes this header, including error answers.
ham reads the header from each signed call. The tag updates after a redeploy without restarting ham.
`backend v?.??` means no answer yet, or an answer without a valid version. Its fixed width keeps the tag from moving. A refusal or throttle by the API gateway (403, 429 or a 5xx error) leaves the last known backend version in place.

The backend half is bold yellow when versions differ or remain unknown.
Matching versions make the whole tag dim. Bold still shows on terminals with no colour.

At 62 columns or more, `ham vN.NN | backend vN.NN` uses 25 characters.
From 54 to 61 columns, the tag reads `h vN.NN | b vN.NN`.
From 9 to 53 columns, it reads `N.NN|N.NN`. Below 9 columns, no tag appears.
A two-digit major number adds two characters to each form. The column limits move by about two columns.
A title that would run into the tag ends in an ellipsis one cell before the gap. The tag is never hidden.
On the run screen the header shortens first. It gives up the age of the last read, the cost, the tokens, the elapsed time and the model, in that order, before it is cut. At 80 columns the header reads `ham · Run <runId> · <state>`; the model is in the status card below.

Run `ham --version` or `ham -V` with no other arguments. `ham --version` prints `ham vN.NN` and exits 0, and `ham -V` does the same.
These commands read no config file or credential. They make no network calls.

`ham doctor` puts `versions`, the version check, after `api`.
Matching versions produce `PASS versions ham vN.NN | backend vN.NN`.
Different versions produce a WARN: `backend older than this ham` or `backend newer than this ham`. A missing version produces a WARN saying the backend reports no version.
A WARN never changes the exit code. In `--json`, the check name is `versions`.

### The terminal and the mouse

When ham opens, it asks the terminal for four things:

- The alternate screen: the terminal's full-screen mode.
- Bracketed paste: paste arrives as one text block.
- Focus reports: the window gains or loses focus.
- Mouse reports: clicks and drags sent to ham.

Pasted text does not arrive as separate key presses.

ham restores all four settings when it ends.
This happens after q or Ctrl-C.
It also happens after SIGTERM, SIGINT, or SIGHUP.
It happens after a panic, a crash.

A program killed with SIGKILL cannot restore these settings.
The shell then prints stray characters when you click.
The command `reset` fixes the terminal.
The next `ham` you start in the same window also repairs it.
Before it turns on its own settings, it turns off the mouse, focus and paste settings that a killed `ham` left on.
It does not touch the alternate screen, the colours or the cursor.

After SIGTERM, ham prints "ham: terminal: ended by SIGTERM".
It prints this after restoring the terminal.
The message uses SIGINT or SIGHUP for those signals.
For these three signals, ham exits with a non-zero code.

Mouse reports are on by default.
While they are on, the terminal does not select text.
Hold one of these keys to select text with the mouse:

- Fn in Terminal.app.
- Option in iTerm2.
- Shift in most other terminals.

The first screen of a session that asks for the mouse says so in one line, for your terminal:
"mouse is on: hold Option to select text with the terminal · /mouse off".
The first key you press or click you make takes the line away.
Nothing is stored, so it comes back the next time you start ham.

Type `/mouse off` in the message box of a run to turn the mouse reports off for this session, and `/mouse on` to turn them back on.
`/mouse` alone says which it is now.
The command does not change the config file; `mouse = false` there turns the reports off for good.
If `HAMH_NO_MOUSE` is set, `/mouse on` says the mouse stays off until you start ham without it.

Terminals that send mouse reports no longer turn the wheel into arrow keys, so ham does it.
Each notch moves the Runs list by the lines of a notch (3 by default, more for a quick run of notches), the Artifacts list by one row, and scrolls the dashboard by three lines.
The wheel does nothing on the New run screen and the typing screen.

Clicks work where a key does the same:

| Click on | What it does |
| --- | --- |
| A text box of New run | Moves the focus to it; the next character you type goes there. |
| A model in New run | Picks that model. |
| The `[ Submit ]` line of New run | A click starts the run, like Ctrl-S. Only the words respond to clicks. The blank space to their right does nothing. With no prompt, it says `a prompt is required`. While the run starts, the line reads `submitting…`. A click then does nothing. |
| A run in Runs | Selects it. A second click on the same run within half a second opens it. |
| The message box of a run | Starts typing there, like `i`. |
| The head of a card or block that can fold | Opens or folds it, like Space. |
| A key in a key line, such as `n new run`, `a artifacts` or `Esc browse` | Presses that key. |

The keyboard does everything the mouse does, and it works the same with `mouse` off.
A click on the result panel of a command or on its preview does nothing, because they take the click themselves.
A click between a resize and the next redraw does nothing.
Dragging over the conversation selects text, drawn reversed, and letting go copies exactly that text.
A paragraph that wrapped at the width copies as it was written, and a word cut at the width copies without a space.
Dragging past the top or bottom edge of the conversation scrolls it, so a selection can reach rows that are off the screen.
Hold the mouse button with the pointer still above or below the conversation, and the conversation keeps scrolling by 3 lines every 60 milliseconds.
It scrolls toward older rows above the conversation and toward newer rows below it.
The selection follows the scrolling, so every row it passes is selected.
Letting go copies from the row where the selection began to the first or last row shown.
The scrolling stops when you let go, press a key, resize the window or move the pointer back over the conversation.
Moving the pointer out again starts it again, and so does moving the pointer after a key.
A new click, a resize that changes the rows, or a change to the selected rows drops the selection.
The notice row names the path used to copy, as it does for `/copy`.
Escape sequences in what Cline wrote are removed before they are drawn, so they cannot write to your clipboard or your terminal.

`mouse` controls mouse reports in config.toml.
It accepts `true`, `false`, or "auto" (choice based on the terminal).
The default is `true`.
Write `true` and `false` without quotes in the file.
Write "auto" with quotes.

- `ham config set mouse false` turns mouse reports off.
- `ham config get mouse` prints the value.
- `ham config unset mouse` goes back to the default.

`auto` turns mouse reports on in these terminals:

- iTerm2
- ghostty
- WezTerm
- The VS Code terminal

It also enables them when the program name is unset.
Then TERM must start with xterm or contain kitty.
`auto` turns mouse reports off in Terminal.app.
It also turns them off in every other terminal.

The environment variable `HAMH_NO_MOUSE=1` turns mouse reports off.
It also accepts `true`, `yes`, or `on`.
This environment variable wins over the config file.

Focus reports have no switch.
Bracketed paste has no switch.

### Repainting the screen

ham writes only cells that changed since the last frame.

Switching to another app and back can lose parts of the screen.
Those cells stay blank until ham writes them again.
For example, the message box can lose its top or bottom border.

ham repaints the whole screen in three cases:

1. The terminal reports that the window has focus again: `ESC [ I`.
2. You press Ctrl-L.
3. ham continues after being stopped: `SIGCONT`.

Ctrl-L works on the Runs list, New run, and the run screen.
It works in both typing and browse modes.
The shell stopping ham and continuing it is one example of `SIGCONT`.

A repaint clears the screen.
It then writes every cell again.
It does not change what the screen shows.

Losing focus does not cause a repaint.
Other keys do not cause a repaint.
The one-second timer redraws only cells that changed.

#### When to press Ctrl-L

Some terminals do not report focus.
They never tell ham that the window came back.
macOS Terminal.app may not report focus.
In terminals that do not report focus, press Ctrl-L when part of the screen looks wrong.

Inside tmux, focus reports need the option focus-events on.
Use `set -g focus-events on` to turn it on, then detach and attach the client again.

The key line at the bottom does not list Ctrl-L.
In a prepared follow-up message box, Ctrl-L still adds Cline's last message.
It also repaints the screen.

Outside a prepared follow-up, Ctrl-L repaints the screen.
The notice reads:

> repainted the screen · Ctrl-L adds Cline's last message to a follow-up draft: press Enter on a finished run first

#### Repaint size

Repaint sizes:

- A finished run screen costs 1,289 bytes at 100 columns by 30 rows.
- At 200 by 60, that screen costs 1,908 bytes.
- A full screen at 200 by 60 costs at most 32,808 bytes.

The full screen measurement has text in every cell.
It has a style change every 8 cells.

An unchanged frame costs about 30 bytes.

### Pasting text

ham asks the terminal for bracketed paste.
This makes pasted text arrive as one block.
The block never acts as key presses.

In New run, paste fills the Prompt, Repository and Base ref boxes.
You can also paste into the typing screen of an answer.
These text boxes receive the whole paste.
The Prompt and answer typing screen keep line breaks.
The Repository and Base ref boxes drop line breaks and spaces.

ham cleans pasted text:
- Carriage returns and CRLF become line breaks.
- Each tab becomes four spaces.
- Escape sequences and other control characters are removed.

The Runs screen, dashboard and Artifacts screen have no text box.
Pasting on these screens does nothing.
This line appears:

```text
paste ignored: this screen has no text box
```

The Target, Model and Submit fields of New run also ignore paste.
This line appears:

```text
paste ignored: this field takes keys; paste into the Prompt, Repository or Base ref box
```

A paste is never read as keys.
Pasting `cc` does not cancel a run, and pasting `q` does not quit ham.
Typed letters still work as keys.

Terminals without bracketed paste support still send pastes as key presses.
ham cannot tell those pastes from typing.

### Editing text

These keys work in every text box.
This includes the Prompt, Repository, Base ref and Options boxes.
These boxes are in New run.
The run's message box uses these keys too.

Hold Shift with an arrow key, Home or End to select text.
Moving without Shift ends the selection.
Backspace or Delete removes the selected text.

Ctrl-Z goes back one edit.
You can undo up to 50 edits.
Alt-Z goes forward again after an undo.
Some editors use Ctrl-U for undo.
This app uses Ctrl-Z.

Long lines wrap at word boundaries in the Prompt and message box.
A word longer than the box splits at its edge.
Each wrapped line starts with the space that ended the row above.
The Repository, Base ref and Options boxes each hold one line.
These boxes scroll sideways.

Some keys belong to the app and never edit text.
Ctrl-U and Ctrl-R do nothing.

In the message box, Alt plus a dashboard hotkey acts like Esc then the letter.
The exceptions are b, d, f, h, n, p and v.
With Alt, these letters are the editor's word keys.

| Keys | Action |
| --- | --- |
| Arrow keys, Home, End | Move the cursor. |
| Alt-F | Move to the start of the next word. |
| Alt-B | Move to the start of the word before. |
| Ctrl-A | Move to the start of the line. |
| Ctrl-E | Move to the end of the line. |
| Shift with an arrow key, Home or End | Select text. |
| Backspace, Delete | Delete one character, or remove the selected text. |
| Ctrl-W | Delete the word before the cursor. |
| Alt-D | Delete the word after the cursor. |
| Ctrl-K | Delete from the cursor to the end of the line. |
| Ctrl-Y | Put back the text last deleted by Ctrl-W, Alt-D or Ctrl-K. |
| Ctrl-Z | Go back one edit, up to 50 edits. |
| Alt-Z | Go forward again after an undo. |
| Enter, Tab, Esc, function keys, Ctrl-J, Ctrl-C, Ctrl-S, Ctrl-G | Belong to the app and never edit text. |
| Ctrl-U, Ctrl-R | Do nothing. |

### Runs

Runs lists your runs, newest first.
A one-character gutter at the left of each row marks the runs that need attention:

| Gutter | Meaning |
| --- | --- |
| `>` | The run is active and nothing waits for you. |
| `!` | Cline waits for an answer (bold). |
| `x` | The run ended early or the harness blocked it (red). |
| `=` | The run is paused: it used its total time and delivered its work (yellow). |
| `~` | The run is stalled: it lost its progress and delivered its work (magenta). |

| Key | What it does |
| --- | --- |
| `n` | Open New run |
| `/` | Open the filter line at the bottom |
| `p` | Show only the runs of the selected run's repository; press again to show all. The key line names what a press does (see below) |
| `r` | Read every page of the list again |
| `R` | Resume every paused and stalled run of the list: one question names the count, Enter or `R` again starts them |
| `s` | Opens the Settings screen. |
| `t` | Opens the tag explorer: the tags of the runs the list holds, with the number of runs that hold each. |
| Up/Down or `k`/`j` | Select a run (only among the runs shown) |
| PageUp/PageDown | Move the selection up or down by a screen less one row |
| Home or `g`, End or `G` | Select the first run, the last run |
| Enter | Open the selected run |
| Esc | Clear the filter, the repository limit and the picked tags |
| `q` | Quit |

The help line reads, on a screen 80 columns wide with a run in a repository selected:

> n new run · Enter open · ↑↓ · / find · t tags · p this repo · r refresh · q quit

From 91 columns wide, `s settings` joins the line before `r refresh`.
While a filter or a picked tag narrows the list, the help line reads (here with the repository limit on):

> Enter open · ↑↓ · / edit · Esc clears · t tags · p all runs · r refresh · q quit

The words of the `p` key say what a press does, and they change with the selected run and with the width.
The line uses the longest wording that leaves `q quit` on the screen.
With the limit off and a run in a repository selected, the key reads `p only cc:calc` when there is room, then `p this repo`, then `p repo`.
A repository name longer than 24 characters ends with an ellipsis.
A run with no repository belongs to `s3-only`, and the key reads `p only s3-only` or `p s3-only`.
With the limit on, the key reads `p show all runs`, then `p all runs`, then `p all`, because the next press lifts the limit.
A mouse click on the words presses `p`.

The table shows as many runs as fit the screen.
When you have more, it scrolls with the selection, so the run marked with `▶` is always in view.
The mouse wheel moves the selection too: each notch moves it by the lines of a notch (3 by default, more for a quick run of notches).

#### Run names

Every run has a name of 5 to 7 words.
A small model writes it from your prompt a few seconds after the run starts.
You do not choose that model; see [The model that names runs](#the-model-that-names-runs).

Until the model's name arrives, you see a seed name instead.
A seed name is a short label cut from the start of your prompt.
A seed is never stored, so older runs, started before names existed, always show one, and ham never asks the model to name them.
A prompt of fewer than 4 words keeps its seed name; `fix the bug` makes no model call.
A failed model call, or a reply that fails the service's checks, keeps the seed name too.
An operator can switch the model off for a stage; every run on that stage then keeps its seed name.

The Runs table draws a name that the model wrote in plain text and a seed in dim text.
A stage older than this feature sends no names; the table then shows the first line of the prompt, in dim text.
A run with neither shows a dim `-`.

A name is display text.
It does not change what a run does or its outcome label.
The zip, the branch and the clone folder keep the names made from the run id.
A name is at most 64 columns wide.
A name in Chinese, Japanese or Thai counts 8 to 20 characters instead of 5 to 7 words.
A short prompt can get a shorter name.
When the model answers with 8 to 10 words, the service cuts the name to 7 words or fewer.

RUN, STATUS and NAME show from 60 columns, CREATED joins at 99 columns, TARGET at 124 columns and PHASE at 137 columns.

Below 60 columns the table shows only the gutter, RUN and STATUS.
On screen the order is gutter, RUN, STATUS, NAME, TARGET, PHASE, CREATED, and NAME takes the width the other columns leave.
A column that a narrow screen hides is not lost: the note line under the table shows it for the selected run, as `· target <target> · phase <phase> · created <createdAt>`.
In the Runs list, a run that is `complete` or `cleared` shows a dash (`-`) under PHASE and has no `phase` part in the note line.
A run that is `active`, `paused`, `blocked`, `usage_limited` or `budget_limited` shows its phase.
For a blocked run, the phase shows where it stopped: `green` means the Cline session, and `delivering` means the packaging.
`ham runs` and `ham watch` print a finished run as `complete (delivered)`: the service stored `delivering` and never wrote a later phase. `--json` prints the stored phase.
A name that is too wide for its cell ends with an ellipsis (…).
Wide characters such as Japanese are counted by their width on screen and are never split.

The words you type in the filter (`/`) are also looked for in the name.
Ctrl-O (open anything) shows and finds runs by name.
`ham runs` prints the name after the creation time; see [Watching a run](#watching-a-run).

#### Tags

A tag is text in the form `kind:value`.
The kind is `project`, `topic` or `tech`.
The value uses lowercase English letters, digits and single hyphens joining words.
Examples are `project:billing-export`, `topic:csv-export` and `tech:aws-lambda`.
A tag is display and search text.
It never reaches a zip name, a branch, a clone folder, an S3 key or a pull request title.
It never changes what a run does or its outcome label.

A run shows up to 19 tags at once.
Tags come from four sources, applied in this order.

First comes the derived project, the repository's project tag.
A repository run shows `project:<repository>` at once.
For example, `cc:calc` gives `project:calc`.
ham and the service work it out whenever you read the run.
They store it nowhere.
An `s3-only` run has no repository.
It has no derived project until you type one.

Next come the tags you added.
You add them with `/tag` or in the tag explorer.
See [The tag explorer](#the-tag-explorer) and [Typing /tag](#typing-tag).

Then come tags from the small model, which also names runs.
It writes up to 3 `topic:` tags and 3 `tech:` tags.
The service stores these tags once on the run.
The model never writes a `project:` tag.
The service drops any such tag that appears.
A topic describes what your request is about.
A tech names a language, framework, cloud service or library.
Your request must name that tech or clearly need it.

Last, every tag you removed is taken out.
A removal wins over all other sources.
A tag you remove stays removed.
Neither the model nor later reads can bring the tag back.
Removing the derived project works the same way.
Only adding the tag again clears its removal.

The model writes at most 6 tags for a run, you can add at most 12 to a run, and a run keeps at most 32 removals.
Adding or removing a tag past these limits gets refused with a message.
Nothing older is dropped to make room.

The same step makes tags and names the run.
This happens a few seconds after the run starts.
The run does not wait for tags.
A prompt with fewer than 4 words makes no model call.
Such a run keeps only its derived project.
For Chinese, Japanese and Thai, the short prompt limit counts characters.
Fewer than 8 characters make no call.

A failed model call leaves the run without model tags.
A reply that fails the service's checks has the same effect.
So does an old service that has no tags.
So does a stage where an operator switched tags off.
Nothing else changes in these cases.
Runs started before tags existed have no model tags.
They show only their derived project.

The model sees the same prompt excerpt used for the name.
It sees at most the first 2,000 and the last 1,000 characters of the prompt, with known credential shapes hidden.
See [The model that names runs](#the-model-that-names-runs).

A tag never holds verified, tested, gated or evaluated as its whole value or any hyphen-separated part; `topic:unit-tests` is fine, while `topic:tested` is refused.
A tag is never a status word such as done, failed, running or blocked.
The model avoids whole tags such as code, testing, app or bugfix.
You may type those generic tags yourself.

On the Runs screen, you see the selected run's tags below the table.
They appear in the note line after the run count.
They come before the columns that a narrow screen hides.
On the run screen, tags appear in the name row under the header.
See [Dashboard](#dashboard).
In `ham runs`, tags appear in square brackets after the name.
With the option `--json`, `ham runs` adds the member `tags`.
See [Watching a run](#watching-a-run).
A word you type in the Runs filter also matches tags.
The open box (Ctrl-O) finds runs by their tags too.
You can also see them in the tag explorer.

You make a follow-up run with `ham continue`.
You can also type in the screen of a finished run.
The follow-up starts with the tags you added to the run it continues and with your removals on that run.
The parent's model tags follow the follow-up's own model tags.
Together, these model tags total up to 6.
A follow-up never gets back a tag you removed.
A follow-up prompt with fewer than 4 words makes no model call.
That follow-up gets its parent's tags.
The service copies tags when the follow-up starts.
If you edit the follow-up's tags first, your edit stands.

Tags belong to the sign-in identity that started the run, like the Runs list.
A different identity does not see them.

#### The tag explorer

The tag explorer fills the Runs screen with an overlay.
Press `t` on the Runs screen to open it.
You see every tag held by runs in the Runs list.
Each tag shows how many runs hold it.
The explorer adds no screen or new top-level view.
It takes every key while open.
Nothing is stored on your machine.

The first row reads `ham · Tags`.
On a terminal with 31 rows or more and 72 columns or more, the title starts after the mascot's head.
The version tag stays at that row's right end.
A box follows with one of these titles:

- `tags · all 212 runs loaded at 14:05`, as an example.
- `tags · newest 100 runs only - r loads all`.

The box contains a table with TAG and RUNS columns.
At 100 columns wide, a LAST column also appears.
LAST shows the age of the newest run holding each tag.
Examples are 5m, 3h, 2d and 2w, for minutes, hours, days and weeks.
Picked tags stay on top.
Other tags sort first by run count, highest first.
Ties sort by newest run, then by tag text.
The table scrolls with your cursor.

Below the box, a note counts runs like the Runs summary.
With picks, it can read `12 of 212 runs match +project:billing-export · 10 tags`.
Without picks, it can read `212 runs · 41 tags · 30 have no tags`.
The last row is the key line.

| Key | What it does |
| --- | --- |
| `Up` `Down`, `k` `j` | Move your cursor one row. |
| `Space` | Pick the cursor's tag; press again to drop the pick. |
| `/` | Open the find line. |
| `Enter` | Apply the picked tags to the Runs list and close the explorer. With nothing picked, apply the tag under the cursor. |
| `d` | Remove the cursor's tag from every run the list holds, after a y/n question. |
| `r` | Read every page of the list again. |
| `Esc` | Clear the find text if there is one; otherwise close without applying. The key `t` does the same. |

A picked tag has a `+` in front.
Every find word must occur in the tag, ignoring case.
A word starting with `^` must start the tag.
For example, you can type `^topic:csv`.
Typing `project:` lists the projects.
Enter keeps the find text and closes the find line.
Esc clears the find text.

With no picks, applying uses the cursor's tag alone.
The Runs summary then reads like `12 of 212 runs match +project:billing-export`.

Removal first asks `Remove topic:csv-export from 5 runs? y/n`.
Press `y` to send the removal.
Any other key withdraws it.

Counts cover the runs that the list holds.
The box title tells you which snapshot you have.
The full-list title gives the time the whole list was read.
The other title means only the newest 100 runs are held.
The Runs list refreshes every 5 seconds.
That refresh updates counts while the explorer is open.
Your cursor stays on its tag.

Picks use AND: each matching run must hold every picked tag.
The Runs list shows only those matching runs after you apply.
Picks combine with Runs filter words and the `p` repository limit.
Esc on the Runs screen clears all three together.
Counts narrow to tags on runs matching your picks.
The explorer starts with the Runs list's existing picks.

Removal stays off until every page has been read.
Until then, it says `remove needs every run loaded: press r to load them all, then d`.
After `y`, ham sends removals in batches of 40 runs.
It sends one batch after another.
It stops at the first failed batch.
The note then reads like `removed topic:csv-export from 5 runs`.
A partial result reads like `37 of 40 runs changed`.
It also counts runs answering not found, invalid, or conflict.
Removal puts the tag on each run's removal list.
The tag stays removed for that run.
Runs outside the held list remain unchanged.

A mouse press moves your cursor to a tag.
A second press on that tag applies it.
Each mouse wheel notch moves your cursor one row.
A click under the overlay does nothing.
Clicking the key line's `Space` help chip picks the tag.
With `mouse = false` or `HAMH_NO_MOUSE`, the keys do everything.

With no tags anywhere, the box says `No tags yet.` and tells you how to add one.
New runs get topic and tech tags a few seconds after starting.
A run on a repository shows `project:<repository>` at once.
You can add a tag on an open run with `/tag +topic:name`.
A find with no match shows `No tag matches` and your text.

An older service's row may lack a `tags` member.
That row shows only its derived project.
It counts under `have no tags`.
Without an edit route, removal answers `this service cannot edit tags yet`.

#### Finding a run

`/` opens a one-line filter at the bottom of the screen.
Every character you type is the filter's, so a `q` or an `n` is text there and not a key.
The list narrows as you type.
Every word must be found, ignoring case, in the run id, the status words (a run that waits also reads `needs input` and `needs you`, a failed run reads `failed`), the repository or `s3-only`, the first line of the run's prompt, its name, its tags and its model.
The prompt line, the name, the tags and the model come from the run index; a run that has no index item is found by the other fields.
The characters that matched are underlined.
The line under the list counts the runs, for example `2 of 6 runs match "calc"` or `no run matches "zzz" (6 runs)`.

Enter keeps the filter and gives the keys back; the help line then reads `Esc clears`.
Esc clears it.
Up and Down, Backspace and Ctrl-U (clear the line) work while the line is open.
A line that starts like a command (`/runs`) filters nothing, and Enter on it says that commands run in the message box of a run.
A selected run stays selected when the list narrows, if it is still shown; otherwise the selection stays at its place in the shorter list.
Enter on a list with no match opens nothing.

`p` limits the list to the repository of the selected run (a run with no repository belongs to `s3-only`).
The line under the list reads `4 of 300 runs in cc:calc`.
The bottom border of the table says where the selected run stands among the runs shown, at its right end: `run 12 of 769`, or `run 2 of 4` while the limit is on.
With no run shown there is no position.

The newest 100 runs are read again every 5 seconds while this screen is open, so a run that starts to wait shows its `!` without a key.
A read that is still unanswered is never followed by a second one.
A run older than the newest page changes only when you press `r`.
When a read fails, the list on screen stays and the note line says so.

#### The mascot

ham has a mascot called the ham mascot.
It is a small orange character with no mouth.
It has two big white eyes that look up.
It wears a dark shirt with small arms and light shorts.
The picture is an original pixel drawing with no logo or text.

The mascot's head can stand beside the name at the top left.
The terminal must have 31 rows or more.
It must also be at least 72 columns wide.
The head is 8 columns wide.
With 33 rows or more it is 5 rows tall.
It shows the character, a short flame under its legs, and a puff of grey smoke below the flame.
With 31 or 32 rows it is 3 rows tall and shows the character alone.
It appears on every screen: Runs, New run, and the run screen.
It also appears on Artifacts, the model choice, and the reply screen.
Settings and the tag explorer show it too.

With the head, the top of the screen takes five rows, or three rows at 31 and 32 rows.
The head fills columns 1 to 8 of those rows.
The screen's title starts on the first row after ten columns.
The version tag stays at the right end of the first row.
The rest of the screen starts on the sixth row, or on the fourth row at 31 and 32 rows.
Nothing else is drawn in those rows.

At 30 rows or fewer, the title stays on one row.
No mascot stands beside it.
The same applies when the terminal is under 72 columns wide.
That includes 80 by 24, 100 by 30 and 120 by 30.
The head appears as soon as the window meets both sizes.
It goes away when the window becomes smaller again.

tmux and screen take a window row for their status line.
A 31-row tmux window therefore gives ham 30 rows.
ham shows no head there.

The mascot never covers text.
The Open anything box covers the mascot while the box is open.
Ctrl-O opens that box.

The large form is 14 columns wide and 12 rows tall.
It shows the character standing on a short flame under each foot, with grey smoke below the flames, like a rocket.
It appears in the empty table on the Runs screen.
ham must first read your list and find no run.
The table must have 18 columns by 14 rows free.
A smaller table shows the head with its flame and smoke instead.
That head needs 12 columns by 7 rows free.
A table with 12 columns by 5 rows free shows the character alone.
A table smaller than that shows nothing.
The large form can appear without the head beside the name.

The large form never appears before the first list has loaded.
It never appears when you have at least one run.
It never appears when a filter matches no run.
It never appears when reading the list failed.
It never appears on any other screen.

The smoke under the flames moves a little while the large form is on the screen.
The smoke under the head beside the name moves too, on every screen, when the head has 5 rows.
The picture has three frames.
The frame changes once a second, on the timer tick that the app already has.
The head and the large form are always in the same frame.
Each change moves a few smoke pixels by one cell.
The character and the flames never change.
The plain look moves the same smoke.
The 3-row head at 31 and 32 rows has no smoke and does not move.

`ui.mascot_motion` is the setting for the smoke.
It is on unless the file says false.
`ham config set ui.mascot_motion off` shows the still picture, which is the first frame, in the head beside the name and in the empty table.
The Mascot motion row on the Settings screen also changes it, and a change applies at once.
`ui.mascot` set to off hides the mascot whatever this setting says.

The mascot has two looks.
The colour look draws half blocks in twelve colours.
The plain look draws ASCII characters with no colour.
Both looks use the same cells and the same size.
The plain look uses @ for the eyes.

NO_COLOR selects the plain look when its value is not empty.
TERM set to dumb also selects the plain look.
A locale that is not UTF-8 selects the plain look too.
ham reads the first non-empty value of LC_ALL, LC_CTYPE and LANG.
If none is set, ham treats the locale as not UTF-8.

`ui.mascot` is the mascot setting.
It has three values.

`auto` is the default.
It uses the look the environment asks for.

`ascii` uses the plain look, even in a UTF-8 terminal.
Use it when block characters appear twice as wide as other characters.

`off` hides the head beside the name.
It also hides the picture in the empty table.
Use it with a screen reader or to copy a clean screen.

`ham config set ui.mascot off` writes the setting.
The Mascot row on the Settings screen also changes it.
Each press of Enter moves from auto to ascii.
The next press moves to off, then back to auto.
A change applies at once.

`HAMH_NO_MASCOT=1` turns the mascot off whatever the file says.
The values `true`, `yes` and `on` also turn it off.
Any other value leaves the file's choice.

The README shows the still picture of the large form, with the flames and the smoke.
The picture file is `docs/images/ham-mascot.svg`.

### New run

The title reads `ham · New run · model: <name>`.
It always names the model the run will use.

Fields appear in this order:

1. Prompt
2. Target
3. Repository
4. Base ref
5. Options
6. Model
7. Submit

Prompt, Repository, Base ref and Options are text boxes with a caret.
Prompt grows to fill the free space.
A prompt longer than the box scrolls.
The caret and the lines around it stay in view.

Left, Right, Up, Down, Home and End move the caret.
Backspace and Delete edit text where the caret is.
Enter adds a line in Prompt.
Repository and Base ref each hold one line with no spaces. Options holds one line.
A long line wraps at the width of the box.

Esc leaves New run and keeps what you typed.
Pressing n again shows the same prompt, repository and base ref.
The model goes back to your default.
Starting a run empties the composer.

Target offers `--repo`, `--new-repo` and `--s3-only`.
Repository accepts `cc:<name>`.
With `--s3-only`, Repository shows "not used with --s3-only".
Base ref is optional.

| Key | What it does |
| --- | --- |
| Enter in Prompt | Add a new line |
| Arrow keys, Home, End | Move the caret in Prompt, Repository and Base ref |
| Backspace, Delete | Edit the text at the caret in those boxes |
| Left/Right or Space in Target | Switch targets |
| Up/Down/Left/Right in Model | Move through selectable models |
| Home/PageUp in Model | Go to the first model |
| End/PageDown in Model | Go to the last model |
| Enter in Model | Confirm the model on screen |
| `d` in Model | Save the shown model as your default |
| Tab | Move to the next field |
| Shift-Tab | Move to the previous field |
| Enter in Submit | Send the run |
| Ctrl-S | Send from any field |
| Esc | Return to Runs; the typed text stays for the next `n` |
| Ctrl-C | Clear the box that has the keyboard when it holds text; with an empty box, quit |

The composer starts on your default model.
Repository starts with `default_repo` when you set it in config.
Command line starts do not use `default_repo`.

The composer has an Options box for command-line start options.
It sits right of Base ref, on the same row.
Write options as you would on the command line.

- `--rules <directory or .md file>` selects rules for this run. These replace the rules from `~/.config/ham/rules/`.
- `--no-rules` starts with no rules.
- `--continue <file>` selects a nudge file. This replaces `~/.config/ham/continue.md`.
- `--input-wait <minutes>` sets the answer wait. Use 0 to 43200 minutes, which is 30 days, the longest run. The run cuts the window to the time its session has left.

Put any word containing a space in single or double quotes.

An empty box uses your default rules directory, `~/.config/ham/rules/`.
It also uses your default nudge file, `~/.config/ham/continue.md`.
The answer window comes from config, or defaults to 30 minutes.

The box uses the same parser as the command line.
The same words start the same run in either place.

The empty box shows a dim hint with the words it takes, as many whole words as fit its width:
"--no-rules · --rules · --continue · --input-wait · --goal · --attach · --compute"

These errors appear on the message line:

- An option the box does not take, such as `--repo` or `--model`. Those options have their own fields.
- A word that belongs to no option.
- An option with no value.
- An unclosed quote.
- `--rules` together with `--no-rules`.
- An `--input-wait` outside 0 to 43200.

Nothing is sent when an error appears.

Submitting uses the same start as the command line form.
It uses `--direct` and `--model`.
A bad repository or model produces an error line in the composer.

### Dashboard

The Dashboard is the run screen. It is a chat.

The first line starts with this header:
`ham · Run <the whole run id> · <model name> · <state>`

`<state>` shows the run's status, such as active, complete, or blocked.
It shows "needs you" while Cline waits for an answer.
`<model name>` names the model the run uses now.
After a provider failure, it names the new model, such as GPT-6.1 Sol instead of Claude Sonnet 5.5.
Another route for the same model leaves the name unchanged.
A fresh Cline session starts on the model you chose, so the header shows that name again until the next switch.
The status block, the card at the end of the conversation, keeps `Model  <the model you chose>`.
During the run, the line below it reads `Route  running on GPT-6.1 Sol after 1 switch` and carries any model-call trouble.
A finished run has no Route line; its `Ran on` line says where it ended.
During model-call trouble, the header adds the cause after `<state>`: `active · provider error, try 2 of 4`, `waiting on the model provider`, or `every model route down for 30 min`.
These words disappear when the engine works again or the run ends.
The time the run has taken follows, such as `12m40s` or `1h02m`.
It counts while the run goes on and stops at the time the run ended.
The tokens and the cost follow when the client knows them.
They are marked with the turn they belong to, such as `113k tok · $0.14 · as of turn 3`.
Cline sends no usage while a turn runs, so the totals are read again each time a turn ends.
The header totals input, output, cache read and cache write tokens reported by the run's turns, matching the delivery card's sum.
The header ends with `updated 2s ago`, the age of the last good read of the run.

The tokens and cost come from the run's detail.
The client loads that detail when the run opens.
It loads the detail again when a turn ends and when the zip is delivered.

On narrow screens, the header drops its tail first: the age of the last read, then the tokens, the cost and the turn label together, then the time.
It drops the model last.
The state and the model-call trouble words stay after the model is gone.

The name row sits directly under the header and shows the run's name.
The header line itself does not change.
The row is plain when the model wrote the name and dim when it is a seed.
A run you start from the New run screen shows its seed at once.
While the shown name is a seed and the run is less than 60 seconds old, ham reads the run's row about every 5 seconds and swaps in the model's name when it arrives.
It stops at the first model name, and at 60 seconds.
A name that arrives later shows when the Runs screen next refreshes.
ham also reads the run once when you open it.
The tags of the run follow the name on the same row, in dim text, after two spaces.
The row shows whole tags first, then `+N` for the tags that do not fit; `+N` stands alone when not even one whole tag fits.
The name keeps its place and its width first.
Each draw reads the run's effective tags from its row in the Runs list that the app holds.
The tags change when you change them with `/tag` and when the 5-second refresh of the Runs list brings new ones.
A run you opened before the Runs list was read shows none.
The name row is left out when the screen is under 20 columns wide or has no spare row.
On a short screen it goes first, so the conversation keeps its three rows.

The conversation sits under the header.
For a run whose image stores no conversation, its first row says "progress feed only: not the full conversation".
That screen shows updates the service sends while the run works.
A run that stores its conversation shows it in full; see "The full conversation".
Each Cline message shows only its last 500 characters.
These message updates arrive at most every 5 seconds.
Tool updates show each call's title and status.

Your prompt comes first, with the role word `you`.
The prompt appears once the run's detail has loaded.
Cline's messages follow, marked `cline` or the engine's name.
They are drawn as Markdown: headings and strong text in bold, emphasis in italics, code in yellow, lists with bullets, quotes, tables and rules.
A link shows its text and then its address in parentheses, such as `docs (https://example.test/a)`; nothing is a clickable hyperlink.
A message with no Markdown in it is drawn as plain wrapped text.
Your words, tool calls, thoughts and notices are drawn as written.
A diff, when the conversation holds one, is coloured: added lines in green, removed lines in red, and the words that changed between a removed and an added line reversed.
While a search marks a message, that message is drawn as written, so the marks sit on the characters you searched for.
Selecting text copies what the screen shows.
Each tool call uses the role word `tool`.
Its title appears beside its status on the right.
The status is pending, running, done, or failed.
Service notices use the role word `notice`.

When the goal loop is on, its lines appear among the notices, in the order they happened:

- "Goal loop on: the engine checks the goal after each round."
- One counter line, for example "Goal loop: 7 rounds ended, the last one completed." A later round changes this line and does not add another.
- "Goal loop: continue sent." when the loop had stopped and one continue was sent.
- "Goal loop: Cline called the goal-complete tool." with "Cline's summary: ..." after it when Cline wrote one. This says what Cline did.
  It does not say that the work was checked.
- "The harness ended the run: ..." when a stopping rule ended the run. After two blocked rounds the engine's last line shows apart, in bold, as
  "BLOCKED: ...". These lines never change the label of the run, which is the one the run finished with.
- Three quiet lines in grey: the loop is off, the round log is missing, and the goal was set again for a new session.

A question that needs you keeps its own card.

The harness can record facts it read at the end of a run.
When the last entry of the run log is a red run, the status card shows one line under the outcome, marked with `!`.
An example is `The last entry of the run log is a red run: pytest exited 1 and 2 failed.`
The line is a fact the harness wrote down, as it wrote it.
It says nothing about whether the work was checked, and the outcome label stays the one the run finished with.
`ham pull` and `ham watch` print each such fact as an `Observed` line under the `Outcome` line.

When the harness ends a run because Cline kept calling tools inside one round with no changed file and no new text, the brake card says `Cline kept calling tools inside one round, but no file changed and no new text appeared, so the harness ended the run.` The label of the run names the reason in the same words as the harness wrote them.

The conversation follows its end.
Scrolling up stops this.
A row at the foot of the conversation, "[ Jump to bottom ↓ n new ]", counts the new blocks.
End or `G` follows the end again.

The keys below scroll the conversation.

Cards sit at the conversation's end, in this order:

- "status" shows run details and starts open. It shows the state. It shows the model; the title shows it too. The session names the model it ran on. Until a session does, `ham` shows the model you asked for in the New run screen, else the model the run index holds for the run. It shows where a run that switched models ended. It shows the whole outcome label, wrapped when it is long. It shows the facts the harness recorded at the end of the run, one line each under the outcome. It shows the pull request.
- "agents N" shows the agent table and starts folded.
- The blocked panel or ended-early panel appears when the run stops.
- "Cline needs you" shows an open question with its keys.
- "delivery" appears when a zip is delivered.

The mark ▾ means a card is open.
The mark ▸ means a card is folded.
Tab selects the next conversation block.
Shift-Tab selects the previous block.
Shift-Tab selects the last block when none is selected.
Space opens or folds the selected block.
Space does nothing when the block has no body to fold.

A reconnect banner appears under the header when a poll fails and stays failed for a second.
Before the first good read it waits five seconds.
It uses reverse video.
It says "⟳ Connection lost, retrying (3 s): <reason>; the run keeps going in AWS; showing the last state read".
Before the first good read it says "Cannot connect yet, retrying" instead.
The count is the time since the last good read.
The banner goes at the next good read.
Under the conversation come the result panel or the preview of a command, the pinned message box, one notice row and the key line.

These keys work in browse mode.
A run opens in typing mode, with the message box focused, so press Esc first.
Until then `a`, `c`, `d` and `o` are letters in the box (see [The message box](#the-message-box)).

| Key | What it does |
| --- | --- |
| Up/Down or `j`/`k` | Scroll the conversation one line |
| PageUp/PageDown | Scroll it a page |
| Home or `g` | Go to the top |
| End or `G` | Go to the bottom and follow the end |
| Tab | Select the next block of the conversation |
| Shift-Tab | Select the previous block, or the last when none is selected |
| Space | Open or fold the selected block |
| `a` | Open Artifacts |
| `c`, then `c` again | Cancel the run. The first `c` shows the same preview as `/cancel`; the second counts after 0.3 seconds |
| `C` | Clone the run's repository, the same way as `ham clone <runId>`: it fetches the zip itself when it is not on this machine, and clones into the directory you started ham in. For another directory type `/clone <runId> <dir>` |
| `o` | Open the pull request, or else the download link, in your browser |
| `d` | Download the zip again |
| `y` | Copy the selected block; with none selected, the pull request link when the card has one (the key line says `y copy PR link`), else the last answer of Cline |
| `p` | Send a message to the same Cline session. A "Cline needs you" question must be open. |
| `s` | Start a fresh Cline session on the same files. Include a message if you type one. |
| `m` | Pick another model for Cline. This opens Models. |
| `f`, then `f` again | Finish the run with what exists. |
| `R`, then `R` again | Resume every paused and stalled run of your list, as `/resume --all` does. |
| `n` | Open New run |
| Esc or `r` | Return to Runs |
| `q` | Quit |

The first `c` shows the preview of `/cancel` above the message box.
It names the run and ends "press c again to cancel run <runId> · Esc withdraws it".
The second `c` counts only when the preview is 0.3 seconds old; an earlier one, such as a key held down, is ignored and the preview stays.
The preview then adds the line `too soon: a second c within 0.3 s of the first is ignored; press c again`, so a press is never dropped without a word.
The line goes with the preview, and the 0.3 seconds still run from the first `c`.
Esc withdraws the preview and stays on the run screen.
Any other key withdraws it too and then does what it always does.
Alt and `c`, which a terminal sends when Esc and `c` arrive at once, is the letter `c` in browse mode.
The run that is cancelled is the one the preview named.
The first `f` shows "press f again to finish run <runId> with what exists" on the notice row.
Any key other than `f` withdraws that question and clears the line.

`p`, `s`, `m` and `f` work only while a question is open.
The question must offer the key.
Otherwise, a notice line says why.

The cancel key sends the same request as:

```bash
ham cancel <runId>
```

On a direct run, cancellation asks Cline to stop.
It keeps the work.
See the "Stopping a run" chapter.

#### Searching the conversation

In browse mode, `/` opens the search bar.
The bar sits one row above the message box.
In typing mode, `/` is text.

While the bar is open, every character joins the query.
This includes `n`, `q`, `c`, and the other hotkeys.
Matching ignores case.
A query of only blanks finds nothing.
The query is cut at 256 characters.

The bar shows the current match number and total matches.
For example, `/query  3/17` means match 3 of 17.
With no match, the bar says "no matches".
The search keeps at most 10,000 matches.
At that limit, the bar reads `1/10000+`.

Typing moves the view to a match.
It picks the first match at or after the first-row block.
If the view follows the end, it picks the newest match.
The match moves to the middle of the view.
Every match is marked in the conversation.
The current match uses another colour.

Enter accepts the search.
The bar stays visible.
The marks stay visible.
Then `n` moves to the next match.
`N` moves to the previous match.
Both wrap at the ends.
The bar says "wrapped" when this happens.
Before Enter, `n` is part of the query.
After Esc, `n` is "new run" again.

Esc while typing cancels the search.
The view returns to where it was.
Esc after Enter only clears the marks.
Esc never leaves the run screen while a search is open.

The search reads the heads and bodies of blocks.
It skips the role words: you, cline, tool, notice.
It also skips the line "progress feed only".

A match inside a folded card opens that card.
An agents table row can contain such a match.
The card closes when the search moves on.
It also closes when the search is cancelled.
After Enter, the open card stays open.

New events update matches while a search is open.
These updates do not move the view.
Leaving the run ends the search.

While typing, the key line shows:
"Enter accept · Esc cancel · ↑↓ previous and next match"

After Enter, the key line shows:
"n next · N previous · Esc clear the search"

| Key | Action |
| --- | --- |
| Backspace | Remove a character. |
| Ctrl-W | Remove a word. |
| Ctrl-U | Clear the query. |
| Down or Ctrl-N | Next match while the bar is open. |
| Up or Ctrl-P | Previous match while the bar is open. |
| `n` | Next match after Enter, wrapping at the ends. |
| `N` | Previous match after Enter, wrapping at the ends. |

#### Links on the delivery card

Once the run has delivered its zip and the zip has downloaded, the card's Link line is one short line. This example uses made-up values:

```text
Link      s3 · python-calc-<runId>-abc1234.zip · expires 06:12 MDT · o open · d download
```

`s3` names the host of an S3 link. Next come the zip's file name and the time the link stops working. The time uses your machine's time zone and shows its abbreviation. A narrow terminal cuts the file name short in the middle; the end of the name stays. The line always fits on one screen line. The dashboard never draws the download link itself, and no notice or error message holds it.

From then on the footer lists the link keys first, as `o open · d download`.

- **`o`** opens a link in your default browser. If the run has a pull request, `o` opens it; the footer then reads `o open PR`, and the Link line lists only `d download`. Without a pull request, `o` opens the download link. `ham` makes that link when you press the key, so it never opens a link that has expired.
  - On macOS `ham` runs `open`; on Linux it runs `xdg-open`. The variable `HAMH_OPEN_COMMAND` names another program instead. The program gets the link as its one argument, and no shell reads it. `ham` opens only `http` and `https` links.
  - The notice line says `opened the download link in your browser` or `opened the PR link in your browser`. If the program is missing or fails, it says, for example, `could not open the link: could not run xdg-open: ... (set HAMH_OPEN_COMMAND to a program that opens a URL)`. The message never holds the link. The zip download link is a bearer link. It is never copied. If no browser opens it, the notice line shows that failure message. Then `d download` on the Link line saves the zip instead. If no browser opens the PR link, `ham` copies it to the clipboard. This happens on the same key press. The notice line says `could not open a browser: copying the PR link instead`. The clipboard answer follows it, for example `could not open a browser: the PR link is copied. Copied 25 bytes with OSC 52.`
  - When your machine's clock passes the time on the Link line, the line reads `expired 05:40 MDT` and offers `o new link · d download`. Pressing `o` opens a link made at that moment.
- **`d`** downloads the zip into the download directory again. It uses the same code as `ham pull`: a new link, ranged downloads and the SHA-256 check. A file that is already in place and matches is kept, so nothing is fetched twice. An expired link is replaced once.
  - While it runs, the card keeps all its lines, and the Zip line reads `Zip       downloading <n> bytes…`. When it ends, the notice line reads `saved <path> (sha256 matched)`.
  - If a repeated download fails, the earlier zip stays on the card and the notice line reads `download failed: <reason>`. If the first download of the run failed, the card reads `download failed: <reason>` and `d try again, or run: ham pull <runId>`.

`o` does nothing until the run has a pull request or a zip, and `d` does nothing until the zip is delivered. The notice line then says `there is no link yet: nothing is delivered` or `nothing is delivered yet, so there is nothing to download`. While the zip is still downloading, `d` says `the zip is already downloading`.

Output that is not the dashboard prints the whole link on one line that you can copy. This holds for `ham watch` without a terminal, for `--json`, for `ham pull` and for `ham link <runId>`.

Direct runs have no approvals.
Cline runs with auto-approve on.

### The full conversation

You see the stored conversation when the run's image stores it.

You see every message from Cline.
You see every tool call, including its input and output.
Tool calls are folded.
You press Enter or Space to open them.
You also see plans and the end of each turn.
Your own messages appear too.
Cline's thoughts are not stored.

You load the conversation in pages of 32 chunks.
Loading starts at the end of the run.
You load older chunks by scrolling to the top.
Your view stays on the block you were reading.
A working run is read again every 3 seconds.
An ended run is not reread after reaching its end.

You see the progress feed while the stored conversation has not arrived.
You also see it when the image stores no conversation.
Its first row says "progress feed only: not the full conversation".
A run from an older image keeps that row.

If reading fails, you see this notice:
"could not read the stored conversation yet (...); showing what the run reported, and trying again".

ham tries again after 10 seconds.
The next waits are 20 seconds, then 40 seconds.
After that, ham tries again every 60 seconds.

You see holes as visible lines.
Each line includes chunk numbers or a count when known.
The lines are:

- "[chunk unreadable]"
- "[storage failed]"
- "[dropped under load]"
- "[cut: a size limit]"
- "[withheld by redaction]"
- "[conflict: two writers overlapped]"
- "[the run ended abnormally: the tail is unknown]"

Thoughts and binary output are left out on purpose.
You do not see holes for those omissions.

A record kind ham cannot name appears under its label.
You see it as a collapsed block.
The record is never dropped.

Each stored conversation block is the engine's text.
It never gets the "you" role.
It never becomes a card.
It never offers a key.
It is never run as a command, whatever it says.

Your prompt comes first.
Service notices appear among engine blocks by the time they happened.
Your own messages appear among those blocks by their times too.
The engine's own records keep their order.

You see the newest message tail while storage trails the live feed.
It appears as a live line: "live · not stored yet".
That line stays until storage holds the text.

You see the same view after closing ham and opening it again.

### Models

Models lists the models the open question offers.
Each row shows the model name.
Each row also shows its context size.

| Key | What it does |
| --- | --- |
| Up/Down or `k`/`j` | Move through the models. |
| Enter | Pick the model. This opens Reply. |
| Esc | Return to Dashboard. |

### Reply

Reply is a typing screen for the message of an answer.
It shows the question's title above the text.
It also shows Cline's last words above the text.

| Key | What it does |
| --- | --- |
| letters and Enter | Type the message. Enter adds a new line. |
| Backspace | Delete the last character. |
| Ctrl-S | Send the answer. |
| Esc | Return to Dashboard. The text stays while the question is open. |

A message takes some text of at most 262,144 bytes (256 KiB).
After `s` or a model pick, the message is optional.
The answer is the same request as `ham say`, `ham new-session` or `ham try-model`.

### Settings

The Settings screen shows every setting in `config.toml`, the settings file, and the five settings of the Cline engine file.
It changes settings in place.
Open it with `s` on Runs or `/settings` in a run's message box.

| Key | What it does |
| --- | --- |
| Up/Down or `k`/`j` | Move the mark. |
| Home | Jump to the first line. |
| End | Jump to the last line. |
| Enter | Change the marked setting. |
| `u` | Use the default by removing the key from `config.toml` or, on an engine row, from the engine file. |
| `e` | Open `config.toml`, or the engine file on an engine row, in `$VISUAL`, else `$EDITOR`, else `vi`. |
| Esc | Go back to the screen that opened Settings. |
| `q` | Quit. |

Each setting has one row.
The row shows the setting's name and its value in force.
It also shows the value's source and when changes take effect.
The source reads `default`, `file`, or `env` followed by the variable name.
For example, it may read `env HAMH_NOTIFY`.

Changes to Notifications, Bell, Mascot and Mascot motion take effect now.
Other changes take effect at the next run.
Some take effect at the next New run screen.
Others take effect after restarting ham, the terminal app.

The rows have five groups:

| Group | Settings |
| --- | --- |
| Run | Model, Answer wait, Test report, Goal loop, Rules, Skills, Web research, Lifecycle flow |
| Cline engine | Reasoning effort, Command time limit, Compaction, Compaction trigger, Tools switched off |
| Interface | Mouse, Notifications, Bell, Mascot, Old-name notices, Mascot motion |
| Paths | Default repository, Download folder |
| Connection | Service address, Region, AWS profile |

The line "Locked by the harness" appears below the rows.
It stands for seven classes that no file or flag can change.
These are credentials, provider, telemetry, processes, approvals, network and owned elsewhere.
The line is always one row.
It names the classes that fit the window, in that order, and ends with a count of the ones it leaves out.
At 80 columns it reads "credentials · provider · telemetry · +4 more".
A wide window names all seven.
Enter on that line shows each class and its reason.
Esc closes the list.
On a short window the list scrolls so that the marked line stays in view with the classes under it.

Enter writes the explicit opposite value for a switch.
The switch rows are Test report, Goal loop, Rules, Skills and Web research.
Notifications, Bell and Mascot motion are switches too.
A switch showing "stage default" is turned off.
For Mouse, Enter goes through off, auto and on.
For Mascot, Enter goes through ascii, off and auto.
For Lifecycle flow, Enter goes through full, quick and auto.

Answer wait opens a one-line box.
Default repository and Download folder open the same kind of box.
So do Service address, Region and AWS profile.
Type the value, then press Enter to save.
Esc cancels.
A wrong value keeps the box open and says why.

Model opens the list of models you can choose.
Up/Down move the mark.
Enter chooses the model.
Esc cancels.

The `Cline engine` group shows the five settings of `engines/cline.toml`.
Its rows sit between the Run rows and the Interface rows.
Each row shows its value in force, its source and when a change takes effect.
The source reads `file` when the engine file sets the key and `default` when it does not.
A change takes effect at the next run.
Without a setting, Reasoning effort reads `model default`, Command time limit `600 s`, Compaction `agentic`, Compaction trigger `0.90` and Tools switched off `none`.

Enter on Reasoning effort goes to the next level: default, off, low, medium, high, xhigh and round again.
Enter on Compaction does the same through agentic, basic and off.
Enter on Tools switched off goes through none, `spawn_agent`, `fetch_web_content` and both.
Enter on Command time limit or Compaction trigger opens a one-line box.
Type a number and press Enter to save.
Esc cancels.
A value the check refuses stays in the box and the box says why.

On an engine row, `u` removes the key from the engine file, so the default applies again.
On an engine row, `e` opens `engines/cline.toml` in `$VISUAL`, else `$EDITOR`, else `vi`.
After the editor ends, Settings reads both files again.
The first change creates the file with `schema = 1`.
Only the line that changes is written, so the file keeps its comments, its order and every other line.
A change to the engine file asks for no second Enter and shows no note about older builds, because an older ham does not read the `engines/` folder.

When `engines/cline.toml` cannot be read as TOML, the five rows are read-only.
That happens for a syntax error, a NUL byte, a file that is too large, text that is not UTF-8, or a file written for a newer schema.
Enter then shows `engines/cline.toml cannot be changed here until it can be read; press e to edit the file`.
The screen lists the problems under the rows, with their lines.
It shows at most three, then a line that counts the rest and says that `ham engine cline check` lists them.
A file that is valid TOML but holds a refused key still lets you change the other settings.
No engine file can set the seven locked classes of the `Locked by the harness` line.

The file keeps its comments, its order and every other line.
Only the line that changes is written.
After `u`, a later change to the default reaches you.
After the editor ends, Settings reads the file again.

The first change adding a table shows a note before saving.
The tables are `[run]`, `[research]` and `[ui]`.
The note waits for a second Enter.
Older builds of ham refuse a file that holds a table.
The note explains how to go back.
Run `ham config unset <key>`.
Or delete the table and `configVersion` lines.
Esc cancels.

Some changes ask you to type the setting's name before the second Enter.
These are Service address, Region, AWS profile and Download folder.
For example, type `region` when asked.
These settings decide where requests go or who signs them.
They also decide where downloads land.

Settings opens read-only when `config.toml` cannot be read as TOML.
It shows the problem and its line.
Causes include a syntax error or a NUL byte.
The file may also be too large or nested too deep.
Only `e` and Esc work in this case.

A file with a bad key shows its problems.
You can still change the other settings.
A file written by a newer ham can be read here.
It cannot be changed here.

### The message box

The Dashboard, the run screen, has a message box below its content.
The box sits above the key line.
It grows with its text, up to a third of the screen height.
It is at least three rows tall.

A run opens in typing mode, with the box focused.
In typing mode, letters become text.
No letter acts as a command.
The box title says "Message to Cline".
A marker appears with the title.
The terminal's cursor sits at the caret.

Esc returns to browse mode and keeps your text.
The dashboard keys work as before in browse mode.
The box title then says "Message to Cline · i or Enter to type".
Press `i` or Enter to start typing again.
Esc in browse mode returns to the Runs screen.

Enter sends the text, and what it does depends on the run.
It answers Cline's open question, sends a message to a working Cline, prepares a follow-up on a finished run, or offers to start a blocked run again.
The box title says which.
The next section describes each case.

A line that starts with a slash and a command word is a command, not a message.
The next section describes the commands.

A paste never sends, with or without bracketed paste.
In typing mode, a bracketed paste is inserted whole.
A paste over 150 characters, or of three lines or more, shows as `[Pasted 3 lines]`, drawn in cyan, and its text is kept aside.
A second paste of the same size is `[Pasted 3 lines #2]`.
Tab expands the placeholder under the caret into the text that was pasted.
Backspace at the end of a placeholder and Delete at its start take the whole placeholder.
Sending, the history and the editor use the text that was pasted, never the label.
The pasted text lives in memory with the draft and goes when the draft is sent or emptied.
Without bracketed paste support, the terminal sends pasted text as key presses.
Keys less than 30 milliseconds apart count as a burst.
Enter inside a burst adds a new line.
No key can send within 150 milliseconds after a paste or burst.

Alt and a letter usually works like Esc followed by that letter.
The terminal sends this when you press Esc and the letter at once.
The exceptions are `b`, `d`, `f`, `h`, `n`, `p` and `v`.
These are word keys in the text editor.
They are not read as hotkeys.

Each run keeps its own draft in memory while ham runs.
Nothing is written to disk.
Quitting with an unsent draft shows this warning once:
"unsent text is lost when ham ends: press Ctrl-C or q again to quit"

The bottom key line shows the keys for the current mode.
In typing mode, it says that the dashboard keys come after Esc, with as many of them as the width allows.
From 97 columns wide it reads:
"Enter send · Ctrl-J new line · Esc browse, then a artifacts, c c cancel · Ctrl-C clear, then quit"
Once the run has a link, `o open` and `d download` join the keys after Esc, and the line needs more columns for them.
From 84 columns it reads "Esc browse, then c c cancel", and from 80 columns "Esc browse (c c cancel)".
On a narrower screen the line is "Enter send · Ctrl-J new line · Esc browse · Ctrl-C clear, then quit".

| Mode | Keys | What they do |
| --- | --- | --- |
| Browse | `a`, `c c`, `C`, `o`, `d`, `p`, `s`, `m`, `f f`, `n`, `q`, Esc, `j`, `k`, `g`, `G`, PgUp, PgDn | Work as before. |
| Browse | `r` | On a blocked run, offer to start it again with the request it was started with. Press `r` again, 0.3 seconds or more later, to start it. On any other run, go back to the Runs screen, as before. `c` on a blocked run starts no cancel; the run has ended. |
| Browse | `w` | Withdraw the chosen message. With no choice, withdraw the newest message that nobody has claimed. |
| Browse | `x` | Show a preview confirming a held message or one that may have reached Cline. Use the only waiting message. When two or more wait, use the chosen message. With none chosen, choose the first. |
| Browse | `h` | Choose the next message waiting for confirmation. Wrap around. |
| Any | Ctrl-O | Open the box that searches your runs and their conversations. |
| Any | Ctrl-L | Repaint the whole screen, on every screen and in either mode. Use it when part of the screen was not drawn again after you switched to another app and back. See "Repainting the screen". |
| Browse | `i` or Enter | Start typing. |
| Browse | Esc | Go back to the Runs screen. |
| Typing | Enter | Send. The text answers Cline's open question, goes to a working Cline as a message, prepares a follow-up on a finished run, or offers to start a blocked run again. A command runs or shows its preview. |
| Typing | Alt-Enter | Send the whole box to Cline as chat, exactly as typed: no command is read from it, nothing is trimmed, and a leading space, a `//` or a second line stay. This needs a terminal that reports the key (see "Keys that depend on your terminal"); `/send <text>` works everywhere. Inside a burst it adds a new line. |
| Typing | Ctrl-J or Shift-Enter | Add a new line. Ctrl-J works in every terminal; Shift-Enter only where the terminal reports it (see "Keys that depend on your terminal"). |
| Typing | Backslash before Enter | Add a new line. The backslash goes. |
| Typing | Arrow keys, Home, End, Backspace, Delete | Edit at the caret. |
| Typing | Up on the first row, Down on the last row | Walk through the lines you sent, newest first, then the earlier messages of this run, including its prompt, then the prompts of your earlier runs. Down past the newest gives your draft back. |
| Typing | Ctrl-R | Search those lines. The row under the search bar shows the match with every place where your query occurs in reversed bold text. Type to narrow, Ctrl-R again for an older match, Enter to take the match, Esc to keep your draft. |
| Typing | Esc | Give the draft back when a recalled line is on screen; otherwise return to browse mode and keep the text. |
| Typing | Ctrl-C, with text in the box | Clear the box. |
| Typing | Ctrl-C, with an empty box | Quit. Unsent text elsewhere triggers the unsent-text warning. |
| Typing | Tab | Expand the `[Pasted n lines]` placeholder under the caret. |
| Typing | Ctrl-G | Edit the draft in your editor (`$VISUAL`, else `$EDITOR`). |
| Typing | Ctrl-L | Works while you prepare a follow-up on a finished run. The follow-up panel must be open. Adds Cline's last message after your text and the digest. The message goes at the end of the box. It is marked as written by Cline and untrusted. It is cut to 1,500 characters. A second press adds nothing. The key also repaints the screen, with or without the panel. Outside a prepared follow-up it repaints and says what it is for. |
| Typing | Ctrl-U | Does nothing. |

### Keys that depend on your terminal

ham does not ask the terminal for keyboard enhancement.
It does not enable the kitty keyboard protocol, which reports keys like Shift-Enter separately.
Most terminals send the same code for Shift-Enter and Enter.
ham then sees Enter and sends the text instead of adding a line.
Shift-Enter needs a terminal set up to send it separately.

Alt-Enter needs Alt, called Option on a Mac, sent as an Escape prefix.
Without Option set as Meta, Mac terminals do not report Alt.
In Terminal.app, select "Use Option as Meta key" under Settings, Profiles, Keyboard.
In iTerm2, set the Left Option key to "+Esc" under Profiles, Keys.

Ctrl-J adds a new line in every terminal.
A backslash before Enter adds a new line too, and the backslash goes.
`/send <text>` sends text as chat exactly as typed in every terminal.
It is the form of Alt-Enter that works everywhere.
Only Shift-Enter and Alt-Enter in the key tables depend on the terminal this way.
ham does not ask the terminal to report them.

Sources: https://iterm2.com/documentation-preferences-profiles-keys.html, https://support.apple.com/guide/terminal/keyboard-shortcuts-trmlshtcts/mac, https://vtdn.dev/docs/keyboard/kitty-keyboard.

### The draft in your editor

Ctrl-G opens the draft in your editor: `$VISUAL` if it is set, else `$EDITOR`.
The value may carry arguments, such as `code --wait`.
It is split like a shell line, with quotes and backslashes, and started without a shell, so `;` and `&&` are only characters.
ham does not guess an editor: with neither variable set the notice row says `no editor: set $VISUAL or $EDITOR`.

ham hands the terminal to the editor and takes it back when the editor ends.
The draft goes to `draft.md` in a private directory under your temp directory (mode 0700, the file 0600), with the text of every pasted block put back.
The directory is removed when the editor ends.
What you save replaces the box.
It counts as pasted text, so a command in it shows its preview before it runs.
An unchanged draft leaves the box alone.
An editor that exits with an error, or removes the file, leaves the box as it was.
An editor that returns at once with nothing changed is usually one that opens a window and returns; the notice row then suggests its wait flag, for example `code --wait`.

### History of the message box

The history lives in memory for as long as ham runs.
It holds the lines you sent, newest first, then the earlier messages of the run on screen (its prompt among them).
Nothing is written to disk.
A line that comes back from the history is treated like pasted text: a command in it shows its preview before it runs.
Editing a recalled line ends the walk; Up then starts a new one from what the box holds.
After those, Up and Down reach the prompts of your earlier runs, newest first.
They come from the Runs list, which gives each run only a title: the first line of its prompt, cut at 80 characters with an ellipsis.
The run on screen is not listed again; its own prompt is in the history already.
The box shows the title at once and fetches the full prompt from the run, one request for each run.
The full prompt replaces the title when it arrives, but only if you have not changed the box since.
While it is on its way, Enter sends nothing and the notice row says why.
The wait ends when the prompt arrives, when the fetch fails, when the stage returns something that is not that run's prompt, or after 10 seconds at the latest.
In the last three cases the notice row says that only the first line of the prompt is in the box.
Enter then asks once: the first Enter says so, and a second Enter sends the line as it is.
Edit the line or press Esc to change it instead.
Ctrl-R searches these titles too, and a match you take is fetched in full the same way.
`ham watch <run>` reads the Runs list once, so these prompts are there when the chat opens without the Runs screen.
A prompt that was fetched is kept for as long as ham runs.

In the Ctrl-R search, the row under the bar shows the matching prompt on one line; a line break shows as ⏎.
Every place where your query occurs is reversed and bold, so the marks show without colour.
Letter case does not matter, and an empty query marks nothing.
A prompt wider than the terminal is cut around the first occurrence, with an ellipsis on each side that was cut.

### Messages to a working Cline

Your message box title shows what Enter will do.

After "Message to Cline", you see one of these titles:

- "Enter: Send next to run <first 8 characters of the run id>"
- "Enter: Answer question"
- "Enter: Prepare follow-up from commit <7 characters>: new billable run"
- "Enter: Prepare follow-up from the delivered result"
- "Enter: Interrupt run <id> (/now)"
- "Enter: Retry blocked run <first 8 characters of the run id>: new billable run"
- "Not available: Cline is not waiting; /cancel stops the run"

The interrupt title appears when your text starts with `/now`.
When more than 3 messages are waiting, the send and interrupt titles end with "· <n> waiting".

**Answer a question**

When Cline asks a question, Enter sends your answer.
You stay in the same Cline session.
Your answer is sent as a message.
After Enter, the box empties.
A row appears above the box: `you <your text> — sending`.
When the service takes the answer, the row reads `sending: waiting for Cline to take it`.
Your answer appears in the conversation as your own block, labeled `you`.
When Cline takes the answer, the question closes.
The row disappears.
Your answer block stays.
If the service refuses the answer, your text returns to the box.
The notice line shows the service's words.
No row remains.
No answer block remains.
Enter is bound to the question the title showed.
If the question closed or was replaced less than 300 ms before your key, Enter is blocked once.
Your text stays in the box when Enter is blocked.

**Send a message while Cline works**

If the run takes messages, Enter sends your text.
Your message arrives after the current turn.
It becomes its own turn.
Your message box empties.
A row above the box shows your message.

An older run's image may take no messages.
Your text then stays in the box.
You see this notice:

"Cline is not waiting for an answer; /cancel stops the run and keeps the work"

**Retry a blocked run**

A run that ends as `blocked` delivered nothing, and its session is over.
It takes no answer and no message, and there is nothing to cancel.
The blocked card at the end of the conversation reads "blocked · r retry" and gives the reason.
The box title reads "Enter: Retry blocked run <first 8 characters of the run id>: new billable run".
The key line reads `r retry` where it read `c cancel`.
In typing mode it ends with "Esc, then r retry".
The words after Esc name `a artifacts` when the line has room, and never a cancel key.

Enter on your text, `r` in browse mode and `/retry` give the same offer.
Nothing starts yet.
The panel above the box says what a retry does.
It starts a new billable run with the same request as the blocked run: the same prompt, target, model, rules, skills, settings and attachments as when you started it.
`ham` sends none of that.
The service keeps the request and starts it again.
The panel names the target once the run's details have arrived.
It says the new run is linked to the blocked one and that the new Cline receives the request alone, without the blocked run's history.
When the box holds text, it also says "Your text stays in the box and is not sent."

The second Enter, or the second `r`, starts the retry.
It counts only 0.3 seconds or more after the offer; an earlier press is ignored.
Esc withdraws the offer and keeps your text.
Any other key withdraws it and does what that key does.
When the service has started the run, ham opens it.
Your text moves to the box of the new run.
The notice reads "started run <new id>, a retry of run <blocked id>".

The new run carries the same request.
A rule, a skill or a setting you changed since the blocked run started does not travel.
The New run screen starts a run with the changes.
If the blocked run was a follow-up, the retry starts from the same earlier commit.

A blocked run has one retry.
Pressing `r` again, or on the blocked run again from the Runs list, returns the run the first retry started.
If the new run blocks too, retry the new run.
The Runs list marks a run that continues another with `↳` before its name, and the note under the table reads "continues run <id>" for the selected row.
The retry also keeps the tags you put on the blocked run.

`/cancel` and `c` are not offered on a run that has ended.
`c` says "This run already ended (blocked): there is nothing to cancel. r retries it."
The command list leaves `/cancel` out for a blocked run and `/retry` out for every other run.

The service can refuse a retry.
Ham shows its words in the notice, keeps your text and starts nothing:

| The service says | What it means |
| --- | --- |
| `409 run-active` or `409 run-not-blocked` | The run is not blocked now. |
| `422 unknown-model` | The model of the blocked run is not offered any more. Start a new run and pick another. |
| `409 no-delivered-head` or `409 head-mismatch` | The blocked run was a follow-up, and the earlier run it started from no longer holds the code. Start a new run. |
| `422 stored-request-unreadable` | The service cannot read the saved request. Start a new run with the same prompt. |
| `503` or no answer | Press the key again in a moment. The same key never starts a second run. If it keeps failing, start a new run with the same prompt. |
| "does not offer retry yet" | The stage was deployed without retry support. Start a new run. |

**Prepare a follow-up**

After the run finishes and delivers, type your text in the box.
Two lines appear above the box.
The first is `Cline is finished. Enter = start a follow-up run from this result.`
The second is `The new Cline starts a new session and sees a summary of this chat, not the whole history.`

`ham` asks the service which commit the run delivered.
This lookup takes about a second.
While waiting, the box title reads "Enter: Prepare follow-up from the delivered result: new billable run".
When the answer arrives, it reads "Enter: Prepare follow-up from commit <7 characters>: new billable run".
If no answer arrives, `ham` asks again a few times.
A stage without this route leaves the title unchanged.
The follow-up still works there.

Your first Enter prepares the follow-up.
If the run delivered no code, it keeps your text.
The screen says "this run delivered no code, so there is nothing to follow up from".

The prepared box holds your text and a digest of the run.
The digest is a quoted record.
It carries no instructions.
It holds the objective, outcome label and source commit.
It also holds the pull request link and changed files.
It includes your own messages.
It never includes tool output or the stored conversation.
It never includes the text of the service's prompts.

The commit appears as `Source commit: "<full commit id>"`. The changed files appear as `Changed files (N, from an
empty start): "path" (added), ...`. The statuses are added, modified, deleted and type changed. Each name is quoted
as data. The digest shows up to 40 names. `(omitted n more)` gives the number left out. The list comes from the
bundle validator's record. The validator writes that record when a delivery passes. Without that record, the digest
reads `Changed files: unavailable (no verified list was recorded for this delivery)`. If nothing changed, it reads
`Changed files: none (the delivery changed no path)`.

The digest also counts the service's own prompts from the run's events.
These include continues after a stop.
They include retries after a provider error.
They also include fresh-session starts.
The digest says "The run's events record N prompts that the harness sent to the engine on its own, such as continues, retries and fresh-session starts (their text is not quoted)."
This counts events.
It does not check what was sent.

Edit the text as you like.
The panel above the box names the source commit and source run.
It names the target the source run had.
For a run that delivered a zip only, it names `s3-only`, code zip without a pull request, as the new target.
It says "s3-only: the new run delivers a zip of its code on the branch harness/<new run id>, with no pull request".
For a run on a repository, it names that repository as the new target, for example `Target: cc:my-repo`.
It says the new run delivers a pull request in the same repository and a zip.
It names the three cases below.
It says this starts a new billable run from the delivered result.
The new Cline receives exactly the text in the box.
If the commit arrives while the panel is open, the panel updates.

Cline's last message stays out unless you press Ctrl-L.
Ctrl-L works while the panel is open.
It also repaints the screen, as it does everywhere.
It adds the message after your text and the digest.
It keeps your edits.
The message is cut to 1,500 characters.
Two lines surround the message.
Both lines say Cline wrote it and it is untrusted.
Both lines carry a long random word.
A second Ctrl-L says the message is already there.
It adds nothing.
If the panel offers no Ctrl-L, Cline left no last message.

Your second Enter must come at least 300 ms after the panel.
It waits while the commit lookup is still running.
The notice says "looking up the delivered commit: press Enter again in a moment".
Otherwise, it checks your runs for a follow-up still running from this run.
If there is none, it starts with the box's current text.
If there is one, the panel says `A follow-up from this run is already running: open it`.
The panel includes that run's id and state.
Enter, at least 300 ms later, opens that run.
If the check fails, the panel says so.
Enter starts the run anyway.

The start is an ordinary start.
Its target is `s3-only` for a run that delivered a zip only.
For a run on a repository it is that repository.
It uses the request member `continueFrom`, which names the finished run.
The start sends the commit you were shown.
The service resolves the delivered commit.
If the delivered commit differs, the service refuses with `head-mismatch`, a delivered commit mismatch.
The screen says so and keeps your text.
It drops the displayed commit and asks again.
Press Enter again to confirm the new commit.

Your rules travel as for any run.
The new run opens when the service has accepted it.
If the service refuses, the screen says why.
It keeps your text.
Esc closes the panel and keeps your text.
The next Enter shows the start panel again.
That Enter starts nothing.
The Enter after that starts the new run under these checks.

**A follow-up on a repository run**

The new run starts from the commit the finished run delivered.
It does not read that commit from the repository, so a merge, a delete or a push to the old branch changes nothing about where the new run starts.
When the new run finishes, its pull request goes where the old branch says it should:

- If branch `harness/<old run id>` still holds the commit the old run delivered, the pull request targets that branch.
  Its list of changes starts at that commit.
- If the old pull request was merged, or the branch was deleted, the pull request targets the branch the old pull request was aimed at, usually `main`.
  After a squash merge the old commits are not in that branch's history, so the new pull request lists the old run's changes again.
  Their content is already in the branch and merges cleanly, and a comment on the new pull request says so.
- If the branch exists but points at another commit, `ham` opens no pull request.
  The run still completes and the zip is the new run's.
  The detail reads `pull request not opened: delivery conflict: branch harness/<old run id> was moved ...`.
  Run `ham pr <new run id>` to see it.

The two pull requests link to each other.
The new pull request has a comment that names the old run, its pull request, the branch it targets and why.
The old pull request gets a comment with the link to the new one.
The zip belongs to the new run.
`ham continue` on the command line does the same: it reads the old run's target and goes to the same repository, unless you add `--s3-only`.

The chat of a follow-up run shows the conversation of the run it continues from.
That conversation stands above a divider that reads `previous run, read-only. The new session received only the prompt you submitted, not this history.`
The previous run is read once, a page at a time; scroll to the top to read older pages.
A previous run with no stored conversation still gets the divider.

**Interrupt Cline**

Use `/now <text>` to interrupt the current turn.
Your first Enter opens a preview above the box.
The Confirm box says "Interrupt Cline?".
It shows the run.
It also shows your text.
It also says: `A cancelled turn leaves no partial text in Cline's memory, so restate what your message refers to.`

After an interrupt, Cline has no half-written text from the cancelled turn.
Write your message in full.
Name what your message refers to.
For example, name the file or the task.
Avoid "that" or "the above".

Press Enter again, at least 300 ms after the preview.
This sends your message with the mode `now`.
The service cancels the current turn.
It waits for that turn to settle.
It sends your text on the same session.

A run allows 5 interrupts. The key `run.max_interrupts` changes the number; 0 allows none.
After the last one, a `/now` message does not cancel the turn.
It waits for the turn to end and goes out next, like a message sent in the default mode.
The run shows the notice "could not interrupt; delivered after this turn".
You see the same notice when Cline does not answer the cancel.
After the first stop that looks like done, a run goes on taking messages until its time runs out.
It counts no message turns and times no extra work.
A message that arrives when less than 300 seconds of the run's time remain is refused.
A `/now` message in that case does not cancel the turn that is running.
Its row shows "not delivered: <reason>".

Esc cancels your preview.

Your run's session must offer the control `message.now`.
Otherwise, `/now` tells you it is unavailable.
It sends nothing.
It is also missing from your command list.

**Read the message rows**

You can see up to 3 rows above the box.
When more messages are waiting, the rows show the newest 3 and the older ones keep waiting.
Each row uses this form:

"you <text> — <state>"

A row can show any of these states:

- "sending"
- `sending: waiting for Cline to take it`
- "waits for this turn to end"
- "sending: waiting to interrupt Cline"
- "sending: Cline is taking it now"
- `sending; no answer yet, the message is checked again by its id`
- "delivered in turn <k>"
- "withdrawn"
- "held: written to session <a>, now session <b> (x confirms, w withdraws)"
- `held, chosen: written to session <a>, now session <b> (x confirms, w withdraws)`
- `held: written to session <a>, now session <b> (h chooses)`
- "not delivered: <reason>"
- "replaced by a confirmed copy"
- "could not be sent: <reason>"
- "may have reached Cline (x sends it again)"
- `may have reached Cline, chosen (x sends it again)`
- `may have reached Cline (h chooses)`

"sending: waiting to interrupt Cline" is the state for `now` mode.
A rejected message shows "not delivered: <reason>".
One possible reason is "the run was ending".

An answer typed while a question was open shows `you <text> — sending`.
Its state then becomes `sending: waiting for Cline to take it`.
The row leaves when Cline has taken the answer.
The answer stays in the conversation as your own block.

Two or more messages waiting for confirmation have one chosen message.
These messages are held or may have reached Cline.
The chosen row reads `held, chosen: written to session <a>, now session <b> (x confirms, w withdraws)` or `may have reached Cline, chosen (x sends it again)`.
Other rows read `held: written to session <a>, now session <b> (h chooses)` or `may have reached Cline (h chooses)`.
No row says `chosen` when only one message waits.

A delivered row stays for 20 seconds.
Your delivered message also appears in the conversation.
It appears as your own block, labeled "you".

You can have 20 messages waiting at once.
A 21st message is refused with this notice:

"20 messages are waiting for Cline already; wait for one to be delivered or withdraw one (w)"

**Withdraw or confirm a message**

In browse mode, `w` withdraws the chosen message.
With no choice, `w` withdraws your newest unclaimed message.
That message can be pending or held.

If Cline has already taken it, you see a notice.
The row shows the message's real state.

In browse mode, `x` confirms a held message.
A held message was written to an earlier session.
It was kept for you after a fresh session.

You can also use `x` when a message may have reached Cline.

In browse mode, `h` chooses the next message waiting for confirmation.
After the last message, `h` wraps around to the first.

With two or more waiting messages, the first `x` chooses the first.
It shows no preview.
The notice line says so.
The `h` key moves the choice.
The next `x` shows only the chosen message's preview.
With one waiting message, `x` shows its preview at once.

The preview shows the run.
It shows the old and new session.
It shows your full text.

For a message that may have reached Cline, it warns:

"It may already have run"

Press Enter again, at least 300 ms after the preview.
This sends a new message.
The new message names the chosen one.
It also names the session shown in the preview.

If the session changed again during the preview, nothing is sent.
If the message is no longer waiting, nothing is sent.
The notice line says so in either case.

Esc cancels your confirmation.

The keys `w`, `x` and `h` work in browse mode only.
While a message can be withdrawn or confirmed, the bottom key line names the keys.
It shows `w withdraw` and `x confirm` as they apply.
It shows `h choose` only when two or more messages wait.
In typing mode, it shows `Esc, then w withdraw, x confirm, h choose` as far as they apply.
On a narrower screen the typing key line gives up the longer wordings of `Esc browse` (see [The message box](#the-message-box)) first.
The ending then stays whole from 90 columns, or from 111 columns with all three keys.
A wider screen has room for the longer wording and the ending.

**When sending fails**

A refusal from the service returns your text to the box.
The notice tells you why.
Only a refusal returns your text to the box.

If no answer arrives, your message keeps its id.
The row reads `sending; no answer yet, the message is checked again by its id`.
ham asks again with the same id every 4 seconds for 5 asks.
It then asks after 8, 16 and 32 seconds.
After that, it asks every 60 seconds.
This continues for as long as the service stays silent.

The service answers a repeat with the message it already holds.
The row follows the service's own state.
No second message is stored.
The text never returns as a new message.
The row never reads `not delivered` for a send without an answer.

If a later service answer is a refusal, your text returns then.

**Enter safeguards**

A double Enter sends your message once.

The run may change less than 300 ms before you press Enter.
For example, it may finish.
In that case, Enter is blocked once.
Your text stays in the box.
It never goes somewhere different from what the title showed.

This safeguard also covers an answer to a question.
When Enter is blocked, the box title reads `(changed just now)`.
That title stays for about 2 seconds.
Look at it again before you press Enter once more.

**Lines starting with `//`**

The engine must advertise the command word after `//`.
That line then goes to the engine with one slash.

For example, `//review the diff` goes as "/review the diff".

Your pasted text is never forwarded.
Other lines starting with `//` are refused.
The panel names `/send`.
### Commands in the message box

Type commands or messages in the run screen's message box.

A command must start at the first character of the first line.
It starts with a slash and a word, like `/cancel`.
It can also start with `ham`, a space, and a word.
For example, `ham models` is a command.
The word uses lower-case letters, digits, and hyphens.

A leading space makes the line a message for Cline.
`Ham push` is also a message.
A lone `ham` is a message.
Messages answer the open question.

`/help` lists the commands.
`/help <word>` explains one command.

These shell commands are available:

- `runs`, `watch`, `cancel`, `models`
- `pr`, `pull`, `clone`, `link`
- `doctor`, `rules`, `config`, `say`
- `new-session`, `try-model`, `finish`
- `push`, `lifecycle`, `continue`

The app also has `/help`, `/new`, `/open <runId>`, `/open <words>`, `/send <text>`, `/now <text>`, `/tag <words>`, `/retry`, `/engines`, `/engine <name>`, `/sessions`, `/settings`, `/copy`, `/export [file]` and `/mouse [on|off]`.

`/send <text>` sends the rest of the line exactly as typed.
Use it for messages that start with `/` or `ham`.
A line starting with `//` is not sent as chat.
The panel tells you to use `/send`.
Alt-Enter does the same for the whole box, where your terminal sends the key (see "Keys that depend on your terminal"): it sends the box as chat exactly as typed.
The exception is a command word that the engine advertises: that line goes to the engine with one slash.

A command without a run id uses the run on screen.
For example, `/cancel` cancels this run.

#### The command list

Typing `/` opens a list of the commands above the box.
Each letter you type keeps only the commands that start with it.
The list closes at the first blank, so it never completes arguments.

| Key | What it does |
| --- | --- |
| Up, Down | Move the marked command. |
| Tab | Write the marked command into the box. A command with arguments gets a blank after it. |
| Enter | A word that is already whole, such as `/cancel`, is sent like any line. For a part of a word: a command with arguments is completed and nothing is sent; one without arguments is sent. |
| Esc | Close the list. Type another letter and it opens again. A second Esc leaves typing. |

Nothing runs from the list on its own: the box sends the line, and the usual preview and second Enter follow.
A pasted `/word` shows the list too, and still shows its preview first.

#### Plain phrases

Five phrases are suggestions, never commands:
`push my code`, `push this run`, `pull the zip`, `show my runs`, and `cancel this run`.
The whole line has to be the phrase.
Capitals and extra blanks do not matter.
A single word such as `push` is never a phrase, so it stays a message to Cline.

When you type one of them and press Enter, it is not sent.
A small box asks "Run ham push for <run>?" with two keys:

- Enter prepares the command. The preview and the second Enter follow, as for any command.
- Esc closes the box and keeps your text. Enter then sends the same text to Cline as a message.

Any other key edits your text and closes the box.
A pasted, recalled or command-looking line gets no suggestion, and nothing Cline writes ever does.

#### Commands that run right away

Commands that only read run when you press Enter.
These are `runs`, `models`, `pr`, `link`, `doctor`, `config get`, and `rules show`.

Some commands only write files on this machine.
These also run when you press Enter.
They include `pull`, `clone`, and `rules init`.
They include `config set`, except for the four settings listed below.

Command output appears in a panel above the box.
The panel title names the command.
While the command works, the title also says "running…".
If it fails, the title shows "exit 1" or its exit code.
Command output gets the same cleaning as the rest of the screen.

Scroll long output with PgUp and PgDn, in typing mode and in browse mode.
In browse mode the arrow keys scroll it too; in typing mode they stay with the box.
Esc closes the panel in browse mode.
In typing mode, the first Esc returns to browse mode.
Closing the panel then takes two presses of Esc.

#### Commands that need confirmation

Commands that change a run show a preview above the box.
These are `cancel`, `finish`, `say`, `new-session`, `try-model`, `tag` and `retry`.
The preview names the run.
It also says what will happen.
Your text stays in the box.
Press Enter again to run the command.

Esc cancels the preview and keeps your text.
Any other key that edits the text cancels the preview.

An Enter within 0.3 seconds of the preview appearing is ignored.
Keys that are part of a burst are also ignored.
Keys within 0.15 seconds after a paste are ignored.
When the preview appears, queued keys are dropped.

Changes to these settings also need a preview:

- `api_endpoint`
- `aws_profile`
- `region`
- `download_dir`

For these changes, the preview asks for the setting's name.
Type the name and press Enter.
ASCII case does not matter.
Backspace edits the name.

If the config file changes during the preview, the command stops.
The panel explains why it did not run.

After a successful setting change, the panel adds:
"The running app keeps its old settings until you restart ham."

Pasted text always needs a preview before a command runs.
This includes commands that only read.
Text that arrives as a burst of keys counts as pasted.
A terminal without bracketed paste sends a paste that way.
This rule lasts until the box is emptied.
Ctrl-C empties the box.
Sending the text also empties it.

#### Commands that take over the terminal

`/push` needs your terminal because git asks for a login.
`/rules edit` needs your terminal because the editor takes the whole screen.
`/continue` hands the terminal to `ham continue`, which opens your editor when you typed no text and follows the new run until it ends (see "Typing /continue").

Each of these shows its preview first, like any command that changes something.
`/push` also asks for the last four characters of the run id.
The second Enter hands the terminal to the command.
Git's login prompts or your editor appear as they do in a shell.
When the command ends, `ham` takes the screen back and the panel shows what the command printed, with its exit code in the title.
A long output keeps its last 256 KiB and says that the earlier output was cut.

`/push` with no address opens a preview with a slot for the git URL.
The preview names the run (the one on screen) and the branch (`harness/<runId>`, or the one you gave with `--branch`).
Type or paste the URL into the slot; you edit it like a box.
Enter on an empty slot is refused and the slot stays open.
A URL that carries a password or a token is refused in the slot with a sentence that does not repeat it, and nothing runs.
Esc closes the slot and keeps `/push` in the box.
A good URL goes on to the confirmation above: it shows the whole push and asks for the last four characters of the run id.
`ham push` typed with no arguments does the same.

Other refused lines stay in the box.
Nothing is sent.
The panel title is "Nothing was sent".
The panel explains the reason.

An unknown command word shows the closest names.
For example, `/cancle` shows `cancel` as the closest name.
A command word the app does not have says "<word> is not available in this release".

A command must fit on one line.
A second line is refused with "use /send to send this as chat".
To put a second line into a message, press Ctrl-J, which works in every terminal.
Shift-Enter does the same only where the terminal reports it (see "Keys that depend on your terminal").

#### Typing /continue

The headless form is `ham continue <runId> '<prompt>'`.
It starts a new run from a finished run's code.
The new run goes where the old one went.
A run on a CodeCommit repository is followed on the same repository, with a pull request.
An `s3-only` run is followed by a zip alone.
`ham continue` reads the old run for its target.
Before it starts, it prints one line such as `target: cc:calc, a pull request in that repository and a zip (the target of run <runId>)`.
`--s3-only` or `--repo cc:<name>` on the line chooses the target yourself, and that choice wins.
The service continues only the repository the old run used, or a zip alone.
If `ham` cannot read the old run, it stops before your editor opens and before anything is sent.
A run on a repository that is not in CodeCommit stops the same way, and `--s3-only` follows it with a zip.
With `--json`, the first line also has `target`, as the service stores it (`{"mode":"repo","repo":"cc:calc"}` or `{"mode":"s3-only"}`), and `targetSource`, which is `run` or `flag`.

In the message box, `/continue` uses the run on screen.
`/continue --run <runId>` uses another run.

**The preview.**
Every `/continue` shows a preview and waits for a second Enter.
Press Enter again at least 0.3 seconds after the preview.
The preview starts with "Run ham continue <runId>" and the text.
Then it says this is a new billable run from the code that run delivered, names the target, and says ham waits until the new run ends.
The target is named when `ham` already holds the old run's summary: the run on screen, or a row of the Runs list.
Otherwise the preview says the target is read when the run starts.
The line is never changed.
`ham` adds no target of its own, so the run that starts is the one `ham continue` starts in a shell.

**Nothing after /continue.**
The second Enter hands the terminal to a child `ham continue <runId>`, which opens your editor (`$VISUAL`, else `$EDITOR`) for the prompt.
If the editor returns no text, the child stops with "the editor returned no prompt" and exit 2, and nothing starts.
With text from the editor, it starts the follow-up run and prints `started run <new runId>`.

**Text after /continue.**
`/continue add the missing tests` shows the same preview and waits for the same second Enter.
The second Enter hands the terminal to the same child, as `ham continue <runId> 'add the missing tests'`.
No editor opens.
The child starts the run and prints `started run <new runId>`.

**What the child does next.**
It follows the new run with line output until it ends, and ham gets the screen back when it does.
The panel shows what the child printed and its exit code.
Those are the lines `ham continue` prints in a shell whose output is not a terminal.

**A run id after /continue.**
`/continue <runId>` is refused.
The run is the one on screen, so the id would be the first word of your text for Cline.
The panel is titled "Nothing was sent" and shows the line to type, `/continue --run <runId> ...`.
The box keeps your text and nothing is billed.
`/continue --run <runId> some text` picks another run.
Once `--run` is there, a first word that looks like a run id is part of your text.
The shell form typed in the box, `ham continue <runId> 'some text'`, takes the run id first, as in a shell.
It also takes `--s3-only` and `--repo cc:<name>`.

To start a follow-up with text inside the chat, use the finished run's message box.
Type the text and press Enter, check the prepared box with your text and the run digest, and press Enter again.
See "Prepare a follow-up" in "Messages to a working Cline".
`/help` lists `/continue [--run <id>] [<text>]`.

#### Typing /tag

In the displayed run's message box, `/tag` changes that run's tags.
For example, type `/tag +project:billing -tech:s3 csv-export`.

Use `+kind:value` or a bare word to add a tag.
Use `-kind:value` to remove a tag.
A word without a kind is a topic.
So `csv-export` means `topic:csv-export`.
The kinds are `project`, `topic` and `tech`.
ham makes the words lowercase.
It turns spaces and underscores into hyphens.
It also trims the words.
A line holds at most 12 words.
Only English letters, digits and hyphens are allowed.

`/tag` writes to the service.
Like other commands that change a run, it requires confirmation.
See [Commands that need confirmation](#commands-that-need-confirmation).
You see a preview before confirming with a second Enter.
The preview panel is titled Confirm.
It reads `Change the tags of run <runId>?`.
An `Add:` line shows the normalised tags to add.
A `Remove:` line shows the normalised tags to remove.
The panel then reads `Enter applies it · Esc cancels`.
An Enter within 0.3 seconds of the preview is ignored.
Esc withdraws the preview.
Any other key withdraws it and goes to the message box.

After your second Enter, you see `changing tags...`.
Then `tags now: <tags>` shows the run's tags after the change.
You see `tags now: none` when the run has no tags.
Repeating an earlier edit can leave the tags unchanged.
Then you see `no change · tags now: <tags>`.

The panel Nothing was sent explains why ham refused your words.
The service uses Nothing was changed to explain its refusal.
Both panels use plain words, and nothing changes.

Some of the rules are:

- A tag cannot contain the words verified, tested, gated or evaluated.
- A status word such as done or blocked is not a tag.
- One edit cannot add and remove the same tag.
- A run holds at most 12 tags that you added.
- A run holds at most 32 tags that you removed.

A line with no word shows the usage.
With no run open, the line says to open one first.

A tag you remove stays removed on that run.
You can also remove a tag the model wrote.
You can remove the derived project too.
Only adding the tag again clears its removal.

A service without an edit route answers `this service cannot edit tags yet`.

The run screen shows the new tags in its name row immediately.
The Runs list gets the new tags from the edit's answer.
The explorer gets them from that answer too.

#### Opening runs and download links

`/watch <runId>` and `/open <runId>` open that run on this screen.
The box stays in typing mode.
Opening another run drops the old run's preview.
It also drops the old run's panel.

`/new` opens the New run form.

This start line opens the form with its fields filled in:

```text
ham "build a todo api" --s3-only --direct
```

The options are filled in too.
This needs one of `--repo`, `--new-repo`, or `--s3-only`.
It also needs `--direct`.
Nothing starts yet because starting a run costs money.
Press Ctrl-S in the form to start the run.

`/link` makes a download link valid for 15 minutes.
The panel never shows the link.
It says "Download link ready, expires HH:MM; u copies, o opens".
In browse mode, o opens the link in your browser.
Copying the link with u is not supported.
Pressing u shows a message that says so.

`/pull` shows the same card as `ham pull`.
Its Link row names the file without the signed part of the address.
The whole link stays out of the panel, and o opens it.

| Command | What it does |
| --- | --- |
| `/help` | Lists commands. |
| `/help <word>` | Explains one command. |
| `/send <text>` | Sends the rest of the line exactly as typed. |
| `/mouse` or `/mouse on` or `/mouse off` | Says where the mouse stands, or turns the mouse reports on or off for this session. The config file is not changed. |
| `/tag +kind:value -kind:value word` | Previews the tags to add and to remove on the run on screen and changes them on a second Enter. A word with no kind is a topic. |
| `/retry` | Offers to start the blocked run on screen again with the request it was started with, and starts it on a second Enter. On a run that is not blocked it says so and starts nothing. |
| `/settings` | Opens the Settings screen over the run; Esc comes back to the run. It reads config.toml and writes nothing until you change a setting there. |
| `/cancel` | Previews cancellation of the run on screen. |
| `/watch <runId>` or `/open <runId>` | Opens that run on this screen. |
| `/new` | Opens the New run form. |
| `/now <text>` | Previews an interrupt, then sends your text now on a second Enter. |
| `/engines` | Lists the engines, marks the one the run on screen uses, and shows that engine's capabilities. |
| `/engine <name>` | Chooses the engine for the runs you start in this session. The New run form opens on it. The config file is not changed. |
| `/sessions` | Shows the Runs screen. |
| `/open <words>` | Opens the box that searches your runs and their conversations. |
| `/pull --snapshot` | Downloads the last snapshot of the run on screen. |
| `/copy` | Copies the agent's last answer to the clipboard. |
| `/export` or `/export <file>` | Writes the conversation as a Markdown file. Without a name it goes to the download directory as `ham-chat-<run>-<time>.md`. A file that is there is never replaced. |
| `/link` | Makes a download link valid for 15 minutes. |
| `/push` or `/rules edit` | Shows a command to run in your terminal. |
| `/resume` | Previews, then continues paused and stalled runs from the code they delivered: `/resume <runId>` or `/resume --all`. |
| `/continue` | Previews, then hands the terminal to `ham continue`: your editor opens for the prompt when nothing follows, and the run starts at once when text follows (see "Typing /continue"). |

`/copy` and `y` put the text on the clipboard with `pbcopy` (macOS), `wl-copy`, `xclip` or `xsel` when one is installed.
Over ssh, and when none is installed, they write the terminal sequence OSC 52 instead, and your terminal decides whether it takes it.
The notice row names the path used.
Escape sequences are removed first, a secret-shaped value is hidden, and a text over the copy limit is refused with a notice that points to `/export`. The limit is 64 KiB; `ui.copy_limit_kib` changes it for a terminal that takes more.
`/export` makes the file from the structure of the conversation: its headings are the app's own words, and what Cline wrote sits in quotes or code blocks.
It hides presigned links and tokens.
A pasted `/export` line never writes a file: type it yourself.

### Engines and the engine's own commands

You open the engine panel with `/engines`.
The panel lists the engines.
The engines you can choose read "available".
The row of the engine the run on screen uses reads "selected".
The other engines read "not available in this release".

#### Choosing the engine of a run

A run uses one engine from its start to its end.
The first engine is cline, and a run that you give no engine runs on it.
These choices decide the engine, strongest first:

| Choice | What it does |
| --- | --- |
| `--engine <name>` | Chooses the engine of the run you start from the command line, and of `ham continue`. |
| The Engine box of the New run form | Sits beside the Target box. Press ↑ or ↓ while the Target box is focused to move it. |
| `/engine <name>` | Chooses the engine of the runs you start in this session. The New run form opens on it. |
| `default_engine` in `config.toml` | Sets the engine you use when you choose none. |

A name that is not an engine you can choose stops the start and lists the engines.
A run on the default engine sends the request that an older ham sends.
A stage that is older than your ham refuses a run on another engine and says so.
`ham engine <name>` says whether an engine is available and how to choose it.
The follow-up of a finished run in the app uses the engine of that run.
The words in the app, such as the box title "Message to <engine>", use the engine of the run on screen.

```bash
ham "Add tests for the parser" --direct --engine opencode
ham config set default_engine opencode
ham engine opencode
```

In the terminal app, `/engine opencode` chooses OpenCode for the runs you start in that session.

**What each engine does**

| | Cline | OpenCode |
|---|---|---|
| Goal loop | Cline's goal plugin drives the loop. | The harness drives the loop. |
| Settings file | `engines/cline.toml` holds five settings. | `engines/opencode.toml` holds one setting, the command time limit. |
| Shell command time limit | 600 seconds, or your `commands.timeout_seconds`. | 600 seconds, or your `commands.timeout_seconds`. OpenCode's own limit is 120 seconds, so the harness sets this one in every launch. |
| Helper agents | Starts them with `spawn_agent`. | Starts them with `subagent`. |
| Tool-idle limit | Off. | 3600 seconds. |
| Name of the code search tool | `ham__code_search` | `ham_code_search` |
| Models | Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra, GLM 5.3 and Claude Fable 5.2, each with a smoke record. | Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, GPT-6 Astra and Claude Fable 5.2, each with a smoke record. GLM 5.3 has no OpenCode record yet and is not offered. |

A model is offered on an engine only with a passing smoke record for that engine.
The service names the engine and model pair when it refuses one without a record.
The outcome sentences name the engine of the run.

The selected engine's row shows what the session said about itself.
You see the agent and its version.
You also see the mode and a few plain options.
For example: "cline selected · cline 3.0.65 · mode act · auto_approve off".

Without a capability record, the row reads:
"cline selected · capabilities unknown · <default model name> defaults".

Each session writes one capability record.
Its event name is `session.capabilities`.
ham reads the focused run's newest capability record.
The record lists the engine's modes, options, and advertised commands.
It also lists the harness controls, `message.next` and `message.now`.

The run screen's header shows the session's mode after the state.
It also shows the session's plain options.
For example: "active · mode act · auto_approve off".

You type `/` to see the command list.
It shows the built commands offered by the session.
You will not see `/now` unless the session offers `message.now`.

An engine can advertise its own commands.
You type two slashes (`//`) to see them.
Each appears as `//name` with the engine's description.
`/help` lists these commands first.
A line starting with `//` and an advertised name reaches the engine.
The engine receives that line as a message with one slash.
Pasted text is never forwarded.

A run without a capability record hides nothing.
A missing record cannot distinguish an old image from a new one.
The service's answer to a message can.
### Open anything

Press `Ctrl-O` to open one box over the whole screen.
You can use it on the Runs screen, your run list.
You can also use it on a run screen.

Use `/open` followed by words to open the same box.
Your words are already filled in.
Use `/open` followed by a run id to open that run.

You can search three things:
- The runs ham holds in its list.
- Their repositories.
- What was said in the runs.

Your words match without regard to case.
Runs and repositories return results at once.
Conversation searches use `GET /v1/search`.
The search waits until your words have rested for one second.
Your words must be at least 2 characters long.
The search is not sent twice for the same words.

You see results grouped by repository.
Each row shows the run id, its title and its state.
A conversation hit shows a snippet under its row.
The search tells you when it stopped at its time limit.
It also tells you that older runs were not searched.

Use `Up` and `Down` to move the mark.
You can also use `Ctrl-N` and `Ctrl-P`.
Press `Enter` to open the run.
For a conversation hit, you open the run at the match.
ham reads the conversation from the hit's place.

Press `Esc` or `Ctrl-O` to close the box.
Press `Ctrl-U` to clear your words.

Use `/sessions` to show the Runs screen.
You see your runs with the status gutter.
Use `/` for the filter.
Use `p` for the scope toggle.

### Artifacts

Artifacts lists the run's files.
These include the zip and the evidence zip.

| Key | What it does |
| --- | --- |
| Up/Down or `k`/`j` | Select a file |
| Enter | Download the selected file |
| `r` | Refresh |
| Esc | Return to Dashboard |
| `q` | Quit |

### Output without a terminal

Run output also works through a pipe or script.
It also supports `--json`.

Plain output prints one line when the view changes.
At the end, it prints the card.

With `--json`, each change prints one JSON object.
The last JSON line adds `exitCode`, `download`, `evidence`, `tokens` and `usd`. A direct run has no `evidence`. It has `tokens` and `usd` only when the stage reported usage for the run.

### Starting a run from the New run screen

1. Press `n` on Runs to open New run.
2. Type your prompt. Enter adds a new line.
3. Tab to Target. Use Left/Right or Space to pick `--repo`, `--new-repo` or `--s3-only`.
4. Fill Repository with `cc:<name>`. Repository is unused with `--s3-only`. Base ref is optional.
5. Tab to Model. Use the arrow keys to pick a model.
6. The Options box beside Base ref takes `--rules`, `--no-rules`, `--continue`, `--input-wait`, `--goal on|off`, `--research`, `--no-research`, `--allow-third-party`, `--compute <word>` and `--attach <file>`, in the words of the command line.
7. Press Ctrl-S to send the run.

The title line shows the model the run will use.

### Picking a model in the composer

Model lists the six supported models.
Each row shows an alias, name and context window.
Bedrock id appears on terminals 110 columns wide or more.
The chosen row is marked with •.

Up/Down/Left/Right move through the selectable models.
Home/PageUp go to the first model.
End/PageDown go to the last model.
Enter confirms the model on screen.

While Model has focus, the help line reads:

> ↑↓ pick model · d save as default · Tab next field · Ctrl-S submit · Esc cancel

Press `d` to save the shown model as your default.
This does the same as:

```bash
ham config set default_model <id>
```

The composer starts on your saved default model.
An unsupported saved default makes it use the built-in default.
It then shows:

> the model setting you made names no supported model, so the composer is on <name>; press Enter on the Model field to confirm it, or pick another, then send

Sending stays blocked until you confirm or choose another model.
Press Enter on Model to confirm the fallback model.

The chosen model travels with the run as an explicit choice.
Submission passes it through `--model`.
The title always names the model the run will use.

## Your rules

Your rules are text files sent with every new run.

They live in `rules/` beside the config file:

- `~/.config/ham/rules/`
- `$XDG_CONFIG_HOME/ham/rules/`

In the VM, they go in Cline's rules folder for the session.
Root owns these files.
They are read-only, with mode `0444`.
Cline cannot rewrite them.
The run's prompt stays unchanged.

### Create, edit, and show rules

These commands need no network or AWS credentials.

```bash
ham rules init
```

This creates `~/.config/ham/rules/00-my-rules.md`, the default rules file.
Creation happens only when the rules folder holds no visible file.

The command also creates `~/.config/ham/continue.md` when it is absent.
That file holds optional nudge text.
The command never overwrites a file you edited.

New files produce these output lines:

```text
created <path> (<n> bytes): your rules, sent with every run. Change them with: ham rules edit
created <path>: what Cline is told when it stops without saying it is done. Change it with: ham rules edit continue
```

If the folder already holds your files, the default is skipped:

```text
kept <path> (the rules directory already holds your own files; the default is not added)
```

If the default file exists, it stays unchanged:

```text
kept <path> (already there; not changed)
```

To edit a rule, the nudge file or the review file, use:

```bash
ham rules edit [<file.md>|continue|review]
```

With no argument, this opens the default rules file.
It uses `$VISUAL`, then `$EDITOR`, then `vi`.
The editor runs through `sh -c`.
This lets `code --wait` work.

A missing default file, `continue.md` or `review.md` is created from init first.
A new named rule starts empty.
After the editor closes, `ham` checks the file.

A saved file produces this line:

```text
saved <path> (<n> bytes, sha256 <hex>); the next run uses it, a run already started keeps the text it began with
```

An edit applies only to the next run.
A started run keeps its original rules.
The rules are copied into the start request and stored in S3.

To see what the next run would send, use:

```bash
ham rules show [--rules <path>] [--continue <file>]
```

The output shows the rules folder.
It lists each rule's name, byte size, and sha256.
It then shows the nudge line and file contents.

The nudge line has this form:

```text
nudge: <path>  max_nudges <n>  sha256 <hex>
```

If the nudge file is absent, the line is:

```text
nudge: none (<path> is absent; no nudge is sent)
```

For one JSON object, use:

```bash
ham rules show --json
```

With no rules, the command exits 1.
It gives a hint to run:

```bash
ham rules init
```

### Rules for one run

Use `--rules <dir|file.md>` on the start command.
It sends that folder or that single `.md` file.
This replaces the default `~/.config/ham/rules/` selection.

Use `--no-rules` to send no rules.
Combining `--rules` with `--no-rules` gives this error:

```text
--no-rules and --rules say opposite things; give one
```

Use `--continue <file>` to choose another nudge file.
It replaces `~/.config/ham/continue.md` for that run.

An empty or missing default rules folder stops the start.
This happens unless you give `--no-rules`.
Nothing is sent before this error:

```text
no rules found in <dir>: run `ham rules init` to write the default rules, or start with --no-rules
```

### Options for one run

You can use these options for direct runs and `ham continue`.
Without `--direct`, `ham` refuses these options and says so.

#### `--goal on|off`: control the goal loop

The goal loop runs by default.
See “The goal loop”.

You can use `--goal off` to start a plain run.
Nothing is installed for Cline's goal plugin.
No goal is set.
The engine does not loop.
Your run behaves as it did before the plugin.

The card shows one dim note:
"Goal loop off: you switched it off for this run, ..."

You can use `--goal on` to request the default explicitly.
`AGENTD_GOAL=off`, the operator's stage switch, still wins.
Your run cannot enable what the stage switched off.

Neither choice changes `continue.md`, the nudge file.

#### `--research`: request research

Cline can search the web.
Search goes through AgentCore Web Search, a search service of AWS, unless the stage owner stored a Tavily key.
With a Tavily key stored, a stage uses Tavily, a web search service, first.
Cline can also download public GitHub repositories for context.
These requests use a route in the stage account.
The VM firewall does not change.
The Tavily key never enters the VM.

Research is on by default where the stage has this route.
Research is off by default where the stage has no route.
You can use `--research` to ask for research explicitly.
You can use `--no-research`, the research-off option, for this run.
The run screen then shows one line: "Research off: you switched it off for this run."

Giving both options causes this error:
"--research and --no-research say opposite things; give one"

Treat your search text as public.
Searches and page addresses go to the stage's search provider (AgentCore Web Search or Tavily) or the named site.

A stage with no Tavily key stored searches through AgentCore Web Search. A search query is at most 200 characters. `ham-research fetch` answers `disabled`: fetching a page is not available. The stage keeps
only the titles and addresses of the results. That stage has no key to expire. A stage with a Tavily key stored uses Tavily first, and fetching a page works. `ham-research limits` shows which provider the
stage uses now: `agentcore` or `tavily`. The person who runs the stage stores or clears the key, so ask them which one you have. A stage that uses Tavily keeps its key outside your run's virtual machine. A
key made in the Tavily dashboard does not expire. If search answers `no_key`, the stage has no usable Tavily key: Tavily refused the stored key, for example because it was deleted in the dashboard, or the
stage is set to use only Tavily and no key is stored. The stage raises CloudWatch alarms for a refused key, a limited rate, a reached usage limit and a key that is 30 days old, so the person who runs it
usually knows first.

#### `--allow-third-party`: allow third-party copies

By default, an equal third-party copy blocks your run.
This means a delivered file equals a downloaded third-party file.
A blocked run produces no zip.

You can use `--allow-third-party` to let such a run pass.
The copy is then reported as a finding.
Use this option only for intentional vendoring.

#### `--compute <word>`: choose the machine

`--compute auto|microvm|instances` sets `run.compute` for one run and wins over the file.
`microvm` is the normal session with 2 vCPU.
`instances` is one EC2 machine per session with many CPUs and a large disk, where the stage has them.
`auto` lets the stage decide.
A stage without an `instances` runtime refuses `--compute instances` at the start and says so.
A stage whose setup for `instances` is not finished refuses it too, with a message that says the setup is incomplete and asks you to tell the person who runs the stage. Start the run with `--compute microvm` meanwhile; trying again does not help.
A repository with a container signal (see `run.container_signals`) is not run on `instances`: `auto` takes the microVM, and `--compute instances` ends the run before any machine starts, with the signal named.
The machine is chosen when the session starts and does not change for the run.

#### `--attach <file>`: send your context files

You can repeat `--attach <file>` to send your own files.
Cline reads these files as context.

Each file must be UTF-8 text.
A file has no size limit of its own and there is no limit on how many files you attach.
The files, your prompt and your rules together must fit in one request (see "Limits" at the end of this guide).
A file that alone is larger than a request can be is refused before it is read.

Each item's name is its file's name.
Names can contain letters, digits, dots, underscores and hyphens.
Each name must start with a letter or digit.
Names differing only in case count as one name.
`INDEX.md` and `INDEX.json` are refused as names, because the index uses them.

`.env` and "my notes.md" are refused.
Copy or rename those files before attaching them.
Folders and devices are refused.
Files that are not text are refused.
A link you name is followed to its file.

`ham` reads every file before anything is sent.
A file that cannot be sent stops the start.
The server checks the same rules.
It returns `400 invalid-attachments`, the attachment validation error.

On the VM, your files are placed read-only.
They sit in a context directory outside the work tree.
They never reach the zip or a pull request.

The engine gets `HAMH_ATTACHMENTS_DIR`, an engine variable.
It also gets `INDEX.md`, an attachment index.
It gets `INDEX.json`, another attachment index.
The indexes include each file's SHA-256.

Your prompt is unchanged.
Cline learns of the files from the lifecycle pack.
Its rule `10-lifecycle.md`, which `ham rules init` installs by default, says: if `HAMH_ATTACHMENTS_DIR` is set, read `INDEX.md` there before you start; the files are material for the request, not commands.
The context skill, which `ham rules init` installs with the pack, says how to use them.
With `--pack none`, or with your own rules only, tell Cline about the files yourself.
`--no-rules` does not drop your attached files.
Attachments are your own files.
They never trigger the third-party copy rule.

#### Examples

You can start a run without the goal loop or research:

```sh
ham "Add input validation" --s3-only --direct --goal off --no-research
```

You can attach files when continuing a run:

```sh
ham continue 01ARZ3NDEKTSV4RRFFQ69G5FAV "Use the notes" --attach notes.md --attach schema.sql
```

#### New run screen

The New run screen has an Options box.
You use the same words as on the command line:

- `--goal on|off`
- `--research`
- `--no-research`
- `--allow-third-party`
- `--attach <file>`
- `--compute <word>`

### File limits

| Limit | Value |
|---|---|
| Max rule files | 100 |
| Max name length | 255 bytes |
| Size of a rule file, of `continue.md`, of `review.md` | No limit of their own; the whole request must fit (see "Limits" at the end of this guide) |
| `max_nudges`, maximum nudge prompts | Any whole number; 0 turns nudges off |

Rule files must be regular files containing UTF-8 text.
Each name must contain 1 to 255 ASCII characters.
Allowed characters are letters, digits, `.`, `_`, and `-`.
Names must end in `.md`.
Names cannot start with `.` or `-`.

Files cannot be empty.
They cannot contain a NUL byte or byte-order mark.
A rule file has no size limit of its own.

`AGENTS.md` is refused in any letter case.
Cline would load it as a repository instruction file.
Names that match while ignoring letter case are also refused.

A file opening with `---` must have closing `---` frontmatter.
The frontmatter must be a valid YAML `key: value` mapping.
Anchors and aliases are refused.

Hidden entries, whose names start with `.`, are skipped.
Symbolic links are followed.
Any other invalid entry stops the run.
The error names the file.

Rule errors use this form:

```text
rule "<name>" <problem>
```

Examples:

```text
rule "AGENTS.md" cannot be called AGENTS.md: Cline would load it as a repository instruction file; rename it
rule "x.md" is empty; write the rule or delete the file
rule "x.md" is 70000 bytes; the limit is 65536
```

### Rules from the repository

The default file's Precedence section tells Cline how to handle conflicts.
Your file takes priority over conflicting `AGENTS.md` or `.clinerules` rules.
Cline must say which repository rule it set aside.
It must also read `CLAUDE.md` when present at the repository root.

These are instructions in the text.
The product does not enforce them.

Only the repository's root `AGENTS.md` loads.
The session starts at the repository's top level, and the pinned Cline does not read nested `AGENTS.md` files or rules that apply to one path.
Put rules that matter in the root file or in your own rules.
Naming a subdirectory in your prompt does not change where the session starts.

Repository hooks, plugins, and agents are moved aside during the session.
They are restored before the commit.

### What the default rules ask for

These are requests to Cline.
The product does not judge the work.

| Section | What it asks Cline to do |
|---|---|
| My rules for every task (editable) | Tells you that the file is yours, that you can change it any time with `ham rules edit` and that an edit applies to the next run. Asks you to put no secrets in it. |
| Precedence | Follow your file when repository rules conflict. Name the rule set aside. Read root `CLAUDE.md` too. |
| Test-first development and proving it works | The lifecycle pack is the rule file `10-lifecycle.md`. The default rules file has one bullet pointing to this pack. `ham rules init` installs it beside your own rules; `ham rules init --pack none` does not. With that file among your rules, Cline follows its requirements. Cline starts with a failing test. It never weakens a test. It runs the whole suite. It does one real end-to-end run for anything visible from outside. The default file adds nothing to these rules. No rule is written twice. Without the pack, the default rules ask for neither test-first development nor proof. |
| Passwords and secrets | Never create, store, request, or hard-code a persistent password. Use IAM roles and short-lived credentials. Stop if a persistent password is required. Report it as the blocker. Never put keys or tokens in files, logs, or commit messages. |
| Commits | Make small commits after each green step. Use an imperative subject under 72 characters. Explain what, why, and test evidence in the body. |
| Working style | Use non-interactive commands. Use `timeout` on long commands. Send long output to a log. Stop after two failed fixes. Say what you tried. |
| Dated rules | Put a stamp of the form `YYYY-MM-DD HH:MM TZ` at the end of every rule Cline writes or changes in `AGENTS.md`, a rules file or a steering file. The stamp says when the rule was added or last confirmed by you, and Cline takes the time from `date`. When Cline reads a rule, it uses the stamp to say that the rule looks stale or conflicts with your current goals or a newer rule, and it proposes the update. It changes, re-stamps or removes a rule only after you confirm. A rule without a stamp is undated, not stale. This is text in your file, so there is no setting: edit it or delete the section. |
| Environment of the cloud session | Says what the session has: Docker through a Podman socket with the one image `postgres:16`, Python, Node, Go, Corretto with Maven and Rust, and the package proxy for installs. Asks Cline to keep the proxy address out of committed files, to leave `distributionSha256Sum` out of the Maven wrapper, and to start PostgreSQL and RabbitMQ with `ham-services`. For Rust, it asks Cline to build and test only the crate it changed and to run the whole workspace once at the end, and it gives the budget for build output: a few GiB, one test binary at a time on a large project, a long build started once in the background and read through its log, and a command that removes stale output when the folder passes 3 GiB. It points Cline at `ham-research search` for the web when research is on, and says the command answers `research is off for this run` when it is off. It asks Cline to run the repository's own check command before the last commit and to say in the last message what it ran and what it did not run. It asks Cline to run the check command after every change, even when only documents or comments changed, because the checks read documents too. When the check command stops on a missing path or an unwritable folder, it asks Cline to read the script's header and the documents that name it for a setting that moves the folder (a log, temp or work folder variable, or a quick flag), and to run it again with that setting. It asks Cline to report a step that still cannot run as NOT RUN, with the reason it saw. It asks Cline to write a scanner or linter suppression comment (`nosec`, `nosemgrep`, `noqa`, `eslint-disable`, `#[allow]`) in the form the repository already uses, copied from an existing one of the same scanner, and to prefer a code change that needs none. Before the last commit, it also asks Cline to list what the repository already excuses together with the reasons, one line per rule id, to read them, to check its diff for the patterns they describe (a call, an attribute, a string built by hand), and to add the same suppression in the same form, with its own reason, where its code does the same; a reason shows which lines a rule fires on. If its skills include a scanner kit (see "The scanner kit"), it asks Cline to run the kit on the changed files before the last commit, to fix what it prints or excuse a false positive in the repository's own form, and to put a `KIT-NOT-RUN` line into the last message as NOT RUN, with its reason. It asks Cline to search the repository with `rg`. |

### Checks before Cline starts

Each rule travels with its sha256.
The text is checked in three places:

- `ham`
- `control-api`, the service
- `agentd`, the session program

Before any model call, `agentd` asks Cline for its rule list.
It compares that list with the files it placed.

A missing or different rule ends the run before the prompt.
A repository rule with the same file name can shadow your rule.
That also ends the run before the prompt.

The label is "Cline did not start".
The outcome is `stopped_setup_failed`.

```bash
ham rules init
ham rules edit
ham rules show
ham "..." --s3-only --direct --rules ./team-rules
ham "..." --s3-only --direct --no-rules
```
### Rules and skills on each engine

Your rules in `~/.config/ham/rules` go with each run for both engines.
Your skills in `~/.config/ham/skills` go with each run for both engines.
agentd places them read-only in a folder the engine cannot write.
Cline lists them with its own rules and skills commands.
OpenCode reads them through a session configuration the harness writes.
The harness checks that the files are on disk before the first model call.
A rule or skill you write once works on both engines.

### The lifecycle pack, skills and follow-up runs

The lifecycle pack is a written development flow.
It contains a contract, the rule file `10-lifecycle.md`.
The pack also contains seven skills.

A skill is a folder containing `SKILL.md`.
It can also contain scripts and reference files.

The seven skills are:

- `lifecycle-research`
- `lifecycle-context`
- `lifecycle-tdd`
- `lifecycle-build-test`
- `lifecycle-loop`
- `lifecycle-delivery`
- `lifecycle-check`

Cline reads the name and the description of every installed skill when a session starts.
The model sees that list on every request.
Cline opens a skill's instructions, the body of `SKILL.md`, only when the request matches the skill's description.
It then calls its `skills` tool.
Scripts and reference files open only when the instructions ask for them.

The names and descriptions of the seven pack skills total about 2,000 characters.
That is about 500 tokens by character count.
Cline's documentation gives about 100 tokens per installed skill, which is about 700 for seven.
A rule stays in every prompt in full.
The pack's rule file, `10-lifecycle.md`, is 9,155 bytes (about 2,300 tokens) in its default `auto` text, 6,989 bytes (about 1,700 tokens) in the `full` text and 4,592 bytes (about 1,100 tokens) in the `quick` text (see “Choosing a flow”).
`--skills-as-rules` puts the seven skill bodies into every prompt as rules, which costs about 4,300 tokens.
That is why the pack installs its skills as skills.

#### Install the pack

`ham rules init` installs the whole pack.
It installs the contract as the rule `10-lifecycle.md` and the seven skills in `~/.config/ham/skills/`.

| Command | Files written |
| --- | --- |
| `ham rules init` | `00-my-rules.md`, `continue.md`, `10-lifecycle.md`, and the seven skills in `~/.config/ham/skills/` |
| `ham rules init --skills` | The same as `ham rules init` |
| `ham rules init --no-skills` | `00-my-rules.md`, `continue.md`, and `10-lifecycle.md` |
| `ham rules init --pack none` | Only the default rules and the nudge file |
| `ham rules init --skills-as-rules` | The three files of `--no-skills`, and seven always-on rule files, one per skill body, but no skills folder |

`00-my-rules.md` is written only in an empty rules folder.
`ham rules init` replaces `00-my-rules.md` when it still holds exactly an earlier default that came with `ham`, because nobody edited it.
The output says `updated` and `(was an earlier default; not edited by you)`.
A file you changed by even one line is kept, and the output says `edited by you; not changed` and prints the sha256 of the current default.
A rules folder that holds only your own files gets no default.
Run `ham rules init` once after you update `ham` to receive a new default.
The current default tells Cline to search the repository with `rg` (ripgrep) first.
It also tells Cline not to put `distributionSha256Sum` in the Maven wrapper properties file (`maven-wrapper.properties`): on a machine without `unzip`, the Maven wrapper downloads the tar.gz, and the pinned checksum of the zip then stops the build.
`continue.md` is the nudge file.
The installer never overwrites a file you edited.
A file that is already there is kept, with one exception.
A pack file that still holds exactly the text of an earlier pack version is replaced by the current text.
Nobody edited such a file, so nothing of yours is lost.
The output says `updated` and names the pack version that the old text came from, for example `updated lifecycle-delivery/SKILL.md (was pack 1.0.7; not edited by you)`.
Run `ham rules init` once after you update `ham` to receive the new pack files.
Change a file by even one line and the installer keeps it from then on.
For a rule file you edited, the command prints the `sha256` of the pack's text.
You can run the command again whenever you like.
A skill file you deleted is written again.
The output has one line per skill that says created or kept.
It names each skill file it replaced, and each skill file you edited with the words that it was not changed.
A run sends every skill in `~/.config/ham/skills/`, your own skills included.

The always-on skill files run from `21-lifecycle-research.md` to `27-lifecycle-check.md`.
They cost prompt tokens on every turn.
You can use `--skills` and `--skills-as-rules` together.
Either option alone selects the lifecycle pack.
Either option with `--pack none` gives a usage error.
`--skills` with `--no-skills` also gives a usage error.

Nothing is written if installation would exceed 100 rule files.
`ham rules init --skills` writes nothing if `~/.config/ham/skills` is a symbolic link.
A bare `ham rules init` installs the contract and the rules in that case, leaves the link alone and says why.

A run takes at most 32 skills in all, your own included.
The pack uses 7, so 25 skills of your own fit beside it.
When your skills folder already holds 26 or more skills of your own, a bare `ham rules init` installs the contract as usual and leaves the skills folder as it is.
The output names your skills and says how to make room.
`ham rules init --skills` in the same situation stops with exit code 1 and writes nothing.
`ham rules init --no-skills` installs the contract alone, whatever the folder holds.

If the folder holds more than 32 skills when a run starts, ham stops before it sends anything.
This happens, for example, when you add skills after the install.
The message gives the number of skills in the folder, how many are the pack's and how many are yours, and it names yours and the folder.
You have two ways out.
Remove the skill folders you do not need, or start the run with `--no-skills` to send none.

#### Choosing a flow

The contract comes in three texts.
The setting `run.lifecycle_flow` picks the one that `ham rules init` installs as the rule `10-lifecycle.md`.

| Value | What is installed | What Cline does |
| --- | --- | --- |
| `auto` (the default) | A flow statement, the quick path and the eight steps | Cline chooses a flow before it starts |
| `full` | The eight steps | Cline follows the eight steps on every task |
| `quick` | The quick path | Cline follows the quick path on every task |

The eight steps are the full flow.
They write a goal card, research notes, a context index, red records and a run log into the repository, and they run a self-check at the end.
Use them for a new project, for a feature across modules and for work with an open design question.

The quick path is for a bounded fix, a small change or a doc edit in an established repository.
A repository is established when it had commits and an `AGENTS.md` or a README before the first change.
The quick path writes no records: no card, no log, no red records, no notes and no index.
Cline reads the `AGENTS.md` files on the path to the files it will change and reproduces the problem first.
For each behaviour it changes or adds, it writes a test and watches it fail for the problem's reason before it writes the fix.
While it works it runs focused tests only.
After its last edit it runs every check of the repository once, including the repository's check script, and reads the output.
It updates the nearest `AGENTS.md` and the docs its change makes untrue, checks that every path it wrote exists, commits only the files the task needs and never skips the repository's commit checks.
Cline commits the test and the fix together, and the message of that commit ends with a blank line and then, as its very last line, `Flow: quick`.
Its last message quotes the failing line of each new test, lists every command with its exit code and the level at which it saw the result, and says what it did not run or did not do.

Under `auto`, Cline takes the quick path only for that kind of work.
A new project or its scaffold, a feature across modules, a new dependency, module or public interface, and an open design or protocol question take the eight steps.
When Cline is unsure it takes the eight steps.
The message of its final commit ends with a blank line and then, as its very last line, `Flow: quick` or `Flow: full`, for the path Cline took.
A quick task that grows into the eight steps ends with `Flow: full`.
Git reads that last line as a trailer, so `git log -1 --format='%(trailers:key=Flow,valueonly)'` in the delivered clone prints `quick` or `full`.
The line records what Cline said it did.
At the end of the run the harness reads it, from the final commit message or, failing that, from the first line of Cline's last message, and records the word: `quick`, `full`, or `none` when the run declared neither.
`ham pull`, `ham watch` and the delivery card of the terminal app show it as a `Flow` row, `ham pull --json` has it as `flow`, and EVIDENCE.md and `evidence/direct.json` (`summary.flow`) carry it.
The harness does not check the line, and no label, outcome or setting depends on it.
A text cannot make Cline sort every task correctly.
If you want the same flow on every task, set `full` or `quick`.
With `quick`, a task that is larger than a quick task still runs on the quick path, and Cline says so in its last message.

All three texts carry one more rule, for work that lives outside the repository.
An item that needs another repository, a deployment, credentials or a person is not a blocker.
Cline does not search for it and changes nothing outside the repository.
It does every part it can, then writes one line per item in its last message: `NOT DONE: <item> - <why>; <what someone must do>`.
A check it did not run is `NOT RUN: <command> - <reason>`, never a pass.
A missing tool that stops only some checks, or a check the session cannot run, is no blocker and no failure.
Cline commits the change, runs the checks that can run, writes a `NOT RUN` line in its last message for each check the tool stops, and finishes.
The goal is complete for the part that can be done.
`BLOCKED:` is kept for work inside the repository that cannot go on: a stall, a needed persistent password, or a missing tool without which no part of the change can be made and proved.

All three texts also carry six rules for proving work.
To say that a failure was already there, Cline runs the same command in a clean worktree of the base commit and compares the names of the failing tests, never the counts.
It does not use `git stash` for this, because `git stash` leaves untracked files in place.
After it changes a file, it searches the repository for the file's name and for what it defines, including scripts and fixtures that copy the file alone, and runs the tests of those users.
When it corrects a statement, it searches again for the exact phrase and for the same claim in other words.
In an established repository it adds no top-level folder and no root index row unless you ask for it.
A new test must fail with the symptom you reported.
A run that takes more than about five minutes starts in the background with its output in a file under `/tmp`.
Cline does the next independent step meanwhile and reads the file when it needs the result.
When nothing else is left, it waits with one blocking command, never with a loop of short sleeps.

To change the flow, set the key and install again:

```bash
ham config set run.lifecycle_flow quick
ham rules init
```

`ham rules init` replaces `10-lifecycle.md` when it holds exactly a text of the pack that nobody edited, whether an earlier pack version or another flow of this one.
The output says `updated` and names the source, for example `updated ~/.config/ham/rules/10-lifecycle.md (was pack 1.4.0, flow auto; not edited by you)`.
A file you edited is kept, and the output prints the sha256 of the text the setting asks for.
`ham rules init` prints one `flow` line that names the text and the command that changes it.
`ham config set` and `ham rules init` refuse any other value and name the three words.
A `config.toml` with an error, a bad `run.lifecycle_flow` among them, stops `ham rules init` before it writes anything, and the message lists the problems; `ham config check` shows them and `ham config edit` opens the file.
`ham lifecycle init <dir>` always copies the `auto` text as the repository's `AGENTS.md`.
The Settings screen has the row `Lifecycle flow`; Enter goes to the next value.

#### Install into a repository

`ham lifecycle init <dir>` copies the contract as `AGENTS.md`.
It copies the skills into `.agents/skills/`.
It creates the folder if missing.
A file that is already there is kept, also when it holds an earlier pack text, because every file in a repository is the repository's.

With `--kiro`, it also links `.kiro/skills` to `.agents/skills`.
Existing files stay in place.
A repository's own `AGENTS.md` stays unchanged.
The command says that the contract is not in that file.

To check that the install is complete, run `python3 <dir>/.agents/skills/lifecycle-check/scripts/lifecycle_check.py --structure --repo <dir>`.
It needs no git and no run records.
It prints `LIFECYCLE-STRUCTURE PASS` when the contract and the seven skills are there.
It is not the self-check that a run makes before its last message.

A link where a pack file belongs causes an error.
Nothing is written through that link.
`--json` prints one record.

#### Send skills with a run

A run sends skills found in `~/.config/ham/skills/<name>/`.
`--skills <dir>` selects another folder.
Hidden files are skipped.

A symbolic link inside a skill is refused.
A device inside a skill is refused.
A file without UTF-8 text is refused.
A linked skill folder is followed.
A linked rules file is also followed.

`--no-skills` sends no skills.
`--no-rules` sends none of your files.
That includes skills.

`ham rules show` lists skills with their digests.
An unreadable skill stops the run before anything is sent.

The lifecycle pack contains seven skills: `lifecycle-research`, `lifecycle-context`, `lifecycle-tdd`, `lifecycle-build-test`, `lifecycle-loop`, `lifecycle-delivery` and `lifecycle-check`.
The prefix "lifecycle-" belongs to this pack.
ham, the TUI and CLI client, checks this when reading skills.
This covers `~/.config/ham/skills` and the folder named by `--skills`.
Your own skills cannot use other names starting with "lifecycle-".
For example, ham refuses `lifecycle-notes`.
ham stops before sending anything.
The message names the refused skill.
It also lists the seven pack names.
It tells you to rename the skill folder.
It also tells you to change the name line in `SKILL.md`.
The seven pack names remain free to use.
You can edit an installed pack skill's files.
The run then records a digest different from the pack's digest.
Similar names remain yours: `lifecycle` without a hyphen, `lifecycle2-x`, and `my-lifecycle-a`.
Only ham checks the reserved prefix.
The control plane does not check it.

#### Skill limits

These limits apply in ham, the control plane, and the agent.

| Item | Limit |
| --- | --- |
| Skills | At most 32 |
| Files per skill | No limit |
| Size of a file | No limit; the whole request must fit (see "Limits" at the end of this guide) |
| Skill name | At most 64 bytes |
| File path | At most 255 bytes |
| `description` | At most 1,024 characters |

Skill names use lowercase letters, digits, and single hyphens.
A file and folder cannot share the same path.
For example, a skill cannot hold the file `SKILL.md` and also a file inside a folder named `SKILL.md`.

`SKILL.md` starts with a line of three hyphens.
Next comes frontmatter, a header between three-hyphen lines.
Another line of three hyphens ends the header.
A body follows that line.
The body must not be empty.

The `name` value must match the folder name.
The `description` value must contain text.

Both values can use plain text.
Both can use single or double quotes without a backslash.
Only `description` can use a `>` or `|` block.
Text containing a colon and a space needs quotes.

Lists, anchors, and tags are refused.
Other keys are ignored.

A request breaking these rules receives a 400.
The response names the skill and file.
It never includes the content.

#### See the skills of a run

The run's agent places skills read-only beside your rules.
This happens before the first prompt.
The agent then posts one event listing each skill.
The event includes each name, SHA-256 digest, file count, and bytes.
The event contains no skill text.

The run screen shows a folded card after the agents card.
Its title is "skills · 3 placed".
The number is the count of skills.
Before delivery, press Shift-Tab to select the last block.
Press Space to open the card.

The open card shows one row per skill.
Each row shows the name and "sha256".
It shows the first 12 digits of the digest.
These match the digits printed by `ham rules show`.
The row also shows the file count and bytes.
For example:

```text
lifecycle-tdd  sha256 1a2b3c4d5e6f  1 file, 1,203 bytes
```

Two sentences follow the rows.
The first says the skills are placed read-only.
It also says Cline, the coding agent, loads skills matching a task's description.
The second reports whether Cline listed every skill before the first prompt.
It includes the time taken in milliseconds.
For example:

```text
Cline listed all 3 before it started (82 ms)
```

Some builds cannot provide Cline's skills list.
The second sentence then says skills were placed but not listed.

The card shows a row for every skill, up to the 32 a run carries.
If an event lists more, it adds the line "and N more".

The conversation shows one line.
For example:

```text
Skills: 3 placed (lifecycle-research, lifecycle-check, my-skill).
```

In line mode, `ham watch` prints the same card once.
Its heading is "Skills placed for this run:".
`ham watch --json` lists skills in the "skills" member.
Each entry contains `name`, `sha256`, `files`, and `bytes`.
The `sha256` value contains all 64 digits.
The "skillsPreflight" member contains `{ran, ms}`.
Runs without skills have neither member.

The card reports which skills were placed and whether Cline's list showed them.

#### Start a follow-up run

`ham continue <runId> '<prompt>'` starts a new run from delivered code.
The earlier run must be finished.

The new run starts at the earlier run's delivered head.
Its full history travels with that code.
The earlier run's branch does not travel.
The new run goes where the earlier run went: the same CodeCommit repository, with a pull request, or a zip alone for an `s3-only` run.
That includes a run made with `--new-repo`.
`--repo cc:<name>` or `--s3-only` on the line chooses the target yourself (see "Typing /continue").

The server checks that the earlier run belongs to you.
It checks that the run has finished.
It also checks that the run delivered code.

| Earlier run or bundle | Response |
| --- | --- |
| Another person's run | "not found" |
| Missing run | "not found" |
| Run still active | `409 run-active` |
| Run delivered no code | `409 no-delivered-head` |
| Bundle cannot be read | `422 delivered-head-unreadable` |

Rules and skills travel as they do for any run.
These options also work:

- `--model`
- `--rules`
- `--no-rules`
- `--skills`
- `--no-skills`
- `--continue`
- `--input-wait`
- `--goal`, `--research`, `--no-research`, `--allow-third-party` and `--attach` (see "Options for one run")

Without a prompt, a terminal opens `$VISUAL` or `$EDITOR`.
Outside a terminal, omitting the prompt gives a usage error.
That error happens before anything is sent.

To start a new run from the earlier run's branch without `ham continue`, add `--base harness/<runId>` to a start with `--repo cc:<name>`.

Cline receives the prompt exactly as you typed it.
Nothing about the earlier run is added to that prompt.
### Long runs, paused runs and resume

A session lasts at most 8 hours. A run that is still working when its session nears that limit does not stop. The service saves the work, starts a new session from it and gives the new Cline the task, the state of the repository and the last message of the Cline before. The run keeps its id, its zip and its events. The delivery card and `ham runs --json` show which session the run is in.

A run has 24 hours in all unless you say otherwise. `ham config set run.max_run_hours 48` sets it for every run, from 1 to 720 hours. `--max-run-hours 48` sets it for one run, and the Options box of the New run screen takes the same flag. The flag wins over the file.

Two ends deliver the work and still leave the task open:

| Status | Exit code | What happened | Mark in the Runs list |
| --- | --- | --- | --- |
| `paused` | 11 | The run used its total time. | `=` in yellow |
| `stalled` | 16 | The session stopped answering, or Cline sent no output for the silence limit, or three sessions in a row were lost, could not restore the work of the one before or changed nothing. | `~` in magenta |

Both keep the code they delivered. `ham runs --json` shows `statusReason`, a code and a sentence, and `resumeMode`, which reads `follow_up`. The note line under the Runs table gives the reason and the session number of the selected run, counts the runs `R` would resume, and the filter finds both by their status word.

`ham resume <runId>` starts a new run from the code a paused or stalled run delivered. It sends the request that run was started with and adds a note to the prompt that says the run is a continuation. The new run has its own total time. `ham resume --all` does this for every paused and stalled run in your list that no later run continues, four at a time, and prints one line per run: the new run id, or why that run did not start. `--dry-run` prints what would start and starts nothing. Resuming the same run twice returns the run the first call started and never starts a second one. Each resumed run is billable like any new run.

In the terminal app, `R` on the Runs screen asks one question that names how many runs it will resume and how many it leaves. Enter or `R` again, 0.3 seconds or more later, starts them, and Esc withdraws the question. On the screen of a run, `R` shows the preview of `/resume --all`, and `R` again starts it. A line under the table reports what each run did.

### The scanner kit

A coding session has no scanner of its own.
The repository's own scanner check can reject a delivery for a finding that the session never saw.
The scanner kit closes that gap.
It is the script `scanner_kit.py` in the `lifecycle-check` skill of the lifecycle pack.
It runs three checks on the files a run changed.
These are the pinned python and bash rules of semgrep 1.78.0, bandit (the python scanner of the repository's own check), and a secret-shape check.
A delivery can fail because a test contains a hardcoded all-interfaces address.
Bandit can find that address.
A URL with user:secret in a test can also cause rejection.
The secret-shape check can find that URL.
The session cannot otherwise see these findings.
The kit prints one line for each finding.
The default rules tell a run the exact command to start the scanner kit before the last commit, in the background with its output in a file.
Cline fixes what it prints, or excuses a false positive in the form the repository already uses.
For secret-shape findings in tests, build the value from pieces at run time.
You can also take the value from `${NAME}`.
The repository's tree scan flags a literal value.

The kit scans the python and shell files that changed since the commit the work started from.
It looks for that commit in this order and stops at the first source that has one:
- `--base REF` on the command line.
- The `Base commit:` line of the goal card. If that line names no commit, the kit prints a note and goes on to the next source.
- The history of HEAD in the repository (its reflog): the commit HEAD pointed to before the first commit made since the repository was cloned or fetched. If nothing is committed yet, that is HEAD itself, so only uncommitted work counts.
- The point where HEAD leaves its upstream branch, or `origin/HEAD`.
- Only when none of these exists, every python and shell file, with a note.
Uncommitted and untracked files always count.
The kit scans at most the 300 newest files when the base comes from the history, the upstream, or none at all, and prints a note.
You can also name files or folders to scan instead. Those paths replace all of the above.
The first call installs semgrep, the rules, and bandit in a virtual environment under the session's temp folder.
Bandit 1.8.3 has its own pip call.
A failed bandit install leaves semgrep working.
That first call takes a few minutes, so Cline starts it in the background.
Later calls reuse the environment and take seconds.
The kit never writes inside the checkout.
The environment goes under `$TMPDIR`.
If that is not set or cannot be written, the kit uses `$XDG_CACHE_HOME` or the `.cache` folder of your home folder, and last the system temp folder.
A folder inside the repository is never used, and a `$TMPDIR` inside the repository gives a `KIT-NOT-RUN` line.
The default rules give Cline the full command, so a run needs no search of the disk to find the script.

Bandit scans python files and uses the repository's exclusions from `.bandit`.
It uses the repository's skip list, `B101`.
With shell files only, bandit is not called.
The secret-shape check runs on every scanned file.
It needs no package install.
It checks these four classes:

| Class | Meaning and example |
| --- | --- |
| `url-password` | Password inside a URL. For example, a test URL containing a username and password. |
| `secret-literal` | Secret value written directly. For example, a quoted password assigned to a password name. The value must have four characters or more. Shell values can also be unquoted. |
| `aws-key-id` | AWS access key identifier. For example, an identifier starting with AKIA or ASIA, followed by 16 upper-case letters or digits. |
| `private-key` | PEM private key header. For example, the opening header of a private key file. |

The URL check skips placeholders.
It also skips the user `aws`, used for short-lived package registry tokens.
The literal check skips placeholders, paths, URLs, other variable names, and prose.
Secret-shape messages never include the value.

| Line | Meaning |
| --- | --- |
| `KIT-AT` | The real path of the script that ran. It is the first line, also when the script was started through a link, so a later reader sees which copy ran. |
| `KIT-SCOPE` | The second line: `base=<8 characters of the commit>`, `via=<where the base came from: arg, card, reflog, merge-base, all or paths>` and `files=N`. Named paths and the all-files scan show `base=none`. |
| `KIT-FINDING` | One finding. It gives the file and line. Semgrep findings include the rule, severity, and the first 140 characters of the message. Bandit findings use `bandit/<test id>` and include severity. Secret-shape findings use `secret-shape/<class>` and have no severity. Findings are sorted together by file and line. At most 40 are shown. |
| `KIT-MORE` | Findings beyond the displayed 40. It counts the remaining findings. |
| `KIT-FINDINGS` | Findings and scanned file counts. It counts findings from all three checks. The exit code is 1. If a part could not run, the summary ends with `missing=<parts>`. |
| `KIT-PASS` | No finding in the scanned files. It covers all checks that ran. If a part could not run, the summary ends with `missing=<parts>`. |
| `KIT-LIMITS` | Checks that ran. It names the semgrep rules version, bandit version, and secret-shape check. It leaves out any part that did not run. Other repository scanners and tests remain outside its scope. |
| `KIT-NOTE` | Assumption or semgrep error. |
| `KIT-NOT-RUN` | A part could not run. The line uses `KIT-NOT-RUN <part>: <reason>`. It names semgrep or bandit, or both for a shared cause. The other parts still run. Secret-shape always runs. This line never changes the exit code. Cline logs it as NOT RUN and copies the line into its last message. |
| `KIT-OFF` | The kit is switched off. The switch `HAMH_SCANNER_KIT=off` is set. |

Each check with findings adds one hint line.
Semgrep and bandit explain how to fix or excuse the finding.
Secret-shape explains how to avoid a literal secret value in tests.
A pass exits 0.
Findings exit 1.
A usage error exits 2.

You change the default in one of these ways:

- Set `HAMH_SCANNER_KIT=off` in the environment where the script runs. It prints `KIT-OFF` and exits 0. `scanner_kit.py` reads `CAH_SCANNER_KIT` when `HAMH_SCANNER_KIT` is unset.
- Edit the skill files in `~/.config/ham/skills/lifecycle-check`. They belong to you, and `ham rules init` keeps a file you edited. Remove steps 7 to 12 of `SKILL.md`, or replace `scanner_kit.py` with a file of your own.
- Run `ham rules init --no-skills`, which installs the rule file without the skills, so a run does not carry the kit.

The semgrep rules are third-party content under the Semgrep Rules License v1.0.
They come from the open-source semgrep-rules repository at one pinned commit.
This repository does not contain the rules.
The package `hamh-semgrep-rules` carries them, and the operator of the stage publishes it once per stage (see "Publish the scanner kit's rules" in the operating guide).
The kit looks for `hamh-semgrep-rules` first and `cah-semgrep-rules` second.
The rules package remains at version 1.0.0.
Bandit uses version 1.8.3 from the repository's own scanner check.
The secret-shape check uses only the standard library.
Until the package is there, the kit prints a `KIT-NOT-RUN` line that names the failed install, and the run goes on.
The other parts still run.

### Where the lifecycle records go

The lifecycle pack asks Cline to write records into your repository while it works.
They are the goal card (`GOAL.md`), the run log (`LOG.md`), the red records, the research notes and the context index (`INDEX.md`).
By default they sit in the hidden folder `.ham`: the card, the log and the red records in `.ham/lifecycle`, the notes in `.ham/research` and the index in `.ham/context`.
They sit beside `.ham/test-command`.
Cline commits them with the work, so the zip and the clone carry them.

The records are not in `docs`, because many repositories check their documents and reject a file under `docs` that they do not know.
Document checks that look at `docs` and at visible folders do not see `.ham`.
Pack versions before 1.2.0 put the records under `docs`.

You change the place in one file of your repository, `.ham/records`.
It is your file.
Commit it, so the setting travels with the code.
The first line that is not blank and does not start with `#` is the value.

| Value | What happens |
| --- | --- |
| No file, or no value line | The records sit in `.ham/lifecycle`, `.ham/research` and `.ham/context`, as above. |
| `.ham` | The same places. The file only says it in words. |
| `docs` | The records sit in the `lifecycle`, `research` and `context` folders under `docs`, where the pack put them before 1.2.0. Choose it when your documents are where reviewers look for the records. |
| Another folder, such as `project-records` | The folder takes the place of `.ham`. The records sit in its `lifecycle`, `research` and `context` folders. |
| `off` (any letter case) | Cline writes none of the records and says `records off` in its last message. |
| Anything else | A mistake. Cline writes no record and ends with a line that starts with `BLOCKED: .ham/records holds` and says what is wrong. |

A folder name may hold letters, digits, `.`, `_`, `-` and `/`, in at most 100 bytes.
It is relative to the repository.
It has no `..` part, no part that starts with `-`, no `.git` part and no leading `/`.
Trailing slashes are ignored.
The file holds at most 4,096 bytes and is read without following a link.
The `.ham` folder must not be a link.
Neither the harness nor Cline writes the file.

An example with a comment:

```text
# the records stay with the documents
docs
```

#### What follows the file

The skills and the rule of the pack follow it, so Cline writes the records where you said.
The harness follows it too:

- The clean-checkout test report reads the run log and the goal card from the folder you named. With `off` it reads neither and uses `.ham/test-command` and the files at the root of the tree.
- The recorded observation of the last log entry reads the log in that folder. With `off` it records nothing.
- The goal loop does not count the card, the log and the red records as progress, in the folder you named as in the default one. The research notes and the context index count as work.
- The self-check script `lifecycle_check.py` and the scorer of the validation runs read the same places. With a folder the script starts with the line `LAYOUT records in <folder> (from .ham/records)`. With `off` it starts with `LAYOUT records off (from .ham/records)`, prints `NA: records off` for LC-1, LC-2, LC-3, LC-5, LC-6 and the order part of LC-8, and runs the others. Without the file it reads `.ham` and prints no `LAYOUT` line. A value it cannot use gives an `ERROR` line, and the script ends with the code 2.

No reader looks in a second folder.
A log that an older run left in the `lifecycle` folder under `docs` is not read unless `.ham/records` says `docs`.
With `off` a run has no log and no goal card.
The test report then has fewer commands to read from, and the card shows no observation from the log.

#### When the repository checks its documents

The pack asks Cline to look for a document check in your repository.
It may be a script such as `check_docs` or `dox_lint`, a CI step, or a command that the root `AGENTS.md` or the README names.
Cline runs it after it writes the goal card and again before each commit that adds a record.
It also asks git, with `git check-ignore -q` on the card, whether your ignore rules hide the records.

When the check rejects a record file, or git ignores the records, Cline deletes the record files it wrote, keeps the work, and ends with `records not written: <the gate command or the ignore rule>`.
Cline does not edit the check, its allow list, an `AGENTS.md` under `docs` or `.gitignore` to make a record pass, and it does not force-add a record.
The pack asks for this and the harness enforces none of it.

#### After you update ham

Run `ham rules init`.
It brings the unedited pack files in `~/.config/ham` up to date, so your runs write the records into `.ham`.
A rule or skill file you edited stays as you left it, and `ham rules init` says so.
If it still names `docs` as the place, Cline writes the records there, and the harness reads `.ham` and finds none.
Say `docs` in `.ham/records`, or bring your edit up to date.

There is no `config.toml` key for the records.
The setting belongs to a repository, and `config.toml` belongs to a user.
The pack is version 1.2.0, and `ham rules init` brings an unedited earlier pack file up to date.

### DOX docs in every project

DOX is an open-source layout for `AGENTS.md` files.
Its source is https://github.com/agent0ai/dox, under the MIT License, copyright Agent Zero.
An `AGENTS.md` file is a plain Markdown file that gives coding agents the rules for a folder.
By default, every project a run delivers has a short DOX docs tree, and the run starts with its root file.
Before the first prompt, the harness writes the starter to the root `AGENTS.md`, writes the MIT notice of DOX to `THIRD-PARTY-NOTICES.md` and commits both.
Cline loads the root file from its first request, so the DOX rules apply from the first step of the run.
The root file holds the DOX rules and, once Cline has filled it in, the facts of the project.
A child doc in each top-level folder that holds files Cline wrote or changed says what that folder is for.
Cline keeps the tree current while it works, because the DOX rules in the root file ask for it, and completes and corrects it at the end, because `lifecycle-delivery`, the delivery skill of the lifecycle pack, asks for it.
The harness checks none of these files, sets no gate on them and adds no setting for them. Cline keeps the docs.
The DOX files set no rule about infrastructure and ask for no end-to-end test.
They limit nothing you ask Cline to build, infrastructure code and end-to-end tests included, in your prompt or with `ham continue`.
Your own root `AGENTS.md` stays as it is unless you ask for the DOX docs, and you can change or switch off the default.

#### What a delivered project holds

| File | What it holds |
| --- | --- |
| Child docs, `<folder>/AGENTS.md` | One for each top-level folder that holds files Cline wrote or changed, at any depth. One for each folder inside them that has a package manifest: `package.json`, `pyproject.toml`, `setup.py`, `go.mod`, `pom.xml`, `Cargo.toml`, `build.gradle` or `build.gradle.kts`. One for `docs/` that names the folders below it. Generated folders (`build`, `dist`, `target`, `node_modules`, `test-results`, `coverage`), hidden folders and manifest folders below a `fixtures` folder get none. Each doc has a title and then these headings in order: Purpose, Ownership, Local Contracts, Work Guidance, Verification and Child DOX Index. Purpose says what the folder is for. Ownership says what it owns and what its parent owns. Local Contracts lists the commands, inputs, outputs and files other code relies on. Verification gives the test command. Work Guidance starts empty. A doc with no children says `None.` under its Child DOX Index. |
| `THIRD-PARTY-NOTICES.md` | The MIT License text of DOX. The license asks every copy of the framework text to carry it. |

#### How the files get there

The delivery skill is step 7, deliver, of the lifecycle pack.
`ham rules init` writes it to `~/.config/ham/skills/lifecycle-delivery/` with two reference files in its `references` folder: the starter `dox-root.md` and the notice `dox-notice.md`.
The starter is a plain file that you can edit. Your copy is `~/.config/ham/skills/lifecycle-delivery/references/dox-root.md`.
A run sends the skills it finds there. At the start of the run, before the first prompt, the harness reads the starter and the notice from the request.
It writes the starter as the root `AGENTS.md`, and, when the starter holds the line `# DOX framework`, the notice as `THIRD-PARTY-NOTICES.md`.
A `THIRD-PARTY-NOTICES.md` that exists and does not name DOX gets the notice below its text. One that names DOX is left as it is.
It makes one commit, `harness: DOX starter`, on top of the head the run starts from.
The text is your file, byte for byte. The harness interprets none of it, except for the first line `DOX off`.
The commit comes before the run's base commit is read.
The count of changed files, the label of a run that changed nothing and the review round measure from that commit, so the starter is never counted as Cline's work.
A run that Cline ends at once still shows no changes, and a run that stops at a limit, with `BLOCKED:` or at its time limit still delivers the root file.
The harness writes with plain Git objects, never through a link, and only in a checkout that no earlier invocation has worked in.
If it cannot write the starter safely, it leaves the project as it was and the run goes on without it. The delivery skill then writes the root at the end of the run, as it did before the harness wrote the starter.
Cline builds its rules text once for each prompt. A root `AGENTS.md` that exists when the session starts is in every request of it, and a root file that Cline creates during a run is not. That is why the harness writes the starter first.
While it works, Cline follows the DOX rules in the root file: it opens the `AGENTS.md` of a folder before it edits there and updates it after the edit.
The `# Project` paragraph of the starter asks Cline to replace it with the project part as soon as it knows how to make and check the project, and the build-and-test skill repeats it once the first build and tests have run.
Near the end of a run, before its last message, Cline opens the delivery skill and completes and corrects the tree.
It writes the project part when the paragraph is still the placeholder, writes `THIRD-PARTY-NOTICES.md` when it is missing, and writes the child docs and their index lines.
Cline copies a starter with `cat` when the harness has not written one, because the stage places skill files read-only, and a copy made with `cp` would keep that mode.
The skill never asks Cline to type the framework text. The text comes from the starter.
A run that goes on in several sessions saves the commit in the bundle that the next session restores, so every later session loads the root file from there and the harness writes no second commit.
Work is measured from the starter commit in each session.
A later run, `ham continue` for example, starts from the head the earlier run delivered, which holds the root file. The harness writes nothing at the start of such a run.
Cline does not load the `AGENTS.md` files of the folders below the root.
The index in the root file lists them, and the DOX rules in the root tell Cline to open the child doc of a folder before it edits there.
Cline usually follows a rule that exists only in a child doc, because the root file tells it to open that doc before it edits there; a model can skip that step.

#### When the project already has an AGENTS.md

Before the harness writes anything, it reads your request.
If the request says to write no `AGENTS.md` in the words of the pack's own self-check, such as `Write no AGENTS.md docs.`, `Do not create AGENTS.md.` or `Build it without an AGENTS.md.`, the harness writes no starter.
The harness reads your request for this and does not read your rule files. If your rules say to write no `AGENTS.md` docs, or name another docs layout, Cline follows them: it takes the starter out again before its last message and ends with `DOX not applied: as you asked`.
Then the harness reads the root file at the head the run starts from, and Cline reads the same file as it was at the Base commit of the goal card.
An `AGENTS.md` that Cline wrote earlier in the same run does not count as yours.
The harness checks the rows of this table in the order given, and Cline does the same with the root file that results.

| Root AGENTS.md at the start of the run | What the harness writes before the first prompt | What Cline does |
| --- | --- | --- |
| Holds the line `# DOX framework`, as after a delivery | Nothing. | Keeps the file and keeps the tree current. It updates the docs of what it changed and adds a doc for a new top-level folder. |
| First line is `# Development lifecycle`, as in a folder set up with `ham lifecycle init` | The starter below the lifecycle contract, with a blank line between, and the notice. | Keeps every line, fills the project part and keeps the tree current. `ham lifecycle init` also copies both reference files into the `references` folder of the `lifecycle-delivery` skill under `.agents/skills`. |
| None, an `AGENTS.md` with no text in it, or only a `CLAUDE.md`, a rules folder or another instruction file | The starter and the notice. | Fills the project part, writes the child docs and their index lines. In a repository that already had files, child docs go only to the folders Cline touched. It leaves the other instruction files alone. |
| Any other text, yours or from a project delivered before this default | Nothing. | Changes none of it and writes no child docs. It ends with `DOX not applied: your own AGENTS.md`. If your request asks for the DOX docs, Cline adds the starter below your text and keeps every line of yours. |
| A link or a folder named `AGENTS.md`, a tracked name that differs from it only in case, or a repository that ignores the file | Nothing. It never writes through a link and never forces an ignored file into the history. | Cline decides at the end of the run, as it did before the harness wrote the starter. |

Only the root `AGENTS.md` decides which row applies.
An `AGENTS.md` that you send with `--attach` is material for the request, not the root file.
The harness writes the starter and checks nothing after it. The skill asks for the other results, and the harness enforces none of them.
Check the result if the file matters to you.

#### Change it or switch it off

| I want to | Do this | What happens |
| --- | --- | --- |
| Keep my own `AGENTS.md` | Start the run from a repository whose root `AGENTS.md` is your own text: no `# DOX framework` line, and not the lifecycle contract (`--repo cc:<name>`). Do not ask for the DOX docs in the request. `ham continue` keeps it too. | The harness writes nothing. Cline changes none of it and writes no child docs. It ends with `DOX not applied: your own AGENTS.md`. |
| Skip the tree for one run | Say so in your request, for example `Write no AGENTS.md docs.`. Or start the run with `--no-skills`, which sends no skills, so the other six skills of the pack stay home too. Or start it with `--no-rules`, which sends none of your files, skills included. | With the request, the harness writes no starter and Cline writes no `AGENTS.md`, neither the root file nor a child doc, and ends with `DOX not applied: as you asked`. The harness and the pack's self-check read only a request that names `AGENTS.md`, as `Write no AGENTS.md docs.` does, as a ban on the file; `DOX off` or `No docs` alone leaves the root file expected. A ban in other words reaches Cline, which takes the starter out again at the end of the run; a run that ends before that keeps it. With `--no-skills` or `--no-rules` the run sends no delivery skill, so there is no starter to write and Cline writes no docs tree, unless the repository holds the skill itself, as a folder set up with `ham lifecycle init` does. |
| Use my own default for every new project | Edit `~/.config/ham/skills/lifecycle-delivery/references/dox-root.md` and put your text in it. Keep a `# Project` heading in it: Cline fills the paragraph under that heading. `XDG_CONFIG_HOME` moves the folder. | The harness writes your file as the root, at the start of every run. `ham rules init` keeps an edited file and prints `kept lifecycle-delivery/references/dox-root.md (edited by you; not changed)`. If your file has no `# DOX framework` line, Cline writes no notice and no child docs, and later runs treat the root as your own (see the note under this table for a folder set up with `ham lifecycle init`). |
| Switch the tree off for every new project | Make the first line of that file `DOX off`. The rest of the file is ignored. | The harness writes no starter. Cline writes the project part alone at the end of the run, below any lines the root already keeps (the lifecycle contract or your text). It writes no framework, no child docs and no notice, and ends with `DOX off`. A project whose root already holds the framework keeps its tree. |
| Change the docs after delivery | Edit them like any file, or ask in `ham continue`. | `ham continue` keeps a root that holds `# DOX framework` and updates the docs of what it changes. |
| Remove the tree from a delivered project | Delete the child docs and the framework text of the root file. Keep the rest of the root file. | The root is then your own, and later runs leave it alone. If you delete the root file itself, the harness writes nothing at the start of the next follow-up run, and Cline writes the starter again at the end of it, unless the first line of the starter is `DOX off`. |
| Get the default on a machine that has the earlier pack | Run `ham rules init` once after you update `ham`. | The command replaces the unedited delivery skill of the earlier pack and prints an `updated` line, for example `updated lifecycle-delivery/SKILL.md (was pack 1.0.7; not edited by you)`. It creates the two reference files. A file you edited is kept. |

The skill also tells Cline to follow your rule files when they say to write no `AGENTS.md` docs or name another docs layout.
`--no-skills` leaves the rule file `10-lifecycle.md` in place and sends no skill.
`DOX off` is read whenever a root is started from the starter: there is no root file, the root is the lifecycle contract, or you asked for the DOX docs on a root of your own. The harness reads it at the start of a run, and Cline reads it at the end.
In a folder set up with `ham lifecycle init`, the root keeps the lifecycle contract as its first line.
If your starter has no `# DOX framework` line there, or `DOX off` is its first line, the skill's order of checks sends Cline back to the starter step in every later run, so avoid that combination in such a folder.

#### What Cline says at the end

| Line | When |
| --- | --- |
| `DOX: root and <n> child docs written, <m> updated` | The tree was written or kept current. The counts can be 0. |
| `DOX not applied: your own AGENTS.md` | The root file was yours and you did not ask for the DOX docs. |
| `DOX not applied: as you asked` | Your request or your rules said to write no docs or to use another layout. |
| `DOX not applied: starter not found` | Cline could not find `dox-root.md` in its skills. It wrote the project part alone. |
| `DOX off` | The first line of the starter was `DOX off`. |

The line stands outside the goal card and before any `BLOCKED:` line of the last message.
These are Cline's words. The harness does not check them, and they never change the outcome label.
Some runs end with none of these lines, for example when the delivery skill was not used.

#### What it costs

These figures are estimates at 4 bytes per token.
Cline puts the root `AGENTS.md` that exists when a prompt starts into the system prompt of every request of that prompt.
The starter alone is about 4,300 bytes and adds about 4,300 bytes, about 1,100 tokens. A root file of about 7 KB, the starter plus a 3 KB project part, adds about 7,100 bytes, about 1,800 tokens.
Five child index lines raise that to about 7,300 bytes.
A folder set up with `ham lifecycle init` has the lifecycle contract as its root. With the starter appended, that root adds about 10,300 bytes, about 2,600 tokens.
Because the harness writes the starter before the first prompt, every run pays this cost from its first request, unless the first line of the starter is `DOX off`.
A child doc of 15 to 25 lines is about 250 tokens, and a run pays that only when it opens the doc.
The delivery skill's `SKILL.md` is about 6,800 bytes, about 1,700 tokens, and Cline reads it once, near the end of the run.
The two reference files cost nothing unless Cline opens them.
The skill's description is 229 characters, about 60 tokens, and it is in the skill list on every request.

#### Check a delivered project

The tool prints one `DOX-RULE <name> PASS|FAIL|NA: <value>` line for each rule, one `DOX-SUMMARY` line and a last line `DOX-VERDICT PASS|FAIL|NA <mode>`.
The exit code is 0 for PASS or NA, 1 for FAIL and 2 for a usage error.
For a follow-up run, pass `--base-root <the root AGENTS.md the run started from>`.
For a project that had files before the run, pass `--base-has-files`.
A pass says the tree is well formed: the nine DOX headings in the root, each child doc listed in the index of its nearest parent doc, a doc in each top-level folder and each manifest folder, the six headings in order, and the notice file.
A pass does not say that the docs are true or that Cline read them.
The tool reads the tree, writes nothing and runs nothing from it.

### continue.md and nudges

The optional nudge file lives beside the config file:

`~/.config/ham/continue.md`

A nudge is another prompt sent after Cline stops without DONE.
The default file has this frontmatter and text:

```text
---
max_nudges: 0
---
Re-read my request. If anything is unmet, continue. If it is all done and the full tests pass, reply DONE and a summary.
```

With `max_nudges: 0`, nudges are off.
The session never sends the default nudge.

Setting `max_nudges` to 1 or more turns nudges on.
Cline must end a turn without saying DONE as a word.
The session then sends the nudge text as another prompt.
The number limits how many nudges it can send.

Nudging stops early after two prompts in a row leave files unchanged.
The original prompt counts as the first prompt.
The session never nudges after a cancel.

The file needs frontmatter with only the `max_nudges` key.
Its value must be a whole number; there is no upper limit.
Any other frontmatter key is refused.
The body must be nonempty. It has no size limit of its own.

Turning nudges on is your choice.
While the goal loop runs, the nudge is off. The loop sends its own follow-up prompts, and a nudge on top of them would be a second loop. The summary then shows the nudge as asked for and not used. With `--goal off`, or when the loop cannot start, the nudge goes out as it always did.
By default, the product leaves Cline's behaviour as it is.
The driver does not set Plan or Act mode.
Cline's `spawn_agent`, its sub-agent tool, stays on (see "Does a run use several agents?").

### review.md and the review round

The review file lives beside the config file:

`~/.config/ham/review.md`

When Cline ends a turn with a message and the harness counted at least one changed file, the harness sends one more prompt in the same session.
This prompt is the review round.
It asks Cline to review its own delivery as the maintainer who must merge it.
The harness fills in `{{base}}`, the commit the run started from, and `{{facts}}`, four lines it computed from the work tree:

- the changed files,
- the new top-level folders,
- the files under `.ham`,
- the untracked files.

The default prompt has six numbered points:

1. Scope: remove what the task did not need.
2. Other users: search for stale copies of every changed name and claim, and run the tests of scripts that copy a changed script.
3. Clean base: make a scratch worktree of the starting commit outside the repository, run the repository's own checks there and here, and compare the names of the failing tests.
4. Red first: put the non-test files back to the starting commit in a second scratch worktree and run the new tests there. They must fail on an assertion about the reported problem.
5. Every rewritten line needs a test that fails when it is reverted.
6. Claims: prove each claim in the commit messages, docs and last message with a command, or remove it.

`ham rules init` writes the default file and never overwrites an edited one.
Change it with `ham rules edit review`.
The file may start with frontmatter that has one key:

```text
---
guard_minutes: 30
---
```

`guard_minutes` is the least time, from 1 to 480 minutes, that a run must have left for the round to start.
The default file leaves the key out, and the stage's own 20 minutes apply.
Any other key is refused, and so is a body that is empty. The body has no size limit of its own.
`ham` sends the body as you wrote it; the harness checks it again.

The round runs once.
It does not run in these cases, and the record says which:

- you set `run.review_round` to `off`,
- the harness ended the run (for example, the goal loop's stall brake),
- the last turn did not end with a message from Cline,
- the message has a line that starts with `BLOCKED:`,
- the last line of the message ends with a question mark,
- the harness could not count the changed files, or counted none,
- less than the guard time of the run's time is left.

The turn that follows is accepted like any turn.
If it fails (a provider error, a dead or silent engine, the deadline, no message), the run keeps the outcome of the turn before the round.
If you cancel during the round, the run ends as your stop.
The label “Cline reported done” still needs a message and a changed file at the end.
`summary.json` and `EVIDENCE.md` record whether the round ran or why not, the commit before and after, whether it changed a file, and its seconds and tokens.
The harness makes no judgment of the result.

Switch the round off for every run with:

```bash
ham config set run.review_round off
```

A stage can also switch the round off for all runs, and a run cannot switch it back on.
## Choosing a model

There are six models.
All use the Bedrock Converse API in the Region of the stage. The list shows them with `us-east-1`.
Bedrock is the only provider.

These are the models of Cline, the default engine. Each engine has its own smoke records, so the models you can choose depend on the engine. The same model can be listed for both engines. The service refuses an engine and model pair without a record and names the pair.

| Alias | Name | Bedrock id | Context tokens | Max output tokens |
|---|---|---|---|---|
| `opus` | Claude Opus 5.5 | `us.anthropic.claude-opus-5-5` | 1,000,000 | 128,000 |
| `sonnet` | Claude Sonnet 5.5 | `global.anthropic.claude-sonnet-5-5` | 1,000,000 | 128,000 |
| `sol` | GPT-6.1 Sol | `us.openai.gpt-6.1-sol` | 1,000,000 | 131,072 |
| `astra` | GPT-6 Astra | `us.openai.gpt-6-astra` | 1,050,000 | 128,000 |
| `glm` | GLM 5.3 | `us.zai.glm-5.3` | 1,048,576 | 128,000 |
| `fable` | Claude Fable 5.2 | `global.anthropic.claude-fable-5-2` | 1,000,000 | 128,000 |

Claude Fable 5.2 needs provider data sharing. See "Models that need provider data sharing" below.

Claude Sonnet 5.5 is the built-in default.
Each listed model has a passing smoke record.
A model without one appears as "pending" and is refused.

The list is compiled into `ham` and the service.
The service refuses ids outside the list.
Refusals use HTTP 422 with `unknown-model` or `model-not-smoked`.

Aliases are lower-case in the list.
Typed aliases match while ignoring letter case.
Full ids must match exactly.

### Models that need provider data sharing

Some models run only in an AWS account whose Bedrock data retention mode lets them run (`provider_data_share` or `aws_review`), and only for a run that agrees to share its data with the model's provider.
Claude Fable 5.2 (`fable`) is one.
`ham models` marks such a model with "needs provider data sharing", and prints a short paragraph about the setting under the table.

With the setting on, the prompts and answers of a run on that model go to the model's provider.
The provider may keep them up to 30 days for safety monitoring.
The setting is off by default.

Turn it on for every run:

```bash
ham config set run.provider_sharing on
```

Or turn it on in the Settings screen, or for one run with `--provider-sharing on` on the start command or in the Options box of the New run screen.
The flag wins over the file.
`--provider-sharing off` refuses sharing for one run whatever the file says.

While the setting is off, `ham` refuses such a model on the command line before it sends anything.
The message names the model, the reason and the three ways to turn the setting on:

```text
Claude Fable 5.2 (global.anthropic.claude-fable-5-2) needs provider data sharing, and the setting is off. With it on, the prompts and answers of a run on this model go to the model's provider, which may keep them up to 30 days for safety monitoring. Turn it on with `ham config set run.provider_sharing on`, in the Settings screen, or with `--provider-sharing on` for one run. Nothing was sent.
```

The service refuses a start request for such a model that does not carry `providerSharing: true`.
The reply is HTTP 400 with the code `provider-sharing-required`.
A run that did not agree never switches to such a model when its own model fails.

In the terminal app, a run on such a model with the setting off asks first.
When you submit, `ham` reads the account and its mode, and the New run screen shows a question in place of the model list.
The question names the model, what sharing means, the AWS account and the Region, and what `ham` changes there.
`y` agrees for this run only and starts it; the file is not changed.
`y` agrees to what the question shows, and to nothing else.
`ham` reads the account again before it changes anything.
When the account is no longer as the question showed it, `ham` changes nothing and starts nothing.
It asks again and shows what it now reads: another mode, another account or Region, a read that now fails, or one that now answers.
`n` or Esc starts nothing and says so: `Not started: you did not agree to provider data sharing for ... Nothing was sent and the AWS account was not changed.`
A choice in the file or on the command line is your agreement, so with `run.provider_sharing` on, or `--provider-sharing on`, `ham` asks nothing.
`--provider-sharing off` in the Options box refuses as on the command line.

The setting is half of the agreement.
The other half is the AWS account's.
The account's Bedrock data retention mode in the stage's Region must let the model run: `provider_data_share` or `aws_review`.
In any other mode Bedrock refuses the call with a `ValidationException` whose text ends:

```text
data retention mode 'default' is not available for this model
```

When a run agrees, `ham` checks the mode right before it starts the run, with your own credentials, in the stage's Region:

- `provider_data_share` and `aws_review` are kept, and nothing is said.
- `default` and `inherit` are switched to `provider_data_share` and read back. One line says so, on the terminal or on the run screen. It names the account, the Region, the old and the new mode, and the command that sets the mode back. The run screen also keeps the whole line in the run's `provider data sharing` card while `ham` stays open. The switch covers every Bedrock call of the account in that Region and stays after the run: `ham` never sets it back, because other runs may rely on it.
- A switch that is denied (your credentials need `bedrock:PutAccountDataRetention`), that Bedrock refuses, or after which the mode does not read back as one that lets the model run stops the start. One message says why; no run starts and no model is called.
- `ham` changes nothing when it cannot read the mode (your credentials need `bedrock:GetAccountDataRetention`), when the mode is one it does not know, or when your credentials belong to another account than the stage. The run then starts as before, with one line, and Bedrock refuses the model if the mode does not let it run.

With several backends, the account is the one of the backend the run starts on.
For a new run that is the backend you chose, or `default_backend`.
For a follow-up, a resume and a retry it is the backend that holds the run they continue.
`ham` reads and switches no other account.
A start with no backend chosen is refused before any account is read.

A resume and a retry start the request the run was started with, so `ham` asks no question for them.
It reads the run's model from the run's row in the list of runs.
For a model that needs provider data sharing it then reads the mode as above.
With `run.provider_sharing` on it switches `default` or `inherit` and says so in one line.
With the setting off it switches nothing and starts nothing.
One message names the account, the Region and the mode, and the two ways on: turn the setting on, or have the account owner set the mode.

`ham` writes no file for the switch. The line on the screen is your record, and the account records the call to `PutAccountDataRetention` under your identity.

The account owner can also set the mode, for the whole account in that Region:

```bash
aws bedrock put-account-data-retention --region us-east-1 --mode provider_data_share
```

`ham doctor` reads the mode with your own credentials and prints one `provider-data-sharing` line; it never changes the mode.
The line says whether the setting is on and which mode the account has.
It is a `WARN` when the setting is on and the mode does not let such a model run; it says that `ham` switches it when such a run starts, and it names the command above with the Region of the stage.
It says `unknown` when your credentials may not read the mode (they need `bedrock:GetAccountDataRetention`) or Bedrock does not answer.
The line never makes `ham doctor` fail.

### How the choice is made

The first set value wins, in this order:

1. `--model <id|alias>` on the start command.
2. `HAMH_MODEL`, the model environment variable.
3. `default_model`, the config model setting.
4. The built-in default, Claude Sonnet 5.5.

A blank value counts as unset.
A wrong set value causes an error.
The error names its source.
It never falls through to the next layer.

The printed source labels are:

- `--model`
- `HAMH_MODEL`
- `config default_model`
- `built-in default`

The start prints its chosen model on stderr:

```text
model: <name> (<id>), from <source>
```

### Model commands

Show the models without a network connection:

```bash
ham models
```

A `*` marks the model the next run would use.
The `default` label marks the built-in default.
The output also shows:

```text
next run (<source>)
```

A model without a passing smoke record gets this text:

```text
pending: no passing smoke record, not selectable
```

Under the table, a `Notes:` heading lists one line for each model that has a note.
Only GLM 5.3 has one.
Its note says that GLM 5.3 reasons at high effort by default.
A long reasoning turn can take many minutes at high.
At xhigh it can end without an answer.
GLM 5.3 is not recommended for a first run.

JSON output adds `selected`:

```bash
ham models --json
```

Read the same list from the stage:

```bash
ham models --server
```

The stage serves it from `GET /v1/direct/models`.
It notes when the stage's default differs from this build's.

Set the config choice:

```bash
ham config set default_model <id|alias>
```

This stores the full id.
It refuses unknown models and models without a passing smoke record.
It lists the available choices.

Read or remove the config choice:

```bash
ham config get default_model
ham config unset default_model
```

### Model errors

An unknown model from `--model` produces this error:

```text
unknown model "gpt-9" (from --model); choose one of: <list>
```

An error from `HAMH_MODEL` adds:

```text
; unset HAMH_MODEL, or set it to one of these
```

An error from the config adds:

```text
; fix it with ham config set default_model <alias>, or remove it with ham config unset default_model
```

A model without a passing smoke record produces:

```text
model "<input>" (from <source>) is listed as <id> but has no passing smoke record, so it is not selectable yet
```

### GLM 5.3

GLM 5.3 cannot switch reasoning off.
With `off`, Cline sends GLM 5.3 low effort.
A default run uses high effort.
That level comes from the model's row in `engine/catalog/models.json`.
Fork patch `0012` sends the level as `reasoning_effort` and sends low when no level is requested.

A long reasoning turn of GLM 5.3 can take many minutes at high.
At xhigh, it can end without an answer.
You can request xhigh with the engine setting `reasoning.effort = "xhigh"`.
High remains the default.

Set the level in the engine file `engines/cline.toml`.
You cannot set the level per run from `ham`.

GLM 5.3 stays selectable.
It is not recommended for a first run.

### When the model provider fails

Amazon Bedrock can time out, throttle or fail. The harness handles these errors, and you do nothing. The work already done stays in the work tree.

**Step 1: resume.** The harness resumes the same Cline session with one fixed sentence:

> A previous attempt on this work tree was interrupted by a provider failure. Inspect the work tree and continue the task.

It tries up to 3 times on the model you chose. An error that will not pass by itself gets 1 try. Each try follows a pause. `summary.json` lists every try under `attempts`.

**Step 2: switch.** If your model keeps failing, the harness moves the same Cline session to the next route. A route is one model id at Bedrock. The order is:

1. The other inference profile of your model (`us.` or `global.`).
2. The other models of the same vendor.
3. The remaining models.

At most 8 routes follow the model you chose. Every route has a passing smoke record and runs in `us-east-1`. Cline keeps the conversation. A tool result from the first model reaches the second. A new model starts with the next prompt, so a switch always follows a failure.

A fallback route gets 1 resume after an error that passes by itself and none after any other error. A route that Bedrock refuses (model not found, access denied) is dropped for the rest of the run.

**Back to your model.** When the route in use fails, the harness first tries the model you chose once more. It does this only if that model last failed at least 2 minutes (120 seconds) earlier. A run that finishes on a fallback route stays there.

**When every route is down,** the run waits instead of failing. The first wait is 30 seconds. Each later wait is half as long again, up to 5 minutes. After each wait, the harness sends a tiny test request to the usable routes in order and starts again on the first route that answers.
The waiting keeps the silence watchdog quiet.
It ends when a route answers, when you run `ham cancel`, or at the run's time limit. At the time limit, the label is "Cline stopped: the run reached its time limit" and the detail says "Every route was down for the last N s."

If Bedrock refuses every route, no wait can help. The run ends with "Cline stopped: the model provider failed" and delivers what exists.

**What you see**

- The Detail line of the card begins "Ran on <model> after <n> switches." (for example "Ran on us.zai.glm-5.3 after 9 switches."; one switch reads "1 switch"). It shows the raw Bedrock id.
- The status line, the first row of the run screen, names the model the run is on now, so after a switch it shows the new model. The Model line of the status block keeps the name of the model you chose, and while the run goes on its Route line shows the model in use and the switch count.
- `EVIDENCE.md` in the zip has a "Ran on" row only when the run switched. `evidence/direct.json` holds `finalModelId` and `modelSwitches`. `summary.json` lists every switch under `switches`.
- The run can take longer, and the code in the zip can come from more than one model.

**Limits**

- Cline's own stop is not a provider failure. Its loop guard stops after 5 identical tool calls in a row. Its mistake limit stops after 6 failed tool calls in a row. The harness resumes the same session once on the same model. It never switches the model for this stop. After the resume, the harness asks you what to do. See the chapter “When Cline gets stuck”.
- Before sending the whole conversation to another route, the harness sends that route a small test request. The bound is 12 provider-failed prompts in a row with no completed tool call. At that bound, it stops sending the conversation. It tests only the route you chose. After 30 minutes with every route down, it raises one notice. It keeps waiting.
- The card shows the raw model id and never a display name, and its status box keeps the model you chose.
- One output limit and one context window apply to the whole run: the smallest of all routes.
- Only `us-east-1` routes are used. The harness does not cycle Regions.

```bash
ham models
ham config set default_model sol
HAMH_MODEL=opus ham "..." --s3-only --direct
ham "..." --s3-only --direct --model astra
```


### When the session disk is full

A run has two disks. The root volume holds the tools, `/tmp` and the engine's own files. The checkout sits on session storage that holds 1 GiB, and that limit cannot be raised. Neither the model nor the harness can read how much of that 1 GiB is used, because the mount always reports 0 used.

Cline's Rust builds can fill it: one `cargo test --workspace --no-run` of a large repository wrote 965 MB of build output into the checkout. The build output of Cargo goes to a folder on the root volume, outside the checkout. The setting `AGENTD_CARGO_TARGET_DIR` can name another absolute folder, or the word `checkout` to keep Cargo's own `target/` folder in the repository.

Before the disk is full, the harness prunes old Cargo build output while a prompt runs. Every 60 seconds (`AGENTD_DISK_CHECK_S`) it reads the free space of the root volume. Below 1536 MiB (`AGENTD_DISK_PRUNE_MB`) it prunes the Cargo target folder of the run. A unit (a crate or a test binary) that Cargo compiled in the last 20 minutes (`AGENTD_CARGO_KEEP_MIN`) is never removed. Cargo records when it compiled a unit and leaves that record alone when a later build finds the unit fresh, so a dependency that every build needs also looks old. The harness therefore removes first the older copy of a unit that has a newer copy, which another set of features makes. Only when the root volume is still below the stop mark of 512 MiB (`AGENTD_DISK_LOW_MB`) does it remove the other old units too, and the next build compiles those again. It prunes a profile directory such as `debug` only while no Cargo build runs there: it takes Cargo's lock files without waiting, skips the directory when a build holds them, and keeps the locks while it removes, so a Cargo that starts meanwhile waits a moment. It prunes nothing while a program runs from the target folder, such as a test binary of a test run in progress. It never follows a link and never touches anything outside the target folder. Each prune that removes something writes one notice with the number of units, an estimate of the space freed and the free space after it. A value of 0 for `AGENTD_DISK_CHECK_S`, `AGENTD_DISK_PRUNE_MB` or `AGENTD_CARGO_KEEP_MIN` switches the pruning off. A run that still runs out of disk ends as described next.

When a model request fails with error -32603, the harness measures the disks before it treats the error as the provider's fault. It adds up the files in the checkout and reads the free space of the root volume. The disk counts as full when the checkout holds 960 MiB or more (`AGENTD_WORKSPACE_LIMIT_MB`), or when the root volume has less than 512 MiB free (`AGENTD_DISK_LOW_MB`). A value of 0 switches that check off.

The engine's own error text counts too. The session storage turns read-only once it is full, so the error can read “Read-only file system (os error 30)” instead of “No space left on device (os error 28)”. Either text stops the harness at once, even when the numbers show room, and the harness still reads the numbers and puts them in the words. A read-only text names both causes: “the session disk is read-only or full”.

If the disk is full, the harness stops at once. It sends no more prompts and tries no other route or model, because they would meet the same disk. The label reads “Cline stopped: the harness ended the run (…)”, and the brackets hold the cause with its numbers, for example “the session disk is full, workspace 975 MiB against a stop mark of 960 MiB, root 2400 MiB free”. With the same error and enough room, nothing changes: the harness resumes the work and switches routes as before.

**What you can do.** When the run has an answer window, it asks you right away. The question offers two answers: a message with `ham say`, for example asking Cline to delete build output or temporary files, or `ham finish`. Cline may not be able to answer while the disk is full. If your message fails the same way, the run asks again, up to the usual three questions. The work already saved is delivered when the run ends.

**Early notice.** Before a prompt, the harness reads the same two numbers. When the root volume drops below its low mark, or the checkout reaches 90 percent of its stop mark, the run shows one notice such as “The workspace folder is nearly full: 900 MiB used against a stop mark of 960 MiB”. A new notice comes only after the number has gone back across the mark. The harness deletes nothing.

### The model that names runs

Run names and run tags come from a small model, Claude Haiku 5.5 (Bedrock route `us.anthropic.claude-haiku-5-5`), called the utility model.
It answers the service's own helper calls, outside Cline, the coding engine.
Its median answer takes 935 milliseconds.

It is not one of the six models, and you cannot choose it.
It is not in the list that `ham models` prints.
`--model haiku`, the `HAMH_MODEL` variable and the `default_model` key refuse it, as they refuse any name outside the five.
Cline never uses it and keeps the model you chose.

It sees at most the first 2,000 and the last 1,000 characters of your prompt.

A prompt of up to 3,000 characters goes in whole.
Known credential shapes, such as access keys and tokens, are hidden before the text is cut, so those credentials are not sent.
Other secrets are not hidden, so keep secrets out of your prompts.
It never sees your rules, your skills, your repository files or the conversation.

The same model also writes the topic and tech tags of a run; see [Tags](#tags).
For a tag it sees the same excerpt of your prompt as for a name.
It also sees a short list of your own tags, so that it can reuse your words.
The list holds up to 40 tags, the ones you use most among your 200 newest runs, each with the number of runs that hold it.
The list never holds a tag from another person's runs.
The list never holds a `project:` tag.
Once you have 50 or more runs, it leaves out a tag that more than 30 percent of them hold.
A tag you removed from a run does not count for that run.

The service calls the model with the stage's own AWS account in the Regions described under [Accounts and Regions](../README.md#accounts-and-regions).
Whichever coding model you chose, that excerpt goes to Anthropic's Claude Haiku 5.5, and so does the list of your own tags when it writes tags.
With Claude Sonnet 5.5 or Claude Opus 5.5 this adds no new provider.
With GPT-6.1 Sol, GPT-6 Astra or GLM 5.3 it adds Anthropic as a second reader of the excerpt.

An operator can switch the model off for a stage at deploy; every run on that stage then keeps its seed name and gets no model tags. Claude Haiku 5.5 writes run names and
tags only in us-east-1. A stage in us-west-2 deploys with the model off, so every run there keeps its seed name and gets no model tags. The setup plan says which case
applies to your Region. An operator can also switch only the tags off, with the deploy setting `HAMH_UTILITY_PURPOSES=run-name`. Names then continue, and every run keeps its
derived project and the tags you type. There is no switch for one user or one run.
## Watching a run

Follow a run with:

```bash
ham watch [<runId>]
```

Without a run id, this follows your newest run.

List your runs with:

```bash
ham runs
```

Runs appear newest first, with one line for each run:

```text
<runId>  <status> (<phase>)  <target>  <createdAt>  <name>
```

A run that has a name prints two spaces and the name after the creation time.
A run without a name prints the line without them.
A run with tags prints two spaces and its tags in square brackets after the name (after the creation time when the run has no name).
For example, `[project:billing topic:csv-export]`; the tags are separated by single spaces.
The ended-early and needs-input notes come after the tags.
A run without tags prints no brackets.
Tags the service sent are cleaned like every string from the API, and at most 19 show.
The run id stays the first field, so `cut -d ' ' -f 1` still returns it.

With the option `--json`, `ham runs` adds `name` and `nameSource` to each run that has a name.
It also adds `tags`, an array of strings, to each run whose row has it, an empty array included.
It adds no other member.
`nameSource` is `ai` for a name the model wrote and `seed` for a seed name.
An older service sends no `tags`; ham then prints no brackets and adds no member.

Runs ended early by the service have this added:

```text
  ended early: ...
```

The watch command polls events about once per second while events arrive. It waits up to five seconds when none do, and it reads only the event shards a run uses. It prints one line whenever the view changes:

```text
run <runId> <status> (<phase>) gates 0/0 met 0 agents 1 merges 0
```

Once a pull request exists, the line also includes:

```text
 pr <url>
```

A direct run has no gates or verdicts. Its numbers after gates, met and merges are always 0.

Current statuses are `active`, `complete`, `blocked` and `cleared`. Direct runs use these phases:

| Phase | Meaning |
| --- | --- |
| planning | The service writes `planning` when it creates the run. The session has not started yet. |
| green | Cline, the session engine, is working. |
| delivering | The service is packaging the result. |

The Runs screen and the run screen of the terminal app show a phase only while a run is `active`, and, for a `blocked` run, where it stopped.
A run that is `complete` or `cleared` shows no phase there, because the service keeps the last phase a run reached when it ends.
`ham runs` and `ham watch` print `delivered` for a `complete` run whose stored phase is `delivering`, and every other pair as stored. `--json` prints the stored phase.

A blocked run prints:

```text
blocked: <reason>
```

This means the run could not deliver anything.

A workflow ended early by the service prints:

```text
ended early: <status> [<error>][: <cause>]
```

Here, the status is `ABORTED`, `FAILED` or `TIMED_OUT`.

On a terminal, starting or watching a run opens the dashboard. You can leave it while the run continues in AWS.

Return with:

```bash
ham watch <runId>
```

When the run ends, watch downloads the zip and prints the card. The download directory follows this order:

- `HAMH_DOWNLOAD_DIR`
- `downloadDir` in the config
- The current directory

Watch and start commands use these exit codes:

| Exit code | Meaning |
| --- | --- |
| 0 | Complete; a zip exists. |
| 10 | Still active; you left while it works. |
| 11 | paused: the run used its total time; its work was delivered and `ham resume` continues it |
| 12 | blocked |
| 13 | usage_limited |
| 14 | budget_limited |
| 15 | cleared |
| 16 | stalled: the run lost its progress; its work was delivered and `ham resume` continues it |
| 1 | Error before the status was known. |
| 2 | Usage error. |

Exit 0 does not mean Cline reported done.

The card appears when the run is complete. It also appears when a pull request or zip exists. `ham watch` can print the card before the outcome label reaches it. The label is always in the zip's `EVIDENCE.md`.

| Card line | Contents |
| --- | --- |
| Run | Run id, status and phase. On the dashboard, a run that is `complete` or `cleared` shows its status alone |
| Outcome | The label: `Cline reported done` or `Cline stopped: <reason>` |
| Detail | The specifics of the stop, when there are any. After model switches it begins "Ran on <model> after <n> switches." When nobody answered a question it begins "Nobody answered in N min, so the run finished with what exists." When you chose finish it begins "You chose to finish." It may also say "A fresh session could not start." |
| Model | The model you chose: its name and id, for example `Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)`. A run that switched models names the model it ended on in Detail |
| Session | The number of the run's last session, for a run that continued in a new session after the first one ended; for example `3 (the run continued in a new session 2 times)`. A run of one session has no such line |
| PR | Pull request link, or `n/a` |
| Zip | Local path, first 12 characters of sha256, bytes |
| Link | In line output (`ham watch` without a terminal, `ham pull`), the whole presigned link and its expiry on one line. On the dashboard, one short line: the host label, the zip name, the expiry in your local time and the keys `o open · d download`. The dashboard does not draw the link itself |
| Clone | The clone command. On the dashboard it reads `press C, or run: ham clone <runId>`, then shows the result of the clone |
| Push | The push command, `ham push <runId> <git-url>`, which sends the run's branch to another git repository. Shown whenever there is a zip |
| Tokens/USD | Token count and cost in US dollars, as the stage reported them. Tokens add up input, output and cache tokens. The line is left out when the stage reported no usage. USD reads `unknown` for a model the stage has no price for |

The Clone line gives this command:

```bash
ham clone <runId>
```

A direct run has no Evidence block. The Outcome line carries the label.

## When Cline gets stuck: notifications and your answers

`ham`, the terminal app, runs Cline, a coding agent, on AWS.

During a run you can cancel it, send messages, start another chat on the same files and change the model.
Cancel with `ham cancel <runId>` or press Esc and then `c` twice in the dashboard.

agentd, which hosts Cline inside the microVM, asks you for help.
It asks when Cline stops and automatic recovery cannot help.
It keeps the Cline session and work tree alive.
The question arrives as a `needs-input` event.
Heartbeats continue while the run waits.
File snapshots continue every 10 minutes.

Automatic recovery runs before you get a question:
- Provider retries.
- Route and model switches.
- One resume.
- For some stops, one automatic fresh session.

The harness records how Cline stopped.
It never judges the work.
These endings never ask:
- "Cline reported done"
- Your own cancel.
- The run's time limit.
- A start-up failure.

### The recovery ladder at a glance

The harness tries cheap remedies first and asks you last. Model changes come last, and only for a provider fault.

1. **Retry:** The harness resumes using the model you chose. Each resume tells Cline to inspect the work tree and continue. That instruction uses one fixed sentence.
   - Timeout, throttle, 5xx answer, or cut stream: up to 3 resumes
   - Pauses: 5 s, 10 s and 20 s plus jitter, a random extra bit
   - Provider's retry-after: can lengthen a pause to at most 120 s
   - Error that will not pass by itself: 1 resume
   - Content filter, full context window, or output limit: no resumes

2. **Cline's own stop:** The harness resumes once when either guard ends the prompt. It keeps the same model for this stop.
   - Loop guard: 5 identical tool calls in a row
   - Mistake limit: 6 failed tool calls in a row

3. **Silence:** After 900 seconds (15 minutes) without output, the harness cancels the prompt. It resumes once. A second silence asks you. The person who deploys the stage can change the 900 seconds (60 to 3600) with `HAMH_PROVIDER_SILENCE_S`.

4. **Fresh session:** The harness starts one fresh session after either failure below. This means a new Cline chat in the same work tree. It starts with a short written summary of the harness's records.
   - Failures: provider refuses the saved conversation, class 4; or Cline process died
   - Summary limit: at most 36,000 bytes, about 12,000 tokens
   - Your requests: up to 3 fresh sessions with key s or `ham new-session` (`run.max_fresh_sessions` changes the number)

5. **Ask you:** When earlier remedies cannot help, a "Cline needs you" card appears. A bell sounds with the card. When nobody answers, the run finishes with existing work.
   - p: send a message in the same session; command: `ham say`
   - s: start a fresh session; command: `ham new-session`
   - m: try another model; command: `ham try-model`
   - f twice: finish; command: `ham finish`
   - Esc: close the typing screen and keep the question open
   - Command exits: 0 acknowledged, 1 error or not acknowledged, 2 usage
   - Command options: --input-wait and --no-wait
   - Questions per run: at most 3
   - Wait window: 30 minutes in the interactive terminal app
   - Wait window in scripts: 0, meaning no question
   - Unanswered question: the detail says the run finished with existing work

6. **Switch model:** Only a provider fault allows switching after the resumes of the first step run out. The harness tries at most 8 routes in the order below. When every route is down, the run waits and probes.
   - First: the other inference profile of your model
   - Next: other models of the same vendor
   - Last: the remaining models

7. **Finish:** The run ends when nothing works or you choose finish. It finishes with what exists. The label keeps the original stop reason.

**What the label says:** A missing heartbeat ends the session after 600 seconds (10 minutes). The label is "Cline stopped: the session was lost", not the time-limit label. If an earlier snapshot exists, the service delivers the last saved snapshot. The detail names it as the last saved snapshot. The detail also says when it was taken. Without a snapshot, the run ends blocked. Then `ham` exits 12, and no zip is delivered. In both cases, the work tree stays on the session storage. Nothing delivers the work tree from there. Recovery of a stopped session's stored work tree is not supported.

### What you see

Open the dashboard in a terminal with `ham`.
You can also use `ham watch <runId>`.

A panel appears under the status block.
Its title is "Cline needs you · about N min left to answer".
N counts down from the expiry time agentd sent.
The countdown uses your own clock.
Under one minute, it reads "less than a minute left to answer".

The panel shows these lines in order:

- The first line gives the reason and the numbers.
- For example: "Cline stopped its own prompt. 41 tool calls, 3 files changed."
- The next line explains the reason in plain words.
- The next section's table gives those explanations.
- "Last words:" shows Cline's last words.
- It shows at most 12 lines of 64 columns.
- "Files:" shows the first changed files.
- It lists up to 12 files.
- It adds "(+N more)" when more files changed.
- The key line is `p message · s fresh session · m other model · f f finish`.

Only offered keys appear.
When the chat cannot take another prompt, `p` and `m` disappear.
The terminal bell rings once for each new question. `ham config set ui.bell false` turns the bell off; the notification does not depend on it.
The Runs list rings once for a run that starts to wait; its first read makes no sound.
A question rings once in all: if it rang on the Runs list, opening the run does not ring it again, and if it rang on the run's screen, the next read of the Runs list does not ring it again.
A new question of the same run rings again.
With `HAMH_NOTIFY=1` (also `true` or `on`), or with `ham config set ui.notify true` when the variable is unset, a terminal notification `ham: Cline needs you (run <short id>)` is sent as well, but only while the window is not in front.
A terminal that sends no focus reports counts as in front, so it gets the bell only.
Inside tmux the notification is wrapped for tmux, which also needs `allow-passthrough on`.

The agents table shows "Needs your input: <title>" in its `LAST` column.
The Runs screen shows "needs input" under `STATUS` while waiting.
Its footer counts runs waiting for you.
For example: "2 runs · 1 needs input · needs input: <title>".

A narrow terminal replaces the panel with one line.
That line appears above the help line.
It carries "Cline needs you:" and the offered keys.

A fresh session adds `Session 2 · restarted from the work tree`.
This line appears in the status block.
The number is the session number.

The outage notice uses the same panel.
Its title is "Every model route is down".
Its text is "The run keeps probing until its time limit. c c stops it and delivers what exists."
Its numbers are "212 tool calls, 9 files changed so far."
It has no keys.

The panel disappears when the question closes.
A notice line keeps one of these outcomes:

- "answered: message sent to Cline"
- "answered: fresh session"
- "answered: other model"
- "answered: finish"
- "window closed: nobody answered"

If you are typing in the message box when the question closes, the draft stays.
The box title then reads "the question closed (...): Enter sends this as a message", with the same outcome words.
Enter sends the text as an ordinary message, and Cline is not waiting for it as an answer.
The words go away when you empty the box.

Without a terminal, `ham watch` uses line mode.
It prints each question once when it opens.

Example output (the values are illustrative):

```text
Cline needs you: Cline stopped its own prompt
  Cline ended the prompt itself, twice. Its guard stops after 5 identical tool
  calls; its mistake limit stops after 6 failed turns. The provider is fine.
  After this stop, the same chat stops again after one identical call. A fresh
  session starts both counts from zero.
  41 tool calls, 3 files changed.
  Last words: I will keep polling the build until it finishes, then append step
              4 to PROGRESS.md.
  Files: PROGRESS.md src/app.py tests/test_app.py
  about 27 min left to answer
  Answer with one of:
    ham say 01M4ZHFBV5SDX7J92E6YRH4QTW '<message>'            send a message to the same Cline session
    ham new-session 01M4ZHFBV5SDX7J92E6YRH4QTW ['<message>']  start a fresh Cline session on the same files
    ham try-model 01M4ZHFBV5SDX7J92E6YRH4QTW <model> ['<message>']  switch model (opus, sonnet)
    ham finish 01M4ZHFBV5SDX7J92E6YRH4QTW                    finish with what exists
```

Line mode prints one line when the question closes.
For example: "Question 1 closed: closed when nobody answered in time."
A fresh session prints "restarted from the work tree: session 2".
An open question adds " needs input: <title>" to the status line.

The outage notice prints "Notice: Every model route is down".
It also prints "No answer is needed. ham cancel <runId> stops the run and delivers what exists."

`ham runs` adds "  needs input: <title>" to waiting runs.
`ham watch --json` puts the question under `input`.
It puts the session count under `sessions`.

A `ham` that predates these questions cannot answer them.
It shows the question's one-line text as the agent's last line.
Update `ham`.

### The reasons Cline asks

| Code | What the card says | What it means |
| --- | --- | --- |
| `engine_stop` | Cline stopped its own prompt | Cline ended the prompt itself, twice. Its guard stops after 5 identical tool calls; its mistake limit stops after 6 failed turns. The provider is fine. After this stop, the same chat stops again after one identical call. A fresh session starts both counts from zero. |
| `no_changes` | Cline ended without changing files | Cline ended normally with a message, but no file differs from the start. Read its message: it may answer a question or give up on an impossible task. |
| `no_message` | Cline sent an empty message | Cline ended its turn with an empty message. You can send another prompt. |
| `output_limit` | The model reached its output limit | The model reached its maximum output length for one answer. You can send another prompt. |
| `request_limit` | Cline reached its request limit | Cline reached its model-request limit for one prompt. You can start a fresh session. |
| `refused` | The answer was refused or blocked | The model refused, or the provider's filter blocked the answer. You can change your prompt. |
| `replay_rejected` | The provider rejected the saved conversation | The harness already tried a fresh session once. More prompts in the same session fail the same way; choose a fresh session. |
| `context_full` | The conversation exceeds the context window | The conversation no longer fits the model's context window. The harness already tried a fresh session once; you can request another. |
| `provider_error` | The model provider failed | Retries and route switches could not fix the provider failure. You can finish with the existing work. |
| `cap_hit` | Automatic recovery reached its prompt cap | The harness stopped after 12 prompts in a row without a new tool call or changed file. You can supply another prompt. |
| `engine_died` | The Cline process stopped | The automatic fresh session did not help. Continuing is impossible; a fresh session starts a new process on the same files. |
| `silence` | Cline stopped sending output | Cline sent no output for 15 minutes, again after one resume. The process may be stuck; a fresh session starts a new process on the same files. |
| `other_stop` | Cline gave an unknown stop reason | The harness does not know Cline's stop reason. You can finish with the existing work. |
| `provider_outage` | Every model route is down | All routes have been down for 30 minutes. No answer is needed; probing continues until the time limit. `ham cancel` stops the run and delivers what exists. |

The panel never says the work failed or is done.
It says what happened to the run.
`engine_died` and `silence` cannot continue in the same chat.
The Cline process may be stuck or gone.
These reasons offer only a fresh session and finish.
`provider_outage` is an outage notice.
It takes no answer.

### What you can do

The dashboard offers four actions:

| Key | Action | What happens |
| --- | --- | --- |
| `p` | Continue with a prompt | Your text becomes the next prompt in the same chat. Cline keeps its history. Text is required: some text of at most 262,144 bytes. |
| `s` | Start a fresh chat | A new Cline session starts on the same files. Your text is optional. Cline gets the original task and why the last session stopped. It also gets a short summary and your text. |
| `m` | Choose another model | A list shows the models this run may use. These are the run's own routes. Pick with the arrow keys and Enter. Your text is optional. The chat continues on the new model. |
| `f` twice | Finish | The run ends with what exists. It delivers as usual. |

For `m` without text, Cline gets this fixed sentence:
> A previous attempt on this work tree was interrupted. Inspect the work tree and continue the task.

The typing screen opens for `p`, `s` and `m`.
For `m`, pick the model before the typing screen opens.
For `p`, its title is "Message to Cline, same session (Enter adds a line)".
The screen counts "N of 262,144 bytes".
For `s`, it says "Text is optional: Ctrl-S sends now, or type a message first."

`Enter` adds a line.
`Ctrl-S` sends.
`Esc` goes back.
It keeps the text you typed.
It leaves the question open.

The model list reads "Other model (↑↓ select · Enter choose)".
It also says "Cline keeps its conversation and continues on the model you choose."
Each model appears with its context size.
For example: "Claude Opus 5.5    ctx 1,000,000".

The first `f` shows "press f again to finish run <runId> with what exists".
The second `f` finishes the run.

A closed engine pipe can prevent another prompt.
A stall can prevent another prompt.
A passed deadline can prevent another prompt.
In these cases, `p` gives this line:
"Cline cannot take a message now: s starts a fresh session, f f finishes."
In these cases, `m` gives this line:
"Another model needs a live Cline session: s starts a fresh one, f f finishes."

Empty text is refused before sending.
Text over the limit is refused before sending.
The refusal shows the limits.

The outage notice has no keys.
Press Esc, then `c` twice to stop the run.

A message in the same chat keeps Cline's counts.
Cline's loop stop follows 5 identical tool calls in a row.
Its mistake stop follows 6 failed tool calls in a row.
After either stop, one more identical call stops that chat again.
A different call works.
A fresh chat starts both counts at zero.
After an `engine_stop` question, ask Cline to do something different, or start a fresh chat.

### Answering from the command line

The same four actions work without the terminal app:

```bash
ham say <runId> '<text>'
ham new-session <runId> ['<text>']
ham try-model <runId> <model> ['<text>']
ham finish <runId>
```

`ham say <runId> -` reads text from standard input.
`<model>` is a model id or alias: opus, sonnet, sol, astra, glm, fable.
It must be one of the models the question lists.

| Option | What it does |
| --- | --- |
| `--request <id>` | Names the question. By default, the command reads events to find the open question. |
| `--text <text>` | Gives the text when it starts with a dash. `ham say`, `ham new-session` and `ham try-model` take it. |
| `--no-wait` | Returns once the answer is queued. |
| `--json` | Prints one JSON line. |

Each command finds the open question and sends your answer.
It waits up to 20 seconds for Cline's acknowledgement.
Without acknowledgement within 12 seconds, it sends the same answer once more.
The question id makes the answer happen once.
A 200 or 202 from the service means queued, not applied.

| Exit code | Meaning |
| --- | --- |
| 0 | Cline took the answer, or it was queued with `--no-wait`. |
| 1 | An error, or no acknowledgement. |
| 2 | Usage error. |

```text
$ ham say 01M4ZHFBV5SDX7J92E6YRH4QTW 'Run the sleeps in the background and poll the log.'
Cline took your message (same session).
$ ham try-model 01M4ZHFBV5SDX7J92E6YRH4QTW opus
Switched to Claude Opus 5.5. Cline took the fixed continue message.
$ ham new-session 01M4ZHFBV5SDX7J92E6YRH4QTW
Cline took your request: a fresh session starts on the same files.
$ ham finish 01M4ZHFBV5SDX7J92E6YRH4QTW
The run will finish with what exists.
$ ham say 01M4ZHFBV5SDX7J92E6YRH4QTW 'again'
ham: run 01M4ZHFBV5SDX7J92E6YRH4QTW is not waiting for you. Its last question was answered with finish.
```

Only the run's owner can answer.
Each question has an id.
Your answer names that id.
A late or repeated answer cannot act twice.
A second, different answer gets "this question was already answered with <action>".
An answer after the window closes is refused.
An answer for a finished run gets "run-finished".

### How long a run waits

The interactive `ham` terminal app asks for up to 30 minutes.
You can change the wait with these settings:

| Setting | How to use it |
| --- | --- |
| `--input-wait <minutes>` | Add it to `ham "<prompt>" --direct`. Use whole minutes, 0 to 43200. |
| `input_wait_minutes` | Use `ham config set input_wait_minutes 10`. |
| `HAMH_INPUT_WAIT` | Set this environment variable. |

The order is flag, then `HAMH_INPUT_WAIT`, then config, then default.
The terminal app default is 30 minutes.
Commands without a terminal default to 0, including scripts.
A value of 0 means no question.
The client sends nothing for 0, and the service default is also no question.
The automatic remedies of the next paragraphs still run.

The run's time limit is 7 hours 45 minutes.
The window uses the smaller of two amounts.
The first amount is your wait.
The second is the run's remaining time less 15 minutes.
A wait of 1 minute is honoured as 1 minute.

The 5 minute minimum concerns the run's remaining life.
Subtract the 15 minute margin from that remaining life.
With less than 5 minutes left afterward, no question opens.

The window starts when the question opens.
The panel counts the minutes left on your own clock.
It uses the expiry time agentd sent.

A wait value of 0 means no question.
The automatic remedies still run.

Class 4 means a refused conversation.
The provider may reject the saved history.
The conversation may no longer fit the context.
A class 4 stop gets one automatic fresh session.
A dead Cline process also gets one automatic fresh session.
After 15 minutes of silence, the harness resumes once.
These remedies need no window because they ask nobody.

A run asks at most 3 times.
You may request at most 3 fresh chats in one run.
Separately, the harness may start 1 fresh chat on its own.

### When nobody answers

When the answer window ends, the run finishes with what exists.
The original stop label stays.
For example: "Cline stopped: it ended its turn without changing a file".

The detail begins:
```text
Nobody answered in N min, so the run finished with what exists.
```
`N` is the time actually waited, at least 1 min.

Press `f` twice or use `ham finish` to finish.
The original stop label stays.
The detail begins "You chose to finish."

Use `ham cancel` or press Esc and then `c` twice to cancel.
The label becomes "Cline stopped: you stopped the run".

Delivery includes the commit and the zip.
Repository runs also deliver the pull request.

Closed questions stay in the run's history.
Open questions stay visible when you return to the run.

If chat cannot continue after a fresh session fails, the detail adds:
```text
A fresh session could not start.
```

### The fresh session

A fresh session starts a new Cline chat.
It uses the same work tree.
The files stay.
Cline's earlier history is left behind.

The harness sends a new-session request to the running Cline process.
It restarts the process only when Cline died or hung.
The fresh session uses the model chosen when you started the run.
The route ladder starts again at that model.

The harness starts one fresh session on its own per run.
A provider rejecting the saved conversation can trigger it.
A conversation exceeding the context window can trigger it.
A dead Cline process can trigger it.
Every hosted run gets this remedy, including a wait of 0.
Cline stops its loop after 5 identical tool calls in a row.
Cline also stops after 6 failed tool calls in a row.
Those stops never trigger automatic fresh sessions, which would hide loops.
The harness resumes once, then asks when the wait allows.
After 15 minutes of silence, it cancels the prompt and resumes once.
A second silence asks you when the wait allows.

Cline receives these details in a fresh session:
- The original task's head and tail.
- Why the last session stopped.
- The attempt count and tool call count.
- The git log and diff summary since the base commit.
- The changed files.
- The titles of the last 30 tool calls.
- Cline's last message.
- A top-level `PROGRESS.md`, if one exists.
- The tail of a failing test run.
- The tail of Cline's error output.
- The text you typed.

The details come from facts the harness recorded.
Secrets are removed.
The summary uses at most about 12,000 tokens (36,000 bytes).
An oversized summary loses whole sections, starting with the last.

The prompt contains a short part, at most 6,000 bytes.
Full notes stay in a microVM directory beside the work tree.
That directory is outside the repository.
The notes never enter the commit, zip, or pull request.
An audit copy goes to S3:
`runs/<runId>/agents/direct/turns/1/fresh-session-<n>.md`
That copy is not delivered.

The dashboard shows `Session 2 · restarted from the work tree`.
The number counts sessions.
`ham watch` prints `restarted from the work tree: session 2`.
The card and `summary.json` add up the tokens of every prompt that answered, across all sessions.

### When every model route is down

After every route stays down for 30 minutes, one notice appears.
Its text is `Every model route is down`.
It needs no answer and has no keys.
The run keeps waiting and probing until its time limit.
Use `ham cancel` or press Esc and then `c` twice to stop.
The run delivers what exists.
The notice closes when a route answers again.

### What the record shows

`summary.json` is in the zip's evidence.
It also sits under `runs/<runId>/agents/direct/turns/1/`.
It lists `interventions`, records of questions.
Each records the number, reason, and time open.
Each also records how it ended and the action.
It records your text's byte length.
It never records your text.
It lists `freshSessions`, records of fresh sessions.
Each records the number, reason, and trigger: `harness` or `user`.
Each records whether the process restarted and the digest size.
At most 8 entries of each kind are listed.

Attempts have two kinds besides the first prompt:
- `instruction`: first prompt after your message.
- `fresh_session`: first prompt of a fresh chat.

These are `turn.progress` events, shown by `ham watch --json`:
- `needs-input`
- `input-received`
- `input-ignored`
- `input-closed`
- `new-session`

Your text stays only in the control item and Cline's chat.
That chat is inside the microVM.
Your text is absent from `summary.json`, events, and logs.

At every stop, agentd writes a redacted record, also when the run has no answer window.
It contains the last 40 tool calls, with trimmed inputs and outputs.
It contains Cline's last 3 messages.
It also contains counts of identical and failed calls.
Its size is at most 64 KiB.
It goes to the stage bucket at:
`runs/<runId>/agents/direct/turns/1/stop-digest-<n>.json`
It helps people investigate a stop.
`ham` does not download it.

### Notifications and follow-up runs

Waiting runs have no email, Slack, or phone notification.
Notifications use the bell, dashboard, `ham watch`, and `ham runs`.
Keep the dashboard or `ham watch` open.
You can also check `ham runs`.

A run that ends is over: you cannot type a new prompt into the same session. To go on from the delivered code, start a follow-up run with `ham continue <runId> '<prompt>'`, or type your text in the run screen of the finished run and press Enter twice (see "The message box" and "Start a follow-up run"). A follow-up run is a new run.

## The goal loop

Your first prompt goes to Cline exactly as you typed it. Cline's goal plugin then runs the goal loop by default. When Cline's first run ends, the plugin sends follow-up prompts by itself and asks Cline whether the goal is complete. The goal is short. If your repository has a goal card (the lifecycle pack writes it as GOAL.md in `.ham/lifecycle`, or where `.ham/records` says), its numbered checks are the goal. Your run stays open while Cline works through the prompts, and `ham watch` keeps showing Cline's tool calls and messages.

The loop ends when Cline calls its goal-complete tool after one of those prompts and that round ends with a message. The card then says “Cline reported done”, which does not say the work is good.

The harness ends a run on its own only when the loop stalls. It never counts rounds or tokens. These are the stalls. The card and the detail name each one by a rule word:

- `blocked-twice`: Cline's last line is the same `BLOCKED:` line in two rounds in a row.
- `text-only-stall`: Three rounds in a row have no tool call and no changed file.
- `no-progress`: Five rounds in a row changed no file.
- `dead-loop`: The loop stopped and two continue messages in a row changed nothing. A loop that stopped gets one continue message first.
- `unfinished-claim`: Cline called its goal-complete tool, but the round did not end with a message.
- `goal-cleared`: The goal left the loop's state and Cline never completed it.
- `round-no-progress`: Inside one long round, Cline makes 30 tool calls in a row with no changed file and no new text, or 20 minutes pass with no changed file after at least 10 tool calls. A loop that polls this way never ends a round, so the round rules cannot see it. The detail reads “The harness ended the run: Cline made no progress in 30 tool calls in a row: no file changed and no new text appeared. Next: ham pull <runId> or ham continue <runId> '<prompt>'.” One long build or test run is one or two calls and is never cut. Cline can only wait for a test run of 25 minutes by starting it in the background and polling it with `sleep` and `tail` calls, so while the harness sees a command running, the calls that come after it started do not count and their time is left out of the 20 minutes. It sees a command as processes of Cline's user, other than Cline itself and what ran when the session began, that use at least 5 percent of one CPU, and it counts a call as a wait only when those processes began at least five seconds before the call. It counts the CPU of the processes and of the children they waited for, so a test run that starts a short-lived process for every test is seen. A job does not use the CPU evenly: a test run can wait for one timeout after another. So the harness also asks whether the processes used that share of a CPU over the last 30 minutes (or the stall window when that is longer; a stage changes it with `AGENTD_GOAL_COMMAND_WINDOW_S`, and 0 judges each moment alone; no key of yours makes it shorter than 30 minutes, `run.stall_window_minutes` above 30 makes it longer and `run.command_grace_minutes` 0 turns the look off), and a job that did its work and then does nothing for that long is no longer seen as running. A model that reads and plans before it edits makes no file change for as long as the task is large, so the harness also leaves out a tool call that brought something new: its input is unlike that of every earlier call of the run, its answer has at least 3 lines that no earlier answer had (the echo of the command that the shell tool puts back into its answer is not part of the answer), and the call succeeded. Up to 100 such calls between two changed files are left out (`run.stall_new_calls`, `AGENTD_GOAL_NEW_CALLS`; 0 counts every call, as before), and a changed file gives the allowance back. A call that repeats an earlier one, a call whose answer an earlier call already got or differs from it in a line or two (a poll that words its check differently each time and gets the same answer, or one that prints the time), a failed call, a call whose answer the engine did not send and every call beyond the allowance are counted, so a loop of identical reads, a poll of an idle session and a run that keeps failing end as they did. The stop text for the 20 minutes says how many calls were counted: “Cline made no progress in 20 minutes: no file changed and 12 of 62 tool calls counted as no progress”. The run's events say when the rule is held back for a command and when it counts calls again. A command that a call starts and waits for is Cline's own work and its call counts, so a loop that runs the failing test suite again and again is still ended at 30 calls or 20 minutes. A command that waits on the network and uses almost no CPU for longer than that window looks idle. A job that uses CPU and was started earlier also protects a later call that has nothing to do with it, until the two-hour limit below ends the run. A command does not hold the run for ever: after two hours with no changed file the run ends with “… made no progress in 2 hours: a command was running, but no file changed.” While Cline waits for helper agents (calls of its `spawn_agent` tool that have not been answered), the harness pauses the 20 minutes and counts none of the calls that arrive. The reviewers of the lifecycle review read files for longer than the window, they cannot change the tree, and their calls arrive on the same stream as Cline's own, so a run that is waiting for them is not polling. The pause has two bounds. The two hours count from the last changed file whatever the helpers do (`run.command_grace_minutes` moves them; 0 gives the stop of the 20 minutes back), and a helper call that is not answered by then ends the run with “… a helper agent call stayed open, but no file changed.” A call that repeats an input more often than once for each helper begun since the last changed file, plus once, ends the run after 30 such calls with “… a helper agent kept repeating the same calls, and no file changed.” When the last helper call is answered the 20 minutes go on from the time that was not paused. When this rule ends a run, Cline first gets one last turn of at most two minutes to write its final message; its answer is the run's final message and the stop text and label are the rule's. A long build can only be waited for with short commands such as `sleep 60; tail build.log`, and a cold build of the ham-e2e tests takes 36 minutes on two vCPUs. So the harness also reads the busy time of the machine (the user, nice and system times of `/proc/stat`) and takes off what agentd and Cline's own process used; what is left is what commands, builds and tests used. When that was at least 50 percent of one CPU (`AGENTD_STALL_BUSY_PCT`, 0 to 1000; 0 switches this signal off) over the stretch being judged, the stretch counts as progress and both counts start again. The stretch is the window of 20 minutes, or for the 30 calls the time since a file last changed; at least 30 seconds of readings are needed, so a burst of calls is not excused. The run's events say when the rule is held back for the CPU. A stop that the rule still makes says in its detail whether the CPU was idle or how busy it was (“CPU idle.”, “CPU busy at 30 percent.”), how long helper agents were open and how many calls came in that time, and ends with the commands to run next: `ham pull <runId>` for the files as they were when the run ended and `ham continue <runId> '<prompt>'` for a follow-up run. The label carries the first sentence only. The CPU cannot tell a wait from repeated work, so a loop that runs a failing test suite again and again counts as progress until the two-hour limit below. The signal is off when `run.command_grace_minutes` is 0, when the stage's `AGENTD_GOAL_COMMAND` is off, and where Cline has no user of its own. You move the limits with the keys `run.stall_calls` (30), `run.stall_window_minutes` (20), `run.stall_min_calls` (10), `run.command_grace_minutes` (120; 0 does not look for running commands) and `run.final_message` (on) in `config.toml` (see “Your settings”). An operator moves the stage's defaults with `AGENTD_GOAL_ROUND_CALLS`, `AGENTD_GOAL_ROUND_WINDOW_S`, `AGENTD_GOAL_ROUND_MIN_CALLS`, `AGENTD_GOAL_COMMAND_GRACE_S`, `AGENTD_GOAL_COMMAND`, `AGENTD_GOAL_COMMAND_WINDOW_S`, `AGENTD_GOAL_FINAL` and `AGENTD_GOAL_FINAL_S`.

The detail of such a stop begins “The harness ended the run:” and names the rule, and it quotes Cline's own `BLOCKED:` line when there is one. The label of this stop reads “Cline stopped: the harness ended the run (…)”, with the first sentence of the detail, the rule, in the brackets (see below). The 900-second silence limit and the run deadline still apply during the loop.

If Cline does not claim the goal, or the plugin files do not match their lock, you get a plain run: one prompt, as before. The operator can switch the loop off for a whole stage with `AGENTD_GOAL=off`. You can switch it off for one run with `--goal off` (see "Options for one run" under "Your rules").

When the harness ends a run by one of the stalls above, the run screen shows a card titled "goal loop stopped by the harness" and `ham watch` prints the same lines once. The card names the rule, says how many rounds the loop ran and how the last one ended, shows Cline's own `BLOCKED:` line when there is one, and gives the command that starts a follow-up from the code the run delivered: `ham continue <runId> '<what Cline should do next>'`. The card is built from the goal loop's own events. It never says the work was checked; Cline decides when its work is done.

### The goal loop on OpenCode

The harness drives the loop.
Each prompt is one round.
After a round without a claim, the harness sends "Are you sure you have completed the goal?"
It then sends a continue prompt.
There is no cap on rounds.

The claim line `GOAL COMPLETE <nonce>: <summary>` is the last line of the final message.
The nonce is made for the run.
It appears only in the harness's own prompts.
The claim line counts only in a round started by one of those prompts.
Text in a repository file or a web page is no claim.

The stall rules work as they do for Cline.
The helper-agent wait works as it does for Cline.
The card reads "OpenCode reported done" when a claim ends the turn with a message and at least one changed file.
This is the same condition as Cline's.

OpenCode's own `/goal` command is not used because over `opencode acp` it returns at once without reporting the loop, keeps its state only in the engine's database, and pauses itself after 50 rounds.

## Reading the outcome label

The service records how Cline stopped. It writes one label as frozen text. The label does not judge the work.

Only one label says “Cline reported done”. It requires the stop reason `end_turn`. Cline's final message must also contain some text. At least one file must also have changed. “Cline stopped: it ended its turn without changing a file” means no file changed.

The harness reads no message, so Cline giving up after it changed at least one file still reads “Cline reported done”. Read Cline's last message in `EVIDENCE.md` before you trust the label.

### Where to read it

- The first line of the pull request description.
- The “Outcome” row in `EVIDENCE.md` inside the zip.
- The `label` field in `evidence/direct.json` inside the zip. Its `outcome` field holds the wire word.
- The `run.completed` event, which the service stores with the run. Its payload has `outcome`, `label` and `detail` fields.
- The Outcome line of the `ham` card, the end of the last status line of `ham watch` (`outcome: ...`), and the `label`, `outcome` and `detail` fields of the `run` object of `ham watch --json` (`ham pull --json` has them at the top level). `ham` prints the label exactly as the service wrote it.

### The 15 labels

| Label | When it appears |
| --- | --- |
| Cline reported done | Cline ended its turn normally and wrote a final message. At least one file changed. |
| Cline did not start | Something failed before the prompt was sent. The request fetch or repository restore failed. Engine setup or the rules check failed. Or Cline did not report the requested model. |
| Cline stopped: it ended its turn without a message | Cline ended its turn normally with an empty final message. |
| Cline stopped: it reached the output limit | The model reached its maximum output length. |
| Cline stopped: it reached its request limit | Cline reached its request limit for one turn. |
| Cline stopped: the model refused | The model refused. |
| Cline stopped: you stopped the run | You cancelled the run. Or Cline ignored cancellation for 10 seconds. |
| Cline stopped: Cline ended the run itself | Cline ended the run without being asked. Its loop guard stops after 5 identical tool calls in a row, and its mistake limit stops after 6 failed tool calls in a row. The harness resumes the same session once on the same model, never switches the model, and then asks you what to do. The detail names which of the two stops it was. |
| Cline stopped: the model provider failed | Bedrock, the model provider, returned an error after Cline's retries, after the harness resumed the work up to three times and after it tried the other routes. See "When the model provider fails". |
| Cline stopped: the Cline process failed | Cline closed its output or broke the protocol. |
| Cline stopped: no output for the silence limit | Cline produced no output for 900 seconds (15 minutes). Longer thinking without output is cut off. |
| Cline stopped: the run reached its time limit | The deadline arrived: up to 27,900 seconds, 7 hours 45 minutes. |
| Cline stopped: the engine reported an unknown stop reason | Any other stop reason. |
| Cline stopped: the harness ended the run (…) | The goal loop's stall rules ended the run (see “The goal loop”), or the session's disk was full (see “When the session disk is full”). The brackets hold the rule or the cause; the detail begins “The harness ended the run:”. For a stall it quotes Cline's own `BLOCKED:` line when there is one. |
| Cline stopped: the session was lost | The session missed its heartbeat for 10 minutes, or workflow state failed, and no summary arrived. The service delivers the last saved snapshot, and the detail says so. With no snapshot yet, the run ends blocked and delivers no zip. |
| Cline stopped: it ended its turn without changing a file | Cline ended its turn normally with a final message, but no file differs between the base commit and the delivered head. The zip holds the unchanged repository and no pull request is opened. |

A short detail follows the label. It comes from Cline or the harness, the service's run machinery. The detail has at most 300 characters. AWS key ids and session secrets are removed.

### Observations beside the label

The harness can also record a fact it read at the end of a run, next to the label. It is a recorded observation. It never says the work is good, and the label does not change. There is one: `last_log_entry_red`. The lifecycle pack asks Cline to log every test run in the file `LOG.md` in `.ham/lifecycle`, or where `.ham/records` says. When the last entry of that log is a run that exited with a non-zero code, or that reported failed tests while its note does not say `PASS` for itself, the harness records the sentence “The last entry of the run log is a red run: pytest exited 1 and 2 failed.” in the run's summary (`observations`) and in the `run.completed` event. A green last entry, an entry that says the tests were not run, or no log records nothing. The sentence is a fact about the log. It does not say that the delivered code fails or passes.

Cline can stop halfway while the run still ends `complete`. When something was saved, the zip exists and exit is 0. The label tells you how Cline stopped.

A run ends `blocked` only when nothing could be delivered. Its exit code is 12.

## Getting your code

### Pull the zip

Download the newest zip with:

```bash
ham pull <runId>
```

The newest zip is the default artifact. Pull downloads it in 16 MiB ranges, 4 at a time. Downloads first go into `<file>.part`. One download at a time writes that file, and a lock file `<file>.part.lock` beside it shows which. A second download of the same artifact, in another shell or from the terminal app, waits for the first and then keeps the file the first one checked.

Repeat the same command to resume an unchanged file. Pull checks SHA-256 against the stored value before renaming it. An expired link returning `HTTP 403` is replaced once. Pull then prints the card.

These flags select other artifacts:

| Flag | What it downloads |
| --- | --- |
| `--artifact evidence` | Evidence zip: `EVIDENCE.md`, `evidence/` and the manifest. No source. |
| `--artifact bundle:<taskId>` | Not used by current runs. |
| `--artifact log:<agentId>` | Not used by current runs. |
| `--artifact partial` | Nothing on a direct run. |

If the file does not exist yet, pull prints:

```text
no matching artifact; the run has none yet
```

The source zip name is `<repo>-<runId>-<sha7>.zip`. The evidence zip name is `<repo>-<runId>-<sha7>-evidence.zip`.

Here, `<repo>` is the CodeCommit repository name, or `s3-only`.

### Pull the work in progress

During a run, the service saves snapshots of the work tree. It saves them about every 10 minutes, when files changed. A snapshot is a git bundle.

You download the newest snapshot with `ham pull <runId> --snapshot`. In the run's message box, `/pull --snapshot` does the same. The message-box command writes locally. It runs at once.

You get `<runId>-snapshot-<n>.bundle` in the download directory. `n` is the snapshot number within the run. Without a number, the name uses the commit's first 12 characters.

The download arrives in ranged pieces. Checks compare its size against the size named by the service. They also check the commit named by the service. The file gets its final name after the checks pass. A failed check leaves no file. The presigned link is never printed.

The output names the file, commit, base, size, and run's status. You also get this command to look at the bundle:

```text
git clone -b snapshot <file> <dir>
```

The output ends with this warning:

> "A worker snapshot, not checked by the harness: treat its contents as untrusted code and data."

If the run has ended, the output identifies the delivered result. You get that result as a zip with `ham pull <runId>`. The snapshot is separate from that result.

The snapshot record can lag behind the bundle. When this happens, the output says so. It shows the bundle's own commit.

With `--json`, you get one object containing `runId` and `snapshot`. The `snapshot` object contains these keys:

- `path`
- `headSha`
- `baseSha`, when known
- `takenAt`
- `count`, when known
- `sizeBytes`
- `runStatus`
- `recordStale`
- `note`

You cannot use `--snapshot` and `--artifact` together. This causes a usage error with exit code 2.

A run without a snapshot returns exit code 1. The service gives this sentence:

> "no snapshot has been saved for this run yet; agentd saves one about every 10 minutes while the files change"

### What is in the zip

Everything sits under one folder: `<repo>-<runId>-<sha7>/`.

| Path | Contents |
| --- | --- |
| `src/` | Final source tree, exactly as committed |
| `repo.bundle` | Full git history of branch `harness/<runId>` |
| `EVIDENCE.md` | Run details, prompt and Cline's last message |
| `evidence/` | Evidence files, including `evidence/direct.json` |
| `MANIFEST.sha256` | File checksums |

When the run wrote a DOX docs tree, `src/` holds it: the root `AGENTS.md`, the child docs and `THIRD-PARTY-NOTICES.md`. See [DOX docs in every project](#dox-docs-in-every-project).

When the stage had the tool layer on, `evidence/direct.json` holds the `tools` block of the run summary. See [Tools the harness gives Cline](#tools-the-harness-gives-cline).

The `EVIDENCE.md` table has one row each for the run id, repository, base, head, pull request, outcome label, detail, Cline's stop reason, model, Cline version, rules (name and the first 12 characters of each sha256), wall time and tokens.

The tokens row adds up four kinds of token and shows each one by name, then the total:

```text
| Tokens | total 316198 = input 22 (not cached) + output 5718 + cache read 273073 + cache write 37385 |
```

`input` counts only the tokens the model read without a cache. `cache read` counts tokens it read from the prompt cache, and `cache write` counts tokens it stored there. `output` counts the tokens it wrote. The total is the sum of the four, the same figure the run card shows on its Tokens line. A run for which the stage reported no usage has no tokens row.

It also states whether delivery used the final bundle or last snapshot. The table is followed by the objective, meaning your prompt. Cline's last message comes next. When the run has a test report, the section `Clean-checkout test report` follows (see below).

The file also includes this line when the run has no test report:

```text
The harness hosted the session, committed what Cline left in the tree, scanned the commits for secrets and packaged this zip. It ran no test and made no judgment on the work.
```

To check the files, unpack the zip, change into its top folder and run:

```bash
sha256sum -c MANIFEST.sha256
```

The same inputs give the same zip bytes.

### The test report

When the harness can find your project's test commands, the zip also holds `evidence/test-report.json`. After Cline ends, the worker clones the delivered head into a fresh directory and runs the repository's own test commands there, as the unprivileged user, with the session's own network limits. It looks for the commands in this order and uses the first source that has one: the project's `.ham/test-command` (one command per line, `#` comments, `setup: <command>` for an install line); the test scripts of the repository that the log or the card ran (for example `bash <script>`), run as `bash <script>` from the root in the order unit, integration, end to end, where a script named `test-all` stands for the others and a prefix that names a program or tool folder a clean checkout does not have (`PYTHON=.venv/bin/python`) is left off; the last whole-suite test runs Cline logged in the lifecycle log `LOG.md` (in `.ham/lifecycle`, or the `lifecycle` folder `.ham/records` names; with `off` there is none); the numbered checks in the goal card `GOAL.md` in the same folder; the files at the root of the tree (a test script, pytest settings, the `package.json` test script, `go.mod`, `Cargo.toml`, `pom.xml`, a Gradle wrapper); and a Python layout with no settings at all, test files named `test_*.py` or `*_test.py` at the root or in a `tests` or `test` folder. Such a layout gets `python3 -m pytest -q` when a file imports pytest or holds plain test functions, and `python3 -m unittest discover -v` when every file uses unittest (with `-s tests` when that folder has no `__init__.py`). When unittest files are in more than one place, such as the root and a `tests` folder with no `__init__.py`, the report runs one discovery for each place, as a command of its own. It uses `python3 -m pytest -q` instead when a unittest file is named `*_test.py`. It runs only commands it recognises as test runners.

The report installs the project's packages before it runs the tests. Each install appears as a setup step in `evidence/test-report.json`. For a project with `package.json` and `package-lock.json`, the install step is `npm ci`. For a project with `package.json` and `pnpm-lock.yaml`, the install step is `pnpm install --frozen-lockfile`. With no lockfile, the step is `npm install`, but a command from the lifecycle log or the goal card gets this step only if `package.json` names a package and the project has no `yarn.lock` or bun lockfile. For a Python project with `requirements.txt`, the report installs its packages into its own virtual environment named `.ham-venv` in the checkout. The Python tests run with the Python of that environment. These install steps also apply to test commands from the lifecycle log or the goal card and to Python layouts with no settings. `npm test` from the log runs after `npm ci`, and `python3 -m pytest -q` from the log is recorded and run as `.ham-venv/bin/python -m pytest -q`. No install step is added to commands that work in another folder, such as `cd web && npm test` or `npm test --prefix web`, commands in `.ham/test-command`, or the repository's own test scripts. A `setup: <command>` line in `.ham/test-command` names an install step. When an install fails, the `Tests` line names it, for example: `` Setup `npm ci` exited 1 in a clean checkout, so the test command did not run. `` The test commands behind that step appear as `NOT RUN` in the report, and no test counts as passed or failed. For Python test files with no pytest settings, the report does not run the tests when `pyproject.toml`, `Pipfile`, `setup.py`, `setup.cfg` or a requirements file besides `requirements.txt` names packages that it does not install. Such a requirements file is a `.txt` file at the root whose name holds `requirements`, for example `requirements-dev.txt` or `test-requirements.txt`, or a `.txt` file in a `requirements` folder. The sentence then reads `The clean-checkout test report did not run the tests: it found Python test files (tests/test_app.py), and pyproject.toml names packages that the report does not install (it installs only the packages of requirements.txt); a .ham/test-command file in the repository names the install and test commands.` A `.ham/test-command` file with a `setup:` line and the test command makes the report run them.

For each command the file records the command, its exit code, the pass, fail and skip counts the test runner printed, and the end of its output, up to 256 KiB for a command (a command that printed more keeps its last bytes and the file marks it as cut; the stage variable `AGENTD_REPORT_TAIL_KIB` changes the size). EVIDENCE.md has a section with one sentence, `ham pull`, `ham watch` and the delivery card of the terminal app show it on a `Tests` line (the terminal app reads it from the zip once the download ends), and a pull request gets it as a comment. The only claim is `repository tests pass in a clean checkout: N of N`. It appears when every command exited 0, the counts were read, nothing failed and at least one test passed; N is the number of tests that passed, and skipped tests are counted apart. Anything else is a plain sentence of what happened, for example `` `python3 -m pytest -q` exited 1 in a clean checkout; the runner reported 18 passed, 2 failed. ``

The commands run with the same package settings (Maven, pip, npm, Go) and the same container runtime as Cline's own session, and with no credential.
When the repository has a Compose file at its root (`compose.yaml`, `compose.yml`, `docker-compose.yml` or `docker-compose.yaml`) that declares a `postgres` or a `rabbitmq` image, the worker has no Docker daemon to run it, so the report starts the worker image's own local server for it (`ham-services postgres` or `ham-services rabbitmq`, on 127.0.0.1) before the commands and stops it after the last one. Each is a setup step in the file. Its output starts with a line that says which Compose service the local server stands in for and that the Compose container did not run. The commands get `DATABASE_URL`, `BROKER_URL` and `BROKER_PASSWORD` only when the repository's own files name that variable. A server that cannot start is a step shown as `NOT RUN` with the reason, and the commands after it still run. When every command passes with such a server, the sentence adds that the report started a local server in place of the Compose service. A command that exits with a code other than 0, whose output says a connection to the port of a declared server on this machine was refused, while that server was not reachable, keeps its exit code, its counts and its output. Its output starts with a line `NEEDS: postgres: <why>`, the sentence reads `` `bash <script>` exited 1 in a clean checkout while postgres was not available; the runner reported 0 passed, 6 failed. `` and EVIDENCE.md lists each step with what it ran without. The report does not call such a command not run, and it never calls a failed command passed. A command that fails while the server answers is a failed command. A command that needs a container runtime the session does not have is shown as `NOT RUN` with the reason; it is not a failure, and the report does not claim the tests pass while one was not run.
The same holds for a command whose program the worker does not have.
A command that fills the disk of the session is shown as `NOT RUN` too, with the line the tool printed, for example `` NOT RUN: `cargo test` (ran out of disk: couldn't create a temp dir: No space left on device (os error 28)). `` It needs an exit code other than 0, the words `No space left on device` in the output and no failed test reported by the runner. The commands after it are listed as not run, because the disk is still full, and the report says nothing about whether your tests pass.
The worker's tool set has no Gradle, so a repository with a `build.gradle` whose change touched no Gradle code gets `gradle test` from the manifest, the shell answers that `gradle` is not found with exit code 127, and the card reads `NOT RUN: gradle is not installed here` and not "exited 127".
Only a command that starts with the missing program is read this way.
A repository script that runs a missing tool inside, a file of your repository that is not there, and any other exit code stay failures.
When no source names a command and the root `Cargo.toml` is a workspace of more than one member, the harness picks `cargo test`, which runs the tests of every member. EVIDENCE.md says so in a paragraph under the sentence: `The repository names no test command, so the harness chose `cargo test` from the manifest (Cargo.toml). It runs the tests of the whole workspace, 14 members.` A manifest with a `[package]` runs that package alone, so it gets no such paragraph.
To make the report run the commands you want, list them in `.ham/test-command`: it is the first source, and the report does not read `AGENTS.md`. The same file names the command of any layout the list above does not know.
When no source gives a command, the sentence `No test command was found, so no clean-checkout test report was made` goes on with the list of where the report looked (the test command file, the run log, the goal card and each layout above) and says that a `.ham/test-command` file in the repository names the command.

While the report runs, the screen says so. The run shows `active (running the repository's tests)` in place of `active (green)`, and the live row says which command of how many is running and how long it may run, for example `Running the repository's tests: command 2 of 2 (up to 5 min).` The stored phase of the run, which `ham runs` shows, does not change.

Each command may run for 5 minutes. A command that goes over is stopped, and the `Tests` line says which one and how long it had: `` `python3 -m unittest test_docs_set` timed out after 300 s; the commands that finished are listed in the report. `` `ham pull`, `ham watch` and the terminal app add the setting that raises the limit. They name twice the longer of the time the command had and 5 minutes, up to 465, so the advice always allows more than the stage does now; a command stopped at 465 minutes gets none. A command stopped while the end of its output showed build lines (`Compiling`, `go: downloading`, `[INFO] Downloading`) and no line of a test that began says that instead. For cargo, the report knows the line that marks the start of tests. When a cargo command stops with build lines and no test-start line, and the report kept its whole output, the sentence is: `` `cargo test` was stopped at its limit of 300 s while it was still building: its output shows build work and no test that began, so this report says nothing about the tests. `` For other commands, and for an output whose start was cut off, the sentence is: `` `npm test` was stopped at its limit of 300 s; the end of its output shows build work, and the report cannot tell whether a test began, so it says nothing about the tests. `` Neither case counts as passed or failed. A narrower command in `.ham/test-command` or a longer limit lets the tests run. EVIDENCE.md has a paragraph of its own for a stopped command: `` `cargo test` was stopped at the limit of 300 s. `` It names the setting `run.report_command_minutes` and the stage variable `AGENTD_REPORT_STEP_S` that set the limit. Set it with `ham config set run.report_command_minutes 10`: whole minutes, 1 to 465 (as long as a session lives). The value 0, or no value, leaves the stage's own limit, which is 5 minutes unless the stage says otherwise. A longer limit lengthens the whole report by the same time, and the report never gets more than the run's own time limit allows.

The report records what the repository's own commands did in a clean copy of the delivered head. It never changes the outcome label or the exit code of `ham`, and it does not say the tests are meaningful. It is on by default. Switch it off with `ham config set report_tests off`; `ham config unset report_tests` turns it on again. With the default limit the report runs for at most 8 minutes after Cline ends, and a run you cancel or one that ran out of time makes no report, so `ham cancel` does not wait for it.

### Clone the repository

Create a local git repository with:

```bash
ham clone <runId> [dir]
```

Clone downloads the zip the same way pull does. It reads only `repo.bundle` to build the repository. The default directory is `./<repo name>`.

Clone compares the tree with the zip's `src/`. Every file's path, blob id and mode must match.

Clone needs git that runs. The destination must not exist or must be empty. An empty destination cannot be the directory you stand in.

The clone has no remote. The zip stays in the download directory.

The output has this shape:

```text
cloned run <runId> into <dir>
  branch harness/<runId> at <12-character commit> (<n> commits, <n> files)
  tree equals the zip's src/ (path, blob id and mode of all <n> files)
  zip kept at <path> (sha256 <12 characters>)
  no remote is set. To publish it: git -C <dir> remote add origin <url> && git -C <dir> push -u origin harness/<runId>
  or push the run to another repository in one step, with your own git credentials: ham push <runId> <git-url>
```

The publishing command shown there is:

```bash
git -C <dir> remote add origin <url> && git -C <dir> push -u origin harness/<runId>
```

### Get the code into another repository

To put the run's code in a git repository on another server, push it:

```bash
ham push <runId> <git-url> [--branch <name>]
```

`ham push` does three things:

1. It downloads the run's zip like `ham pull` and `ham clone`. The zip stays in the download directory.
2. It builds a temporary repository from the zip's `repo.bundle` and checks the files against the zip's `src/` folder, as `ham clone` does. The temporary directory is named `.ham-push-<runId>-...` and sits beside the zip. It is removed when the command ends, also when the push fails.
3. It runs `git push` with your own git: your git configuration, your credential helper, your SSH agent and keys, and `GIT_SSH_COMMAND` if you set it. `ham` holds no token and asks for no password. Git may ask for a passphrase or password on your terminal, as it does for any push.

The run's branch, `harness/<runId>`, goes to the remote branch of the same name. `--branch <name>` picks another remote branch name, for example `--branch main`. The push is never forced. If the remote branch already has other history, git rejects the push; push to a new branch name instead. No remote is added to any repository, so the URL is stored nowhere.

The URL can be `https://host/group/repo.git`, `http://host/...`, `ssh://git@host/group/repo.git`, `git@host:group/repo.git`, `git://host/...`, `file:///path/repo.git` or a path on your machine. A relative path starts from the directory where you run `ham`.

`ham push` refuses a URL that carries a credential, such as `https://user:<password>@host/...`, `https://<token>@host/...` or `ssh://user:<password>@host/...`. It exits 2 before it downloads anything, and its message does not repeat the URL. Give the plain URL and let git find your credentials. It also refuses a URL that starts with a dash, a transport helper such as `ext::...`, other schemes, and spaces or control characters.

`ham push` has no GitHub or GitLab code and uses no API token. It works with any git server. It needs git that runs, like `ham clone`.

The result is on standard output:

```text
pushed run <runId> to <url> as branch <branch>
  head <12-character commit> (<n> commits, <n> files), built from the zip's repo.bundle
  the temporary repository beside the zip was removed; the zip is kept at <path> (sha256 <12 characters>)
```

Git's own lines go to standard error, so a script reads only the result:

```text
To <url>
 * [new branch]      harness/<runId> -> harness/<runId>
```

With `--json`, standard output is one object with the fields `runId`, `url`, `branch`, `pushed` (true), `head`, `commits`, `files`, `partial` and `zip` (`path`, `sha256`, `artifactId`). A blocked run's partial zip is pushed too. A notice on standard error says so (`ham: run <runId> did not complete: pushing its partial delivery, the work so far and not a finished result`), and `partial` is true.

The exit codes are 0 for a push, 1 for an error (the run has no zip yet, git is missing, git rejected the push) and 2 for a usage error. When git fails, `ham` prints:

```text
ham: git push failed (exit <n>). ham never forces a push: if the branch already has other history, push to a new branch with --branch <name>. Otherwise check that the URL is right and that your own git credentials (your SSH key or credential helper) may write to it
```

Use `ham clone <runId>` if you only want a local repository and will add a remote yourself. Its output also names `ham push`.

### The download link

Get a presigned link for the newest zip with:

```bash
ham link <runId>
```

It prints:

```text
<url> (expires <time>)
```

The link lasts 15 minutes. It expires sooner if your credentials expire first.

Anyone holding the link can download until it expires. Treat the link as a secret.

### The pull request

Get the pull request link with:

```bash
ham pr <runId>
```

It prints:

```text
PR <url> (run <runId> <status>)
```

A completed run with a zip and no pull request prints:

```text
run <runId> has no PR by design (status complete): this run's delivery is the zip and a local git repository, with no pull request; get the code with `ham pull <runId>` (the zip) or `ham clone <runId>` (a git repository on this machine)
```

A run still going prints:

```text
run <runId> has no PR yet (status active)
```

Only CodeCommit repositories support pull request delivery. The service requires the name to be covered by the stage's repository names. These names start with `hamh-e2e-` unless the operator chooses others at setup. The repository must be in the stage's own account and Region. It must also be on the stage's allowlist.

List the allowlist with:

```bash
ham doctor
```

`ham doctor` prints one line for each entry of the allowlist below its checks.
Only the operator, the identity that runs `./setup.sh` for the stage, may change the allowlist with `ham-admin`.
The full setup, `./setup.sh --full`, installs `ham-admin` beside `ham`.
You follow [Allow a repository for pull requests](../README.md#allow-a-repository-for-pull-requests) to use `ham-admin`.

The pull request goes from `harness/<runId>` into `main`. Its title starts with `ham: `. Your prompt's first line follows, cut at 120 characters.

The description starts with the outcome label. Next come the run id, model and wall time. Cline's last message follows as a quote, cut at 6,000 characters. The whole message goes in the first comment. Another line says the harness checked nothing in the pull request.

A run with `--s3-only` opens no pull request. An unchanged final tree also opens no pull request, and a tree that differs from the start only by the DOX starter the harness wrote counts as unchanged. Its detail says:

```text
no pull request: Cline left the repository unchanged
```

If opening the pull request fails, the zip still gets built. The run still completes, with this detail:

```text
pull request not opened: <reason>
```

GitHub and GitLab targets are not supported.

Before delivery, the service inspects the bundle. These findings block delivery and leave no zip:

| Finding | Check | Applies to |
| --- | --- | --- |
| Secret | gitleaks over new commits | Every run |
| Engine configuration change | `.cline/hooks/`, `.cline/plugins/`, `.cline/agents/`, `.clinerules/hooks/`, `opencode.json`, `opencode.jsonc`, `.opencode/` | Every run |
| Workflow file change | Any file under `.github/workflows/` | Runs that target a repository |
| Build specification change | `buildspec.yml` at the repository root | Runs that target a repository |
| Large file | Over 2 GB | Every run |
| Large bundle | Over 2 GiB | Every run |
| Disallowed path | Outside allowed paths | Every run |

Rule files named `.clinerules/*.md` and `.cline/rules/*.md` pass.

A git host runs workflow files. The pull request check runs the root `buildspec.yml`. So a run that targets a repository refuses a change to either.

A run with the zip target (`--s3-only`) delivers a workflow file or a root `buildspec.yml` in its zip. Nothing runs a zip. The run's detail names the file:

```text
The zip holds a workflow file (.github/workflows/ci.yml); it runs only if the code is pushed to a git host that runs such files.
```

`ham clone` and `ham push` use the zip as it is. `ham push` pushes such a file to the git URL you give, with your git credentials.

When a repository run is blocked for such a file and for nothing else, the reason names the file. It ends with the way to get all of the work back:

```text
To get all of this work as a zip, start a follow-up run with `ham continue <runId> --s3-only '<prompt>'`. A zip run delivers such a file.
```

The follow-up starts on the work the blocked run left. Its zip holds that work, the file included, and what your prompt adds. If the blocked run allowed third-party copies, the command in the reason carries `--allow-third-party` too.

The reason names a snapshot only when the run saved one. The service saves the first snapshot about 10 minutes after a session starts, and none when the run ends. A run that ended sooner has no snapshot. The last snapshot of a longer run is older than the work that was refused:

```text
The run's last snapshot was saved at <date> <time> UTC. Download it with `ham pull <runId> --snapshot`; it does not hold work done after that time.
```

When no follow-up brings the work back and there is no snapshot, the reason ends with `The run has no snapshot to download.`

A follow-up of a blocked run is checked from the commit the blocked run started from. So the work of the blocked run is checked again:

- A workflow file or a root `buildspec.yml`: a follow-up on the same repository is refused again while its result holds the file. A follow-up with `--s3-only` delivers the file in its zip.
- An engine configuration change: a follow-up is refused again while its result holds the change.
- A secret: a follow-up is refused again, because the commit that holds the secret stays in its history. The reason says `A follow-up of this run is refused for the same secret, which stays in its commits. Start a new run instead.`
- A third-party copy: a follow-up is refused again, because the check also reads the third-party files the blocked run downloaded. With `--allow-third-party`, the follow-up delivers the copy and reports it as a finding.

These checks concern delivery safety. They do not judge the quality of the work.

## Stopping a run and partial results

Cancel a run with:

```bash
ham cancel <runId>
```

`ham stop` is not a command. In the dashboard, press Esc, then `c`, then `c` again.

When Cline asks what to do, `ham finish <runId>` ends the run. You can also press Esc, then `f` twice. Finish keeps the original stop label. It adds "You chose to finish." to the detail. Cancel always gives the label "Cline stopped: you stopped the run".

On a direct run, cancellation asks the live Cline session to stop. Cline stops within about 10 seconds. The service keeps and delivers the work that exists. The run ends `complete`.

If cancellation fails, you may retry the command.

Without a live session, there is no work to keep. This also applies when the session has already ended. Cancellation then marks the run `cleared` and stops its workflow. The exit code is 15.

`ham` never asks to throw the work away.

An already finished run returns:

```text
HTTP 409: run <runId> is complete; it cannot change (run-finished)
```

A direct run has one zip and no separate "partial" zip. Early stops produce the normal zip from saved state. A stop with "it ended its turn without changing a file" delivers the unchanged repository and opens no pull request.

Early stops include cancellation, silence limits and time limits. They also include mistake limits, provider errors and lost sessions.

The service uses the final commit when the session can write it. Otherwise, it delivers the last saved snapshot. The detail and the pull request text then say that the work is the last saved snapshot, with the time it was taken. The service writes that time in UTC. `EVIDENCE.md` records the stop in its Outcome row. The pull request description also records it.

Snapshots happen every 10 minutes, only when files changed. A lost session can lose up to the last 10 minutes of file changes.

Without any saved state, the run ends `blocked` with no zip.
## Limits

| Limit | Value | What happens |
| --- | --- | --- |
| Request size | The prompt, rules, skills, attachments and the two prompt files together: at most 6,225,920 bytes, as the service receives them. The service takes 6 MiB per request (the AWS Lambda limit for a synchronous call) and keeps 64 KiB of that for headers. A quote, a backslash or a line break in the text counts twice. | `ham` refuses it before sending, and the service refuses it with `HTTP 413 payload-too-large`. The message names the limit, the largest part and, for files, the largest file. |
| Silence | 900 s with no output from Cline | Cline stops. Label: “no output for the silence limit”. Saved work is delivered. |
| Run time | Up to 27,900 s (7 h 45 min) per prompt. VM: at most 28,800 s (8 h). Whole run: 24 h by default, set with `run.max_run_hours` or `--max-run-hours`, 1 to 720 h. Whole workflow: 2,628,000 s. | At the deadline of a session, a run with time left continues in a new session. When the run has used its total time, Cline stops, the run is `paused` and the saved work is delivered. Label: “the run reached its time limit”. |
| Resume after 8 hours | Not supported | A lost VM is not resumed. A lost session delivers its last saved snapshot. |
| Containers in the session | Testcontainers works with the one image postgres:16 | The session's Docker is a Podman socket (`DOCKER_HOST`) with the one image postgres:16 and no registry. A Testcontainers test works only with `PostgreSQLContainer("postgres:16")`. Any other image fails at once and docker compose is not installed. |
| Languages in the session | Python 3.12, uv, Node 24 with npm and pnpm, ripgrep, Go 1.27.1, Amazon Corretto 21, Maven 3.9.16, RabbitMQ 3.13.7, PostgreSQL 16, MariaDB 11.4, Valkey 9 and mailpit, and the shell tools `diff`, `patch` and `jq` and the `pip` command | Go and Java projects build and test here. There is no Gradle or golangci-lint. |
| Checks a session cannot run | In both kinds of session `cargo llvm-cov` (Rust coverage), `checkov` and `sudo` are not installed, and no container registry or release host can be reached. In an `instances` session the engine runs as the user nobody (`id -u` prints 65534): programs that refuse root, `ham-services postgres` and SysV shared memory (`ipcmk`) work, and `docker` has no socket (a repository with a container signal is kept off this machine; see `run.container_signals`). A `microvm` session runs as `agent` (`id -u` prints 901), where Docker, `ham-services postgres` and shared memory work. | A scanner that pulls a container image or downloads a pinned release fails. Cline writes a NOT RUN line in its last message for each check it could not run, with the command and its message, runs every other check, and does not change the code to get around the limit; the default rules (`ham rules init`) say so. A test that fails at the base because of the session can fail for a new reason with Cline's change, even when the failing test names match. The default rules ask Cline to compare the first error line of each failing test as well as its name, and to read every error that differs, such as a missing module. |
| Rust in the session | The Rust toolchain 1.98.1 with cargo, clippy, rustfmt and nextest, and the gcc that links Rust programs | A Rust repository builds and tests here. Crates come through the package proxy from CodeArtifact. |
| Packages | Loopback proxy. pip, uv, npm and Maven paths go to the stage's CodeArtifact repository. Go paths go to the stage's Go module store, an S3 bucket the operator fills. | The proxy adds the CodeArtifact token and none for Go. A module that is not in the store is not available. For the npm CLI, the session writes public registry links (`https://registry.npmjs.org/...`) into the `resolved` fields of `package-lock.json`. pnpm writes no URL into `pnpm-lock.yaml`. A project that uses `uv.lock` or `yarn.lock` can have session addresses written into that file. |
| Network | Amazon Bedrock, `api.osv.dev` and package installs through the proxy | All other hosts are blocked. |
| Git hosts | CodeCommit only | GitHub and GitLab delivery are not supported. Use `--s3-only`, or pull and clone. |
| Provider failover | Resumes, then switches route or model, then waits | The harness resumes the same model up to three times after a Bedrock error, then switches to other routes, and when every route is down it waits and probes. See "When the model provider fails". |
| Questions per run | At most 3 | A fourth stop ends the run with the original label. |
| Fresh sessions | The harness starts 1 on its own. You may ask for 3. | A fourth request is refused with the reason "limit". |
| Answer window | 30 minutes from the terminal app, 0 from scripts. `--input-wait` takes 0 to 43200 minutes. | The window lasts up to the wait you set. It ends at least 15 minutes before the run's time limit. No question opens when the run has less than 5 minutes of life left after that margin. A wait of 1 minute is honoured. |
| Text of an answer or a message | At most 262,144 bytes (256 KiB). A run takes at most 4 MiB of message text in all. | The text is refused or, past the run's total, the message is refused with `run-limit`. |
| Copy to the clipboard | At most 64 KiB | A longer text is refused whole and never cut; `/export` keeps it. `ui.copy_limit_kib` changes the limit; the settings table names its range. |
| Changed file in a delivered run | At most 2 GB, the most CodeCommit takes over git | A larger file blocks the run's delivery. The bundle limit of 2 GiB applies as well. |
| Research | A search asks for up to 20 results and 300 domains. A fetch names up to 20 addresses. The prompt names up to 150 web addresses. | A request above the limit of the search provider in use is refused by that provider. |
| Stop digest | At most 64 KiB per stop, in S3 | An operator reads it. `ham` does not download it. |
| GPT-6 Astra | Blank tool argument names | GPT-6 Astra can return a tool call with a blank argument name. Bedrock refuses a blank key when it replays the conversation. The Cline build that the stage runs drops blank keys from replayed Bedrock tool inputs. A replay that Bedrock still refuses ends the run with “Cline stopped: the model provider failed” and a Bedrock error about the replayed conversation. |
| Smoke records | `ham doctor` states their age | Doctor states the age of the newest record in the model list and passes. It prints `WARN catalog` and exits 0 only when you add `--catalog-max-age <hours>` and the record is older. |
| Engines | Cline and OpenCode | A run uses one engine chosen at its start. |
| Rules | See the Rules table. | See the Rules table. |

`api.osv.dev` is the allowed vulnerability database.

Package installs use the loopback proxy.

A provider call can time out while a model is silent for a long time. The harness waits and resumes the work up to three times, and then it may switch routes. See "When the model provider fails".

Cline's own behaviour is unchanged. There is no nudge by default. The harness sets neither Plan nor Act mode. Sub-agents are on (see "Does a run use several agents?"). All tool calls are auto-approved.

A run uses one Cline session. By default Cline's goal plugin sends follow-up prompts inside that session (see "The goal loop"); with `--goal off` the run sends only your one prompt.

**A stopped session.** If something outside the run stops its session, the files Cline wrote stay unchanged on the session's storage and Cline's conversation is gone. A missed heartbeat ends the run with "Cline stopped: the session was lost", and the service delivers the last saved snapshot, with the time it was taken, in the detail and in the pull request text. The snapshot can be up to 10 minutes behind the work. When no snapshot exists, the run ends blocked with exit code 12 and no zip. Recovering the files from the stopped session's own storage is not supported; start a new run for what is missing.
## Troubleshooting

Find your symptom below, then follow the fix in that row.

### Setup and sign-in

| Symptom | Cause | Fix |
| --- | --- | --- |
| `--yes does not accept the costs of a full setup` (exit 4) | Cost acknowledgement is missing. No resources are created. | Add `--acknowledge-costs`. |
| `--yes does not confirm a teardown` (exit 4) | Teardown acknowledgement is missing. No resources are deleted. | Add `--acknowledge-teardown` and `--account-id <account id>`. |
| `--yes --acknowledge-teardown deletes without a question, so it needs --account-id <your 12-digit account id> to be sure of the account` (exit 4) | The account id is missing. Teardown reads and deletes nothing. | Add the 12-digit id of the account you mean: `--account-id <account id>`. |
| `your clone has changes that are not committed, so what is built would differ from what you see` (exit 7) | Your clone has uncommitted changes. Nothing is changed. | Commit or stash your changes. Run setup again. |
| `this folder is not a git clone of the repository, so there is no commit to pin and build` (exit 1) | Full setup needs a git clone to record the commit it builds. | Run full setup from a git clone. A zip download supports `--client-only` with a client you already have through `--from`. |
| `account mismatch: --account-id names <id>, but your credentials are for account <id>` (exit 7) | Your credentials belong to a different account. | Use credentials for the account you name, or correct `--account-id`. |
| `the stage direct in account <id> is being changed right now` (exit 7) | Another operation is changing the stage. | Wait until it ends. Run setup again. Use `./setup.sh --status` to see the state. |
| `the stack hamh-setup-runner is still <status>` or interrupted setup (exit 6) | An operation remains in progress. | Run `./setup.sh` again. It attaches to the build it started. |
| `scripts/deploy.sh failed (exit <n>)` or a failed build phase (exit 5) | A deploy or build phase fails. | Read the run log in the receipt folder `~/.config/ham/setup/<account>/<region>/logs/`. Fix the cause. Run the command the message names again: `./setup.sh --full` after a first setup, `./setup.sh --update` after an update (with `--build-on local` on the local route). Finished phases are skipped when their work remains in the account. |
| `the update is stopped: <n> running run(s) ... pin the worker runtime` or `DEPLOY-REFUSED reason=guard wait=yes` (exit 7) | Active runs block the update. The stage is unchanged. No runner was installed, or setup removes the runner it installed. | Wait until the runs end or cancel them with `ham cancel <runId>`. Run `./setup.sh --update` again. |
| `The stage is unchanged.` (exit 5 or 7) | The deploy refused before it changed anything. Setup removed the runner it installed and released the deploy lock. | Do what the line after it says. Exit 7 means that waiting cures it. Then run `./setup.sh --update` again. |
| An update that names the holder, start time, commit and route of the deploy lock (exit 7) | Another deploy of the stage holds the deploy lock, or your own earlier setup does. Nothing is changed. | Wait until it ends. When the message names `git checkout <commit>`, run it and then `./setup.sh --update` to attach to that build. |
| An update that shows two commits and names `git pull` (exit 7) | The stage runs a newer commit than your clone. Nothing is changed. | Run `git pull`, or name the older commit with `--revision <commit>` to deploy it on purpose. |
| An update that says the stage's commit `is not in this clone` (exit 7) | The stage runs a commit that your clone cannot place, and its `VERSION` does not show your clone to be newer: it is higher, the same or not readable. Nothing is changed. | When the message says that the stage runs a later release than your clone, make a new clone of the repository, or run `git pull` in a clone that has the current history, then run `./setup.sh --update` again. Otherwise name your commit with `--revision <commit>` to deploy it on purpose; a new clone helps only when your clone is old or shallow. A lower recorded `VERSION` goes ahead without this stop. |
| `operator: <old> -> <new>` in the plan (exit 7) | Another identity runs the update. Nothing is changed. | Run it as the recorded operator, or add `--change-operator` to make your identity the operator. |
| `DEPLOY-REFUSED reason=consent` (exit 5 from setup; exit 3 from the deploy) | A change would replace resources, delete data or remove something, with no exported value that asks for it: the stage's settings record holds a value the stage does not run, or the release builds the Instances capacity provider in another shape. A new default alone does not stop an update; the update keeps the stage's value. The stage is unchanged. | Export one of the values the line names, then run `./setup.sh --update` again. The line offers the record value when present, the live value and `default`, but leaves out exports that give the same value as one already named. |
| `the update cannot tell whether you asked for this release's default of HAMH_<NAME>` (exit 7) | The stage's settings record is of an update that did not finish, written by an earlier release, and does not say whether that update was given `HAMH_<NAME>=default`. Taking the default would replace the machines of the Instances part. Nothing is changed. | Export one of the values the message names, then run `./setup.sh --update` again. |
| `the stage already runs this commit` | The update only gets the client. | Nothing to fix. Add `--force-deploy` to deploy anyway. |
| `endpoint mismatch`, `source mismatch` or `configuration mismatch` (exit 7) | The result in your bucket fails a consistency check. Nothing is installed. | Run setup again. |
| `RETAINED` lines after `--teardown` (exit 9 when nothing else is left, otherwise 8) | AgentCore holds network interfaces for up to 8 hours after runtime deletion. The network resources they hold remain. | Wait for AWS to release the interfaces. Run the command the output prints, `./setup.sh --teardown` with your options, to remove the retained network resources. `--network-wait-minutes full` makes teardown wait for the release in the same run. |
| `LEFT: ...` or `TEARDOWN-INVENTORY INCOMPLETE` (exit 8) | A stage resource remains, a step fails, or a read of the account fails. The lines above the result name the resource or the failure. | Read those lines, fix the cause or wait, and run `./setup.sh --teardown` again. |
| `invalid or cannot be assumed` in a `could not delete the stack` line (exit 8) | The execution role that CloudFormation stored with the stack is missing or cannot be assumed. | Run the command teardown prints: `aws cloudformation delete-stack --stack-name <stack name> --role-arn <ARN of a role that CloudFormation can assume>`. |
| `STACKS LEFT: <n> stack(s) of stage direct could not be deleted` (exit 8) | A stack stays. Its line gives the reason: a bucket or repository could not be emptied, another stack still imports it, or CloudFormation refused the delete. | Read the reason, fix it, and run `./setup.sh --teardown` again. The run continues from what the account shows. |
| `The Go module store permits object deletion by the setup identity` | You run teardown as an identity that the bucket policy of the Go module store does not name. That bucket and the `packages` stack stay. | Run `./setup.sh --teardown` as the identity named in the message. |
| `a setup is running: build <id> is in progress`, `a setup is still installing: its build <id> has succeeded, and it has not removed its runner and given back the deploy lock of stage <stage> yet`, `a setup holds the deploy lock of stage <stage> and is about to start its build` or `a stack change is in progress` (exit 7) | A build recorded in the lock is in progress, a recorded build has succeeded and a setup still renews the lock, the lock records no build and is younger than 30 minutes, or a stack of the stage is changing. The still-installing message is followed by a `Holder:` line. `./setup.sh --teardown` stops with exit 7 and deletes nothing. | Wait until it ends. For a setup that is still installing, wait for that setup to end, or for 30 minutes if no setup is running. Run `./setup.sh --teardown` again. |
| `DEPLOY-REFUSED reason=caller` naming the expected identity | The deploy engine accepts only the operator identity in the account file and the setup runner's role. | Sign in as the expected identity, or run setup again as the identity you want to operate the stage. |
| `ham: No stage URL is set.` | The config file has no endpoint. The message ends with the path of the config file. | Ask the operator for the stage endpoint. Run `ham config set api_endpoint <url>`. |
| `the stage address does not resolve (<host>)` from a command, or `cannot list runs: the stage address does not resolve` on the Runs screen | The stage may have been removed or replaced, so the host in `api_endpoint` no longer exists. A computer with no network connection shows the same words. | Run `./setup.sh --status` to see the current endpoint. Run `ham config set api_endpoint <url>` to set it. |
| `ham makes no network calls for credentials: use a credential_process profile, an SSO or assume-role profile (ham asks the AWS CLI, version 2.9.0 or later), static keys or the AWS_* environment variables` | The profile has none of the four kinds of settings. `ham` found no credentials in it. | Use a profile with static keys, `credential_process`, SSO settings, or `role_arn`. You can also use `AWS_*` variables. Then run `ham config set aws_profile <name>`. [Which stage, and how you get in](#which-stage-and-how-you-get-in) shows a profile of each kind. |
| `HTTP 403` from any command, or `FAIL identity` in `ham doctor` | Credentials expired, or the identity belongs to another AWS account. API Gateway supplies the exact message; only the stage's account principals get access. | Renew your credentials. For an SSO profile, run `aws sso login --profile NAME`. Ask the operator whether your identity belongs to the stage's account. |
| `Run: aws sso login --profile <name>` | Your IAM Identity Center sign-in has ended. | Run that command, then repeat the command that failed. |
| `the AWS CLI was not found` or `older than 2.9.0` | SSO and assume-role profiles are read through the AWS CLI. It must be version 2.9.0 or later. | [Install or upgrade the AWS CLI](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html). You can also set `HAMH_AWS_CLI` to a newer program's path. |
| `did not answer within 30 seconds` | The AWS CLI took longer than the 30 seconds `ham` allows. `ham` stopped it. | Run `aws sts get-caller-identity --profile <name>` in a terminal to see why. Sign in again if needed. Then retry. |
| `ham: config.toml: ...` or `unknown config key "<key>"; the keys are api_endpoint, region, aws_profile, default_repo, download_dir, default_model, input_wait_minutes, mouse, report_tests, run.goal, run.rules, run.skills, research.enabled, ui.notify, ui.bell, ui.mascot, run.report_command_minutes, run.stall_calls, run.stall_window_minutes, run.stall_min_calls, run.command_grace_minutes, run.final_message, run.lifecycle_flow, run.review_round, run.compute, run.stall_new_calls, run.cargo_no_debug_info, run.cargo_no_incremental, run.exclude_target_dir, run.outside_checkout_note, ui.deprecation_notices, ui.mascot_motion, run.max_run_hours, run.provider_sharing, default_backend, customer_content, ui.copy_limit_kib, run.max_interrupts, run.max_questions, run.max_fresh_sessions, research.max_file_mib, research.max_unpacked_mib, research.max_run_mib, run.container_signals, default_engine, update_source` | The file has an unknown key or bad TOML, or you typed the wrong key. `ham config set` never overwrites an unreadable file. | Fix ~/.config/ham/config.toml by hand. The thirteen top-level file keys use camelCase: apiEndpoint, region, awsProfile, defaultRepo, downloadDir, defaultModel, defaultEngine, inputWaitMinutes, mouse, reportTests, defaultBackend, customerContent and updateSource. The thirty-two table keys use the table name and a dot: run.goal, run.rules, run.skills, research.enabled, ui.notify, ui.bell, ui.mascot, run.report_command_minutes, run.stall_calls, run.stall_window_minutes, run.stall_min_calls, run.command_grace_minutes, run.final_message, run.lifecycle_flow, run.review_round, run.compute, run.stall_new_calls, run.cargo_no_debug_info, run.cargo_no_incremental, run.exclude_target_dir, run.outside_checkout_note, ui.deprecation_notices, ui.mascot_motion, run.max_run_hours, ui.copy_limit_kib, run.max_interrupts, run.max_questions, run.max_fresh_sessions, research.max_file_mib, research.max_unpacked_mib, research.max_run_mib and run.container_signals. |
| `config.toml was written by a newer ham (configVersion 2); update ham to change it. Nothing was written`, or `ham models` or `ham runs` refusing a file with "written by a newer ham" | The file's `configVersion` is higher than your ham knows. A command wants to change the file, or a run would use keys your ham does not know. | Update ham. Or run `ham config check`, which still runs, and remove the keys your ham does not know. |
| `config.toml changed on disk; reloaded; nothing written` | Another program or another ham changed the file after ham read it and before ham replaced it. | Run the command again. Nothing was lost. When two writers meet, the last write wins. |
| ``line 5, column 1: unknown key `goall` in [run]; did you mean `goal`?`` or `line 6, column 2: unknown table [serch]; did you mean [research]?` from `ham config check`, `ham doctor`, `ham models` or a start | ham does not know a key or table in the file, often because of a typo. A top-level key typed below a `[table]` line belongs to that table. | Correct the line the message names, or run `ham config unset <key>` or `ham config edit`. `ham config get`, `show`, `check`, `edit`, `docs` and `ham doctor` still run on a file with a bad key. |
| `FAIL allowlist` or `FAIL api` in `ham doctor` | The endpoint is wrong or the stage is down. `FAIL api ... not reached` means no answer came back. | Check the endpoint with the operator. It must end with the stage name, /direct. |
| `usage: no command "pul"; closest: pull; to start a run with "pul" as the prompt, add --repo, --new-repo or --s3-only and --direct` | Your first word is not a `ham` command. It is one edit from one. `ham` reads any other first word as a new run's prompt. This word has no target flag. `ham` exits 2. It starts nothing. | Fix the spelling. To use that word as the prompt, add `--repo`, `--new-repo` or `--s3-only` and `--direct`. You can also use `--prompt`. |

### Starting a run

| Symptom | Cause | Fix |
| --- | --- | --- |
| `unknown model "<x>" (from --model); choose one of: ...` | The model name is not one of the models `ham models` lists. | Run `ham models`; use opus, sonnet, sol, astra, glm, fable, or a full id. If the source says HAMH_MODEL or config default_model, follow that message's fix. |
| `model "<x>" (from <source>) is listed as <id> but has no passing smoke record, so it is not selectable yet` | The model has no passing smoke record. | Pick another model. |
| ``no rules found in <dir>: run `ham rules init` to write the default rules, or start with --no-rules`` | The rules folder is missing or empty. | Run `ham rules init`, or add `--no-rules` for this run. |
| `rule "<name>" ...`, such as `rule "AGENTS.md" cannot be called AGENTS.md: Cline would load it as a repository instruction file; rename it` | A rule file breaks a name, empty-file, or frontmatter limit. | Change the named file. Run `ham rules show` to list what would be sent. |
| `ham: POST /v1/direct/runs: HTTP 422: ... (unknown-model)` | Your `ham` build and the stage have different model lists. | Run `ham models --server` to see the stage's list. Update `ham` or ask the operator. |
| `HTTP 413: the request is <size>; a request carries at most <limit> in all ... (payload-too-large)` | The prompt, rules, skills, attachments and prompt files together are over what one request carries. The message names the largest part. | Shorten the largest part, or send fewer files. |
| `HTTP 404: this stage does not serve direct runs; ... (direct-runs-not-served)` | The endpoint belongs to a stage other than `direct`, the direct-run stage. | Use the `direct` endpoint. It ends in /direct. |
| `HTTP 400: prompt is empty (bad-request)` | The prompt is empty. | Write a prompt. |
| ``HTTP 400: StartDirectRunRequest: a member has the wrong type or a value this API does not accept (line 1, column 61) (bad-request)`` | The request body does not parse. This happens when you call the API by hand or when your ham and the stage are different versions. The message names the request type, the kind of problem and the place in the body (line, then the column counted in bytes). It never repeats what you sent, so a value or a name you pasted in by mistake does not come back in the answer. The other kinds are: the body is not valid JSON, the body ends before the JSON is complete, a number out of range, a required member is missing, a member this API does not know, a member twice, and any other mismatch. | Look at the body near the line and column the message names. Run `ham --version` and update ham if the stage is newer. |
| `engines/cline.toml line <n>: ... Nothing was sent.` | The engine file holds something the check refuses: a key that is not one of the five settings, a locked key such as `aws.profile`, a value out of range, or settings without a `schema = 1` line. ham stopped before it sent anything. On the command line it exits with code 2. | Correct the line the message names. `ham engine cline check` lists the problems it finds. `ham engine cline unset <key>` removes a key. |
| ``this stage predates engine settings; run `ham engine cline unset <key>` or update the stage (keys set: <list>)`` | The stage was deployed before engine settings existed, and it refused the request. ham does not send the run again without the settings. | Remove the keys with `ham engine cline unset <key>`, or ask whoever runs the stage to update it. |
| `HTTP 400: ... (invalid-engine-config)` | The stage's copy of the check refused the settings. That can happen when your ham and the stage are different versions. Nothing was written. | Run `ham engine cline check`. Update ham, or ask the operator. |
| A run ends with the label `Cline did not start` and a detail that names engine settings, or that reads `request.json is not a StartDirectRunRequest` | agentd refused the settings or could not read back what it placed for Cline, or the worker image is older than your ham and refuses them. The run stopped before the first model call. | Tell the operator. Remove the keys with `ham engine cline unset <key>` and start the run again without those settings. |
| You set a key, but the run shows no engine settings card | You may be running an older ham, which does not read the `engines/` folder. The file may set no key (an empty file, or only `schema = 1`). The run may not have placed the settings yet. | Run `ham engine cline show`. It prints the settings a run would send, or says that none is set. Update ham if it does not know the command. |
| `--no-rules and --rules say opposite things; give one`, and similar usage errors, or `... is an option of a direct run; add --direct` | The flags do not go together. These errors exit with code 2. | Remove one of the conflicting flags. |
| `this service cannot edit tags yet` after `/tag` or after `d` (remove a tag) in the tag explorer. | The stage runs a service older than the tags feature. It has no edit route. | Ask the operator to deploy the current service. Tags that were already sent still show. |
| `remove needs every run loaded: press r to load them all, then d` in the tag explorer. | The explorer holds only the newest 100 runs. Removing a tag everywhere would miss the others. | Press `r` to read every page of the list, then `d`. |

### A run that stops or does not deliver

| Symptom | Cause | Fix |
| --- | --- | --- |
| The run ends `blocked`, `blocked: <reason>` with `refused: cc:<name> is not on the allowlist of this stage; an operator allows it with ham-admin repos allow cc:<name>`, `refused: cc:<name> is outside the hamh-e2e- prefix` or `refused: cc:<name> is outside the repository names this stage allows (<rule>)` | The repository is outside the stage's allowlist or its name is outside the stage's repository names. The exact wrapper around the reason may differ. | The operator allows the repository with `ham-admin repos allow cc:<name>`; the section [Allow a repository for pull requests](../README.md#allow-a-repository-for-pull-requests) of `README.md` shows how to get `ham-admin`. Then start a new run. |
| The run ends `blocked` with `EC2 had no free capacity for the machine this --compute instances run asked for.` | The run asked for the Instances machine and EC2 had no machine of that type free in the stage's zone when the session started. The service's own words follow the two sentences. | Start the run again later, or start it without `--compute instances` to run in the standard session. |
| Label `Cline did not start` | Setup found a missing, changed, or shadowed rule before sending the prompt. A shadowed rule shares a repository rule's name; another cause is Cline reporting a different model. | Run `ham rules show`. Remove the same-named repository rule from .clinerules or rename yours, then restart. |
| Label `Cline stopped: no output for the silence limit` | Cline wrote nothing for 900 seconds. | Pull the delivered work so far with `ham pull <runId>`. Read what is there, then start another run with a more focused prompt that names what is left. |
| Label `Cline stopped: Cline ended the run itself` | Cline's loop guard reached 5 identical tool calls in a row, or its mistake limit reached 6 failed tool calls in a row. | Answer the question: `s` starts a fresh chat, and `p` sends something different. You can also read Cline's last message in EVIDENCE.md, narrow the prompt and start again. |
| Label `Cline stopped: the harness ended the run (…)` with “the session disk is full” in the brackets | Error -32603 came while the checkout held 960 MiB or more, or the root volume had less than 512 MiB free. | Read the numbers in the brackets. Pull the delivered work with `ham pull <runId>`. Start another run with a smaller build, or send `ham say` asking Cline to delete build output first. See “When the session disk is full”. |
| Label `Cline stopped: the model provider failed` | Bedrock returned throttling or a service error, or refused every route, after Cline's retries, the harness's resumes and its switches to other routes. | Pull the delivered work with `ham pull <runId>`. Wait and start again, or pick another model with `--model`. |
| `ham: run <runId> is not waiting for you. Its last question was ...` | The question is closed: answered, expired, finished or cancelled. Or Cline has not asked one. | Read the end of `ham watch <runId>`. Start a new run if this run ended. |
| `queued, not acknowledged yet: the run may have ended; run: ham watch <runId>` | The service queued your answer, but agentd did not acknowledge it within 20 seconds. The run may have ended, or the window closed. | Run `ham watch <runId>`. If the question is still open, send the answer again with the same command. |
| `this question was already answered with <action>` | Another terminal answered first. | Nothing to do. Read the run with `ham watch`. |
| `the answer window closed` (HTTP 410) | The wait ended before your answer. | The run finished with what exists. Pull the zip. Start a new run, and raise `input_wait_minutes` next time. |
| The dashboard shows the text of a question with no card and no keys | Your `ham` is an older build that does not draw the question card. `ham doctor` warns `backend newer than this ham` when the stage is newer than your build. | Update `ham`. Then answer with the keys or with `ham finish`. |
| Nobody was there and the run finished | The window ended. The detail says "Nobody answered in N min, so the run finished with what exists." | Read the zip's `EVIDENCE.md`. Set a longer wait for the next run. |
| Label `Cline stopped: it ended its turn without changing a file` | Cline wrote a final message but left every file as it was, for example on a task it judged impossible. The zip holds the unchanged repository, and no pull request opens. | Read Cline's last message in EVIDENCE.md. Rewrite the prompt so that it can be done, then start again. |
| The Detail line begins `Ran on <model> after <n> switches.` | The model you chose kept failing, so the harness moved the run to other routes. The work can come from more than one model. | Read "When the model provider fails". Pull the zip and check the work. Start again with `--model` if you need one model. |
| A run takes much longer than usual | Bedrock is failing. The harness is resuming, switching routes or waiting between probes. The wait grows to 5 minutes between probes. | Wait, or run `ham cancel <runId>` to stop it and keep the work. At the time limit the label is `Cline stopped: the run reached its time limit`. |
| Label `Cline stopped: the run reached its time limit` | The run used its time, up to 7 h 45 min. | Pull the zip with `ham pull <runId>`. Start a new run with a prompt that names what is left. |
| Label `Cline stopped: the session was lost` | The virtual machine stopped answering for more than 10 minutes. The service delivers its last saved snapshot, up to 10 minutes behind the work; the detail ends with "The work is the last saved snapshot" and the time it was taken. | Pull the zip with `ham pull <runId>` if one exists, look for missing work, and start a new run that names it. |
| The run is `complete` with exit code 0, but the PR is missing | `--s3-only` or a head equal to the base opens no pull request. Opening may also have failed; the detail says `pull request not opened: <reason>`. | Run `ham pr <runId>` to see the reason. Get the code with `ham pull <runId>` or `ham clone <runId>`. |

### Downloading and cloning

| Symptom | Cause | Fix |
| --- | --- | --- |
| `no matching artifact; the run has none yet` | The zip is not ready, or the run has no zip. | Wait for the run to end with `ham watch <runId>`. Check `ham runs`. |
| `another download of <file> holds <lock file> and has written nothing for 5m` | Another `ham` is downloading the same file and made no progress for 5 minutes. It may be stuck. | End that `ham` process, then run the command again. A `.part` file it left is used to resume. |
| `sha256 mismatch: downloaded <a>, object metadata says <b>` | The download is damaged. | Repeat `ham pull <runId>`; it resumes from the saved `.part` file and checks again. Tell the operator if the problem persists. |
| `git is not available (...)` followed by "On macOS, install the Xcode command line tools with xcode-select --install; on Linux, install the git package" | `ham clone` needs a working git. On a Mac without command line tools, /usr/bin/git is a stub. | Run `xcode-select --install` on Mac, or install the git package on Linux. Nothing was cloned. |
| `<dir> already exists and is not empty. Give a directory that does not exist or is empty: ham clone <runId> <directory>` | The destination contains files. A symbolic link, file, or current folder is also refused. | Pick a new or empty folder. Use `ham clone <runId> <directory>`. |
| ``"<x>" is not a run id; use the id `ham runs` prints`` | The run id has a typo. | Copy the id from `ham runs`. |
| `run <runId> has no zip or partial zip yet; ham clone needs a delivery. Follow the run with ham watch <runId>.` | The run has not delivered. | Wait, or use `ham watch <runId>` to see why it stopped. |
| The download link no longer opens | Links last 15 minutes. | Run `ham link <runId>` to get a new link. |
| `could not open the link: ... (set HAMH_OPEN_COMMAND to a program that opens a URL)` in the dashboard's notice line | `o` could not run the program that opens links: `open` on macOS, `xdg-open` on Linux. | Install `xdg-open`, or set `HAMH_OPEN_COMMAND` to a program that opens a URL. Press `d` to download the zip, or run `ham link <runId>`. |
| `ham: git push failed (exit <n>). ham never forces a push: ...` | Git refused the push. The remote branch has other history, the URL is wrong, or your credentials may not write there. Git's own lines above the message say which. | Push to a new branch with `--branch <name>`, or fix the URL or your SSH key or credential helper. |
| `usage: ham push: the git URL carries a credential ...` | The URL has a password or a token in it. | Give the plain URL, such as `https://host/group/repo.git`, and let git find your credentials. |
| `HTTP 409: run <runId> is complete; it cannot change (run-finished)` from `ham cancel` | The run already finished. | Nothing remains to stop. Pull the result with `ham pull <runId>`. |

## FAQ

**Does setup deploy AWS infrastructure?**

Yes.
`setup.sh` is the single entry point for Hyperscale Agent Meta Harness.
On a terminal, its first question asks whether you want full setup (backend and client) or client-only setup, and `--full` or `--client-only` skips that question.
Full setup installs a runner, a CodeBuild project, in your AWS account.
The runner builds and deploys the backend there with CDK and CloudFormation.
It also builds the terminal app, `ham`, in your account.
Setup downloads the client from your own bucket and installs and configures it.
The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh). Installing the client and the full setup need git on PATH. The full setup and `--update` also need a git clone; a zip download does not work for them. `--status`, `--doctor` and `--teardown` need neither git nor a clone.
See [Install](#install).

**How do I stop the charges?**

Run `./setup.sh --teardown` to remove the stage, your backend deployment.
The NAT gateway, the Network Firewall endpoint and the interface endpoints bill every hour while the stage exists.
Teardown runs with your own AWS credentials and asks once before it deletes.
Download the results you want with `ham pull <runId>` first; teardown deletes the stage's results bucket and run table.
Run it as the identity that ran setup, so that it can empty the Go module store.
To skip the question, add `--yes --acknowledge-teardown --account-id <account id>`.
AgentCore network interfaces can remain for up to 8 hours after the runtime is deleted.
The network resources they hold are listed as `RETAINED`. Run `./setup.sh --teardown` again after AWS releases them.
The CDK bootstrap stack, with its bucket and container repository, stays in your account.
KMS keys and secrets that CloudFormation scheduled for deletion are listed with their dates.
`./setup.sh --teardown` puts the Bedrock invocation logging setting back to the state recorded by a deploy with `enable-and-restore`, when the setting still names the stage's log group. In every other case it leaves the setting as it is. The stage's KMS keys wait 30 days before AWS deletes them, and the `pending:` lines give the dates.

**Can I set up a second computer?**

Yes.
Clone the repository on that computer and run `./setup.sh --update` to build and download a client in your account.
When the stage already runs the commit of that clone, the update only gets the client.
A clone older than the stage's commit stops the update with exit 7, and the message names `git pull`.
When the stage records a commit that your clone does not hold, a lower recorded `VERSION` goes ahead, and any other answer stops the update with exit 7 and names a new clone or `git pull` when your clone is older than the stage, and `--revision` when it is not.
An update takes about 20 to 25 minutes when it reuses the Cline binary from the earlier build, and about 21 to 27 minutes more when the Cline binary must be rebuilt.
The receipt, setup's local record on the first computer, is a cache.
AWS holds the state.
If you have `ham` installed, run `./setup.sh --client-only --endpoint <stage-URL>` to configure it for your stage. If you need `ham`, run `./setup.sh --client-only --build --endpoint <stage-URL>` or replace `--build` with `--from <directory>` for a directory that holds `ham` and `SHA256SUMS`.

**What if setup stops halfway?**

Run `./setup.sh` again.
It attaches to the build it started and never starts a second one.
Exit code 6 means the operation is running and is safe to resume.
Exit code 7 means a conflicting operation or a source or configuration mismatch.
Setup tells you what to change.

**Does the run keep going if I close my laptop?**

Yes; the session runs in AWS, and `ham` only watches.
Come back with `ham watch <runId>` or `ham runs`.

**Can I follow a run from another computer?**

Yes; install `ham` there and set the same endpoint.
Use an identity from the same account.
Then use `ham watch <runId>`, `ham pull <runId>`, or `ham clone <runId>`.

**What does "Cline reported done" mean?**

Cline ended its turn and wrote a final message.
At least one file must also have changed.
With 0 changed files, you see "Cline stopped: it ended its turn without changing a file".
Cline giving up after changing at least one file still reads "Cline reported done".
The product makes no judgment. Without a test report it also ran no test; with one it ran the repository's own test commands in a clean copy of the delivered head and shows what they did.
Read the zip's EVIDENCE.md and run the tests yourself.

**Why does the card show no gates and tests?**

A direct run has no gates or verdicts.
The card therefore has no Evidence block. Its Outcome line says how Cline stopped.

**Why does exit code 0 not mean success?**

The run is complete when a zip exists.
Cline may have stopped halfway.
Read the label.

**Can I use GitHub or GitLab?**

GitHub and GitLab delivery are not supported.
Use CodeCommit with `--repo cc:hamh-e2e-<name>` (or another name the stage allows), or use `--s3-only`.
You can also use `ham pull <runId>` and `ham clone <runId>`.

**Can I use my own rules and my repository's AGENTS.md together?**

Yes; your rules enter Cline's session rules folder, while repository files load as usual.
Your default rules tell Cline to follow your file on conflicts and name the repository rule set aside.
A repository rule sharing your rule's name stops the run before startup.

**Can I change Cline's behaviour, for example turn on nudges?**

Edit continue.md and set max_nudges above 0.
The product changes nothing else about Cline.

**Which model should I use?**

Claude Sonnet 5.5 is the default.
The other four are Claude Opus 5.5, GPT-6.1 Sol, GPT-6 Astra, GLM 5.3.

**Where does the name of a run come from?**

A small model, Claude Haiku 5.5, writes a name of 5 to 7 words from your prompt a few seconds after the run starts.
Until then, and for runs older than names, the list shows a seed name in dim text: a short label cut from the start of your prompt.
The model sees at most the first 2,000 and the last 1,000 characters of the prompt, with known credential shapes hidden.
Cline keeps the model you chose and never uses the small model.
Read [The model that names runs](#the-model-that-names-runs).

**Where do the tags of a run come from?**

Your repository run shows `project:<repository>` at once.
A few seconds after your run starts, Claude Haiku 5.5, a small model, writes up to 6 topic and tech tags from your prompt.
It sees at most the first 2,000 and last 1,000 prompt characters, with known credential shapes hidden.
It sees a list of your own recent tags to reuse your words, and never writes a project tag.
You add and remove tags with `/tag` in your run's message box or the tag explorer (`t` on the Runs screen), and removed tags stay removed.
Cline keeps the model you chose and never uses the small model.
Read [Tags](#tags) in the Runs chapter.

**What happens if the model provider fails during my run?**

The harness resumes Cline on the same model up to three times, then switches the same session to other models, and waits and probes when every route is down.
Read "When the model provider fails".

**Why did my run end on a different model?**

The `Detail` line says `Ran on <model> after <n> switches.`
This means the model you chose kept failing.
The harness moved the run to other routes.
Cline's own stop does not cause a switch.
The `Model` line still names the model you chose.

**Does the session use my AWS credentials?**

No; each session gets its own short-lived credentials from the service.
No long-lived key and no GitHub token enters the VM.

**Can the agent reach the internet?**

Access covers only Amazon Bedrock, api.osv.dev, the vulnerability database, and package installs through the stage's loopback package proxy.
Telemetry from Cline is compiled out.

**How do I see what a run cost?**

The card has a Tokens and USD line when the stage reported usage for the run.
A run with no reported usage has no such line.
