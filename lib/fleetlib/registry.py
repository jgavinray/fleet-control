"""Where the model engines live.

Two sources, in this order:

1. the registry file ``fleet.toml`` (found next to this library in the
   fleet-control checkout, or at ``$FLEET_TOML``, or at
   ``~/dev/fleet-control/fleet.toml``), if it exists and lists the engine;
2. the built-in defaults below (Tailscale names; the repo is public, so no IP
   address is ever written here).

Whichever source an engine comes from, its URL is then passed through the
optional local LAN override, ``~/.config/fleet-control/local.toml`` (or the
file named by ``$FLEET_LOCAL``), which maps a host to its LAN address::

    [hosts.hyper01]
    address = "192.0.2.10"

A URL whose host is ``hyper01`` or ``hyper01.<tailnet>.ts.net`` is rewritten
to that address. The same rule is in ``check/fleet_check.py``.

A registry entry only needs to say what differs from the default, so an entry
with just ``url`` for ``glm`` keeps the default model, method and limits.
The file is read with the standard ``tomllib``.

What the library reads from an ``[engines.NAME]`` table (the real fleet.toml
schema, written by fleet-check's author):

    url, model        where the engine is and the model id it serves
    kind              tensorfold | laya | jev-glm | lfm2d
    shared            true: a shared box, so back off while anyone else is connected
    expected_features.guided_choice   "yes" or "no"

and from those it derives how to ask:

    kind laya or jev-glm                    -> method systemone
    kind tensorfold, guided_choice = "yes"  -> method guided_choice
    kind tensorfold, guided_choice = "no"   -> method parse
    kind lfm2d (opinion engines)            -> not usable here, ignored

Optional keys, for tuning, are also read: ``method`` (guided_choice | parse |
systemone, wins over the derivation), ``k``, ``max_inflight``, ``share_check``,
``temperature``, ``max_tokens``, ``timeout``, ``priority``, ``thinking_off``,
``seed_param``. Other keys are ignored. A trailing ``/v1`` on the url is
dropped. The tables may also be named ``[engine.NAME]`` or be a ``[[engines]]``
list with a ``name`` key; ``host`` + ``port`` can stand in for ``url``.
"""
from __future__ import annotations

import os
import sys
import tomllib
import urllib.parse
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Optional

METHODS = ("guided_choice", "parse", "systemone")


@dataclass(frozen=True)
class Engine:
    name: str
    url: str
    method: str  # guided_choice | parse | systemone
    model: str = ""
    k: int = 1  # samples per classification (ignored for systemone: the service samples)
    max_inflight: int = 1  # requests in flight at once, process-wide, for this engine
    share_check: bool = False  # shared box: back off while /health shows other connections
    temperature: Optional[float] = None
    max_tokens: int = 16
    timeout: float = 120.0
    priority: Optional[str] = None  # sent as "priority" (GLM scheduling class)
    thinking_off: bool = True  # send chat_template_kwargs.enable_thinking=false
    seed_param: bool = True  # engine accepts "seed"

    def __post_init__(self) -> None:
        if self.method not in METHODS:
            raise ValueError(f"engine {self.name!r}: method must be one of {METHODS}, not {self.method!r}")


DEFAULTS: "dict[str, Engine]" = {
    # GLM on spark3: constrained decoding, so sample K times and count.
    "glm": Engine("glm", "http://spark3.puma-wyvern.ts.net:8888", "guided_choice", "GLM-5.3-Flash-EXL3",
                  k=5, max_inflight=2, temperature=1.0, priority="background"),
    # DeepSeek rejects guided_choice: ask for one word and parse it.
    "deepseek": Engine("deepseek", "http://spark.puma-wyvern.ts.net:8888", "parse", "DeepSeek-V4.1-Flash-1017",
                       k=1, max_inflight=1, temperature=0.0, seed_param=False),
    # Qwen is shared: one request at a time, and back off when anyone else is on it.
    "qwen": Engine("qwen", "http://hyper03.puma-wyvern.ts.net:8081", "parse", "Qwen3.8-27B-MLX-4bit",
                   k=1, max_inflight=1, share_check=True, temperature=0.0, seed_param=False),
    # laya (hyper01, :8001): Jev-style /v1/systemone, probabilities come back from the service.
    "laya": Engine("laya", "http://hyper01.puma-wyvern.ts.net:8001", "systemone", "laya-rl-agent", max_inflight=1),
    # jev-glm (hyper01): the GLM-backed service with the same /v1/systemone wire format.
    "jev-glm": Engine("jev-glm", "http://hyper01.puma-wyvern.ts.net:8102", "systemone", "jev-glm-0.1", max_inflight=2),
}

_TABLE_NAMES = ("engines", "engine", "servers", "server", "models", "model", "services", "service")
_URL_KEYS = ("url", "base_url", "base", "endpoint", "api_base")


def default_path() -> Optional[Path]:
    """The registry file to use, or None if there is none."""
    env = os.environ.get("FLEET_TOML")
    if env:
        return Path(env)
    for p in (Path(__file__).resolve().parents[2] / "fleet.toml",  # lib/fleetlib/ -> fleet.toml
              Path.home() / "dev" / "fleet-control" / "fleet.toml"):
        if p.is_file():
            return p
    return None


def local_path() -> Path:
    env = os.environ.get("FLEET_LOCAL")
    return Path(env) if env else Path.home() / ".config" / "fleet-control" / "local.toml"


def local_addresses(path: "Optional[str | os.PathLike[str]]" = None) -> "dict[str, str]":
    """{host name: LAN address} from the local override file; {} if there is none."""
    p = Path(path) if path else local_path()
    try:
        with open(p, "rb") as fh:
            doc = tomllib.load(fh)
    except OSError:
        return {}
    except tomllib.TOMLDecodeError as e:
        print(f"fleetlib: ignoring unreadable local override {p}: {e}", file=sys.stderr)
        return {}
    hosts = doc.get("hosts")
    return {n: h["address"] for n, h in (hosts.items() if isinstance(hosts, dict) else ())
            if isinstance(h, dict) and isinstance(h.get("address"), str) and h["address"]}


def apply_local_address(url: str, addresses: "dict[str, str]") -> str:
    """Swap the host part of ``url`` for its local address, if one is set."""
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


def _localize(engines: "dict[str, Engine]") -> "dict[str, Engine]":
    addrs = local_addresses()
    if not addrs:
        return engines
    return {n: replace(e, url=apply_local_address(e.url, addrs)) for n, e in engines.items()}


def _clean_url(u: str) -> str:
    u = u.strip().rstrip("/")
    if u.endswith("/v1"):
        u = u[:-3]
    return u


def _entries(doc: dict) -> "list[tuple[str, dict]]":
    out: list[tuple[str, dict]] = []
    for tn in _TABLE_NAMES:
        t = doc.get(tn)
        if isinstance(t, dict):
            for name, v in t.items():
                if isinstance(v, dict):
                    out.append((name, v))
        elif isinstance(t, list):
            for v in t:
                if isinstance(v, dict) and isinstance(v.get("name"), str):
                    out.append((v["name"], v))
    if not out:  # engines as top-level tables
        for name, v in doc.items():
            if isinstance(v, dict) and name in DEFAULTS:
                out.append((name, v))
    return out


def _derive_method(v: dict) -> Optional[str]:
    """The ask-method a registry entry implies (see the module docstring)."""
    if isinstance(v.get("method"), str):
        return v["method"]
    kind = v.get("kind")
    if kind in ("laya", "jev-glm"):
        return "systemone"
    if kind == "tensorfold":
        feats = v.get("expected_features") if isinstance(v.get("expected_features"), dict) else {}
        g = feats.get("guided_choice")
        if g == "yes":
            return "guided_choice"
        if g == "no":
            return "parse"
    return None


def _apply(base: Optional[Engine], name: str, v: dict) -> Optional[Engine]:
    if v.get("kind") == "lfm2d":
        return base  # opinion engines are not asked a classification question this way
    url = next((v[k] for k in _URL_KEYS if isinstance(v.get(k), str)), None)
    if url is None and isinstance(v.get("host"), str) and v.get("port"):
        url = f"http://{v['host']}:{v['port']}"
    fields = {}
    if url:
        fields["url"] = _clean_url(url)
    method = _derive_method(v)
    if method:
        fields["method"] = method
    if "share_check" not in v and isinstance(v.get("shared"), bool):
        fields["share_check"] = v["shared"]
    for key, typ in (("model", str), ("k", int), ("max_inflight", int), ("share_check", bool),
                     ("temperature", (int, float)), ("max_tokens", int), ("timeout", (int, float)),
                     ("priority", str), ("thinking_off", bool), ("seed_param", bool)):
        if key in v and isinstance(v[key], typ) and not (typ is int and isinstance(v[key], bool)):
            fields[key] = v[key]
    if base is not None:
        return replace(base, **fields) if fields else base
    if "url" not in fields or "method" not in fields:
        return None  # unknown engine with no url or no way to ask it: cannot be used
    return Engine(name, **fields)


def load_registry(path: "Optional[str | os.PathLike[str]]" = None) -> "dict[str, Engine]":
    """Defaults, overridden by the registry file when there is one. Pass
    ``path`` to read a specific file; a missing explicit path is an error."""
    engines = dict(DEFAULTS)
    p = Path(path) if path else default_path()
    if p is None:
        return _localize(engines)
    if path and not p.is_file():
        raise FileNotFoundError(f"fleet registry not found: {p}")
    if not p.is_file():
        return _localize(engines)
    with open(p, "rb") as fh:
        doc = tomllib.load(fh)
    for name, v in _entries(doc):
        e = _apply(engines.get(name), name, v)
        if e is not None:
            engines[name] = e
    return _localize(engines)


def get_engine(engine: "str | Engine", registry: "Optional[dict[str, Engine]]" = None) -> Engine:
    """Resolve an engine name (or pass an Engine through)."""
    if isinstance(engine, Engine):
        return engine
    reg = registry if registry is not None else load_registry()
    try:
        return reg[engine]
    except KeyError:
        raise KeyError(f"unknown engine {engine!r}; known: {sorted(reg)}") from None
