# File Explorer — file-manager provider seam

## MODIFIED Requirements

### Requirement: Yazi drawer is a reserved focusable panel

The file drawer SHALL be a reserved chrome region (a focusable `Drawer` zone) toggled by a keybind, sized as part of layout computation, with its open/closed state persisted per worktree. The drawer SHALL run the configured file-manager provider (yazi by default), and the drawer's pooling, prewarm, layout, per-worktree open flags, and containment MUST behave identically for every provider kind.

#### Scenario: Toggle the drawer

- **WHEN** the user toggles the file drawer
- **THEN** a reserved drawer region opens, is focusable, and its state is
  remembered for that worktree

#### Scenario: A swapped manager keeps the drawer chrome behavior

- **WHEN** `[drawer] kind = "custom"` selects a non-yazi manager and the user
  toggles, hides, and re-shows the drawer
- **THEN** the pool/prewarm, per-worktree open flag, and layout behave exactly
  as with yazi, and the host toggle keybind closes the drawer without any
  manager cooperation

## ADDED Requirements

### Requirement: The file manager is a provider seam

The drawer's file manager SHALL be a provider seam (`thegn_core::seam`
pattern): an object-safe sync `FileManager` trait resolved by a
`[drawer] kind` `config_enum!` — `yazi` (default, implemented), `custom`
(implemented: runs `[drawer] command` with no integration caps), with
unimplemented kinds marked `reserved` and rejected by
`thegn config validate --strict`. A caps struct MUST declare each optional
integration (git-status linemode, accent theming, drawer control channel,
private config isolation, image-preview policy), and integrations MUST be
attempted only when their caps bit is set. A non-empty `[drawer] command` with
`kind` unset MUST resolve to `custom` so existing configs keep today's
behavior.

#### Scenario: Swapping to a custom manager degrades cleanly

- **WHEN** `[drawer] command = "lf"` is set with no `kind`
- **THEN** the drawer spawns `lf` in the worktree as a plain contained PTY,
  and no yazi config seeding, theming, or control-byte scanning is attempted

#### Scenario: A reserved kind is rejected under strict validation

- **WHEN** a config sets `[drawer] kind = "lf"` and
  `thegn config validate --strict` runs
- **THEN** validation fails naming `lf` as reserved (accepted but not
  implemented in this build)

### Requirement: Vendor specifics stay inside the provider implementation

All yazi-specific behavior — `YAZI_CONFIG_HOME` seeding, managed
image-preview/git-status blocks, the vendored `git.yazi` and drawer-control
plugins, `THEGN_YAZI_BIN` resolution — SHALL live only in the yazi
implementation module. Generic drawer code (pool, prewarm, flags, layout,
containment, PTY drain) MUST NOT reference yazi symbols, and drawer control
commands MUST be decoded through the seam's `control` operation only for
providers whose caps declare a control channel. Integration-only `[drawer]`
keys (`config_home`, `image_previews`, `git_status`) MUST be inert for
providers without the corresponding caps.

#### Scenario: Control bytes from a capless manager are ignored

- **WHEN** a `custom` manager writes an `OSC 5379;close` sequence on its PTY
- **THEN** the host does not scan or dispatch it, and the drawer stays open
  until a host keybind closes it

#### Scenario: Yazi keeps its full integration

- **WHEN** the default yazi kind opens the drawer
- **THEN** the private config is prepared, the accent theme regenerated, git
  status and control plugins active — identical to the pre-seam behavior

### Requirement: The drawer file manager reports a probe

The selected file-manager provider SHALL report a `ProbeReport` in
`thegn doctor` — kind, binary availability, config-home mode, and caps — using
a cheap offline probe (binary resolution and config checks only).

#### Scenario: A missing manager binary is diagnosed

- **WHEN** `[drawer] kind = "custom"` names a binary not on PATH and
  `thegn doctor` runs
- **THEN** the drawer provider row reports `unavailable` naming the missing
  binary, and no drawer spawn is attempted until it resolves
