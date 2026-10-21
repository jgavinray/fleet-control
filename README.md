# fleet: the registry and fleet-check

Two things live here.

1. **`fleet.toml`**, the one file that says which machines we have and which
   model engines run on them.
2. **`fleet-check`**, one command that reads that file and tells you whether
   every engine is up and behaving the way the file says it should.

**Run `fleet-check` after every engine deploy or restart.** A restart that
comes back "healthy" can still answer wrongly, or have lost a feature (for
example guided choices). Health alone does not show that. This does.

## Running it

```
fleet-check                 # every engine, with feature probes
fleet-check glm             # one engine
fleet-check --quick         # skip the feature probes (health + sanity question only)
fleet-check --json          # machine-readable output
fleet-check --force-shared  # also test a shared engine that is busy (see below)
```

`~/.local/bin/fleet-check` is a symlink to `fleet/check/fleet-check` in this
repo. It needs Python 3.11 or newer and nothing else.

### Reading the output

```
ENGINE    STATUS  MODEL                     LOAD                         SANITY  LATENCY  FEATURES
deepseek  OK      DeepSeek-V4.1-Flash-1017  busy (4 running, 1 waiting)  pass    1.15s    guided=no bg=yes logprobs=no prompt_lp=no
glm       OK      GLM-5.3-Flash-EXL3        busy (7 running, 0 waiting)  pass    4.62s    guided=yes bg=yes logprobs=no prompt_lp=no
```

- **STATUS**
  - `OK`: reachable, right model, correct answer, no drift.
  - `DOWN`: no answer, or it says it is not ready or still warming up.
  - `WRONG`: it answered, but the wrong model id or the wrong answer to the
    sanity question.
  - `SLOW`: the sanity question timed out (even at normal priority) while the
    engine was busy. It is not down, but it could not answer in time.
    Re-run later or raise `--timeout`. Counts as exit 1.
  - `DRIFT`: it works, but a feature differs from what the registry expects.
  - `BUSY`: a shared engine in use by someone else; left alone (see below).
- **LOAD**: whether anyone is using it right now (running and waiting
  requests). Busy is normal for the big engines; it only explains latency.
- **SANITY**: one question with a known answer.
  - TensorFold engines: "What is 17 plus 25? Reply with the number only." with
    thinking off. Must answer `42`.
  - laya and jev-glm: a tiny `/v1/systemone` decision (laya: "charged twice,
    refund me" must be `billing`; jev-glm: `ls -la` must be `read`).
  - lfm2d: a cheap read (the 8B must list a spec; the encoders must scan a
    short string). The 8B's metrics exporter is checked with it.
- **LATENCY**: how long the sanity request took.
- **FEATURES**: measured fresh on every run, never assumed. A `!` after a
  value means it differs from the registry.
  - `guided` is `guided_choice` (thinking off).
  - `bg` is `priority: "background"`.
  - `logprobs` is chat `logprobs`.
  - `prompt_lp` is completions `prompt_logprobs`.

  Each is `yes`, `no` or `error`. `no` covers both a refusal (HTTP 400) and a
  silent ignore (HTTP 200 with nothing in it). `error` means the probe could
  not tell (timeout, server error). The reason for every `no`, `error` and
  drift is printed under the table.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | All good. |
| 1 | An engine is down or answered wrong. Fix this first. |
| 2 | Engines work, but a feature differs from the registry ("drift"). |

If both kinds of problem exist, the exit code is 1.

### Drift is a question, not always a bug

Drift means "the registry and the engine disagree". Either the engine changed
by accident (look into it), or it changed on purpose (an upgrade added
`prompt_logprobs`) and the registry needs updating. Edit `expected_features`
for that engine in `fleet.toml` in the same change as the deploy.

### How gentle it is

- About 6 small requests per engine at most, one after another. Probes
  use `max_tokens` of 1 to 8.
- `priority: "background"` is sent wherever the engine accepts it, so coding
  agents on the same engine go first.
- On a saturated engine, background requests wait behind everyone else. If
  the background sanity request waits more than 20 seconds (`--bg-timeout`),
  it is re-sent once at normal priority, the probes then go without the
  priority flag, and a note says so. Only if that also times out is the
  engine reported `SLOW` (or `DOWN` if it was idle).
- **Shared engines.** Qwen on hyper03 serves one request at a time and the
  ghostwriter uses it, on a machine shared with another engineer. If its
  `/health` shows `live.connections` above 0, fleet-check sends it only GET
  requests: no sanity question, no probes. It prints `BUSY` and says so.
  Re-run when it is idle, or pass `--force-shared` if you decide to.
- jev-glm's sanity decision makes 8 background requests to GLM (that is how
  the service works), so it is the slowest row.

## The registry: `fleet.toml`

Two sections: `[engines.*]` and `[hosts.*]`. The comments in the file say what
each field means. Rules:

- Keys are sorted and there are no timestamps, so a diff shows only real
  changes. A test checks this.
- Base URLs and LAN addresses only. No keys, no Tailscale addresses.
- It records facts that were checked. `notes` is for the warnings people keep
  re-learning (for example "NEVER use the old `dsv41-exl3.service`").

### Add an engine

1. Make sure its host is under `[hosts.*]`. Add the host first if not.
2. Add a block under `[engines.<name>]`:

   ```toml
   [engines.my-engine]
   host = "spark3"
   kind = "tensorfold"                 # tensorfold | lfm2d | laya | jev-glm
   model = "The-Model-Id"              # the id the engine must report
   notes = "One line of things people need to know."
   supervisor_scope = "system"         # "user", "system", or "external"
   supervisor_unit = "my-engine.service"
   url = "http://192.168.0.213:9999"   # no trailing slash
   watchdog_id = ""                    # id in llm-watchdog services.json, or ""

   [engines.my-engine.expected_features]
   background_priority = "yes"
   guided_choice = "yes"
   logprobs = "no"
   prompt_logprobs = "no"
   ```

3. Run `fleet-check my-engine`. Read the FEATURES column and the reasons under
   the table. Set `expected_features` to what you want to be true (leave out
   a feature you have no opinion on; it is measured and shown but never
   flagged). Run it again; it should exit 0.
4. If the engine is shared with other people, add `shared = true`.
5. Run `fleet/check/test.sh`. It checks the file is well formed.

Kinds:

| kind | health | sanity |
|---|---|---|
| `tensorfold` | `/health`, model from `/v1/models` | chat question, plus the four probes |
| `laya` | `/health` | `/v1/systemone` refund example |
| `lfm2d` | `health_path` (`/v1/models`); optional `exporter_url` | `read = "specs"` or `"spans"` |
| `jev-glm` | `/health` and `/ready` | one `/v1/systemone` decision |

A new kind needs a new function in `fleet/check/fleet_check.py` and an entry
in `CHECKS`.

## Testing fleet-check itself

```
fleet/check/test.sh             # unit tests against stub HTTP servers; about a second
fleet/check/test.sh --selftest  # also breaks a copy of the code on purpose and
                                # requires the tests to go red
```

The tests cover a healthy engine, a down engine (exit 1), feature drift
(exit 2), a wrong answer (exit 1), the shared-engine skip, each engine kind,
and the shape of `fleet.toml`. They do **not** touch the real fleet; the
real check is running `fleet-check`.

## Caveats

- Silent ignore versus real support for `priority: "background"` cannot be
  told apart on engines that accept any value: GLM returns 200 even for
  `"bogus-value"`. `bg=yes` means "accepted", not "ordering verified".
- An engine's version is shown only where it reports one (Qwen does; GLM and
  DeepSeek do not expose it over HTTP).
- The check cannot see inside a machine. For supervisors, logs and restarts,
  use the unit named in `fleet.toml` and the llm-watchdog on hyper01.
