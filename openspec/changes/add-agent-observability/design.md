# Design

## Reused seams (no new substrate)

- **`crates/superzej-core/src/account.rs`** — `struct Provider { id, home_env,
default_dir, login_argv, auth_marker }` + the `const PROVIDERS` registry.
  Each `Provider` gains a **usage-file parser** seam: given the resolved
  credential-home dir (`account::resolve_dir`, already worktree/workspace/global
  layered), parse the provider-native on-disk rate/usage state (e.g. under
  `~/.claude`, `~/.codex`) into a typed `UsageState { plan_pct, reset_at,
window }`. Pure, fully unit-tested against fixture files; reads only, never
  writes. Unknown / absent files yield `None` (widget hides) — never an error.
- **`crates/superzej-host/src/hydrate.rs`** — `enum RefreshKind` gains
  `RefreshKind::Usage`. `spawn_refresh_ticker` emits it on a coarse ~30–60s
  cadence (a whole multiple of the 500ms half-tick, like `Disk`); a
  `spawn_usage_cache_refresh` worker does the off-loop disk read and pulses the
  `TerminalWaker`. Like every other `RefreshKind`, it is a staleness backstop —
  never blocking the loop.
- **Activity FSM + emulator OSC title** — the emulator already exposes the app's
  OSC 0/2 title via `PaneEmulator::title()`. OSC-title detection makes that the
  **authoritative** working/idle/waiting signal: a small pure classifier maps
  title text → `ActivityState`, and `activity::poll_and_save_with` consults it
  first, falling back to the existing CPU-jiffy heuristic only when no title
  signal is present. The sticky-red `waiting`/`read` semantics are unchanged.
- **`EventBus`** (`crates/superzej-core/src/event_bus.rs`) — the agents feed is a
  view over existing `Event` variants (`AgentDone`, `AgentFailed`, plus a new
  agent state-change event), grouped per worktree into threads.
- **Panel `Section`** (`crates/superzej-host/src/panel/mod.rs`) — a new
  `Section::Agents` feed section plus the L-group statusbar widgets (usage chip,
  agent-state chip, feed badge).
- **Session history** — transcripts live in provider-native dirs (`~/.claude`,
  `~/.codex/sessions`); a `SessionHistoryBackend` trait (per provider) scans
  them read-only; resume shells the agent's own command with `--resume`.

## Rendering & event loop

- **Usage / agents-feed hydration is off-loop.** `RefreshKind::Usage` and the
  agent-state read run on the `hydrate.rs` worker / ticker thread, send their
  result on the existing mpsc channel, and **pulse the `TerminalWaker`**. The
  loop drains on wake and re-renders only when the model changed.
- **Damage channels.** An idle wake with no usage/feed delta ⇒ **`Skip`** (the
  0%-idle contract holds — model equality via `hydration_eq` drops the no-change
  frame). A change to the usage chip, agent-state chip, agents-feed section, or
  an activity dot is **chrome** damage ⇒ a **`Full`** frame (chrome recompose),
  exactly like every other sidebar/panel/statusbar change. Pure pane output is
  unaffected and still routes to **`Panes`**. No new polling timeout is added.
- The agent-state classifier reads the title the emulator already captured on
  the PTY reader thread — no extra loop work.

## Persistence

SQLite gains a small `agent_sessions` cache table (transcript path, provider,
cwd, branch, model, token estimate, first-ask, mtime) so the session-history
list paints instantly without re-walking dirs each open; it is a cache, the
transcript files on disk are the source of truth, and a missing file evicts its
row. Usage state is _not_ persisted (it is volatile and re-read each tick). This
bumps the DB `user_version` by one (additive migration, no backfill). Feed
entries are derived from the `EventBus` / existing notification rows and need no
new table.

## Invariants

- **0% idle.** All disk reads (usage, transcript scan) and feed assembly run
  off the loop on the existing hydration/ticker threads, send over mpsc, and
  pulse the waker; the loop never polls and re-renders only on a real delta.
- **Off-loop.** No git/DB/subprocess/file-walk on the event loop. Resume spawns
  the agent command as an ordinary pane launch, not on the loop.
- **AI-additive, never a hard dependency.** Every signal is read **locally**
  from disk or the terminal — there is no proxy call and no provider API. With
  the AI/proxy layer absent or disabled, the usage widget, OSC-title dots,
  session list, and feed all still function as a pure observability surface; the
  AI-free shell never hard-depends on any AI layer.
