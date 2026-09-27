# Coordination brief — THE-552

`reconstruct` omits every event at elapsed millisecond zero, so a pane's first
output and initial resize can vanish from replay, and search can select a
timestamp whose reconstructed grid lacks the matched text.

## Where to look

`replay.rs:213-225`. `reconstruct` builds a fresh emulator then calls
`feed_into(emu, 0, at_ms)`, and `feed_into` skips `ev.at_ms <= from_exclusive` —
so `0` is treated as an already-consumed sentinel rather than a valid time.
`search_next` and `export_cast` include those same events, which is why they
disagree.

## The distinction that is the whole fix

**Reconstruction from the start of history is not the same operation as forward
playback after an already-consumed event.** One is inclusive of time 0; the other
is exclusive of a cursor. Today both go through one exclusive-bound helper. Give
them distinct meanings rather than special-casing `0`.

If a millisecond timestamp cannot express the playback cursor — and with multiple
events sharing a millisecond it cannot — **use event sequence identity.** Say so
explicitly in your report if you take that route.

## Hard requirements

- Exactly-once incremental playback must survive same-millisecond groups, paused
  seek, resumption, and backward seek.
- `search` and `reconstruct` must agree at every timestamp.
- **No `sleep` in tests to shift events off zero.** That is the bug hiding itself;
  deterministic construction only.

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
