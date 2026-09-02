# THE-62 reconcile — merge current main into the lane

## Files to touch (exact paths)

- `crates/thegn-host/src/handlers/merge_queue.rs`
- `crates/thegn-host/src/handlers/pr_queue.rs`
- `crates/thegn-host/src/handlers/provision.rs`
- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/notify.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-svc/src/seam/registry.rs`
- `docs/help/notifications.md`

## State you are landing in

`git merge main` is ALREADY IN PROGRESS in this worktree and left the files
above conflicted. Do not abort or restart the merge. Resolve, then `git commit`.

## The big one: main renamed workspace -> project

THE-10 landed on main and renames the workspace vocabulary to **project**
across config sections, action ids, i18n catalog keys and help text. Most of
these conflicts are that rename meeting your lane's additions.

- Where main renamed something your lane also touched, **keep the rename** and
  apply it to your lane's code. Never revert to the old spelling, and never
  leave the two spellings mixed.
- Where your lane ADDED something new using the old vocabulary, rename it to
  match main.
- An `ActionSpec`'s `message_key` MUST equal `action-<id>`, and every shipped
  locale (`crates/thegn-core/locales/*/main.ftl`) must carry that exact key.
  Rename the spec, both locales and the value together or the i18n surface test
  fails.

## Other named reconcile items

- `config_validate.rs` holds a marked-enum comment ladder and a **pinned
  count**. Main's count has moved. Append your lane's entries at the next free
  numbers and set the count to main's value plus what your lane adds.
- `config.toml.example` must document every config key, including ones main
  added — under main's current section names. `config_example` is the gate.
- `docs/help/*.md` are ratcheted; keep both sides' entries.
- `notify.rs` and the handlers: main added notification producers and the
  autopilot/PR-queue wiring. Keep both sides' notification kinds; the
  `NotificationKind::ALL` count is pinned by a test, so update it to the real
  total rather than guessing.

Default to **keeping both sides** unless one side deliberately DELETED
something; verify with `git log` before dropping any code.

## Verification required before you report

Run the FULL suite, not just scoped checks — this merge crosses a large rename
and scoped suites will not catch it:

    XDG_STATE_HOME=/home/blake/.superzej/pipeline-state RUSTC_WRAPPER= \
      THEGN_ALLOW_HEAVY=1 cargo nextest run --workspace --no-fail-fast

`--no-fail-fast` matters: fail-fast hides later failures and costs a round trip
each. Fix everything it reports, then re-run until green, then `git diff --check`.

The `XDG_STATE_HOME` prefix is REQUIRED on every command that may open thegn's
database. Without it a schema-ahead branch migrates the RUNNING instance's live
database, locking the supervisor out of its roster and emptying the user's
sidebar.
