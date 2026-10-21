"""One-word classification calls to the local models.

    from fleetlib.classify import classify
    r = classify("git push --force origin main", ["read", "edit", "push", "destroy"], "glm", k=5)
    r["destroy"]      # probability, a plain float; r is a dict {option: probability}
    r.meta            # counts, invalid, errors, backoffs, seeds, seconds ...

How an engine is asked depends on its ``method`` (see registry.py):

guided_choice (GLM)
    ``k`` requests, each with ``guided_choice`` = the options, thinking off,
    ``temperature`` 1, its own ``seed`` and its own shuffled option order,
    ``priority: "background"``. Probability = count / valid samples.
    GLM gives identical text at temperature 1 unless the seed differs, hence
    the distinct seeds; the shuffle cancels position bias.
parse (DeepSeek, Qwen)
    These reject guided_choice. Ask for one word, parse the reply; a reply that
    is not one of the options counts in ``meta["invalid"]`` and not in the
    probabilities. ``k`` defaults to 1 (temperature 0). With k > 1 the
    temperature is 1 and each sample gets a seed and a shuffled order.
systemone (laya, jev-glm)
    One POST to ``/v1/systemone``; the service returns the probabilities
    (jev-glm does its own sampling, laya reads them off the model).

Limits and back-off, for every engine:
  * at most ``max_inflight`` requests at once per engine, across threads
    (GLM 2, DeepSeek 1, Qwen 1, laya 1, jev-glm 2; override in the registry
    or with ``max_inflight=``);
  * before each request ``GET /health``; if it shows requests_waiting > 0
    (or ``live.waiting`` > 0) sleep ``backoff_s`` (30 s) and ask again;
  * a ``share_check`` engine (Qwen, which other people use) also backs off
    while ``live.connections`` > 0;
  * after ``max_wait_s`` (900 s) of backing off, ``EngineBusy`` is raised.

Text is masked with fleetlib.redact before it is sent (``redact_text=True``)
and cut to ``max_chars`` (4000, head kept).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import random
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from typing import Callable, Optional, Sequence

from .redact import redact
from .registry import Engine, get_engine

__all__ = ["classify", "Classification", "EngineBusy", "EngineError", "parse_option"]


class EngineError(Exception):
    """The engine could not be reached, or answered nothing usable."""


class EngineBusy(EngineError):
    """The engine stayed busy for longer than ``max_wait_s``."""


class Classification(dict):
    """``{option: probability}`` in the order the options were given, plus ``.meta``."""

    meta: dict

    def __init__(self, probs, meta):
        super().__init__(probs)
        self.meta = meta

    @property
    def choice(self) -> Optional[str]:
        return self.meta.get("choice")


# ------------------------------------------------------------------ http


def _http(url: str, body: Optional[dict], timeout: float) -> dict:
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, headers={"content-type": "application/json"},
                                 method="POST" if body is not None else "GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        detail = e.read()[:200].decode("utf-8", errors="replace")
        e.close()
        raise EngineError(f"http {e.code} from {url}: {detail}") from None
    except (urllib.error.URLError, OSError, ValueError) as e:
        raise EngineError(f"{type(e).__name__} from {url}: {e}") from None


# ------------------------------------------------------------------ limits and back-off

_limits_lock = threading.Lock()
_semaphores: "dict[tuple, threading.BoundedSemaphore]" = {}


def _semaphore(e: Engine, n: int) -> threading.BoundedSemaphore:
    key = (e.name, e.url, n)
    with _limits_lock:
        s = _semaphores.get(key)
        if s is None:
            s = _semaphores[key] = threading.BoundedSemaphore(n)
        return s


def waiting_count(health: dict, share_check: bool) -> int:
    """How many requests are queued (or, on a shared engine, connected) according to /health."""
    live = health.get("live") if isinstance(health.get("live"), dict) else {}
    n = int(health.get("requests_waiting") or 0) + int(live.get("waiting") or 0)
    if share_check:
        n += int(live.get("connections") or 0)
    return n


class _Run:
    """Counters for one classify() call, shared by its worker threads."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.backoffs = 0
        self.health_checks = 0
        self.health_errors = 0
        self.waited_s = 0.0
        self.last_health = 0.0
        self.tokens_in = 0
        self.tokens_out = 0


def _wait_until_idle(e: Engine, run: _Run, backoff_s: float, max_wait_s: float, health_ttl: float, sleep: Callable) -> None:
    started = time.monotonic()
    slept = 0.0
    while True:
        with run.lock:
            fresh = (not e.share_check) and (time.monotonic() - run.last_health) < health_ttl
        if fresh:
            return
        try:
            h = _http(e.url + "/health", None, 10)
            waiting = waiting_count(h, e.share_check)
            with run.lock:
                run.health_checks += 1
        except EngineError:
            waiting = 0  # an unreachable /health is not a reason to wait; the request will report the failure
            with run.lock:
                run.health_errors += 1
        with run.lock:
            run.last_health = time.monotonic()
        if waiting <= 0:
            return
        waited = max(slept, time.monotonic() - started)
        if waited + backoff_s > max_wait_s:
            raise EngineBusy(f"{e.name} still busy ({waiting} waiting/connected) after {waited:.0f}s")
        with run.lock:
            run.backoffs += 1
            run.waited_s += backoff_s
            run.last_health = 0.0
        sleep(backoff_s)
        slept += backoff_s


# ------------------------------------------------------------------ prompts and parsing


def _norm(s: str) -> str:
    return s.strip().lower().strip("`'\".,:;*()[]<>! \n\t")


def parse_option(raw: Optional[str], options: Sequence[str]) -> Optional[str]:
    """The option a reply names, or None. Exact match first (ignoring case,
    quotes and punctuation), then a reply that starts with an option followed
    by a non-word character ("push - because ...")."""
    if not raw:
        return None
    t = _norm(raw)
    by_norm = {_norm(o): o for o in options}
    if t in by_norm:
        return by_norm[t]
    low = raw.strip().lstrip("`'\"*([<").lower()
    for n, o in sorted(by_norm.items(), key=lambda kv: -len(kv[0])):
        if low.startswith(n) and (len(low) == len(n) or not (low[len(n)].isalnum() or low[len(n)] == "_")):
            return o
    return None


def _prompt(instructions: str, order: Sequence[str], descs: dict, text: str, guided: bool) -> str:
    lines = "\n".join(f"- {o}: {descs[o]}" if descs.get(o) else f"- {o}" for o in order)
    if guided:
        tail = "Answer with exactly one of: " + ", ".join(order) + "."
    else:
        tail = "Reply with exactly one option word from this list and nothing else: " + ", ".join(order) + "."
    return f"{instructions}\n\nOptions:\n{lines}\n\nText:\n```\n{text}\n```\n\n{tail}"


def _base_seed(e: Engine, text: str, options: Sequence[str], instructions: str) -> int:
    h = hashlib.sha256("\x1f".join([e.name, instructions, *options, text]).encode()).hexdigest()
    return int(h[:8], 16) % (2**31 - 1)


def _sample_seed(base: int, i: int) -> int:
    return (base + i) % (2**31 - 1)


def _shuffled(options: Sequence[str], seed: int) -> "list[str]":
    o = list(options)
    random.Random(seed ^ 0x5BD1E995).shuffle(o)
    return o


# ------------------------------------------------------------------ the call


def classify(
    text: str,
    options: "Sequence[str] | dict[str, str]",
    engine: "str | Engine",
    k: Optional[int] = None,
    *,
    instructions: str = "Which option best describes the text?",
    registry: "Optional[dict[str, Engine]]" = None,
    seed: Optional[int] = None,
    smoothing: float = 0.0,
    temperature: Optional[float] = None,
    redact_text: bool = True,
    max_chars: Optional[int] = 4000,
    max_inflight: Optional[int] = None,
    backoff_s: float = 30.0,
    max_wait_s: float = 900.0,
    health_ttl: float = 1.0,
    sleep: Callable[[float], None] = time.sleep,
) -> Classification:
    """Classify ``text`` into one of ``options`` with a local engine.

    options      a list of option names, or {name: description} (descriptions go in the prompt)
    engine       a name known to the registry ("glm", "deepseek", "qwen", "laya", "jev-glm") or an Engine
    k            samples (guided_choice and parse engines); default is the engine's own
    seed         base seed; default is derived from the request, so a repeat gives the same seeds
    smoothing    added to each option's count before normalising (0 = plain frequencies)

    Returns a dict {option: probability} with ``.meta``. Raises EngineBusy or
    EngineError (every sample failed). Samples that fail while others succeed
    are counted in ``meta["errors"]``.
    """
    e = get_engine(engine, registry)
    if isinstance(options, dict):
        names, descs = list(options), dict(options)
    else:
        names, descs = list(options), {}
    if len(names) < 2 or len(set(names)) != len(names):
        raise ValueError("options must be at least two distinct names")
    t0 = time.monotonic()
    masked_count = 0
    if redact_text:
        text, masked_count = redact(text)
    text_chars = len(text)
    cut = max_chars is not None and len(text) > max_chars
    if cut:
        text = text[:max_chars]
    run = _Run()
    gate = dict(backoff_s=backoff_s, max_wait_s=max_wait_s, health_ttl=health_ttl, sleep=sleep)
    sem = _semaphore(e, max_inflight or e.max_inflight)

    meta: dict = {"engine": e.name, "model": e.model, "method": e.method, "options": names,
                  "text_chars": text_chars, "truncated": cut, "redactions": masked_count}

    if e.method == "systemone":
        probs, extra = _systemone(e, text, names, descs, instructions, sem, run, gate)
        meta.update(extra, k=1)
        counts = None
    else:
        guided = e.method == "guided_choice"
        n = k or e.k
        if n < 1:
            raise ValueError("k must be at least 1")
        base = seed if seed is not None else _base_seed(e, text, names, instructions)
        temp = temperature if temperature is not None else (e.temperature if e.temperature is not None else 1.0)
        if not guided and n > 1 and temperature is None and not temp:
            temp = 1.0  # several samples at temperature 0 would be one sample repeated
        seeds = [_sample_seed(base, i) for i in range(n)]
        orders = [_shuffled(names, s) for s in seeds]
        results: "list[tuple[Optional[str], Optional[str], str]]" = [(None, None, "")] * n

        def one(i: int) -> None:
            body = {
                "model": e.model,
                "messages": [{"role": "user", "content": _prompt(instructions, orders[i], descs, text, guided)}],
                "temperature": temp,
                "max_tokens": e.max_tokens,
            }
            if e.seed_param:
                body["seed"] = seeds[i]
            if e.priority:
                body["priority"] = e.priority
            if e.thinking_off:
                body["chat_template_kwargs"] = {"enable_thinking": False}
            if guided:
                body["guided_choice"] = list(orders[i])
            with sem:
                try:
                    _wait_until_idle(e, run, **gate)
                    r = _http(e.url + "/v1/chat/completions", body, e.timeout)
                    raw = r["choices"][0]["message"]["content"]
                    u = r.get("usage") or {}
                    with run.lock:
                        run.tokens_in += int(u.get("prompt_tokens") or 0)
                        run.tokens_out += int(u.get("completion_tokens") or 0)
                    results[i] = (parse_option(raw, names), None, (raw or "")[:60])
                except EngineBusy:
                    raise
                except (EngineError, KeyError, IndexError, TypeError) as ex:
                    results[i] = (None, f"{type(ex).__name__}: {ex}"[:200], "")

        workers = max(1, min(n, max_inflight or e.max_inflight))
        if workers == 1:
            for i in range(n):
                one(i)
        else:
            with ThreadPoolExecutor(max_workers=workers) as pool:
                for f in [pool.submit(one, i) for i in range(n)]:
                    f.result()
        counts = {o: 0 for o in names}
        invalid = 0
        errors = [err for _, err, _ in results if err]
        for lab, err, raw in results:
            if err:
                continue
            if lab is None:
                invalid += 1
            else:
                counts[lab] += 1
        valid = sum(counts.values())
        if len(errors) == n:
            raise EngineError(f"{e.name}: all {n} requests failed; first: {errors[0]}")
        denom = valid + smoothing * len(names)
        probs = {o: ((counts[o] + smoothing) / denom if denom > 0 else 0.0) for o in names}
        meta.update(k=n, counts=counts, valid=valid, invalid=invalid, errors=len(errors), error_samples=errors[:3],
                    seeds=seeds, orders=orders, replies=[r[2] for r in results], temperature=temp,
                    tokens_in=run.tokens_in, tokens_out=run.tokens_out)

    best = max(names, key=lambda o: (probs[o], -names.index(o))) if any(probs.values()) else None
    meta.update(choice=best, backoffs=run.backoffs, waited_s=run.waited_s, health_checks=run.health_checks,
                health_errors=run.health_errors, seconds=round(time.monotonic() - t0, 3))
    return Classification({o: probs[o] for o in names}, meta)


def _systemone(e: Engine, text: str, names: "list[str]", descs: dict, instructions: str, sem, run: _Run, gate: dict):
    question = {"type": "choice", "instructions": instructions, "criteria": {o: descs.get(o) or o for o in names}}
    body = {"state": text, "model": e.model or "fleetlib", "questions": {"verdict": question}}
    with sem:
        _wait_until_idle(e, run, **gate)
        r = _http(e.url + "/v1/systemone", body, e.timeout)
    try:
        ans = r["answers"]["verdict"]
        p = ans["probabilities"]
        probs = {o: float(p[o]) for o in names}
    except (KeyError, TypeError, ValueError) as ex:
        raise EngineError(f"{e.name}: unexpected /v1/systemone reply ({type(ex).__name__}: {ex})") from None
    usage = r.get("usage") or {}
    extra = {"confidence": ans.get("confidence"), "service_model": r.get("model"),
             "tokens_in": int(usage.get("input_tokens") or 0), "tokens_out": int(usage.get("output_tokens") or 0)}
    return probs, extra


# ------------------------------------------------------------------ CLI


def main(argv: "Optional[list[str]]" = None) -> int:
    ap = argparse.ArgumentParser(prog="python -m fleetlib.classify",
                                 description="Classify the text on stdin (or TEXT) into one option. Prints JSON.")
    ap.add_argument("--engine", required=True, help="glm, deepseek, qwen, laya, jev-glm, or a name in fleet.toml")
    ap.add_argument("--options", required=True, help="comma-separated option names")
    ap.add_argument("-k", type=int, default=None, help="samples (default: the engine's)")
    ap.add_argument("--instructions", default="Which option best describes the text?")
    ap.add_argument("--registry", default=None, help="path to a fleet.toml")
    ap.add_argument("text", nargs="?", help="text to classify (default: read stdin)")
    a = ap.parse_args(argv)
    text = a.text if a.text is not None else sys.stdin.read()
    reg = None
    if a.registry:
        from .registry import load_registry
        reg = load_registry(a.registry)
    try:
        r = classify(text, [o.strip() for o in a.options.split(",") if o.strip()], a.engine, a.k,
                     instructions=a.instructions, registry=reg)
    except EngineError as ex:
        print(json.dumps({"error": str(ex)}), file=sys.stderr)
        return 1
    print(json.dumps({"probabilities": dict(r), "meta": r.meta}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
