# images/lang-test

## Purpose

The language test container: an Amazon Linux 2023 image for a Linux build host, in which the language tool set (`images/tools/lang/install.sh`: Node 24, Go, the JDK, Maven) installs and runs, because Node 24 needs glibc 2.28 and some build hosts have 2.26. `scripts/lib/gomod_store.py` finds the Go toolchain installed with it.

## Ownership

Owned by `images/AGENTS.md`. Built by `scripts/gates-lang-tools.sh`; the image is not deployed and never reaches a stage.

## Local Contracts

- `Dockerfile`: the worker image's Amazon Linux 2023 base (pinned by digest), git, gcc, glibc-devel and the archive tools, then
  `images/services/install.sh` through the named build context `services`, then the MariaDB client. `scripts/gates-lang-tools.sh` tags the
  image `hamh-gates-lang:<12 hex of the SHA-256 over this Dockerfile, images/services/install.sh and services.lock>`, so a changed input
  rebuilds it once per host.
- The worker image key (`scripts/lib/digests.py` IMAGE_PATHS) does not hash this directory.

## Work Guidance

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest
scripts/gates-lang-tools.sh   # builds the container and installs the language tool set into it
```

## Child DOX Index

None.
