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

# Primary revision brief — THE-488 (round 4): your own new tests fail

The compile fix worked and the design is accepted. But four of the tests **this
lane added** fail. Run them with the authorization above and make them pass:

```
nix develop --command cargo test -p thegn-core --lib ratchet
```

## The four failures, with the primary's reading

1. **`code_only_strips_comments_without_touching_strings`**

   ```
   left:  "let s = \"\""
   right: "let s = \"\"// stays\";  #[cfg(unix)]"
   ```

   The stripper ends the string literal at the **escaped** quote. `\"` inside a
   string is an escaped quote, not a terminator — you must consume the
   backslash-escape while scanning a literal. This is the exact
   string-literal-awareness the issue asked for, so fix the **stripper**, not
   the expectation.

2. **`explicit_empty_marker_allows_zero_hits_and_update_preserves_it`**

   ```
   left:  "# header\n\n# RATCHET-EMPTY\n"
   right: "# header\n# RATCHET-EMPTY\n"
   ```

   A blank-line discrepancy when the updater rewrites a file that keeps the
   marker. Decide the canonical form, make the writer emit it, and assert that.
   Whichever you choose, `update` must be **idempotent** — writing twice must
   not keep adding blank lines. Add that assertion.

3. **`symlinked_source_root_fails_closed_without_external_traversal`**

   ```
   called `Result::unwrap()` on an `Err`: NotFound
   ```

   at `ratchet.rs:829:54` — this is the **test's own setup** unwrapping a path
   that does not exist, not the production refusal. Fix the fixture so it
   actually builds the symlinked root it intends to test, then assert the
   refusal.

4. **`filesystem_failures_are_not_silently_omitted`**
   ```
   normalize source path outside.rs relative to …/crates/x/src: prefix not found
   ```
   The fixture places `outside.rs` outside the source root and the normalizer
   rejects it — which is arguably correct fail-closed behaviour. Decide what
   this test is really asserting: if it means to prove an unreadable file fails
   the ratchet, inject an unreadable file **inside** the root; if it means to
   prove an out-of-root path is refused, assert that refusal instead of treating
   it as a setup step.

## Do not change anything else

F1/F2/F3 behaviour, the explicit-empty marker, symlink refusal, the documented
concurrency limitation, `profile.rs`'s written allowlist reason, and the
byte-identical helper copies all stand. **Remember all three copies** — fix once
and mirror it, and re-check `thegn-core`, `thegn-media` and `thegn-metrics`.
