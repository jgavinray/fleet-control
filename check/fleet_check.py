"""fleet-check: is every model engine in fleet.toml up and behaving?

Standard library only (Python 3.11+ for tomllib).

Exit codes:
  0  every engine is reachable, answers its sanity question, and no feature drifted
  1  at least one engine is down or answered wrong
  2  engines are fine but a feature differs from `expected_features` (drift)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request

FEATURES = ("guided_choice", "background_priority", "logprobs", "prompt_logprobs")
SHORT = {
    "guided_choice": "guided",
    "background_priority": "bg",
    "logprobs": "logprobs",
    "prompt_logprobs": "prompt_lp",
}
SANITY_PROMPT = "What is 17 plus 25? Reply with the number only."
NO_THINK = {"enable_thinking": False}
# A 5xx whose body says one of these means "this engine does not do that",
# not "this engine is broken".
UNSUPPORTED_RE = re.compile(r"required|not supported|unsupported|not implemented|libtf", re.I)

DEFAULT_REGISTRY = os.path.join(
    os.path.dirname(os.path.dirname(os.path.realpath(__file__))), "fleet.toml"
)

# The repo is public, so fleet.toml holds Tailscale names only. The owner's
# standing rule is to connect over the home LAN, so an optional untracked file
# maps a host to its LAN address:
#
#   [hosts.hyper01]
#   address = "192.0.2.10"
#
# A URL whose host is that host's name (hyper01, or hyper01.<tailnet>.ts.net)
# is rewritten to the address. The same rule lives in fleetlib.registry.
DEFAULT_LOCAL = os.path.join(os.path.expanduser("~"), ".config", "fleet-control", "local.toml")


def load_local_addresses(path=None):
    """{host name: address} from the local override file; {} if there is none."""
    path = path or os.environ.get("FLEET_LOCAL", DEFAULT_LOCAL)
    try:
        with open(path, "rb") as f:
            doc = tomllib.load(f)
    except OSError:
        return {}
    except tomllib.TOMLDecodeError as e:
        print(f"fleet-check: ignoring unreadable local override {path}: {e}", file=sys.stderr)
        return {}
    out = {}
    for name, h in (doc.get("hosts") or {}).items():
        if isinstance(h, dict) and isinstance(h.get("address"), str) and h["address"]:
            out[name] = h["address"]
    return out


def apply_local_addresses(url, addresses):
    """Swap the host part of `url` for its local address, if one is set."""
    parts = urllib.parse.urlsplit(url)
    host = parts.hostname or ""
    name = host.split(".")[0]
    if name not in addresses or not (host == name or host.endswith(".ts.net")):
        return url
    addr = addresses[name]
    if ":" in addr:
        addr = f"[{addr}]"
    netloc = addr + (f":{parts.port}" if parts.port else "")
    return urllib.parse.urlunsplit(parts._replace(netloc=netloc))


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------

class Resp:
    """One HTTP answer. status 0 means no answer at all (refused, timeout)."""

    def __init__(self, status, text, elapsed, error=""):
        self.status = status
        self.text = text
        self.elapsed = elapsed
        self.error = error
        try:
            self.json = json.loads(text) if text else None
        except ValueError:
            self.json = None

    @property
    def ok(self):
        return self.status == 200

    def why(self, limit=110):
        """A short reason for a failed answer, for the report."""
        if self.status == 0:
            return self.error[:limit]
        return f"HTTP {self.status}: {self.message(limit)}"

    def message(self, limit=110):
        """The message text out of an error body."""
        j = self.json
        msg = None
        if isinstance(j, dict):
            err = j.get("error")
            if isinstance(err, dict):
                msg = err.get("message")
            elif isinstance(err, str):
                msg = err
            if msg is None:
                detail = j.get("detail")
                if isinstance(detail, dict):
                    msg = detail.get("message") or json.dumps(detail)
                elif detail is not None:
                    msg = detail if isinstance(detail, str) else json.dumps(detail)
            if msg is None:
                msg = j.get("message")
        if msg is None:
            msg = self.text.strip()
        return " ".join(str(msg).split())[:limit]


class Client:
    """Sequential HTTP client that counts requests."""

    def __init__(self, base, timeout):
        self.base = base.rstrip("/")
        self.timeout = timeout
        self.count = 0

    def request(self, method, path_or_url, body=None, timeout=None):
        url = path_or_url if path_or_url.startswith("http") else self.base + path_or_url
        data = None
        headers = {"Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        self.count += 1
        t0 = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=timeout or self.timeout) as r:
                text = r.read().decode("utf-8", "replace")
                return Resp(r.status, text, time.monotonic() - t0)
        except urllib.error.HTTPError as e:
            text = e.read().decode("utf-8", "replace")
            e.close()
            return Resp(e.code, text, time.monotonic() - t0)
        except Exception as e:  # refused, DNS, timeout, reset
            reason = getattr(e, "reason", None) or e
            return Resp(0, "", time.monotonic() - t0, error=f"{type(e).__name__}: {reason}")

    def get(self, path, **kw):
        return self.request("GET", path, **kw)

    def post(self, path, body, **kw):
        return self.request("POST", path, body=body, **kw)


# ---------------------------------------------------------------------------
# Result record
# ---------------------------------------------------------------------------

class Result:
    def __init__(self, name, cfg):
        self.name = name
        self.cfg = cfg
        self.status = "ok"  # ok | slow | wrong | down | skipped-busy
        self.model = ""
        self.version = ""
        self.load = ""
        self.sanity = {"result": "n/a", "detail": "", "latency_s": None}
        self.features = {}  # name -> {result, reason, expected, drift}
        self.problems = []  # things that make the engine down or wrong
        self.notes = []
        self.requests = 0

    RANK = {"ok": 0, "skipped-busy": 0, "slow": 1, "wrong": 2, "down": 3}

    def fail(self, status, why):
        """Record a problem. The worst status wins: down, then wrong, then slow."""
        if self.RANK[status] > self.RANK[self.status]:
            self.status = status
        self.problems.append(why)

    @property
    def drifted(self):
        return [n for n, f in self.features.items() if f["drift"]]

    @property
    def verdict(self):
        if self.status in ("down", "wrong", "slow"):
            return self.status
        if self.drifted:
            return "drift"
        return "ok"

    def to_json(self):
        return {
            "engine": self.name,
            "url": self.cfg.get("url", ""),
            "host": self.cfg.get("host", ""),
            "kind": self.cfg.get("kind", ""),
            "verdict": self.verdict,
            "status": self.status,
            "model": self.model,
            "version": self.version,
            "load": self.load,
            "sanity": self.sanity,
            "features": self.features,
            "problems": self.problems,
            "notes": self.notes,
            "requests": self.requests,
        }


# ---------------------------------------------------------------------------
# Helpers shared by the checks
# ---------------------------------------------------------------------------

def health_up(j):
    """TensorFold engines report health in two shapes: {ok: true} or {status: "ok"}."""
    if not isinstance(j, dict):
        return False
    if "ok" in j:
        return j["ok"] is True
    return j.get("status") == "ok"


def describe_load(h):
    """(text, in-flight count) from whichever counters the health body exposes."""
    if not isinstance(h, dict):
        return "", 0
    running = h.get("requests_running")
    waiting = h.get("requests_waiting")
    live = h.get("live") if isinstance(h.get("live"), dict) else {}
    if running is None:
        running = live.get("connections")
    if waiting is None:
        waiting = live.get("waiting")
    if running is None and waiting is None:
        return "", 0
    running = running or 0
    waiting = waiting or 0
    if running or waiting:
        return f"busy ({running} running, {waiting} waiting)", running + waiting
    return "idle", 0


def chat_body(model, content, max_tokens=16, **extra):
    body = {
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "chat_template_kwargs": dict(NO_THINK),
    }
    body.update(extra)
    return body


def chat_text(resp):
    try:
        return resp.json["choices"][0]["message"].get("content")
    except (TypeError, KeyError, IndexError, AttributeError):
        return None


def classify_refusal(resp):
    """Turn a non-200 answer into (result, reason) for a feature probe."""
    if resp.status == 0:
        return "error", resp.error
    if 400 <= resp.status < 500 or resp.status == 501:
        return "no", resp.why()
    if resp.status >= 500 and UNSUPPORTED_RE.search(resp.text or ""):
        return "no", resp.why()
    return "error", resp.why()


def record_feature(res, name, result, reason):
    expected = (res.cfg.get("expected_features") or {}).get(name)
    res.features[name] = {
        "result": result,
        "reason": reason,
        "expected": expected,
        "drift": result != "skipped" and expected is not None and result != expected,
    }


# ---------------------------------------------------------------------------
# Probes (TensorFold: OpenAI-style chat/completions)
# ---------------------------------------------------------------------------

def probe_guided(send, model):
    choices = ["apple", "granite"]
    r = send("/v1/chat/completions",
             chat_body(model, "Which of these is a fruit?", 8, guided_choice=choices))
    if not r.ok:
        return classify_refusal(r)
    text = chat_text(r)
    if text is None:
        return "no", "HTTP 200 but content was null (guidance ignored, or token budget spent thinking)"
    if text.strip() in choices:
        return "yes", f"answered {text.strip()!r}"
    return "no", f"HTTP 200 but answered {text.strip()[:40]!r}, not one of the choices (ignored)"


def probe_background(c, model):
    r = c.post("/v1/chat/completions", chat_body(model, "Say ok.", 4, priority="background"))
    if r.ok:
        return "yes", "accepted (HTTP 200)"
    return classify_refusal(r)


def probe_logprobs(send, model):
    r = send("/v1/chat/completions",
             chat_body(model, "Say ok.", 1, logprobs=True, top_logprobs=1))
    if not r.ok:
        return classify_refusal(r)
    try:
        lp = r.json["choices"][0].get("logprobs")
    except (TypeError, KeyError, IndexError, AttributeError):
        lp = None
    if lp:
        return "yes", "logprobs returned"
    return "no", "HTTP 200 but no logprobs in the answer (silently ignored)"


def probe_prompt_logprobs(send, model):
    body = {
        "model": model,
        "prompt": "The sky is",
        "max_tokens": 1,
        "temperature": 0,
        "prompt_logprobs": 1,
    }
    r = send("/v1/completions", body)
    if not r.ok:
        return classify_refusal(r)
    j = r.json if isinstance(r.json, dict) else {}
    found = j.get("prompt_logprobs")
    if found is None:
        try:
            found = j["choices"][0].get("prompt_logprobs")
        except (KeyError, IndexError, TypeError, AttributeError):
            found = None
    if found:
        return "yes", "prompt_logprobs returned"
    return "no", "HTTP 200 but no prompt_logprobs field (silently ignored)"


# ---------------------------------------------------------------------------
# Engine kinds
# ---------------------------------------------------------------------------

def check_tensorfold(res, c, opts):
    cfg = res.cfg
    model = cfg["model"]
    expected = cfg.get("expected_features") or {}

    h = c.get(cfg.get("health_path", "/health"))
    if not h.ok:
        res.fail("down", f"health: {h.why()}")
        return
    hj = h.json
    if not health_up(hj):
        res.fail("down", f"health says not ok: {h.message(80)}")
        return
    if hj.get("warming"):
        res.fail("down", "still warming up (health.warming is true)")
        return
    res.load, busy = describe_load(hj)
    res.version = str(hj.get("version") or "")

    ids = hj.get("model_ids")
    if not ids:
        m = c.get("/v1/models")
        if not m.ok:
            res.fail("down", f"model list: {m.why()}")
            return
        try:
            ids = [d["id"] for d in m.json["data"]]
        except (TypeError, KeyError):
            res.fail("wrong", "model list has no data[].id")
            return
    res.model = ids[0] if ids else ""
    if model not in ids:
        res.fail("wrong", f"model is {', '.join(ids) or 'none'}, registry expects {model}")
        return

    if cfg.get("shared") and busy and not opts.force_shared:
        res.status = "skipped-busy"
        res.sanity = {"result": "skipped", "detail": "shared engine is busy", "latency_s": None}
        res.notes.append(
            "shared engine is busy (live.connections > 0): sanity question and probes "
            "skipped so the other user is not slowed. Re-run when idle, or use --force-shared."
        )
        if not opts.quick:
            for f in FEATURES:
                record_feature(res, f, "skipped", "shared engine busy")
        return

    # Sanity question. It carries priority:"background" when the registry says
    # the engine accepts it; a 4xx then means the engine refuses it, so retry plain.
    want_bg = expected.get("background_priority") == "yes"
    extra = {"priority": "background"} if want_bg else {}
    r = c.post("/v1/chat/completions", chat_body(model, SANITY_PROMPT, 16, **extra),
               timeout=opts.bg_timeout if want_bg else None)
    bg_result = None
    bg_ok = want_bg  # may we keep sending priority:"background" on the probes?
    spent = r.elapsed
    if want_bg:
        if r.ok:
            bg_result = ("yes", "accepted with the sanity request (HTTP 200)")
        elif 400 <= r.status < 500:
            bg_result = classify_refusal(r)
            bg_ok = False
            r = c.post("/v1/chat/completions", chat_body(model, SANITY_PROMPT, 16))
            spent += r.elapsed
        elif r.status == 0 and "timed out" in r.error and busy:
            # Accepted but starved: a saturated engine serves background last.
            bg_result = ("yes", f"accepted, but starved {opts.bg_timeout:g}s behind other work; "
                                "sanity re-sent at normal priority")
            bg_ok = False
            res.notes.append(f"background-priority sanity request waited {opts.bg_timeout:g}s "
                             f"behind other work ({res.load}); re-sent once at normal priority")
            r = c.post("/v1/chat/completions", chat_body(model, SANITY_PROMPT, 16))
            spent += r.elapsed
    res.sanity["latency_s"] = round(spent, 3)
    if not r.ok:
        res.sanity.update(result="error", detail=r.why())
        if r.status == 0 and "timed out" in r.error and busy:
            # Background priority queues behind everyone; a busy engine is not a dead one.
            res.fail("slow", f"sanity request timed out after {c.timeout:g}s while the engine is "
                             f"{res.load}: not down, but it did not answer in time "
                             "(re-run later, or raise --timeout)")
        else:
            res.fail("down" if r.status == 0 or r.status >= 500 else "wrong",
                     f"sanity request: {res.sanity['detail']}")
        return
    text = chat_text(r)
    shown = "null" if text is None else repr(text.strip()[:30])
    if text is not None and re.fullmatch(r"\W*42\W*", text):
        res.sanity.update(result="pass", detail=f"answered {shown}")
    else:
        res.sanity.update(result="fail", detail=f"answered {shown}, expected '42'")
        res.fail("wrong", f"sanity: {res.sanity['detail']}")
        return

    if opts.quick:
        return

    if bg_result is None:
        bg_result = probe_background(c, model)
    record_feature(res, "background_priority", *bg_result)
    # Priority on the probes only where the engine accepts it and has not starved us.
    state = {"bg": bg_ok and bg_result[0] == "yes"}

    def send(path, body):
        if not state["bg"]:
            return c.post(path, body)
        r = c.post(path, dict(body, priority="background"), timeout=opts.bg_timeout)
        if r.status == 0 and "timed out" in r.error:
            state["bg"] = False
            res.notes.append(f"a background-priority probe waited {opts.bg_timeout:g}s behind other "
                             "work; it and the remaining probes were re-sent at normal priority")
            r = c.post(path, body)
        return r

    record_feature(res, "guided_choice", *probe_guided(send, model))
    record_feature(res, "logprobs", *probe_logprobs(send, model))
    record_feature(res, "prompt_logprobs", *probe_prompt_logprobs(send, model))


def check_laya(res, c, opts):
    cfg = res.cfg
    h = c.get(cfg.get("health_path", "/health"))
    if not h.ok:
        res.fail("down", f"health: {h.why()}")
        return
    if not health_up(h.json):
        res.fail("down", f"health says not ok: {h.message(80)}")
        return
    loaded = h.json.get("loaded") or []
    res.notes.append(f"loaded {','.join(loaded) or 'nothing'} on {h.json.get('device', '?')}")
    body = {
        "state": {"text": "I was charged twice for my order, please refund me."},
        "questions": {
            "intent": {
                "type": "choice",
                "instructions": "What is this message about?",
                "criteria": {
                    "billing": "payment, a charge or a refund",
                    "shipping": "delivery of an order",
                    "other": "anything else",
                },
            }
        },
    }
    r = c.post("/v1/systemone", body)
    res.sanity["latency_s"] = round(r.elapsed, 3)
    _check_systemone(res, r, cfg["model"], "intent", "billing")


def _check_systemone(res, r, model, qid, want):
    if not r.ok:
        res.sanity.update(result="error", detail=r.why())
        res.fail("down" if r.status == 0 else "wrong", f"sanity request: {res.sanity['detail']}")
        return
    try:
        ans = r.json["answers"][qid]
        choice = ans["choice"]
        got_model = r.json.get("model", "")
        probs = ans.get("probabilities") or {}
    except (TypeError, KeyError):
        res.sanity.update(result="fail", detail="response has no answers/choice")
        res.fail("wrong", "sanity: response has no answers/choice")
        return
    res.model = got_model
    if choice == want:
        p = probs.get(want)
        extra = f" (p={p:.2f})" if isinstance(p, (int, float)) else ""
        res.sanity.update(result="pass", detail=f"chose {choice!r}{extra}")
    else:
        res.sanity.update(result="fail", detail=f"chose {choice!r}, expected {want!r}")
        res.fail("wrong", f"sanity: {res.sanity['detail']}")
    if got_model and got_model != model:
        res.fail("wrong", f"model is {got_model}, registry expects {model}")


def check_lfm2d(res, c, opts):
    cfg = res.cfg
    h = c.get(cfg.get("health_path", "/v1/models"))
    if not h.ok:
        res.fail("down", f"model list: {h.why()}")
        return
    if not isinstance(h.json, list):
        res.fail("wrong", "model list is not a JSON list")
        return
    ids = [m.get("id") for m in h.json if isinstance(m, dict)]
    if cfg["model"] not in ids:
        res.model = ids[0] if ids else ""
        res.fail("wrong", f"models are {', '.join(map(str, ids)) or 'none'}, registry expects {cfg['model']}")
        return
    res.model = cfg["model"]

    if cfg.get("read", "specs") == "specs":
        r = c.get("/v1/opinion/specs")
        good = r.ok and isinstance(r.json, list) and len(r.json) >= 1
        detail = f"{len(r.json)} spec(s) listed" if good else "expected at least one spec"
    else:
        r = c.post("/v1/spans/credentials", {"inputs": ["fleet-check ping"]})
        good = r.ok and isinstance(r.json, list)
        detail = "span scan answered" if good else "expected a list of span lists"
    res.sanity["latency_s"] = round(r.elapsed, 3)
    if good:
        res.sanity.update(result="pass", detail=detail)
    else:
        why = r.why() if not r.ok else detail
        res.sanity.update(result="error" if not r.ok else "fail", detail=why)
        res.fail("down" if r.status == 0 else "wrong", f"read: {why}")

    ex = cfg.get("exporter_url")
    if ex:
        e = c.get(ex.rstrip("/") + "/healthz")
        if e.ok and e.text.strip() == "ok":
            res.notes.append("exporter /healthz ok")
        else:
            res.fail("wrong", f"exporter {ex}: {e.why(60)}")


def check_jev_glm(res, c, opts):
    cfg = res.cfg
    h = c.get("/health")
    if not h.ok:
        res.fail("down", f"health: {h.why()}")
        return
    if not health_up(h.json):
        res.fail("down", f"health says not ok: {h.message(80)}")
        return
    res.model = h.json.get("model", "")
    rd = c.get("/ready")
    if not rd.ok:
        res.fail("wrong", f"/ready: the GLM behind it is not answering ({rd.why(70)})")
        return
    body = {
        "state": "ls -la",
        "model": cfg["model"],
        "questions": {
            "verdict": {
                "type": "choice",
                "instructions": "What kind of shell command is this?",
                "criteria": {
                    "read": "only looks at things",
                    "destroy": "deletes data that cannot be brought back",
                },
            }
        },
    }
    r = c.post("/v1/systemone", body, timeout=max(c.timeout, 90))
    res.sanity["latency_s"] = round(r.elapsed, 3)
    _check_systemone(res, r, cfg["model"], "verdict", "read")


CHECKS = {
    "tensorfold": check_tensorfold,
    "laya": check_laya,
    "lfm2d": check_lfm2d,
    "jev-glm": check_jev_glm,
}


def check_engine(name, cfg, opts):
    res = Result(name, cfg)
    c = Client(cfg["url"], opts.timeout)
    fn = CHECKS.get(cfg.get("kind"))
    if fn is None:
        res.fail("wrong", f"unknown kind {cfg.get('kind')!r} in registry")
    else:
        fn(res, c, opts)
    res.requests = c.count
    return res


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

def feature_cell(res, quick):
    if not res.features:
        if res.status == "skipped-busy":
            return "skipped (busy)"
        return "(--quick)" if quick and res.cfg.get("kind") == "tensorfold" else "n/a"
    if all(f["result"] == "skipped" for f in res.features.values()):
        return "skipped (busy)"
    parts = []
    for n in FEATURES:
        f = res.features.get(n)
        if f:
            parts.append(f"{SHORT[n]}={f['result']}{'!' if f['drift'] else ''}")
    return " ".join(parts)


def render_table(results, quick):
    headers = ["ENGINE", "STATUS", "MODEL", "LOAD", "SANITY", "LATENCY", "FEATURES"]
    rows = []
    for r in results:
        lat = "-" if r.sanity["latency_s"] is None else f"{r.sanity['latency_s']:.2f}s"
        model = (r.model or "-") + (f" v{r.version}" if r.version else "")
        status = "BUSY" if r.status == "skipped-busy" else r.verdict.upper()
        rows.append([r.name, status, model, r.load or "-", r.sanity["result"], lat,
                     feature_cell(r, quick)])
    widths = [max(len(str(x[i])) for x in [headers] + rows) for i in range(len(headers))]
    lines = ["  ".join(str(v).ljust(widths[i]) for i, v in enumerate(row)).rstrip()
             for row in [headers] + rows]
    detail = []
    for r in results:
        for p in r.problems:
            detail.append(f"{r.name}: PROBLEM {p}")
        for n in FEATURES:
            f = r.features.get(n)
            if f and f["result"] not in ("yes", "skipped"):
                tag = "DRIFT " if f["drift"] else ""
                exp = f" (expected {f['expected']})" if f["drift"] else ""
                detail.append(f"{r.name}: {tag}{n} = {f['result']}{exp}: {f['reason']}")
        for n in r.notes:
            detail.append(f"{r.name}: note: {n}")
    out = "\n".join(lines)
    if detail:
        out += "\n\n" + "\n".join(detail)
    return out


def exit_code(results):
    if any(r.verdict in ("down", "wrong", "slow") for r in results):
        return 1
    if any(r.verdict == "drift" for r in results):
        return 2
    return 0


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

class Parser(argparse.ArgumentParser):
    def error(self, message):
        # argparse exits 2 by default, which here would mean "feature drift".
        self.print_usage(sys.stderr)
        print(f"fleet-check: {message}", file=sys.stderr)
        sys.exit(1)


def main(argv=None, out=None):
    out = out or sys.stdout
    ap = Parser(
        prog="fleet-check",
        description="Check that every model engine in fleet.toml is up and behaving.",
        epilog="Exit 0 all good, 1 an engine is down or wrong, 2 only feature drift.",
    )
    ap.add_argument("engine", nargs="*", help="engine name(s) to check (default: all)")
    ap.add_argument("--json", action="store_true", help="print JSON instead of a table")
    ap.add_argument("--quick", action="store_true", help="skip the feature probes")
    ap.add_argument("--registry", default=os.environ.get("FLEET_REGISTRY", DEFAULT_REGISTRY),
                    help="path to fleet.toml")
    ap.add_argument("--timeout", type=float, default=60.0, help="seconds per request (default 60)")
    ap.add_argument("--bg-timeout", type=float, default=20.0,
                    help="seconds a background-priority sanity request may wait before it is "
                         "re-sent once at normal priority (default 20)")
    ap.add_argument("--force-shared", action="store_true",
                    help="run sanity and probes on a shared engine even if it is busy")
    opts = ap.parse_args(argv)

    try:
        with open(opts.registry, "rb") as f:
            reg = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError) as e:
        print(f"fleet-check: cannot read registry {opts.registry}: {e}", file=sys.stderr)
        return 1
    engines = reg.get("engines", {})
    local = load_local_addresses()
    if local:
        for cfg in engines.values():
            for key in ("url", "exporter_url"):
                if isinstance(cfg.get(key), str):
                    cfg[key] = apply_local_addresses(cfg[key], local)
    names = opts.engine or sorted(engines)
    unknown = [n for n in names if n not in engines]
    if unknown:
        print(f"fleet-check: unknown engine {', '.join(unknown)}; known: {', '.join(sorted(engines))}",
              file=sys.stderr)
        return 1

    results = [check_engine(n, engines[n], opts) for n in names]
    code = exit_code(results)
    if opts.json:
        doc = {"exit_code": code, "engines": [r.to_json() for r in results]}
        print(json.dumps(doc, indent=2, sort_keys=True), file=out)
    else:
        print(render_table(results, opts.quick), file=out)
        summary = {0: "ALL GOOD", 1: "PROBLEM: an engine is down or answered wrong",
                   2: "DRIFT: engines work, but a feature differs from the registry"}[code]
        print(f"\n{summary} (exit {code})", file=out)
    return code


if __name__ == "__main__":
    sys.exit(main())
