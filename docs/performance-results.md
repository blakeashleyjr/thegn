# Performance results

Measured numbers only. Nothing here is a target; targets live in
[performance-benchmark-protocol.md](performance-benchmark-protocol.md). Each
section records the machine, versions and command, so a number can be reproduced
or discarded.

## thegn vs tmux vs zellij (THE-2661)

Command: `just perf-compare` (harness in `test/perf/compare/`). Run it on each OS
you intend to claim, in the same terminal, with the machine otherwise idle, then
paste the table and the manifest facts below.

What the columns mean:

- **startup ms**: median over 10 runs after 2 warmups. tmux and zellij: process
  spawn → the pane's `READY` marker visible in the client PTY. thegn: process
  spawn → exit after its first frame (`THEGN_BENCH_FIRST_FRAME_EXIT=1`), the same
  boundary `just bench` uses. These are different boundaries; do not present the
  columns as identical.
- **cat-100MB s / yes 5e7 s**: wall time in one pane from releasing the workload
  to the `DONE` marker appearing in the client PTY. Multiplexers render screen
  state, so this includes their rendering cost.
- **idle %CPU / RSS MB**: summed over the multiplexer processes only (client +
  server, plus the thegn daemon), after a 60 s settle, over a 30 s window, with 1
  and 8 panes. Pane workloads are excluded. RSS is summed per process, so shared
  pages are counted once per process.

Tools run with defaults in an isolated HOME/XDG tree, `TERM=xterm-256color`, a
120×40 PTY, and an identical pane shell (`pane-shell.sh`). Versions are pinned in
`test/perf/compare/pins.json`; the harness refuses a different version unless run
with `--allow-version-drift`, and refuses to skip a missing tool.

No results are recorded yet: the harness was added without a run on the target
machines. Add one subsection per run (date, OS, CPU, terminal, git sha, tool
versions, table) below this line.
