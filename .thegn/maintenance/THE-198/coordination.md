# Primary coordination brief — THE-198

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

## The task

`deny.toml` globally ignores `RUSTSEC-2023-0071` (the RSA "Marvin" timing
side-channel). A blanket ignore cannot prove the affected private-key
operations never sit behind a hostile timing boundary, and it silently covers
future growth.

**This lane is mostly analysis. That is the deliverable — do not force a
dependency bump that does not exist.**

## Primary decisions

1. **First establish reachability, with evidence.** Which crate pulls `rsa`,
   which operations are reachable, and from which call sites. `cargo tree
-i rsa` plus call-site reading. Write the path from entry point to trust
   boundary in the artifact. If it turns out to be unreachable in the shipped
   binary, that is the finding and the exception can be narrowed to nothing.
2. **Prefer removal over acceptance.** If a patched version or a drop-in
   replacement exists for the actual use (JWT/credential paths), take it.
   Check whether the dependency is optional or feature-gated — dropping the
   feature may be cheaper than replacing the crate.
3. **If neither: narrow the exception**, do not keep it global. Scope it to the
   exact package and version, with an owner, an expiry/review trigger, and a
   link. A `deny.toml` ignore with no expiry is how this became invisible.
4. **Add the guard.** A test must fail if the affected code becomes reachable
   from a remote/untrusted request path. Without that, the risk acceptance
   expires silently the next time someone wires it up.

**Do not** weaken `deny.toml` in any other respect, and do not add new ignores
for unrelated advisories you happen to notice — report those separately.

## Scope

`deny.toml`, whatever dependency change follows, and a reachability test.
Record exact resolved versions in your artifact.
