# THE-360 plan

Scope: crates/thegn-host/src/managed_tool.rs only.
THE-251 shared primitive is not landed; reuse existing platform seam instead
(spawn_grouped = pgid / Job Object, gate_child_exited = waitid WNOWAIT).
Approach: one run_bounded (owned group, one deadline, TERM grace then group KILL,
leader unreaped until group killed, both pipes drained to EOF with head+tail
retention, redacted, typed RunError). run_setup_cmd and tar extraction both use
it; CLI echo is a tee, capture is mode-independent.
Tests: flood bound, pipe-holding descendant, timeout tree kill, nonzero redaction,
spawn error, thread leak. Out of scope: credential isolation (THE-361), download bounds (THE-182).
