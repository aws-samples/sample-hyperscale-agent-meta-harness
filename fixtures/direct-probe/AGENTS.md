# fixtures/direct-probe

## Purpose

The script `sandbox_probe.py` probes engine isolation inside the Landlock domain or as the engine user.
It attempts to read a private file and the secrets process's environment, memory and maps.
It attempts to signal that process, inherit a descriptor and see a canary value whose SHA-256 is the only canary data the driver passes.
It attempts connections to agentd's own port and the metadata address.
It probes paths around the connect rule with TCP Fast Open, a multipath TCP socket and io_uring.
It probes another process's SysV shared-memory segment or message queue only when `--sysv-shm-id` or `--sysv-msg-id` is given.
It checks the launcher's refusal of a policy with `minAbi` 99 that no kernel can satisfy.
Its required working controls are temp files in the run's folder, `/tmp` and `/var/tmp`, a multiprocessing lock, loopback with bind to port 0, and an exec.

## Ownership

Owned by `fixtures/AGENTS.md`.
The probe is run by `crates/agentd/tests/sandbox_probe.rs` and `crates/agentd/tests/sandbox_ipc_network.rs`.

## Local Contracts

- You run the probe through the engine launcher: `agentd sandbox-exec --policy-json <json> -- python3 sandbox_probe.py <options>`.
  Required options are `--mode uid|landlock`, `--private-file`, `--secret-pid`, `--signal-pid`, `--canary-sha256` and `--launcher`.
  Optional options are `--agentd-port` (default 8080), `--sysv-shm-id`, `--sysv-msg-id` and `--out`.
- The report schema is `hamh-sandbox-probe/1`: `{schema, mode, uid, failed, unknown, checks}`.
  Each check is `{status, detail}`, with `status` set to `pass`, `fail`, `unknown` or `info`.
  The lists `failed` and `unknown` contain the check names with those statuses.
  The script prints the report as JSON and also writes it to `--out` when given.
  It exits 1 when `failed` or `unknown` is not empty, else 0.
- A refusal check passes when the action fails with `EACCES` or `EPERM`.
  It fails when the action works.
  Any other error is `unknown`.
  Connections to agentd's port and the metadata address must be refused in `landlock` mode.
  Those connections are reported only in `uid` mode.
- The script uses only the Python standard library.
  It prints and stores no secret.

## Work Guidance

- You add a row to `checks` in `main()` for each new check.
- You add a case to `crates/agentd/tests/sandbox_probe.rs` that shows the row fails when run unconfined.

## Verification

```bash
scripts/cargo-slot.sh cargo nextest run -p agentd --test sandbox_probe --test sandbox_ipc_network
```

## Child DOX Index

None.
