# PRIMARY AUTHORIZATION — you MAY run `cargo check` this round

The stage prompt says not to run Cargo because the primary centrally schedules
Rust validation. **The primary is granting a narrow exception for this round**,
because three lanes in a row reported `implementation-ready` for code that does
not compile, and each round-trip costs far more than the check would.

You are authorized to run **exactly this**, as many times as you need:

```
nix develop --command cargo check -p <crate> --all-targets
```

`--all-targets` is required: the library frequently builds when the **test**
targets do not.

Still forbidden, and still the primary's job: `cargo build`, `cargo test`,
`nextest`, `clippy`, `just lint`, `just test`, `just ci`, and anything
full-workspace. Do not run them.

**Your row is not finished until `cargo check -p <crate> --all-targets` exits
clean.** If you cannot make it clean within your approved scope, report the
remaining errors verbatim as a blocker — that is a good outcome. Reporting
`implementation-ready` for code that does not build is not.

Report what you actually ran. `rustfmt` passing is not evidence of compilation.

---

# Primary revision brief — THE-189 (round 3): make it compile

Round 2 implemented F-001..F-005 and the primary's review of the _design_ is
positive — the latest-value slot, untagged-wins coalescing, publication fences
and the reminder-cursor release are all the right shapes. **But the branch does
not build.** Fix exactly these, change nothing else, then re-run the check.

`cargo check -p thegn-host --all-targets` fails with:

1. **`run.rs:11792` — non-exhaustive match: `RefreshKind::Scheduled { .. }` not
   covered** (twice, once per import path). You added the `Scheduled` variant at
   `hydrate.rs:204` but did not extend the match at `run.rs:11792`.
   Handle it explicitly — do **not** add a `_ => {}` arm. An exhaustive match is
   what forces the next person who adds a refresh kind to think about fencing,
   and a wildcard would silently swallow it.

2. **`hydrate_refresh_ticker.rs:381` — mismatched types: expected `u64`, found
   `Arc<Atomic<u64>>`.** You are passing the shared generation handle where the
   value is wanted. Load it (`.load(Ordering::…)`) at the call site, with the
   ordering that matches how the slot is published.

3. **`hydrate_calendar_tests.rs:674` — this function takes 7 arguments but 6
   were supplied** (`hydrate_calendar.rs:381`). You added a parameter (the
   generation) to the production function and did not update this caller.

4. **`hydrate_calendar.rs:720` — borrow of moved value: `generation`.** The
   generation is moved into a closure/await and then borrowed afterwards. Clone
   it (it is cheap) or restructure so the borrow precedes the move.

## Do not change anything else this round

The accepted F-001..F-005 behaviour stands as implemented. This round is a
compile fix. If making it build forces a behavioural change to any of the five,
say so explicitly in your report rather than quietly altering it.

All the round-2 constraints still hold: sibling module only, 0% idle, pulse the
waker, no blocking I/O on the loop, `render_plan::plan` stays pure, fake time.
