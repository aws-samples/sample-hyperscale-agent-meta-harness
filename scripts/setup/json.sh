# shellcheck shell=bash
# shellcheck disable=SC2034  # the variables are set here and read by setup.sh and the other helper files
# Sourced by setup.sh (never run on its own): a reader for the JSON that the runner writes (result.json, phases.jsonl) and that setup.sh writes (the
# lease, the receipt). A stock macOS has no jq and no usable python3, so the reader is one awk program.
#
#   json_flat    reads one JSON document on stdin and prints one line per value: the dotted path, a tab, the value.
#                  {"client": {"darwin-arm64": {"sha256": "ab"}}, "phases": [{"name": "x"}]}
#                  client.darwin-arm64.sha256<TAB>ab
#                  phases.0.name<TAB>x
#                A string is decoded; a newline or a tab inside it is written as the two characters \n or \t, so every value stays on one line.
#                A number, true, false and null print as they are written. An empty object or array prints nothing.
#   json_value P prints the first value of the path P from the document on stdin, nothing when it is not there.

json_flat() {
  LC_ALL=C awk '
    function fail(msg) { print "json_flat: " msg > "/dev/stderr"; bad = 1; exit 2 }
    function skip_ws(   c) {
      while (i <= n) {
        c = substr(doc, i, 1)
        if (c == " " || c == "\t" || c == "\n" || c == "\r") i++; else break
      }
    }
    function hexval(h,   k, d, v) {
      v = 0; h = tolower(h)
      for (k = 1; k <= length(h); k++) {
        d = index("0123456789abcdef", substr(h, k, 1)) - 1
        if (d < 0) return -1
        v = v * 16 + d
      }
      return v
    }
    function unicode(h,   v) {
      v = hexval(h)
      if (v >= 32 && v < 127) return sprintf("%c", v)
      return "\\u" h
    }
    function parse_string(   out, c, e) {
      i++
      out = ""
      while (i <= n) {
        c = substr(doc, i, 1)
        if (c == "\"") { i++; return out }
        if (c == "\\") {
          e = substr(doc, i + 1, 1)
          if (e == "n") out = out "\\n"
          else if (e == "t") out = out "\\t"
          else if (e == "r") out = out "\\r"
          else if (e == "b" || e == "f") out = out " "
          else if (e == "u") { out = out unicode(substr(doc, i + 2, 4)); i += 4 }
          else out = out e
          i += 2
          continue
        }
        if (c == "\t") out = out "\\t"
        else out = out c
        i++
      }
      fail("unterminated string")
    }
    function child(path, key) { return (path == "") ? key : (path "." key) }
    function parse_value(path,   c, key, idx, start, tok) {
      skip_ws()
      c = substr(doc, i, 1)
      if (c == "{") {
        i++
        skip_ws()
        if (substr(doc, i, 1) == "}") { i++; return }
        while (1) {
          skip_ws()
          if (substr(doc, i, 1) != "\"") fail("expected a key at " i)
          key = parse_string()
          skip_ws()
          if (substr(doc, i, 1) != ":") fail("expected : at " i)
          i++
          parse_value(child(path, key))
          skip_ws()
          c = substr(doc, i, 1)
          if (c == ",") { i++; continue }
          if (c == "}") { i++; return }
          fail("expected , or } at " i)
        }
      }
      if (c == "[") {
        i++
        skip_ws()
        if (substr(doc, i, 1) == "]") { i++; return }
        idx = 0
        while (1) {
          parse_value(child(path, idx))
          idx++
          skip_ws()
          c = substr(doc, i, 1)
          if (c == ",") { i++; continue }
          if (c == "]") { i++; return }
          fail("expected , or ] at " i)
        }
      }
      if (c == "\"") { tok = parse_string(); printf "%s\t%s\n", path, tok; return }
      start = i
      while (i <= n) {
        c = substr(doc, i, 1)
        if (c ~ /[-A-Za-z0-9.+]/) i++; else break
      }
      tok = substr(doc, start, i - start)
      if (tok == "") fail("unexpected character at " i)
      printf "%s\t%s\n", path, tok
    }
    BEGIN { RS = "\001" }
    { doc = doc $0 }
    END {
      if (bad) exit 2
      n = length(doc); i = 1
      skip_ws()
      if (i > n) exit 0
      parse_value("")
    }'
}

json_value() {
  json_flat | awk -F'\t' -v p="$1" '$1 == p { print $2; exit }'
}

# json_text <value as json_flat prints it>: the value with its \n and \t written out, for showing to a person.
json_text() {
  printf '%b\n' "$1"
}
