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

# Primary revision brief — THE-488 (round 5): char literals break the stripper

Your round-4 tests all pass (15/15 core, 13/13 media, 13/13 metrics, copies
byte-identical). But when the primary merged this lane and ran the **whole
workspace**, the repaired scanner produced a new failure:

```
ratchet test/glyph-literal-ratchet.txt: new violation in ["task.rs"]
```

## Diagnosis (done by the primary — the finding is real, the flag is spurious)

The only box-drawing glyphs in `crates/thegn-host/src/task.rs` are in
**comments** — section dividers at `:1979` and `:2335`, of the form
`// -- Task auto-discovery -----`.

`code_only` should have stripped those. It did not, because of
`task.rs:878`, which builds an array of char literals whose first element is a
char literal **containing a double quote**.

**`code_only` has no char-literal handling.** Reading
`crates/thegn-core/src/test_support/ratchet.rs:263-310`, it handles raw
strings, normal strings, byte strings, and both comment forms — but a single
quote is just an ordinary byte. So at that char literal it sees the inner
double quote, enters `quoted_string_end`, and scans forward for a closing quote
that is not there. Everything after is treated as string content, comments stop
being stripped, and the comment dividers reach the hit predicate.

**This matters in both directions.** Here it produced a false positive. The same
mis-tracking will equally _hide_ a real platform cfg that follows a char
literal — a false negative, which is precisely the class of bug this issue
exists to eliminate. Fixing it is in scope, not an extra.

## Required

1. Handle char literals in `code_only`: ordinary ones, an escaped quote, an
   escaped backslash, `\n`, and unicode escapes. Consume the literal whole so
   its contents can never open a string.
2. **Do not confuse a lifetime with a char literal.** `&'static str`, `<'a>`
   and `'a: 'b` are not literals and must not start one. The usual
   discriminator: a quote begins a char literal only when the matching closing
   quote appears within escape-aware bounds; otherwise it is a lifetime. Get
   this right — treating a lifetime as an unterminated literal would
   reintroduce the same mis-tracking with a different trigger.
3. Add direct regressions for: a char literal holding a double quote, an
   escaped quote, an escaped backslash, a unicode escape, a lifetime followed
   by a comment that must still be stripped, and a platform cfg appearing
   _after_ a char literal (the false-negative direction). Assert both
   `code_only` output and `has_platform_cfg`.
4. Re-mirror all three copies and re-check `thegn-core`, `thegn-media` and
   `thegn-metrics`.

## Then reconcile, as the original brief required

After the fix, `task.rs` should no longer be flagged — its glyphs are
comment-only. **Do not pin `task.rs`**: that would freeze a scanner bug as if it
were debt. If anything else is still newly flagged once the stripper is sound,
report it with your reasoning rather than mass-pinning.

Everything else from rounds 2-4 stands.
