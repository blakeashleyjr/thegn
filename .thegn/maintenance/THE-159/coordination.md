# Primary coordination brief — THE-159

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

A stored `u32` PID is cast to `i32` before signalling. Large values become
**negative**, and a negative argument to `kill(2)` means "signal the whole
process group" — so corrupt state can broadcast a signal instead of killing one
proxy. A reused PID can also target an unrelated process.

## Primary decisions — this is a safety fix; be conservative

- **Reject before converting:** zero, anything above the platform's safe
  positive PID range, and anything that would not round-trip `u32 -> i32`
  losslessly. Never cast first and validate after.
- **Never signal a negative PID from stored state.** Process-group signalling
  must be explicit at the call site and only for a group this supervisor
  created and owns. If the code needs group semantics, it says so in the type,
  not via a sign bit.
- **Prove identity before TERM/KILL.** A PID alone is not identity. Bind it to
  the process start time (`/proc/<pid>/stat` field 22 on Linux) or an equivalent
  platform check, and refuse when it cannot be established. Fail closed: a
  process you cannot identify is one you do not signal.
- A vanished process and a stale file are **normal** outcomes, not errors to
  surface loudly — clean up and move on.

## Tests

`u32::MAX`, values that cast to negative, `0`, PID reuse (same PID, different
start time), malformed/empty state file, and a vanished process. Assert that no
signal is sent in every refusal case — asserting "we returned an error" is not
enough; assert the signal did not happen.

## Scope

`crates/thegn-host/src/model_proxy_daemon.rs` and the Unix platform seam
(`crates/thegn-host/src/platform/unix.rs`). Platform code stays behind
`platform/` — that is the seam the ratchet enforces. Do not touch THE-160's
listener-identity work.
