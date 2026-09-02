# Chunk 3 — off-loop ingestion, Work UI, PR prompt evidence, and autofix

## Files touched

- `crates/thegn-host/src/ci_refresh.rs`
- `crates/thegn-host/src/ci_autofix.rs` (new)
- `crates/thegn-host/src/actions.rs`
- `crates/thegn-host/src/detail.rs`
- `crates/thegn-host/src/detail/ci_drill.rs`
- `crates/thegn-host/src/detail_tests.rs`
- `crates/thegn-host/src/panel/mod.rs`
- `crates/thegn-host/src/panel/sections/ci.rs`
- `crates/thegn-host/src/panel/section_keys.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/pr_driver.rs`
- `docs/help/panel.md`
- `docs/help/cli.md`
- `docs/local-ci.md`

## Approach

1. Extend the existing off-loop CI refresh worker to compare old/new terminal
   runs, fetch missing failed-job logs in bounded batches, apply core tailing and
   redaction before every write, retain configured recent runs, and pulse the
   waker. Preserve current TTL/backoff/health behavior and never fetch on the
   render loop.
2. Add `ci_autofix.rs` as a coordinator, not a second runner. It reads the
   redacted cache and effective PR cache/queue, validates current head SHA and
   ownership, dedupes/persists before spawn, then calls the existing
   `PrCiFailure` prompt/template and `agent_run::run` seam. Reuse PR queue agent,
   sandbox, timeout, ownership, and attempt settings. Implement off/suggest/auto
   exactly, with suggestion notification/action and no automatic dispatch unless
   all guards pass.
3. Change `pr_driver`’s CI `{log}` variable to use bounded redacted excerpts,
   retaining check URLs as the explicit fallback. Do not add `CiFailure` to
   `TaskKind` or a new notification enum.
4. Make the Work-tab CI detail cache-first and show source, age, truncation, and
   redaction state. Add one documented fix action only if it is unclaimed and
   wire it through `DetailAction`, `CiActionCtx`, the run loop, panel dispatch,
   `section_keys`, caps-aware rendering, and tests together. No one-shot pane or
   browser escape replaces the in-place detail.
5. Update the existing panel/CLI/local-CI help pages. Document `act`, `gama`,
   and `wrkflw` as optional `[[tools]]` recipes/reproduction helpers, not
   `CiProvider`s. Run the source help ratchets; do not hand-edit generated help.

The help ratchet files are currently empty for the existing `panel:ci` context,
and the new key is a section-local key rather than a bindable global action, so
do not manufacture no-op ratchet edits. If the implementation introduces a new
action id or context instead, update the corresponding existing help ratchet in
this same chunk and include that path in the commit.

## Overlap and dependency

No file overlap with chunks 1 or 2. Run serially after both because this chunk
consumes the v62 cache APIs, repo CI policy, catalog/control read projections,
and bounded provider behavior. The host files listed here are exclusively owned
by this chunk; if a service/control file needs a fix, return it rather than
editing across the chunk boundary.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host ci_refresh`
- `cargo nextest run -p thegn-host ci_drill`
- `cargo nextest run -p thegn-host detail_tests`
- `cargo nextest run -p thegn-host pr_driver`
- `cargo nextest run -p thegn-host panel`
- `cargo nextest run -p thegn-host help`

Do not run `just test`, `just ci`, a full-workspace compile, e2e, a migration, or
the built binary. Any manual `thegn` invocation must set `XDG_STATE_HOME` to a
fresh temporary directory.

## Done criteria

- CI refresh, detail fetches, autofix, and PR prompt composition all run off-loop
  and pulse the terminal waker; no vendor call or blocking DB work is in a draw
  or input path.
- Work-tab CI list/detail can read cached redacted logs, reports stale/source /
  truncated/redacted metadata, and degrades to status/URL/error without panic.
- `off` never dispatches; `suggest` is deduped and actionable; `auto` requires a
  known PR, current head SHA, configured effective agent, and unused existing
  attempt budget. Refresh races cannot double-dispatch.
- Existing `PrCiFailure` prompts receive evidence when available and the URL
  fallback otherwise; no new task kind, AI dependency, notification kind, or
  local provider exists.
- Panel, CLI, config, and local-runner help pages pass their help ratchets and
  advertise every new action/config key exactly where it is implemented; the
  currently-empty help ratchets remain unchanged unless a new action/context is
  actually introduced.
- Commit exactly as: `feat(the-48): add off-loop CI autofix handoff and UI`
