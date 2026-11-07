# images

## Purpose

Container images and the CodeBuild ARM build specs that produce native linux-arm64 artifacts for stage `direct`: the Cline fork binary, the worker image with its native services (`services/`) and the pinned tool installers it runs (`tools/`), the delivery image, and the language test container (`lang-test/`).

## Ownership

Owned by the root contract. The CodeBuild project is `hamh-direct-image-build` (runtimes stack).

## Local Contracts

- `codebuild/*.yml` run only through `scripts/codebuild-run.sh`, which builds the committed HEAD
  (`SOURCE_SHA`) from an S3 zip; builds write only the `builds/out/<sha>/` prefixes of the stage bucket and
  `hamh-<stage>-*` ECR repositories.
- `codebuild/*.yml` name no Region and no account: they build in whichever
  `accounts.json` entry `scripts/deploy.sh` targets, every ECR login is
  `aws ecr get-login-password --region "$AWS_REGION"` (CodeBuild sets `AWS_REGION` to the build's
  Region), and the account and bucket arrive as `--env` values (`infra/test/buildspecs.test.ts`).
- Every `codebuild/*.yml` starts its first phase with `. scripts/lib/child-build-env.sh`: when `scripts/codebuild-run.sh` carried a generated account file in the source bundle, it checks the file against `HAMH_ACCOUNTS_FILE_SHA256` and exports `HAMH_ACCOUNTS_FILE`; otherwise it does nothing. The worker image key (`codebuild/worker-python.yml`) and the delivery image key (`codebuild/delivery.yml`) cover the line.
- Each `codebuild/*.yml` stays under 22,000 bytes (`infra/test/buildspecs.test.ts`): it reaches
  CodeBuild as a buildspec override, and CodeBuild refuses a buildspec over 25,600 characters.
- Every release asset a `codebuild/*.yml` downloads is one `bash images/tools/fetch.sh "$X_URL" "$X_ARCHIVE_SHA256"
  build/<name>.tgz` step: the file lands in `<dest>.part`, its pinned SHA-256 is checked before it takes its name,
  and a failed transfer or a mismatch is fetched again. The curl inside passes `--retry 5 --retry-all-errors`,
  because plain `--retry` does not retry a reset transfer (curl exit 56). The pins never change: a file that stays
  wrong fails with the installer's own message. The base image is
  `public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:...` in every Dockerfile, so no build pulls a Docker Hub image
  and no mirror fallback applies; dnf keeps its own retries.
- `codebuild/setup-runner.yml`: the buildspec of the setup runner's own project `hamh-setup-runner` (`infra/setup-runner.yaml`; Linux x86_64, standard image 5.0, Python 3.12). It is not run through `scripts/codebuild-run.sh`: `setup.sh` starts the project with the source archive of the clone it runs from, so the buildspec of a run is the one of that commit (`--source-location-override` names the archive; the project's own source location is a placeholder). A teardown (`SETUP_ACTION=teardown`) and the build that prints the CDK bootstrap template (`SETUP_ACTION=bootstrap-template`) run the same buildspec. After the hook every buildspec of the folder starts with, its one command is `bash scripts/setup/runner-build.sh`, which holds all the logic; the buildspec names no Region and no account and prints no environment. It sets two variables around that command and exports one: `SETUP_RESULT_SHA256` (`none` until the build passed; then the SHA-256 of the `result.json` that the runner uploaded, read from `<work>/anchor.env`, which the last command sources) is listed under `env: exported-variables`, so CodeBuild keeps it with the build where nothing that writes the setup bucket can change it, and `setup.sh` believes a `result.json` only when it hashes to that value (`scripts/tests/test_setup_runner_buildspec.py`).
- `codebuild/cline-fork-arm64.yml`: `scripts/build-cline-fork.sh --target linux-arm64` must end with
  `FORK-BUILD PASS series=15`; its smoke test (`--version`, ACP `initialize` reporting 3.0.71) is the
  first native execution of the binary. Promotion to `engine/cline/<sha256>/` in the harness bucket
  and the SHA-256 in `engine/lock.toml` are a separate builder step; the worker image build spec runs no fork
  build. The spec also leaves `build-record.json` (the build's own record under a fixed name) and
  `build-id.txt` (the id of this CodeBuild build) beside the binary: a deploy of a fresh account
  runs this spec before the worker image and reads both back from
  `builds/out/cline-fork/<sha>/` in `scripts/lib/deploy.py`, and the build id keeps the output of another build out.
- `codebuild/worker-python.yml`: stages agentd (`builds/out/agentd/<sha>/agentd` in the stage bucket), the
  promoted Cline artifact (`engine/cline/<sha256>/`) and the OpenCode and uv release
  binaries, builds gitleaks from source, checks each SHA-256, fetches Cline's goal plugin from its pinned sources and bundles it,
  builds
  `worker-python/Dockerfile` for linux/arm64 (unpacked size gated at 3,200 MiB before the push), runs the 14 checks of `engine/plugins/build/plugin-unit.mjs` against the shipped goal bundle
  and the checks of `observer-unit.mjs` against the observer plugin inside the built image (the image's own Node), pushes `hamh-<stage>-worker-python:<tag>`
  (`IMAGE_TAG`, which `scripts/deploy.sh` sets to `in-<image key>`, else `SOURCE_SHA`; tags are
  immutable), gates the compressed size ECR holds for the tag at 1,800 MB (the size gate bullet below) and writes
  `builds/out/images/worker-python/<sha>/image.json` (digest, pins, unpacked MiB and compressed bytes).
  The build spec passes `LANG_FAMILY` (the Dockerfile build argument, default `python`; `scripts/deploy.sh` passes `direct` for stage
  `direct`). For `direct` it also checks the pinned `go version`, `java -version` and `mvn -v`, that curl, unzip, git, `cc` and `ld` are present,
  that golangci-lint, Gradle, cargo-llvm-cov and llvm-tools are absent, and runs `worker-python/rust-check.sh` (a compile-and-run check of the Rust
  toolchain as the engine user, offline) before the push.
- Size gate of the worker image: the platform counts the compressed bytes of the layers in ECR (about 2,048 MB), so `codebuild/worker-python.yml` fails an image above 1,800
  MB compressed (1 MB = 1,000,000 bytes; `aws ecr describe-images` after the push, which the build role's `ecr:DescribeImages` allows) and above 3,200 MiB unpacked (a loose
  ceiling that keeps growth visible, checked before the push). Both go through `worker-python/image-size-gate.sh`, which prints `IMAGE-GATE PASS|FAIL ...` and fails on a size
  it cannot read. Tags are immutable and `scripts/lib/deploy.py` reuses a tag it finds without reading its size, so after a failed compressed gate change an image input
  (which moves the key) before deploying again.
- The worker image copies binaries with `COPY --chmod=0755` and reaches gitleaks, uv and the tool installers through
  `RUN --mount=type=bind` (BuildKit, `DOCKER_BUILDKIT=1`), so no layer rewrites the binaries or keeps a build input. It installs
  `images/tools/install.sh` (the Python tool set) at `/opt/harness/gate-tools` and `tools/lang/install.sh <family>` (the language tool set)
  at `/opt/harness/gate-tools/lang`, records both manifests' SHA-256 in `/opt/harness/pins.json` and carries the label
  `harness.family=<family>`.
- Stage `direct` builds the worker with LANG_FAMILY=direct: Go, the JDK, Maven and the slim Rust set with the gcc, binutils and
  glibc-devel that rustc links through, without golangci-lint, gosec, Gradle, llvm-tools or cargo-llvm-cov.
  It also carries RabbitMQ 3.13.7 and the `ham-services` helper that starts it with PostgreSQL in a session, and rootless Podman with the
  offline `postgres:16` and the `ham-containers` socket helper
  and the `ham-research` client of agentd's research API.
  Every image also carries `ham-nobody-exec`, the helper that starts the engine of an Instances session as the user nobody in a private IPC namespace. It leaves out the OpenCode binary: the build spec runs OpenCode's version check for the
  other families only. It measured 1,780 MiB unpacked before Rust; the size gate above holds the image to 1,800 MB compressed.
- The worker image runs `services/install.sh` after its package installs (PostgreSQL, MariaDB, Valkey, mailpit; uid `svc` 904;
  `/var/lib/harness-services`). The build spec runs `postgres`, `mariadbd` and `valkey-server --version`, runs `mailpit --help`, checks
  that mailpit is `root:root` and that `svc` is 904, and prints `/opt/harness/services/manifest.json`. The image has uid `sut` 903 (system,
  no home, nologin), which code under test runs as (agentd's lock step); the build spec checks it is 903.
- `codebuild/image-secret-scan.yml`: pulls the image named by `IMAGE_URI` and scans it with the image's own pinned
  `/opt/harness/gate-tools/bin/gitleaks`: `/opt/harness`, `/etc`, `/home`, `/root`, `/usr/local`, `/var/lib`, `/srv`, `/mnt`, the
  image configuration (`docker inspect`) and the build history. It builds and pushes nothing. The runtime base role can pull the
  stage's images and the engine can read that role's credentials, so an image must hold no secret. The rules are gitleaks'
  built-ins plus `codebuild/image-secret-scan.gitleaks.toml`, which exempts only three paths from hash-pinned upstream downloads
  (Go standard library test vectors, one RabbitMQ management documentation page, the JDK `java.security`) and never a file the
  harness writes. Run it as `scripts/codebuild-run.sh --buildspec images/codebuild/image-secret-scan.yml --stage direct --env
  IMAGE_URI=<repo>@<digest>`; it ends `IMAGE-SECRET-SCAN PASS` or `IMAGE-SECRET-SCAN FAIL` (any finding fails; a missing report
  counts as 1,000 findings).
- `codebuild/delivery.yml`: stages the three delivery bootstraps (`builds/out/delivery/<sha>/` in the stage bucket,
  cargo lambda arm64 builds) and gitleaks 8.30.1 built from source with the Go toolchain of `tools/gitleaks.lock`, checks each
  SHA-256, builds `delivery/Dockerfile` for linux/arm64 below 600 MB, checks `git`,
  `git-remote-https` and gitleaks run, pushes `hamh-<stage>-delivery:<tag>` (`IMAGE_TAG`, which
  `scripts/deploy.sh` sets to `dl-<delivery key>`) and writes
  `builds/out/images/delivery/<sha>/image.json`.

## Work Guidance

## Verification

```bash
scripts/codebuild-run.sh --buildspec images/codebuild/cline-fork-arm64.yml --env HARNESS_BUCKET=hamh-direct-harness-<account>
```

## Child DOX Index

| Path | Covers |
| --- | --- |
| `services/AGENTS.md` | The native services installer and its pins: PostgreSQL 16 (started by ham-services), MariaDB 11.4, Valkey 9 and mailpit for the worker image |
| `tools/AGENTS.md` | See that file. |
| `lang-test/AGENTS.md` | See that file. |
| `worker-python/AGENTS.md` | The worker image of stage direct: agentd, Cline fork, the Python and language tool sets, python 3.12, Node 24, Go, the JDK, Maven, Rust with gcc, RabbitMQ, rootless Podman; the size gate and the Rust check |
| `delivery/AGENTS.md` | The delivery image: repo-gateway, bundle-validator and packager bootstraps, git, gitleaks |
