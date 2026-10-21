#!/usr/bin/env bash
# Symlink fleet-worker.md into ~/.claude/agents/. Safe to run again.
set -euo pipefail

src="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)/fleet-worker.md"
dest_dir="${HOME}/.claude/agents"
dest="${dest_dir}/fleet-worker.md"

if [ ! -f "$src" ]; then
  echo "error: source not found: $src" >&2
  exit 1
fi

mkdir -p "$dest_dir"

if [ -L "$dest" ]; then
  current="$(readlink "$dest")"
  if [ "$current" = "$src" ]; then
    echo "already installed: $dest -> $src (no change)"
    exit 0
  fi
  ln -sfn "$src" "$dest"
  echo "updated symlink: $dest -> $src (was -> $current)"
  exit 0
fi

if [ -e "$dest" ]; then
  echo "error: $dest exists and is not a symlink; refusing to overwrite it" >&2
  echo "move or remove it yourself, then run this script again" >&2
  exit 1
fi

ln -s "$src" "$dest"
echo "installed: $dest -> $src"
