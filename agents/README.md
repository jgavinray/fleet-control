# fleet-worker

## What it is

`fleet-worker` is a Claude Code agent definition (`fleet-worker.md`). It holds
the owner's standing rules once: verification, git, fleet safety, secrets,
model use, scope, reporting and memory. A worker started from it already knows
them, so a brief only needs the task.

It runs on Sonnet (`model: sonnet`) at high effort (`effort: high`, the
frontmatter key Claude Code documents for this) and has every tool (the
`tools` line is left out on purpose, which means all tools).

## Install

    agents/install.sh

It links `~/.claude/agents/fleet-worker.md` to the file in this repo and says
what it did. Running it again changes nothing. It refuses to overwrite a real
file at that path. After the first install you never need to run it again.

## How to use it

Start a worker with `subagent_type: "fleet-worker"`. Write only the task, the
gate command that proves it is done, and the signoff line format. Do not
repeat the rules; if a brief and a rule disagree, the worker follows the rule
and says so.

## How to change a rule

Edit `agents/fleet-worker.md` in this repo, commit, push. The install
is a symlink, so `~/.claude/agents/fleet-worker.md` shows the change at once.
There is no reinstall. (Whether an already-running Claude Code session picks
up the edit without a restart has not been checked here.) Keep the prompt short and ranked; put detail in the source file
the rule links to, not in the prompt.

## Where each rule comes from

| Rule | Source |
|---|---|
| Scope: exact task, investigations change nothing | `~/exomemory/feedback/stay-in-authorized-scope.md`, `no-changes-without-permission.md` |
| Stop on destructive or refused actions | `~/exomemory/feedback/ship-verified-work.md` (the "still stop" list) |
| No legal conclusions | `~/exomemory/feedback/no-legal-assumptions.md` |
| Verify claims, label unverified | `~/exomemory/feedback/never-assert-from-memory.md` |
| Verification and gates | `~/.claude/CLAUDE.md` (Verification); `~/exomemory/CLAUDE.md` ("Proper gates are required") |
| No GPG, `-c commit.gpgsign=false` | `~/exomemory/CLAUDE.md` (Standing Rules); `feedback/never-gpg-sign.md` |
| No attribution trailers | `~/.claude/CLAUDE.md` (Git); `feedback/no-coauthored-by-trailer.md` |
| Push only to the owner's remotes, never rewrite pushed history | `~/.claude/CLAUDE.md` (Git); `feedback/ship-verified-work.md` |
| Third-party remotes (Agent-Quest `origin`, DeepSeek and GLM recipes, localjev, jevlike) | `~/exomemory/signoff.md` ("If you read nothing else"); `wiki/agent-quest.md` |
| Separate worktree when the main checkout is busy; rebase on rejected push | The owner's brief for this agent (10/20/2026). No exomemory file states it. |
| LAN not Tailscale | `feedback/use-lan-not-tailscale.md`; Tailscale names in `fleet.toml`, LAN addresses in the untracked `~/.config/fleet-control/local.toml` |
| hyper03 is shared | `feedback/hyper03-tensorfold-paused.md`. The NVIDIA-cards, reboot and Qwen3.8-27B (:8081) wording is the owner's ruling of 10/21/2026 (relayed by the coordinator); no exomemory file states it. |
| Never read `~/.bashrc` on hyper01 | `~/exomemory/tasks/done/laya-fix.md` (it holds Hugging Face tokens) |
| Stop services via their supervisor, pause the watchdog | `feedback/find-supervisors-before-stopping.md`; `wiki/llm-watchdog.md` |
| No large image builds or copies while a model is loaded | `~/exomemory/signoff.md` (fleet facts, GLM entry) |
| Secrets: never print, mask, never commit | `~/.claude/CLAUDE.md`; masking helper is `fleetlib.redact` in `lib/` |
| `priority: "background"`, `guided_choice` needs thinking off, DeepSeek rejects `guided_choice` | `wiki/glm53-tensorfold-upgrade.md`; `wiki/local-jev-research.md`; `signoff.md` fleet facts |
| Plain words, short replies | `feedback/plain-words-with-context.md`; `feedback/user-wants-action.md` |
| How-to doc for every operational change | `feedback/sonnet-high-for-agents.md` ("Reinforced" note) |
| One signoff line, append only | `~/exomemory/CLAUDE.md` (Standing Rules, Session Protocol) |
| Memory is only exomemory | `~/exomemory/CLAUDE.md`; `~/.claude/CLAUDE.md` (Memory) |
| Sonnet at high effort | `feedback/sonnet-high-for-agents.md` |

`fleet.toml` and `lib/` (`fleetlib.redact`) are written by other
workers. The agent only points at them.

## What is not checked yet

Having the file in `~/.claude/agents/` and valid frontmatter does not prove a
spawned worker loads it. The first real spawn with `subagent_type:
"fleet-worker"` is that check.
