# Primary coordination brief — THE-685

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

## This one is live. The user reported it while the batch was running.

35 merged worktrees are stuck in their sidebar right now, the oldest 9 days
past its TTL, and `--force` cannot clear them. The primary already diagnosed it
and the evidence below was gathered on **current main**, not an audit commit —
so unlike the rest of this batch, these citations are fresh. Verify them, but
expect them to hold.

## Confirmed diagnosis

`crates/thegn-host/src/merge_cleanup.rs:276-284` runs
`git config --null --includes --list` and refuses if any key matches
`filter.*.clean` / `filter.*.process`.

`git config --list` includes **global and system** scope. Installing git-lfs
machine-wide sets those keys permanently, so merged-worktree cleanup is
disabled for **every** repository on the machine — including ones with no LFS
content whatsoever.

Measured on this repo:

- `git ls-files -z ':(attr:filter)'` → **0 files**
- no `.gitattributes` anywhere in the tree
- `git config --get-regexp '^filter\.'` → `filter.lfs.{clean,process,smudge,required}` from `~/.gitconfig`

## Primary decisions

- **Keep the refusal. Fix the predicate.** The comment above the check is right
  that `git status` can invoke clean/process drivers while refreshing the index,
  and that `fsmonitor=false` alone does not make that safe. Do not delete the
  guard or downgrade it to a warning.
- **Ask the right question: does any path in THIS worktree carry a `filter`
  attribute?** `git ls-files -z ':(attr:filter)'` answers it in one call.
  Non-empty ⇒ refuse exactly as today. Empty ⇒ no path can invoke a driver, so
  proceed.
- **The probe must not itself run a filter.** `ls-files` with pathspec magic
  reads attributes and the index; it does not smudge/clean content. Do not
  implement the probe with `git status`, `git add`, or anything that touches
  worktree content.
- **Keep config values data-only.** The existing code is careful never to log
  config values; preserve that. You may name _whether_ filters apply, never
  their command lines.
- **Improve the message.** Today an operator cannot tell "this repo uses
  filters" from "some filter exists on this machine" — which is exactly why this
  took a manual diagnosis. Say which tracked paths triggered it (bounded count
  or first few paths), not the driver config.
- **Change nothing else in `clean()`.** The `skip-worktree`/`assume-unchanged`
  refusal, the submodule check, and every other guard stay byte-for-byte as they
  are. This is a one-predicate fix.

## Tests

Three fixtures, all with `commit.gpgsign=false` (see the note below):

1. Filter driver configured in **global/local config only**, no `.gitattributes`
   → cleanup proceeds.
2. A `.gitattributes` assigning `filter=lfs` to a tracked path → still refuses,
   same message class.
3. No filter config and no attributes → proceeds.

Set the filter config in the **fixture's own** config so the test does not
depend on the developer's machine having git-lfs.

**Fixture hygiene:** this repo's git fixtures must pass
`-c commit.gpgsign=false -c tag.gpgsign=false`. A fixture that inherits a real
`~/.gitconfig` with `commit.gpgSign = true` blocks on gpg-agent and fails ~60s
later — that exact bug was fixed in `forge/checkout.rs` and `github.rs` last
batch; do not reintroduce it.

## Scope

`crates/thegn-host/src/merge_cleanup.rs` and its tests. Do not touch the sweep
scheduling, the TTL policy, or `on_landed`.

## Out of scope

The other refusal seen in the same run — `landed commit identity is missing`
for `tg/spark-radar`, `tg/bold-petal`, `tg/keen-marble`, `tg/bold-mango` — is a
**different** cause. Do not try to fix it here. If you understand it while
reading the code, record it as a follow-up finding.
