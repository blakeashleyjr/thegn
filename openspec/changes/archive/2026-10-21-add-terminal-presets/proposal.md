# Terminal presets — named launch configurations + the launch menu, resurrected

Linear: THE-18

## Why

Launching something _into the worktree you are in_ is scattered and mostly
creation-time. The agent/tool picker exists only inside the new-worktree
wizard; at runtime the options are one hard-coded chord per tool (`Alt-g`
lazygit), a `[[actions]]` entry you must hand-bind per command, or a plain
shell. There is no way to say "my dev setup is the agent + `just dev` + a log
tail" once and summon it — the zellij-era launch menu (`thegn pick-agent`) was
absorbed into the wizard and never came back as a runtime surface. Superset's
terminal presets (name, description, working dir, commands, split-vs-tab mode,
one-click bar) are the comparable: named, reusable, multi-command launch
shapes.

thegn already has four places a launch shape half-lives: `[[agents]]`/
`[[tools]]` (the program registry), `[[pins]]` (supervised strip programs),
`[[worktree_templates]]` (creation-time bundles with `commands`/`layout`), and
`[[actions]]` `new-pane` (keybound one-shots). What is missing is the
composition layer usable at runtime, from the palette, and from the CLI.

## What Changes

- **`[[presets]]` — named launch configurations.** `name`, `description`,
  `commands` (each string resolved **first as an `[[agents]]`/`[[tools]]`
  name**, else run via the login shell — the single program registry stays
  single), `mode = "split" | "tabs"`, worktree-relative `cwd`, an `env`
  overlay, and an optional named-`layout` ref that takes precedence over
  `commands` (the `[[worktree_templates]]` precedence rule). Presets carry no
  provider/account/provisioning semantics — a referenced agent brings its own.
- **The launch menu.** A bindable `launch-menu` action opens a dedicated
  picker (agent-picker pattern): presets first (name + description), then the
  existing picker choices — agents, tools, `shell`. Selection launches into
  the **active worktree**, sandbox-wrapped exactly like the wizard's launch;
  picking an `[[agents]]` entry updates the worktree's remembered agent.
- **Preset application.** `split` = the commands as an even split in one new
  tab (the `WorktreeTemplate::commands` semantics); `tabs` = one new tab per
  command. Launch-spec resolution runs off the event loop.
- **Creation-time layering.** `[[worktree_templates]]` gains an optional
  `preset = "<name>"` ref (exclusive with `layout`/`commands`) so creation and
  runtime share one definition.
- **CLI.** `thegn open <repo> --preset <name>` enqueues a launch-preset intent
  (the preset **name** only — never argv over the wire) after the focus
  intent; with no live instance the launched compositor applies it after the
  first frame. The operation is a new capability-catalog row
  (`Verb::LaunchPreset`) with its own `required_scope`, so any future
  HTTP/gRPC/MCP/plugin projection answers to one policy table.

## Non-goals

- **Unifying the registries.** `[[agents]]`/`[[tools]]`/`[[pins]]` are not
  folded into presets (argued in design.md — distinct lifecycles, and
  `[[agents]]` is the sandbox-provisioning source of truth per the agent spec).
- **Lifecycle hooks** (setup/post-create scripts) — owned elsewhere (unit G11
  scope); a preset only launches panes.
- **Auto-apply on every new tab** (Superset's setting) and a preset **drawer
  placement** — open questions in design.md, not v1.
- **A preset bar** (Superset's pinned strip) — `[[pins]]` already owns strip
  real estate; revisit after the launch menu exists.

## Impact

- Roadmap: group **M** (command palette / launcher — 161/162/165 are the
  substrate), relates to **D 54** (worktree templates — gains the `preset`
  ref) and **E** (pins, deliberately not touched); adds a new item to group
  **M**.
- Specs: new `terminal-presets` capability; `cli` — ADDED requirement
  (`open --preset`). The main `cli` spec's `open <repo>` remote-control
  requirement (intents mailbox) is the substrate and is unchanged.
- In-flight reconciliation: **`add-cli-namespaces-and-remote-open`** (owns the
  `open` remote-control shape this rides; already reflected in the main cli
  spec). **`complete-control-surface-coverage`** (THE-39) — the new
  `LaunchPreset` catalog row lands under its implement-or-excuse coverage
  tests; coordinate rather than re-derive policy. The **MCP write-tools/scope
  branch** (in flight) owns MCP scope gating — an MCP projection of
  `LaunchPreset` is a follow-up there, not scoped here.
  **`add-config-trust-resolution`** — presets from non-user config layers are
  subject to trust gating. **`add-drawer-tool-registry`** (THE-11, sibling) —
  both changes layer on the same `[[agents]]`/`[[tools]]` registry; neither
  depends on the other.
- Code (indicative): `thegn-core` sibling config module (`Preset` + validation
  - pure resolution fold), `thegn-host` launch-menu picker + application
    (`handlers/`), intent enqueue/consume in `cmd/open.rs` + hydration,
    catalog row + scope in `thegn-core/src/{capability,control}.rs`.
- New action id `launch-menu` (and the preset picker) must be claimed by a
  `docs/help/` page (help ratchet); every new config key gets a documented
  `config/config.toml.example` entry.
