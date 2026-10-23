# fleet-control: the fleet registry, fleet-check and the fleet-worker agent

This repository was split out of `mission-control` (its former `fleet/`
directory, with history) on 10/22/2026. `lib/` is the shared Python library
(`fleetlib`) and `agents/` holds the `fleet-worker` agent definition and its
installer; both have their own README.

## Install

```
git clone git@github.com:jgavinray/fleet-control.git ~/dev/fleet-control
ln -sfn ~/dev/fleet-control/check/fleet-check ~/.local/bin/fleet-check
~/dev/fleet-control/agents/install.sh      # links ~/.claude/agents/fleet-worker.md
```

Tests: `check/test.sh` and `lib/test.sh` (Python 3.11 or newer, standard
library only).

## The registry and fleet-check

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

`~/.local/bin/fleet-check` is a symlink to `check/fleet-check` in this
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

## The local LAN override (untracked)

The owner's standing rule is to connect over the home LAN, not Tailscale
(Tailscale SSH once muddied a forensics trace). Since the repo holds
Tailscale names only, each machine that should use the LAN keeps a small
file outside the repo, `~/.config/fleet-control/local.toml` (or the file
named by `$FLEET_LOCAL`):

```toml
# Never commit this file.
[hosts.hyper01]
address = "192.0.2.10"     # this host's LAN address (example value)

[hosts.spark3]
address = "192.0.2.11"
```

A URL whose host is `hyper01` or `hyper01.<tailnet>.ts.net` is rewritten to
that address before it is used, by both `fleet-check` and `fleetlib`. Hosts
with no entry keep their Tailscale name. If the file is missing, nothing
changes; if it is not valid TOML it is ignored with a warning. To check the
Tailscale route on purpose, run with `FLEET_LOCAL=/dev/null`.

## The registry: `fleet.toml`

Two sections: `[engines.*]` and `[hosts.*]`. The comments in the file say what
each field means. Rules:

- Keys are sorted and there are no timestamps, so a diff shows only real
  changes. A test checks this.
- Base URLs with Tailscale names (FQDNs) only. No keys and no IP addresses
  of any kind; the repo is public. `check/test.sh` fails if a tracked file
  holds a private (RFC 1918) or Tailscale (CGNAT, 100.64/10) address, or the name
  of another person who uses a host.
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
   url = "http://spark3.<tailnet>.ts.net:9999"   # no trailing slash; the host's fqdn
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
5. Run `check/test.sh`. It checks the file is well formed.

Kinds:

| kind | health | sanity |
|---|---|---|
| `tensorfold` | `/health`, model from `/v1/models` | chat question, plus the four probes |
| `laya` | `/health` | `/v1/systemone` refund example |
| `lfm2d` | `health_path` (`/v1/models`); optional `exporter_url` | `read = "specs"` or `"spans"` |
| `jev-glm` | `/health` and `/ready` | one `/v1/systemone` decision |

A new kind needs a new function in `check/fleet_check.py` and an entry
in `CHECKS`.

## Testing fleet-check itself

```
check/test.sh             # unit tests against stub HTTP servers; about a second
check/test.sh --selftest  # also breaks a copy of the code on purpose and
                                # requires the tests to go red
```

The tests cover a healthy engine, a down engine (exit 1), feature drift
(exit 2), a wrong answer (exit 1), the shared-engine skip, each engine kind,
and the shape of `fleet.toml`. They do **not** touch the real fleet; the
real check is running `fleet-check`.

## The Rust workspace (fleet-core and fleet-check-rs)

The repo root is also a cargo workspace: `crates/fleet-core` is the library
(the registry loader with the local LAN override, the counting HTTP client,
the result and verdict types, the per-kind engine checks and the feature
probes), and `crates/fleet-check-rs` is the same tool as `check/fleet_check.py`
as a compiled binary.

**Status.** The port is additive and is not installed anywhere. The `ln -sfn`
install above still points at the Python launcher; the Python tool remains the
operating tool until the owner rules on a cutover. Nothing in `stopgap/` or
`agents/` changes.

**Build, test, gate.** From the repo root:

```
cargo build --release      # binary at target/release/fleet-check-rs
cargo test
bash crates/gate.sh
```

The gate builds in release mode, requires at least 8 tests per crate, runs
`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`, then runs
`crates/parity.py`: a 154-comparison mock-engine harness that runs the Python
and Rust tools side by side on loopback and compares exit codes, engine order
and the comparable JSON fields (any stderr on a normal run fails). No live
engine is contacted.

**Parity.** The Rust tool matches the Python on every scenario in
`crates/parity.py`. Message wording may differ; counts, fields and exit codes
do not. Replies containing `NaN`, `Infinity` or `-Infinity` tokens parse
exactly as Python's `json.loads` accepts them, and request bodies identify as
Python-urllib/3.14.

**Known divergences from the Python tool.**

- A timeout applies to the whole request in the Rust tool (ureq's global
  deadline) rather than to each socket operation as in Python: a response
  that trickles in indefinitely in small chunks times out in Rust where
  Python would succeed.
- Error-message and note text may differ in wording; the gate compares
  problem and note counts, not text.
- Request bodies are serialized with sorted keys.
- Where the Python would crash on a malformed reply or registry (uncaught
  `TypeError`, `KeyError`, `AttributeError` or `ValueError`), the Rust tool
  reports the failure through its normal statuses and exit code 1 instead of
  a traceback; the exit code matches.

**Requirements.** A Rust toolchain (the port is built with 1.92); `serde_json`,
`toml` and `ureq` are fetched from crates.io on the first build. The Python
side stays Python 3.11+ standard-library only.

## Caveats

- Silent ignore versus real support for `priority: "background"` cannot be
  told apart on engines that accept any value: GLM returns 200 even for
  `"bogus-value"`. `bg=yes` means "accepted", not "ordering verified".
- An engine's version is shown only where it reports one (Qwen does; GLM and
  DeepSeek do not expose it over HTTP).
- The check cannot see inside a machine. For supervisors, logs and restarts,
  use the unit named in `fleet.toml` and the llm-watchdog on hyper01.

## Stopgap uptime alert (Mac)

`stopgap/` is a small, disposable watcher so the owner is told when an engine is
down, without having to ask. Step 0 of `fleet-uptime-design.md`; the planned
`fleet-watch` daemon replaces it.

**What it does.** A LaunchAgent runs `~/.local/bin/fleet-check --quick --json`
every 2 minutes on the Mac. `--quick` skips the feature probes, so it stays
gentle on hyper03 and on Qwen, which serves one request at a time. Each run:

- appends one line to the history file (see below);
- keeps the last 5 results per engine, where failed means `down` or `wrong`
  (`slow`, `busy` and feature drift do not count);
- shows a desktop notification when an engine failed **3 of its last 5 runs**,
  once per outage, naming the engine and the first problem line;
- shows one "back up" notification after **2 passes in a row**, then clears that
  engine's window. A flapping engine (down, up, down, up) never gets two passes
  in a row, so it is one alert, not many;
- if several engines cross the line in the same run, sends one notice naming all;
- if `fleet-check` itself cannot run (Mac offline, crash), logs an `error` line
  and says nothing: that run counts for nobody.

**Where things are.**

| What | Where |
|---|---|
| History, one JSON line per run | `~/.local/state/fleet-stopgap/history.jsonl` |
| Per-engine window and open alerts | `~/.local/state/fleet-stopgap/state.json` |
| launchd output and errors | `~/.local/state/fleet-stopgap/launchd.log` |
| Installed copy of the script | `~/.local/libexec/fleet-stopgap/fleet-stopgap.py` |
| The LaunchAgent | `~/Library/LaunchAgents/com.jgavinray.fleet-stopgap.plist` |

**Was it up overnight?** Each history line has `ts`, `result` (`ok`, `fail` or
`error`), the status of every engine, the first problem for each failing one,
and any `alert:` / `recovered:` events. For example, every run since 10 PM that
was not fully healthy:

```
jq -c 'select(.ts > "2026-10-21T22:00" and .result != "ok") | {ts, result, engines}' \
  ~/.local/state/fleet-stopgap/history.jsonl
```

About 720 lines a day, roughly 300 KB.

**Install, stop, restart.**

```
stopgap/install.sh            # install or reinstall, and start (also runs once at once)
stopgap/install.sh uninstall  # stop and remove; history is kept
launchctl bootout gui/$(id -u)/com.jgavinray.fleet-stopgap      # stop now (it comes back at next login)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.jgavinray.fleet-stopgap.plist   # start again
launchctl print gui/$(id -u)/com.jgavinray.fleet-stopgap        # is it loaded, when did it last run
```

**Tests.** `stopgap/test.sh` fakes `fleet-check` and the notifier (no engine is
touched) and checks: no alert on 1 or 2 failures in 5, an alert at 3, no repeat
alert while it continues, exactly one alert for a flapping engine, one recovery
note, nothing on a `fleet-check` crash. `stopgap/test.sh --selftest` also breaks
copies of the script six ways and requires the tests to go red each time.

**Known gaps.** It only runs while the Mac is awake and online, and it sees what
`fleet-check` sees: it cannot tell a driver or disk cause from a crash. The first
run after loading the LaunchAgent on 10/22/2026 saw four engines as "No route to
host" while a terminal run a minute later saw them all up (cause not
established; the 3-of-5 rule absorbs a single blip like this).

## License

GPL-2.0-only. The full text is in `LICENSE`.
