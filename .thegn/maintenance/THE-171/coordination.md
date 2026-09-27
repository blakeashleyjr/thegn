# Coordination brief — THE-171

Daemon shutdown does not signal owned PTY process groups, so children can outlive
the daemon and keep mutating worktrees. The issue also carries a second,
independently confirmed finding that is arguably worse: a **PID-reuse TOCTOU** in
ordinary pane teardown.

## Read the second finding carefully — it is the sharper half

- `pane.rs:204-221` reads `child_reaped`, then signals a numeric process group.
- `pane_pty.rs:207-210` calls `child.wait()` — **releasing the PID** — before
  publishing `reaped = true`.
- `platform/unix.rs:373-404` holds a numeric PGID and calls `killpg`.

So between the boolean check and the signal, the PID can be reused, and thegn can
signal an unrelated process. The issue notes that existing comments claiming the
check prevents reuse are **stronger than the implementation** — fix the code or
the comment, never leave them disagreeing.

## Hard requirements

- Serialize signalling with ownership/reaping, **or** acquire a stable platform
  process identity before the reaper starts. An `AtomicBool` cannot preserve OS
  process identity across concurrent reaping.
- Cover **ordinary pane `Drop`** and a simultaneous daemon Kill/Exit, not only
  whole-daemon shutdown.
- Bounded TERM → drain → KILL, with verification that survivors actually died.
- **Deterministic tests must never target an unrelated process.** Do not write a
  test that could signal something outside its own fixture; a flaky test here
  kills a developer's editor.
- Platform-conditional code belongs in `src/platform/` — there is a shrink-only
  ratchet enforcing it, and `platform/unix.rs` is already an entry on it.

Note THE-577's fixtures will move to a private PID-namespace supervisor. **That
does not fix this**, and this issue must not be declared fixed by test-harness
changes.

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
