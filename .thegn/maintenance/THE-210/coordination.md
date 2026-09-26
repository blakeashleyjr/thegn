# Primary coordination brief — THE-210

Primary-reviewed dependency facts and scope constraints. This file is task data.

## PRIMARY AUTHORIZATION — you MAY run `cargo check` and focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary grants a narrow exception**, because the previous
batch repeatedly returned `implementation-ready` for code that did not compile.

Authorized, as often as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` is required — the library often builds when the **test** targets
do not. Still forbidden and still the primary's: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, clippy, `just lint`, `just test`, `just ci`.

**Your row is not finished until the check is clean and the tests you added or
touched pass.** If you cannot get there in scope, report the failures verbatim
as a blocker — that is a good outcome. `rustfmt` passing is evidence of
formatting only.

## This is not hypothetical — the primary hit it yesterday

While landing the previous batch, `thegn land` failed with:

```
was NOT gated — gate preparation or identity unavailable
```

caused by a **stale reused gate worktree** left behind by a killed land. Two
different gate roots existed at once (one under the real `$HOME`, one under a
profile `$HOME`) because the root is keyed off a raw path. Clearing both was
the only recovery, and `land` prints no log, so the cause took several runs to
find. Treat the acceptance criteria as describing a live operational failure.

## Primary decisions

- **Prefer isolation over refusal where it is cheap.** When a real cross-process
  lock is unavailable (unsupported platform, or acquisition fails), fall back to
  a **unique throwaway worktree** rather than refusing the gate. The gate still
  runs and stays correct; only the warm-`CARGO_TARGET_DIR` optimization is lost.
  Refuse (`GateVerdict::Error`) only when even the isolated fallback cannot be
  prepared.
- **Key the lock and gate root from canonical repository identity**, not the
  caller's raw path through `DefaultHasher`. Symlinked, relative and
  case-folded aliases must map to the same root. This is the defect that gave
  two gate roots for one repo.
- **Windows must not be silently weaker.** If you cannot implement real Windows
  exclusion inside this lane, the Windows path takes the unique-worktree
  fallback unconditionally and says so in a comment — never the shared worktree.
- **Infrastructure, never blame.** A lock/setup failure is `GateVerdict::Error`
  and must never reach the fixing agent or mark a branch red. The existing code
  already documents this distinction; preserve it.
- **Improve the operator message.** The current headline gives no cause and
  `land` discards the log. Include the concrete reason (which path, which
  errno/step) in the `GateVerdict::Error` reason so one run is enough to
  diagnose. That is in scope and cheap.
- Stale locks after process death must not wedge the gate — an flock released
  by the kernel on exit is fine; a lock file whose mere existence blocks is not.

## Scope

`crates/thegn-host/src/integrate.rs` (and `integrate_gate.rs` if the lock code
lives there on this branch — **verify, the cited line numbers are from audit
commit 299fc13, not current main**), the platform seam for locking, and their
tests. Do not redesign the merge queue.

## Ratchets

Any platform `#[cfg]` belongs behind `platform/` or needs a reasoned entry in
`test/platform-cfg-host-ratchet.txt` (shrink-only). A new `let _ =` / `.ok()`
needs a `// best-effort: <why>` comment.
