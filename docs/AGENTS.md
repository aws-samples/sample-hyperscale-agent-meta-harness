# docs

## Purpose

This folder holds the user guide for `ham`, the terminal app.

`docs/user-guide.md` covers install, first run, and the chat screen and its commands.
It also covers settings, rules, skills, delivery, and recovery.

`docs/images/` holds the pictures used in the README.

The root `README.md` and `setup.sh` belong to this documentation set.

## Ownership

The root `AGENTS.md` owns this folder.

## Local Contracts

- Each document states only what the code at its head does.
- A document states what the code does in the present tense and states a limit as a fact. It carries no statement about how the project tested the behavior, no date, no release name and no tracking id.
- No document says the harness verified, tested, gated, or evaluated delivered work.
  The outcome labels are frozen text from `crates/harness-protocol/src/direct.rs`:
  - "Cline reported done"
  - "Cline did not start"
  - "Cline stopped: <reason>"
- Relative links and file paths must resolve.
- Documents must contain no secrets, persistent passwords, or real account numbers.
- Pages must say `your AWS CLI profile`.
  Use placeholders such as `<your-profile>`, `<stage account>`, and `<build-host>`.
- A page must never call a built command unbuilt.

## Work Guidance

Use plain words.
Keep sentences short.
Use concrete numbers.

## Verification

The command `python3 scripts/dox_lint.py` must print DOX PASS.

## Child DOX Index

| Path | Covers |
| --- | --- |
| `docs/images/AGENTS.md` | The README pictures: the architecture picture and the ham mascot, their sources, and how they are regenerated. |
