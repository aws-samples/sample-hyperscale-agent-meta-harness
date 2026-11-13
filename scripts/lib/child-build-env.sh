# Sourced as the first command of every buildspec under images/codebuild: `-. scripts/lib/child-build-env.sh`.
# POSIX sh, because CodeBuild runs the commands of a buildspec in its default shell and keeps them in one shell instance, so the export below
# reaches every command after this one.
#
# scripts/codebuild-run.sh puts the generated account file a deploy uses (HAMH_ACCOUNTS_FILE) into the source bundle as
# .hamh-build/accounts.json (scripts/lib/source_bundle.py) and tells the build its SHA-256 in HAMH_ACCOUNTS_FILE_SHA256. This file checks the
# member against the digest and exports HAMH_ACCOUNTS_FILE with its absolute path, so a script of the repository that reads the account
# selection (scripts/lib/aws_target.py) reads the file the deploy used and not the committed accounts.json of the archive.
# Without HAMH_ACCOUNTS_FILE_SHA256 it does nothing. A missing member or a digest that does not match fails the build.
if [ -n "${HAMH_ACCOUNTS_FILE_SHA256:-}" ]; then
  _ham_member="${CODEBUILD_SRC_DIR:-$PWD}/.hamh-build/accounts.json"
  case "$HAMH_ACCOUNTS_FILE_SHA256" in
    *[!0-9a-f]* | "") _ham_err="HAMH_ACCOUNTS_FILE_SHA256 is not a sha256 digest" ;;
    *) if [ "${#HAMH_ACCOUNTS_FILE_SHA256}" -ne 64 ]; then
         _ham_err="HAMH_ACCOUNTS_FILE_SHA256 is not a sha256 digest"
       elif [ ! -f "$_ham_member" ]; then
         _ham_err="the source bundle has no .hamh-build/accounts.json ($_ham_member)"
       else
         _ham_got=$(sha256sum "$_ham_member" | cut -d ' ' -f 1)
         if [ "$_ham_got" = "$HAMH_ACCOUNTS_FILE_SHA256" ]; then
           _ham_err=""
         else
           _ham_err="sha256 of .hamh-build/accounts.json is $_ham_got, not the $HAMH_ACCOUNTS_FILE_SHA256 the deploy named"
         fi
       fi ;;
  esac
  if [ -n "$_ham_err" ]; then
    echo "child-build-env: $_ham_err" >&2
    unset _ham_member _ham_err _ham_got
    return 1 2>/dev/null || exit 1
  fi
  HAMH_ACCOUNTS_FILE="$_ham_member"
  export HAMH_ACCOUNTS_FILE
  unset _ham_member _ham_err _ham_got
fi
