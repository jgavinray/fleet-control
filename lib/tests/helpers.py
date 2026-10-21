"""Test helpers: fake secrets built at run time, and a stub model server.

Secret-shaped test values are assembled from pieces here so that no
token-shaped literal is ever committed (the runner's push scan would stop it,
and a real-looking string in a repo is a bad habit anyway). Every value is
made of a repeating pattern and is not a credential for anything.
"""
from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def fake(prefix: str, n: int, alphabet: str = "Ab1Cd2Ef3Gh4Ij5Kl6") -> str:
    """prefix + n characters of a repeating pattern."""
    body = (alphabet * (n // len(alphabet) + 1))[:n]
    return prefix + body


class Stub:
    """A local HTTP server that plays a model engine.

    health    callable(call_number) -> dict, the /health JSON (call_number counts from 1), or (status, text)
    chat      callable(body, call_number) -> reply text, or (status, text) to fail
    systemone callable(body) -> dict
    delay     seconds each POST takes (lets concurrency show)
    """

    def __init__(self, health=None, chat=None, systemone=None, delay: float = 0.0):
        self.health_fn = health or (lambda n: {"status": "ok"})
        self.chat_fn = chat
        self.systemone_fn = systemone
        self.delay = delay
        self.log: list = []  # ("health",) / ("chat", body) / ("systemone", body), in arrival order
        self.lock = threading.Lock()
        self.inflight = 0
        self.max_inflight = 0
        self.n_health = 0
        self.n_chat = 0
        stub = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):  # silence
                pass

            def _send(self, status, obj):
                data = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                with stub.lock:
                    stub.n_health += 1
                    n = stub.n_health
                    stub.log.append(("health",))
                if self.path == "/health":
                    out = stub.health_fn(n)
                    if isinstance(out, tuple):
                        self._send(out[0], {"error": out[1]})
                    else:
                        self._send(200, out)
                else:
                    self._send(404, {})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
                with stub.lock:
                    stub.inflight += 1
                    stub.max_inflight = max(stub.max_inflight, stub.inflight)
                try:
                    if stub.delay:
                        time.sleep(stub.delay)
                    if self.path == "/v1/chat/completions" and stub.chat_fn:
                        with stub.lock:
                            stub.n_chat += 1
                            n = stub.n_chat
                            stub.log.append(("chat", body))
                        out = stub.chat_fn(body, n)
                        if isinstance(out, tuple):
                            self._send(out[0], {"error": out[1]})
                        else:
                            self._send(200, {"choices": [{"message": {"content": out}}],
                                             "usage": {"prompt_tokens": 10, "completion_tokens": 1}})
                    elif self.path == "/v1/systemone" and stub.systemone_fn:
                        with stub.lock:
                            stub.log.append(("systemone", body))
                        self._send(200, stub.systemone_fn(body))
                    else:
                        self._send(404, {})
                finally:
                    with stub.lock:
                        stub.inflight -= 1

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.01), daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()
        self.server.server_close()

    def chat_bodies(self):
        return [e[1] for e in self.log if e[0] == "chat"]
