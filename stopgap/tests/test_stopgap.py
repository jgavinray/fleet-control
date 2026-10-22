"""Tests for fleet-stopgap.py. They fake fleet-check and the notifier; nothing touches the fleet.

Set FLEET_STOPGAP_SCRIPT to test a different copy of the script (test.sh --selftest does).
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = os.environ.get("FLEET_STOPGAP_SCRIPT", str(HERE.parent / "fleet-stopgap.py"))
LIB = HERE.parents[1] / "lib"


class Harness:
    """A temp state dir, a scripted fleet-check, and a notifier that records."""

    def __init__(self, seq):
        self.dir = Path(tempfile.mkdtemp(prefix="stopgap-test-"))
        self.seq = self.dir / "seq"
        self.seq.write_text("\n".join(seq) + "\n")
        self.notes = self.dir / "notes"
        self.fail_flag = self.dir / "notify-fails"
        self.env = dict(os.environ,
                        FAKE_SEQ=str(self.seq), FAKE_NOTES=str(self.notes),
                        FAKE_NOTIFY_FAIL=str(self.fail_flag),
                        FLEET_STOPGAP_CHECK=str(HERE / "fake-fleet-check"),
                        FLEET_STOPGAP_NOTIFIER=str(HERE / "fake-notifier"),
                        FLEET_STOPGAP_STATE_DIR=str(self.dir / "state"),
                        FLEET_STOPGAP_LIB=str(LIB))

    def run(self, times=1):
        for _ in range(times):
            p = subprocess.run([sys.executable, SCRIPT], env=self.env,
                               capture_output=True, text=True)
            assert p.returncode == 0, p.stderr

    def notes_list(self):
        return self.notes.read_text().splitlines() if self.notes.exists() else []

    def history(self):
        return [json.loads(x) for x in (self.dir / "state" / "history.jsonl").read_text().splitlines()]


OK = "glm=ok,deepseek=ok"
D = "glm=down,deepseek=ok"


class StopgapTests(unittest.TestCase):
    def go(self, seq):
        h = Harness(seq)
        h.run(len(seq))
        return h

    def alerts(self, h):
        return [n for n in h.notes_list() if "back up" not in n]

    def recoveries(self, h):
        return [n for n in h.notes_list() if "back up" in n]

    def test_healthy_fleet_is_silent(self):
        h = self.go([OK] * 10)
        self.assertEqual(h.notes_list(), [])

    def test_one_failure_in_five_is_silent(self):
        h = self.go([OK, OK, D, OK, OK, OK])
        self.assertEqual(h.notes_list(), [])

    def test_two_failures_in_five_is_silent(self):
        for seq in ([D, D, OK, OK, OK], [D, OK, D, OK, OK], [OK, D, OK, OK, D, OK, OK]):
            with self.subTest(seq=seq):
                self.assertEqual(self.go(seq).notes_list(), [])

    def test_three_failures_in_five_alerts_once(self):
        h = self.go([D, D, OK, D])   # third failure within five runs, on run 4
        self.assertEqual(len(self.alerts(h)), 1)
        self.assertIn("glm is down", self.alerts(h)[0])
        self.assertNotIn("deepseek", self.alerts(h)[0])
        self.assertEqual(self.recoveries(h), [])

    def test_alert_fires_on_the_run_that_makes_three(self):
        h = Harness([D, D, D, D])
        h.run(2)
        self.assertEqual(h.notes_list(), [])
        h.run(1)
        self.assertEqual(len(h.notes_list()), 1)
        h.run(1)
        self.assertEqual(len(h.notes_list()), 1, "a continuing outage must not re-alert")

    def test_sustained_outage_then_recovery_sends_one_each(self):
        h = self.go([D] * 8 + [OK] * 6)
        self.assertEqual(len(self.alerts(h)), 1)
        self.assertEqual(len(self.recoveries(h)), 1)
        self.assertIn("glm is back up", self.recoveries(h)[0])

    def test_recovery_needs_two_passes_in_a_row(self):
        h = self.go([D] * 4 + [OK])
        self.assertEqual(self.recoveries(h), [])
        h = self.go([D] * 4 + [OK, OK])
        self.assertEqual(len(self.recoveries(h)), 1)

    def test_old_failures_do_not_reopen_after_recovery(self):
        # after recovery the window is cleared: two stray failures afterwards are silent
        h = self.go([D, D, D, OK, OK, D, OK, D, OK, OK])
        self.assertEqual(len(self.alerts(h)), 1)
        self.assertEqual(len(self.recoveries(h)), 1)

    def test_second_outage_after_recovery_alerts_again(self):
        h = self.go([D, D, D, OK, OK, OK, D, D, D])
        self.assertEqual(len(self.alerts(h)), 2)
        self.assertEqual(len(self.recoveries(h)), 1)

    def test_flapping_engine_alerts_exactly_once(self):
        h = self.go([D, OK, D, OK, D, OK])
        self.assertEqual(len(self.alerts(h)), 1)
        self.assertEqual(self.recoveries(h), [])
        h = self.go([D, OK] * 10)
        self.assertEqual(len(self.alerts(h)), 1)
        self.assertEqual(self.recoveries(h), [])

    def test_two_engines_down_together_make_one_notification(self):
        both = "glm=down,deepseek=down"
        h = self.go([both] * 3)
        self.assertEqual(len(h.notes_list()), 1)
        self.assertIn("deepseek, glm are down", h.notes_list()[0])

    def test_slow_busy_and_drift_do_not_count(self):
        for status in ("slow", "skipped-busy", "drift"):
            with self.subTest(status=status):
                h = self.go([f"glm={status},deepseek=ok"] * 8)
                self.assertEqual(h.notes_list(), [])

    def test_wrong_counts_as_failed(self):
        h = self.go(["glm=wrong,deepseek=ok"] * 3)
        self.assertEqual(len(h.notes_list()), 1)

    def test_fleet_check_crash_is_logged_and_says_nothing(self):
        h = self.go([D, D, "CRASH", "CRASH", "EMPTY", "CRASH"])
        self.assertEqual(h.notes_list(), [])
        hist = h.history()
        self.assertEqual([x["result"] for x in hist],
                         ["fail", "fail", "error", "error", "error", "error"])
        # errors do not count: the third real failure still alerts, and only then
        h.seq.write_text(D + "\n")
        h.run(1)
        self.assertEqual(len(h.notes_list()), 1)

    def test_history_has_one_line_per_run_with_results(self):
        h = self.go([OK, D, D, D, OK, OK])
        hist = h.history()
        self.assertEqual(len(hist), 6)
        self.assertEqual([x["result"] for x in hist], ["ok", "fail", "fail", "fail", "ok", "ok"])
        self.assertEqual(hist[1]["engines"], {"deepseek": "ok", "glm": "down"})
        self.assertEqual(hist[3]["events"], ["alert:glm"])
        self.assertEqual(hist[3]["open"], ["glm"])
        self.assertEqual(hist[5]["events"], ["recovered:glm"])
        self.assertEqual(hist[5]["open"], [])
        for x in hist:
            self.assertIn("ts", x)

    def test_runs_fleet_check_quick_and_json(self):
        h = self.go([OK])
        self.assertEqual(Path(str(h.seq) + ".args").read_text().strip(), "--quick --json")

    def test_secrets_in_problem_text_are_masked_in_history_and_notice(self):
        h = self.go([D] * 3)
        text = (h.dir / "state" / "history.jsonl").read_text() + "\n".join(h.notes_list())
        self.assertNotIn("hunter2hunter2", text)
        self.assertIn("[REDACTED:", text)  # masked by the real library, not dropped

    def test_failed_notification_is_retried_next_run(self):
        h = Harness([D] * 5)
        h.fail_flag.write_text("x")
        h.run(3)
        self.assertEqual(h.notes_list(), [])
        self.assertIn("notify_error", h.history()[-1])
        h.fail_flag.unlink()
        h.run(1)
        self.assertEqual(len(h.notes_list()), 1)
        h.run(1)
        self.assertEqual(len(h.notes_list()), 1)

    def test_state_survives_between_runs(self):
        h = Harness([D, D, D])
        h.run(2)
        self.assertEqual(h.notes_list(), [])
        h.run(1)   # a fresh process, same state dir
        self.assertEqual(len(h.notes_list()), 1)


if __name__ == "__main__":
    unittest.main()
