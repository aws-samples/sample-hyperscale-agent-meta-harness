# images/delivery

## Purpose

The delivery image for the three container-image Lambdas `repo-gateway`, `bundle-validator` and
`packager`: their bootstraps, `git` and gitleaks 8.30.1 on
Amazon Linux 2023 (arm64).

## Ownership

Owned by `images/AGENTS.md`. Built only by `images/codebuild/delivery.yml`; code in
`crates/delivery`.

## Local Contracts

- Container images, not zip Lambdas: the functions need `git` with its HTTPS remote helper and
  gitleaks, which `provided.al2023` zip functions lack.
- Base image pinned by digest (the worker image's Amazon Linux 2023 digest); `git-core` from dnf;
  every binary the build spec stages (the three bootstraps and gitleaks) is checked against its SHA-256 build
  argument, and the pins are recorded in `/opt/delivery/pins.json` and image labels.
- Binaries live in `/opt/delivery/bin`; each function's `ImageConfig.EntryPoint` names its own
  binary (the default entry point is `repo-gateway`). `GIT_BIN`, `GITLEAKS_BIN` and
  `WORK_ROOT=/tmp/delivery` are set; Lambda mounts only `/tmp` writable.
- The image must stay below 600 MB; the build spec fails otherwise.

## Work Guidance

## Verification

```bash
scripts/codebuild-run.sh --buildspec images/codebuild/delivery.yml --env HARNESS_BUCKET=... --env REPO_URI=... --env REPO_GATEWAY_SHA256=... --env BUNDLE_VALIDATOR_SHA256=... --env PACKAGER_SHA256=...
```

## Child DOX Index

None.
