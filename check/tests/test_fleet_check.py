"""Tests for fleet-check against local stub HTTP servers. Standard library only."""

import contextlib
import io
import json
import os
import socket
import sys
import tempfile
import threading
import tomllib
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
import fleet_check  # noqa: E402

REAL_REGISTRY = os.path.join(os.path.dirname(os.path.dirname(HERE)), "fleet.toml")


class Stub:
    """A tiny HTTP server. `routes` maps (METHOD, path) to fn(body) -> (status, obj)."""

    def __init__(self, routes):
        self.routes = routes
        self.log = []  # (method, path, body)
        stub = self

        class H(BaseHTTPRequestHandler):
            def _serve(self, method):
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n) if n else b""
                body = json.loads(raw) if raw else None
                stub.log.append((method, self.path, body))
                fn = stub.routes.get((method, self.path))
                if fn is None:
                    status, obj = 404, {"detail": "Not Found"}
                else:
                    status, obj = fn(body)
                data = obj.encode() if isinstance(obj, str) else json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                self._serve("GET")

            def do_POST(self):
                self._serve("POST")

            def log_message(self, *a):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.02},
                         daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def posts(self, path):
        return [b for m, p, b in self.log if m == "POST" and p == path]

    @property
    def gets(self):
        return [p for m, p, _ in self.log if m == "GET"]


def tensorfold_routes(model="Stub-Model", answer="42", guided="yes", background="yes",
                      logprobs="refuse", prompt_logprobs="ignore", connections=0,
                      version=None):
    """A TensorFold-like engine. Each knob picks how one feature behaves."""

    def health(_):
        h = {"status": "ok", "model": model, "warming": False,
             "live": {"connections": connections, "waiting": 0}}
        if version:
            h["version"] = version
        return 200, h

    def models(_):
        return 200, {"object": "list", "data": [{"id": model}]}

    def chat(body):
        if "priority" in body and background == "400":
            return 400, {"error": {"message": "unknown field priority"}}
        if "guided_choice" in body:
            if guided == "yes":
                return 200, {"choices": [{"message": {"content": "apple"}}]}
            if guided == "400":
                return 400, {"error": {"message": "structured output not supported"}}
            if guided == "500-lib":
                return 500, {"error": {"message": "libtfgrammar.so is required"}}
            if guided == "500":
                return 500, {"error": {"message": "internal boom"}}
            return 200, {"choices": [{"message": {"content": "I think apples."}}]}  # ignored
        if body.get("logprobs"):
            if logprobs == "refuse":
                return 400, {"error": {"message": "logprobs not supported"}}
            if logprobs == "yes":
                return 200, {"choices": [{"message": {"content": "ok"},
                                          "logprobs": {"content": [{"token": "ok", "logprob": -0.1}]}}]}
            return 200, {"choices": [{"message": {"content": "ok"}}]}  # ignored
        if body["messages"][0]["content"].startswith("What is 17"):
            return 200, {"choices": [{"message": {"content": answer}}]}
        return 200, {"choices": [{"message": {"content": "ok"}}]}

    def completions(body):
        if prompt_logprobs == "yes":
            return 200, {"choices": [{"text": "x"}], "prompt_logprobs": [None, {"1": -1.0}]}
        return 200, {"choices": [{"text": "x"}]}

    return {
        ("GET", "/health"): health,
        ("GET", "/v1/models"): models,
        ("POST", "/v1/chat/completions"): chat,
        ("POST", "/v1/completions"): completions,
    }


def dead_url():
    """A URL nobody listens on."""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return f"http://127.0.0.1:{port}"


def toml_for(engines):
    """Write a registry. engines: {name: dict(url, kind, model, expected_features?, ...)}."""
    lines = []
    for name, e in sorted(engines.items()):
        lines.append(f"[engines.{name}]")
        for k, v in sorted(e.items()):
            if k == "expected_features":
                continue
            lines.append(f"{k} = {json.dumps(v)}")
        if "expected_features" in e:
            lines.append(f"[engines.{name}.expected_features]")
            for k, v in sorted(e["expected_features"].items()):
                lines.append(f"{k} = {json.dumps(v)}")
        lines.append("")
    return "\n".join(lines)


GLM_LIKE = {"background_priority": "yes", "guided_choice": "yes",
            "logprobs": "no", "prompt_logprobs": "no"}


class Base(unittest.TestCase):
    def setUp(self):
        self.stubs = []
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(lambda: [s.close() for s in self.stubs])

    def stub(self, routes):
        s = Stub(routes)
        self.stubs.append(s)
        return s

    def run_check(self, engines, *args):
        path = os.path.join(self.tmp.name, "fleet.toml")
        with open(path, "w") as f:
            f.write(toml_for(engines))
        out = io.StringIO()
        code = fleet_check.main(["--registry", path, "--timeout", "10", *args], out=out)
        return code, out.getvalue()

    def run_json(self, engines, *args):
        code, text = self.run_check(engines, "--json", *args)
        doc = json.loads(text)
        self.assertEqual(doc["exit_code"], code)
        return code, {e["engine"]: e for e in doc["engines"]}


def tf(url, **kw):
    e = {"url": url, "kind": "tensorfold", "model": "Stub-Model",
         "expected_features": dict(GLM_LIKE)}
    e.update(kw)
    return e


class TensorFoldTests(Base):
    def test_healthy_engine_exits_0(self):
        s = self.stub(tensorfold_routes())
        code, eng = self.run_json({"glm": tf(s.url)})
        self.assertEqual(code, 0)
        e = eng["glm"]
        self.assertEqual(e["verdict"], "ok")
        self.assertEqual(e["sanity"]["result"], "pass")
        self.assertEqual({k: v["result"] for k, v in e["features"].items()},
                         {"guided_choice": "yes", "background_priority": "yes",
                          "logprobs": "no", "prompt_logprobs": "no"})
        self.assertFalse(any(v["drift"] for v in e["features"].values()))
        self.assertLessEqual(e["requests"], 6)
        self.assertEqual(e["requests"], len(s.log))

    def test_sanity_request_uses_thinking_off_and_background(self):
        s = self.stub(tensorfold_routes())
        self.run_check({"glm": tf(s.url)})
        sanity = [b for b in s.posts("/v1/chat/completions")
                  if b["messages"][0]["content"].startswith("What is 17 plus 25?")]
        self.assertEqual(len(sanity), 1)
        self.assertEqual(sanity[0]["chat_template_kwargs"], {"enable_thinking": False})
        self.assertEqual(sanity[0]["priority"], "background")
        guided = [b for b in s.posts("/v1/chat/completions") if "guided_choice" in b][0]
        self.assertEqual(guided["chat_template_kwargs"], {"enable_thinking": False})
        self.assertEqual(guided["priority"], "background")

    def test_down_engine_exits_1(self):
        code, eng = self.run_json({"glm": tf(dead_url())})
        self.assertEqual(code, 1)
        self.assertEqual(eng["glm"]["verdict"], "down")
        self.assertTrue(eng["glm"]["problems"])

    def test_down_beats_drift(self):
        s = self.stub(tensorfold_routes(guided="400"))  # drifts: expected guided yes
        code, eng = self.run_json({"a-down": tf(dead_url()), "b-drift": tf(s.url)})
        self.assertEqual(code, 1)
        self.assertEqual(eng["b-drift"]["verdict"], "drift")

    def test_feature_drift_exits_2(self):
        # Registry says logprobs work; the engine refuses them.
        want = dict(GLM_LIKE, logprobs="yes")
        s = self.stub(tensorfold_routes())
        code, eng = self.run_json({"glm": tf(s.url, expected_features=want)})
        self.assertEqual(code, 2)
        e = eng["glm"]
        self.assertEqual(e["verdict"], "drift")
        self.assertTrue(e["features"]["logprobs"]["drift"])
        self.assertEqual(e["features"]["logprobs"]["result"], "no")
        self.assertIn("400", e["features"]["logprobs"]["reason"])
        self.assertEqual(e["sanity"]["result"], "pass")

    def test_wrong_sanity_answer_exits_1(self):
        s = self.stub(tensorfold_routes(answer="41"))
        code, eng = self.run_json({"glm": tf(s.url)})
        self.assertEqual(code, 1)
        self.assertEqual(eng["glm"]["verdict"], "wrong")
        self.assertEqual(eng["glm"]["sanity"]["result"], "fail")
        self.assertEqual(eng["glm"]["features"], {})  # no probes on a wrong engine

    def test_sanity_timeout_on_busy_engine_is_slow_not_down(self):
        import time

        def slow_chat(body):
            time.sleep(1.0)
            return 200, {"choices": [{"message": {"content": "42"}}]}

        r = tensorfold_routes(connections=3)
        r[("POST", "/v1/chat/completions")] = slow_chat
        s = self.stub(r)
        code, eng = self.run_json({"glm": tf(s.url)}, "--timeout", "0.3", "--bg-timeout", "0.3")
        self.assertEqual(code, 1)
        self.assertEqual(eng["glm"]["verdict"], "slow")
        self.assertIn("not down", eng["glm"]["problems"][0])
        # the same timeout on an idle engine is a real failure
        r2 = tensorfold_routes(connections=0)
        r2[("POST", "/v1/chat/completions")] = slow_chat
        s2 = self.stub(r2)
        code, eng = self.run_json({"glm": tf(s2.url)}, "--timeout", "0.3", "--bg-timeout", "0.3")
        self.assertEqual(code, 1)
        self.assertEqual(eng["glm"]["verdict"], "down")

    def test_starved_background_sanity_is_retried_once_at_normal_priority(self):
        import time
        base = tensorfold_routes(connections=3)
        chat = base[("POST", "/v1/chat/completions")]

        def starve_background(body):
            if body.get("priority") == "background":
                time.sleep(1.0)  # queued behind other users' work
            return chat(body)

        base[("POST", "/v1/chat/completions")] = starve_background
        s = self.stub(base)
        code, eng = self.run_json({"glm": tf(s.url)}, "--timeout", "5", "--bg-timeout", "0.3")
        e = eng["glm"]
        self.assertEqual(code, 0)
        self.assertEqual(e["sanity"]["result"], "pass")
        self.assertEqual(e["features"]["background_priority"]["result"], "yes")
        self.assertIn("starved", e["features"]["background_priority"]["reason"])
        self.assertTrue(any("normal priority" in n for n in e["notes"]))
        later = s.posts("/v1/chat/completions")[2:]  # after the starved and the retried sanity
        self.assertTrue(later)
        self.assertTrue(all("priority" not in b for b in later), "probes must not queue behind the load")

    def test_starved_probe_is_retried_at_normal_priority(self):
        import time
        base = tensorfold_routes()
        comp = base[("POST", "/v1/completions")]

        def starve(body):
            if body.get("priority") == "background":
                time.sleep(1.0)
            return comp(body)

        base[("POST", "/v1/completions")] = starve
        s = self.stub(base)
        code, eng = self.run_json({"glm": tf(s.url)}, "--timeout", "5", "--bg-timeout", "0.3")
        self.assertEqual(code, 0)
        self.assertEqual(eng["glm"]["features"]["prompt_logprobs"]["result"], "no")
        self.assertTrue(any("probe waited" in n for n in eng["glm"]["notes"]))
        self.assertEqual(len(s.posts("/v1/completions")), 2)

    def test_null_answer_is_wrong(self):
        r = tensorfold_routes()
        r[("POST", "/v1/chat/completions")] = lambda b: (200, {"choices": [{"message": {"content": None}}]})
        s = self.stub(r)
        code, _ = self.run_check({"glm": tf(s.url)})
        self.assertEqual(code, 1)

    def test_model_mismatch_exits_1(self):
        s = self.stub(tensorfold_routes(model="Other-Model"))
        code, eng = self.run_json({"glm": tf(s.url)})
        self.assertEqual(code, 1)
        self.assertIn("registry expects", eng["glm"]["problems"][0])

    def test_features_are_measured_not_hardcoded(self):
        # Same registry, different engine behaviour: results follow the engine.
        s = self.stub(tensorfold_routes(guided="ignore", logprobs="yes", prompt_logprobs="yes",
                                        background="400"))
        code, eng = self.run_json({"glm": tf(s.url)})
        f = eng["glm"]["features"]
        self.assertEqual(f["guided_choice"]["result"], "no")
        self.assertIn("ignored", f["guided_choice"]["reason"])
        self.assertEqual(f["logprobs"]["result"], "yes")
        self.assertEqual(f["prompt_logprobs"]["result"], "yes")
        self.assertEqual(f["background_priority"]["result"], "no")
        self.assertEqual(code, 2)
        self.assertEqual(eng["glm"]["sanity"]["result"], "pass")  # retried without priority

    def test_unsupported_5xx_is_no_but_other_5xx_is_error(self):
        s = self.stub(tensorfold_routes(guided="500-lib"))
        _, eng = self.run_json({"q": tf(s.url, expected_features={"guided_choice": "no"})})
        self.assertEqual(eng["q"]["features"]["guided_choice"]["result"], "no")
        self.assertIn("libtfgrammar.so is required", eng["q"]["features"]["guided_choice"]["reason"])
        self.assertEqual(eng["q"]["verdict"], "ok")
        s2 = self.stub(tensorfold_routes(guided="500"))
        code, eng = self.run_json({"q": tf(s2.url, expected_features={"guided_choice": "no"})})
        self.assertEqual(eng["q"]["features"]["guided_choice"]["result"], "error")
        self.assertEqual(code, 2)

    def test_feature_without_expectation_is_not_drift(self):
        s = self.stub(tensorfold_routes())
        code, eng = self.run_json({"q": tf(s.url, expected_features={})})
        self.assertEqual(code, 0)
        self.assertIsNone(eng["q"]["features"]["logprobs"]["expected"])

    def test_quick_skips_probes(self):
        s = self.stub(tensorfold_routes())
        code, eng = self.run_json({"glm": tf(s.url)}, "--quick")
        self.assertEqual(code, 0)
        self.assertEqual(eng["glm"]["features"], {})
        self.assertEqual(len(s.posts("/v1/chat/completions")), 1)
        self.assertEqual(s.posts("/v1/completions"), [])

    def test_version_and_load_are_reported(self):
        s = self.stub(tensorfold_routes(version="9.9.9", connections=2))
        _, eng = self.run_json({"q": tf(s.url)})
        self.assertEqual(eng["q"]["version"], "9.9.9")
        self.assertIn("busy (2 running", eng["q"]["load"])

    def test_glm_style_health_ok_flag(self):
        r = tensorfold_routes()
        r[("GET", "/health")] = lambda _: (200, {"ok": True, "backend": "tensorfold",
                                                 "busy": False, "requests_running": 0})
        s = self.stub(r)
        code, eng = self.run_json({"q": tf(s.url)})
        self.assertEqual(code, 0)
        self.assertEqual(eng["q"]["load"], "idle")
        self.assertIn("/v1/models", s.gets)  # model id comes from the model list here

    def test_warming_engine_is_down(self):
        r = tensorfold_routes()
        r[("GET", "/health")] = lambda _: (200, {"status": "ok", "warming": True, "model_ids": ["Stub-Model"]})
        s = self.stub(r)
        code, eng = self.run_json({"q": tf(s.url)})
        self.assertEqual(code, 1)
        self.assertEqual(eng["q"]["verdict"], "down")

    def test_shared_busy_engine_is_left_alone(self):
        s = self.stub(tensorfold_routes(connections=1))
        code, eng = self.run_json({"qwen": tf(s.url, shared=True)})
        self.assertEqual(code, 0)
        e = eng["qwen"]
        self.assertEqual(e["status"], "skipped-busy")
        self.assertEqual(e["sanity"]["result"], "skipped")
        self.assertTrue(all(f["result"] == "skipped" and not f["drift"] for f in e["features"].values()))
        self.assertTrue(all(m == "GET" for m, _, _ in s.log), "only reads on a busy shared engine")
        _, text = self.run_check({"qwen": tf(s.url, shared=True)})
        self.assertIn("busy", text)
        self.assertIn("skipped", text)

    def test_shared_idle_engine_is_checked(self):
        s = self.stub(tensorfold_routes(connections=0))
        code, eng = self.run_json({"qwen": tf(s.url, shared=True)})
        self.assertEqual(code, 0)
        self.assertEqual(eng["qwen"]["sanity"]["result"], "pass")

    def test_force_shared_runs_on_busy_engine(self):
        s = self.stub(tensorfold_routes(connections=1))
        _, eng = self.run_json({"qwen": tf(s.url, shared=True)}, "--force-shared")
        self.assertEqual(eng["qwen"]["sanity"]["result"], "pass")


class OtherKindTests(Base):
    def laya_routes(self, choice="billing", model="laya-rl-agent"):
        return {
            ("GET", "/health"): lambda _: (200, {"status": "ok", "loaded": ["english"], "device": "cuda:0"}),
            ("POST", "/v1/systemone"): lambda b: (200, {
                "model": model,
                "answers": {"intent": {"type": "choice", "choice": choice,
                                       "probabilities": {"billing": 0.9, "other": 0.1}}}}),
        }

    def test_laya_pass_and_wrong(self):
        reg = lambda u: {"laya": {"url": u, "kind": "laya", "model": "laya-rl-agent"}}
        s = self.stub(self.laya_routes())
        code, eng = self.run_json(reg(s.url))
        self.assertEqual(code, 0)
        self.assertIn("billing", eng["laya"]["sanity"]["detail"])
        s2 = self.stub(self.laya_routes(choice="shipping"))
        code, eng = self.run_json(reg(s2.url))
        self.assertEqual(code, 1)
        self.assertEqual(eng["laya"]["verdict"], "wrong")

    def lfm_routes(self, specs):
        return {
            ("GET", "/v1/models"): lambda _: (200, [{"id": "LFM-Test", "kind": "adjudicator"}]),
            ("GET", "/v1/opinion/specs"): lambda _: (200, specs),
            ("POST", "/v1/spans/credentials"): lambda b: (200, [[]]),
        }

    def test_lfm2d_specs_and_exporter(self):
        ex = self.stub({("GET", "/healthz"): lambda _: (200, "ok")})
        s = self.stub(self.lfm_routes([{"id": "abc", "spec": "email-triage-v2"}]))
        cfg = {"url": s.url, "kind": "lfm2d", "model": "LFM-Test", "read": "specs",
               "health_path": "/v1/models", "exporter_url": ex.url}
        code, eng = self.run_json({"lfm": cfg})
        self.assertEqual(code, 0)
        self.assertEqual(ex.gets, ["/healthz"])
        # no spec loaded: the cheap read fails
        s2 = self.stub(self.lfm_routes([]))
        code, eng = self.run_json({"lfm": dict(cfg, url=s2.url)})
        self.assertEqual(code, 1)
        # exporter down: wrong
        code, eng = self.run_json({"lfm": dict(cfg, exporter_url=dead_url())})
        self.assertEqual(code, 1)

    def test_lfm2d_encoders_spans_read(self):
        s = self.stub(self.lfm_routes([]))
        cfg = {"url": s.url, "kind": "lfm2d", "model": "LFM-Test", "read": "spans",
               "health_path": "/v1/models"}
        code, _ = self.run_json({"enc": cfg})
        self.assertEqual(code, 0)
        self.assertEqual(len(s.posts("/v1/spans/credentials")), 1)
        self.assertLessEqual(len(s.posts("/v1/spans/credentials")[0]["inputs"][0]), 100)

    def test_lfm2d_wrong_model_list(self):
        s = self.stub(self.lfm_routes([{"id": "x"}]))
        cfg = {"url": s.url, "kind": "lfm2d", "model": "Not-There", "read": "specs",
               "health_path": "/v1/models"}
        code, _ = self.run_json({"lfm": cfg})
        self.assertEqual(code, 1)

    def jev_routes(self, ready=200, choice="read"):
        return {
            ("GET", "/health"): lambda _: (200, {"status": "ok", "model": "jev-glm-0.1", "k": 8}),
            ("GET", "/ready"): lambda _: (ready, {"ok": ready == 200}),
            ("POST", "/v1/systemone"): lambda b: (200, {
                "model": "jev-glm-0.1",
                "answers": {"verdict": {"type": "choice", "choice": choice,
                                        "probabilities": {"read": 0.9, "destroy": 0.1}}}}),
        }

    def test_jev_glm(self):
        cfg = lambda u: {"j": {"url": u, "kind": "jev-glm", "model": "jev-glm-0.1"}}
        s = self.stub(self.jev_routes())
        code, _ = self.run_json(cfg(s.url))
        self.assertEqual(code, 0)
        s2 = self.stub(self.jev_routes(ready=503))
        code, eng = self.run_json(cfg(s2.url))
        self.assertEqual(code, 1)
        self.assertEqual(s2.posts("/v1/systemone"), [], "no decision when GLM behind it is down")
        s3 = self.stub(self.jev_routes(choice="destroy"))
        code, _ = self.run_json(cfg(s3.url))
        self.assertEqual(code, 1)


class CliTests(Base):
    def test_single_engine_only_touches_that_engine(self):
        a = self.stub(tensorfold_routes())
        b = self.stub(tensorfold_routes())
        code, eng = self.run_json({"a": tf(a.url), "b": tf(b.url)}, "b")
        self.assertEqual(list(eng), ["b"])
        self.assertEqual(a.log, [])
        self.assertTrue(b.log)

    def test_unknown_engine_exits_1(self):
        s = self.stub(tensorfold_routes())
        with contextlib.redirect_stderr(io.StringIO()) as err:
            code, _ = self.run_check({"a": tf(s.url)}, "nope")
        self.assertEqual(code, 1)
        self.assertIn("unknown engine", err.getvalue())

    def test_bad_flag_exits_1_not_2(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
            fleet_check.main(["--no-such-flag"], out=io.StringIO())
        self.assertEqual(cm.exception.code, 1)

    def test_table_output_has_header_summary_and_reasons(self):
        s = self.stub(tensorfold_routes())
        code, text = self.run_check({"glm": tf(s.url)})
        self.assertEqual(code, 0)
        first = text.splitlines()[0]
        for col in ("ENGINE", "STATUS", "MODEL", "LOAD", "SANITY", "LATENCY", "FEATURES"):
            self.assertIn(col, first)
        self.assertIn("glm: logprobs = no: HTTP 400", text)
        self.assertIn("ALL GOOD (exit 0)", text)

    def test_drift_shown_in_table(self):
        s = self.stub(tensorfold_routes())
        code, text = self.run_check({"glm": tf(s.url, expected_features=dict(GLM_LIKE, logprobs="yes"))})
        self.assertEqual(code, 2)
        self.assertIn("logprobs=no!", text)
        self.assertIn("DRIFT logprobs = no (expected yes)", text)

    def test_json_is_deterministic_in_shape(self):
        s = self.stub(tensorfold_routes())
        _, t1 = self.run_check({"glm": tf(s.url)}, "--json")
        self.assertEqual(list(json.loads(t1)), sorted(json.loads(t1)))


class RegistryTests(unittest.TestCase):
    """The real fleet/fleet.toml must be well formed."""

    @classmethod
    def setUpClass(cls):
        with open(REAL_REGISTRY, "rb") as f:
            cls.reg = tomllib.load(f)
        with open(REAL_REGISTRY, encoding="utf-8") as f:
            cls.text = f.read()

    def assert_sorted(self, d, path="root"):
        # TOML needs plain values before sub-tables, so: sorted values, then sorted sub-tables.
        keys = list(d)
        values = sorted(k for k in keys if not isinstance(d[k], dict))
        tables = sorted(k for k in keys if isinstance(d[k], dict))
        self.assertEqual(keys, values + tables, f"keys not sorted in {path}")
        for k, v in d.items():
            if isinstance(v, dict):
                self.assert_sorted(v, f"{path}.{k}")

    def test_keys_are_sorted_everywhere(self):
        self.assert_sorted(self.reg)

    def test_every_engine_has_required_fields_and_a_known_host(self):
        need = {"host", "kind", "model", "supervisor_scope", "supervisor_unit", "url", "watchdog_id"}
        for name, e in self.reg["engines"].items():
            self.assertFalse(need - set(e), f"{name} lacks {need - set(e)}")
            self.assertIn(e["host"], self.reg["hosts"], name)
            self.assertIn(e["kind"], fleet_check.CHECKS, name)
            self.assertIn(e["supervisor_scope"], ("user", "system", "external"), name)
            for feat, val in (e.get("expected_features") or {}).items():
                self.assertIn(feat, fleet_check.FEATURES, f"{name}.{feat}")
                self.assertIn(val, ("yes", "no"), f"{name}.{feat}")

    def test_every_host_has_required_fields(self):
        for name, h in self.reg["hosts"].items():
            for k in ("ip", "notes", "role", "ssh_user"):
                self.assertIn(k, h, f"{name} lacks {k}")

    def test_no_tailscale_addresses_secrets_or_timestamps(self):
        self.assertNotRegex(self.text, r"\b100\.\d+\.\d+\.\d+\b")
        self.assertNotIn(".ts.net", self.text)
        self.assertNotRegex(self.text, r"(?i)api[_-]?key\s*=|token\s*=|password\s*=|secret\s*=")
        self.assertNotRegex(self.text, r"\b20\d\d-\d\d-\d\dT\d\d:")

    def test_known_facts(self):
        e = self.reg["engines"]
        self.assertEqual(e["deepseek"]["supervisor_unit"], "dsv41-boot-start.service")
        self.assertEqual(e["deepseek"]["supervisor_scope"], "user")
        self.assertNotIn("dsv41-exl3", e["deepseek"]["supervisor_unit"])
        self.assertEqual(e["glm"]["supervisor_unit"], "glm53-tf.service")
        self.assertTrue(self.reg["hosts"]["hyper03"]["shared"])
        self.assertTrue(e["qwen"]["shared"])


if __name__ == "__main__":
    unittest.main()
