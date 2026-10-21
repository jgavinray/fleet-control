"""Prove the redaction tests can fail.

For every rule in fleetlib.redact.RULES: take the rule out, run the redact
suite, and require that it FAILS. If removing a rule leaves the suite green,
that rule has no test that needs it, and this exits non-zero.

    PYTHONPATH=. python3 tests/mutation_check.py [rule-id ...]
"""
import io
import sys
import unittest

from fleetlib import redact as R

from tests import test_redact


def suite_passes() -> bool:
    suite = unittest.defaultTestLoader.loadTestsFromModule(test_redact)
    res = unittest.TextTestRunner(stream=io.StringIO(), verbosity=0).run(suite)
    return res.wasSuccessful()


def main(only) -> int:
    original = list(R.RULES)
    if not suite_passes():
        print("FAIL: the redact suite is red before any rule is removed")
        return 1
    survivors = []
    for rule in original:
        if only and rule.id not in only:
            continue
        R.RULES[:] = [r for r in original if r.id != rule.id]
        try:
            ok = suite_passes()
        finally:
            R.RULES[:] = original
        print(f"  without {rule.id:15s} suite {'STILL PASSES  <-- no test needs this rule' if ok else 'fails (good)'}")
        if ok:
            survivors.append(rule.id)
    if survivors:
        print("FAIL: rules no test needs:", ", ".join(survivors))
        return 1
    print("mutation check ok: the suite fails when any single rule is removed")
    return 0


if __name__ == "__main__":
    sys.exit(main(set(sys.argv[1:])))
