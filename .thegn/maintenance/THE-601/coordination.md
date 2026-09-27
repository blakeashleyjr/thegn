# Coordination brief — THE-601

`integrate.rs::gate_tip` runs setup and gate through `Command::output()`, which
buffers both complete streams until exit. A noisy gate grows host memory without
bound, and there is no gate deadline — a grandchild holding a pipe can keep the
reusable gate lease indefinitely.

## Context you should have

The primary has just spent a long session in this exact area. Relevant landed
work, all of which you must preserve:

- **THE-210** made gate worktree selection fail closed or isolate, and keyed the
  gate root from a canonical repository identity.
- **THE-589** bounds _retained diagnostics_ — explicitly **not** this initial
  allocation. Do not mistake one for the other.
- `thegn land`'s gate is the thing that decides whether a branch may advance
  `main`, so a wrong verdict here is worse than a slow one.

## Hard requirements

- **Exit classification is sacred.** Timeout, transport and setup failures are
  **infrastructure holds** — never candidate blame, never a pass. The existing
  code goes out of its way to say so because calling an infra failure a red gate
  falsely accuses a good branch.
- **Do not change the user's configured gate command**, and do not impose a
  default deadline short enough to kill a legitimate build. An operator-
  configurable deadline, or a documented disabled default.
- Cancellation must **retain ownership**: process-group termination, reaping, and
  pipe-reader settlement. Do not signal a reaped or reused PID, and do not detach
  an unbounded reader thread.
- This crate **bans blocking child waits** outside sanctioned off-loop sites —
  read `crates/thegn-host/clippy.toml` before adding a subprocess call, and follow
  the existing `#[expect(clippy::disallowed_methods)]` + one-line-justification
  pattern rather than inventing one.
- Be honest about OS limits: a process blocked in uninterruptible I/O cannot be
  cancelled, and the comment should say so rather than implying a guarantee.

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
