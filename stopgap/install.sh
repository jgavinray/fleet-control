#!/usr/bin/env bash
# Install or remove the stopgap LaunchAgent on this Mac.
#   stopgap/install.sh            install (or reinstall) and start it
#   stopgap/install.sh uninstall  stop it and remove the LaunchAgent (history is kept)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
label=com.jgavinray.fleet-stopgap
plist="$HOME/Library/LaunchAgents/$label.plist"
bin_dir="$HOME/.local/libexec/fleet-stopgap"
state_dir="$HOME/.local/state/fleet-stopgap"
domain="gui/$(id -u)"

launchctl bootout "$domain/$label" 2>/dev/null || true
if [ "${1:-install}" = "uninstall" ]; then
  rm -f "$plist"
  echo "stopped and removed $label; history kept in $state_dir"
  exit 0
fi

py="$(command -v python3)"
mkdir -p "$bin_dir" "$state_dir" "$HOME/Library/LaunchAgents"
# A copy outside the repo, so moving or switching the checkout cannot break the schedule.
install -m 755 "$here/fleet-stopgap.py" "$bin_dir/fleet-stopgap.py"
sed -e "s|@PYTHON@|$py|" -e "s|@PYTHON_DIR@|$(dirname "$py")|" \
    -e "s|@SCRIPT@|$bin_dir/fleet-stopgap.py|" -e "s|@STATE_DIR@|$state_dir|" \
    "$here/$label.plist.in" > "$plist"
plutil -lint "$plist" >/dev/null
launchctl bootstrap "$domain" "$plist"
echo "installed $label: runs every 2 minutes; history in $state_dir/history.jsonl"
