# Tasks — merge-queue ambient surface relocation

## 1. Per-repo rollup (pure)

- [ ] 1.1 A pure rollup `mq_rollup(rows, repo_path) -> Option<(Tier, count)>`
      (blocked > working > populated) shared by the token, the rail tint, and
      the `mq` widget — unit-tested per tier, including the
      unknown-attribution case (contributes to no token). Place so the status
      partition stays in one module (host unit tests; core untouched unless
      the vocabulary already lives there).

## 2. Sidebar token (thegn-host)

- [ ] 2.1 `sidebar_view.rs`: compose the token into the workspace header's
      right cluster (before the warm-pool chip), with width-yield order
      count → token; colors via `Tok::Hue`/`Tok::Slot`, glyphs via the shared
      `MqStatus` vocabulary through `caps::active_glyphs()` (ratchets stay
      shrink-only).
- [ ] 2.2 Hit-testing: expose the token cell through the `build_sidebar`
      layout pass (`RowHit`), so click activation derives from the painted
      geometry; clicks elsewhere on the row keep existing semantics.
- [ ] 2.3 Activation: mouse click + a keyboard route on the header row (row
      menu entry or key) opening the merge-queue detail scoped to that repo;
      wire through the shared sidebar dispatch so the surfaces cannot
      diverge.
- [ ] 2.4 Rail mode: red/amber urgency tint on the workspace cell; dim tier
      stays quiet.

## 3. Statusbar → bars widget

- [ ] 3.1 Remove `BarBadge::MergeQueue` from the default badge emission;
      register an `mq` widget id in the `[bars]` slot vocabulary rendering
      the existing chip + overlay activation (fit priority preserved).
- [ ] 3.2 `config/config.toml.example`: document `mq` in the `[bars]`
      bottom-bar widget list (spec'd key, no new table).
- [ ] 3.3 Guardrail test: default bars show no MQ chip; a config with `"mq"`
      in a slot shows it.

## 4. Docs + help

- [ ] 4.1 `docs/help/sidebar.md`: the project token (tiers, activation);
      claim + mention any new action id (both help ratchets).
- [ ] 4.2 `docs/help/bars.md`: the `mq` widget. `docs/help/merge-queue.md`:
      ambient-surface paragraph updated (chip → token).
- [ ] 4.3 CHANGELOG: behaviour change — the bottom-bar MQ chip is now opt-in.

## 5. Validation

- [ ] 5.1 Re-record affected e2e baselines (`just e2e-update`, review the
      diff; coordinate with `add-sidebar-visual-hierarchy` /
      `rename-workspaces-to-projects` to batch one re-record if landing
      together). Pin any new volatile content in `e2e_freeze` (counts are
      state-driven, not time-driven — expected none).
- [ ] 5.2 Run `just ci` once (openspec validate, ratchets, coverage).
