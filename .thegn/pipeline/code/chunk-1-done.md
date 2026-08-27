# Chunk 1 — done

**Commit:** `e6f9a07c` — _fix(notify): "clear all" clears exactly what the inbox
shows (THE-68)_. Branch `tg/the-68-log-noise`, landed on top of chunk 2's
`67aaf335` (chunk 2 committed mid-work; my staging was scoped to my own files
either way).

## What shipped

Implemented exactly as specced. 7 files, +260/−25.

| File                                   | Change                                                                                                                                                                                |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `thegn-core/src/notification_scope.rs` | **NEW.** `shows_in_repo_inbox(worktree_path, repo_paths, all_known)` + the module header explaining the fail-open intent and the drift that motivated it. 5 unit tests.               |
| `thegn-core/src/lib.rs`                | `pub mod notification_scope;` beside `notification_route`.                                                                                                                            |
| `thegn-core/src/store/notification.rs` | `mark_notifications_read_scoped(&self, repo_paths, all_known)` — new signature + the specced doc comment.                                                                             |
| `thegn-core/src/db_notification.rs`    | Loop-of-UPDATEs → one statement, three arms, `params_from_iter`, placeholders built in the `unread_counts_for_kinds` style.                                                           |
| `thegn-core/src/db_tests.rs`           | 4 new tests + a shared fixture, appended at the tail (after chunk 2's block).                                                                                                         |
| `thegn-host/src/hydrate_feed.rs`       | `retain` now projects the core predicate; local comment trimmed to a pointer. "Scope BEFORE capping" comment left intact.                                                             |
| `thegn-host/src/handlers/attention.rs` | `mark_all_read` builds `all_known` from `db.worktrees()` and passes it; `use thegn_core::store::{NotificationStore, WorkspaceStore}`. Optimistic update and status strings untouched. |

The `all_known.is_empty()` edge is an early return to an unconditional
`UPDATE notifications SET read=1`, with the comment the spec asked for — the
`NOT IN ()` arm degenerating to "everything" is the correct fail-open answer, and
it looks alarming without one.

## Verification

- `cargo nextest run -p thegn-core notification_scope` — **5/5 pass** (host-global,
  repo path, other repo's known path, `the_repo_main_checkout_has_no_registry_row_so_it_shows`,
  `the_arms_are_an_or_not_a_precedence_chain`).
- `cargo nextest run -p thegn-core scoped_clear` — **4/4 pass**, including the
  regression `scoped_clear_marks_rows_the_registry_does_not_know` (`/repo/main`
  goes read, `/wt/other-repo` stays unread) and both empty-slice edges.
- `cargo check -p thegn-host` — clean.
- `treefmt` — applied (the committed files are formatted; pre-commit passed).
- `test/ignored-result-ratchet.txt` — **unchanged**. It is file-level and
  `handlers/attention.rs` is already listed, so the new `let _ =` causes no churn.

### The predicate-copy grep

`grep -rn "worktree_path.is_empty() ||" crates/` returns **three** hits, not the
one the chunk predicted — but **no surviving copy of the inbox rule**. All three
are different predicates that happen to share the leading term:

- `detail.rs:2262` — Alerts dedup against the _needs-you_ worktree set.
- `handlers/attention.rs:167` — chip counting, dedup against the _covered_ set.
- `hydrate.rs:3798` — Work-tab repo filter with **inverted polarity** (it
  _drops_ untagged rows; the inbox keeps them).

The hydrate_feed copy — the one that had drifted — is gone. (The new module's own
line doesn't match the pattern: it has no `n.` receiver prefix.)

### Not run

`THEGN_ALLOW_HEAVY=1 just test` / `just coverage` — per the dev-loop policy these
are the pre-push gate, and chunk 3 still has to land in this worktree. Whoever
pushes should run them once at the end. The new module is fully covered by its
own tests, so the core 95% gate should not move against us.

Manual check (`a` on a main-checkout-tagged row, verify it stays read across a
rehydrate) is deferred to the same pre-push pass — it needs a live host with
`just start name=the68`.

## Flag for the branch owner (NOT mine to fix)

`just quick thegn-core` and `just quick thegn-host` both **fail**, on a lint that
predates this chunk and this branch:

```
error: manual implementation of `ok`
  --> crates/thegn-core/src/sandbox_cpucap.rs:297:16   [clippy::manual_ok_err]
```

That file is untouched on `tg/the-68-log-noise`; the lint arrives with `d4f3aeb9`,
which is **on `main`**. It is a one-line fix (`v.parse().ok()`), but it is
outside this chunk and editing a fourth file mid-flight risks a needless conflict
with the other coders in this worktree. **It will block the pre-push gate for the
whole branch**, so it needs fixing before push — just not here. I verified my own
edits separately with `cargo check -p thegn-host` (clean) and by reading clippy's
output: `thegn-core` reported that one error and nothing else, so nothing in this
chunk is lint-dirty.

## Notes for chunk 3

- `mark_all_read` in `handlers/attention.rs` is the shared anchor. The
  `(false, Some(wt))` arm now reads:

  ```rust
  let paths: Vec<String> = crate::hydrate::repo_worktree_paths(&db, &repo_root)…;
  // comment about the fail-open arm (THE-68)
  let all_known: Vec<String> = db.worktrees()…;
  let _ = db.mark_notifications_read_scoped(&paths, &all_known);
  ```

  Chunk 3 adds its `clear_session_attention_for_worktree` calls **beside** this,
  so rebasing should be clean.

- `WorkspaceStore` is now in that file's `use` list — chunk 3 needs it too and
  should not re-add it.
