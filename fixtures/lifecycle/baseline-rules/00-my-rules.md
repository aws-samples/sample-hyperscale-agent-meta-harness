# My rules for every task (editable)

> This file is yours. Change it any time (`cah rules edit`). `cah` sends the current text with each new run;
> an edit applies to the next run, not to one already running. Keep it under 8 KB and put no secrets in it.

## Precedence

- If a rule in the repository (AGENTS.md, .clinerules) conflicts with a rule in this file, follow this file and say which repository rule you set aside.
- If the repository root has a CLAUDE.md, read it too.

## Test-driven development

- For every behavior change or bug fix, write a test that fails first. Run it, and keep the red output (command, exit code, failing assertion) for your final report.
- Then write the least code that makes it pass. Then refactor with the tests green. Repeat one test at a time; do not write all the code and then all the tests.
- For a bug, state your theory of the cause, reproduce it with a failing test, then fix the cause rather than the symptom.
- Never weaken, skip, delete or loosen a test, an assertion or a threshold to get green. If a test is wrong, say why and fix it in its own commit.
- Cover edge cases (empty, boundary, invalid input, error paths) as well as the happy path.

## Proving it works

- Before you say you are done, run the full test suite, the linter and the type check, not only the new tests. Report the exact commands, exit codes and pass/fail counts.
- A change to something observable from outside (an API, a CLI, a UI, a network call) needs one real end-to-end run through the actual code path. A mock is not enough. If you cannot run it, write "Not run: <reason>".
- Read the output and the logs after every run. Never claim a result you did not see.
- Restart a service after you change its code or configuration, then test.

## Passwords and secrets

- Never create, store, request or hard-code a persistent password for any system: not in source, .env, Compose, infrastructure code, a secret store or a document.
- Use identity: IAM roles and short-lived STS credentials, SigV4, IAM database authentication, OIDC. A throwaway local test container may use trust authentication on 127.0.0.1 or a random secret made at run time and kept in the environment; the file holds `${NAME}`, never a value.
- If the task cannot work without a persistent password, stop and report that as the blocker.
- Never put a key, token or credential in a file, a log or a commit message.

## Commits

- Commit small, after each green step. Subject in the imperative, under 72 characters; the body says what changed, why, and the test evidence.
- Do not commit generated files, logs or secrets, and do not rewrite published history.

## Working style

- Run every command non-interactively. Prefix commands that can run long with `timeout <seconds>`, send long output to a log under /tmp, and read its tail.
- When two different fixes have failed, stop, write down what you tried and what you saw, and say what you need.

## Environment of the cloud session

- Docker works in this session: DOCKER_HOST points at a Podman socket (/var/run/docker.sock), so Testcontainers and docker run, ps and logs work; Ryuk is switched off and mapped ports are on 127.0.0.1. The first call can wait up to about 20 seconds while the runtime starts. Only the image postgres:16 exists and no registry can be reached, so use PostgreSQLContainer("postgres:16") or the native cah-services postgres; any other image or tag fails at once, and docker compose is not installed.
- Installed: Python 3.12 with uv, Node 24 with npm and pnpm, Go 1.27.1, Corretto 21 with Maven 3.9.16.
- Package installs go through the loopback package proxy only.
- The package proxy URL (127.0.0.1:3128) only works inside this session. Never write it into files you commit (Maven wrapper properties, pom repositories, .npmrc, pip or Go settings); committed files use the public registry URLs (for example https://repo.maven.apache.org/maven2/ for the Maven wrapper). Lockfiles count: before you commit, run `grep -l 127.0.0.1 package-lock.json yarn.lock uv.lock` in each project directory; a hit names the session, so fix or leave out that file (`npm install` and `pnpm add` here write public URLs; `uv lock` and yarn do not).
- For PostgreSQL 16 or RabbitMQ, run `cah-services postgres` and `cah-services rabbitmq`, then `eval "$(cah-services env)"` to get the connection variables. Both listen on 127.0.0.1 only.
