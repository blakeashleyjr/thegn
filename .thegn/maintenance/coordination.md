# Coordination brief — THE-677

`target/` is 91 GiB, with 15.1 GiB in stale generations of integration-test
binaries. The issue body is unusually good — measured numbers, an explicit
estimate, two named traps, and three options with a recommendation. Read it
carefully; it has done much of your work.

## The primary's decision on direction

Take the **third** option: extend `thegn_core::disk_reclaim`. The issue's own
reasoning is right and matches CLAUDE.md — that module already owns this policy
as pure, tested code and already runs at the tail of the background disk scan.
`cargo-sweep` would add a dependency and a second policy home; `clean-aux` is a
manual recipe, not a policy.

## Respect the two traps, they are both real

1. **No `Cargo.lock` entry does not mean orphaned** — `tests/*.rs` integration
   binaries legitimately have none. A naive scan flags 86 names and is wrong
   about 85 of them.
2. **An old mtime does not mean dead.** Cargo does not touch files on a cache
   hit, so a 40-day-old rlib may be the live artifact. An age-based prune trades
   disk for rebuild time, and on this machine a full rebuild is the most
   expensive thing available.

So the policy cannot be "old" or "not in the lockfile". It must identify a
**superseded generation** — a non-live artifact for a target that has a newer
one. If you cannot determine liveness reliably, say so and propose what would
make it determinable; do not guess and delete.

## Hard requirements

- Policy lives in `thegn_core::disk_reclaim`, **pure and unit-tested** like its
  neighbours, and honours the existing exemptions: active worktree, running
  build, uncommitted work.
- **Never removes the live generation.** The acceptance criterion is behavioural:
  a subsequent `just test` must not trigger a full-workspace rebuild. State how
  your tests establish that without running a full build.
- Report reclaimed bytes; record before/after.
- `thegn-core` is substrate-free and gated at 95% lines — new core logic needs
  unit tests.

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
