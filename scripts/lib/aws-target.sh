#!/usr/bin/env bash
# Sourced by the script wrappers after lib/python.sh (which sets $py): exports HAMH_ACCOUNT, HAMH_REGION,
# HAMH_PROFILE and AWS_PROFILE from the accounts.json entry HAMH_AWS_PROFILE names by profile, else the
# active entry. An inherited AWS_PROFILE never selects.
# HAMH_ACCOUNTS_FILE, an absolute path, names the accounts file to read in place of the committed accounts.json; it must be exported to
# reach this library, and a relative path, a missing file or a directory is a refusal like any other.
# A refusal (unknown profile, malformed accounts.json, two different values for a selector under its two spellings) prints `aws_target: <reason>` and exits 2.
# COMPAT-CAH: the eval also assigns the previous spellings CAH_ACCOUNT, CAH_REGION and CAH_PROFILE to the sourcing shell for the window of the previous names; they are
# not exported, so no child process sees them.
ham_target=$("$py" "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/aws_target.py" shell) || exit 2
eval "$ham_target"
export HAMH_ACCOUNT HAMH_REGION HAMH_PROFILE AWS_PROFILE
unset ham_target
