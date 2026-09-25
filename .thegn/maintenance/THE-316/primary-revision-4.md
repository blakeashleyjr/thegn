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

# Primary revision brief — THE-316 (round 5): your own tests fail

The design is **accepted and complete**. `PartialUpdate.unverified` is the right
shape, the 400/401/403/404/409/422 narrowing is right, the typed source is
preserved, and the control wire is correctly untouched. Nothing about the
approach needs to change.

What remains is that the lane's own tests do not pass. Run them with the
authorization above:

```
nix develop --command cargo test -p thegn-svc --lib issue::jira
```

Six failures, all in `crates/thegn-svc/src/issue/jira.rs`:

```
request_timeout_transition_failure_is_not_reported_as_unapplied
ambiguous_transition_failure_is_not_reported_as_unapplied
stale_transition_id_propagates_post_failure_without_final_fetch
partial_status_failure_can_retry_only_the_unapplied_field
title_failure_does_not_attempt_status_transition
transition_lookup_http_failure_is_propagated_without_followup
```

A representative one, at `jira.rs:1202`:

```
assertion failed: matches!(result, Err(IssueError::Api(message)) if message == "jira HTTP 502")
```

Six failures sharing a shape usually means **one** cause in the shared fixture
rather than six separate bugs — likely the local HTTP fixture's response
wiring, or an error-message/variant shape that moved when you added
`unverified`. Find the common cause first; do not patch six assertions
individually.

Two rules while fixing:

- A bare `matches!(…)` assertion prints nothing useful when it fails, which is
  why this took a round-trip to diagnose. Where you touch these, assert on the
  actual value (`assert_eq!` on the error, or destructure and compare) so the
  next failure reports what it got.
- **Do not weaken an assertion to make it pass.** If the production behaviour
  genuinely changed for a good reason, say so explicitly and justify it; if the
  expectation was wrong, fix the expectation and say why it was wrong.

## Scope

`crates/thegn-svc/src/issue/jira.rs`, `crates/thegn-svc/src/issue/mod.rs`, and
their tests. The control wire and `docs/api/control-v1.json` stay untouched.
Everything from rounds 2-4 stands.
