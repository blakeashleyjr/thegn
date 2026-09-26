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

# Primary revision brief — THE-316 (round 4): blocker resolved

Round 3's classification work is **accepted**: `NotApplied` narrowed to
400/401/403/404/409/422, everything else (408, 418, other 4xx, 5xx,
intermediary) falling to `Unknown` and disambiguated by the final fetch, and the
typed POST error preserved so `is_transient` survives. That is exactly right.

## Your blocker is resolved — and the fix is better than the message

You reported that the bounded "verify before retrying" message cannot coexist
with the typed source in the current `PartialUpdate` shape, and that adding it
needs `crates/thegn-svc/src/issue/mod.rs`, outside round 3's scope.

Correct, and well stopped. **The primary authorizes the `issue/mod.rs` change** —
but do not add a message field. Add the **third state** instead:

```rust
PartialUpdate {
    applied:    Vec<&'static str>,
    unapplied:  Vec<&'static str>,
    unverified: Vec<&'static str>,   // NEW
    source:     Box<IssueError>,
},
```

`unverified` carries the fields whose outcome could not be determined — the
`Unknown` case. This is strictly better than a prose message: the whole point of
this issue is that a caller must be able to _branch_ on what happened, and
"verify before retrying" in a string is not branchable. Render the advisory
wording from `unverified` in the `Display` impl, so the human message and the
machine-readable state cannot drift apart.

Update the `Debug` impl, `source()`, and `class()` arms at
`issue/mod.rs:54/89/111/140` accordingly, plus every construction site.

Invariant to assert in a test: a field appears in **at most one** of
`applied` / `unapplied` / `unverified`, and a status transition that timed out
lands in `unverified` — never in `unapplied`.

## Scope

`crates/thegn-svc/src/issue/jira.rs`, `crates/thegn-svc/src/issue/mod.rs`, and
their tests. The control-wire item stays **deferred** (see the round-2
addendum) — adding `unverified` to the typed error does not oblige you to plumb
it through `ErrorBody`, and you must not change `docs/api/control-v1.json`. If
adding the field somehow forces a snapshot change, STOP and report it.

Everything else from rounds 2-3 stands: title-then-status order, no raw provider
bodies in errors, no retry/poll loop, bounded credential-free messages.
