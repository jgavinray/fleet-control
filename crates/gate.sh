#!/bin/bash
# Gate for the Rust port of fleet-check (task T5). Run from the repo root:
#   bash crates/gate.sh
# Red until crates/fleet-core and crates/fleet-check-rs exist and behave:
#   1. builds the workspace in release mode and passes `cargo test` (at least
#      8 tests per crate), `cargo clippy --all-targets -- -D warnings` and
#      `cargo fmt --check`
#   2. agrees with the Python fleet-check on a fixture registry served by mock
#      HTTP engines on loopback (crates/parity.py): --quick --json and full
#      mode, tensorfold plus the laya, lfm2d and jev-glm kinds, exit code,
#      engine order, the 14-field comparable projection and whole feature
#      objects; any stderr on a normal run fails
#   3. changes nothing outside crates/ (checked against the merge-base),
#      apart from the root .gitignore, which the ignore check below depends on,
#      and the two workspace manifests (Cargo.toml, Cargo.lock), which adding a
#      workspace member requires
#
# No live engine is ever contacted: the parity harness serves mock engines on
# loopback only. The Python tool stays ground truth and is never modified.
set -u
fail=0
bad() { echo "GATE FAIL: $*" >&2; fail=1; }
HERE=$(cd "$(dirname "$0")" && pwd)          # crates/ — where parity.py lives
ROOT=$(cd "$HERE/.." && pwd)                 # repo root (the gate sits one level down)
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export DEVELOPER_DIR=/Library/Developer/CommandLineTools      # git works through CLT, not Xcode
export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk  # default SDK is malformed

CORE=crates/fleet-core
BINCRATE=crates/fleet-check-rs

[ -f "$ROOT/check/fleet_check.py" ] || { echo "GATE FAIL: the Python fleet-check is gone" >&2; exit 1; }
missing=""
[ -f "$ROOT/$CORE/Cargo.toml" ] || missing="$missing $CORE/Cargo.toml"
[ -f "$ROOT/$BINCRATE/Cargo.toml" ] || missing="$missing $BINCRATE/Cargo.toml"
if [ -n "$missing" ]; then
  echo "GATE FAIL: no Rust binary to gate:$missing does not exist (red until the crate lands)" >&2
  echo "GATE FAIL: skipped: cargo build --release, cargo test, clippy, fmt --check, parity, scope check" >&2
  exit 1
fi

( cd "$ROOT" && cargo build --release --quiet 2>&1 | tail -20; exit "${PIPESTATUS[0]}" ) || { echo "GATE FAIL: cargo build --release failed" >&2; exit 1; }
BIN=""
for cand in "$ROOT/target/release/fleet-check-rs" "$ROOT/$BINCRATE/target/release/fleet-check-rs"; do
  if [ -x "$cand" ]; then BIN="$cand"; break; fi
done
[ -n "$BIN" ] || { echo "GATE FAIL: no binary at target/release/fleet-check-rs (build failed or the bin is not named fleet-check-rs)" >&2; exit 1; }

for p in fleet-core fleet-check-rs fleet-mcp; do
  t=$(cargo test -p "$p" 2>&1); st=$?
  echo "$t" | grep -E "^test result|error" | head -8
  [ "$st" -eq 0 ] || bad "cargo test -p $p failed"
  passed=$(echo "$t" | awk '/^test result:/ {p += $4} END {print p + 0}')
  [ "$passed" -ge 8 ] || bad "only $passed tests in $p; at least 8 are required"
done
( cd "$ROOT" && cargo clippy -p fleet-core -p fleet-check-rs -p fleet-mcp --all-targets --quiet -- -D warnings 2>&1 | tail -20; exit "${PIPESTATUS[0]}" ) || bad "clippy -D warnings failed"
( cd "$ROOT" && cargo fmt --check >/dev/null 2>&1 ) || bad "cargo fmt --check failed"

python3 -I "$HERE/parity.py" "$ROOT/check/fleet_check.py" "$BIN" || bad "parity with the Python fleet-check failed"

# No build artifacts committable. The root .gitignore must carry a `target`
# pattern without a trailing slash: a directory-only pattern cannot match a
# path that does not exist, and the nested target dirs never do in a workspace.
for t in "$ROOT/target" "$ROOT/$BINCRATE/target" "$ROOT/$CORE/target"; do
  git check-ignore -q "$t" || bad "$t is not git-ignored (add target to the root .gitignore)"
done

# Diff against the branch point on local main. origin/main can lag main, and
# an origin/main-first base would flag already-merged main work (the workspace
# Cargo.toml/Cargo.lock) as out-of-scope changes of the branch under test.
base=$(git merge-base HEAD main 2>/dev/null || git merge-base HEAD origin/main 2>/dev/null || git rev-parse HEAD)
# Scope: committed or uncommitted work against the merge-base. Only the new
# crates may change, plus the two workspace manifests that a new member
# requires; the Python tool, fleet.toml and everything else stay as they are.
# The gate itself and parity.py live under crates/ and are allowed, as is the
# root .gitignore the ignore check above depends on.
changed=$( { git diff --name-only "$base"; git ls-files --others --exclude-standard; } | sort -u)
for f in $changed; do
  case "$f" in
    crates/*|.gitignore|Cargo.toml|Cargo.lock) ;;  # manifests: adding a workspace member needs them
    *) bad "file outside crates/ changed: $f" ;;
  esac
done

if [ "$fail" -ne 0 ]; then echo "GATE: FAIL" >&2; exit 1; fi
echo "GATE: PASS"
