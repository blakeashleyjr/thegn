# Tasks

## 1. Stale-state coercion (superzej-core)

- [ ] 1.1 `coerce_stale(state, age_ms, grace_ms) -> State` pure helper —
      **unit tests**: fresh running stays running, stale running downgrades to a
      settled state, non-running states pass through, exact-`grace_ms` boundary.

## 2. Schema (superzej-core / state-db)

- [ ] 2.1 Bump `user_version`: add a `scrollback_snapshot` column to the tab-group
      table (additive, null-default) and read `agent_dispatches.dispatched_at_ms`
      at restore for the age computation — **unit tests**: migration is additive,
      an old snapshot with no scrollback restores unchanged.

## 3. Capture + restore (superzej-host)

- [ ] 3.1 In `session.rs` `persist()`, capture a bounded tail of each leaf pane's
      scrollback (configurable line/byte cap) into the new column.
- [ ] 3.2 In `resurrect()`, feed the captured tail back into the pane emulator so
      the restored pane repaints recent history before new output.
- [ ] 3.3 Apply `coerce_stale` to persisted agent/activity state at resurrection
      before the sidebar renders — **render test**: a coerced dot is a chrome
      repaint, and a stale running state does not survive restore.

## 4. Docs + validate

- [ ] 4.1 Document the scrollback cap config and the restore-time stale-state
      grace in `config/config.toml.example` + the session-persistence doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
