# Coordination brief — THE-696

## How this lane is run

You are one lane of a maintenance batch. A primary agent (the Lead) reviews
every plan and every diff before anything merges. Your job is this issue and
nothing else.

### Cargo — you MAY run these, and should

The stage prompt forbids Cargo. The primary relaxes that for exactly two
commands, because a report of `implementation-ready` for code that does not
compile is worthless:

    nix develop --command cargo check -p <crate> --all-targets
    nix develop --command cargo test  -p <crate> --lib <narrow filter>

Run them as often as you need. Everything else — `build`, unfiltered `test`,
`nextest --workspace`, clippy, any `just` recipe — stays the primary's.

**Attempt it; if `nix develop` fails inside the sandbox, report that verbatim
and stop.** The pipeline sandbox read-only-binds `/nix/store`, so `nix develop`
sometimes cannot create its temporary store paths. That is an environment
limitation, not your failure, and "checks not run, nix develop failed with
<error>" is a perfectly good report. What is NOT acceptable is claiming the
code compiles without having checked.

### Database safety

**Never run a thegn command that opens or migrates the real database.**
`$XDG_STATE_HOME` defaults to the user's live state and a migration from your
shell mutates the running instance's DB. Any test needing a DB must build its
own fixture in a temp dir.

### `.thegn/maintenance/` hygiene

This directory carries previous batches' records. You may add files under
`.thegn/maintenance/<ISSUE>/`. **Do not delete anything** — a deletion on this
branch deletes it from main when the lane merges.

### Ratchets that a change like this trips

`just lint` / `just test` enforce shrink-only allowlists. Expect to need an
update in the same change for: a new `section.key` in config (three ratchets —
`config.toml.example` key coverage, the env-overlay list, and the completion
slot), a new capability row (surface-gaps), a new action id (help page
`actions:` frontmatter _and_ prose), a control-API schema snapshot
(`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`),
platform `#[cfg]` outside `platform/`, colour/glyph literals outside the caps
chokepoints, and ignored `Result`s. Never add a ratchet entry without a written
reason.

### Report vocabulary

Exactly one of: `plan-ready`, `implementation-ready`, `source-review-clear`,
`revisions-needed`. Never PASS/APPROVED.

## Scope for THE-696

This is the largest of the three lanes and the one most likely to sprawl. Read
this scope fence carefully; the primary will reject work outside it.

**In scope, in priority order:**

1. `thegn wt folder <worktree> <name>` and a clear/unfile form, on the existing
   `wt` subcommand tree in `crates/thegn-host/src/cmd/wt.rs`.
2. `--folder <name>` on `thegn wt new`.
3. A capability-catalog row so the operation exists on the control API, MCP and
   plugins — matching how every other worktree mutation is exposed.
4. A config default: `[[worktree_templates]].folder`, so headless creation files
   itself.

**Do item 1 and 2 first and make them complete and tested.** If you run out of
room, a lane that ships a correct CLI verb is worth more than four half-done
surfaces. Say in your report exactly where you stopped.

**Constraints the primary is setting:**

1. **Use the existing primitives.** `ensure_folder`, `set_worktree_folder` and
   `set_worktree_folder_if_identity` already exist at
   `db_workspace.rs:741/752/760` and already have find-or-create semantics.
   Write no new SQL for the assignment path.
2. **Identity-checked writes only.** Use `set_worktree_folder_if_identity`; a
   bare `UPDATE` against a row that does not exist yet is a silent no-op, which
   is the exact trap `set_worktree_env` documents at `db_workspace.rs:779`. A
   freshly created worktree may not have its row yet — the upsert case is in the
   acceptance criteria and must be tested.
3. **Reject the branch-name-prefix approach.** The issue explains why; do not
   propose it.
4. **No schema change.** `folders` and `worktrees.folder_id` already exist. If
   you believe you need one, stop and report that instead.
5. A new config key trips **three** ratchets (`config.toml.example` key
   coverage, the env overlay, the completion slot) and a new capability row
   trips the surface-gaps ratchet. Budget for all four.
6. Folder names: find-or-create must match **case-insensitively and trimmed**,
   so running the verb twice yields one folder. Decide and test what a
   whitespace-only or empty name does.
7. Deleting a folder that still holds worktrees needs **defined, tested**
   behaviour. Pick unfile-or-refuse, state which and why in your plan, and be
   consistent with what the TUI already does — read `sidebar_order.rs` first
   rather than inventing a second policy.

**Out of scope:** any TUI/sidebar rendering change, nested folders, folder
ordering via CLI, and auto-filing pipeline lanes (the last acceptance-criteria
line) — the config default makes it possible, and wiring the pipeline itself is
a follow-up the primary will schedule.
