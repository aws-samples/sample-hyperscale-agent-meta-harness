# crates/ham-admin

## Purpose

The operator's command line: repository allowlist
administration through `/v1/admin/repos`. `ham` never writes the allowlist.

## Ownership

Owned by `crates/AGENTS.md`. It reuses the `ham` configuration file and SigV4 client
(`ham::cli::configured_api`, `ham::api`).

## Local Contracts

- `ham-admin repos allow <target>`, `ham-admin repos deny <target>`,
  `ham-admin repos list`; a target is `cc:`, `gh:` or `gl:` (`harness_protocol::target`).
- `ham-admin --version` (`-V`) prints `ham-admin <crate version>` and `ham-admin --help` (`-h`) prints the usage
  line, each alone on stdout with exit 0, before the `ham` config file is read or any call is made; an
  argument after the flag is a usage error.
- Exit 0 when done (`allowed <target> (added by <arn> at <time>)`, `denied <target>`, or one
  line per entry); 1 when the API refused or failed, with its exact reason on stderr
  (`ham-admin: refused: ...`); 2 on a usage error, before any call.
- control-api allows `/v1/admin/*` only for its `OPERATOR_PRINCIPALS` and
  refuses, with the reason, what `repo-gateway` would refuse: a `cc:` name outside `hamh-e2e-`.
- The full setup (`setup.sh --full`) builds and installs `ham-admin` beside `ham` (`scripts/setup/AGENTS.md`); the operator needs no Rust toolchain.
- `ham-admin` has no alias under the tool's earlier name. Operators build the binary from the checkout.

## Work Guidance

- Tests run the binary against the local `/v1` API over the real control-api handler
  (`crates/ham/tests/support`, included by `#[path]`).

## Verification

```bash
scripts/cargo-slot.sh cargo test -p ham-admin
```

## Child DOX Index

None.
