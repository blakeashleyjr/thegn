# Design — plugin UI extension boundary decisions

## Common evaluation gate

For each surface record: user value, stable identity, negotiation/versioning,
required capabilities/scopes, host-versus-plugin state ownership, placement and
visibility config, render/input budget, failure degradation, accessibility/help,
compatibility, and an explicit reason to adopt, defer, or reject.

## SidebarTab questions

- Is the contribution a whole tab, bounded rows in an existing native tab, or a
  provider whose native host renderer owns the rows?
- Who owns selection, scrolling, row identity, hit actions, narrow layout, and
  user placement/order?
- How do disabled/crashed plugins disappear without corrupting sidebar mode?

Arbitrary retained UI or terminal byte output is never an option.

## Theme questions

- Does a plugin contribute immutable token data, a named theme provider, or
  executable runtime styling?
- How are complete token coverage, contrast audit, terminal capability degrade,
  name collisions, persistence, reload, and untrusted values handled?
- Can equivalent value be served by ordinary user theme files without executing
  a plugin? Prefer the data-only path unless runtime value is demonstrated.

Plugins never supply terminal escape sequences or bypass host token resolution.

## Key-zone questions

- Are plugins allowed only actions inside their own focused surface, or global
  chords? Default evaluation is surface-local only.
- The host owns chord conflict resolution, focus/zone attribution, which-key and
  help projections, dispatch, and user overrides.
- Plugin input maps to versioned plugin action ids, never arbitrary host actions.

## Output

The decision record and public support matrix must say adopt/defer/reject for
all three. Adopted surfaces receive separate OpenSpec implementation changes;
deferred/rejected ones remain negotiation failures. Nothing here duplicates
THE-108 PanelSection behavior.
