# Drawer tool registry — arbitrary per-worktree and global drawer occupants

Linear: THE-11

## Why

The bottom drawer is a single hard-wired occupant: the file manager. Everything
about the surface is already general — a reserved chrome region
(`layout::compute_with_drawer`), a focusable `Drawer` zone, a keep-alive pane
pool with an eviction bound, per-worktree open-flag persistence, off-loop cold
spawns, and systemd-scope containment (`drawer_state.rs`) — but only one
program can ever live in it. Users who want an API client (ATAC), a scratch
REPL, a DB shell, or a log tail in that slot today must burn a center tab or a
`[[pins]]` strip slot, neither of which is worktree-scoped chrome.

THE-11 asks for two things: an **arbitrary number of drawer options** — per
worktree and possibly global — and a **visual indicator that the drawer
exists** (today it is invisible until you know `Ctrl-Alt-f`).

## What Changes

- **`[[drawer.tools]]` — a registry of additional drawer occupants.** Each
  entry names a `[[tools]]` entry (`tool = "atac"`) or declares a one-off
  inline `command`; optional `name` label, `cwd`, `env`, and
  `scope = "worktree" | "global"`. The built-in **files** occupant (the
  file-manager drawer, whatever provider `add-file-manager-seam` resolves it
  to) stays occupant #0; with no `[[drawer.tools]]` configured, behavior is
  byte-identical to today. This deliberately adds **no second command
  registry**: occupants reference the existing `[[tools]]` table, and inline
  commands mirror the `[[pins]]` one-off escape hatch.
- **One visible occupant, switchable.** The drawer shows one occupant at a
  time. `files-drawer` (Ctrl-Alt-f / Alt-y) keeps toggling the drawer, now
  restoring the worktree's last-open occupant. New bindable actions
  `drawer-cycle` (next occupant) and `drawer-pick` (a dedicated picker palette,
  agent-picker pattern) switch occupants — as global chrome chords, since a
  focused occupant owns every keystroke.
- **Scope semantics.** `worktree` occupants get a per-worktree pane (cwd = the
  worktree root, or `cwd` relative to it) and per-worktree persistence;
  `global` occupants are a single shared pane that follows you across
  worktrees (cwd = `$HOME` or an absolute `cwd`), pin-style. The pane pool,
  `pool_limit` eviction, prewarm, and process-exit cleanup extend to be keyed
  by (scope key, occupant).
- **Persistence generalizes.** The per-worktree drawer flag file records
  _which_ occupant is open, not just `true`; legacy `true` flags mean files.
  No SQLite schema change, no `user_version` bump.
- **Visual indicator.** A new `drawer` bars widget (statusbar chip, default in
  `bottom_left`): dim drawer glyph when closed, highlighted glyph + active
  occupant name when open, occupant count when several are configured;
  clicking toggles the drawer. Removable like any `[bars]` widget.
- **Containment for every occupant.** The `[drawer]` systemd-scope caps
  (`contain`/`memory_max`/`memory_swap_max`/`cpu_quota`) wrap every occupant,
  not just the file manager, with the same fail-safe skip rules.

## Non-goals

- **The file-manager occupant's internals.** Seeding, theming, the OSC 5379
  control channel, and `[drawer] kind` belong to `add-file-manager-seam`
  (THE-14); registry occupants are plain contained PTYs with no integration
  caps.
- **`[[agents]]` refs in the drawer.** Agent panes carry account/provisioning/
  activity-attribution semantics and belong in the center; the drawer hosts
  auxiliary tools. The launch menu (`add-terminal-presets`, THE-18) is the
  runtime agent surface.
- **Lifecycle hooks** (pre/post scripts around drawer tools) — owned elsewhere
  (unit G11 scope).
- **Multiple occupants visible at once** (drawer splits) — see design
  alternatives.

## Impact

- Roadmap: extends **AF** (file viewer / search — the drawer items, e.g. 606)
  and touches **L** (status bar widgets) for the indicator; adds a new item to
  group **AF**.
- Specs: new `drawer` capability (this delta). `file-explorer` is deliberately
  **not** modified — its drawer requirement stays true and is being modified
  in-flight by `add-file-manager-seam`; this change layers beside it (the
  files occupant IS that spec'd drawer).
- In-flight reconciliation: **`add-file-manager-seam`** (THE-14) — the files
  occupant resolves through its `FileManager` seam; this change must not
  reference yazi symbols from generic drawer code either (same ratchet).
  **`add-config-trust-resolution`** — drawer entries from non-user config
  layers are subject to config trust gating.
- Code (indicative): `thegn-core/src/config.rs` sibling module
  (`DrawerTool` + validation), `thegn-host/src/drawer_state.rs` (occupant
  keying, flag value, pool), `thegn-host/src/handlers/` (cycle/pick),
  `palette.rs` (picker), `chrome.rs`/bars (widget), keymap + help.
- New action ids `drawer-cycle`, `drawer-pick` and the `drawer` widget must be
  claimed by `docs/help/drawer-and-corner.md` (help + help-prose ratchets);
  every new config key gets a documented `config/config.toml.example` entry.
- e2e: the statusbar chip and occupant switch alter frames — baselines need
  re-recording via `just e2e-update` when implemented (local gate).
