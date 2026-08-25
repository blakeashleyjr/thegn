# Fork existing sessions

Linear: THE-29

## Why

The research seed (orca's fork-session issue) wants to branch a working
session at a point in time to explore an alternative path without losing the
original: same place, same command, diverge from here. In orca that means
re-injecting an AI conversation; thegn's AI layer is excised, so the honest,
generic core of the feature is:

**You cannot fork a live process.** A running PTY child (a shell, a build, an
agent CLI) has open fds, sockets, and in-memory state; no amount of protocol
can duplicate it (process checkpointing à la CRIU is far out of scope and
would break every credential/sandbox rule). What CAN be delivered, precisely:

1. **Re-spawn the recipe.** The daemon knows how a session was started
   (`OpenSpec`: argv/cwd/env, or an `agent:` launch it composed itself). Fork
   = open a _new_ session from the _same resolved recipe_ — a fresh process,
   honestly presented as such.
2. **Carry the context, not the state.** The forked session gets lineage:
   env vars naming the source session, and optionally the source's retained
   scrollback dumped to a file the new process can read. A conversation-aware
   program (any agent CLI, a script) can use that to resume its own way —
   thegn stays generic and never interprets it.
3. **Fork the workspace too.** For a worktree IDE the valuable fork is often
   "same session, _diverged files_": fork the worktree (branch from the
   source's — the existing fork-worktree flow, roadmap D-52) and start the
   forked session in the new worktree. This composes two existing pieces.

Today none of this exists: the daemon retains only `program`+`cwd` in
`SessionMeta` (not the full recipe), and there is no fork verb anywhere.

## What Changes

- **`sessions.fork` capability** (`Verb::ForkSession`, all non-streaming
  surfaces like `sessions.open`, scope via `required_scope` — same scope as
  open, since fork ≡ open with an inherited spec): daemon-side
  `fork(session, opts) -> SessionInfo`.
- **The daemon retains each session's resolved spawn recipe** (argv, env
  pairs, cwd, worktree, agent-launch marker) in memory for the session's
  lifetime — never persisted to the DB (env may hold credentials; the DB
  outlives the process). `agent:`-launched sessions re-resolve their
  composition (command, sandbox, environment) fresh at fork time instead of
  replaying stale env.
- **Lineage:** the forked session's env carries `THEGN_FORKED_FROM` (source
  session id) beside the existing `THEGN_SESSION_ID`/`THEGN_CONTROL_SOCKET`;
  `SessionInfo` gains `forked_from` so listings and the UI can show lineage.
  With `--scrollback`, the source's retained scrollback tail is written to a
  private file and exposed as `THEGN_FORK_SCROLLBACK`.
- **Placement:** fork files the existing adopt intent, so a running
  compositor grafts the fork as a split beside the source pane (or a new tab
  with `--tab`); headless forks just exist in the daemon like any opened
  session.
- **Worktree fork:** `thegn session fork <id> --fork-worktree` (and the UI
  flow) first creates a new worktree branched from the source session's
  worktree via the existing worktree-creation path, then forks the session
  with cwd/worktree remapped into it.
- **CLI:** `thegn session fork <id> [--scrollback] [--fork-worktree] [--tab]
[--cwd <dir>]`; UI: a `fork-session` action (palette + pane context) on the
  focused pane's session.

## Impact

- **Roadmap:** the generic substrate for AQ-219 "Fork task"; composes with
  D-52 (fork worktree, done). tasks.md wiring happens in the audit phase.
- **Specs:** `control-plane` — ADDED fork requirement. Capability catalog
  gains one row (`sessions.fork`); control wire schema
  (`docs/api/control-v1.json`) regenerates (`SessionInfo.forked_from`,
  `ForkSpec`).
- **In-flight changes reconciled:** **make-daemon-default** (daemon sessions
  are the default fork substrate), **add-runtime-session-split** (adopt/graft
  placement becomes an `apply_layout` op when the daemon owns layout; fork
  itself is layout-agnostic), **add-agent-task-engine** (a future "fork task"
  can call `sessions.fork`; no dependency either way). No overlap with the
  MCP scope-gating work: fork is not exposed on MCP in v1.
- **Help/config:** `fork-session` action claimed in
  `docs/help/daemon-and-sessions.md`; no new config section (fork has no
  knobs beyond CLI flags).

## Non-goals

- **Duplicating a live process or its in-memory state** — impossible;
  explicitly out.
- **Copying scrollback into the new emulator.** The fork's screen shows only
  what the fork's process writes — replaying the source's output into a
  different process's terminal would fabricate history. Context rides the
  scrollback _file_ instead.
- **Interpreting agent conversations.** Resume/re-injection is the forked
  program's business (it can read `THEGN_FORK_SCROLLBACK` or use its own
  native resume); thegn never parses it. The shell must not hard-depend on
  any AI layer.
- **Forking non-daemon (in-process, `[daemon] enabled = false`) panes** — the
  recipe lives with the daemon; fallback mode gets a clear "requires the
  daemon" error.
- **Fork lineage as a persistent tree view** (orca's sidebar lineage
  indicators). `forked_from` is surfaced in listings; a lineage tree UI can
  layer on later without protocol change.
