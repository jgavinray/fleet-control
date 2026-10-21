"""classify and registry, against a local stub HTTP server (no real engines)."""
import json
import os
import tempfile
import unittest
from pathlib import Path

from fleetlib import classify as C
from fleetlib.classify import Classification, EngineBusy, EngineError, classify, parse_option, waiting_count
from fleetlib.registry import DEFAULTS, Engine, get_engine, load_registry

from .helpers import Stub, fake

OPTS = ["read", "edit", "push"]


def engine(stub, **kw):
    base = dict(name="stub", url=stub.url, method="guided_choice", model="stub-model", k=6, max_inflight=3,
                temperature=1.0, priority="background")
    base.update(kw)
    return Engine(**base)


def pick_by_seed(body, n):
    """Deterministic 'model': the answer depends only on the seed."""
    return OPTS[body["seed"] % 3]


def no_sleep(s):
    pass


class GuidedPath(unittest.TestCase):
    def test_request_shape_seeds_and_shuffles(self):
        with Stub(chat=pick_by_seed) as s:
            r = classify("git status", OPTS, engine(s, k=8), backoff_s=0.01)
            bodies = s.chat_bodies()
        self.assertEqual(len(bodies), 8)
        for b in bodies:
            self.assertEqual(b["temperature"], 1.0)
            self.assertEqual(b["priority"], "background")
            self.assertEqual(b["chat_template_kwargs"], {"enable_thinking": False})
            self.assertEqual(sorted(b["guided_choice"]), sorted(OPTS))
            self.assertEqual(b["model"], "stub-model")
            self.assertIn("git status", b["messages"][0]["content"])
        seeds = [b["seed"] for b in bodies]
        self.assertEqual(len(set(seeds)), 8, "every sample needs its own seed")
        orders = {tuple(b["guided_choice"]) for b in bodies}
        self.assertGreater(len(orders), 1, "option order must be shuffled between samples")
        # the order in the prompt is the order given to guided_choice
        for b in bodies:
            content = b["messages"][0]["content"]
            pos = [content.index(f"- {o}") for o in b["guided_choice"]]
            self.assertEqual(pos, sorted(pos))
        # counts follow from the stub's rule, and the probabilities from the counts
        expect = {o: sum(1 for x in seeds if OPTS[x % 3] == o) for o in OPTS}
        self.assertEqual(r.meta["counts"], expect)
        self.assertEqual(dict(r), {o: expect[o] / 8 for o in OPTS})
        self.assertAlmostEqual(sum(r.values()), 1.0)
        self.assertIsInstance(r, Classification)
        self.assertEqual(list(r), OPTS)
        self.assertEqual(r.meta["invalid"], 0)
        self.assertEqual(r.meta["errors"], 0)
        self.assertEqual(sorted(r.meta["seeds"]), sorted(seeds))

    def test_seeded_sampling_repeats_and_changes_with_the_seed(self):
        with Stub(chat=pick_by_seed) as s:
            e = engine(s, max_inflight=1)
            a = classify("same text", OPTS, e)
            b = classify("same text", OPTS, e)
            c = classify("same text", OPTS, e, seed=12345)
            d = classify("other text", OPTS, e)
        self.assertEqual(a.meta["seeds"], b.meta["seeds"])
        self.assertEqual(a.meta["orders"], b.meta["orders"])
        self.assertEqual(c.meta["seeds"], [12345 + i for i in range(6)])
        self.assertNotEqual(a.meta["seeds"], d.meta["seeds"])

    def test_options_with_descriptions_go_into_the_prompt(self):
        with Stub(chat=pick_by_seed) as s:
            classify("x", {"read": "only looks", "edit": "changes files", "push": "publishes"}, engine(s, k=1))
            content = s.chat_bodies()[0]["messages"][0]["content"]
        self.assertIn("read: only looks", content)
        self.assertIn("push: publishes", content)

    def test_smoothing(self):
        with Stub(chat=lambda b, n: "read") as s:
            r = classify("x", OPTS, engine(s, k=4), smoothing=0.1)
        self.assertAlmostEqual(r["read"], 4.1 / 4.3)
        self.assertAlmostEqual(r["edit"], 0.1 / 4.3)
        self.assertAlmostEqual(sum(r.values()), 1.0)

    def test_respects_the_inflight_limit(self):
        with Stub(chat=pick_by_seed, delay=0.05) as s:
            classify("x", OPTS, engine(s, k=6, max_inflight=3))
            self.assertGreaterEqual(s.max_inflight, 2)
            self.assertLessEqual(s.max_inflight, 3)
        with Stub(chat=pick_by_seed, delay=0.02) as s:
            classify("x", OPTS, engine(s, k=4, max_inflight=1))
            self.assertEqual(s.max_inflight, 1)
        with Stub(chat=pick_by_seed, delay=0.02) as s:  # the call can lower the limit
            classify("x", OPTS, engine(s, k=4, max_inflight=3), max_inflight=1)
            self.assertEqual(s.max_inflight, 1)

    def test_text_is_masked_before_it_is_sent(self):
        tok = fake("ghp_", 36)
        with Stub(chat=pick_by_seed) as s:
            r = classify(f"curl with {tok} and password=hunter2", OPTS, engine(s, k=1))
            sent = json.dumps(s.chat_bodies())
            self.assertNotIn(tok, sent)
            self.assertNotIn("hunter2", sent)
            self.assertIn("[REDACTED:github-token]", sent)
            self.assertEqual(r.meta["redactions"], 2)
        with Stub(chat=pick_by_seed) as s:
            classify(f"curl with {tok}", OPTS, engine(s, k=1), redact_text=False)
            self.assertIn(tok, json.dumps(s.chat_bodies()))

    def test_long_text_is_cut(self):
        with Stub(chat=pick_by_seed) as s:
            r = classify("z" * 500, OPTS, engine(s, k=1), max_chars=100)
            content = s.chat_bodies()[0]["messages"][0]["content"]
        self.assertTrue(r.meta["truncated"])
        self.assertEqual(r.meta["text_chars"], 500)
        self.assertIn("z" * 100, content)
        self.assertNotIn("z" * 101, content)

    def test_bad_options(self):
        with Stub(chat=pick_by_seed) as s:
            for bad in (["one"], ["a", "a"], []):
                with self.assertRaises(ValueError):
                    classify("x", bad, engine(s))


class ParsePath(unittest.TestCase):
    """DeepSeek and Qwen: guided_choice is rejected, so it must never be sent."""

    @staticmethod
    def strict(replies):
        def chat(body, n):
            if "guided_choice" in body:
                return (400, "guided_choice is not supported")
            return replies[(n - 1) % len(replies)]
        return chat

    def test_parses_one_word_and_counts_invalid(self):
        replies = ["push", "`edit`.", "banana", "Push - because it publishes", ""]
        with Stub(chat=self.strict(replies)) as s:
            r = classify("x", OPTS, engine(s, method="parse", k=5, max_inflight=1, seed_param=False, temperature=0.0))
            bodies = s.chat_bodies()
        self.assertEqual(r.meta["valid"], 3)
        self.assertEqual(r.meta["invalid"], 2)
        self.assertEqual(r.meta["errors"], 0)
        self.assertEqual(r.meta["counts"], {"read": 0, "edit": 1, "push": 2})
        self.assertAlmostEqual(r["push"], 2 / 3)
        self.assertAlmostEqual(r["edit"], 1 / 3)
        self.assertEqual(r["read"], 0.0)
        for b in bodies:
            self.assertNotIn("guided_choice", b)
            self.assertNotIn("seed", b)  # seed_param=False
            self.assertIn("Reply with exactly one option word", b["messages"][0]["content"])
            self.assertEqual(b["chat_template_kwargs"], {"enable_thinking": False})
        self.assertEqual({b["temperature"] for b in bodies}, {1.0}, "k > 1 at temperature 0 would repeat one sample")

    def test_single_sample_uses_the_engine_temperature(self):
        with Stub(chat=self.strict(["read"])) as s:
            r = classify("x", OPTS, engine(s, method="parse", k=1, temperature=0.0, seed_param=False))
            self.assertEqual(s.chat_bodies()[0]["temperature"], 0.0)
        self.assertEqual(dict(r), {"read": 1.0, "edit": 0.0, "push": 0.0})

    def test_all_invalid_gives_zeros_not_an_error(self):
        with Stub(chat=self.strict(["I cannot decide"])) as s:
            r = classify("x", OPTS, engine(s, method="parse", k=3, temperature=0.0))
        self.assertEqual(dict(r), {"read": 0.0, "edit": 0.0, "push": 0.0})
        self.assertEqual((r.meta["valid"], r.meta["invalid"]), (0, 3))
        self.assertIsNone(r.meta["choice"])

    def test_parse_option(self):
        self.assertEqual(parse_option("Push.", OPTS), "push")
        self.assertEqual(parse_option("  **edit**\n", OPTS), "edit")
        self.assertEqual(parse_option("read - it only looks", OPTS), "read")
        self.assertIsNone(parse_option("reading", OPTS))
        self.assertIsNone(parse_option("pushed it", OPTS))
        self.assertIsNone(parse_option("", OPTS))
        self.assertIsNone(parse_option(None, OPTS))
        self.assertEqual(parse_option("push_all", ["push_all", "push"]), "push_all")


class Errors(unittest.TestCase):
    def test_all_samples_failing_raises(self):
        with Stub(chat=lambda b, n: (500, "boom")) as s:
            with self.assertRaises(EngineError) as cm:
                classify("x", OPTS, engine(s, k=3))
        self.assertIn("all 3 requests failed", str(cm.exception))

    def test_a_failed_sample_is_counted_and_the_rest_used(self):
        def chat(body, n):
            return (500, "boom") if n == 1 else "edit"
        with Stub(chat=chat) as s:
            r = classify("x", OPTS, engine(s, k=4, max_inflight=1))
        self.assertEqual(r.meta["errors"], 1)
        self.assertEqual(r.meta["valid"], 3)
        self.assertEqual(r["edit"], 1.0)

    def test_unreachable_engine(self):
        e = Engine("dead", "http://127.0.0.1:9", "guided_choice", "m", k=1, timeout=2)
        with self.assertRaises(EngineError):
            classify("x", OPTS, e, backoff_s=0.01)


class BackOff(unittest.TestCase):
    def test_backs_off_while_requests_waiting(self):
        sleeps = []
        health = lambda n: {"requests_waiting": 2 if n <= 2 else 0, "requests_running": 4}
        with Stub(health=health, chat=pick_by_seed) as s:
            r = classify("x", OPTS, engine(s, k=1), backoff_s=7.5, sleep=sleeps.append)
            order = [e[0] for e in s.log]
        self.assertEqual(sleeps, [7.5, 7.5])
        self.assertEqual(r.meta["backoffs"], 2)
        self.assertEqual(r.meta["health_checks"], 3)
        self.assertEqual(order, ["health", "health", "health", "chat"], "no request until /health shows nothing waiting")

    def test_live_waiting_counts_too(self):
        health = lambda n: {"live": {"connections": 3, "waiting": 1 if n == 1 else 0}}
        with Stub(health=health, chat=pick_by_seed) as s:
            r = classify("x", OPTS, engine(s, k=1), backoff_s=0.0, sleep=no_sleep)
        self.assertEqual(r.meta["backoffs"], 1)  # connections alone do not block a non-shared engine

    def test_running_requests_do_not_block_background_work(self):
        with Stub(health=lambda n: {"busy": True, "requests_running": 6, "requests_waiting": 0}, chat=pick_by_seed) as s:
            r = classify("x", OPTS, engine(s, k=1), sleep=no_sleep)
        self.assertEqual(r.meta["backoffs"], 0)

    def test_shared_engine_backs_off_on_other_connections_and_runs_one_at_a_time(self):
        sleeps = []
        health = lambda n: {"live": {"connections": 1 if n <= 2 else 0, "waiting": 0}}
        with Stub(health=health, chat=lambda b, n: "read", delay=0.02) as s:
            e = engine(s, name="qwen-like", method="parse", k=3, max_inflight=1, share_check=True, temperature=0.0,
                       seed_param=False)
            r = classify("x", OPTS, e, backoff_s=0.5, sleep=sleeps.append)
            self.assertEqual(s.max_inflight, 1)
            order = [x[0] for x in s.log]
        self.assertEqual(sleeps, [0.5, 0.5])
        self.assertEqual(r.meta["backoffs"], 2)
        # /health is asked before every one of the three requests on a shared engine
        self.assertEqual(r.meta["health_checks"], 5)
        self.assertEqual(order[:3], ["health", "health", "health"])
        self.assertEqual(order.count("chat"), 3)
        self.assertEqual(order[3], "chat")
        self.assertEqual(order[4], "health")

    def test_gives_up_after_max_wait(self):
        with Stub(health=lambda n: {"requests_waiting": 5}, chat=pick_by_seed) as s:
            with self.assertRaises(EngineBusy):
                classify("x", OPTS, engine(s, k=1), backoff_s=10, max_wait_s=25, sleep=no_sleep)
            self.assertEqual(s.n_chat, 0)

    def test_dead_health_endpoint_does_not_block(self):
        with Stub(health=lambda n: (500, "no health"), chat=pick_by_seed) as s:
            r = classify("x", OPTS, engine(s, k=1), sleep=no_sleep)
        self.assertEqual(r.meta["valid"], 1)

    def test_waiting_count(self):
        self.assertEqual(waiting_count({"requests_waiting": 2}, False), 2)
        self.assertEqual(waiting_count({"live": {"waiting": 1, "connections": 4}}, False), 1)
        self.assertEqual(waiting_count({"live": {"waiting": 1, "connections": 4}}, True), 5)
        self.assertEqual(waiting_count({}, True), 0)


class SystemOne(unittest.TestCase):
    def reply(self, body):
        q = body["questions"]["verdict"]
        probs = {o: 0.1 for o in q["criteria"]}
        probs[list(q["criteria"])[-1]] = 0.8
        return {"model": "svc-1", "answers": {"verdict": {"type": "choice", "choice": list(q["criteria"])[-1],
                                                          "probabilities": probs, "confidence": 0.5}},
                "usage": {"input_tokens": 12, "output_tokens": 0}}

    def test_one_request_and_probabilities_from_the_service(self):
        tok = fake("ghp_", 36)
        with Stub(systemone=self.reply) as s:
            e = engine(s, method="systemone", model="shadow", k=5)
            r = classify(f"git push {tok}", {"read": "looks", "edit": "changes", "push": "publishes"}, e)
            sent = [x for x in s.log if x[0] == "systemone"]
        self.assertEqual(len(sent), 1, "the service does its own sampling; k is not used")
        body = sent[0][1]
        self.assertEqual(body["model"], "shadow")
        self.assertNotIn(tok, body["state"])
        self.assertIn("[REDACTED:github-token]", body["state"])
        q = body["questions"]["verdict"]
        self.assertEqual(q["type"], "choice")
        self.assertEqual(q["criteria"], {"read": "looks", "edit": "changes", "push": "publishes"})
        self.assertEqual(dict(r), {"read": 0.1, "edit": 0.1, "push": 0.8})
        self.assertEqual(r.meta["choice"], "push")
        self.assertEqual(r.meta["confidence"], 0.5)
        self.assertEqual(r.meta["service_model"], "svc-1")
        self.assertEqual(r.meta["tokens_in"], 12)

    def test_option_names_are_the_criteria_when_no_descriptions(self):
        with Stub(systemone=self.reply) as s:
            classify("x", OPTS, engine(s, method="systemone"))
            self.assertEqual(s.log[-1][1]["questions"]["verdict"]["criteria"], {o: o for o in OPTS})

    def test_backs_off_before_posting(self):
        with Stub(health=lambda n: {"requests_waiting": 1 if n == 1 else 0}, systemone=self.reply) as s:
            r = classify("x", OPTS, engine(s, method="systemone"), backoff_s=0, sleep=no_sleep)
            self.assertEqual([x[0] for x in s.log], ["health", "health", "systemone"])
        self.assertEqual(r.meta["backoffs"], 1)

    def test_unexpected_reply_is_an_error(self):
        with Stub(systemone=lambda b: {"answers": {}}) as s:
            with self.assertRaises(EngineError):
                classify("x", OPTS, engine(s, method="systemone"))


class Registry(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmp, True))
        self._env = os.environ.pop("FLEET_TOML", None)
        self.addCleanup(lambda: os.environ.__setitem__("FLEET_TOML", self._env) if self._env else os.environ.pop("FLEET_TOML", None))

    def write(self, text):
        p = self.tmp / "fleet.toml"
        p.write_text(text)
        return p

    def test_defaults(self):
        self.assertEqual(set(DEFAULTS), {"glm", "deepseek", "qwen", "laya", "jev-glm"})
        g = DEFAULTS["glm"]
        self.assertEqual((g.method, g.priority, g.temperature), ("guided_choice", "background", 1.0))
        self.assertEqual(DEFAULTS["deepseek"].method, "parse")
        q = DEFAULTS["qwen"]
        self.assertEqual((q.method, q.max_inflight, q.share_check), ("parse", 1, True))
        self.assertEqual(DEFAULTS["laya"].method, "systemone")
        self.assertEqual(DEFAULTS["jev-glm"].url, "http://192.168.0.44:8102")

    def test_missing_file_means_defaults(self):
        os.environ["FLEET_TOML"] = str(self.tmp / "nope.toml")
        self.assertEqual(load_registry(), DEFAULTS)

    def test_explicit_missing_path_is_an_error(self):
        with self.assertRaises(FileNotFoundError):
            load_registry(self.tmp / "nope.toml")

    def test_registry_overrides_only_what_it_names(self):
        p = self.write('[engines.glm]\nurl = "http://glm.test:9000/v1/"\nk = 7\n\n[engines.laya]\nhost = "10.0.0.5"\nport = 8001\n')
        reg = load_registry(p)
        self.assertEqual(reg["glm"].url, "http://glm.test:9000")
        self.assertEqual(reg["glm"].k, 7)
        self.assertEqual(reg["glm"].model, DEFAULTS["glm"].model)
        self.assertEqual(reg["glm"].priority, "background")
        self.assertEqual(reg["laya"].url, "http://10.0.0.5:8001")
        self.assertEqual(reg["laya"].method, "systemone")
        self.assertEqual(reg["qwen"], DEFAULTS["qwen"])

    def test_list_form_and_new_engine(self):
        p = self.write('[[engine]]\nname = "deepseek"\nbase_url = "http://ds.test:1"\n\n'
                       '[[engine]]\nname = "newone"\nurl = "http://n.test:2"\nmethod = "parse"\nmodel = "m"\nmax_inflight = 4\n\n'
                       '[[engine]]\nname = "unusable"\nurl = "http://x"\n')
        reg = load_registry(p)
        self.assertEqual(reg["deepseek"].url, "http://ds.test:1")
        self.assertEqual((reg["newone"].method, reg["newone"].max_inflight), ("parse", 4))
        self.assertNotIn("unusable", reg)  # no method: cannot be used

    def test_env_var_selects_the_file(self):
        p = self.write('[engines.qwen]\nurl = "http://q.test:3"\n')
        os.environ["FLEET_TOML"] = str(p)
        self.assertEqual(load_registry()["qwen"].url, "http://q.test:3")

    def test_fleet_check_schema(self):
        """The real fleet.toml shape: kind, shared, expected_features; lfm2d engines are skipped."""
        p = self.write(
            '[engines.deepseek]\nkind = "tensorfold"\nurl = "http://ds.test:1"\nmodel = "ds-2"\n'
            '[engines.deepseek.expected_features]\nguided_choice = "yes"\n\n'
            '[engines.qwen]\nkind = "tensorfold"\nshared = true\nurl = "http://q.test:2"\nmodel = "qw"\n'
            '[engines.qwen.expected_features]\nguided_choice = "no"\n\n'
            '[engines.laya]\nkind = "laya"\nurl = "http://l.test:3"\nmodel = "laya-x"\n\n'
            '[engines.lfm2d-8b]\nkind = "lfm2d"\nurl = "http://o.test:4"\nmodel = "o"\n')
        reg = load_registry(p)
        self.assertEqual((reg["deepseek"].method, reg["deepseek"].model), ("guided_choice", "ds-2"))
        self.assertEqual((reg["qwen"].method, reg["qwen"].share_check), ("parse", True))
        self.assertEqual((reg["laya"].method, reg["laya"].url, reg["laya"].model), ("systemone", "http://l.test:3", "laya-x"))
        self.assertNotIn("lfm2d-8b", reg)

    @unittest.skipUnless((Path(__file__).resolve().parents[2] / "fleet.toml").is_file(), "no fleet/fleet.toml in this checkout")
    def test_the_real_registry_file_agrees_with_the_defaults_on_how_to_ask(self):
        reg = load_registry(Path(__file__).resolve().parents[2] / "fleet.toml")
        for name, d in DEFAULTS.items():
            self.assertIn(name, reg)
            self.assertEqual((reg[name].method, reg[name].share_check), (d.method, d.share_check), name)

    def test_get_engine(self):
        self.assertEqual(get_engine("glm", DEFAULTS), DEFAULTS["glm"])
        e = Engine("x", "http://x", "parse")
        self.assertIs(get_engine(e), e)
        with self.assertRaises(KeyError):
            get_engine("nope", DEFAULTS)

    def test_bad_method_rejected(self):
        with self.assertRaises(ValueError):
            Engine("x", "http://x", "telepathy")

    def test_cli_with_a_registry_file(self):
        with Stub(chat=lambda b, n: "edit") as s:
            p = self.write(f'[engines.glm]\nurl = "{s.url}"\nk = 2\n')
            import io, contextlib
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = C.main(["--engine", "glm", "--options", "read,edit,push", "--registry", str(p), "git add ."])
        self.assertEqual(rc, 0)
        d = json.loads(out.getvalue())
        self.assertEqual(d["probabilities"]["edit"], 1.0)
        self.assertEqual(d["meta"]["k"], 2)


if __name__ == "__main__":
    unittest.main()
