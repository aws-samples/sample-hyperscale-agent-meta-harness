# images/services

## Purpose

The native services the worker image of stage `direct` carries: PostgreSQL 16, MariaDB 11.4, Valkey 9 and mailpit 1.31.2.
`images/worker-python/ham-services.sh` starts PostgreSQL 16 from here (`/usr/bin/postgres`, `initdb`, `psql`) and RabbitMQ from
`images/worker-python/broker.lock`; nothing in the line starts MariaDB, Valkey or mailpit, so they are a trim candidate: dropping their
rpm, lib and mailpit rows, the `svc` user and the build-spec version checks frees an estimated 90 to 100 MiB (mailpit alone is 26 MB) of
image size, and changes the image key.

## Ownership

Owned by `images/AGENTS.md`. Consumer: `images/worker-python/Dockerfile`.

## Local Contracts

- `services.lock` rows, five columns each: `rpm <package> <version-release> any -` (Amazon Linux
  2023 packages, installed without their dependency closure), `lib <package> - any -` (the shared libraries
  the servers link, installed with dnf), `mailpit <version> <arch> <tarball SHA-256> <url>` and
  `mailpit-bin <version> <arch> <binary SHA-256> -` per architecture (x86_64, aarch64).
- `install.sh [<download cache>]` (root, Amazon Linux 2023; the dnf runs and the mailpit download are run again after a failure, 4
  attempts with pauses of 5, 15 and 45 seconds, by the copy of `images/tools/fetch.sh`'s `retry-lib` block it
  carries because this directory is bind-mounted alone; `scripts/tests/test_fetch_retry.py` keeps the copy equal): installs the lib rows with dnf, then
  the rpm rows with `rpm -i --nodeps --noscripts --excludedocs` (no systemd, perl, package users or
  units), removes every program of those packages under `/usr/bin`, `/usr/sbin` and
  `/usr/libexec` that the native start does not run (it keeps postgres, initdb, psql, pg_isready,
  mariadbd, mariadb-install-db, my_print_defaults, resolveip, valkey-server), the units, the
  MariaDB plugins and every message language but English, checks mailpit's tarball and binary
  SHA-256, requires every kept binary to resolve its libraries (`ldd`), writes
  `/opt/harness/services/manifest.json` (arch, versions, the lock's SHA-256) and ends
  `SERVICES PASS`. It never runs `mailpit version`, which asks GitHub for updates.
- The worker image runs it after its package installs (no later dnf transaction sees the packages
  without their dependencies), adds uid `svc` 904 (no login shell, owns nothing) and
  `/var/lib/harness-services` (0711), and records the manifest's SHA-256 in
  `/opt/harness/pins.json` (`services`). The worker image key (`scripts/lib/digests.py`) hashes this directory.
- Size on Amazon Linux 2023 (`du` of the root before and after): +131 MiB on x86_64, +136 MiB on aarch64 (libicu, which
  PostgreSQL links, is 35 MB of it; mailpit 26 MB).

## Work Guidance

- Change a pin only with its measured SHA-256 or an available package version of the pinned base
  image's repository.

## Verification

```bash
uv run --no-project --python 3.12 python -m unittest
scripts/gates-lang-tools.sh   # builds images/lang-test, whose Dockerfile runs install.sh
```

## Child DOX Index

None.
