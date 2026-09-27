# Coordination brief — THE-284

Terminal query parsing is stateless across PTY fairness slices, so a DA/DSR/OSC
query split at a 16 KiB boundary never gets a reply and inner programs disable
capabilities based on scheduling luck.

## Where to look

`pty_drain.rs:773-779,810-818` calls `queries::query_responses` per delivered
slice; `PtyBacklog::take_slice` splits at the configured maximum; the query
scanner keeps no per-pane state.

## The precedent to follow, not duplicate

THE-244 already made **OSC52 passthrough** stateful across slices. Read that
implementation first: if it established a per-pane bounded-buffer pattern, reuse
it rather than inventing a second one. Two different streaming-state mechanisms
in the same drain path would be the defect this issue is about, one layer up.

## Hard requirements

- Per-pane state must be **generation-scoped** and reset at the real
  reattach/fallback/exit boundary — not at an arbitrary slice edge.
- Retention of an incomplete CSI/OSC/APC must be **bounded**, and recovery must
  not interpret trailing text as a query.
- Exactly **one** reply per query, at every possible split byte offset.
- Two panes must never complete each other's parser state.

This is on the event loop's hot path. The 0%-idle and <16ms-render invariants in
CLAUDE.md apply: no unbounded buffer, no per-byte allocation.

## Line numbers in the issue are STALE

Citations come from an audit commit (`299fc13` or similar), not current `main`.
Batch 2 found three issues whose headline defect was already fixed and two whose
file inventory was wrong. **Re-verify every citation on this branch, and report
an already-met criterion as met, with evidence, rather than re-fixing it.**

## Cargo — attempt it, and say plainly if you cannot

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so `nix develop` usually fails outright** — if it does,
say exactly that and stop. The primary runs clippy, the full workspace nextest
and smoke centrally.

**Never report `implementation-ready` for code you could not compile.** Every lane
in the previous chain shipped something that did not build — an ambiguous
`Vec::new()`, a by-value row where a reference was wanted, a `PathBuf` never
imported, a `String` read of a nullable column, an unqualified function path, a
test reaching for a private method. The primary caught each one. That division of
labour is expected and is not held against you; an optimistic report is.

## Testing traps, all measured on this machine

- **Use nextest, never `cargo test`.** `TestIsolation` mutates process-wide env,
  so threaded `cargo test` cross-contaminates: one gate test failed under
  `cargo test` and passed under nextest.
- Fixture git commands need `-c commit.gpgsign=false`. Global signing is on and an
  unconfigured fixture hangs ~120s instead of failing.
- **A new `section.key` trips THREE ratchets** (config example coverage, env
  overlay, config validate). Name them if you add one.
- **`#[expect(...)]` on an item used only under `cfg(test)`** becomes _unfulfilled_
  in the test build. Use `#[cfg_attr(not(test), expect(lint, reason = ...))]`, or
  `#[cfg(test)]` on the item if only fixtures use it.
- **`--all-targets` dead-code warnings come from the NON-test build.** They say
  nothing about whether fixtures use the item — do not delete on that basis.
- **A fixture built from the same mental model as the code proves nothing about
  the real interface.** A parser in the previous chain passed its unit tests AND
  its own smoke shim, then refused everything against the real `docker`, because
  both fixtures modelled the parser's own assumption. If you touch anything that
  reads external output, capture the real thing (`cat -A`) first.
- **A worker running a DB migration from its shell mutates the LIVE database.**
  Use isolated fixture DBs; never point `XDG_STATE_HOME` at the live path for
  anything but the final `dispatch report`.

## Scope discipline

No new features. If an acceptance criterion needs a design decision beyond the
stated scope, **stop and report the blocker** rather than silently omitting it or
inventing a boundary. Flagging an unmet criterion is a good outcome; quietly
dropping it is the one thing that wastes a whole round.
