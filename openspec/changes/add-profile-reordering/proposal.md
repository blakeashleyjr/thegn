# Add profile reordering

## Summary

The profile switcher lists profiles in **alphabetical order** with no way to
reorder them. Add a stable, user-editable profile order — persisted in shared,
never-rerooted config — and a reorder affordance inside the switcher, mirroring
the existing workspace/worktree reorder pattern.

## Impact

- **H** (Profiles & subprofiles) — extends item **108** (Profile switcher); adds
  a new sub-item for user-controlled switcher ordering.

## Rationale

`palette::build_profile_palette` dedupes profile names into a `BTreeSet` (the
config `profiles` collection is a `BTreeMap`, and on-disk
`~/.config/thegn/profiles/*` are readdir-scanned), so the switcher order is
alphabetical and shifts as profiles are added. Profiles have **no DB rows** (each
profile is its own rerooted `thegn.db`), so a cross-profile order must live in
**shared config at the real `XDG_CONFIG_HOME`** — the one location `profile.rs`
never reroots and where the shared base config + profile overlays are already
discovered. thegn already ships a proven reorder mechanism for workspaces
(`db.set_workspace_order(&[String])` + the `move-item-up/down` actions and
`handlers/sidebar_reorder.rs`), whose "persist the entire visible order" approach
we reuse verbatim.

## Non-goals

- Reordering profiles as sidebar tree rows (profiles are switcher entries, not
  sidebar rows).
- Cross-profile data/workspace movement (a separate concern).
- Changing the `default` profile's semantics; it remains selectable and is
  orderable like any other, defaulting to first.
