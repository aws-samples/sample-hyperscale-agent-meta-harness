# fixtures/lifecycle

## Purpose

Test data of the lifecycle validation: the baseline rule file and the earlier texts of the lifecycle pack files that tests compare against.

## Ownership

Owned by `fixtures/AGENTS.md`. Read by `crates/ham-e2e` (the driver and its tests).

## Local Contracts

- `pack-earlier/` holds earlier lifecycle pack texts: `AGENTS.md`, its `full` and `quick` variants, the delivery, loop, check and test-first (`lifecycle-tdd`) skills, `lifecycle_check.py` and `scanner_kit.py`. Texts come from `git show`; `.txt` suffixes prevent tools from reading them as live skills or scripts. A name starts with the pack version of the text and then names the file, for example `1.0.8-AGENTS.md.txt`. `ham rules init` tests install them at older `ham` locations: unedited files are replaced; edited files are kept. Do not edit them: a changed byte is no longer an earlier pack text.
- `research-judge-output-keyless.txt` is the same output for a keyless run of the repository sub-leg (`--without-key`): E0, E3, E4 and E13 not run, the key-free checks passing, E11 included since the judges' collection sends the gate probe; the verdict line is the one a keyless run prints.

## Work Guidance

- A new field of the judges' lines is added here first, with a test of the parser that fails without it.

## Verification

## Child DOX Index

None.
