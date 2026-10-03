# THE-436 plan

Scope: justfile start/start-dev/start-mq/start-term/start-term-release, test/lib/instance.sh (new), tests.
Approach: pidfile records `pid start-generation`; stop only if both still match (bounded TERM then KILL, re-verified); strict parse; atomic mktemp+mv write; flock-serialized launch; rotation scoped by per-process XDG_STATE_HOME via /proc, replacing pkill -f. `live` already fixed (scripts/live.py, never kills).
Tests: test/instance-lifecycle.sh (reuse, malformed, symlink, stubborn, concurrent, two-root rotation); dev-tui-plan.sh asserts semantics.
Out of scope: pidfd (no shell access), crash-at-every-transition fuzzing, macOS scoped rotation (skips).
