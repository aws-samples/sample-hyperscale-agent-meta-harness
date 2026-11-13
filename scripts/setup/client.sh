# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): getting the client from the user's own bucket.
#
# The runner builds the client and the operator tool beside it for macOS on Apple silicon and for Linux (x86_64 and arm64) and writes them to the
# bucket of the user's account, with a SHA256SUMS file and the digests in result.json. setup.sh downloads them with the user's own credentials and installs
# a file only when its SHA-256 is the one result.json names AND the one SHA256SUMS lists, for both files: when either fails, neither is installed. This
# script holds no URL and no checksum of its own: the trust rests on the pinned source commit that was built inside the user's account.
#
#   cl_platform                         prints the platform key of this machine (darwin-arm64, linux-amd64, linux-arm64) or returns 1
#   cl_fetch <result-flat> <bucket> <build id> <platform>
#                                       downloads and checks the client and the operator tool; on success CL_DIR is a folder that holds both and a SHA256SUMS
#                                       with a line for each, in the form `setup.sh --from` reads, CL_SHA is the digest of the client and CL_ADMIN_SHA the one of
#                                       the operator tool; returns 1 after FAIL text on stderr and leaves CL_DIR empty

CL_DIR=""
CL_SHA=""
CL_ADMIN_SHA=""

cl_platform() {
  local os arch
  os=$(uname -s 2>/dev/null || echo unknown)
  arch=$(uname -m 2>/dev/null || echo unknown)
  case "$os/$arch" in
    Darwin/arm64) echo darwin-arm64 ;;
    Linux/x86_64|Linux/amd64) echo linux-amd64 ;;
    Linux/aarch64|Linux/arm64) echo linux-arm64 ;;
    Darwin/x86_64)
      # uname -m says x86_64 on an Intel Mac and in a terminal that an Apple silicon Mac runs under Rosetta; the kernel tells them apart (1: translated by Rosetta)
      if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
        err "this terminal runs under Rosetta: uname -m says x86_64, and sysctl -n sysctl.proc_translated prints 1. The runner builds the client for Apple silicon (darwin-arm64)."
        detail "Open a terminal that does not run under Rosetta (or start one with  arch -arm64 /bin/bash ), check that  uname -m  says arm64, and run setup there."
      else
        err "this Mac has an Intel processor (uname -m says x86_64, and sysctl -n sysctl.proc_translated does not print 1, so it is not Rosetta). The runner builds the client for Apple silicon Macs and for Linux."
      fi
      detail "To set up from this Mac, deploy and compile the client here:  ./setup.sh --${FULL_ACTION:-full} --build-on local  (it needs cargo, cargo-zigbuild, zig, node, docker and Python 3.11)."
      detail "Or install a client built elsewhere with  setup.sh --client-only --from DIR  (a directory with the client and its SHA256SUMS). Nothing was created."
      return 1 ;;
    *)
      err "the runner builds the client for macOS on Apple silicon and for Linux (x86_64 and arm64); this machine is $os $arch."
      detail "Build a client elsewhere and install it here with  setup.sh --client-only --from DIR  (a directory with the client and its SHA256SUMS),"
      detail "or compile it here with  setup.sh --client-only --build."
      return 1 ;;
  esac
}

# cl_flat_get <flat text> <path>: the value of one path in the text json_flat printed.
cl_flat_get() {
  printf '%s\n' "$1" | awk -F'\t' -v p="$2" '$1 == p { print $2; exit }'
}

# cl_sums_get <SHA256SUMS text> <name>...: the digest listed for the first of those names (a path as `sha256sum` writes it, with or without ./ and *).
cl_sums_get() {
  local text=$1 name found
  shift
  for name in "$@"; do
    found=$(printf '%s\n' "$text" | awk -v n="$name" '{ f = $2; sub(/^\*/, "", f); sub(/^\.\//, "", f); if (f == n && length($1) == 64) { print tolower($1); exit } }')
    if [ -n "$found" ]; then
      printf '%s' "$found"
      return 0
    fi
  done
  return 1
}

# cl_get_checked <label> <name> <want> <key> <platform> <build> <bucket> <SHA256SUMS text> <dest dir>: download one file of the build and check it against the SHA-256
# result.json names (<want>) and against the manifest. <label> is "client" or "operator tool", <name> the file name, <key> its object key. Sets CL_GOT (its digest) and returns 0,
# or prints the FAIL text and returns 1.
CL_GOT=""
cl_get_checked() {
  local label=$1 name=$2 want=$3 key=$4 platform=$5 build=$6 bucket=$7 sums_text=$8 dest=$9
  local got sums_want
  CL_GOT=""
  if [ -z "$want" ] || [ -z "$key" ]; then
    fail "the result of the build lists no $label for $platform" "Nothing was installed." \
      "Run  ./setup.sh --update  to build again in your account (the runner is installed again for it), or install a client you have with  setup.sh --client-only --from DIR."
    return 1
  fi
  case $want in
    *[!0-9a-f]*) fail "the result of the build names a SHA-256 for the $label that is not 64 hexadecimal digits" "got: $want"; return 1 ;;
  esac
  [ "${#want}" -eq 64 ] || { fail "the result of the build names a SHA-256 for the $label that is not 64 hexadecimal digits" "got: $want"; return 1; }
  if ! aws_run s3 cp "s3://$bucket/$key" "$dest/download/$name"; then
    fail "could not download $key from your bucket $bucket" "$AWS_ERR"
    return 1
  fi
  got=$(setup_sha256 <"$dest/download/$name")
  if ! sums_want=$(cl_sums_get "$sums_text" "$platform/$name" "$name" "$key" "${key#client/"$build"/}"); then
    fail "client/$build/SHA256SUMS lists no entry for $platform/$name" "Nothing was installed."
    return 1
  fi
  if [ "$got" != "$want" ]; then
    fail "the downloaded $label ($name) does not match the SHA-256 that result.json names" "expected $want" "got      $got" "Nothing was installed. The file stays at $dest/download/$name for you to look at."
    return 1
  fi
  if [ "$sums_want" != "$want" ]; then
    fail "SHA256SUMS and result.json name different SHA-256 values for the $label ($name)" "result.json $want" "SHA256SUMS  $sums_want" "Nothing was installed."
    return 1
  fi
  CL_GOT=$got
  return 0
}

cl_fetch() {
  local flat=$1 bucket=$2 build=$3 platform=$4 bin=$HAMH_CLIENT_BIN admin=$HAMH_ADMIN_BIN
  local want key admin_want admin_key sums_text dest client_sha admin_sha
  CL_DIR=""
  CL_SHA=""
  CL_ADMIN_SHA=""
  want=$(cl_flat_get "$flat" "client.$platform.sha256")
  key=$(cl_flat_get "$flat" "client.$platform.key")
  admin_want=$(cl_flat_get "$flat" "client.$platform.admin.sha256")
  admin_key=$(cl_flat_get "$flat" "client.$platform.admin.key")
  if [ -z "$want" ] || [ -z "$key" ]; then
    fail "the result of the build lists no client for $platform" "The runner built: $(printf '%s\n' "$flat" | awk -F'\t' '$1 ~ /^client\.[^.]*\.sha256$/ { split($1, a, "."); printf "%s ", a[2] }')" \
      "Build a client elsewhere and install it with  setup.sh --client-only --from DIR."
    return 1
  fi
  dest=$ST_DIR/download
  mkdir -p "$dest/download" "$dest/verified" || { fail "could not make $dest"; return 1; }
  if ! aws_run s3 cp "s3://$bucket/client/$build/SHA256SUMS" "$dest/download/SHA256SUMS"; then
    fail "could not download client/$build/SHA256SUMS from your bucket $bucket" "$AWS_ERR"
    return 1
  fi
  sums_text=$(cat "$dest/download/SHA256SUMS")
  cl_get_checked "client" "$bin" "$want" "$key" "$platform" "$build" "$bucket" "$sums_text" "$dest" || return 1
  client_sha=$CL_GOT
  cl_get_checked "operator tool" "$admin" "$admin_want" "$admin_key" "$platform" "$build" "$bucket" "$sums_text" "$dest" || return 1
  admin_sha=$CL_GOT
  # Both files passed: only now do they enter the folder that setup.sh installs from.
  cp "$dest/download/$bin" "$dest/verified/$bin" || { fail "could not copy the checked client into $dest/verified"; return 1; }
  cp "$dest/download/$admin" "$dest/verified/$admin" || { fail "could not copy the checked operator tool into $dest/verified"; return 1; }
  chmod 755 "$dest/verified/$bin" "$dest/verified/$admin"
  printf '%s  %s\n%s  %s\n' "$client_sha" "$bin" "$admin_sha" "$admin" >"$dest/verified/SHA256SUMS" || { fail "could not write $dest/verified/SHA256SUMS"; return 1; }
  CL_DIR=$dest/verified
  CL_SHA=$client_sha
  CL_ADMIN_SHA=$admin_sha
  return 0
}
