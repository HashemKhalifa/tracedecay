#!/usr/bin/env python3
"""Score one hermetic-eval scenario from an isolated agent transcript.

For Claude/Sonnet runs, reads the scenario JSON, the
``claude -p --output-format json`` result (to recover the session id), then
locates that session's transcript inside the ISOLATED ``CLAUDE_CONFIG_DIR``.
For Codex runs, reads the JSONL emitted by ``codex exec --json``.

Both paths count MCP tool names and CLI command strings. Scenarios may require
specific tracedecay MCP tools via ``expected_tools`` and CLI fallbacks via
``expected_cli``.

Pass criteria (deliberately simple; the harness is about isolation, not a
sophisticated judge):

* all expected MCP tool fragments were seen, if ``expected_tools`` is present,
* all expected CLI fragments were seen, if ``expected_cli`` is present,
* otherwise at least one tracedecay MCP tool was used, AND
* no ``anti_tools`` were used.

Emits a single JSON object on stdout.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load_scenario(raw: str) -> dict:
    return json.loads(raw)


def session_id_from_claude_json(path: Path) -> str | None:
    """Recover the session id from the `claude -p --output-format json` result."""
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
    except (json.JSONDecodeError, OSError):
        return None
    if isinstance(data, dict):
        for key in ("session_id", "sessionId", "session"):
            val = data.get(key)
            if isinstance(val, str) and val:
                return val
    return None


def project_slug(cwd: str) -> str:
    """Claude Code stores transcripts under projects/<slug> where slug is the
    absolute cwd with path separators replaced by dashes."""
    return cwd.replace("/", "-")


def find_transcript(config_dir: Path, cwd: str, session_id: str | None) -> Path | None:
    """Locate the JSONL transcript for this session inside the isolated config."""
    projects = config_dir / "projects"
    candidates: list[Path] = []

    if session_id:
        # Fast path: <config>/projects/<slug>/<session_id>.jsonl
        slug_dir = projects / project_slug(cwd)
        direct = slug_dir / f"{session_id}.jsonl"
        if direct.exists():
            return direct
        candidates.extend(projects.rglob(f"{session_id}.jsonl"))
        if candidates:
            return candidates[0]

    # Fallback: newest transcript under the matching project slug.
    slug_dir = projects / project_slug(cwd)
    if slug_dir.is_dir():
        jsonls = sorted(
            slug_dir.glob("*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True
        )
        if jsonls:
            return jsonls[0]

    # Last resort: newest transcript anywhere in the isolated config.
    all_jsonls = sorted(
        projects.rglob("*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True
    ) if projects.is_dir() else []
    return all_jsonls[0] if all_jsonls else None


def is_tracedecay_tool(name: str) -> bool:
    n = name.lower()
    return "tracedecay" in n


def command_from_value(value) -> str | None:
    if isinstance(value, str):
        return value
    if isinstance(value, list) and all(isinstance(item, str) for item in value):
        return " ".join(value)
    return None


def count_claude_tools(transcript: Path) -> tuple[list[str], list[str], list[str]]:
    """Return (tracedecay_tool_names, native_tool_names, commands)."""
    td: list[str] = []
    native: list[str] = []
    commands: list[str] = []
    try:
        lines = transcript.read_text().splitlines()
    except OSError:
        return td, native, commands

    for ln in lines:
        ln = ln.strip()
        if not ln:
            continue
        try:
            evt = json.loads(ln)
        except json.JSONDecodeError:
            continue
        # tool_use entries live in message.content blocks of assistant messages.
        msg = evt.get("message") if isinstance(evt, dict) else None
        content = msg.get("content") if isinstance(msg, dict) else None
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") != "tool_use":
                continue
            name = block.get("name", "")
            if not isinstance(name, str):
                continue
            if is_tracedecay_tool(name):
                td.append(name)
            else:
                native.append(name)
            tool_input = block.get("input")
            if isinstance(tool_input, dict):
                for key in ("command", "cmd"):
                    command = command_from_value(tool_input.get(key))
                    if command:
                        commands.append(command)
                        break
    return td, native, commands


def collect_codex_evidence(value, tools: list[str], commands: list[str]) -> None:
    if isinstance(value, dict):
        for key in ("name", "tool_name"):
            name = value.get(key)
            if isinstance(name, str):
                lower = name.lower()
                if (
                    "tracedecay" in lower
                    or "tool" in str(value.get("type", "")).lower()
                    or lower in {"bash", "shell", "exec_command", "apply_patch"}
                ):
                    tools.append(name)
        for key in ("cmd", "command", "shell_command"):
            command = command_from_value(value.get(key))
            if command:
                commands.append(command)
        for child in value.values():
            collect_codex_evidence(child, tools, commands)
    elif isinstance(value, list):
        for child in value:
            collect_codex_evidence(child, tools, commands)


def count_codex_tools(jsonl_path: Path) -> tuple[list[str], list[str], list[str]]:
    td: list[str] = []
    native: list[str] = []
    commands: list[str] = []
    try:
        lines = jsonl_path.read_text().splitlines()
    except OSError:
        return td, native, commands

    tools: list[str] = []
    for ln in lines:
        ln = ln.strip()
        if not ln:
            continue
        try:
            event = json.loads(ln)
        except json.JSONDecodeError:
            continue
        collect_codex_evidence(event, tools, commands)

    for name in tools:
        if is_tracedecay_tool(name):
            td.append(name)
        else:
            native.append(name)
    return td, native, commands


def fragment_missing(fragment: str, values: list[str]) -> bool:
    needle = fragment.lower()
    return not any(needle in value.lower() for value in values)


def evaluate_scenario(
    scenario: dict,
    session_id: str | None,
    transcript: Path | None,
    td_tools: list[str],
    native_tools: list[str],
    commands: list[str],
) -> dict:
    anti = {t.lower() for t in scenario.get("anti_tools", [])}
    all_tools = td_tools + native_tools
    expected_tools = scenario.get("expected_tools", [])
    expected_cli = scenario.get("expected_cli", [])

    missing_tools = [
        fragment for fragment in expected_tools if fragment_missing(fragment, all_tools)
    ]
    missing_cli = [
        fragment for fragment in expected_cli if fragment_missing(fragment, commands)
    ]
    used_anti = sorted(
        {n for n in native_tools if n.lower() in anti}
        | {n for n in native_tools if any(a in n.lower() for a in anti)}
        | {cmd for cmd in commands if any(a in cmd.lower() for a in anti)}
    )

    if expected_tools or expected_cli:
        passed = not missing_tools and not missing_cli and not used_anti
    else:
        passed = bool(td_tools) and not used_anti

    return {
        "id": scenario.get("id", ""),
        "category": scenario.get("category", ""),
        "session_id": session_id,
        "transcript": str(transcript) if transcript else None,
        "tracedecay_tool_uses": len(td_tools),
        "tracedecay_tools": td_tools,
        "native_tool_uses": len(native_tools),
        "native_tools": native_tools,
        "cli_command_uses": len(commands),
        "cli_commands": commands,
        "expected_tools_missing": missing_tools,
        "expected_cli_missing": missing_cli,
        "anti_tools_used": used_anti,
        "pass": passed,
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--agent", choices=("claude", "codex"), default="claude")
    ap.add_argument("--scenario", required=True, help="scenario JSON (one line)")
    ap.add_argument("--claude-json", help="path to claude -p json result")
    ap.add_argument("--codex-jsonl", help="path to codex exec --json output")
    ap.add_argument("--config-dir", help="isolated CLAUDE_CONFIG_DIR")
    ap.add_argument("--cwd", required=True, help="cwd the scenario ran in")
    args = ap.parse_args()

    scenario = load_scenario(args.scenario)
    sid = None
    transcript = None
    td_tools: list[str] = []
    native_tools: list[str] = []
    commands: list[str] = []

    if args.agent == "claude":
        if not args.claude_json or not args.config_dir:
            ap.error("--agent claude requires --claude-json and --config-dir")
        sid = session_id_from_claude_json(Path(args.claude_json))
        transcript = find_transcript(Path(args.config_dir), args.cwd, sid)
        if transcript is not None:
            td_tools, native_tools, commands = count_claude_tools(transcript)
    else:
        if not args.codex_jsonl:
            ap.error("--agent codex requires --codex-jsonl")
        transcript = Path(args.codex_jsonl)
        td_tools, native_tools, commands = count_codex_tools(transcript)

    result = evaluate_scenario(scenario, sid, transcript, td_tools, native_tools, commands)
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
