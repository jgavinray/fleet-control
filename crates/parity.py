#!/usr/bin/env python3
"""Parity harness for the Rust port of fleet-check (gate-owned).

Starts mock engines on loopback ports, writes a fixture registry that points at
them, runs the Python fleet-check and the Rust one with --json on the same
registry, and compares exit code and the comparable JSON fields. No real fleet
is touched.

Two modes are compared for every scenario:
  --quick --json   the ancestor coverage: exit code, engine order, the 14-field
                   comparable projection (counts, not message text)
  --json (full)    the probes run; features are compared as complete objects

The mock engines are deterministic: no scenario sleeps, so no timeouts and no
starvation retries are exercised; every request is answered at once.

usage: parity.py PYTHON_TOOL RUST_BIN
"""
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PY, RS = sys.argv[1], sys.argv[2]
MODEL = "M-1"
SANITY_PROMPT = "What is 17 plus 25? Reply with the number only."
OK_CHAT = {"choices": [{"message": {"content": "42"}}]}


def chat(text):
    return {"choices": [{"message": {"content": text}}]}


# The answers an engine gives when nothing is being tested: sanity passes,
# guided picks a listed choice, logprobs and prompt_logprobs come back, and a
# background-priority probe is accepted. Full-mode scenarios override one
# behaviour each and fall back to this for the rest.
def ideal_chat(b):
    content = (b.get("messages") or [{}])[0].get("content")
    if "guided_choice" in b:
        return 200, chat("apple")
    if b.get("logprobs"):
        return 200, {"choices": [{"message": {"content": "ok"},
                                  "logprobs": {"content": [{"token": "ok", "logprob": -0.01}]}}]}
    if content == SANITY_PROMPT:
        return 200, OK_CHAT
    if b.get("priority") == "background":
        return 200, chat("ok")
    return 200, OK_CHAT


def ideal_plp(b):
    return 200, {"prompt_logprobs": [{"token": " The", "logprob": -0.6}]}


def refuse_bg(b):
    return (400, {"error": "priority not supported"}) if b.get("priority") == "background" else ideal_chat(b)


# name -> dict(engine=registry fields, health=(status, body), models=(status, body),
#              chat=callable(body)->(status, body), completions=..., systemone=(status, body),
#              ready=(status, body), opinion_specs=(status, body), spans=(status, body),
#              healthz=(status, text), refused=True)
def scen():
    s = {}
    healthy = {"ok": True, "model_ids": [MODEL], "version": "1.2.3", "requests_running": 0, "requests_waiting": 0}
    s["ok_ids_in_health"] = dict(health=(200, healthy), chat=lambda b: (200, OK_CHAT))
    s["ok_status_shape"] = dict(health=(200, {"status": "ok"}), models=(200, {"data": [{"id": MODEL}]}),
                                chat=lambda b: (200, chat("The answer is 42.")))
    s["busy_load"] = dict(health=(200, dict(healthy, requests_running=2, requests_waiting=1)), chat=lambda b: (200, OK_CHAT))
    s["live_shape"] = dict(health=(200, {"ok": True, "model_ids": [MODEL], "live": {"connections": 3, "waiting": 1}}),
                           chat=lambda b: (200, OK_CHAT))
    s["wrong_model"] = dict(health=(200, dict(healthy, model_ids=["Other"])), chat=lambda b: (200, OK_CHAT))
    s["not_ok"] = dict(health=(200, {"ok": False}), chat=lambda b: (200, OK_CHAT))
    s["warming"] = dict(health=(200, {"ok": True, "warming": True, "model_ids": [MODEL]}), chat=lambda b: (200, OK_CHAT))
    s["health_500"] = dict(health=(500, {"error": "boom"}), chat=lambda b: (200, OK_CHAT))
    s["models_404"] = dict(health=(200, {"ok": True}), models=(404, {"error": "no"}), chat=lambda b: (200, OK_CHAT))
    s["models_no_ids"] = dict(health=(200, {"ok": True}), models=(200, {"x": 1}), chat=lambda b: (200, OK_CHAT))
    s["sanity_wrong"] = dict(health=(200, healthy), chat=lambda b: (200, chat("41")))
    s["sanity_null"] = dict(health=(200, healthy), chat=lambda b: (200, {"choices": [{"message": {"content": None}}]}))
    s["sanity_500"] = dict(health=(200, healthy), chat=lambda b: (500, {"error": "x"}))
    s["sanity_404"] = dict(health=(200, healthy), chat=lambda b: (404, {"error": "x"}))
    s["bg_accepted"] = dict(engine={"expected_features": {"background_priority": "yes"}}, health=(200, healthy),
                            chat=lambda b: (200, OK_CHAT))
    s["bg_refused"] = dict(engine={"expected_features": {"background_priority": "yes"}}, health=(200, healthy),
                           chat=lambda b: (400, {"error": "priority not supported"}) if "priority" in b else (200, OK_CHAT))
    s["shared_busy"] = dict(engine={"shared": True}, health=(200, dict(healthy, requests_running=2)), chat=lambda b: (200, OK_CHAT))
    s["shared_idle"] = dict(engine={"shared": True}, health=(200, healthy), chat=lambda b: (200, OK_CHAT))
    s["unknown_kind"] = dict(engine={"kind": "mystery"}, health=(200, healthy), chat=lambda b: (200, OK_CHAT))
    s["refused"] = dict(refused=True)
    # messy engine names: leading digit, upper vs lower case ordering, dash and underscore
    for odd in ("9lives", "Zeta", "alpha", "a-b_c"):
        s[odd] = dict(health=(200, healthy), chat=lambda b: (200, OK_CHAT))

    # --- tensorfold full mode: the probes run ---
    s["guided_yes"] = dict(health=(200, healthy), chat=lambda b: (200, chat("apple")) if "guided_choice" in b else ideal_chat(b))
    s["guided_refused"] = dict(health=(200, healthy), chat=lambda b: (400, {"error": "guided_choice not supported"}) if "guided_choice" in b
                               else ideal_chat(b))
    s["guided_null"] = dict(health=(200, healthy), chat=lambda b: (200, {"choices": [{"message": {"content": None}}]}) if "guided_choice" in b
                            else ideal_chat(b))
    s["guided_offlist"] = dict(health=(200, healthy), chat=lambda b: (200, chat("granite and apple")) if "guided_choice" in b
                               else ideal_chat(b))
    s["logprobs_yes"] = dict(health=(200, healthy), chat=ideal_chat)
    s["logprobs_absent"] = dict(health=(200, healthy), chat=lambda b: (200, chat("ok")) if b.get("logprobs") else ideal_chat(b))
    s["plp_yes"] = dict(health=(200, healthy), chat=ideal_chat, completions=ideal_plp)
    s["plp_absent"] = dict(health=(200, healthy), chat=ideal_chat, completions=lambda b: (200, {"choices": [], "id": "cmpl"}))
    s["bg_refused_sanity_yes"] = dict(health=(200, healthy), engine={"expected_features": {"background_priority": "yes"}},
                                      chat=refuse_bg, completions=ideal_plp)
    s["bg_refused_sanity_no"] = dict(health=(200, healthy), engine={"expected_features": {"background_priority": "no"}},
                                     chat=refuse_bg, completions=ideal_plp)
    s["lp_drift"] = dict(health=(200, healthy), engine={"expected_features": {"logprobs": "yes"}},
                         chat=lambda b: (200, chat("ok")) if b.get("logprobs") else ideal_chat(b),
                         completions=ideal_plp)

    # --- the other kinds (full mode only; --quick does not touch them) ---
    s["laya_ok"] = dict(engine={"kind": "laya"},
                        health=(200, {"ok": True, "loaded": ["en"], "device": "cuda:0"}),
                        systemone=(200, {"answers": {"intent": {"choice": "billing",
                                                                "probabilities": {"billing": 0.87}}},
                                         "model": MODEL}))
    s["laya_wrong_choice"] = dict(engine={"kind": "laya"},
                                  health=(200, {"ok": True, "loaded": [], "device": "cpu"}),
                                  systemone=(200, {"answers": {"intent": {"choice": "shipping"}}, "model": MODEL}))
    s["laya_model_mismatch"] = dict(engine={"kind": "laya"},
                                    health=(200, {"ok": True, "loaded": ["en"], "device": "cuda:0"}),
                                    systemone=(200, {"answers": {"intent": {"choice": "billing"}}, "model": "Other"}))
    s["laya_health_bad"] = dict(engine={"kind": "laya"}, health=(200, {"ok": False}))
    s["lfm2d_specs_ok"] = dict(engine={"kind": "lfm2d"}, models=(200, [{"id": MODEL}]),
                               opinion_specs=(200, [{"id": "s1", "name": "bash-risk"}]))
    s["lfm2d_wrong_model"] = dict(engine={"kind": "lfm2d"}, models=(200, [{"id": "Other"}]))
    s["lfm2d_specs_empty"] = dict(engine={"kind": "lfm2d"}, models=(200, [{"id": MODEL}]), opinion_specs=(200, []))
    s["lfm2d_spans_ok"] = dict(engine={"kind": "lfm2d", "read": "spans"}, models=(200, [{"id": MODEL}]),
                               spans=(200, [[{"text": "sk-1", "start": 0, "end": 5}]]))
    s["lfm2d_exporter_ok"] = dict(engine={"kind": "lfm2d", "exporter_url": "SELF"}, models=(200, [{"id": MODEL}]),
                                  opinion_specs=(200, [{"id": "s1"}]), healthz=(200, "ok"))
    s["lfm2d_exporter_bad"] = dict(engine={"kind": "lfm2d", "exporter_url": "SELF"}, models=(200, [{"id": MODEL}]),
                                   opinion_specs=(200, [{"id": "s1"}]), healthz=(200, "starting"))
    s["jev_ok"] = dict(engine={"kind": "jev-glm"}, health=(200, {"ok": True, "model": MODEL}),
                       ready=(200, {"status": "ok"}),
                       systemone=(200, {"answers": {"verdict": {"choice": "read",
                                                                "probabilities": {"read": 0.9}}},
                                        "model": MODEL}))
    s["jev_ready_fail"] = dict(engine={"kind": "jev-glm"}, health=(200, {"ok": True, "model": MODEL}),
                               ready=(500, {"error": "glm is down"}))
    s["jev_wrong_choice"] = dict(engine={"kind": "jev-glm"}, health=(200, {"ok": True, "model": MODEL}),
                                 ready=(200, {"status": "ok"}),
                                 systemone=(200, {"answers": {"verdict": {"choice": "destroy"}}, "model": MODEL}))
    s["jev_model_mismatch"] = dict(engine={"kind": "jev-glm"}, health=(200, {"ok": True, "model": MODEL}),
                                   ready=(200, {"status": "ok"}),
                                   systemone=(200, {"answers": {"verdict": {"choice": "read"}}, "model": "Other"}))

    # --- the edge cases the fleet-check alignment pinned; if these drift the
    # old behaviour is back ---
    # A shared engine whose health reports an integral float: the counter is
    # still truthy, so quick and full mode both report skipped-busy instead of
    # probing (the regression used to get probed).
    s["shared_float_busy"] = dict(engine={"shared": True}, health=(200, dict(healthy, requests_running=1.0)),
                                  chat=lambda b: (200, OK_CHAT))
    # model_ids that is not a list of strings. The string form is a character
    # sequence: ids[0] is "M" and the registry model passes because
    # "M-1" in "M-1" is a substring hit. The dict form dies on ids[0] and the
    # int form dies in the wrong-model message join -- both uncaught (exit 1,
    # a traceback on stderr, no stdout), which the usage-style compare covers.
    s["model_ids_string"] = dict(health=(200, dict(healthy, model_ids=MODEL)), chat=lambda b: (200, OK_CHAT))
    s["model_ids_object"] = dict(health=(200, dict(healthy, model_ids={"a": 1})), chat=lambda b: (200, OK_CHAT))
    s["model_ids_int"] = dict(engine={"model": "5"}, health=(200, dict(healthy, model_ids=[5])),
                              chat=lambda b: (200, OK_CHAT))
    # A sanity reply with a body that is not valid UTF-8: status 200, but the
    # sanity answer is unreadable, so the result is a sanity failure, not a
    # transport error.
    s["sanity_raw_bytes"] = dict(health=(200, healthy), chat_raw=b"\x80\x81\x82 not json")
    # expected_features may pin a raw number: 1 on a healthy engine is drift
    # (exit 2), and the features map carries expected: 1 as a number.
    s["expected_logprobs_one"] = dict(engine={"expected_features": {"logprobs": 1}}, health=(200, healthy),
                                      chat=ideal_chat, completions=ideal_plp)
    # A string "false" is truthy: a busy engine is still skipped as shared.
    s["shared_string_false"] = dict(engine={"shared": "false"}, health=(200, dict(healthy, requests_running=2)),
                                    chat=lambda b: (200, OK_CHAT))
    # read = "" is not "specs", so the spans endpoint is probed; health_path =
    # "" requests the base URL, which only this mock serves (no /health route).
    s["read_empty"] = dict(engine={"kind": "lfm2d", "read": ""}, models=(200, [{"id": MODEL}]),
                           spans=(200, [[{"text": "sk-1", "start": 0, "end": 5}]]))
    s["health_path_empty"] = dict(engine={"health_path": ""}, root=(200, healthy), chat=lambda b: (200, OK_CHAT))
    return s


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_raw(self, status, text):
        data = text.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_bytes(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        sc = self.server.sc
        if self.path == "/":
            return self._send(*sc["root"]) if "root" in sc else self._send(404, {"error": "no route"})
        if self.path == "/health":
            return self._send(*sc["health"]) if "health" in sc else self._send(404, {"error": "no route"})
        if self.path == "/v1/models":
            return self._send(*sc.get("models", (404, {"error": "none"})))
        if self.path == "/v1/opinion/specs":
            return self._send(*sc["opinion_specs"]) if "opinion_specs" in sc else self._send(404, {"error": "no route"})
        if self.path == "/ready":
            return self._send(*sc["ready"]) if "ready" in sc else self._send(404, {"error": "no route"})
        if self.path == "/healthz":
            return self._send_raw(*sc["healthz"]) if "healthz" in sc else self._send_raw(404, "no route")
        self._send(404, {"error": "no route"})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}")
        sc = self.server.sc
        if self.path == "/v1/chat/completions":
            if "chat_raw" in sc:  # a body that is not valid UTF-8, sent verbatim
                return self._send_bytes(200, sc["chat_raw"])
            if "chat" not in sc:
                return self._send(404, {"error": "no route"})
            return self._send(*sc["chat"](body))
        if self.path == "/v1/completions":
            return self._send(*sc["completions"](body)) if "completions" in sc else self._send(404, {"error": "no route"})
        if self.path == "/v1/systemone":
            return self._send(*sc["systemone"]) if "systemone" in sc else self._send(404, {"error": "no route"})
        if self.path == "/v1/spans/credentials":
            return self._send(*sc["spans"]) if "spans" in sc else self._send(404, {"error": "no route"})
        self._send(404, {"error": "no route"})


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


# The refused engine's port is held bound but never listened on: connects get
# RST (ECONNREFUSED) for sure, and no mock server can silently take the port.
hold = socket.socket()
hold.bind(("127.0.0.1", 0))
refused_port = hold.getsockname()[1]

servers = []
engines = {}
for name, sc in scen().items():
    if sc.get("refused"):
        port = refused_port
    else:
        srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        srv.sc = sc
        port = srv.server_address[1]
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        servers.append(srv)
    cfg = {"url": f"http://127.0.0.1:{port}", "model": MODEL, "host": "h-" + name, "kind": "tensorfold"}
    cfg.update(sc.get("engine", {}))
    if cfg.get("exporter_url") == "SELF":
        cfg["exporter_url"] = cfg["url"]  # the exporter is probed on the same mock server
    engines[name] = cfg


def toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):  # expected_features may pin a raw number
        return str(v)
    if isinstance(v, str):
        return json.dumps(v)
    raise TypeError(v)


lines = []
for name in sorted(engines):
    cfg = engines[name]
    lines.append(f"[engines.{name}]")
    for k in sorted(k for k in cfg if k != "expected_features"):
        lines.append(f"{k} = {toml_value(cfg[k])}")
    if "expected_features" in cfg:
        lines.append(f"\n[engines.{name}.expected_features]")
        for k in sorted(cfg["expected_features"]):
            lines.append(f"{k} = {toml_value(cfg['expected_features'][k])}")
    lines.append("")
tmp = tempfile.mkdtemp(prefix="fleetparity-")
reg = tmp + "/fleet.toml"
open(reg, "w").write("\n".join(lines))

# A second fixture per malformed registry shape: engines that is not a table,
# an engine entry that is a string, an engine without url, an engine without
# model. The Python tool dies uncaught on each (exit 1, traceback on stderr,
# no stdout) and the Rust tool rejects the file at load; the usage-style
# compare covers both.
bad_regs = {}
for name, content in {
    "engines-not-a-table": "engines = 5\n",
    "engine-entry-is-a-string": 'engines = { bad = "hello" }\n',
    "engine-without-url": '[engines.nourl]\nmodel = "M-1"\nhost = "h"\n',
    "engine-without-model": '[engines.nomodel]\nurl = "http://127.0.0.1:1"\nhost = "h"\nkind = "tensorfold"\n',
}.items():
    path = os.path.join(tmp, f"bad-{name}.toml")
    open(path, "w").write(content)
    bad_regs[name] = path

problems = []
n_compared = 0


def run(cmd, args, cwd=None):
    p = subprocess.run(cmd + args, capture_output=True, text=True, timeout=120, cwd=cwd)
    return p.returncode, p.stdout, p.stderr


def comparable(e):
    sanity = e.get("sanity", {})
    return {
        "engine": e.get("engine"), "url": e.get("url"), "host": e.get("host"), "kind": e.get("kind"),
        "verdict": e.get("verdict"), "status": e.get("status"), "model": e.get("model"),
        "version": e.get("version"), "load": e.get("load"),
        "sanity_result": sanity.get("result"), "sanity_latency_is_null": sanity.get("latency_s") is None,
        "features": e.get("features"), "problem_count": len(e.get("problems", [])),
        "note_count": len(e.get("notes", [])), "requests": e.get("requests"),
    }


def compare(label, args, cwd=None, env_reg=None):
    global n_compared
    n_compared += 1
    if env_reg:
        os.environ["FLEET_REGISTRY"] = env_reg
    try:
        pc, po, pe = run([sys.executable, "-I", PY] if PY.endswith(".py") else [PY], args, cwd)
        rc, ro, re_ = run([RS], args, cwd)
    finally:
        os.environ.pop("FLEET_REGISTRY", None)
    if pc != rc:
        problems.append(f"{label}: exit code python={pc} rust={rc} (rust stderr: {re_.strip()[:200]})")
        return
    if pc == 1 and not po.strip():
        if not re_.strip():
            problems.append(f"{label}: rust failed with exit 1 but printed nothing on stderr")
        return  # usage-style failure: only the exit code is compared (and that stderr has a message)
    if re_.strip():
        problems.append(f"{label}: rust wrote to stderr on a normal run: {re_.strip()[:200]!r}")
    if pe.strip():
        problems.append(f"{label}: python wrote to stderr on a normal run: {pe.strip()[:200]!r}")
    try:
        pj, rj = json.loads(po), json.loads(ro)
    except ValueError as e:
        problems.append(f"{label}: output is not JSON ({e}); rust stdout starts {ro[:120]!r}")
        return
    if pj.get("exit_code") != rj.get("exit_code"):
        problems.append(f"{label}: JSON exit_code python={pj.get('exit_code')} rust={rj.get('exit_code')}")
    pe_, re2 = pj.get("engines", []), rj.get("engines", [])
    if [e.get("engine") for e in pe_] != [e.get("engine") for e in re2]:
        problems.append(f"{label}: engine order python={[e.get('engine') for e in pe_]} rust={[e.get('engine') for e in re2]}")
        return
    for a, b in zip(pe_, re2):
        ca, cb = comparable(a), comparable(b)
        if ca != cb:
            diff = {k: (ca[k], cb[k]) for k in ca if ca[k] != cb[k]}
            problems.append(f"{label}: engine {a.get('engine')} differs (python, rust): {diff}")


def compare_help(label, args):
    """A run that prints help and exits 0 (argparse ignores a value attached to
    -h). The usage text itself is compared: the Rust port carries it verbatim."""
    global n_compared
    n_compared += 1
    pc, po, pe = run([sys.executable, "-I", PY] if PY.endswith(".py") else [PY], args)
    rc, ro, re_ = run([RS], args)
    if pc != rc:
        problems.append(f"{label}: exit code python={pc} rust={rc} (rust stderr: {re_.strip()[:200]})")
    elif po != ro:
        problems.append(f"{label}: help text differs (python {len(po)} bytes, rust {len(ro)} bytes)")
    elif pe.strip() or re_.strip():
        problems.append(f"{label}: help run wrote to stderr "
                        f"(python: {pe.strip()[:120]!r}, rust: {re_.strip()[:120]!r})")


# ---- --quick --json: the ancestor coverage, verbatim ----
compare("all engines", ["--quick", "--json", "--registry", reg])
for name in sorted(engines):
    compare(f"engine {name}", ["--quick", "--json", "--registry", reg, name])
compare("two engines in the order given", ["--quick", "--json", "--registry", reg, "wrong_model", "ok_ids_in_health"])
# argument handling, taken from how the Python (argparse) reads its command line
compare("flags first, engines last", ["--registry", reg, "--json", "--quick", "ok_ids_in_health", "9lives"])
compare("engines on both sides of the flags", ["ok_ids_in_health", "--quick", "--json", "--registry", reg, "alpha", "Zeta"])
compare("--registry=PATH form", ["--quick", "--json", f"--registry={reg}", "a-b_c"])
compare("relative registry path", ["--quick", "--json", "--registry", "./fleet.toml", "9lives"], cwd=tmp)
compare("registry from FLEET_REGISTRY", ["--quick", "--json", "9lives"], env_reg=reg)
for val in ("5", ".5", "1e1", "0.25"):
    compare(f"--timeout {val}", ["--quick", "--json", "--registry", reg, "--timeout", val, "9lives"])
    compare(f"--bg-timeout {val}", ["--quick", "--json", "--registry", reg, "--bg-timeout", val, "bg_refused"])
compare("--timeout=7 form", ["--quick", "--json", "--registry", reg, "--timeout=7", "9lives"])
compare("--bg-timeout=.5 form", ["--quick", "--json", "--bg-timeout=.5", "--registry", reg, "bg_refused"])
compare("--timeout not a number", ["--quick", "--json", "--registry", reg, "--timeout", "abc", "9lives"])
compare("--timeout path-like value", ["--quick", "--json", "--registry", reg, "--timeout", "/x", "9lives"])
compare("--timeout without a value", ["--quick", "--json", "--registry", reg, "9lives", "--timeout"])
compare("--registry without a value", ["--quick", "--json", "--registry"])
compare("--force-shared", ["--quick", "--json", "--force-shared", "--registry", reg, "shared_busy"])
compare("repeated engine name (checked twice)", ["--quick", "--json", "--registry", reg, "wrong_model", "wrong_model"])
compare("unknown engine", ["--quick", "--json", "--registry", reg, "nope"])
compare("missing registry", ["--quick", "--json", "--registry", tmp + "/missing.toml"])
compare("bad flag", ["--quick", "--json", "--registry", reg, "--no-such-flag"])

# ---- full mode (no --quick): the probes run; features compared whole ----
compare("all engines, full mode", ["--json", "--registry", reg])
for name in sorted(engines):
    compare(f"engine {name}, full mode", ["--json", "--registry", reg, name])
compare("down and drift combined (exit 1 wins)", ["--json", "--registry", reg, "not_ok", "lp_drift"])
compare("--force-shared, full mode", ["--json", "--force-shared", "--registry", reg, "shared_busy"])

# ---- the edge cases the fleet-check alignment pinned ----
# A non-finite --timeout makes every request fail, so the engine is down after
# one request; both tools agree (the Python client's socket layer refuses an
# infinite timestamp, the Rust client refuses the deadline itself).
compare("--timeout inf (a non-finite timeout fails the request)",
        ["--quick", "--json", "--registry", reg, "--timeout", "inf", "9lives"])
compare("--timeout 1e400 (overflows to infinity, same shape)",
        ["--quick", "--json", "--registry", reg, "--timeout", "1e400", "9lives"])
# An explicit argument on a flag that takes none is a usage error: exit 1, the
# usage block and the message on stderr, nothing on stdout.
compare("--help=x (explicit argument on a flag that takes none)", ["--help=x"])
compare("-h=foo (same, short form)", ["-h=foo"])
compare_help("-hfoo (an attached value is ignored, help is printed)", ["-hfoo"])
for name, path in bad_regs.items():
    compare(f"malformed registry: {name.replace('-', ' ')}", ["--quick", "--json", "--registry", path])

hold.close()
for srv in servers:
    srv.shutdown()
for p in problems:
    print("GATE FAIL (parity):", p, file=sys.stderr)
print(f"parity: {n_compared} comparisons, {len(problems)} problem(s)")
sys.exit(1 if problems else 0)
