#!/usr/bin/env python3
"""Leak gate: fail if a tracked file holds an address or name that must not be public.

This repo is public. Forbidden anywhere in a tracked (or not yet ignored, not
yet committed) file:

  - a private IPv4 address (RFC 1918: the 10/8, 172.16/12 and 192.168/16 blocks)
  - a Tailscale IPv4 address (the 100.64/10 block)
  - the name of the other engineer who uses hyper03 (spelled out only at run
    time below, so this file does not trip itself)

Allowed: 192.0.2.x (RFC 5737 documentation addresses), loopback (127.x),
Tailscale names (*.ts.net).

Usage: leak_check.py [ROOT]    ROOT defaults to the repo this file is in. If ROOT
is a git checkout the files are `git ls-files` plus untracked-but-not-ignored;
otherwise every file under it. Exit 0 clean, 1 on any hit (each is printed with
file and line, but the matched value is masked).
"""
import os
import re
import subprocess
import sys

OCTET = r"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)"
TAIL = rf"\.{OCTET}\.{OCTET}"
IPV4 = re.compile(
    r"(?<![\d.])(?:"
    rf"10{TAIL}\.{OCTET}"
    rf"|172\.(?:1[6-9]|2\d|3[01]){TAIL}"
    rf"|192\.168{TAIL}"
    rf"|100\.(?:6[4-9]|[7-9]\d|1[01]\d|12[0-7]){TAIL}"
    r")(?![\d]|\.\d)"
)
NAME = re.compile("zu" + "rih", re.I)


def files(root):
    try:
        out = subprocess.run(
            ["git", "-C", root, "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            capture_output=True, check=True).stdout
        return [f for f in out.decode().split("\0") if f]
    except (OSError, subprocess.CalledProcessError):
        found = []
        for d, dirs, names in os.walk(root):
            dirs[:] = [x for x in dirs if x != ".git"]
            found += [os.path.relpath(os.path.join(d, n), root) for n in names]
        return found


def scan(root):
    hits = []
    for rel in sorted(set(files(root))):
        path = os.path.join(root, rel)
        if not os.path.isfile(path) or os.path.islink(path):
            continue
        with open(path, "rb") as fh:
            text = fh.read().decode("latin-1")
        for n, line in enumerate(text.splitlines(), 1):
            if IPV4.search(line):
                hits.append(f"{rel}:{n}: private or Tailscale IP address")
            if NAME.search(line):
                hits.append(f"{rel}:{n}: name of the other hyper03 user")
    return hits


def main(argv):
    root = os.path.abspath(argv[1]) if len(argv) > 1 else os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
    hits = scan(root)
    for h in hits:
        print("LEAK:", h)
    if hits:
        print(f"FAIL: {len(hits)} leak(s); this repo is public. Use Tailscale names or 192.0.2.x.")
        return 1
    print("leak gate: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
