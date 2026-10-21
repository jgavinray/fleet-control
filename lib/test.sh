#!/usr/bin/env bash
# fleetlib gate. Exits non-zero on any failure.
#   1. the unit tests (redact, logs, classify + registry, against a stub server)
#   2. the mutation check: the redact suite must FAIL when any single rule is removed
#   3. the repo-clean check: nothing under fleet/lib trips the library's own redactor
# Usage: fleet/lib/test.sh        (Python 3.11+, standard library only)
set -euo pipefail
cd "$(dirname "$0")"
PY="${PYTHON:-python3}"
"$PY" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' || { echo "fleetlib needs Python 3.11+" >&2; exit 2; }
export PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$PWD"

echo "== unit tests"
"$PY" -m unittest discover -s tests -t . -v 2>&1 | tee /dev/stderr | tail -3 >/dev/null
echo "== mutation check (remove each redact rule, the suite must fail)"
"$PY" tests/mutation_check.py
echo "== repo-clean check (the redactor finds nothing in fleet/lib)"
"$PY" tests/repo_clean.py
echo "fleetlib gate: OK"
