"""Read Claude Code and omp session logs as typed units.

    from fleetlib.logs import iter_units
    for u in iter_units(kinds={"shell_command"}):
        print(u.host, u.session_id, u.command)

Sources (``default_sources``):
  Claude Code  ~/.claude/projects/**/*.jsonl, which includes <session>/subagents/*.jsonl
  omp          ~/.omp/agent/sessions/**/*.jsonl, which includes sub-session files
                 <session>/<Name>.jsonl (they carry ``parentSession``)

Four kinds of unit come out, all with host, source, session id, timestamp and
cwd (``Unit`` below):

  UserMessage    text, ``human`` (True only for real human input) and ``category``:
                 human, tool_result, system_reminder, task_notification, command,
                 interrupt, meta, sdk, agent, summary, no_text, pasted_only, developer, other
  AssistantTurn  one assistant turn (everything between two user messages): text,
                 tool calls (name + short summary), stop reason, ended/interrupted
  ShellCommand   one Bash (Claude Code) or bash (omp) tool call
  ToolError      one failed tool call: tool, input, error text, exit code, error class

Units are redacted by default (``redact=True``) with fleetlib.redact, and each
unit carries ``redactions``, the number of spans masked. Truncation, if asked
for with ``max_chars``, happens after masking. Nothing here writes to disk.

Order: units come in file order, except that an AssistantTurn is emitted when
it closes (just before the user message that follows it, or at end of file).
Sort by ``timestamp`` if you need strict time order.

omp schema (read from real files, 10/20/2026): line 1 is ``title``, then
``session`` {id, cwd, timestamp, parentSession?}; ``message`` records have
message.role of user (attribution "user" = human, "agent" = machine-written),
developer, assistant (content blocks text/thinking/toolCall{id,name,arguments};
stopReason stop/toolUse/aborted/error/length) or toolResult (toolCallId,
toolName, isError, details{exitCode?}); ``custom_message`` {customType, content,
attribution} are injected context. Claude Code schema: ``type`` user/assistant/
attachment, message.content blocks text/tool_use/tool_result{is_error},
``isMeta``, ``isSidechain``, ``origin.kind``, ``entrypoint``, ``cwd``, ``sessionId``.
"""
from __future__ import annotations

import collections
import dataclasses
import json
import os
import re
import socket
from dataclasses import dataclass, field
from pathlib import Path
from typing import ClassVar, Iterable, Iterator, Optional

from .redact import redact_counts

CLAUDE = "claude-code"
OMP = "omp"

KINDS = ("user_message", "assistant_turn", "shell_command", "tool_error")


@dataclass(frozen=True)
class Source:
    host: str
    kind: str  # CLAUDE or OMP
    root: Path


def local_host() -> str:
    return socket.gethostname().split(".")[0]


def default_sources(host: Optional[str] = None, home: Optional[str] = None) -> "list[Source]":
    """This machine's Claude Code and omp logs."""
    h = host or local_host()
    home_p = Path(home) if home else Path.home()
    return [
        Source(h, CLAUDE, home_p / ".claude" / "projects"),
        Source(h, OMP, home_p / ".omp" / "agent" / "sessions"),
    ]


def raw_sources(raw_dir: "str | os.PathLike[str]") -> "list[Source]":
    """Logs copied from other machines into <raw_dir>/<host>/{claude,omp}
    (the layout jev-evals uses)."""
    out = []
    base = Path(raw_dir)
    if not base.is_dir():
        return out
    for hostdir in sorted(p for p in base.iterdir() if p.is_dir()):
        if (hostdir / "claude").is_dir():
            out.append(Source(hostdir.name, CLAUDE, hostdir / "claude"))
        if (hostdir / "omp").is_dir():
            out.append(Source(hostdir.name, OMP, hostdir / "omp"))
    return out


# ------------------------------------------------------------------ units


@dataclass(kw_only=True)
class Unit:
    kind: ClassVar[str] = ""
    host: str
    source: str
    session_id: Optional[str]
    timestamp: Optional[str]
    cwd: Optional[str]
    file: str = ""
    subagent: bool = False
    agent_id: Optional[str] = None
    redactions: int = 0
    truncated: bool = False

    # string fields that hold log content: masked and cut by ``finish``
    _TEXT_FIELDS: ClassVar[tuple] = ("cwd",)

    def to_dict(self) -> dict:
        d = dataclasses.asdict(self)
        d["kind"] = self.kind
        return d

    def finish(self, redact: bool, max_chars: Optional[int]) -> "Unit":
        """Return a copy with content fields masked (then cut to max_chars)."""
        changes: dict = {}
        total = 0
        cut = False
        for name in self._TEXT_FIELDS:
            v = getattr(self, name)
            if not isinstance(v, str):
                continue
            if redact:
                v, c = redact_counts(v)
                total += sum(c.values())
            if max_chars is not None and name != "cwd" and len(v) > max_chars:
                v, cut = v[:max_chars], True
            changes[name] = v
        if hasattr(self, "tool_calls"):
            calls = []
            for n, s in getattr(self, "tool_calls", ()):
                if redact:
                    s, c = redact_counts(s)
                    total += sum(c.values())
                calls.append((n, s))
            changes["tool_calls"] = tuple(calls)
        return dataclasses.replace(self, redactions=self.redactions + total, truncated=self.truncated or cut, **changes)


@dataclass(kw_only=True)
class UserMessage(Unit):
    kind: ClassVar[str] = "user_message"
    text: str
    human: bool
    category: str
    pasted_chars: int = 0
    queued: bool = False
    _TEXT_FIELDS: ClassVar[tuple] = ("cwd", "text")


@dataclass(kw_only=True)
class AssistantTurn(Unit):
    kind: ClassVar[str] = "assistant_turn"
    text: str = ""
    tool_calls: tuple = ()  # ((tool name, short input summary), ...), first 30
    tool_call_count: int = 0
    stop_reason: Optional[str] = None
    ended_turn: Optional[bool] = None  # True: model finished; False: it called a tool; None: unknown
    interrupted: bool = False
    _TEXT_FIELDS: ClassVar[tuple] = ("cwd", "text")


@dataclass(kw_only=True)
class ShellCommand(Unit):
    kind: ClassVar[str] = "shell_command"
    command: str = ""
    tool: str = ""
    _TEXT_FIELDS: ClassVar[tuple] = ("cwd", "command")


@dataclass(kw_only=True)
class ToolError(Unit):
    kind: ClassVar[str] = "tool_error"
    tool: Optional[str] = None
    tool_input: str = ""
    error_text: str = ""
    exit_code: Optional[int] = None
    error_class: str = "other"
    _TEXT_FIELDS: ClassVar[tuple] = ("cwd", "tool_input", "error_text")


# ------------------------------------------------------------------ text helpers

_WRAPPER_CATEGORY = (
    ("<task-notification", "task_notification"),
    ("<local-command", "command"),
    ("<command-name", "command"),
    ("<command-message", "command"),
    ("<bash-input", "command"),
    ("<bash-stdout", "command"),
    ("<bash-stderr", "command"),
    ("<system-reminder", "system_reminder"),
    ("Caveat:", "system_reminder"),
    ("[SYSTEM NOTIFICATION", "system_reminder"),
    ("<user-prompt-submit-hook", "system_reminder"),
    ("<artifact-content", "system_reminder"),
    ("[budget notice]", "system_reminder"),
    ("[Request interrupted", "interrupt"),
    ("<agent-message", "agent"),
    ("<teammate-message", "agent"),
    ("This session is being continued", "summary"),
)
_SYSTEM_REMINDER = re.compile(r"<system-reminder>.*?</system-reminder>", re.S)
_PASTE = re.compile(r"<pasted_content[^>]*>.*?</pasted_content>|<pasted_content[^>]*>.*\Z", re.S)
_IMAGE = re.compile(r"\[Image[^\]]*\]")


def blocks_text(content) -> str:
    """Text of a message content: a string, or the text blocks of a block list."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text" and isinstance(b.get("text"), str))
    return ""


def classify_user_text(text: str) -> "tuple[str, str, int]":
    """Decide whether a user-role text is real human input.
    Returns (cleaned_text, category, pasted_chars); category "human" means yes."""
    had_reminder = _SYSTEM_REMINDER.search(text) is not None
    t = _SYSTEM_REMINDER.sub("", text).strip()
    if not t:
        return text, "system_reminder" if had_reminder else "no_text", 0
    for prefix, cat in _WRAPPER_CATEGORY:
        if t.startswith(prefix):
            return t, cat, 0
    if len(re.findall(r"[A-Za-z]", _IMAGE.sub("", t))) < 2:
        return t, "no_text", 0
    pasted = sum(len(m.group(0)) for m in _PASTE.finditer(t))
    t2 = _PASTE.sub("", t).strip()
    if pasted and len(t2) < 3:
        return t, "pasted_only", pasted
    return t2, "human", pasted


def summarize_tool(name: str, inp) -> str:
    """Short one-line view of a tool call's input (first 200 characters)."""
    if not isinstance(inp, dict):
        return str(inp)[:200]
    for k in ("command", "file_path", "path", "pattern", "url", "query", "description", "prompt"):
        if isinstance(inp.get(k), str):
            return inp[k][:200]
    return json.dumps(inp, sort_keys=True)[:200]


def exit_code_of(text: str, details=None) -> Optional[int]:
    if isinstance(details, dict) and isinstance(details.get("exitCode"), int):
        return details["exitCode"]
    m = re.search(r"(?:Exit code|exited with code|exit code)[: ]+(\d+)", text or "")
    return int(m.group(1)) if m else None


def classify_error(tool: Optional[str], text: str, exit_code: Optional[int]) -> str:
    """Bucket a tool error: hook_block, permission_denied, skipped, aborted,
    user_rejected, edit_mismatch, file_not_found, timeout, nonzero_exit,
    invalid_args, service_failure, tool_misuse, other."""
    t = text or ""
    low = t.lower()
    if "PreToolUse" in t and "hook error" in t:
        return "hook_block"
    if ("auto mode classifier" in t or "Permission for this action" in t or "Permission to use" in t
            or "requested permissions" in t or "permission was denied" in low):
        return "permission_denied"
    if "Skipped due to" in t:
        return "skipped"
    if "Command aborted" in t:
        return "aborted"
    if "doesn't want to proceed" in t or "cancelled by the user" in t or "user rejected" in low or "user denied" in low:
        return "user_rejected"
    if ("String to replace not found" in t or "has been modified since" in t or "File has been" in t
            or ("oldText" in t and "not found" in low) or "did not match" in low
            or ("Could not find" in t and tool in ("edit", "Edit"))):
        return "edit_mismatch"
    if "does not exist" in t or "No such file or directory" in t or "ENOENT" in t:
        return "file_not_found"
    if re.search(r"timed out|timeout", t[:300], re.I):
        return "timeout"
    if exit_code not in (None, 0):
        return "nonzero_exit"
    if re.search(r"Validation failed|Invalid args|Invalid todo|must begin with|rejected:|payload line|does not match schema|Input header|not found\. Available|anchors to lines", t):
        return "invalid_args"
    if "MCP failure" in t or "request failed after" in t or "could not complete" in t:
        return "service_failure"
    if "<tool_use_error>" in t or "Blocked:" in t[:80]:
        return "tool_misuse"
    return "other"


class _Turn:
    """Assistant records collected since the last user message."""

    def __init__(self) -> None:
        self.texts: list[str] = []
        self.tools: list[tuple[str, str]] = []
        self.n_tools = 0
        self.stop: Optional[str] = None
        self.interrupted = False
        self.first_ts: Optional[str] = None
        self.sub = False

    def add(self, ts, text, tools, stop, sub) -> None:
        if self.first_ts is None:
            self.first_ts = ts
        self.sub = self.sub or sub
        if text:
            self.texts.append(text)
        for t in tools:
            self.n_tools += 1
            if len(self.tools) < 30:
                self.tools.append(t)
        if stop is not None or text or tools:
            self.stop = stop

    def empty(self) -> bool:
        return not self.texts and not self.tools


def _ended_claude(stop):
    return True if stop == "end_turn" else (False if stop == "tool_use" else None)


def _ended_omp(stop):
    return True if stop == "stop" else (False if stop == "toolUse" else None)


# ------------------------------------------------------------------ parsers


class _Ctx:
    def __init__(self, src: Source, path: Path, stats) -> None:
        self.src = src
        self.path = path
        self.stats = stats
        self.sid: Optional[str] = None
        self.cwd: Optional[str] = None
        self.agent_id: Optional[str] = None
        self.subagent = False
        self.out: list[Unit] = []

    def common(self, ts, sub=None) -> dict:
        return dict(host=self.src.host, source=self.src.kind, session_id=self.sid, timestamp=ts, cwd=self.cwd,
                    file=str(self.path), subagent=self.subagent if sub is None else sub, agent_id=self.agent_id)

    def flush(self, turn: _Turn, ended) -> None:
        if turn.empty():
            return
        self.out.append(AssistantTurn(
            text="\n\n".join(turn.texts), tool_calls=tuple(turn.tools), tool_call_count=turn.n_tools,
            stop_reason=turn.stop, ended_turn=ended(turn.stop),
            interrupted=turn.interrupted, **self.common(turn.first_ts, turn.sub)))


def _json_lines(path: Path, stats) -> Iterator[dict]:
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                d = json.loads(line)
            except ValueError:
                stats["bad_lines"] += 1
                continue
            if isinstance(d, dict):
                yield d


def _parse_claude(src: Source, path: Path, stats, include_tool_results: bool) -> "list[Unit]":
    c = _Ctx(src, path, stats)
    rel = path.relative_to(src.root).parts if src.root in path.parents else path.parts
    file_sub = "subagents" in rel or path.name.startswith("agent-")
    c.subagent = file_sub
    c.sid = None
    turn = _Turn()
    tool_names: dict = {}
    for d in _json_lines(path, stats):
        c.sid = c.sid or d.get("sessionId")
        c.cwd = d.get("cwd") or c.cwd
        c.agent_id = c.agent_id or d.get("agentId")
        ty = d.get("type")
        ts = d.get("timestamp")
        sub = file_sub or bool(d.get("isSidechain"))
        if ty == "assistant" and isinstance(d.get("message"), dict):
            m = d["message"]
            texts, tools = [], []
            for b in m.get("content") or []:
                if not isinstance(b, dict):
                    continue
                if b.get("type") == "text" and isinstance(b.get("text"), str):
                    texts.append(b["text"])
                elif b.get("type") == "tool_use":
                    inp = b.get("input") or {}
                    summ = summarize_tool(b.get("name", ""), inp)
                    tool_names[b.get("id")] = (b.get("name"), summ)
                    tools.append((b.get("name", ""), summ))
                    if b.get("name") == "Bash" and isinstance(inp, dict) and isinstance(inp.get("command"), str) and inp["command"].strip():
                        c.out.append(ShellCommand(command=inp["command"], tool="Bash", **c.common(ts, sub)))
                        stats["shell_commands"] += 1
            turn.add(ts, "\n".join(texts), tools, m.get("stop_reason"), sub)
        elif ty == "user" and isinstance(d.get("message"), dict):
            content = d["message"].get("content")
            results = [b for b in content if isinstance(b, dict) and b.get("type") == "tool_result"] if isinstance(content, list) else []
            if results:
                for b in results:
                    t = b["content"] if isinstance(b.get("content"), str) else blocks_text(b.get("content") or [])
                    nm, summ = tool_names.get(b.get("tool_use_id"), (None, ""))
                    stats["tool_results"] += 1
                    if include_tool_results:
                        c.out.append(UserMessage(text=t, human=False, category="tool_result", **c.common(ts, sub)))
                    if b.get("is_error"):
                        code = exit_code_of(t)
                        stats["tool_errors"] += 1
                        c.out.append(ToolError(tool=nm, tool_input=summ, error_text=t, exit_code=code,
                                               error_class=classify_error(nm, t, code), **c.common(ts, sub)))
                continue
            text = blocks_text(content)
            if text.startswith("[Request interrupted"):
                turn.interrupted = True
                c.out.append(UserMessage(text=text, human=False, category="interrupt", **c.common(ts, sub)))
                continue
            origin = d.get("origin") if isinstance(d.get("origin"), dict) else {}
            okind = origin.get("kind")
            cleaned, cat, pasted = classify_user_text(text)
            if sub:
                cat = "agent"
            elif d.get("isMeta"):
                cat = "meta"
            elif d.get("entrypoint") == "sdk-cli":
                cat = "sdk"
            elif okind == "task-notification":
                cat = "task_notification"
            elif okind not in (None, "human") and cat == "human":
                cat = "other"
            c.flush(turn, _ended_claude)
            turn = _Turn()
            c.out.append(UserMessage(text=cleaned, human=(cat == "human"), category=cat, pasted_chars=pasted, **c.common(ts, sub)))
            stats["human_messages" if cat == "human" else "other_user_messages"] += 1
        elif ty == "attachment":
            a = d.get("attachment") or {}
            if a.get("type") == "queued_command" and a.get("commandMode") == "prompt" and isinstance(a.get("prompt"), str):
                okind = (a.get("origin") or {}).get("kind")
                cleaned, cat, pasted = classify_user_text(a["prompt"])
                if sub:
                    cat = "agent"
                elif d.get("entrypoint") == "sdk-cli":
                    cat = "sdk"
                elif okind == "task-notification":
                    cat = "task_notification"
                elif okind != "human" and cat == "human":
                    cat = "other"
                c.flush(turn, _ended_claude)
                turn = _Turn()
                c.out.append(UserMessage(text=cleaned, human=(cat == "human"), category=cat, pasted_chars=pasted, queued=True,
                                         **c.common(a.get("timestamp") or ts, sub)))
                stats["human_messages" if cat == "human" else "other_user_messages"] += 1
    c.flush(turn, _ended_claude)
    return _drop_queued_duplicates(c.out, stats)


def _drop_queued_duplicates(units: "list[Unit]", stats) -> "list[Unit]":
    """A message typed while the model was busy is logged twice: once as the
    queued prompt and once as the user message. Keep the typed one."""
    typed = {(u.session_id, u.text) for u in units if isinstance(u, UserMessage) and u.human and not u.queued}
    out = []
    for u in units:
        if isinstance(u, UserMessage) and u.queued and u.human and (u.session_id, u.text) in typed:
            stats["queued_duplicates_dropped"] += 1
            continue
        out.append(u)
    return out


_OMP_NOTIFICATION_TYPES = ("async-result", "launch-completion", "irc:incoming", "goal-continuation")


def _parse_omp(src: Source, path: Path, stats, include_tool_results: bool) -> "list[Unit]":
    c = _Ctx(src, path, stats)
    turn = _Turn()
    tool_names: dict = {}
    for d in _json_lines(path, stats):
        ty = d.get("type")
        ts = d.get("timestamp")
        if ty == "session":
            c.sid = d.get("id")
            c.cwd = d.get("cwd")
            c.subagent = bool(d.get("parentSession"))
            continue
        if ty == "custom_message":
            ct = d.get("customType") or ""
            cat = "task_notification" if ct in _OMP_NOTIFICATION_TYPES else "system_reminder"
            c.flush(turn, _ended_omp)
            turn = _Turn()
            c.out.append(UserMessage(text=blocks_text(d.get("content")), human=False, category=cat, **c.common(ts)))
            stats["other_user_messages"] += 1
            continue
        if ty != "message" or not isinstance(d.get("message"), dict):
            continue
        m = d["message"]
        role = m.get("role")
        if role == "assistant":
            texts, tools = [], []
            for b in m.get("content") or []:
                if not isinstance(b, dict):
                    continue
                if b.get("type") == "text" and isinstance(b.get("text"), str):
                    texts.append(b["text"])
                elif b.get("type") == "toolCall":
                    args = b.get("arguments") or {}
                    summ = summarize_tool(b.get("name", ""), args)
                    tool_names[b.get("id")] = (b.get("name"), summ)
                    tools.append((b.get("name", ""), summ))
                    if b.get("name") == "bash" and isinstance(args, dict) and isinstance(args.get("command"), str) and args["command"].strip():
                        u = ShellCommand(command=args["command"], tool="bash", **c.common(ts))
                        if isinstance(args.get("cwd"), str) and args["cwd"].startswith("/"):
                            u.cwd = args["cwd"]
                        c.out.append(u)
                        stats["shell_commands"] += 1
            turn.add(ts, "\n".join(texts), tools, m.get("stopReason"), c.subagent)
            if m.get("stopReason") == "aborted":
                turn.interrupted = True
        elif role == "toolResult":
            t = blocks_text(m.get("content") or [])
            details = m.get("details")
            code = exit_code_of(t, details)
            stats["tool_results"] += 1
            if include_tool_results:
                c.out.append(UserMessage(text=t, human=False, category="tool_result", **c.common(ts)))
            nonzero = m.get("toolName") == "bash" and isinstance(details, dict) and isinstance(details.get("exitCode"), int) and details["exitCode"] != 0
            if m.get("isError") or nonzero:
                nm, summ = tool_names.get(m.get("toolCallId"), (m.get("toolName"), ""))
                stats["tool_errors"] += 1
                c.out.append(ToolError(tool=nm or m.get("toolName"), tool_input=summ, error_text=t, exit_code=code,
                                       error_class=classify_error(nm, t, code), **c.common(ts)))
        elif role in ("user", "developer"):
            cleaned, cat, pasted = classify_user_text(blocks_text(m.get("content")))
            if role == "developer":
                cat = "developer"
            elif m.get("attribution") != "user":
                cat = "agent"
            c.flush(turn, _ended_omp)
            turn = _Turn()
            c.out.append(UserMessage(text=cleaned, human=(cat == "human"), category=cat, pasted_chars=pasted, **c.common(ts)))
            stats["human_messages" if cat == "human" else "other_user_messages"] += 1
    c.flush(turn, _ended_omp)
    return c.out


# ------------------------------------------------------------------ public API


def iter_files(source: Source) -> "list[Path]":
    """All session files under a source root, sorted."""
    if not source.root.is_dir():
        return []
    return sorted(source.root.glob("**/*.jsonl"))


def iter_units(
    sources: Optional[Iterable[Source]] = None,
    *,
    kinds: Optional[Iterable[str]] = None,
    redact: bool = True,
    max_chars: Optional[int] = None,
    since: Optional[str] = None,
    human_only: bool = False,
    include_tool_results: bool = False,
    stats: "Optional[collections.Counter]" = None,
) -> Iterator[Unit]:
    """Yield units from the given sources (default: this machine).

    kinds                subset of KINDS to yield (default all four)
    redact               mask secrets in every unit (default True; turn off only
                         for in-memory work that never leaves the process)
    max_chars            cut text fields to this length, after masking
    since                ISO timestamp; skip units stamped before it
    human_only           yield only user messages with human=True
    include_tool_results also yield tool results as UserMessage(category="tool_result")
    stats                a Counter that collects files, bad_lines, counts of things seen
    """
    st = stats if stats is not None else collections.Counter()
    want = set(kinds) if kinds else set(KINDS)
    bad = want - set(KINDS)
    if bad:
        raise ValueError(f"unknown unit kinds: {sorted(bad)}")
    for src in (list(sources) if sources is not None else default_sources()):
        parse = _parse_claude if src.kind == CLAUDE else _parse_omp
        for path in iter_files(src):
            st["files"] += 1
            for u in parse(src, path, st, include_tool_results):
                if u.kind not in want:
                    continue
                if human_only and not (isinstance(u, UserMessage) and u.human):
                    continue
                if since and (u.timestamp or "") < since:
                    continue
                yield u.finish(redact, max_chars)
