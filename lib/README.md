# fleetlib

One shared, tested Python library for three jobs that several scripts had each
written their own copy of:

1. **Masking secrets** (`fleetlib.redact`). There were at least five separate
   maskers, so five chances to miss a password.
2. **Reading Claude Code and omp session logs** (`fleetlib.logs`).
3. **One-word classification calls to the local models** (`fleetlib.classify`,
   with engine addresses from `fleetlib.registry`).

Python 3.11 or newer. Standard library only. Nothing to install: put
`fleet/lib` on `PYTHONPATH`.

```
export PYTHONPATH=~/dev/mission-control/fleet/lib
python3 -m fleetlib.redact < in.txt > out.txt
```

Run the tests with `fleet/lib/test.sh` (see "The gate" at the end).

## fleetlib.redact: mask secrets

`redact(text)` returns `(masked_text, count)`. Every masked span becomes a
typed marker such as `[REDACTED:github-token]`, and `count` is how many spans
were masked.

```python
from fleetlib.redact import redact
masked, count = redact("curl -H 'Authorization: Bearer abc123def456ghi789' https://x")
print(masked)   # curl -H 'Authorization: Bearer [REDACTED:bearer]' https://x
print(count)    # 1
```

What it catches (the rule table is `RULES` in `redact.py`; `--list-rules`
prints it):

| Rule | Marker | Catches |
|---|---|---|
| `pem` | `pem` | any `-----BEGIN ...-----` block (an unfinished one masks to the end) |
| `url-credential` | `url-password` | the password part of a URL with `user:password` before the `@` (the user name stays) |
| `github-token` | `github-token` | `ghp_` `gho_` `ghu_` `ghs_` `ghr_` and `github_pat_` |
| `hf-token` | `hf-token` | `hf_` tokens |
| `sk-key` | `api-key` | `sk-` keys, including `sk-ant-` |
| `aws-key` | `aws-key` | `AKIA...` and `ASIA...` key ids |
| `slack-token` | `slack-token` | `xox?-` tokens |
| `google-key` | `google-key` | `AIza...` keys |
| `jwt` | `jwt` | three-part `eyJ...` tokens |
| `auth-header` | `auth` | `Authorization: Basic/Digest/Token <value>` |
| `bearer` | `bearer` | `Bearer <token>` (not `Bearer $VAR`, not the word in prose) |
| `key-value` | `secret` | the value after `password`, `passwd`, `passphrase`, `secret`, `api_key`, `access_key`, `private_key` or `token` in `k=v`, `k: v`, JSON (`"k": "v"`), YAML, env files and JSON-escaped log lines |
| `secret-flag` | `secret` | the value in `--password hunter2`, `--token abc` |
| `email` | `email` | email addresses (not `git@github.com:owner/repo`) |
| `hex-run` | `hex` | hex runs over 32 characters (this includes full git SHAs) |
| `base64-run` | `base64` | base64 runs over 32 characters that mix letters and digits (file paths are left alone) |
| `base64url-run` | `base64` | url-safe base64 (`-` and `_`) runs of 40 or more with upper, lower and digits |
| `mixed-token` | `token` | 20 to 32 character alphanumeric runs with upper, lower and digits |

It leaves alone: values that are variables or code (`$TOKEN`, `${X}`,
`{{ x }}`, `os.environ[...]`, `Optional[str]`), `max_tokens` and `tokenizer`,
type words (`password: str`), `password == "x"`, `git@github.com:...`
addresses, paths, UUIDs, and ordinary prose. Masking twice gives the same
result as masking once.

Other functions: `redact_counts(text)` gives the count per marker type;
`redact_obj(obj)` masks every string in a JSON-like structure;
`leftovers(text)` lists the rules that still find a secret (a gate check: on
masked output it must be empty).

**Command line**, for bash callers like the guard hook:

```
python3 -m fleetlib.redact < in > out          # masked text on stdout, always exit 0
python3 -m fleetlib.redact --count < in        # also print the count to stderr
python3 -m fleetlib.redact --max-chars 4000    # cut the masked output
```

Where this came from: it is the union of `jev-evals/glm/redact.py`,
`jev-evals/agentlogs/redact.py` (and its `leftovers` gate),
`mission-control/jev/redact.py`, the guard hook's shadow redactor
(`claude-base/hooks/jev-shadow.sh`) and the secret rules in
`mission-control/runner/src/scan.rs`. Where they disagreed it takes the wider
rule, then adds guards. On 11,900 real commands from this Mac's logs the old
maskers flagged about 2,700; almost all of the difference is false positives
that this one skips (`max_tokens`, paths with a `-` in a long name, the word
"token" in prose, `git@github.com`, `Bearer $VAR`). The rest are intended.

## fleetlib.logs: read session logs

Reads Claude Code (`~/.claude/projects/**/*.jsonl`, which includes
`<session>/subagents/`) and omp (`~/.omp/agent/sessions/**/*.jsonl`, which
includes sub-session files). Units are masked by default.

```python
from fleetlib.logs import iter_units
for u in iter_units(kinds={"user_message"}, human_only=True, max_chars=500):
    print(u.host, u.source, u.session_id, u.timestamp, u.cwd, u.text)
```

Four kinds of unit, each with `host`, `source` (`claude-code` or `omp`),
`session_id`, `timestamp`, `cwd`, `file`, `subagent`, `redactions` (spans
masked) and `truncated`:

| Class | `kind` | Fields |
|---|---|---|
| `UserMessage` | `user_message` | `text`, `human`, `category`, `pasted_chars`, `queued` |
| `AssistantTurn` | `assistant_turn` | `text`, `tool_calls`, `tool_call_count`, `stop_reason`, `ended_turn`, `interrupted` |
| `ShellCommand` | `shell_command` | `command`, `tool` (`Bash` or `bash`) |
| `ToolError` | `tool_error` | `tool`, `tool_input`, `error_text`, `exit_code`, `error_class` |

`human` is True only for real human input. Everything else has `human=False`
and a `category`: `tool_result` (only with `include_tool_results=True`),
`system_reminder`, `task_notification`, `command` (slash and local-command
wrappers), `interrupt`, `meta`, `sdk` (programmatic `claude -p`), `agent`
(subagent prompts, omp agent-attributed messages), `summary` (session
continuation), `developer`, `no_text`, `pasted_only`, `other`. Pasted blocks
are cut out of human text and counted in `pasted_chars`; a message typed while
the model was busy is logged twice by Claude Code and is kept once.
`error_class` buckets failures: `hook_block`, `permission_denied`, `skipped`,
`aborted`, `user_rejected`, `edit_mismatch`, `file_not_found`, `timeout`,
`nonzero_exit`, `invalid_args`, `service_failure`, `tool_misuse`, `other`.

Options of `iter_units`: `sources` (default: this machine; `default_sources()`
or `raw_sources("~/dev/jev-evals/raw")` for logs copied from other hosts),
`kinds`, `redact=True`, `max_chars` (cuts after masking), `since` (ISO
timestamp), `human_only`, `include_tool_results`, and `stats` (a Counter that
collects file and line counts, including unreadable lines, which are skipped).
Units come in file order, except an `AssistantTurn` arrives when it closes (just
before the user message that follows it); sort by `timestamp` for strict time
order. Nothing is written to disk and nothing is deduplicated across files.

omp schema, learned from real files on 10/20/2026: a `session` record
(`id`, `cwd`, `parentSession` on sub-sessions), `message` records with role
`user` (`attribution: "user"` is the human; `"agent"` is machine-written),
`developer`, `assistant` (blocks `text`, `thinking`, `toolCall`; thinking is
not kept) or `toolResult` (`isError`, `details.exitCode`), and
`custom_message` records for injected context. A bash result with a non-zero
`exitCode` counts as an error even when `isError` is false.

## fleetlib.classify: one-word calls to the local models

`classify(text, options, engine, k=...)` returns a dict `{option: probability}`
(a `Classification`, with the details in `.meta`).

```python
from fleetlib.classify import classify
r = classify("git push --force origin main", ["read", "edit", "push", "destroy"], "glm", k=5)
print(r)                      # {'read': 0.0, 'edit': 0.0, 'push': 1.0, 'destroy': 0.0}
print(r.meta["choice"], r.meta["invalid"], r.meta["backoffs"], r.meta["seconds"])
```

`options` may be a dict `{name: description}`; the descriptions go into the
prompt (and become the `criteria` for the `/v1/systemone` engines).

How each engine is asked (`meta["method"]`):

- **GLM** (`guided_choice`): `k` requests (default 5), each with
  `guided_choice` set to the options, thinking off, `temperature: 1`, its own
  `seed`, a differently shuffled option order, and `priority: "background"`.
  Probability = share of the `k` answers. GLM returns identical text at
  temperature 1 unless the seed differs, so the seeds differ; the seeds come
  from a hash of the request unless you pass `seed=`, so a repeat call sends the
  same seeds. `smoothing=` adds a pseudo-count if you want no exact zeros.
- **DeepSeek** (`parse`): DeepSeek rejects `guided_choice` with HTTP 400, so it
  is asked to "reply with exactly one option word" and the reply is parsed
  (case, quotes and trailing punctuation ignored; "push - because..." counts as
  push). A reply that is not an option counts in `meta["invalid"]` and not in
  the probabilities; if every reply is invalid the probabilities are all 0.0.
  `k` defaults to 1 at temperature 0; with `k > 1` the temperature is 1.
- **Qwen** (`parse`, shared): same method as DeepSeek, one request at a time, and
  before every request it asks `/health` and backs off while `live.connections > 0`
  (other people use it).
- **laya** and **jev-glm** (`systemone`): one `POST /v1/systemone` with the
  text as `state` and one choice question; the service returns the
  probabilities (jev-glm samples GLM 8 times itself). `k` is not used.

Limits and back-off, all engines: at most `max_inflight` requests at once per
engine across threads (GLM 2, DeepSeek 1, Qwen 1, laya 1, jev-glm 2;
configurable in the registry or per call); before each request `GET /health`,
and if `requests_waiting` (or `live.waiting`) is above 0 it sleeps `backoff_s`
(30 s) and asks again, up to `max_wait_s` (900 s) and then raises `EngineBusy`.
Samples that fail are counted in `meta["errors"]`; if all fail, `EngineError`.

The text is masked with `fleetlib.redact` before it is sent
(`redact_text=False` to turn that off) and cut to `max_chars` (4000).

Command line: `python3 -m fleetlib.classify --engine glm --options read,edit,push "text"`
prints JSON.

### Engine addresses: the registry, else built-in defaults

`fleetlib.registry` finds engines in this order:

1. **`fleet/fleet.toml`** (next to this library; or the file named by
   `$FLEET_TOML`; or `~/dev/mission-control/fleet/fleet.toml`). It is the
   registry that `fleet-check` reads. From each `[engines.NAME]` table the
   library uses `url`, `model`, `kind`, `shared` and
   `expected_features.guided_choice`: `kind` `laya` or `jev-glm` means
   `/v1/systemone`; `kind = "tensorfold"` means `guided_choice` when the
   expected feature is `"yes"` and ask-and-parse when it is `"no"`; `shared =
   true` means back off on other connections. `lfm2d` engines are ignored here.
   Optional tuning keys: `method`, `k`, `max_inflight`, `temperature`,
   `max_tokens`, `timeout`, `priority`.
2. **Built-in defaults** for `glm` (192.168.0.213:8888), `deepseek`
   (192.168.0.33:8888), `qwen` (192.168.0.43:8081), `laya`
   (192.168.0.44:8001) and `jev-glm` (192.168.0.44:8102), used when there is
   no file or the file does not list the engine. A registry entry only
   overrides the keys it names.

You can also pass an `Engine(...)` object instead of a name.

## Callers that should move onto fleetlib later

Not moved yet; each is its own follow-up task. Do not edit these while other
workers are using them.

| File | Has its own | Move to |
|---|---|---|
| `~/dev/jev-evals/glm/redact.py` | masker | `fleetlib.redact.redact` |
| `~/dev/jev-evals/agentlogs/redact.py` | masker with counts | `fleetlib.redact.redact_counts` |
| `~/dev/jev-evals/agentlogs/gate_redaction.py` | leftovers gate | `fleetlib.redact.leftovers` |
| `~/dev/jev-evals/glm/extract.py` | Claude Code command extractor | `fleetlib.logs.iter_units(kinds={"shell_command"})` |
| `~/dev/jev-evals/agentlogs/extract.py` | Claude Code and omp unit extractor (users, commands, errors) | `fleetlib.logs.iter_units` (dedupe by text on top, as it does now) |
| `~/dev/jev-evals/agentlogs/run_http.py` | GLM, DeepSeek and Qwen labelling, back-off | `fleetlib.classify.classify` |
| `~/dev/jev-evals/glm/glm.py` | GLM sampling call | `fleetlib.classify.classify` |
| `~/dev/claude-base/hooks/jev-shadow.sh` (`redact()` in perl, also `redact` mode) | masker | `python3 -m fleetlib.redact` |
| `~/dev/mission-control/jev/redact.py` | the service's second-line masker | `fleetlib.redact.redact` |
| `~/dev/mission-control/jev/backend.py` (`SamplingBackend`) | GLM sampling (seeds, shuffle, background priority) | could share `fleetlib.classify` internals; it is a service with its own limits, so lowest priority |
| `~/dev/mission-control/runner/src/scan.rs` | Rust secret scan over added lines | stays Rust (different language); keep its rules in step with the `RULES` table, using `fleetlib.redact.leftovers` for cross-checks |

## The gate

`fleet/lib/test.sh` exits non-zero on any failure. It runs:

1. the `unittest` suites: `tests/test_redact.py` (a positive and a negative
   case per rule, PEM, JSON and YAML forms, prose and shell commands that must
   not change, the CLI), `tests/test_logs.py` (hand-written fixture files in
   `tests/fixtures/`, fake data only), `tests/test_classify.py` (a local stub
   HTTP server: seeded sampling, the guided and parse paths, back-off, the
   registry);
2. `tests/mutation_check.py`: removes each redact rule in turn and requires the
   redact suite to fail;
3. `tests/repo_clean.py`: runs the library's own redactor over every file in
   `fleet/lib` and fails on any secret-shaped value that is not on a short list
   of made-up ones.

What it does not cover: the live engines (the tests use a stub; a real call is
a separate manual check, `python3 -m fleetlib.classify --engine glm ...`), real
transcripts (none are in the repo, by design), and whether the masking rules
catch every secret format that exists. Test secrets are assembled at run time
(`tests/helpers.py`, `fake()`), so no token-shaped string is committed.
