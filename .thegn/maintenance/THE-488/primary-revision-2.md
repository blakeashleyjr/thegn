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

# Primary revision brief — THE-488 (round 3): make it compile

Round 2's fail-closed work is the right shape — `# RATCHET-EMPTY`, empty-hit
protection, `symlink_metadata` refusal, the documented concurrency limitation,
and the byte-identical helper copies all match the accepted brief. **But the
branch does not build.**

`cargo check -p thegn-core --all-targets` fails with:

```
error[E0308]: mismatched types
   --> crates/thegn-core/src/test_support/ratchet.rs:112:5
111 | pub fn allowlist(manifest_dir: &str, name: &str) -> BTreeSet<String> {
112 |     read_allowlist(&RealIo, manifest_dir, name).unwrap_or_else(|error| panic!("{error}"))
    |     expected `BTreeSet<String>`, found `Allowlist`
```

`read_allowlist` now returns your new `Allowlist` type (which carries the
explicit-empty marker), but the public `allowlist()` wrapper still declares
`BTreeSet<String>`.

Resolve it deliberately rather than by papering over it:

- If callers of `allowlist()` need to know about the explicit-empty state,
  change the wrapper's return type to `Allowlist` and update its callers.
- If they genuinely only want the entries, project explicitly (e.g. return
  `Allowlist::entries()`), and make sure the fail-closed check still happens
  **inside** `read_allowlist` so projecting cannot bypass it.

Whichever you choose, the F1 guarantee must survive: an empty or comment-only
allowlist without the explicit marker is an error, and an empty hit set fails
the ratchet. A projection that throws away the marker and returns an empty set
would reintroduce exactly the bypass this round closed. Say which you chose and
why.

**Remember the three copies.** `thegn-core`, `thegn-media` and `thegn-metrics`
each carry this helper and the identity test asserts they are byte-identical —
fix all three the same way, and check each crate:

```
nix develop --command cargo check -p thegn-core   --all-targets
nix develop --command cargo check -p thegn-media  --all-targets
nix develop --command cargo check -p thegn-metrics --all-targets
```

## Do not change anything else this round

F1/F2 behaviour and the F3 documentation stand as implemented. `profile.rs`
stays reconciled by its written allowlist reason. No Rust-parser dependency.
