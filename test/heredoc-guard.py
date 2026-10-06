#!/usr/bin/env python3
"""PreToolUse guard: refuse an UNQUOTED heredoc whose body would execute code.

An unquoted heredoc (`<<EOF`) expands its body: `$VAR`, and also `$(...)` and
backticks, which RUN commands. Writing prose through one turns every inline
code span into a command. On 2026-10-05 an agent wrote a markdown brief via
`python3 - <<EOF` and the shell ran `just live`, `thegn`, `just smoke` and
queued `just e2e` straight out of the text.

So: a heredoc whose delimiter is unquoted AND whose body contains a backtick or
`$(` is blocked (exit 2, reason on stderr for the model). Quoted delimiters
(`<<'EOF'`, `<<"EOF"`, `<<\\EOF`) expand nothing and always pass, as does an
unquoted heredoc that only uses `$VAR`. A deliberate command substitution can
opt in with THEGN_ALLOW_EXPANDING_HEREDOC=1 anywhere in the command.

Reads the Claude Code hook payload on stdin (`.tool_input.command`). Any parse
problem passes: a broken guard must never block work.
"""

import json
import re
import sys

INTRO = re.compile(r"(?<!<)<<(-?)[ \t]*(\\?)(['\"]?)([A-Za-z_][A-Za-z0-9_.-]*)(['\"]?)")
OPT_IN = "THEGN_ALLOW_EXPANDING_HEREDOC=1"


def offending_heredocs(command):
    """Delimiters of unquoted heredocs whose body contains `$(` or a backtick."""
    lines = command.split("\n")
    found = []
    i = 0
    while i < len(lines):
        pending = []
        for match in INTRO.finditer(lines[i]):
            dash, backslash, open_quote, word, close_quote = match.groups()
            quoted = bool(backslash or open_quote or close_quote)
            pending.append((word, bool(dash), quoted))
        i += 1
        # Bodies follow the introducing line, one after another, in order.
        for word, dash, quoted in pending:
            body = []
            while i < len(lines):
                line = lines[i]
                i += 1
                if (line.lstrip("\t") if dash else line) == word:
                    break
                body.append(line)
            text = "\n".join(body)
            if not quoted and ("`" in text or "$(" in text):
                found.append(word)
    return found


def main():
    try:
        payload = json.load(sys.stdin)
        command = payload.get("tool_input", {}).get("command", "")
    except (ValueError, AttributeError):
        return 0
    if not isinstance(command, str) or "<<" not in command or OPT_IN in command:
        return 0
    bad = offending_heredocs(command)
    if not bad:
        return 0
    print(
        f"Blocked: unquoted heredoc <<{bad[0]} has a body containing backticks or $(...), "
        "which the shell would EXECUTE before the command sees the text.\n"
        "To write a file, use the Write tool. Otherwise quote the delimiter "
        f"(<<'{bad[0]}') so nothing expands, and pass values in via argv or env.\n"
        f"If the command substitution is intended, put {OPT_IN} in the command.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    sys.exit(main())
