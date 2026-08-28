# THE-77 chunk 1 — Re-arm three gates that ARCHITECTURE.md claims are enforced

**Findings:** F1, F2, F3 of `.thegn/pipeline/THE-77/architect/design.md`.

Read the design's F1/F2/F3 sections before starting — they carry the evidence and
the reproduction for each claim.

## Files touched (exact)

- `test/ratchet.sh`
- `crates/thegn-core/tests/crate_boundaries.rs`
- `crates/thegn-proxy/Cargo.toml`

## Overlap / dependency

**None with chunk 2 or chunk 3 — fully file-disjoint, runs in parallel with
both.** Do not touch the `justfile` (chunk 2 edits it) or any file under
`crates/thegn-host/` or `docs/help/`.

Do **not** run `just ratchet-update` or regenerate any `test/*-ratchet.txt`. Fix
(a) below changes only how the lists are _compared_, never their contents; every
list must be byte-identical before and after this chunk.

---

## (a) F1 — `test/ratchet.sh` sorts by locale and compares by byte

`test/ratchet.sh:32` and `:49` build the hit list and the allowlist with a bare
`sort -u`, which honours `LC_COLLATE`. `test/ratchet.sh:53-54` then compares them
with `comm`, which needs byte order. On a `en_US.UTF-8` machine the two disagree
(`.` and `_` are ignored at the primary collation level), so `comm` prints
`comm: file 1 is not in sorted order` on every run and can emit a **spurious
stale-entry error naming a still-violating file** — instructing the maintainer to
delete a load-bearing allowlist line.

### Approach

Force byte ordering everywhere the script sorts or compares. The minimal, most
robust form is to set the collation locale once, near the top of the script
(just after the existing `set -euo pipefail` / `cd` preamble), rather than
sprinkling `LC_ALL=C` per pipeline:

```sh
# Byte order, not locale order: `comm` below compares bytes, so the `sort`s that
# feed it must too. Under a UTF-8 collation `.`/`_` sort at a different primary
# level than they compare, which made `comm` report an unrelated still-violating
# file as a stale entry (THE-77 F1).
export LC_ALL=C
```

`LC_ALL=C` is safe for everything this script does — it only greps ASCII
patterns, cuts paths and sorts. Do not narrow it to `LC_COLLATE` only unless you
verify `comm`, `sort` and `comm`'s input validation all agree under that
narrower setting.

### Verification (do this — it is cheap and it is the whole point)

```sh
bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates
```

Before: prints four `comm: … is not in sorted order` lines, then
`ratchet(ignored-result): clean (323 pinned)`.
After: **no `comm:` warnings**, still `clean (323 pinned)`.

Then confirm the gate still detects a real violation and no longer invents a
stale one:

```sh
cp test/ignored-result-ratchet.txt /tmp/irr.bak
grep -v '^crates/thegn-core/src/config_resolve.rs$' /tmp/irr.bak > test/ignored-result-ratchet.txt
bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates; echo "rc=$?"
cp /tmp/irr.bak test/ignored-result-ratchet.txt
```

Expected after the fix: `rc=1`, **exactly one** error line — a new violation in
`crates/thegn-core/src/config_resolve.rs` — and **no** "stale entry
crates/thegn-core/src/config.rs" line. (Before the fix it wrongly prints both.)
Confirm `git diff --stat test/` shows the allowlist restored to unchanged.

Also run the four other bash ratchets and confirm each is `clean` with no `comm:`
noise:

```sh
bash test/ratchet.sh forge-leak 'thegn_core::github::|use thegn_core::github|Command::new\("gh"\)' crates/thegn-host/src crates/thegn-svc/src crates/thegn-core/src
bash test/ratchet.sh async-trait '#\[allow\(async_fn_in_trait\)\]' crates
bash test/ratchet.sh json-emit 'serde_json::to_string(_pretty)?\(' crates/thegn-host/src/cmd ':!crates/thegn-host/src/cmd/mod.rs'
bash test/ratchet.sh element 'draw_text\(' crates/thegn-host/src ':!crates/thegn-host/src/logotype.rs' ':!crates/thegn-host/src/loading/screen.rs' ':!crates/thegn-host/src/chrome_tests.rs'
```

`shellcheck` runs on this file in pre-commit — keep it clean.

---

## (b) F2 — `gix` has no `OWNERS` row

`docs/ARCHITECTURE.md:30-32` lists `gix` among the substrates and says "each
substrate has exactly the owner crates listed in
`crates/thegn-core/tests/crate_boundaries.rs`". `OWNERS`
(`crate_boundaries.rs:28-50`) has no `gix` row, so
`substrates_are_only_used_by_their_owners` (`:112`) never checks it. `gix`
appears only in `CORE_FORBIDDEN` (`:66`), which by its own doc comment
(`:52-54`) constrains `thegn-core` and nothing else.

### Approach

Add one row to `OWNERS`, after `("alacritty_terminal", &["thegn-host"])`:

```rust
    ("gix", &["thegn-svc"]),
```

`crates/thegn-svc/Cargo.toml:37` declares `gix.workspace = true` and it is the
only member that does — so `substrates_are_only_used_by_their_owners`'s
"an owner that no longer uses the substrate should be removed" assertion
(`:120-133`) is satisfied and the test passes with a zero-diff outcome today.

Leave `"gix"` in `CORE_FORBIDDEN`: the two lists express different rules and the
constant's doc comment already says so.

---

## (c) F3 — `thegn-proxy` never opted into `[workspace.lints]`

`Cargo.toml:241` declares `let_underscore_future = "deny"` — the gate
ARCHITECTURE.md §9 names. Cargo applies `[workspace.lints]` only to members that
declare `[lints] workspace = true`. Eleven of twelve members do;
`crates/thegn-proxy/Cargo.toml` has no `[lints]` section at all — and it is the
one member that is entirely async I/O (tokio + axum + reqwest), i.e. where a
dropped future is most likely.

No dropped future exists today (`relay.rs:338,343`, `lib.rs:72,83`,
`router.rs:290,466` are all `.await`ed) — this re-arms the tripwire.

### Approach

Two parts.

**1. Opt the crate in.** Add to `crates/thegn-proxy/Cargo.toml`, matching the
placement the other manifests use (immediately before `[[bin]]` / `[dependencies]`):

```toml
[lints]
workspace = true
```

**2. Pin the invariant so member #13 cannot repeat it.**
`crate_boundaries.rs` already parses every member manifest — `members()` at
`:74-105` — and already owns the "a new crate must be placed" rule in
`every_member_is_covered` (`:159`). Add a sibling test there. `members()`
currently returns only dependency names, so read the manifests directly rather
than reshaping it:

```rust
/// `[workspace.lints]` (notably `let_underscore_future = "deny"`, the gate
/// ARCHITECTURE.md §9 names) applies ONLY to members that opt in with
/// `[lints] workspace = true`. A member that forgets silently loses every
/// workspace lint — which is exactly what happened to `thegn-proxy` (THE-77 F3).
#[test]
fn every_member_inherits_workspace_lints() { … }
```

Implement it by walking `workspace.members` from the root `Cargo.toml` (the same
way `members()` does at `:74-84`), parsing each member manifest, and asserting
`manifest["lints"]["workspace"] == true`. Collect every offender into a `Vec` and
assert once with a message naming the file and the fix, in the style of the
existing assertions in this file — do not `panic!` on the first.

## Tests to run (scoped — no full-workspace gates)

```sh
# (a) — see the verification block above; no compile needed.

# (b) + (c) — the boundary tests are an integration test on thegn-core:
cargo nextest run -p thegn-core --test crate_boundaries

# (c) — confirm the newly-armed lints do not fail the proxy build:
cargo clippy -p thegn-proxy --all-targets -- -D warnings
```

If `cargo clippy -p thegn-proxy` surfaces a genuine `let_underscore_future`
violation, **fix the code** (`.await` it, or `tokio::spawn` it if it is meant to
be detached) — do not `#[allow]` it, and do not revert the opt-in. If it surfaces
something unrelated and non-trivial, stop and report rather than widening the
chunk.

Do not run `just test`, `just lint`, `just ci`, `just coverage`, or e2e.

## Done criteria

- `bash test/ratchet.sh …` for all five bash ratchets: `clean`, **zero**
  `comm: … not in sorted order` lines.
- The negative test in (a) reports exactly one new violation and zero stale
  entries; `test/ignored-result-ratchet.txt` is byte-identical to `HEAD`.
- No `test/*-ratchet.txt` file is modified by this chunk (`git status` shows
  only the three files listed at the top).
- `cargo nextest run -p thegn-core --test crate_boundaries` passes, including the
  new `every_member_inherits_workspace_lints`.
- `cargo clippy -p thegn-proxy --all-targets -- -D warnings` passes.
- `shellcheck test/ratchet.sh` clean (pre-commit runs it).
- Sanity-check the new test actually fires: temporarily delete the `[lints]`
  block from `crates/thegn-proxy/Cargo.toml`, confirm
  `every_member_inherits_workspace_lints` **fails** naming that file, then
  restore it.

**Exact commit subject (use verbatim):**

```
fix(the-77): re-arm three gates — ratchet.sh byte order, gix owner row, proxy workspace lints
```
