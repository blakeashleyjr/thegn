# Design

The decision logic is pure and lives in `thegn_core::pane_memwatch`: file-body
parsers, a threshold state machine, the restart guard and the notice text. The
host owns the I/O: the loop publishes `(pane id, pid, program)` triples from the
drain (throttled, no I/O); a `thegn-memwatch` thread resolves each pid's scope
through `/proc/<pid>/cgroup`, reads the cgroup files, and raises alerts through
`notify::record` (kind `resource_alert`). A pane at or above 85% shows a title
badge. The cgroup v2 probe also runs on the thread. The thread exits when no
pane is published.

An exit is restartable only when an `oom_kill` was seen within 180 s before it
and the status is not a clean exit. The sole-pane respawn already leaves the
dead leaf for the materialize pipeline; a pane in a split now does the same
instead of vanishing. An exit after user input to the pane is the user closing
it and is never restarted. The agent is recognised from the foreground process
on every sample and cached, because nothing is left to inspect after the exit;
only the harness's fixed continue command is ever typed (never a command line
reconstructed from `/proc`), optionally after `cd` into the agent's directory,
and only once the respawned pane is in the foreground tab, within two minutes. The ten-minute cooldown is keyed by pane slot: materialize moves the stamp
from the dead id to its replacement.

Not covered: delivering the warning to the agent (no agent-notification channel
exists; nothing is typed into a running pane), standalone terminals (they close
on exit by design), and sandboxed or remote panes (no host-runnable relaunch).
