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

# Primary revision brief — THE-210 (round 3)

It compiles now. Two of the lane's own tests fail, and **one of them is a real
behavioural defect**, not a harness problem. The primary ran:

```
nix develop --command cargo nextest run -p thegn-host gate
→ 40 run: 38 passed, 2 failed
```

## F1 (BEHAVIOURAL — fix this first)

```
integrate::gate_runner::tests::gate_isolation_never_advances_or_blames_candidate
  panicked at integrate_gate_tests.rs:993: assertion failed: !report.advanced
```

The isolation path **advanced the target ref**. That is precisely the property
this whole issue exists to guarantee: acceptance criterion 6 says lock/setup
failures surface as `GateVerdict::Error`, and the module's own doctrine is that a
`GateError` says _nothing_ about the branch — so it must never advance and never
blame the candidate.

Work out which it is and say so explicitly:

- the isolation fallback is reaching the CAS advance when it should stop, or
- the fallback is being treated as a successful gate rather than an
  infrastructure hold, or
- the test's scenario does not actually trigger isolation, so it is asserting
  against the wrong path.

If it is the third, fix the test so it genuinely exercises isolation — do not
relax the assertion. `!report.advanced` is the invariant; it is not negotiable.

## F2 (harness)

```
integrate::gate_runner::tests::root_git_mapping_is_revalidated_before_reporting_success
  panicked at integrate.rs:699: test repository must have a canonical Git
  common directory: GitProbeFailed { operation: "rev-parse --git-common-dir" }
```

Your `#[cfg(test)]` helper at `integrate.rs:698-701` does:

```rust
let identity = thegn_core::repo::repository_id(repo_root)
    .expect("test repository must have a canonical Git common directory");
```

The fixture that test uses is not a repo the probe can resolve. Either give the
fixture a real common dir, or have the helper return `Option`/`Result` and let
the test state what it expects. **Do not** paper over it by widening
`repository_id` to accept a non-repo — production must keep failing closed when
it cannot establish canonical identity, and a test helper's `expect` must not be
the thing that defines that contract.

## Everything else stands

The approved design is unchanged: canonical Git common directory plus stable
SHA-256 `RepositoryId`, isolation on lock/state failure, private target dir for
the isolated path, Windows isolating before the Unix-only seam, concrete
sanitized reasons reaching `GateVerdict::Error` and `land`'s output.

## Run before reporting

```
nix develop --command cargo check -p thegn-host --all-targets
nix develop --command cargo nextest run -p thegn-host gate
```

Both must be clean. This is round 3 on the batch's most safety-critical lane —
report remaining failures verbatim as a blocker rather than claiming ready.
