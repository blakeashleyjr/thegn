# PRIMARY AUTHORIZATION — you MAY run `cargo check` AND focused `cargo test`

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary is granting a narrow exception**, because lanes
kept reporting `implementation-ready` for code that did not compile or whose own
new tests failed, and each round-trip costs far more than the checks would.

You are authorized to run **exactly these**, as many times as you need:

```
nix develop --command cargo check -p <crate> --all-targets
nix develop --command cargo test -p <crate> --lib <narrow-filter>
```

`--all-targets` on the check is required: the library frequently builds when the
**test** targets do not. Keep the test filter narrow (your module or your test
names) — it must not become a workspace run.

Still forbidden, and still the primary's job: `cargo build`, unfiltered
`cargo test`, `nextest --workspace`, `clippy`, `just lint`, `just test`,
`just ci`, and anything full-workspace. Do not run them.

**Your row is not finished until the check is clean AND the tests you added or
touched pass.** If you cannot get there inside your approved scope, report the
remaining failures verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build, or whose own tests fail,
is not.

Report what you actually ran. `rustfmt` passing is evidence of formatting only.

---

# Primary revision brief — THE-210 (round 2): the branch does not compile

The design is **accepted** — canonical Git common directory plus a stable
SHA-256 `RepositoryId`, isolation fallback on lock/state failure, private target
dir for the isolated path, Windows isolating before the Unix-only seam, and
concrete reasons reaching `GateVerdict::Error`. All of that matches the
greenlight. Do not re-open any of it.

But `cargo check -p thegn-host --all-targets` fails with **12 errors**, all the
same root cause:

```
error[E0425]: cannot find function `gate_base_for_repo` in this scope
  --> crates/thegn-host/src/integrate_gate_tests.rs:780
  --> crates/thegn-host/src/integrate_gate_tests.rs:950
  --> crates/thegn-host/src/integrate_gate_tests.rs:1007
  (and more)
```

Your tests in `integrate_gate_tests.rs` call `gate_base_for_repo`, but no such
function exists anywhere in the tree — the identity helper you actually wrote
has a different name or visibility. Reconcile the two: either expose the real
helper under the name the tests use, or update the tests to call what you built.
Pick whichever leaves the production API cleaner and say which.

While you are there, confirm the helper is reachable from the test module at the
visibility you intend — this is exactly the kind of thing that compiles for the
lib and fails for the test target, which is why `--all-targets` is required.

## Run these before reporting

```
nix develop --command cargo check -p thegn-host --all-targets
nix develop --command cargo nextest run -p thegn-host gate
```

You are authorized for both (see the header). **The row is not finished until
both are clean.** Four of the five batch-2 lanes reported `implementation-ready`
without running anything; yours is the one that did not compile.

## Do not change anything else

No behavioural change this round. If making it compile forces a design change,
say so explicitly rather than quietly altering the approved shape.
