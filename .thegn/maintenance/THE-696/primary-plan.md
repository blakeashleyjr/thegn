# THE-696 — primary decisions, unblocking row 693

The block was correct to raise and is now answered. Implement **items 1 and 2
only**. Items 3 and 4 are deferred to their own issues, for reasons below.
Where this document and the investigation disagree, this document wins.

## I verified both claims the block rests on

- **`set_worktree_folder_if_identity` is a plain `UPDATE`**, not an upsert
  (`db_workspace.rs:767-775` — `UPDATE worktrees SET folder_id … WHERE worktree
= ?2 AND repo_path = ?3 AND EXISTS (…)`). So it returns `false` for an absent
  row and cannot by itself satisfy the issue's "upsert" criterion. Correct.
- **`WorktreeTemplate` is invisible to the headless path.** It is read only by
  `config*.rs`, `layout_import.rs`, `layout_spec.rs`, `keymap.rs`, `wizard.rs`
  and `run.rs`. There is no reference anywhere under
  `crates/thegn-host/src/cmd/`. So adding `.folder` to it would be dead config
  for `wt new`. Correct, and this is the real design gap in the issue.

## Decision 1 — the config default is DEFERRED, not redesigned here

The issue asks for `[[worktree_templates]].folder`. That is the wrong shape and
the block is what surfaced it: `wt new` has no template selection at all, so the
field would be read by nothing. Inventing a selection rule ("first template
wins") to make it work would be a new feature smuggled into a bug lane, and I
said in the scope fence not to do that.

**And it is not needed for the motivating problem.** THE-696 exists because
pipeline batches left 13 lanes unfiled. Once `wt new --folder` exists, the
dispatcher passes `--folder` explicitly. No config key is required to fix the
thing that raised the issue.

So: **do not touch `config.rs`, `config.toml.example`, or any config ratchet.**
I am filing the config-default design as a separate issue.

## Decision 2 — your "upsert" reading is confirmed

Discover the git identity → register with the existing `put_worktree` → assign
with `set_worktree_folder_if_identity`. That is exactly the "no new SQL"
constraint, honoured. Proceed with it.

Keep the two failure modes distinct and both surfaced: a path that is **not a
git worktree** is a refusal, and an identity mismatch (folder deleted, renamed,
or belonging to another repo) is an error — never a silent success. A `false`
from the identity helper must never be reported as "filed".

## Decision 3 — the capability row is DEFERRED, and here is why

You are right that a catalog row entails the routed request/handler, the wire
schema snapshot and surface coverage. That makes it **all-or-nothing**:
`test/surface-gaps-ratchet.txt` is shrink-only, so a capability row landed
without every declared surface makes the ratchet grow and is simply refused. A
half-done capability cannot land at all.

Combining that with the CLI work in one lane means either a very large diff or a
lane that cannot land. So the capability projection gets its own issue, where
the schema snapshot and surface coverage are the whole job and can be reviewed
as such.

**Do not add a `folders.*` row to `capability.rs` in this lane.**

## Approved scope — this is the entire lane

1. **`thegn wt folder <worktree> <name>`** plus a clear/unfile form, as a new
   `cmd::wt::Action` variant. `main.rs` dispatches the nested action already, so
   no top-level command is needed.
2. **`--folder <name>` on `thegn wt new`**, applied after registration in
   `create_and_register`, so both the single-repo and `--program` paths get it.

With these behaviours, which your failure-case list already has right:

- Trim the name and **reject an empty/whitespace-only result before calling
  `ensure_folder`** — it would otherwise create a nameless folder
  (`db_workspace.rs:741-748`).
- Find-or-create matches trimmed and case-insensitively, so running it twice
  yields one folder. Because the identity helper compares the name **exactly**,
  read back the persisted `FolderRow.name` and pass that — `handlers/
sidebar_folder.rs:121-171` already does this; follow it.
- Folders are repo-scoped: the same name in another repo is a different folder
  and must not cross-assign.
- Clear unfiles the worktree and **leaves the folder row intact**
  (`set_worktree_folder(.., None)`).
- Folder deletion keeps the existing **unfile-on-delete** contract
  (`db_workspace.rs:719-734`). Do not change it, and do not invent a second
  policy — you correctly found the TUI's and it is authoritative.
- `wt new --folder` with an invalid folder must not report the creation as
  filed.

## Out of scope — explicit

- `config.rs`, `config/config.toml.example`, `WorktreeTemplate`, and all three
  config-key ratchets (deferred — separate issue).
- `capability.rs`, control-API routing, `docs/api/control-v1.json`, MCP,
  plugins, `surface-gaps-ratchet.txt` (deferred — separate issue).
- Any TUI/sidebar rendering change, nested folders, folder ordering via CLI.
- Pipeline auto-filing (it becomes a one-flag change once `--folder` exists).
- No schema change. If you think you need one, stop and tell me.

## Verification you owe me

- `cargo check -p thegn-host --all-targets` and `-p thegn-core --all-targets`
  clean. Attempt it; if `nix develop` fails inside the sandbox, report that
  verbatim and stop — that is an environment limit, not your failure.
- Tests for: repeat-invocation idempotence across case/whitespace, the
  absent-row register-then-file path, cross-repo isolation, clear-preserves-
  folder, and the empty-name refusal.
- `git diff main...HEAD --diff-filter=D --name-only` prints nothing.
- The diff touches `cmd/wt.rs` and tests. If it touches `config.rs` or
  `capability.rs`, you have left the plan.
