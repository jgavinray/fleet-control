#!/bin/bash
# Parity-harness helper: runs the Python fleet-check itself as the "rust" side.
# Proves the harness agrees with ground truth end-to-end before any Rust
# binary exists. Allowed under crates/ by the gate's scope rule.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
exec python3 -I "$ROOT/check/fleet_check.py" "$@"
