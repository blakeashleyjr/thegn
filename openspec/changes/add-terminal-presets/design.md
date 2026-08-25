# Design — terminal presets & launch menu

## Unify or layer? Layer — one program registry, one composition layer

The tempting shape is a new `[[presets]]` table that _contains programs_
(name + command + env + …). That would be the fifth place a command string
lives, after `[[agents]]`/`[[tools]]` (`NamedCommand`), `[[pins]]`,
`[[worktree_templates]].commands`, and `[[actions]]` `new-pane.run` — and the
second registry the launch picker would have to merge, dedupe, and explain.

The alternative — unifying everything into one `[[programs]]` table with
roles — was also considered and rejected:

- `[[agents]]` is load-bearing beyond launching: the agent spec makes it the
  **source of truth for sandbox provisioning** and account/provider
  semantics; churning its shape churns provisioning.
- `[[pins]]` carry supervision semantics (eager/lazy start, restart,
  singleton, strip geometry) that make no sense on an ephemeral launch.
- It is a breaking config migration for alpha users, for zero new capability.

So presets **layer**: a preset is a named launch _shape_, and each `commands`
string resolves **first as an exact `[[agents]]`/`[[tools]]` name** (through
the same resolution the picker uses), else runs via the login shell. `"shell"`
(or an empty string) is the login shell, matching the picker sentinel and
`WorktreeTemplate::commands`. A preset never restates what `claude` means; it
says _"claude, plus `just dev`, plus a log tail, split, in this worktree"_:

```toml
[[presets]]
name = "dev"
description = "agent + server + logs"
commands = ["claude", "just dev", "tail -f log/dev.log"]
mode = "split"            # "split" (default): even split in one new tab | "tabs": one tab per command
# layout = "ide"          # named-layout ref; takes precedence over commands
# cwd = "services/api"    # worktree-relative; default worktree root
# env = { RUST_LOG = "debug" }
```

Validation (pure `thegn-core`, 95% gate): unique names; `commands` empty with
no `layout` rejected; unknown `layout` ref warns and falls back to `commands`;
duplicate names warn (first wins, matching config precedent).

Superset parity check: their fields (name, description, working dir, commands,
split-pane vs new-tab mode) map 1:1; their preset bar and per-new-tab
auto-apply are deliberately out (non-goals — pins own strip real estate; see
open questions).

## The launch menu

`launch-menu` is an ordinary bindable action (proposed default `Ctrl-Alt-l`,
which is free today — must pass the keymap uniqueness tests, else ship
palette-only like the wizard verbs). It opens a **dedicated picker palette**
(the `build_agent_palette` pattern): presets first, with descriptions, then
the existing `agent::choices()` fold — agents, tools, `shell`. Rows are keyed
by choice name and routed through a pending-selection gate in the Enter
handler, so the _main_ command palette's "every row is an action" invariant is
untouched (it only gains the `launch-menu` action row).

Semantics on selection, always against the **active worktree**:

- an `[[agents]]` entry → new tab, sandbox-wrapped launch, and the worktree's
  remembered agent (`worktrees.agent`) is updated — the same write the wizard
  picker makes, so resurrection and activity attribution follow the launch;
- a `[[tools]]` entry / `shell` → new tab, sandbox-wrapped, no agent update;
- a preset → applied per its `mode` (below). No active worktree (e.g. an
  empty workspace) → a status-line message, no-op.

## Preset application

`split` reuses the `WorktreeTemplate::commands` semantics: one new tab in the
active worktree's group, the commands as an even split (a command that is an
agent/tool name resolves first, as above). `tabs` creates one new tab per
command. Every process composes through the same launch-spec pipeline as the
picker (sandbox wrap, env assembly, worktree cwd + preset `cwd`, preset `env`
overlaid last), so a preset cannot bypass the worktree's sandbox, and the
spawned panes join the shared `thegn.slice` resource ceilings like any
interactive pane.

Event loop: launch-spec resolution opens the DB and resolves sandboxes, so a
preset application resolves **off the loop** (blocking task → channel →
`TerminalWaker` pulse → loop drain spawns the panes), the drawer cold-spawn
pattern. Render damage: new tabs/splits are chrome/geometry ⇒ `Full`.

## Creation-time layering

`WorktreeTemplate` gains `preset: Option<String>` — exclusive with its
`layout`/`commands` (validation rejects combinations) — so "the dev shape" is
defined once and referenced from templates. The template's own `agent` field
still governs the remembered agent when set; otherwise an agent launched by
the preset's first agent-resolving command claims it (same rule as the launch
menu).

## CLI & capability catalog

`thegn open <repo> --preset <name>` rides the existing intents-mailbox
remote-control (main cli spec): validate the name against config (miss →
candidates + exit 3, the `open` convention), enqueue a launch-preset intent
carrying the **name only** after the `focus_workspace` intent; the compositor
consumes it claim-and-delete and applies the preset against that workspace's
active worktree. No live instance → the compositor launches focused and
applies the preset after the first frame (deferred work, never before it —
the startup contract).

Catalog: a `Verb::LaunchPreset` row. Executing configured commands is a
strictly bigger power than focusing a workspace, so it gets its **own
`required_scope`** (an exec-level scope, not `open`'s) — policy stays in the
one `required_scope` table, never a second list. Surfaces: `Cli` implemented
now; HTTP/gRPC/MCP/plugin projections answer to the
`complete-control-surface-coverage` implement-or-excuse tests (and MCP scope
gating belongs to the in-flight MCP write-tools branch). The wire/intent
payload never carries argv or env — resolution happens against the receiving
compositor's own config.

## Security

- **What runs**: only commands the user declared — in `[[presets]]` or the
  registries they reference. Non-user config layers must pass config trust
  gating (`add-config-trust-resolution`) before contributing presets; a
  hostile repo config must not inject a preset that the launch menu or
  `open --preset` would execute.
- **Remote trigger surface**: `LaunchPreset` is name-only-over-the-wire, so a
  client holding the scope can trigger pre-declared launches but cannot inject
  commands, env, or cwd. It gets a distinct scope so an `open`-scoped token
  cannot execute anything.
- **Credentials**: preset `env` is plain config; secrets belong behind the
  env/secret indirection (`add-env-setup-ux` `SecretRef`/`env:`/`file:`),
  documented at the key — never raw tokens in `[[presets]]`.
- **Blast radius**: launched panes are sandbox-wrapped per the worktree's
  sandbox resolution and capped by the shared `thegn.slice` ceilings; a preset
  adds no privilege beyond what launching the same commands by hand has.

## Testing

- `thegn-core` (pure, 95% gate): preset parse/validation matrix, the
  name-resolution fold (agent → tool → shell), mode/cwd/env computation,
  template-`preset` exclusivity, intent payload round-trip.
- `thegn-host`: picker composition + pending gate, remembered-agent update,
  off-loop application drain — unit tests; `open --preset` end-to-end —
  smoke; picker frames — e2e (local gate, baselines re-recorded).

## Open questions

- Auto-apply (a default preset on new tab / new worktree, Superset's
  toggles)? Creation-time is covered via templates; per-new-tab auto-apply
  deferred until asked for.
- Should a preset be able to activate pins or a drawer occupant (full
  "workspace shape")? Deferred — that is `[[worktree_templates]]`'s
  trajectory (roadmap 535, unified layout+task template).
- `Alt-Enter` in the launch menu to force split-beside-current for single
  entries? Nice-to-have, not specced.
