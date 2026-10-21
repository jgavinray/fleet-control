"""Secret masking: one rule table, typed markers, a CLI.

    from fleetlib.redact import redact
    masked, count = redact("password=hunter2 and ghp_...")

Every masked span becomes ``[REDACTED:<type>]`` (for example
``[REDACTED:github-token]``) and ``count`` is the number of spans masked.
Shell and bash callers use the CLI: ``python -m fleetlib.redact < in > out``.

This is the union of the maskers that existed before it: jev-evals
``glm/redact.py`` and ``agentlogs/redact.py``, the runner's ``scan.rs`` rules,
the guard hook's shadow redactor (``claude-base/hooks/jev-shadow.sh``) and
``mission-control/jev/redact.py``. Where they disagreed it takes the wider
rule, then adds guards so ordinary prose and normal shell commands survive.

The table is ``RULES`` and the rules run in order, most specific first. The
tests prove every rule is needed by removing each one in turn and checking
that the suite fails (``tests/mutation_check.py``).
"""
from __future__ import annotations

import argparse
import collections
import re
import sys
from dataclasses import dataclass
from typing import Callable, Optional

MARK = "[REDACTED:"


def marker(kind: str) -> str:
    return f"{MARK}{kind}]"


@dataclass(frozen=True)
class Rule:
    """One masking rule. ``fn`` returns the replacement for a match, or None
    to leave that match alone (it looked like a secret but is not one)."""

    id: str
    kind: str  # the type in the marker
    pattern: "re.Pattern[str]"
    fn: Callable[["re.Match[str]"], Optional[str]]


def _simple(kind: str) -> Callable[["re.Match[str]"], Optional[str]]:
    return lambda m: marker(kind)


def _digit(s: str) -> bool:
    return re.search(r"\d", s) is not None


# ---------------------------------------------------------------- prefixed tokens

_NB = r"(?<![A-Za-z0-9])"  # not in the middle of a longer word

_GITHUB = re.compile(_NB + r"(?:github_pat_[A-Za-z0-9_]{6,}|gh[pousr]_[A-Za-z0-9]{6,})")
# real Hugging Face tokens are hf_ plus 34 characters; short ones need a digit,
# so identifiers such as hf_tokenizer are left alone
_HF = re.compile(_NB + r"hf_(?:[A-Za-z0-9]{20,}|(?=[A-Za-z0-9]*\d)[A-Za-z0-9]{6,})")
# sk- keys (OpenAI, Anthropic sk-ant-): long, or shorter with a digit. The
# lookbehind keeps "task-runner" and "disk-usage" out.
_SK = re.compile(_NB + r"sk-(?:[A-Za-z0-9_\-]{20,}|(?=[A-Za-z0-9_\-]*\d)[A-Za-z0-9_\-]{6,})")
_AWS = re.compile(_NB + r"(?:AKIA[A-Z0-9]{8,}|ASIA[A-Z0-9]{16})")
_SLACK = re.compile(_NB + r"xox[A-Za-z]?-[A-Za-z0-9\-]{4,}")
_GOOGLE = re.compile(_NB + r"AIza[0-9A-Za-z_\-]{35}")
_JWT = re.compile(_NB + r"eyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}")

# ---------------------------------------------------------------- PEM

# Any PEM block. An unterminated block (a cut-off log line) masks to the end.
_PEM = re.compile(r"-----BEGIN [A-Z0-9 ]+-----.*?(?:-----END [A-Z0-9 ]+-----|\Z)", re.S)

# ---------------------------------------------------------------- credentials in context

_NOT_A_VALUE_START = ("$", "{{", "<", "%", "@", MARK)
_TYPE_WORDS = {
    "str", "string", "int", "integer", "bool", "boolean", "none", "null", "nil", "true", "false",
    "undefined", "required", "optional", "bytes", "any", "self", "args", "kwargs", "env",
}


def _looks_like_code_or_placeholder(v: str, env_assignment: bool = False) -> bool:
    """True for values that name a secret rather than hold one. In a NAME=value assignment with no spaces
    (shell, env file) the name is the signal, so type words such as true or none are values there."""
    if not v:
        return True
    if v.startswith(_NOT_A_VALUE_START):
        return True
    if not env_assignment and v.lower() in _TYPE_WORDS:
        return True
    if re.match(r"[A-Za-z_][\w.]*[(\[]", v):  # f(...), Optional[str], os.environ[...]
        return True
    if any(w in v for w in ("os.environ", "getenv", "process.env", "import.meta.env")):
        return True
    return False


# key = value, key: value, "key": "value", key: 'value', also JSON-escaped quotes.
# "tokens" / "tokenizer" are not credentials (max_tokens, tokenizer=...).
_KV = re.compile(
    r"(?i)((?:password|passwd|passphrase|passcode|secret|api[_\-]?key|apikey|access[_\-]?key"
    r"|private[_\-]?key|token(?!s(?![A-Za-z])|iz))[A-Za-z0-9_.\-]{0,64}\\?[\"']?\s*[=:]\s*)(?!=)"
    r"(\\\"[^\"\\\n]*\\\"|\"[^\"\n]*\"|'[^'\n]*'|[^\s'\"&;|,)}\]\\]+)"
)


def _kv(m: "re.Match[str]") -> Optional[str]:
    v = m.group(2)
    if v.startswith('\\"'):
        q, inner = '\\"', v[2:-2]
    elif v[0] in "\"'":
        q, inner = v[0], v[1:-1]
    else:
        q, inner = "", v
    if _looks_like_code_or_placeholder(inner, env_assignment=re.search(r"[^\s\"'\\]=$", m.group(1)) is not None):
        return None
    return m.group(1) + q + marker("secret") + q


# --password VALUE, --token VALUE (space separated command-line flag)
_FLAG = re.compile(
    r"(?i)(?<![A-Za-z0-9])(--?[A-Za-z0-9\-]{0,40}(?:password|passwd|secret|api-?key|token(?!s(?![A-Za-z])|iz))"
    r"(?!-(?:stdin|file|env|prompt|cmd|command|helper|store|path|dir|length|policy)(?![A-Za-z0-9]))[A-Za-z0-9\-]{0,40})"
    r"(\s+)(\"[^\"\n]*\"|'[^'\n]*'|(?![-$<\[{(])[^\s'\";&|<>)]+)"
)


def _flag(m: "re.Match[str]") -> Optional[str]:
    v = m.group(3)
    inner = v[1:-1] if v[0] in "\"'" else v
    if _looks_like_code_or_placeholder(inner) or inner.startswith("-"):
        return None
    q = v[0] if v[0] in "\"'" else ""
    return m.group(1) + m.group(2) + q + marker("secret") + q


# ---------------------------------------------------------------- passwords in command lines and in prose
#
# The rules above know token shapes and "name = value" for names that contain
# password, secret, token, api_key. They did not know:
#   ZB_PW=..., PASS=..., DB_PASSWD=...        env assignments with short names
#   --pass X, --pw X                          short long-flags
#   sshpass -p X, mysql -pX / -p X, docker login -p X, redis-cli -a X,
#   ldapsearch -w X, openssl -pass pass:X, curl -u user:X
#   "the password is X", "log in as U / X", "user U password X", "pw: X",
#   "credentials: U:X"
# -p is a password only for the tools above; ssh -p 22, scp -P and grep -p are
# ports and options, and stay. Only the value is masked, so the command shape
# (and with it the risk of the command) can still be judged.
# The prose rules are the prose-credential block of jev-evals
# agentlogs/redact.py (see wiki/prose-credential-redaction.md).

_SKIP_VALUES = frozenset(
    "true false yes no ok none null nil nan undefined pass passed fail failed password passwd pwd pw user username "
    "xxx xxxx secret-key".split()
)
_STOP = frozenset(
    """a an the this that these those it its is was are be been being not no yes none null nil true false
empty blank unset set missing required invalid wrong incorrect correct valid expired stored saved hashed encrypted salted
plain plaintext cleartext visible hidden shown printed logged exposed leaked for in on at to of and or but if so then than
too now here there what which who how why when where while with without from into by as your you my our their his her we
they i he she one another same different new old default current former right okay fine good bad long short strong weak
simple complex random generated needed asked entered typed sent read written changed reset updated rotated removed deleted
rejected accepted ignored supplied provided given used unused only also still just very quite really actually likely
probably maybe perhaps always never often sometimes already again yet please optional mandatory stdin env environment
file prompt field form page screen box input output value string text example below above see shown via using through
hard easy ready done fixed unchanged unknown unavailable disabled enabled allowed denied sufficient fed pasted cheap
expensive secure insecure safe unsafe public private known lost forgotten reused shared temporary temp local remote locked
unlocked pending authorised authorized accessible protected supposed meant important""".split()
)


def _unquote(v: str) -> str:
    return v[1:-1] if len(v) > 1 and v[0] in "`'\"" else v


def _wrap(v: str) -> str:
    """The marker, inside the same quotes the value had."""
    if len(v) > 1 and v[0] in "`'\"":
        return v[0] + marker("password") + v[-1]
    return marker("password")


def _prose_ok(v: str, strong: bool) -> bool:
    """Is this value after a prose / env password cue a password? A strong cue (is, :, =) also masks a plain
    word; a weak one only masks a value with a digit, a symbol, mixed case or quotes, so prose stays readable."""
    quoted = v[:1] in "`'\""
    inner = _unquote(v)
    if not inner or "REDACTED" in inner or inner[0] in "$%{<*/" or re.fullmatch(r"[xX.\-_#*]+", inner):
        return False
    low = inner.lower()
    if low in _SKIP_VALUES:
        return False
    if quoted:
        return low not in _STOP
    if re.fullmatch(r"[-+]?[\d.,:/%]+", inner) and len(inner) < 8:  # counts, ratios, versions
        return False
    if re.fullmatch(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+", inner) or "::" in inner:  # code: attribute chains
        return False
    if len(inner) < 3 and not re.search(r"[A-Za-z0-9]", inner):
        return False
    has_digit, has_alpha = re.search(r"\d", inner), re.search(r"[A-Za-z]", inner)
    mixed_case = re.search(r"[A-Z]", inner[1:]) and re.search(r"[a-z]", inner)
    if re.search(r"[!@#$%^&*~?]", inner):
        return True
    if strong:
        if has_digit or mixed_case:
            return True
        return re.fullmatch(r"[A-Za-z]+", inner) is not None and len(inner) >= 3 and low not in _STOP
    return bool((has_digit and has_alpha and len(inner) >= 6) or (mixed_case and len(inner) >= 6))


def _shell_ok(v: str, dash_ok: bool = False) -> bool:
    """A value on a command line is a password unless it is a variable, a redirect, another flag or a marker."""
    inner = _unquote(v)
    if not inner or "REDACTED" in inner:
        return False
    if not v.startswith("'") and (inner[0] == "<" or (inner[0] == "$" and not inner.startswith("$'"))):
        return False  # $VAR, ${VAR}, $(cmd) and redirects are not values
    if not dash_ok and inner.startswith("-"):
        return False  # "--password --verbose": the next flag, not a value
    return True


def _value_rule(ok: "Callable[[str], bool]") -> "Callable[[re.Match[str]], Optional[str]]":
    """Group 1 is kept, group 2 is the value that gets masked (when ok says it is a password)."""

    def fn(m: "re.Match[str]") -> Optional[str]:
        return m.group(1) + _wrap(m.group(2)) if ok(m.group(2)) else None

    return fn


# a value on a command line: quoted (spaces allowed) or bare
_SV = r"""\"[^\"\n]*\"|'[^'\n]*'|[^\s'\"`;&|<>()]+"""
# the arguments between a tool name and its -p flag, inside one simple command
_GAP = r"[^;&|\n()]{0,300}?"

# The NAME is the signal: a password-like name masks the value whatever its shape (fake-value-xyz, abc-def,
# correct-horse-battery). Bare PWD is the shell's working directory and is not matched.
_ENV_PW = re.compile(
    r"(?<![A-Za-z0-9$/])((?:(?:[A-Za-z0-9]{1,30}[_.\-]){0,6}(?i:pass|passwd|passcode|pw)[0-9]{0,2}"
    r"|(?:[A-Za-z0-9]{1,30}[_.\-]){1,6}(?i:pwd)[0-9]{0,2}"
    r"|(?i:(?!(?:com|by|tres|sur|over|under)pass(?![A-Za-z]))[A-Za-z]{1,20}(?:pass|passwd|pwd|pw)))"
    r"(?:[_.\-][A-Za-z0-9]{1,30}){0,4}=)(" + _SV + ")"
)
_PASS_FLAG = re.compile(r"(?i)(?<![A-Za-z0-9_\-])(--(?:pass|pw|pwd)(?:=|[ \t]+))(" + _SV + ")")
# sshpass -p VALUE, after its own flags only, so "sshpass -e ssh -p 22 host" keeps its port
_SSHPASS = re.compile(r"(\bsshpass[ \t]+(?:(?:-[ev]|-[fdP][ \t]*\S+)[ \t]+)*-p[ \t]*)(" + _SV + ")")
_MYSQL = re.compile(
    r"(\b(?:mysql|mysqldump|mysqladmin|mysqlimport|mysqlshow|mysqlcheck|mysqlpump|mysqlbinlog|mariadb|mariadb-dump"
    r"|mariadb-admin|mariadb-import|mariadb-show|mariadb-check)\b" + _GAP + r"[ \t]-p(?!rint\b|rune\b|erm\b)[ \t]*)(" + _SV + ")"
)
_TOOL_PW = re.compile(
    r"(\bdocker[ \t]+login\b" + _GAP + r"[ \t]-p[ \t]*"
    r"|\bredis-cli\b" + _GAP + r"[ \t]-a[ \t]+"
    r"|\bldap(?:search|modify|add|delete|passwd|compare|whoami|modrdn)\b" + _GAP + r"[ \t]-w[ \t]*"
    r"|-pass(?:in|out)?[ \t]+pass:)(" + _SV + ")"
)
# curl -u USER:PASS, --user USER:PASS (the user name stays)
_CURL_USER = re.compile(
    r"(\bcurl\b" + _GAP + r"""[ \t](?:-[A-Za-z]{0,10}u|--user)(?:[ \t]+|=)?["']?[^\s:'"]+:)([^\s'";&|]+)"""
)

_PW = r"(?:password|passwd|passcode|passphrase|pwd|pw)"
_USER = r"""[`'"]?[\w.@\-]+[`'"]?"""
_VAL = r"""(`[^`\s]{1,64}`|"[^"\s]{1,64}"|'[^'\s]{1,64}'|[^\s`'"<>(){}\[\],;]*[^\s`'"<>(){}\[\],;.:?*])"""
# same, but a bare value ends at '&' (query strings: user=bob&pw=abc123&next=1)
_VAL_Q = r"""(`[^`\s]{1,64}`|"[^"\s]{1,64}"|'[^'\s]{1,64}'|[^\s`'"<>(){}\[\],;&]*[^\s`'"<>(){}\[\],;&.:?*])"""
_PW_FOR = r"(?:[ \t]+(?:for|of)[ \t]+(?:the[ \t]+)?(?:(?:user|account|login|admin)[ \t]+)?" + _USER + r")?"
_LOGIN = r"\blog(?:ged|ging)?[ \t-]?in[ \t]+(?:as|with|using)[ \t]+(?:user(?:name)?[ \t]+)?" + _USER

_PROSE_KV = re.compile(
    r"(?i)(?<![A-Za-z0-9$])((?:[A-Za-z0-9]{1,30}[_\-]){0,6}(?:(?-i:pass|Pass)|pw|pwd)[`'\"]?[ \t]*(?::=|[:=])[ \t]*)" + _VAL_Q
)
_PROSE_IS = re.compile(r"(?i)\b(" + _PW + _PW_FOR + r"[ \t]+(?:is|was|are)\b[ \t]*:?[ \t]*)" + _VAL)
_PROSE_SET_TO = re.compile(
    r"(?i)\b(" + _PW + _PW_FOR + r"[ \t]+(?:(?:is|was|has been|will be|should be)[ \t]+)?"
    r"(?:set|changed|reset|updated|rotated)?[ \t]*(?:to|->|→)[ \t]+)" + _VAL
)
_LOGIN_SEP = re.compile(r"(?i)(" + _LOGIN + r"[ \t]*[/:,][ \t]*)" + _VAL)
_LOGIN_PW = re.compile(
    r"(?i)(" + _LOGIN + r"[,;]?[ \t]+(?:(?:with|and)[ \t]+)?(?:the[ \t]+)?" + _PW + r"[ \t]*(?:is|=|:)?[ \t]*)" + _VAL
)
_USER_PW = re.compile(
    r"(?i)\b((?:user(?:name)?|login|account|acct|id)[ \t]*[:=]?[ \t]*(?:is[ \t]+)?" + _USER
    + r"[,;]?[ \t]+(?:(?:and|with)[ \t]+)?" + _PW + r"[ \t]*[:=]?[ \t]*)" + _VAL
)
_CREDS = re.compile(
    r"(?i)\b((?:credentials?|creds?)\b[ \t]*(?:(?:are|is)[ \t]*)?[:=]?[ \t]*" + _USER + r"[ \t]*[:/][ \t]*)" + _VAL
)

_strong = lambda v: _prose_ok(v, True)  # noqa: E731
_weak = lambda v: _prose_ok(v, False)  # noqa: E731
_shell = lambda v: _shell_ok(v, False)  # noqa: E731
_shell_dash = lambda v: _shell_ok(v, True)  # noqa: E731


# Bearer <token>. A bare word after "bearer" in prose ("bearer of news") stays.
_BEARER = re.compile(r"(?i)\b(bearer)(\s+)([A-Za-z0-9._~+/=\-]{8,})")


def _bearer(m: "re.Match[str]") -> Optional[str]:
    v = m.group(3)
    if _digit(v) or len(v) >= 20 or re.search(r"[._~+/=\-]", v):
        return m.group(1) + m.group(2) + marker("bearer")
    return None


# Authorization: Basic <b64> / Token <t> / Digest <x>
_AUTH_HEADER = re.compile(
    r"(?i)\b(authorization[\"']?\s*[=:]\s*[\"']?)(basic|digest|token)(\s+)([^\s'\"]{8,})"
)


def _auth_header(m: "re.Match[str]") -> Optional[str]:
    if m.group(4).startswith(_NOT_A_VALUE_START):
        return None
    return m.group(1) + m.group(2) + m.group(3) + marker("auth")


# a URL with user and password before the @: the user name stays, the password goes
_URL_CRED = re.compile(r"(?<=://)([^/\s:@'\"]+):([^@\s/'\"]+)(?=@)")


def _url_cred(m: "re.Match[str]") -> Optional[str]:
    if m.group(2).startswith(("$", "{", "<", "%", MARK)):
        return None
    return m.group(1) + ":" + marker("url-password")


# ---------------------------------------------------------------- email

# name@host.tld. scp-style "git@github.com:owner/repo" is an address, not an email.
_EMAIL = re.compile(
    r"(?<![A-Za-z0-9._%+\-])[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}"
    r"(?![A-Za-z0-9\-])(?!:[A-Za-z0-9_~/.\-])"
)

# ---------------------------------------------------------------- long runs

_HEX = re.compile(_NB + r"[A-Fa-f0-9]{33,}(?![A-Za-z0-9])")
_B64 = re.compile(_NB + r"[A-Za-z0-9+/]{33,}={0,2}(?![A-Za-z0-9])")
# urlsafe base64 (- and _): needs upper, lower and digit so kebab-case names survive
_B64URL = re.compile(_NB + r"(?=[A-Za-z0-9_\-]*[-_])[A-Za-z0-9_\-]{40,}(?![A-Za-z0-9])")
# 20-32 alphanumeric characters mixing upper, lower and digits: random-token shape
_MIXED = re.compile(
    _NB + r"(?=[A-Za-z0-9]*[a-z])(?=[A-Za-z0-9]*[A-Z])(?=[A-Za-z0-9]*\d)[A-Za-z0-9]{20,32}(?![A-Za-z0-9])"
)

_WORDY = re.compile(r"(?:[A-Z]?[a-z]+){1,5}[0-9]{0,3}")


def _is_blob(t: str) -> bool:
    """A long alphanumeric run is a secret if it mixes letters and digits, or
    is very long with both cases."""
    if _digit(t) and re.search(r"[A-Za-z]", t):
        return True
    return (
        "/" not in t and len(t) > 40
        and re.search(r"[a-z]", t) is not None and re.search(r"[A-Z]", t) is not None
    )


def _b64(m: "re.Match[str]") -> Optional[str]:
    t = m.group(0)
    if "/" in t:
        segs = [s for s in t.split("/") if s]
        wordy = sum(1 for s in segs if _WORDY.fullmatch(s))
        if len(segs) >= 3 and wordy * 2 >= len(segs):
            # a file path: only the long, random-looking segments are secrets
            out = [marker("base64") if len(s) > 32 and _is_blob(s) else s for s in t.split("/")]
            r = "/".join(out)
            return r if r != t else None
    return marker("base64") if _is_blob(t) else None


def _b64url(m: "re.Match[str]") -> Optional[str]:
    t = m.group(0)
    if _digit(t) and re.search(r"[a-z]", t) and re.search(r"[A-Z]", t):
        return marker("base64")
    return None


RULES: list[Rule] = [
    Rule("pem", "pem", _PEM, _simple("pem")),
    Rule("url-credential", "url-password", _URL_CRED, _url_cred),
    Rule("github-token", "github-token", _GITHUB, _simple("github-token")),
    Rule("hf-token", "hf-token", _HF, _simple("hf-token")),
    Rule("sk-key", "api-key", _SK, _simple("api-key")),
    Rule("aws-key", "aws-key", _AWS, _simple("aws-key")),
    Rule("slack-token", "slack-token", _SLACK, _simple("slack-token")),
    Rule("google-key", "google-key", _GOOGLE, _simple("google-key")),
    Rule("jwt", "jwt", _JWT, _simple("jwt")),
    Rule("auth-header", "auth", _AUTH_HEADER, _auth_header),
    Rule("bearer", "bearer", _BEARER, _bearer),
    Rule("key-value", "secret", _KV, _kv),
    Rule("secret-flag", "secret", _FLAG, _flag),
    Rule("env-password", "password", _ENV_PW, _value_rule(_shell_dash)),
    Rule("pass-flag", "password", _PASS_FLAG, _value_rule(_shell)),
    Rule("sshpass", "password", _SSHPASS, _value_rule(_shell_dash)),
    Rule("mysql-password", "password", _MYSQL, _value_rule(_shell)),
    Rule("tool-password", "password", _TOOL_PW, _value_rule(_shell)),
    Rule("curl-user", "password", _CURL_USER, _value_rule(_shell_dash)),
    Rule("prose-pw-kv", "password", _PROSE_KV, _value_rule(_weak)),
    Rule("prose-is", "password", _PROSE_IS, _value_rule(_strong)),
    Rule("prose-set-to", "password", _PROSE_SET_TO, _value_rule(_weak)),
    Rule("prose-login-sep", "password", _LOGIN_SEP, _value_rule(_weak)),
    Rule("prose-login-pw", "password", _LOGIN_PW, _value_rule(_strong)),
    Rule("prose-user-pw", "password", _USER_PW, _value_rule(_weak)),
    Rule("prose-creds", "password", _CREDS, _value_rule(_weak)),
    Rule("email", "email", _EMAIL, _simple("email")),
    Rule("hex-run", "hex", _HEX, _simple("hex")),
    Rule("base64-run", "base64", _B64, _b64),
    Rule("base64url-run", "base64", _B64URL, _b64url),
    Rule("mixed-token", "token", _MIXED, _simple("token")),
]


def _n_marks(s: str) -> int:
    return s.count(MARK)


def redact_counts(text: str) -> "tuple[str, collections.Counter[str]]":
    """Mask ``text``. Returns (masked, Counter of spans masked per marker type)."""
    counts: "collections.Counter[str]" = collections.Counter()
    if not text:
        return text or "", counts
    for rule in RULES:
        def sub(m: "re.Match[str]", rule: Rule = rule) -> str:
            r = rule.fn(m)
            if r is None:
                return m.group(0)
            added = _n_marks(r) - _n_marks(m.group(0))
            if added > 0:
                counts[rule.kind] += added
            return r

        text = rule.pattern.sub(sub, text)
    return text, counts


def redact(text: str) -> "tuple[str, int]":
    """Mask secrets in ``text``. Returns (masked_text, number_of_spans_masked)."""
    masked, counts = redact_counts(text)
    return masked, sum(counts.values())


def leftovers(text: str) -> "list[str]":
    """Gate check: ids of the rules that still find an unmasked secret in
    ``text``. On the output of ``redact`` this must be empty."""
    bad = []
    for rule in RULES:
        for m in rule.pattern.finditer(text):
            r = rule.fn(m)
            if r is not None and r != m.group(0):
                bad.append(rule.id)
                break
    return bad


def redact_obj(obj, _counts: "Optional[collections.Counter[str]]" = None):
    """Mask every string inside a JSON-like structure (dict keys are kept).
    Returns (new_obj, count)."""
    counts = _counts if _counts is not None else collections.Counter()

    def walk(o):
        if isinstance(o, str):
            masked, c = redact_counts(o)
            counts.update(c)
            return masked
        if isinstance(o, dict):
            return {k: walk(v) for k, v in o.items()}
        if isinstance(o, (list, tuple)):
            return [walk(v) for v in o]
        return o

    out = walk(obj)
    return out, sum(counts.values())


def main(argv: "Optional[list[str]]" = None) -> int:
    ap = argparse.ArgumentParser(
        prog="python -m fleetlib.redact",
        description="Read text on stdin, write it to stdout with secrets masked.",
    )
    ap.add_argument("--count", action="store_true", help="print the number of masked spans to stderr")
    ap.add_argument("--max-chars", type=int, default=0, help="cut the masked output to this many characters (0 = no cut)")
    ap.add_argument("--list-rules", action="store_true", help="print the rule ids and marker types, then exit")
    args = ap.parse_args(argv)
    if args.list_rules:
        for r in RULES:
            print(f"{r.id}\t{r.kind}")
        return 0
    data = sys.stdin.buffer.read().decode("utf-8", errors="replace")
    masked, n = redact(data)
    if args.max_chars and len(masked) > args.max_chars:
        masked = masked[: args.max_chars]
    sys.stdout.buffer.write(masked.encode("utf-8"))
    sys.stdout.buffer.flush()
    if args.count:
        print(n, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
