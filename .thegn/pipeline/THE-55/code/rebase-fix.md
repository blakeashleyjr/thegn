# THE-55 — resolve main conflicts and re-gate (Lead work order)

files:
  - crates/thegn-core/src/completion/catalog.rs
  - crates/thegn-core/src/lib.rs
  - crates/thegn-core/src/store/mod.rs
  - crates/thegn-host/src/cmd/mod.rs
  - docs/help/daemon-and-sessions.md

## Why this exists

`thegn land` refused this branch with a genuine CONFLICT (not a gate failure):

```
conflicts with main: crates/thegn-core/src/completion/catalog.rs,
crates/thegn-core/src/lib.rs, crates/thegn-core/src/store/mod.rs,
crates/thegn-host/src/cmd/mod.rs, docs/help/daemon-and-sessions.md
```

`tg/the-27-pr-comments-in-diff` and `tg/the-7-theme-builder-popup` landed since
row 319 reviewed this branch. All five files are registry/module-list files
where two branches each append an entry — the classic additive collision.

## Done criteria

- `git merge main`, then resolve every conflict by **keeping BOTH sides**
  wherever the conflict is two independent additions to a registry, module
  list, catalog, `cmd/mod.rs` dispatch arm or help page. Dropping the other
  branch every time is the tempting wrong answer and it silently deletes landed
  work — check each hunk.
- After resolving, confirm nothing from main was lost: `git diff main...HEAD`
  must still contain main tip content for those files plus this branch is
  additions.
- Preserve the row-315/319 fencing work and the security fixes exactly.
- Run `THEGN_ALLOW_HEAVY=1 just test` and report its result. If it dies before
  any test runs (sccache/RUSTC_WRAPPER), retry once with `RUSTC_WRAPPER=` unset
  and report BLOCKED, not FAIL.
- Report PASS only with a green full gate against current main.
