# THE-19 — resolve main conflicts (round 2) (Lead work order)

files:

- crates/thegn-host/src/platform/mod.rs
- crates/thegn-host/src/platform/unix.rs
- crates/thegn-host/src/platform/windows.rs
- docs/help/cli.md

## Why this exists

`thegn land` refused with a CONFLICT (not a gate failure):

```
conflicts with main: crates/thegn-host/src/platform/mod.rs,
crates/thegn-host/src/platform/unix.rs,
crates/thegn-host/src/platform/windows.rs, docs/help/cli.md
```

`tg/the-7-theme-builder-popup` landed since, and it made the SAME kind of change
this branch did: it moved a `#[cfg]` behind the platform seam (commit
`cbea76dc`, `theme_store.rs`). Both branches therefore added new functions to
`platform/mod.rs` and its unix/windows arms. This is an additive collision, not
a disagreement.

## Done criteria

- `git merge main`, then resolve by **keeping BOTH sides**. Both branches added
  distinct platform helpers; the merged file must contain THE-7's
  (`theme_store`-related) helpers AND this branch's (`hook_run`-related) ones.
  Deleting either is a silent regression of landed work — check every hunk.
- `docs/help/cli.md` is a help page where both branches appended entries; keep
  both entries and keep them in the page's existing ordering convention.
- After resolving, confirm nothing landed was lost: the merged
  `platform/mod.rs` must still define everything main's tip defines.
- The help ratchet must stay green — every claimed action id must still be
  mentioned by its page.
- Then run `RUSTC_WRAPPER= THEGN_ALLOW_HEAVY=1 just test` and report the result.
  Row 330's review already passed at 7146; this round must not regress it.
