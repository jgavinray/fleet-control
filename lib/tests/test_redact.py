"""redact: one positive and one negative case per rule, forms, and no false positives.

POSITIVE[rule id] = (text, expected marker type, secret substring that must be gone).
Each positive is built so that ONLY its own rule can produce the expected
marker; tests/mutation_check.py removes each rule in turn and requires this
suite to fail, which proves the cases bite.
"""
import json
import os
import subprocess
import sys
import unittest

from fleetlib import redact as R
from fleetlib.redact import leftovers, redact, redact_counts, redact_obj

from .helpers import fake

LIB = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

_gh = fake("ghp_", 36)
_pat = fake("github_pat_", 60)
_hf = fake("hf_", 34)
_sk = fake("sk-", 48)
_aws = "AKIA" + "IOSFODNN7EXAMPLE"
_slack = "xoxb-" + "1234567890-" + "abcdefABCDEF"
_google = fake("AIza", 35)
_jwt = "eyJ" + "hbGciOiJIUzI1NiJ9" + "." + "eyJ" + "zdWIiOiIxMjM0NTY3ODkwIn0" + "." + "SflKxwRJSMeKKF2QT4fwpMeJf36"
_pem_body = "TUlJRXZRSUJBREFOQmdrcWhraUc5dzBCQVFFRkFBU0NCS2N3Z2dTakFnRUFBb0lCQVFD"
_BEGIN, _END = "-----BEGIN " + "PRIVATE KEY-----", "-----END " + "PRIVATE KEY-----"  # built here so no key header sits in the file
_pem = _BEGIN + "\n" + _pem_body + "\n" + _END
_hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
_b64 = "dGhpcyBpcyBhIGxvbmcgYmFzZTY0IGJsb2Igb2YgcmFuZG9tIGJ5dGVz1234"
_b64url = "Ab12Cd34Ef56Gh78Ij90-Kl12Mn34Op56Qr78St90_Uv12Wx34"
_mixed = "aB3dE5gH7jK9mN1pQ3sT5vW"

POSITIVE = {
    "pem": (f"key:\n{_pem}\nend", "pem", _pem_body),
    "url-credential": ("git clone https://bob:" + "s3cretpw" + "@git.example.com/x.git", "url-password", "s3cretpw"),
    "github-token": (f"clone with {_gh} now", "github-token", _gh),
    "hf-token": (f"huggingface-cli login --token {_hf}", "hf-token", _hf),
    "sk-key": (f"key is {_sk} ok", "api-key", _sk),
    "aws-key": (f"id {_aws} here", "aws-key", _aws),
    "slack-token": (f"slack {_slack} sent", "slack-token", _slack),
    "google-key": (f"maps {_google} used", "google-key", _google),
    "jwt": (f"session {_jwt} set", "jwt", _jwt),
    "auth-header": ('curl -H "Authorization: Basic dXNlcjpwYXNzd29yZA==" https://x', "auth", "dXNlcjpwYXNzd29yZA"),
    "bearer": ("curl -H 'Authorization: Bearer abc123def456ghi789' https://x", "bearer", "abc123def456ghi789"),
    "key-value": ("export DB_PASSWORD=hunter2 and go", "secret", "hunter2"),
    "secret-flag": ("mysql -u root --password hunter2 db", "secret", "hunter2"),
    "email": ("mail alice.smith@example.com today", "email", "alice.smith@example.com"),
    "hex-run": (f"sha256 {_hex}", "hex", _hex),
    "base64-run": (f"blob {_b64} end", "base64", _b64),
    "base64url-run": (f"token {_b64url} end", "base64", _b64url),
    "mixed-token": (f"id {_mixed} used", "token", _mixed),
}

# Text each rule must leave alone.
NEGATIVE = {
    "pem": "Use five dashes ----- as a divider, and BEGIN and END as words.",
    "url-credential": "see https://example.com/a/b and postgres://user:" + "${DB_PASS}" + "@db/x",
    "github-token": "ghp_ is the prefix GitHub uses; ghp_abc is too short to be one",
    "hf-token": "tokenizer = hf_tokenizer.from_pretrained(name)",
    "sk-key": "the task-runner and disk-usage and risk-assessment tools",
    "aws-key": "AKIA is how AWS key ids start; AKIAB is not one",
    "slack-token": "xoxo hugs and kisses, xox",
    "google-key": "AIza is the prefix of Google API keys",
    "jwt": "eyJ is how base64 JSON usually starts",
    "auth-header": "Authorization: Basic ${CREDS}",
    "bearer": "the bearer of good news; Authorization: Bearer $TOKEN",
    "key-value": "max_tokens: 16, tokens_in = 0, def login(password: str), and if password == 'x'",
    "secret-flag": "docker login --password-stdin < file; run --max-tokens 100",
    "email": "git clone git@github.com:owner/repo.git; send me an email",
    "hex-run": "md5 d41d8cd98f00b204e9800998ecf8427e is 32 characters",
    "base64-run": "see /Users/someone/projects/a-long-name/src/module/file.py and averyveryverylongplainwordwithoutdigits",
    "base64url-run": "this-is-a-very-long-kebab-case-name-without-any-digits-at-all",
    "mixed-token": "Qwen38ForCausalLM is a class and averyveryverylongplainword",
}

PROSE = [
    "The quick brown fox jumps over the lazy dog. Tokens are counted per request, and a secret garden is nice.",
    "Please review the password policy document and tell me whether the token refresh flow is documented.",
    "We discussed the bearer bonds, the key-value store, and how an API key rotation schedule should work.",
    "Meeting at 3pm on 10/20/2026 with the infra team about the GLM-5.3-Flash-EXL3 rollout and DeepSeek-V4.1.",
]
COMMANDS = [
    "ls -la /Users/jgavinray/dev/mission-control/fleet/lib",
    "git log --oneline -n 20 && git status --short",
    "git push -u origin fleetlib",
    "git clone git@github.com:jgavinray/mission-control.git ~/dev/mc",
    "curl -s -H \"Authorization: Bearer $TOKEN\" http://192.168.0.44:8102/health",
    "export GITHUB_TOKEN=$(gh auth token) && gh pr list",
    "python3 -m unittest discover -s tests -t . -v 2>&1 | tail -20",
    "ssh jgavinray@192.168.0.44 'systemctl --user status jev-glm.service'",
    "curl -d '{\"model\": \"x\", \"max_tokens\": 16, \"temperature\": 1}' http://localhost:8888/v1/chat/completions",
    "docker login --password-stdin < ~/.docker/pw && docker compose up -d",
    "cargo test --manifest-path runner/Cargo.toml -- --nocapture",
    "grep -rn 'def redact' fleet/lib | head -5; sed -n '1,40p' README.md",
]


class RuleCases(unittest.TestCase):
    """Positive and negative case for every rule (test methods are generated below)."""


def _add_cases():
    assert set(POSITIVE) == {r.id for r in R.RULES} == set(NEGATIVE), "every rule needs a positive and a negative case"
    for rid, (text, kind, secret) in POSITIVE.items():
        def pos(self, text=text, kind=kind, secret=secret, rid=rid):
            out, n = redact(text)
            self.assertNotIn(secret, out)
            self.assertIn(f"[REDACTED:{kind}]", out)
            self.assertGreaterEqual(n, 1)
            self.assertEqual(leftovers(out), [], "masked output must have no leftovers")
            self.assertEqual(redact(out), (out, 0), "masking must be idempotent")
        setattr(RuleCases, "test_positive_" + rid.replace("-", "_"), pos)
    for rid, text in NEGATIVE.items():
        def neg(self, text=text):
            self.assertEqual(redact(text), (text, 0))
        setattr(RuleCases, "test_negative_" + rid.replace("-", "_"), neg)


_add_cases()


class Forms(unittest.TestCase):
    def test_json_form(self):
        out, n = redact('{"user": "bob", "password": "hunter2", "api_key": "k-1234", "token": "abc9"}')
        self.assertEqual(n, 3)
        self.assertEqual(json.loads(out), {"user": "bob", "password": "[REDACTED:secret]",
                                           "api_key": "[REDACTED:secret]", "token": "[REDACTED:secret]"})

    def test_json_escaped_inside_a_log_line(self):
        out, n = redact('{"cmd": "curl -d {\\"password\\": \\"hunter2\\"}"}')
        self.assertNotIn("hunter2", out)
        self.assertEqual(n, 1)

    def test_yaml_form(self):
        text = "db:\n  host: localhost\n  password: hunter2\n  api_key: 'abc123'\n  secret: \"s3\"\nname: x\n"
        out, n = redact(text)
        self.assertEqual(n, 3)
        for s in ("hunter2", "abc123", "s3\""):
            self.assertNotIn(s, out)
        self.assertIn("host: localhost", out)
        self.assertIn("name: x", out)

    def test_env_and_python_forms(self):
        out, n = redact("PASSWORD=hunter2\nclient_secret = 'abcd'\nreq(api_key='zzz9', timeout=3)\n")
        self.assertEqual(n, 3)
        self.assertIn("timeout=3", out)

    def test_pem_variants(self):
        for kind in ("RSA PRIVATE KEY", "OPENSSH PRIVATE KEY", "EC PRIVATE KEY", "CERTIFICATE"):
            text = f"a\n-----BEGIN {kind}-----\nQUJD\n-----END {kind}-----\nb"
            out, n = redact(text)
            self.assertEqual((out, n), ("a\n[REDACTED:pem]\nb", 1), kind)

    def test_unterminated_pem_masks_to_the_end(self):
        out, n = redact("x\n" + _BEGIN + "\nQUJDREVGRw")
        self.assertEqual((out, n), ("x\n[REDACTED:pem]", 1))

    def test_secret_keeps_its_key_and_quotes(self):
        self.assertEqual(redact("password='abc def'")[0], "password='[REDACTED:secret]'")

    def test_prefixed_token_inside_assignment_is_typed_by_the_token(self):
        out, n = redact(f"GITHUB_TOKEN={_gh}")
        self.assertEqual((out, n), ("GITHUB_TOKEN=[REDACTED:github-token]", 1))

    def test_url_password_only(self):
        out, _ = redact("https://bob:" + "pw12345" + "@host/x")
        self.assertEqual(out, "https://bob:" + "[REDACTED:url-password]" + "@host/x")

    def test_base64_run_inside_a_path_is_still_found(self):
        out, n = redact(f"/data/models/runs/{_b64}/final")
        self.assertEqual(n, 1)
        self.assertIn("/data/models/runs/", out)

    def test_counts_by_type(self):
        text = f"{_gh} and {_gh} and alice@example.com and password=hunter2"
        out, c = redact_counts(text)
        self.assertEqual(dict(c), {"github-token": 2, "email": 1, "secret": 1})
        self.assertEqual(redact(text)[1], 4)


class NoFalsePositives(unittest.TestCase):
    def test_prose(self):
        for p in PROSE:
            self.assertEqual(redact(p), (p, 0), p)

    def test_normal_shell_commands(self):
        for c in COMMANDS:
            self.assertEqual(redact(c), (c, 0), c)

    def test_paths_uuids_and_short_hashes(self):
        for t in ("/Users/jgavinray/dev/jev-evals/agentlogs/run_http.py",
                  "session 0a1b2c3d-4e5f-6789-abcd-ef0123456789 started",
                  "commit 1a2b3c4 and 9f86d081884c7d65",
                  "~/.claude/projects/-Users-jgavinray-dev-docs/abc.jsonl"):
            self.assertEqual(redact(t), (t, 0), t)


class Api(unittest.TestCase):
    def test_empty_and_plain(self):
        self.assertEqual(redact(""), ("", 0))
        self.assertEqual(redact("hello"), ("hello", 0))

    def test_unicode_survives(self):
        t = "héllo wörld — 日本語 password=hunter2"
        out, n = redact(t)
        self.assertEqual(out, "héllo wörld — 日本語 password=[REDACTED:secret]")

    def test_redact_obj_walks_structures(self):
        obj = {"a": [f"x {_gh}", {"b": "password=hunter2"}], "n": 5, "t": ("alice@example.com",)}
        out, n = redact_obj(obj)
        self.assertEqual(n, 3)
        self.assertNotIn("hunter2", json.dumps(out))
        self.assertEqual(out["n"], 5)

    def test_leftovers_finds_unmasked(self):
        self.assertEqual(leftovers("password=hunter2"), ["key-value"])
        self.assertEqual(leftovers("password=[REDACTED:secret]"), [])

    def test_large_pathological_input_is_fast(self):
        import time
        t0 = time.time()
        for t in ("-" * 200000, "ab-" * 70000, "password" * 20000, "a" * 500000):
            redact(t)
        self.assertLess(time.time() - t0, 10)


class Cli(unittest.TestCase):
    def run_cli(self, data, *args):
        env = dict(os.environ, PYTHONPATH=LIB)
        return subprocess.run([sys.executable, "-m", "fleetlib.redact", *args], input=data, capture_output=True, env=env, cwd="/")

    def test_stdin_to_stdout(self):
        p = self.run_cli(b"curl -u me --password hunter2 http://x\nsecond line\n")
        self.assertEqual(p.returncode, 0)
        self.assertEqual(p.stdout, b"curl -u me --password [REDACTED:secret] http://x\nsecond line\n")

    def test_count_and_max_chars(self):
        p = self.run_cli(b"password=hunter2 and more text here", "--count", "--max-chars", "25")
        self.assertEqual(p.stdout, b"password=[REDACTED:secret")
        self.assertEqual(p.stderr.strip(), b"1")

    def test_invalid_utf8_does_not_crash(self):
        p = self.run_cli(b"ok \xff\xfe password=hunter2")
        self.assertEqual(p.returncode, 0)
        self.assertNotIn(b"hunter2", p.stdout)

    def test_list_rules(self):
        p = self.run_cli(b"", "--list-rules")
        self.assertIn(b"github-token\tgithub-token", p.stdout)


if __name__ == "__main__":
    unittest.main()
