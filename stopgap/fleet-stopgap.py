#!/usr/bin/env python3
"""Stopgap fleet watcher for the owner's Mac. Disposable.

Run on a schedule (launchd, every 2 minutes). Each run:

1. runs ``fleet-check --quick --json``,
2. appends one line to the history file (so "was it up overnight?" has an answer),
3. keeps the last 5 results for each engine,
4. shows a desktop notification when an engine failed in 3 of its last 5 runs,
   and one when it has passed its last 2 runs again.

Rules, in plain words:

* Failed means fleet-check says ``down`` or ``wrong``. ``slow`` (a busy engine that
  did not answer in time), ``skipped-busy`` and feature drift are not failures.
* One alert per outage. While an alert is open there is no second alert.
* An alert closes after 2 passes in a row. A flapping engine (down, up, down, up)
  never gets 2 passes in a row, so it stays one outage and is not re-announced.
* When an alert closes the 5-run window is cleared, so the old failures cannot
  immediately re-open it.
* If fleet-check itself cannot run (Mac offline, crash, bad output) the run is logged
  as an error and counts for nobody: no alert, no recovery.

Everything is stdlib. Paths and commands can be overridden with the
FLEET_STOPGAP_* environment variables, which is how the tests drive it.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path

WINDOW = 5      # how many recent runs to look at
THRESHOLD = 3   # failures within WINDOW that open an alert
RECOVER = 2     # consecutive passes that close an alert
FAIL_STATUSES = {"down", "wrong"}
CHECK_TIMEOUT = 240  # seconds; fleet-check waits up to 80 s on one slow engine

HOME = Path.home()
DEFAULT_CHECK = str(HOME / ".local" / "bin" / "fleet-check")
DEFAULT_STATE_DIR = HOME / ".local" / "state" / "fleet-stopgap"
DEFAULT_LIB = HOME / "dev" / "fleet-control" / "lib"


def now() -> datetime:
    return datetime.now().astimezone()


def hhmm(ts: str) -> str:
    return datetime.fromisoformat(ts).strftime("%I:%M %p").lstrip("0")


def mask(text: str) -> str:
    """Mask secrets with fleetlib.redact. If it cannot be imported, drop the text."""
    lib = os.environ.get("FLEET_STOPGAP_LIB", str(DEFAULT_LIB))
    try:
        sys.path.insert(0, lib)
        from fleetlib.redact import redact  # type: ignore
        return redact(text)[0]
    except Exception:
        return "(detail omitted: redaction library not available)"
    finally:
        if sys.path and sys.path[0] == lib:
            sys.path.pop(0)


def run_fleet_check(cmd: str) -> tuple[dict | None, int, str]:
    """Return (parsed json or None, exit code, error text)."""
    try:
        p = subprocess.run([cmd, "--quick", "--json"], capture_output=True, text=True,
                           timeout=CHECK_TIMEOUT)
    except (OSError, subprocess.TimeoutExpired) as e:
        return None, -1, f"could not run fleet-check: {type(e).__name__}"
    try:
        data = json.loads(p.stdout)
        if not isinstance(data.get("engines"), list):
            raise ValueError("no engines list")
    except ValueError:
        return None, p.returncode, f"fleet-check output was not usable (exit {p.returncode})"
    return data, p.returncode, ""


def notify(cmd: str | None, title: str, body: str) -> bool:
    if cmd:
        argv = [cmd, title, body]
    else:
        script = ('on run argv\n display notification (item 2 of argv) '
                  'with title (item 1 of argv)\nend run')
        argv = ["osascript", "-e", script, title, body]
    try:
        return subprocess.run(argv, capture_output=True, timeout=30).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def load_state(path: Path) -> dict:
    try:
        st = json.loads(path.read_text())
        if isinstance(st.get("engines"), dict):
            return st
    except (OSError, ValueError):
        pass
    return {"engines": {}}


def save_state(path: Path, state: dict) -> None:
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, sort_keys=True, indent=1))
    os.replace(tmp, path)


def main() -> int:
    state_dir = Path(os.environ.get("FLEET_STOPGAP_STATE_DIR", DEFAULT_STATE_DIR))
    state_dir.mkdir(parents=True, exist_ok=True)
    history_path = state_dir / "history.jsonl"
    state_path = state_dir / "state.json"
    notifier = os.environ.get("FLEET_STOPGAP_NOTIFIER") or None
    check_cmd = os.environ.get("FLEET_STOPGAP_CHECK", DEFAULT_CHECK)

    ts = now().isoformat(timespec="seconds")
    state = load_state(state_path)
    data, code, err = run_fleet_check(check_cmd)

    line: dict = {"ts": ts, "exit": code}
    to_alert: list[str] = []
    to_recover: list[str] = []
    details: dict[str, str] = {}

    if data is None:
        line.update(result="error", note=err, engines={}, open=sorted(
            n for n, e in state["engines"].items() if e.get("open_since")))
    else:
        statuses: dict[str, str] = {}
        for eng in data["engines"]:
            name = str(eng.get("engine", "?"))
            status = str(eng.get("status", "?"))
            statuses[name] = status
            if status == "skipped-busy":
                continue  # nobody was asked, so it says nothing either way
            failed = status in FAIL_STATUSES
            es = state["engines"].setdefault(name, {"recent": [], "open_since": None})
            es["recent"] = (es["recent"] + [1 if failed else 0])[-WINDOW:]
            if failed:
                probs = eng.get("problems") or []
                details[name] = mask(str(probs[0]))[:160] if probs else status
            if es["open_since"] is None:
                if sum(es["recent"]) >= THRESHOLD:
                    to_alert.append(name)
            elif es["recent"][-RECOVER:] == [0] * RECOVER:
                to_recover.append(name)
        line.update(result="fail" if any(s in FAIL_STATUSES for s in statuses.values()) else "ok",
                    engines=statuses, problems=details)

    events: list[str] = []
    if to_alert:
        who = ", ".join(sorted(to_alert))
        verb = "is" if len(to_alert) == 1 else "are"
        body = "; ".join(f"{n}: {details.get(n, 'failed')}" for n in sorted(to_alert))
        body = f"Failed {THRESHOLD} or more of the last {WINDOW} checks. {body}"
        if notify(notifier, f"Fleet: {who} {verb} down", body[:300]):
            for n in to_alert:
                state["engines"][n]["open_since"] = ts
                events.append(f"alert:{n}")
        else:
            line["notify_error"] = "alert notification failed; will retry next run"
    if to_recover:
        who = ", ".join(sorted(to_recover))
        verb = "is" if len(to_recover) == 1 else "are"
        since = "; ".join(f"{n} since {hhmm(state['engines'][n]['open_since'])}"
                          for n in sorted(to_recover))
        body = f"Passed the last {RECOVER} checks. Was down: {since}."
        if notify(notifier, f"Fleet: {who} {verb} back up", body[:300]):
            for n in to_recover:
                state["engines"][n]["open_since"] = None
                state["engines"][n]["recent"] = []  # old failures must not re-open it
                events.append(f"recovered:{n}")
        else:
            line["notify_error"] = "recovery notification failed; will retry next run"

    line["events"] = events
    if data is not None:
        line["open"] = sorted(n for n, e in state["engines"].items() if e.get("open_since"))
        save_state(state_path, state)
    with history_path.open("a") as fh:
        fh.write(json.dumps(line, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
