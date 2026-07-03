# Tasks

## 1. Stale-state coercion (superzej-core)

- [x] 1.1 `coerce_stale(state, age_ms, grace_ms) -> State` pure helper —
      **unit tests**: fresh running stays running, stale running downgrades to a
      settled state, non-running states pass through, exact-`grace_ms` boundary.

## 2. Schema (superzej-core / state-db)

- [x] 2.1 Bump `user_version`: add a `scrollback_snapshot` column to the tab-group
      table (additive, null-default) and read `agent_dispatches.dispatched_at_ms`
      at restore for the age computation — **unit tests**: migration is additive,
      an old snapshot with no scrollback restores unchanged.

## 3. Capture + restore (superzej-host)

- [x] 3.1 In `session.rs` `persist()`, capture a bounded tail of each leaf pane's
      scrollback (configurable line/byte cap) into the new column.
- [x] 3.2 In `resurrect()`, feed the captured tail back into the pane emulator so
      the restored pane repaints recent history before new output.
- [x] 3.3 Apply `coerce_stale` to persisted agent/activity state at resurrection
      before the sidebar renders — **render test**: a coerced dot is a chrome
      repaint, and a stale running state does not survive restore.

## 4. Docs + validate

- [x] 4.1 Document the scrollback cap config and the restore-time stale-state
      grace in `config/config.toml.example` + the session-persistence doc section.
- [x] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`). Verified locally:
      rustfmt (changed files), `clippy --workspace --all-targets -D warnings`
      clean, build clean, `superzej-core`+`superzej-host` tests green, core
      coverage 95.35% ≥ 95%, `openspec validate --strict` valid, god-file ratchet
      green (run.rs shrank 65 lines via new `snapshot.rs`; db.rs/config.rs
      ceilings raised for the unavoidable schema column + `[session]` config).
      `just smoke`/`nix-build`/`e2e` need the nix dev shell (run there before merge).
