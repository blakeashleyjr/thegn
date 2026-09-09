# File manager as a provider seam

Linear: THE-14

## Why

The bottom file drawer is nominally swappable — `[drawer] command = "lf"` runs
any binary — but everything that makes the drawer good is hard-wired to yazi:
the private `YAZI_CONFIG_HOME` seeding, the accent-derived `theme.toml`, the
vendored `git.yazi` status plugin, the image-preview containment policy, and
the `OSC 5379` control plugins that let the manager close the drawer and open
files in the editor (`thegn_core::yazi`, `queries::drawer_command`,
`actions::dispatch_drawer_command`). A user who swaps the command gets a bare
PTY that silently loses every integration, with no diagnostic anywhere and no
statement of what was lost. That violates the house rule that every
substitutable backend is a provider seam (`thegn_core::seam`): object-safe
trait, caps ⇔ optional integrations, `kind` implemented-or-reserved, `Probe`
in `thegn doctor`, vendor specifics only inside the implementation file.

## What Changes

- **A `FileManager` provider seam** in `thegn-core` (sync, object-safe): the
  drawer resolves its manager through a trait covering spawn resolution
  (argv/env/cwd for the drawer PTY), private-config preparation, theming, and
  control-byte decoding — with a caps struct declaring which integrations the
  provider actually has (git status, themed accent, drawer control channel,
  private config isolation, image-preview policy).
- **Kinds via `config_enum!`**: `[drawer] kind` — `yazi` (default,
  implemented), `custom` (implemented: runs `[drawer] command` with no
  integration caps), `lf` and `broot` (reserved). Back-compat: a non-empty
  `[drawer] command` with `kind` unset behaves exactly like today (resolved as
  `custom`); empty command keeps the pinned yazi.
- **Yazi specifics move behind the seam**: everything in `thegn_core::yazi`
  (config seeding, managed blocks, `git.yazi`, the `tg-drawer-*` control
  plugins, `THEGN_YAZI_BIN` resolution) becomes the yazi implementation file;
  no `yazi`-named symbol is referenced from generic drawer code paths.
- **Host drawer plumbing stays manager-agnostic**: the pool, prewarm,
  per-worktree open flags, layout, containment scope
  (`contain`/`memory_max`/`cpu_quota`) and the drawer zone apply to every
  kind; OSC control decoding is attempted only when the provider's caps
  declare a control channel.
- **Probe in `thegn doctor`**: the drawer manager reports kind, binary
  availability, and config-home mode like every other seam.

No new externally invokable operation — no `capability::CATALOG` row and no
CLI/MCP surface change. The drawer toggle action and keybind are unchanged.

## Impact

- **Linear**: THE-14 (allow swapping yazi out easily).
- **Roadmap**: group **AF** — extends **606** (file management from the tree,
  today explicitly yazi-shaped) and underpins **477** (files tile "yazi/lf").
- **Specs**: `file-explorer` — MODIFIED (drawer requirement generalized to the
  provider), ADDED (seam, vendor-isolation, probe requirements). Conforms to
  `provider-seams` (no delta needed there — its requirements are generic).
- **Config**: new `[drawer] kind` key (config_enum, documented in
  `config/config.toml.example`); every existing `[drawer]` key keeps its
  meaning. The generated config-reference help page picks it up at runtime.
- **Gates satisfied** (per `docs/extending/provider-impl.md`): `kind_coverage`
  for the new enum, `config validate --strict` rejecting reserved kinds,
  doctor probe presence in `test/smoke.sh`, conformance probe-shape tests, and
  the core coverage gate on the moved pure logic.
- **In-flight overlap**: `add-viewers-and-quick-open` touches the _preview
  pane_ (a sibling surface), not the drawer — orthogonal.
  `add-config-trust-resolution` governs trusting `[drawer] command` from
  repo-level config; this change adds no new trust surface beyond the existing
  key. `add-workspace-search-replace` (THE-5, written alongside) is
  independent: search lives outside the drawer.
