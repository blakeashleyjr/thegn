# Tasks — drawer tool registry

## 1. Config (thegn-core)

- [x] 1.1 `[[tools]]` drawer metadata (`drawer_scope`, `drawer_cwd`) in the
      sibling config policy module, with the files occupant first.
- [x] 1.2 Strict validation and warn-and-omit policy for malformed metadata,
      duplicate IDs, and metadata on `[[agents]]`.
- [x] 1.3 Pure registry, scope, cwd, and state-key helpers with focused tests.
- [x] 1.4 Document the metadata and ATAC/global examples in the config example.

## 2. Drawer state (thegn-host)

- [x] 2.1 Generalize the flag store: value = open occupant ID, legacy
      `true` ⇒ files; a global slot for `scope = "global"`; unit tests incl.
      legacy files.
- [x] 2.2 Re-key the runtime pool and spawn dedupe by (scope key, occupant);
      `pool_limit` spans all occupants; exit-cleanup clears persisted state.
- [x] 2.3 Generalize launch/containment to any occupant
      (`contain_drawer_argv`), keeping the fail-safe skips; files occupant
      keeps resolving through the file-manager path (coordinate with
      `add-file-manager-seam` — no yazi symbols in generic drawer code).
- [x] 2.4 Runtime reconciliation restores the remembered occupant on
      worktree/tab switch; global occupants survive the switch unstashed.

## 3. Actions, picker, keymap

- [x] 3.1 `drawer-cycle` + `drawer-pick` action entries; `files-drawer`
      toggle restores the last-open occupant. Default chords must pass the
      keymap uniqueness tests (or ship palette-only).
- [x] 3.2 Dedicated occupant picker palette (agent-picker pattern: pending
      gate, rows keyed by occupant label).
- [x] 3.3 Handler wiring in `src/handlers/` (not the run.rs god-file).

## 4. Indicator

- [x] 4.1 `drawer` bars widget: closed/open/count states, click-to-toggle hit
      target, glyphs/colors via `caps::active_glyphs()`/theme chokepoints
      (color/glyph ratchets stay clean); add to the default `bottom_left`.
- [x] 4.2 Render-plan check: widget state changes mark `Full`; occupant output
      stays `Panes` (keep the render_plan tests green).

## 5. Help & docs

- [x] 5.1 Update `docs/help/drawer-and-corner.md`: claim `drawer-cycle` and
      `drawer-pick`, describe the registry, scopes, and the statusbar chip
      (help + help-prose ratchets).
- [x] 5.2 Update `docs/help/bars.md` for the `drawer` widget.

## 6. Validation

- [ ] 6.1 Re-record affected e2e baselines; deferred by the architect design and
      this revision's no-e2e constraint.
- [ ] 6.2 Run full `just ci` once, pre-PR; targeted revision checks are recorded
      in the completion summary.
