#!/usr/bin/env python3
"""Tests for test/heredoc-guard.py (the PreToolUse unquoted-heredoc guard)."""

import json
from pathlib import Path
import subprocess
import sys
import unittest

GUARD = Path(__file__).resolve().parent / "heredoc-guard.py"


def run(command):
    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})
    return subprocess.run([sys.executable, "-B", str(GUARD)], input=payload.encode(),
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)


class HeredocGuardTests(unittest.TestCase):
    def assertBlocked(self, command):
        result = run(command)
        self.assertEqual(result.returncode, 2, command)
        self.assertIn(b"Blocked", result.stderr)

    def assertAllowed(self, command):
        self.assertEqual(run(command).returncode, 0, command)

    def test_the_incident_is_blocked(self):
        self.assertBlocked('python3 - <<EOF\nnote = """never run `just live` here"""\nEOF\n')

    def test_command_substitution_in_an_unquoted_body_is_blocked(self):
        self.assertBlocked("cat > f <<EOF\ntoday is $(date)\nEOF")
        self.assertBlocked("cat <<-END\n\tuse `x`\n\tEND")

    def test_quoted_delimiters_always_pass(self):
        for intro in ("<<'EOF'", '<<"EOF"', "<<\\EOF", "<<-'EOF'", "<< 'EOF'"):
            self.assertAllowed(f"cat > f {intro}\nrun `just live` and $(rm -rf /)\nEOF")

    def test_plain_variable_expansion_passes(self):
        self.assertAllowed("cat > f <<EOF\nhome is $HOME and ${USER}\nEOF")

    def test_only_the_body_counts_not_the_command_line(self):
        self.assertAllowed("echo $(date) && cat <<EOF\nplain text\nEOF")

    def test_here_strings_and_no_heredoc_pass(self):
        self.assertAllowed("grep x <<< \"$(cat f)\"")
        self.assertAllowed("echo `date`")
        self.assertAllowed("echo '<<EOF'")

    def test_second_heredoc_on_one_line_is_checked(self):
        self.assertBlocked("cmd <<'A' <<B\nsafe `x`\nA\nunsafe `y`\nB")

    def test_opt_in_and_bad_payloads_pass(self):
        self.assertAllowed("THEGN_ALLOW_EXPANDING_HEREDOC=1 cat <<EOF\n$(date)\nEOF")
        result = subprocess.run([sys.executable, "-B", str(GUARD)], input=b"not json",
                                stdout=subprocess.PIPE, timeout=10)
        self.assertEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
