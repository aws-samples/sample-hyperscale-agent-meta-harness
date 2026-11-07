# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): the local receipt of a setup.
#
# AWS holds the authoritative state of a setup: the stack outputs, the CodeBuild build and the result manifest. The receipt only caches the
# identifiers so a repeated run finds its build at once. It holds no credential, no password and no token (every value must be a plain word, a
# commit, a digest, a build id, a name or a short sentence), it is private to the user (mode 0600) and a fresh machine does without it.
#
#   st_init <account> <region> <stage>   sets ST_DIR and ST_FILE (~/.config/<config directory>/setup/<account>/<region>/<stage>.json) and starts the data
#   st_load                              reads the file, when there is one, into the data
#   st_set <key> <value>                 sets one key; refuses (returns 1) a value with a quote, a backslash, a newline or a control character
#   st_get <key>                         prints the value of a key, nothing when it is not set
#   st_save                              writes the file; returns 1 when it cannot
#
# The data is a text of `key=value` lines in ST_DATA; the file is one flat JSON object of strings.

ST_ACCOUNT=""
ST_REGION=""
ST_STAGE=""
ST_DIR=""
ST_FILE=""
ST_DATA=""

st_init() {
  ST_ACCOUNT=$1
  ST_REGION=$2
  ST_STAGE=$3
  ST_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/$HAMH_CONFIG_DIR_NAME/setup/$1/$2"
  ST_FILE="$ST_DIR/$3.json"
  ST_DATA="schema=1${NL}account=$1${NL}region=$2${NL}stage=$3"
}

# st_set <key> <value>
st_set() {
  local key=$1 val=$2 line out="" found=0
  case $key in
    ''|*[!A-Za-z0-9]*) return 1 ;;
  esac
  case $val in
    *'"'*|*\\*|*[[:cntrl:]]*) return 1 ;;
  esac
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    if [ "${line%%=*}" = "$key" ]; then
      line="$key=$val"
      found=1
    fi
    out="$out$line$NL"
  done <<<"$ST_DATA"
  [ "$found" = 1 ] || out="$out$key=$val$NL"
  ST_DATA=${out%"$NL"}
  return 0
}

st_get() {
  printf '%s\n' "$ST_DATA" | awk -v k="$1" 'index($0, k "=") == 1 { print substr($0, length(k) + 2); exit }'
}

st_load() {
  local line key val flat
  [ -f "$ST_FILE" ] || return 0
  flat=$(json_flat <"$ST_FILE")
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    key=${line%%"$TAB"*}
    val=${line#*"$TAB"}
    case $key in
      *.*) continue ;;
    esac
    st_set "$key" "$val" || true
  done <<<"$flat"
}

st_save() {
  local line key val out="" first=1 tmp
  st_set updatedAt "$(setup_now)" || true
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    key=${line%%=*}
    val=${line#*=}
    if [ "$first" = 1 ]; then first=0; else out="$out,$NL"; fi
    out="$out  \"$key\": \"$val\""
  done <<<"$ST_DATA"
  tmp=$ST_FILE.new
  (
    umask 077
    mkdir -p "$ST_DIR" || exit 1
    printf '{%s%s%s}%s' "$NL" "$out" "$NL" "$NL" >"$tmp" || exit 1
    mv -f "$tmp" "$ST_FILE" || exit 1
  )
}
