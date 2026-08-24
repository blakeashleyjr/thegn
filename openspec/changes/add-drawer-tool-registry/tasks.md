# Tasks — drawer tool registry

## 1. Config (thegn-core)

- [ ] 1.1 `DrawerTool` struct (`tool`/`command` exclusive-or, `name`, `cwd`,
      `env`, `scope` enum via `config_enum!`) as `DrawerConfig.tools`
      (`[[drawer.tools]]`), in a sibling module — not appended to `config.rs`.
- [ ] 1.2 Validation: both/neither of `tool`+`command` rejected; dangling
      `tool` ref and duplicate labels warn; wire into `config_validate`.
- [ ] 1.3 Pure resolution helpers: occupant list (files + entries), label,
      cwd/env computation per scope. Unit tests for all of 1.1–1.3 (95% gate).
- [ ] 1.4 Document every key in `config/config.toml.example` (with the ATAC
      example), noting the env-indirection guidance for secrets.

## 2. Drawer state (thegn-host)

- [ ] 2.1 Generalize the flag store: value = open occupant label, legacy
      `true` ⇒ files; a global slot for `scope = "global"`; unit tests incl.
      legacy files.
- [ ] 2.2 Re-key `DrawerPool` and the spawn dedupe by (scope key, occupant);
      `pool_limit` spans all occupants; exit-cleanup clears persisted state.
- [ ] 2.3 Generalize `resolve_launch`/`contain_yazi_argv` to any occupant
      (`contain_drawer_argv`), keeping the fail-safe skips; files occupant
      keeps resolving through the file-manager path (coordinate with
      `add-file-manager-seam` — no yazi symbols in generic drawer code).
- [ ] 2.4 `sync_drawer_persistence` restores the remembered occupant on
      worktree/tab switch; global occupants survive the switch unstashed.

## 3. Actions, picker, keymap

- [ ] 3.1 `drawer-cycle` + `drawer-pick` `ACTION_SPECS` entries; `files-drawer`
      toggle restores the last-open occupant. Default chords must pass the
      keymap uniqueness tests (or ship palette-only).
- [ ] 3.2 Dedicated occupant picker palette (agent-picker pattern: pending
      gate, rows keyed by occupant label).
- [ ] 3.3 Handler wiring in `src/handlers/` (not the run.rs god-file).

## 4. Indicator

- [ ] 4.1 `drawer` bars widget: closed/open/count states, click-to-toggle hit
      target, glyphs/colors via `caps::active_glyphs()`/theme chokepoints
      (color/glyph ratchets stay clean); add to the default `bottom_left`.
- [ ] 4.2 Render-plan check: widget state changes mark `Full`; occupant output
      stays `Panes` (keep the render_plan tests green).

## 5. Help & docs

- [ ] 5.1 Update `docs/help/drawer-and-corner.md`: claim `drawer-cycle` and
      `drawer-pick`, describe the registry, scopes, and the statusbar chip
      (help + help-prose ratchets).
- [ ] 5.2 Update `docs/help/bars.md` for the `drawer` widget.

## 6. Validation

- [ ] 6.1 Re-record affected e2e baselines (`just e2e-update`, statusbar chip + drawer frames); review the diff.
- [ ] 6.2 Run `just ci` once, pre-PR (includes openspec validate).
