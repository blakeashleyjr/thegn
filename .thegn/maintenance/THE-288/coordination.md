# Primary coordination brief — THE-288

## PRIMARY AUTHORIZATION — you MAY run `cargo check` and focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary grants a narrow exception**, because the previous
batch repeatedly returned `implementation-ready` for code that did not compile,
and each round-trip costs far more than the checks would.

Authorized, as often as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` is required — the library often builds when the **test** targets
do not. Keep the test filter narrow; it must not become a workspace run.

Still forbidden, and still the primary's job: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, clippy, `just lint`, `just test`, `just ci`,
and anything full-workspace.

**Your row is not finished until the check is clean and the tests you added or
touched pass.** If you cannot get there inside your approved scope, report the
remaining failures verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build is not. `rustfmt` passing is
evidence of formatting only.

## Line numbers in the issue are STALE

Every citation comes from audit commit `299fc13`, not current `main`. The
previous batch found three issues whose headline defect was already fixed and
two whose file inventory was wrong. **Re-verify each citation on this branch and
report already-met criteria as met, with evidence, rather than re-fixing them.**

---

## The defect

Every clipboard copy spawns a detached OS thread that tries several helper
programs and blocks on `child.wait()` with **no deadline**. A hung helper leaks
a thread, a process, pipes, and a full copy of the clipboard payload — per copy.

## Primary decisions — scope this tightly

- **One bounded worker with a latest-wins queue**, not a thread per copy. A
  clipboard has one current value; an older pending copy is worthless. State the
  policy in a comment.
- **Deadlines on all three phases**: spawn, stdin write, and exit. A helper that
  never reads stdin must not block the worker.
- **Kill and reap the whole helper tree** on timeout/shutdown/replacement. On
  Unix use the process group. **THE-274 (Windows Job Object containment) is
  unlanded** — do not build Windows containment here. Scope Windows to the
  existing behaviour plus the deadline, and record the gap as an explicit
  follow-up referencing THE-274.
- **Bound the retained text** and keep clipboard content out of diagnostics
  entirely — not truncated, not redacted-in-place: absent. A clipboard carries
  passwords.
- Report real async success/failure to the caller. "Thread spawned" is not
  success. Do not put the wait on the event loop.

## Tests

Repeated copy bursts do not grow threads/processes. A helper that never reads
stdin and never exits is cancelled within the bound. Shutdown leaves nothing
alive. Failure reaches the caller without exposing content.

## Scope

`crates/thegn-host/src/clipboard.rs` and its tests, plus the platform seam for
process-group kill. Do not touch THE-152's terminal-write work.
