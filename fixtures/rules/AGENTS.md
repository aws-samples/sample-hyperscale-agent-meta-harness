# fixtures/rules

## Purpose

Test data of the default rules file: `default-earlier/` holds the 23 earlier texts of `crates/ham-core/assets/default-rules.md`, the ones `ham rules init` replaces when a machine still has one of them unedited.

## Ownership

Owned by `fixtures/AGENTS.md`. Read by `crates/ham-core/tests/rules_history.rs` and `crates/ham/tests/rules_default_refresh.rs`.

## Local Contracts

- Each file in `default-earlier/` is named `<date>-<commit>-00-my-rules.md.txt`: the date the text became the default and the commit that introduced it. Its bytes are `git show <commit>:crates/ham-core/assets/default-rules.md`. The `.txt` suffix keeps tools from reading it as a live rule.
- The SHA-256 of every file is listed in `crates/ham-core/assets/default-rules.history.json` with the date from its name, and the history lists nothing without a file here. `crates/ham-core/tests/rules_history.rs` checks both directions.
- Every file in `default-earlier/` is a text of the default rules that an earlier `ham` wrote with `ham rules init`. `ham rules init` replaces a copy that was not edited and leaves an edited one alone.
- `fixtures/lifecycle/baseline-rules/00-my-rules.md` is also one of these texts, byte for byte.
- Never edit a file here: it is what an older `ham` wrote.

## Work Guidance

- When `crates/ham-core/assets/default-rules.md` changes, save the text it replaces here, add its hash and date to `default-rules.history.json`, and set `CURRENT_SHA256` in `crates/ham-core/tests/rules_history.rs` to the new text. `a_change_to_the_default_rules_must_bring_the_text_it_replaces_into_the_history` fails until all three are done.

## Verification

```bash
scripts/cargo-slot.sh cargo nextest run -p ham-core --test rules_history
scripts/cargo-slot.sh cargo nextest run -p ham --test rules_default_refresh
```

## Child DOX Index

None.
