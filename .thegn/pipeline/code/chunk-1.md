# Chunk 1 — "Clear all" clears exactly what the inbox shows

**Issue:** THE-68 (second half). **Branch:** `tg/the-68-log-noise`.
**Depends on:** nothing. **Land order:** first.
**Overlaps:** `crates/thegn-host/src/handlers/attention.rs` with chunk 3 (chunk 3
rebases onto this one).

Read `.thegn/pipeline/architect/design.md` §2 and §4 first.

---

## The bug

Two functions decide what belongs in the active repo's notification inbox, and
they disagree.

**Display** — `hydrate_feed::populate_notifications`, `crates/thegn-host/src/hydrate_feed.rs:99-107` —
is **fail-open**: a row tagged with a worktree path the `worktrees` registry does
not know (the repo's own **main checkout**, which never gets a `worktrees` row;
an externally-created worktree; a renamed or differently-spelled path) is
**kept**.

**Clear** — `handlers::attention::mark_all_read`, `crates/thegn-host/src/handlers/attention.rs:284`,
via `Db::mark_notifications_read_scoped`, `crates/thegn-core/src/db_notification.rs:96-107` —
is **fail-closed**: it marks `worktree_path=''` plus each path in
`repo_worktree_paths(repo_root)`, and nothing else.

So rows in the fail-open set are displayed and never cleared. Pressing `a`
optimistically greys them (`mark_read_where(|_| true)`, `handlers/attention.rs:307`),
the next hydration re-reads the DB, and they return unread.

## The fix

One pure predicate in `thegn-core`, used by both call sites. The clear becomes a
single statement with the same three arms as the display filter.

---

## Files

### 1. `crates/thegn-core/src/notification_scope.rs` — NEW

A small pure module (no substrate, fully unit-tested — this is the 95%-gated
crate). Header comment should say plainly that it exists because the display
filter and the clear had drifted, and that both must go through it.

```rust
//! The ONE predicate for "does this notification belong to the active repo's
//! inbox?".
//!
//! It is deliberately FAIL-OPEN on the worktree registry: a row tagged with a
//! path the `worktrees` table does not know — the repo's own main checkout
//! (which never gets a row), an externally-created worktree, a path renamed
//! outside thegn — is KEPT rather than hidden. Only a row tagged with a KNOWN
//! path belonging to a DIFFERENT repo is out of scope.
//!
//! It lives here, alone, because the inbox's display filter and its "clear all"
//! used to carry separate copies and drifted: display was fail-open, the clear
//! fail-closed, so exactly the fail-open rows were shown forever and could never
//! be cleared (THE-68). Both call sites project this function; there is no
//! second copy to drift.

use std::collections::HashSet;

pub fn shows_in_repo_inbox(
    worktree_path: &str,
    repo_paths: &HashSet<String>,
    all_known: &HashSet<String>,
) -> bool {
    worktree_path.is_empty()
        || repo_paths.contains(worktree_path)
        || !all_known.contains(worktree_path)
}
```

Register it in `crates/thegn-core/src/lib.rs` beside `pub mod notification_route;`.

**Tests** (in-module `#[cfg(test)]`), one per arm and one per regression:

- host-global (`""`) is in scope regardless of the sets;
- a path in `repo_paths` is in scope;
- a known path of another repo is **out** of scope;
- an unknown path (in neither set) is **in** scope — name this test after the
  main-checkout case, e.g. `the_repo_main_checkout_has_no_registry_row_so_it_shows`;
- a path in `repo_paths` that is _also_ absent from `all_known` still shows
  (the arms are an OR, not a precedence chain).

### 2. `crates/thegn-core/src/store/notification.rs` — CHANGE ONE SIGNATURE

```rust
    /// Mark read exactly what the repo-scoped inbox DISPLAYS — the same three
    /// arms as [`crate::notification_scope::shows_in_repo_inbox`]: untagged
    /// (host-global) rows, rows tagged with one of `repo_paths`, and rows tagged
    /// with a path `all_known` does not contain (fail-open: the main checkout,
    /// an externally-created worktree). Passing `all_known` is what makes the
    /// clear and the display agree; before THE-68 the clear omitted the
    /// fail-open arm, so those rows were shown forever and `a` never cleared
    /// them. The unscoped [`Self::mark_all_notifications_read`] stays for the
    /// all-worktrees (`g`) view.
    fn mark_notifications_read_scoped(
        &self,
        repo_paths: &[String],
        all_known: &[String],
    ) -> Result<()>;
```

### 3. `crates/thegn-core/src/db_notification.rs` — the SQL

Replace the loop-of-UPDATEs at lines 96-107 with one statement carrying all three
arms. Build the two `IN (?, ?, …)` placeholder lists from the slice lengths and
bind with `rusqlite::params_from_iter`; mirror the existing placeholder-building
style in `Db::unread_counts_for_kinds` (`crates/thegn-core/src/db.rs:995`) rather
than inventing a new one.

Shape:

```sql
UPDATE notifications SET read=1
 WHERE worktree_path = ''
    OR worktree_path IN (<repo placeholders>)
    OR worktree_path NOT IN (<known placeholders>)
```

Two edge cases that must be right, and both need a test:

- **`repo_paths` empty** — emit no `IN ()` (SQLite rejects it). Drop that arm.
- **`all_known` empty** — the `NOT IN` arm degenerates to "everything", which is
  the correct fail-open answer (a registry with no rows knows nothing, so
  nothing can be attributed to another repo). Drop the arm and let the
  statement mark all rows read. Say so in a comment; it looks alarming and is
  deliberate.

### 4. `crates/thegn-core/src/db_tests.rs`

Extend the existing scoped-clear coverage (see the tests around lines 1712-1790
and 2188-2222 for the fixture idiom — note the repo convention of passing
`-c commit.gpgsign=false` in git fixtures if you touch one).

- `scoped_clear_marks_untagged_and_repo_rows` — keep whatever exists, updated for
  the new argument.
- `scoped_clear_marks_rows_the_registry_does_not_know` — **the regression.** Two
  rows: one tagged `/repo/main` (absent from both sets, i.e. the main checkout),
  one tagged `/wt/other-repo` (present in `all_known`, absent from `repo_paths`).
  After the clear: the first is read, the second is still unread.
- `scoped_clear_with_empty_registry_marks_everything` — `all_known` empty.
- `scoped_clear_with_no_repo_paths_still_marks_untagged_and_unknown`.

### 5. `crates/thegn-host/src/hydrate_feed.rs`

`populate_notifications` keeps identical behaviour but stops carrying its own
copy of the rule:

```rust
notifications.retain(|n| {
    thegn_core::notification_scope::shows_in_repo_inbox(
        &n.worktree_path,
        &repo_paths,
        &all_known,
    )
});
```

Trim the local comment to a pointer at the module (the "why" now lives there);
keep the "Scope BEFORE capping" comment above — it explains a different fix.

### 6. `crates/thegn-host/src/handlers/attention.rs`

In `mark_all_read`, inside the `(false, Some(wt))` arm (line ~281), build the
second set from the same registry read the display uses and pass it through:

```rust
let paths: Vec<String> = crate::hydrate::repo_worktree_paths(&db, &repo_root)
    .into_iter()
    .collect();
// The clear must cover exactly what the inbox SHOWS, including the fail-open
// arm (rows tagged with a path the registry doesn't know — the main checkout,
// an external worktree). Without `all_known` those rows were displayed and
// never cleared: `a` looked like a no-op on them (THE-68).
let all_known: Vec<String> = db
    .worktrees()
    .map(|wts| wts.into_iter().map(|w| w.worktree).collect())
    .unwrap_or_default();
let _ = db.mark_notifications_read_scoped(&paths, &all_known);
```

`db.worktrees()` comes from `thegn_core::store::WorkspaceStore` — check the
`use` list. Keep the `let _ =` (best-effort cache write) and leave the existing
comment above the `match` intact.

Do **not** change the optimistic model update or the status strings.

---

## Approach notes

- This chunk is pure bug-fix: no config key, no schema change, no new
  notification kind, no UI change. Resist scope creep.
- The one behaviour change users can observe is that `a` now also clears rows
  it always displayed. That is the fix.
- `mark_notifications_read_scoped` has exactly one production caller and one
  trait impl (`Db`) — grep confirms — so the signature change is contained.

## Done criteria

- [ ] `cargo nextest run -p thegn-core notification_scope` passes; every arm and
      both empty-set edges covered.
- [ ] `cargo nextest run -p thegn-core -- db_tests` passes, including the new
      `scoped_clear_marks_rows_the_registry_does_not_know` regression test.
- [ ] `just quick thegn-core && just quick thegn-host` clean.
- [ ] `grep -rn "worktree_path.is_empty() ||" crates/` returns **one** site (the
      new module) — no surviving second copy of the predicate.
- [ ] `test/ignored-result-ratchet.txt` unchanged (the new `let _ =` sits on an
      existing allowlisted line region; if the ratchet complains, the `// best-effort:`
      comment is the fix, not a new ratchet line).
- [ ] Manual: with an inbox row tagged to the repo's main checkout, press `a` in
      System ▸ Notifications; it goes read and **stays** read after a rehydrate.
- [ ] Before push: `THEGN_ALLOW_HEAVY=1 just test`, and `THEGN_ALLOW_HEAVY=1 just coverage`
      (core ≥95% — the new module is fully covered by its own tests).
