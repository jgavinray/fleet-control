"""Run the library's own redactor over every text file under fleet/lib.

Tier 1, every file: the high-confidence rules (private key blocks, GitHub,
Hugging Face, sk-, AWS, Slack and Google keys, JWTs, bearer and basic auth,
passwords in URLs) must match nothing, except the few made-up values in
ALLOWED_VALUES. Real-looking tokens in the tests are assembled at run time
(tests/helpers.py fake()) so they never appear in a file.

Tier 2, every file except the ones that exist to hold example data: the
generic rules (key=value, flags, email, long hex or base64 runs, mixed-case
tokens) must match nothing. That covers the fixtures and all library code
other than the rule table itself.

    PYTHONPATH=. python3 tests/repo_clean.py
"""
import sys
from pathlib import Path

from fleetlib import redact as R

ROOT = Path(__file__).resolve().parent.parent
SKIP_DIRS = {"__pycache__", ".git"}

TIER1 = {"pem", "url-credential", "github-token", "hf-token", "sk-key", "aws-key", "slack-token", "google-key",
         "jwt", "auth-header", "bearer"}
# files whose job is to hold example secrets for the rules (generic shapes allowed there)
EXAMPLE_FILES = {"README.md", "fleetlib/redact.py", "tests/test_redact.py", "tests/test_logs.py", "tests/test_classify.py"}
# made-up values that are fine anywhere
ALLOWED_VALUES = {"s3cretpw", "pw12345", "abc123def456ghi789", "dXNlcjpwYXNzd29yZA==", "dXNlcjpwYXNzd29yZA"}


def matches(text):
    for rule in R.RULES:
        for m in rule.pattern.finditer(text):
            r = rule.fn(m)
            if r is None or r == m.group(0):
                continue
            yield rule.id, (m.group(m.lastindex) if m.lastindex else m.group(0))


def main() -> int:
    bad = []
    n = 0
    for p in sorted(ROOT.rglob("*")):
        if not p.is_file() or SKIP_DIRS & set(p.parts):
            continue
        rel = str(p.relative_to(ROOT))
        try:
            text = p.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            bad.append((rel, "not utf-8 text"))
            continue
        n += 1
        for rid, val in matches(text):
            if val in ALLOWED_VALUES:
                continue
            if rid not in TIER1 and rel in EXAMPLE_FILES:
                continue
            bad.append((rel, f"rule {rid} matches a {len(val)}-character value starting {val[:3]!r}"))
    for rel, why in bad:
        print(f"FAIL {rel}: {why}")
    print(f"repo-clean: {n} files scanned, {len(bad)} problem(s)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
