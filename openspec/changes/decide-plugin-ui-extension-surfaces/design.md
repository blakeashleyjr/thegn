# Design — plugin UI extension boundary decisions

## Common evaluation gate

The inventory at this change's base has two reserved wire names,
`SidebarTab` and `Theme`, but no host runtime for either; their support rows
remain Reserved with `surface:sidebar` and `surface:theme`. Key zones have no
plugin vocabulary, support row, or capability. The loader negotiates the two
reserved names as unsupported; the host renders plugin `View`/`Span`/
`StyleRole` models with semantic styles and does not accept plugin terminal
output. There is no plugin config or contribution schema for these surfaces.
User themes are closed, validated TOML palettes, and configured drawer tools
are not plugin surfaces. The help page states this current boundary, and
there is no runtime contribution path or fixture contract for these surfaces.
The decisions below use those existing boundaries and leave all three
surfaces rejected by negotiation until their implementation changes land.

Each surface is evaluated for user value, ownership, identity and placement,
lifecycle, rendering and input, theme/sidebar limits, capabilities and
resource bounds, localization/accessibility, platforms, and fixtures. The
implementation changes must turn the bounds below into measured limits and
versioned wire contracts before negotiation can change.

## Decisions (owner, 2026-10-20)

| Surface    | Decision | Bounded shape                                                  | Negotiation now       |
| ---------- | -------- | -------------------------------------------------------------- | --------------------- |
| SidebarTab | Adopt    | Host-rendered, bounded plugin rows after native navigation     | Reserved; rejected    |
| Theme      | Adopt    | User-selected, data-only complete palette contribution         | Reserved; rejected    |
| Key zones  | Adopt    | Suggested chords for actions in a plugin's own focused surface | No contract; rejected |

### SidebarTab — adopt

- **User value and example:** A project review plugin can show a short,
  changing queue of review items with status badges and activate a declared
  plugin action for a selected item. A statusbar segment or a panel section
  cannot provide a navigable sidebar list with host-managed selection and
  focus restoration.
- **Ownership and placement:** The host owns the sidebar mode, section,
  geometry, selection, scrolling, hit targets, truncation, and focus
  restoration. The plugin supplies row data only. Rows appear in a dedicated
  plugin section after native navigation; they never replace or reorder
  native navigation. Persisted sidebar mode and placement remain host-owned.
- **Identity and ordering:** Each row has a stable, plugin-namespaced element
  id; each activation names a declared `<plugin-id>.<action>` id. The host
  assigns row order within the plugin section and applies any user ordering
  override after native navigation. Duplicate ids are rejected, and plugin
  ids cannot claim another plugin's namespace or native ids.
- **Lifecycle and cache:** Initial rows are validated before display. Updates
  replace the plugin's prior row snapshot. The host may cache only the latest
  bounded snapshot for the current plugin/API version; it does not restore
  stale rows after restart or config reload without a fresh contribution.
  Crash, removal, or incompatible API version clears that plugin's rows while
  preserving the current sidebar mode and other plugins' rows.
- **Rendering and keyboard:** The host renders the existing semantic
  `View`/`Span`/`StyleRole` model, with an optional badge. No ANSI stream,
  arbitrary widget tree, or replacement chrome is accepted. Keyboard input
  changes host selection; activation emits only the row's declared action id.
  Escape, focus, and safety chords stay host-owned.
- **Theme boundary:** Row styles are semantic roles resolved by the host's
  currently selected theme. A sidebar contribution cannot add tokens, select
  a theme, or bypass host contrast and terminal-capability resolution.
- **Bounds and capability:** Require `surface:sidebar`; enforce row count and
  per-row byte limits set by the implementation change from a measured
  rendering and memory budget. Excess or malformed content receives a typed
  refusal and an audit event. Use the existing plugin queue, rate limits,
  resident/one-shot admission, sandbox posture, and audit machinery; this
  surface grants no subprocess, filesystem, or network authority by itself.
- **Localization and accessibility:** Coordinate with THE-51 before defining
  localized labels, accessible names, badge announcements, truncation, and
  focus behavior. The host exposes semantic text and focus state to its
  accessibility path; plugins do not supply terminal control sequences.
- **Platforms and fixtures:** Keep the same row, ordering, and activation
  contract on every supported platform. The implementation change needs
  unit fixtures for valid/over-limit/duplicate rows and action ids, visual
  fixtures for narrow and wide sidebars, and an end-to-end fixture proving
  activation and crash/removal preserve sidebar mode and focus safely.

### Theme — adopt

- **User value and example:** A design-system plugin can offer a complete
  palette derived from a project's design tokens, with provenance, through
  the normal theme picker. This adds a selectable palette tied to plugin
  output; statusbar, panel, and palette-action seams cannot contribute a
  validated complete theme. Ordinary theme files remain the right choice for
  static palettes.
- **Ownership and placement:** The host owns theme selection, persistence,
  palette resolution, contrast auditing, terminal-capability degradation,
  and live reload. The plugin supplies immutable data and cannot apply a
  theme or change native chrome directly.
- **Identity and ordering:** A contribution is named
  `plugin:<plugin-id>:<name>`. Names are unique within a plugin namespace;
  collisions are rejected. The user's normal theme selection takes
  precedence over plugin arrival or update, and a plugin contribution is
  never auto-applied.
- **Lifecycle and cache:** Validate a complete contribution before exposing
  it. Cache only the latest valid palette for its plugin/API version; refresh
  it on a valid update or config reload. On crash, restart without a fresh
  contribution, removal, or incompatible API version, remove the stale
  palette. If the removed contribution was selected, fall back to the default
  preset and show a status message.
- **Rendering and keyboard:** Contributions use the closed `UserTheme`
  palette shape and the existing token-resolution path. No executable
  styling, raw color escape sequences, partial palette, or alternate
  renderer is allowed. Selection and keyboard behavior stay in the host's
  normal theme flow.
- **Sidebar boundary:** A plugin theme appears only as a choice in the host's
  normal theme picker; it cannot add, remove, order, or style sidebar rows
  independently of the selected host palette.
- **Bounds and capability:** Require `surface:theme`; apply the theme
  importer's completeness and contrast rules, terminal-capability token
  degradation, and a size cap no larger than the existing 64 KiB theme-file
  limit. Invalid or oversized data receives a typed refusal and audit event.
  Use existing plugin queue, mode, sandbox, and audit machinery; this
  capability grants no process or I/O authority by itself. Show the
  contributing plugin as provenance in the picker and selected-theme
  status.
- **Localization and accessibility:** Coordinate with THE-51 for localized
  theme names/descriptions and accessible picker labels. Contrast and
  capability degradation are evaluated by the host and must remain legible
  across supported terminal color levels.
- **Platforms and fixtures:** Use the same palette validation and selection
  behavior on every supported platform. The implementation change needs
  unit fixtures for missing tokens, invalid contrast, oversized and
  colliding names, visual fixtures across terminal color capabilities, and
  an end-to-end fixture for user selection, reload, provenance, and removal
  fallback.

### Key zones — adopt

- **User value and example:** A plugin that owns a focused review sidebar can
  suggest `n`/`p` actions for next/previous review item while that surface is
  focused. A global palette action cannot express a chord active only in that
  plugin surface, and a statusbar action has no focused-surface key zone.
- **Ownership and placement:** The host keymap owns chord conflict
  resolution, focus and zone attribution, which-key/help projection,
  dispatch, and user overrides. The plugin declares versioned action ids and
  suggested chords only for its own focused sidebar rows or statusbar
  segment. Ordering follows host keymap precedence and explicit user
  overrides; plugins cannot reorder or replace host bindings.
- **Identity and conflicts:** Actions use stable `<plugin-id>.<action>` ids
  and a versioned declaration. Suggested chords are scoped to the declaring
  plugin surface. Duplicate action ids are rejected. A chord that conflicts
  with a host or user binding remains unbound and is reported; it never
  shadows that binding.
- **Lifecycle and cache:** The host validates declarations at plugin load
  and config reload, and caches them only for the current plugin/API
  version. Updates replace the prior declaration. Crash, removal, or
  incompatible API version removes the plugin's bindings and help projection
  without changing host or user bindings.
- **Rendering and keyboard:** The host renders key hints in its existing
  help and which-key surfaces. Dispatch sends only a declared semantic
  plugin action/event. No unrestricted interception of pane or global
  keystrokes is permitted; escape, focus, and safety chords can never be
  bound by a plugin.
- **Theme boundary:** Key hints use host-rendered semantic roles and the
  active host theme. A plugin cannot provide colors, tokens, or terminal
  styling through a key-zone declaration.
- **Sidebar boundary:** A sidebar key zone may operate only while its own
  plugin rows are focused; it cannot replace native navigation, change
  sidebar geometry, or act on another plugin's rows.
- **Bounds and capability:** Require a distinct `surface:keyzone`
  capability, to be added by the implementation change. Apply existing
  plugin queue, rate, mode, sandbox, and audit limits; invalid declarations
  and conflicts receive typed refusals and audit events. The capability
  grants no subprocess or I/O authority by itself.
- **Localization and accessibility:** Coordinate action names, descriptions,
  key hints, and accessible key announcements with THE-51. Help and
  which-key output are host-localized; plugins provide semantic labels, not
  terminal sequences.
- **Platforms and fixtures:** Keep conflict, focus, and dispatch semantics
  consistent on every supported platform. The implementation change needs
  unit fixtures for versioning, invalid ids, reserved chords, host/user
  conflicts, and overrides; visual fixtures for help/which-key projection;
  and an end-to-end fixture proving a chord works only while its owning
  surface is focused and is removed on plugin failure/removal.

## Proposed wire and schema changes

These are planned additions for three separate implementation changes; this
decision change makes none of them.

- **SidebarTab:** the implementation change will change its support row from
  Reserved to Wired with `surface:sidebar`, define the row contribution
  payload and declared action id, and specify resident updates plus
  one-shot initial contribution and their cadences.
- **Theme:** the implementation change will change its support row from
  Reserved to Wired with `surface:theme`, define a contribution payload using
  the closed `UserTheme` shape and namespaced name, and specify one-shot
  contribution plus resident refresh/removal behavior and cadences.
- **Key zones:** the implementation change will add the `surface:keyzone`
  capability, define a versioned action/chord declaration payload, and
  change the key-zone support row from Reserved to Wired with focused-surface
  resident updates and one-shot declaration modes and cadences.

Each child change must specify typed negotiation refusals and bind its
declared modes/cadences to existing queue and audit limits. All three remain
Reserved and rejected until that child lands with runtime negotiation,
documentation, snapshots, and tests together.

## Output

This record answers the THE-107 questions and publishes the owner decision.
Adopted surfaces receive separate OpenSpec implementation changes. Nothing
here duplicates THE-108 PanelSection behavior. No public documentation may
claim that the whole UI is plugin-editable.
