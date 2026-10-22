#!/usr/bin/env bash
# Tests for the stopgap watcher. Fakes fleet-check and the notifier; never touches the fleet.
#   stopgap/test.sh             run the tests
#   stopgap/test.sh --selftest  also break copies of the script on purpose and require red
set -u
here="$(cd "$(dirname "$0")" && pwd)"
py="${PYTHON:-python3}"

"$py" -I "$here/tests/test_stopgap.py" || { echo "FAIL: tests"; exit 1; }
[ "${1:-}" = "--selftest" ] || exit 0

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
bad=0
# name | line in the script | replacement
mutants=(
  "3-of-5 rule removed (one failure alerts)|THRESHOLD = 3 |THRESHOLD = 1 "
  "alert needs 4 of 5 (no alert at 3)|THRESHOLD = 3 |THRESHOLD = 4 "
  "recover after 1 pass (flap re-announced)|RECOVER = 2 |RECOVER = 1 "
  "window not cleared on recovery|state[\"engines\"][n][\"recent\"] = []|pass"
  "slow counts as failed|FAIL_STATUSES = {\"down\", \"wrong\"}|FAIL_STATUSES = {\"down\", \"wrong\", \"slow\"}"
  "history not written|fh.write(json.dumps(line, sort_keys=True) + \"\\n\")|pass"
)
for m in "${mutants[@]}"; do
  IFS='|' read -r name from to <<<"$m"
  copy="$tmp/fleet-stopgap.py"
  "$py" -I - "$here/fleet-stopgap.py" "$copy" "$from" "$to" <<'PY' || { echo "FAIL: mutation '$name' did not apply"; bad=1; continue; }
import sys
src, dst, a, b = sys.argv[1:]
t = open(src).read()
assert t.count(a) == 1, f"pattern found {t.count(a)} times"
open(dst, "w").write(t.replace(a, b))
PY
  if FLEET_STOPGAP_SCRIPT="$copy" "$py" -I "$here/tests/test_stopgap.py" >/dev/null 2>&1; then
    echo "FAIL: tests stayed green against broken copy: $name"; bad=1
  else
    echo "ok: tests go red when: $name"
  fi
done
exit $bad
