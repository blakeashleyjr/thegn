# THE-11 — drawer tool registry and presence indicator

## Decision

Extend the existing `[[tools]]` capability catalog with drawer metadata. A
tool becomes a drawer occupant when it has `drawer_scope = "worktree"` or
`drawer_scope = "global"`; its existing `name`, `command`, and `env` remain the
single source of launch configuration. An optional `drawer_cwd` supplies a
scope-relative working directory. The built-in files provider remains the
first occupant, so the effective registry is:

```text
files provider, then eligible [[tools]] in config order
```

This deliberately prunes the OpenSpec draft's `[[drawer.tools]]` table and its
inline-command/tool-reference XOR. A second table would duplicate the
picker's catalog and make command/env metadata disagree. The existing
`NamedCommand` is already the picker record (`crates/thegn-core/src/config.rs:1697-1740`),
and the picker already consumes `cfg.tools` (`crates/thegn-host/src/palette.rs:718-733`).
ATAC is therefore configured as an ordinary `[[tools]]` entry with drawer
metadata, not as a vendor/provider implementation.

Example shape for `config/config.toml.example`:

```toml
[[tools]]
name = "atac"
command = "atac"
drawer_scope = "worktree"
# drawer_cwd = ".atac"
# env = { ATAC_MAIN_DIR = ".atac" }

[[tools]]
name = "db"
command = "psql"
drawer_scope = "global"
```

`drawer_scope` being absent keeps the existing tool picker behavior and does
not add the tool to the drawer. `drawer_cwd` is relative to the worktree for a
worktree occupant; for a global occupant it must be absolute or `~`-prefixed.
An omitted cwd means the worktree root or the local user's home respectively.
The core policy module validates these forms without filesystem or process
access. The host expands `~`, expands existing `env:`/`file:` references off
the loop, and owns process launch.

## Verified branch constraints

- The existing drawer is already provider-neutral at its boundary:
  `DrawerLaunch` carries plain argv/cwd/env and resolves the file manager
  through `thegn_core::file_manager` (`crates/thegn-host/src/drawer_state.rs:182-295`).
  The current containment function is generic over every manager kind
  (`crates/thegn-host/src/drawer_state.rs:329-368`), so it must be reused for
  configured tools rather than adding an ATAC branch.
- Cold drawer resolution is already channel+waker based and deduplicated
  off-loop (`crates/thegn-host/src/drawer_state.rs:201-259`). The registry
  resolver must extend its key from only a worktree directory to
  `(scope-key, occupant-id)`; no filesystem, PATH probe, or config expansion
  may move onto the event loop.
- Drawer panes intentionally use `spawn_argv_env_local`, not the pane daemon
  (`crates/thegn-host/src/drawer_state.rs:308-325`; the local-spawn contract is
  `crates/thegn-host/src/panes.rs:437-449`, with a regression test at
  `crates/thegn-host/src/panes.rs:1659-1685`). A global occupant means one live
  local PTY reused across in-process worktree switches. It does not survive
  thegn detach/quit and must not be daemon-owned or reattached from the state
  database; this avoids orphaning ephemeral chrome.
- The pool is currently keyed only by worktree and bounded by
  `[drawer].pool_limit` (`crates/thegn-host/src/drawer_state.rs:115-178`). The
  new pool key must include the occupant and a global sentinel. Eviction and
  `pool_limit = 0` remain unchanged in meaning and apply across all occupants.
- The current state cache is memory-first with write-through persistence
  (`crates/thegn-host/src/drawer_state.rs:32-113`), and the current startup and
  switch orchestration is embedded in `run.rs` (`crates/thegn-host/src/run.rs:6579-6650`,
  `8978-9041`, `12599-12629`, `19280-19335`). Extract drawer-specific policy
  into sibling handler/state code; do not grow those god-file sections with
  registry branches.
- `NamedCommand.env` already documents secret indirection and last-wins
  launch overlays (`crates/thegn-core/src/config.rs:1722-1728`). Reuse that
  path. Do not add an inline shell parser or a vendor-specific provider.
- The local command seam already has cross-platform login-shell argv building
  (`crates/thegn-core/src/shellinv.rs:45-67`), and `panes.rs` already exposes
  `tool_drawer_argv` (`crates/thegn-host/src/panes.rs:282-284`). Configured
  commands must use this seam; the drawer registry returns data, not PTY or
  shell objects.
- `Config::env_overlay` is intentionally shallow and structured list entries
  are pinned rather than made addressable by environment variables
  (`crates/thegn-core/src/config.rs:5552-5615`, `5724-5763`). Pin the new
  `tools.drawer_scope`/`tools.drawer_cwd` surface in the env-overlay ratchet;
  do not invent an index-based `THEGN_TOOLS_*` syntax.
- Config enums are schema-walked automatically but their definition count is
  pinned at 90 (`crates/thegn-core/src/config_validate.rs:541-630`). Adding
  `DrawerScope` requires the deliberate 90→91 ratchet note and test update.
- The default statusbar left cluster is configured in `BarsConfig`
  (`crates/thegn-core/src/config.rs:2972-3028`), while paint and hit testing
  are separate but intended to agree (`crates/thegn-host/src/statusbar_left.rs:1-114`;
  `crates/thegn-host/src/chrome.rs:1867-1905`). The indicator must be one
  shared layout item, not a draw-only special case. Use existing glyph and
  theme chokepoints (`crate::caps::glyph`, `col(S::...)`); `Glyph::Folder` and
  existing Dim/Accent slots are sufficient, so no raw glyph literal is added.
- Default action rows come from `ACTION_SPECS`
  (`crates/thegn-host/src/keymap_specs.rs:1108-1140`) and the main palette
  preserves the invariant that rows dispatch actions
  (`crates/thegn-host/src/palette.rs:321-397`). The drawer picker must be a
  dedicated pending-selection palette, analogous to the launch picker
  (`crates/thegn-host/src/palette.rs:736-772`), with `drawer:<occupant-id>`
  keys handled before generic action dispatch.

## Core policy

Add a small `config_drawer` sibling module and re-export its enum/types from
`config`; do not add another registry module inside the already ratcheted
`config.rs`. The module owns:

1. `DrawerScope` (`worktree`, `global`) through `config_enum!`.
2. The pure selection of eligible `cfg.tools` entries, preserving config
   order, omitting empty names/commands, warning on duplicate drawer IDs, and
   keeping the built-in files occupant at index zero in the returned policy
   view.
3. Stable IDs: `files` for the built-in occupant and `tool:<name>` for a
   configured occupant. A renamed tool intentionally does not resurrect a
   stale state record; it falls back to files.
4. Pure scope/cwd validation and scope-key calculation. Worktree keys are the
   existing slugged absolute directory; the global key is a fixed sentinel.

The `NamedCommand` additions are `drawer_scope: Option<DrawerScope>` and
`drawer_cwd: Option<String>`, both serde-defaulted. They are meaningful on
`[[tools]]`; validation warns/errs on their use in `[[agents]]`. Existing Rust
constructors must initialize them to `None`. Existing default tools remain
picker-only unless explicitly opted in. Core tests cover ordering, duplicate
IDs, legacy/invalid scope values, cwd policy, and no-I/O behavior.

Strict config validation should report malformed drawer metadata and dangling
tool names, while normal layered loading follows the repository's existing
warn-and-degrade rule: omit only the bad occupant and keep files/other tools
usable. No repository overlay support is added. The current trust resolution
limits repo overlays to approved sandbox/config surfaces, and command
registries are already treated as global/user configuration; allowing
arbitrary repo-local drawer commands would require a new trust/security design
outside THE-11.

## Host state and lifecycle

Replace the boolean flag value with an occupant ID while keeping the same
memory-first/off-loop write contract:

- Existing `true` files decode as `files`; `false` decodes closed.
- New worktree files contain `files`, `tool:<name>`, or `false`.
- One separate global slot stores `tool:<name>` or `false`.
- The desired worktree state and desired global state are independent, but
  only one occupant is visible. On switch, an open destination worktree
  occupant wins; otherwise an open global occupant resumes from the pool.
  Switching back restores the prior worktree/global pane by `(scope-key,
occupant-id)`.

Cycle advances through the effective registry and opens the next occupant;
`files-drawer` toggles the last occupant for the active scope, falling back to
files. Switching stashes the outgoing live PTY rather than killing it. A
process exit removes the pane from both pool/table, clears its state, and
closes the drawer. A stale async result is dropped unless its scope key and
occupant still match the current request.

Every configured command is converted through `tool_drawer_argv`, wrapped by
the existing `contain_drawer_argv`, and launched through
`spawn_argv_env_local`. A missing/empty command, unavailable worktree cwd, or
failed spawn produces a warning/status and leaves the remaining registry
usable. No daemon session, migration, or SQLite schema change is involved.

## Input, picker, and help

Add `drawer-cycle` and `drawer-pick` to the action registry, parser, key
serialization, and palette. They are palette-visible and have no default chord
in v1, avoiding a new collision in the global keymap; users can bind them in
`[keybinds]`. They remain chrome-level actions even while an occupant owns
focus. The existing files action and aliases remain compatible.

`drawer-pick` opens a dedicated modal containing files plus every eligible
tool in registry order. It uses a pending drawer selection gate, not a
string-keyed back door in the main command palette. Entering a row requests or
restores that `(scope-key, occupant-id)` pane; Escape cancels. The picker and
cycle paths share the same state transition, pool, async request, and exit
handling. Help frontmatter/body must claim and explain both new action IDs,
scope behavior, the picker, and the config keys.

## Indicator and layout

Add the removable `drawer` widget to the default `[bars] bottom_left` order
before `keyhints`, so the presence affordance survives narrow-width keyhint
shedding. The widget is a compact, atomic item:

- closed: existing folder glyph, dim color, label `drawer`, and configured
  occupant count when useful;
- open: same glyph through `crate::caps::glyph(Glyph::Folder)`, Accent color,
  and the active occupant label; append the count only when more than one
  occupant exists;
- zero valid configured tools still shows the built-in files affordance.

The pure `FrameModel` carries a small drawer-bar state snapshot. The same
builder is used by `left_layout` and `left_item_spans`; clicking its span
dispatches the files-drawer toggle. The indicator does not become a new focus
zone. This preserves keyboard reachability through actions/help while making
mouse hit testing agree with paint. Do not make the widget silently disappear
when a command is unavailable: the registry count is config validity, while
the active pane state is runtime state.

## Ratchets, snapshots, and verification

This change adds no CLI verb, clap value-taking argument, daemon/control route,
or external control schema. Therefore completion-slot and control-schema
ratchets must be run and remain unchanged; do not manufacture entries. The
help ratchets must shrink/remain empty after documenting the two action IDs.
The env-overlay ratchet must add the structured `tools.drawer_scope` and
`tools.drawer_cwd` paths with a reason, because list-entry metadata is not an
addressable environment knob. Any enum-count/help-ratchet edits belong in the
same coder commit as the code they pin.

The default indicator intentionally changes chrome snapshots. List, but do not
re-record or run e2e in this task, these affected baselines:

- `test/muse/snapshots/chrome_regions__chrome/xterm__100x30__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__160x40__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__200x50__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__40x12__linux.txt`
- `test/muse/snapshots/chrome_regions__chrome/xterm__80x24__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__160x40__linux.txt`
- `test/muse/snapshots/glitch_hunt_chrome_consistency__bars/kitty__80x24__linux.txt`

Do not run a built `thegn` binary or migration against the live state DB. If a
coder needs an invocation, it must set `XDG_STATE_HOME` to a fresh temporary
directory.

## OpenSpec audit

The draft correctly identified ordered occupants, one-visible-pane switching,
worktree/global scope policy, state migration from boolean flags, async
deduplicated cold spawns, common containment, a removable indicator, and the
need for picker/help/snapshot coverage. The current branch already satisfies
two draft prerequisites: provider-neutral containment is in
`drawer_state.rs:329-368`, and the file-manager provider seam is in
`thegn-core/src/file_manager.rs:136-210`; the draft must not re-land those as
new abstractions. The current branch also already has the local ephemeral
spawn seam and `tool_drawer_argv` in `panes.rs:282-284,437-449`.

Pruned or changed draft claims:

- `[[drawer.tools]]` and inline command/tool-reference XOR are replaced by
  metadata on `[[tools]]`, preserving one capability catalog.
- Draft references to repo-local drawer commands and a new trust gate are cut;
  repo overlays do not gain arbitrary command registries in this issue.
- “Global survives detach/restart” is cut. The existing local-spawn contract
  explicitly prevents daemon orphaning; global means one pooled live PTY while
  this process switches worktrees.
- The draft's “no new chrome behavior with no registry entries” is narrowed:
  drawer process behavior remains compatible, but the requested presence
  indicator is intentionally visible even for the built-in files occupant.
- Draft e2e re-recording is deferred to the listed snapshot follow-up, per the
  issue's no-e2e constraint.
