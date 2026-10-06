# THE-437 plan

Scope: replace highest-pid child heuristic with tpgid/pgrp foreground-group selection (Linux /proc stat, macOS proc_bsdinfo e_tpgid); pure select_foreground shared; fail closed without a ctty.
Files: platform/proc.rs, pane.rs. Tests: pure selector, stat parse, real-PTY bash job-control test.
Out of scope: argv redaction, typed idle/shell/unsupported states, generation binding, remote panes (need design; larger than lane).
