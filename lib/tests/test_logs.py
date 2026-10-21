"""logs: typed units from small hand-written fixtures (fake data only)."""
import collections
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path

from fleetlib import logs
from fleetlib.logs import (AssistantTurn, ShellCommand, ToolError, UserMessage, Source, iter_units,
                           classify_user_text, classify_error, default_sources, raw_sources)

from .helpers import fake

FIX = Path(__file__).resolve().parent / "fixtures"
SRC_CLAUDE = Source("testhost", logs.CLAUDE, FIX / "claude")
SRC_OMP = Source("testhost", logs.OMP, FIX / "omp")


def units(src, **kw):
    st = collections.Counter()
    return list(iter_units([src], stats=st, **kw)), st


def by_kind(us, cls):
    return [u for u in us if isinstance(u, cls)]


class ClaudeCode(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.us, cls.st = units(SRC_CLAUDE)

    def test_common_fields(self):
        for u in self.us:
            self.assertEqual(u.host, "testhost")
            self.assertEqual(u.source, "claude-code")
            self.assertEqual(u.session_id, "sess-1111")
            self.assertEqual(u.cwd, "/tmp/demo")
            self.assertRegex(u.timestamp, r"^2026-10-01T10:")

    def test_human_vs_non_human_user_messages(self):
        msgs = {(m.category, m.text[:30]): m for m in by_kind(self.us, UserMessage) if not m.subagent}
        humans = [m.text for m in by_kind(self.us, UserMessage) if m.human]
        self.assertEqual(humans, ["please list the files in this project", "now explain the plan to me",
                                  "also check the readme", "and one more thing: run the tests"])
        cats = collections.Counter(m.category for m in by_kind(self.us, UserMessage) if not m.human and not m.subagent)
        self.assertEqual(cats, {"system_reminder": 1, "task_notification": 1, "interrupt": 1, "meta": 1})
        for m in by_kind(self.us, UserMessage):
            self.assertEqual(m.human, m.category == "human")

    def test_system_reminder_is_stripped_from_human_text(self):
        m = [m for m in by_kind(self.us, UserMessage) if m.text == "now explain the plan to me"]
        self.assertEqual(len(m), 1)
        self.assertTrue(m[0].human)

    def test_queued_prompt_logged_twice_is_kept_once(self):
        readme = [m for m in by_kind(self.us, UserMessage) if m.text == "also check the readme"]
        self.assertEqual(len(readme), 1)
        self.assertFalse(readme[0].queued)
        self.assertEqual(self.st["queued_duplicates_dropped"], 1)
        queued_only = [m for m in by_kind(self.us, UserMessage) if m.text.startswith("and one more")]
        self.assertTrue(queued_only[0].queued)

    def test_tool_results_are_not_user_messages_by_default(self):
        self.assertFalse([m for m in by_kind(self.us, UserMessage) if m.category == "tool_result"])
        us, _ = units(SRC_CLAUDE, include_tool_results=True)
        trs = [m for m in by_kind(us, UserMessage) if m.category == "tool_result"]
        self.assertEqual(len(trs), 4)  # three in the main session, one in the subagent
        self.assertTrue(all(not m.human for m in trs))

    def test_shell_commands(self):
        cmds = sorted((c.command, c.subagent, c.tool) for c in by_kind(self.us, ShellCommand))
        self.assertEqual(cmds, sorted([("ls -la", False, "Bash"), ("cat missing.txt", False, "Bash"),
                                ("python3 -m unittest", False, "Bash"), ("grep -rn TODO .", True, "Bash")]))

    def test_tool_error(self):
        errs = by_kind(self.us, ToolError)
        self.assertEqual(len(errs), 1)
        e = errs[0]
        self.assertEqual((e.tool, e.tool_input, e.exit_code, e.error_class), ("Bash", "cat missing.txt", 1, "file_not_found"))
        self.assertIn("No such file", e.error_text)

    def test_assistant_turns(self):
        turns = [t for t in by_kind(self.us, AssistantTurn) if not t.subagent]
        self.assertEqual(len(turns), 3)
        first = turns[0]
        self.assertEqual(first.text, "There is one file, file.txt. The other one does not exist.")
        self.assertEqual(first.tool_call_count, 2)
        self.assertEqual(first.tool_calls, (("Bash", "ls -la"), ("Bash", "cat missing.txt")))
        self.assertTrue(first.ended_turn)
        self.assertFalse(first.interrupted)
        # the interrupt marker sits between "Sure..." and the next human message
        self.assertTrue(turns[1].interrupted)

    def test_subagent_file(self):
        sub = [u for u in self.us if u.subagent]
        self.assertTrue(sub)
        self.assertTrue(all(u.agent_id == "a1" for u in sub))
        prompt = [m for m in by_kind(sub, UserMessage)]
        self.assertEqual([(m.category, m.human) for m in prompt], [("agent", False)])

    def test_bad_line_is_counted_not_fatal(self):
        self.assertEqual(self.st["bad_lines"], 1)

    def test_filters(self):
        us, _ = units(SRC_CLAUDE, kinds={"shell_command"})
        self.assertEqual({u.kind for u in us}, {"shell_command"})
        us, _ = units(SRC_CLAUDE, human_only=True)
        self.assertEqual(len(us), 4)
        us, _ = units(SRC_CLAUDE, since="2026-10-01T10:05:00", kinds={"user_message"}, human_only=True)
        self.assertEqual([m.text for m in us], ["also check the readme", "and one more thing: run the tests"])
        with self.assertRaises(ValueError):
            list(iter_units([SRC_CLAUDE], kinds={"nonsense"}))


class Omp(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.us, cls.st = units(SRC_OMP)
        cls.main = [u for u in cls.us if not u.subagent]

    def test_common_fields(self):
        for u in self.main:
            self.assertEqual((u.host, u.source, u.session_id), ("testhost", "omp", "01aaaaaa-0000-7000-8000-000000000001"))
            self.assertRegex(u.timestamp, r"^2026-10-01T10:0")
            # cwd is the session's, except for the one command that named its own
            self.assertEqual(u.cwd, "/tmp/other" if getattr(u, "command", "") == "false" else "/tmp/demo")

    def test_human_attribution(self):
        ms = by_kind(self.main, UserMessage)
        self.assertEqual([m.text for m in ms if m.human], ["show me the disk usage here", "ok, now stop"])
        cats = [(m.category, m.text[:20]) for m in ms if not m.human]
        self.assertEqual(cats, [("task_notification", "background job finis"), ("system_reminder", "signoff block"),
                                ("agent", "Continue with the pl"), ("developer", "Be brief.")])

    def test_shell_commands_and_cwd_argument(self):
        cmds = [(c.command, c.cwd) for c in by_kind(self.main, ShellCommand)]
        self.assertEqual(cmds, [("du -sh .", "/tmp/demo"), ("false", "/tmp/other")])

    def test_tool_errors_include_nonzero_exit_without_is_error(self):
        errs = by_kind(self.main, ToolError)
        got = [(e.tool, e.tool_input, e.exit_code, e.error_class) for e in errs]
        self.assertEqual(got, [("bash", "false", 1, "nonzero_exit"), ("read", "/nope", None, "file_not_found")])

    def test_assistant_turns(self):
        turns = by_kind(self.main, AssistantTurn)
        self.assertEqual(len(turns), 2)
        self.assertEqual(turns[0].text, "Checking.\n\nDone: 4K used.")  # thinking blocks are not kept
        self.assertEqual(turns[0].tool_call_count, 3)
        self.assertTrue(turns[0].ended_turn)
        self.assertTrue(turns[1].interrupted)
        self.assertEqual(turns[1].stop_reason, "aborted")

    def test_sub_session_file(self):
        sub = [u for u in self.us if u.subagent]
        self.assertEqual({u.session_id for u in sub}, {"01aaaaaa-0000-7000-8000-000000000002"})
        self.assertEqual([c.command for c in by_kind(sub, ShellCommand)], ["ls crates"])
        self.assertEqual([(m.category, m.human) for m in by_kind(sub, UserMessage)], [("agent", False)])


class Redaction(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir, True)
        self.token = fake("ghp_", 36)
        proj = self.dir / "claude" / "-tmp-x"
        proj.mkdir(parents=True)
        recs = [
            {"type": "user", "timestamp": "2026-10-01T09:00:00.000Z", "sessionId": "s", "cwd": "/tmp/x",
             "message": {"role": "user", "content": f"my password=hunter2 and email bob@example.com and {self.token}"},
             "origin": {"kind": "human"}},
            {"type": "assistant", "timestamp": "2026-10-01T09:00:01.000Z", "sessionId": "s", "cwd": "/tmp/x",
             "message": {"role": "assistant", "stop_reason": "tool_use", "content": [
                 {"type": "text", "text": f"using {self.token}"},
                 {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": f"curl -H 'Authorization: Bearer {self.token}' http://x"}}]}},
            {"type": "user", "timestamp": "2026-10-01T09:00:02.000Z", "sessionId": "s", "cwd": "/tmp/x",
             "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "is_error": True,
                                                       "content": f"Exit code 22\nrejected {self.token}"}]}},
        ]
        with open(proj / "s.jsonl", "w") as fh:
            for r in recs:
                fh.write(json.dumps(r) + "\n")
        self.src = Source("h", logs.CLAUDE, self.dir / "claude")

    def test_redacted_by_default(self):
        us = list(iter_units([self.src]))
        blob = json.dumps([u.to_dict() for u in us])
        for secret in (self.token, "hunter2", "bob@example.com"):
            self.assertNotIn(secret, blob)
        self.assertIn("[REDACTED:github-token]", blob)
        self.assertEqual(sum(u.redactions for u in us), 3 + 1 + 1 + 1 + 1 + 1)  # message 3, turn text 1, turn tool summary 1, command 1, tool_input 1, error 1

    def test_redaction_can_be_turned_off_for_in_memory_use(self):
        us = list(iter_units([self.src], redact=False))
        self.assertIn(self.token, json.dumps([u.to_dict() for u in us]))
        self.assertEqual(sum(u.redactions for u in us), 0)

    def test_truncation_happens_after_masking(self):
        us = list(iter_units([self.src], kinds={"shell_command"}, max_chars=30))
        c = us[0]
        self.assertTrue(c.truncated)
        self.assertEqual(len(c.command), 30)
        self.assertNotIn(self.token[:10], c.command)


class Sources(unittest.TestCase):
    def test_default_sources_paths(self):
        s = default_sources("h", home="/home/x")
        self.assertEqual([(a.host, a.kind, str(a.root)) for a in s],
                         [("h", "claude-code", "/home/x/.claude/projects"), ("h", "omp", "/home/x/.omp/agent/sessions")])

    def test_raw_sources_layout(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d, True)
        (d / "zoidberg" / "claude").mkdir(parents=True)
        (d / "zoidberg" / "omp").mkdir()
        (d / "spark").mkdir()
        self.assertEqual([(s.host, s.kind) for s in raw_sources(d)], [("zoidberg", "claude-code"), ("zoidberg", "omp")])
        self.assertEqual(raw_sources(d / "nope"), [])

    def test_missing_roots_yield_nothing(self):
        self.assertEqual(list(iter_units([Source("h", logs.CLAUDE, Path("/nonexistent/x"))])), [])


class Helpers(unittest.TestCase):
    def test_classify_user_text(self):
        self.assertEqual(classify_user_text("hello there")[1], "human")
        self.assertEqual(classify_user_text("<system-reminder>x</system-reminder>")[1], "system_reminder")
        self.assertEqual(classify_user_text("<task-notification>x")[1], "task_notification")
        self.assertEqual(classify_user_text("<command-name>/clear</command-name>")[1], "command")
        self.assertEqual(classify_user_text("This session is being continued from a previous")[1], "summary")
        self.assertEqual(classify_user_text("[Image #1]")[1], "no_text")
        t, cat, pasted = classify_user_text("see this <pasted_content>aaaa bbbb</pasted_content> please")
        self.assertEqual((cat, t, pasted), ("human", "see this  please", len("<pasted_content>aaaa bbbb</pasted_content>")))
        self.assertEqual(classify_user_text("<pasted_content>aaaa bbbb</pasted_content>")[1], "pasted_only")

    def test_classify_error(self):
        self.assertEqual(classify_error("Edit", "String to replace not found in file", None), "edit_mismatch")
        self.assertEqual(classify_error("Bash", "Exit code 2\nboom", 2), "nonzero_exit")
        self.assertEqual(classify_error("Bash", "PreToolUse:Bash hook error: blocked", 2), "hook_block")
        self.assertEqual(classify_error("Bash", "something odd", None), "other")


if __name__ == "__main__":
    unittest.main()
