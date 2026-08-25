# Design — drawer tool registry

## Registry model: references, not a second command table

thegn already has one program registry (`[[agents]]`/`[[tools]]`, the shared
`NamedCommand` shape) and three composition layers over it (the wizard picker,
pins, worktree templates). A `[[drawer.tools]]` entry is another composition
layer, not another place a command string lives:

```toml
[[drawer.tools]]
tool = "atac"              # reference into [[tools]] by name (exclusive with `command`)
# name = "api"             # display label; defaults to the tool name / command basename
# command = "atac"         # inline one-off (exclusive with `tool`), run via the login shell
scope = "worktree"         # "worktree" (default) | "global"
# cwd = ".atac"            # worktree-relative (worktree scope) or absolute/~ (global)
# env = { ATAC_MAIN_DIR = ".atac" }
```

`tool` refs get the `[[tools]]` entry's command (and its statusbar `hints`);
`command` is the pins-style escape hatch for a program not worth a `[[tools]]`
entry. Config validation rejects an entry with both or neither of
`tool`/`command`, warns on a `tool` naming no `[[tools]]` entry (the occupant
is omitted at runtime), and warns on duplicate labels. All of this is pure
`thegn-core` config logic under the 95% coverage gate.

ATAC is the motivating example: `[[tools]] name = "atac" command = "atac"`
plus a worktree-scoped drawer entry with `env = { ATAC_MAIN_DIR = ".atac" }`
gives every worktree its own API-collection state in-repo.

## Occupant model: one visible, N registered

The drawer region stays a single PTY rect. Occupants form an ordered list:
`files` (built-in, occupant #0 — the file-manager drawer exactly as
`add-file-manager-seam` specs it) followed by `[[drawer.tools]]` in config
order. Switching swaps which pane composites into the rect; the previous
occupant is stashed in the pool (position/state survives) under its
(scope-key, occupant) key.

Alternative considered — drawer-internal tabs rendered on the divider row
(browser-devtools style). Rejected for v1: the divider is 1 row of chrome that
the render plan treats as geometry, and an in-drawer tab strip needs its own
hit-tables and focus rules; the picker + cycle actions deliver the same
capability with two actions and no new chrome contract. The divider-strip can
layer on later without config changes.

Alternative considered — folding occupants into `[[pins]]` (`location =
"drawer"`). Rejected: pins are supervised, mostly-global singletons with
start/restart semantics and strip presence; drawer occupants are lazy,
worktree-keyed chrome with pool eviction. Sharing the table would force each
side to carry the other's knobs.

## Switching is chrome-level

While the drawer owns focus, every key goes to the occupant (the existing
contract — that is why yazi needs an OSC channel to close the drawer). So
`drawer-cycle` and `drawer-pick` are global chords/actions, dispatched by the
keymap before pane forwarding, exactly like `files-drawer` today. `drawer-pick`
opens a dedicated picker palette (the `build_agent_palette` pattern: rows keyed
by occupant label, routed through a pending-selection gate in the Enter
handler) — which keeps the "every palette row is an action" invariant intact,
since the main command palette only gains the two actions, and occupant rows
live in the dedicated picker.

Occupant process exit (e.g. `q` in ATAC) closes the drawer, drops the pane
from pool/table, and clears the persisted open state — generalizing the
existing exited-yazi `remove_id` path.

## State & persistence

The per-worktree flag file (`~/.thegn/drawer/<slug>`) currently stores
`true`/`false`. It becomes the open occupant's label (empty/absent = closed);
`true` is read as the files occupant for back-compat, and the write path keeps
the memory-first, write-through-off-loop contract of `FlagCache`. Global-scope
occupants persist one global slot (a reserved key in the same store), since
"the scratch REPL is open" is not a per-worktree fact. No DB involvement.

## Spawn pipeline & event loop

Cold spawns reuse `drawer_state::request_spawn`'s off-loop resolve (spawn
request → blocking task resolves the launch via the config/tools lookup +
containment wrap → channel send + `TerminalWaker` pulse → loop drain opens the
pane), with the in-flight dedupe key extended from worktree to (scope-key,
occupant). Nothing new blocks the loop; `[drawer] prewarm` continues to
prewarm only the files occupant (registry occupants are cheap-on-demand; a
per-entry `prewarm` is an open question below). Render damage: open/close/
switch are geometry/chrome changes ⇒ `Full`; occupant output while open is
`Panes`, unchanged.

## Indicator

A `drawer` widget joins the `[bars]` vocabulary (default appended to
`bottom_left` after `keyhints`): closed = dim drawer glyph; open = highlighted
glyph + active occupant label; a small `×N` count when more than one occupant
is configured. Click toggles (bars already have clickable chips — `help` — and
fitted hit-tables). Glyphs go through `caps::active_glyphs()`; colors through
the theme chokepoints — no literals at the draw site (color/glyph ratchets).

## Containment

`contain_yazi_argv` generalizes to `contain_drawer_argv`: every occupant argv
is wrapped in the bounded user `systemd-run --scope` under the same `[drawer]`
caps and the same fail-safe skips (disabled, no systemd-run, already-wrapped
sandbox argv). Registry occupants are host processes like the file manager —
they are chrome, never daemon-routed.

## Security

- **What runs**: arbitrary argv from the user's trusted config. Repo-local or
  otherwise less-trusted config layers must pass the trust gates
  (`add-config-trust-resolution`) before their `[[drawer.tools]]`/`[[tools]]`
  entries take effect — a hostile repo must not be able to plant a drawer
  occupant that runs on toggle.
- **Blast radius**: occupants run as the user on the host (not in the worktree
  sandbox — same as the file manager today), bounded by the `[drawer]` scope
  caps so a runaway occupant is OOM-killed in its own cgroup instead of taking
  the terminal down. `env` values are plain config strings; secrets should use
  the env indirection story (`add-env-setup-ux`) rather than raw values —
  documented at the config key.
- **No new external surface**: no CLI verb, control route, MCP tool, or plugin
  call is added — everything is in-UI, so no capability-catalog row and no
  scope changes.
- **No credential handling** beyond what a configured command does itself.

## Testing

- `thegn-core`: `DrawerTool` parse/validate (both/neither/unknown/duplicate),
  label resolution, cwd/env computation — pure unit tests (95% gate).
- `thegn-host`: `FlagCache` value semantics incl. legacy `true`, pool keying/
  eviction across occupants, exit-cleanup, containment wrap — unit tests;
  picker/switch/indicator — e2e (baseline re-record).

## Open questions

- Per-entry `prewarm` (an occupant the user opens constantly)? Deferred; the
  global `prewarm` stays files-only.
- Default chord for `drawer-cycle`/`drawer-pick` (Ctrl-Alt-d is free today) or
  palette-only like the wizard verbs? Implementation must pass the keymap
  uniqueness tests either way.
- Should the indicator be in the default `bottom_left` set, or opt-in? Default
  proposed on (discoverability is the point of THE-11), revisit if it crowds
  narrow terminals.
