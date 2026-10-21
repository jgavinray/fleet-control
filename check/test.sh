#!/usr/bin/env bash
# Gate for fleet-check. One command, non-zero on any failure.
#
#   fleet/check/test.sh              run the unit tests (stub HTTP servers, no real fleet)
#   fleet/check/test.sh --selftest   also prove the tests can fail: break a copy of the
#                                    code on purpose and require the tests to go red
#
# Covers: logic of the checker and the shape of fleet/fleet.toml.
# Does NOT cover: the real engines. For that, run `fleet-check` itself.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

run_tests() {
  python3 -m unittest discover -s "$1/tests" -p 'test_*.py' "${@:2}"
}

echo "== unit tests"
run_tests "$here"

echo "== launcher smoke"
"$here/fleet-check" --help >/dev/null
bad=0
"$here/fleet-check" --registry /nonexistent.toml >/dev/null 2>&1 || bad=$?
[ "$bad" -eq 1 ] || { echo "FAIL: missing registry should exit 1, got $bad"; exit 1; }

if [ "${1:-}" = "--selftest" ]; then
  echo "== selftest: each deliberate break must make the tests fail"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  mutate() { # name, python expression that edits the source text s
    rm -rf "$tmp/c"; mkdir -p "$tmp/c"
    cp -R "$here/." "$tmp/c/"
    cp "$here/../fleet.toml" "$tmp/fleet.toml"
    python3 - "$tmp/c/fleet_check.py" "$2" <<'PY'
import sys
p, old_new = sys.argv[1], sys.argv[2]
old, new = old_new.split("=>", 1)
s = open(p).read()
assert old in s, "mutation target not found: " + old
open(p, "w").write(s.replace(old, new, 1))
PY
    if run_tests "$tmp/c" >/dev/null 2>&1; then
      echo "FAIL: tests stayed green after break '$1'"; exit 1
    fi
    echo "ok: tests went red for break '$1'"
  }
  mutate "drift exits 0"          '    if any(r.verdict == "drift" for r in results):
        return 2=>    if any(r.verdict == "drift" for r in results):
        return 0'
  mutate "wrong answer accepted"  'r"\W*42\W*"=>r"\W*\d+\W*"'
  mutate "down engine not fatal"  'if any(r.verdict in ("down", "wrong") for r in results):
        return 1=>if any(r.verdict in ("wrong",) for r in results):
        return 1'
  mutate "feature hard-coded"     'if lp:
        return "yes", "logprobs returned"=>if False:
        return "yes", "logprobs returned"'
fi

echo "PASS"
