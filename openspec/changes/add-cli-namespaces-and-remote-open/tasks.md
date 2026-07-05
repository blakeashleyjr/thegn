# Tasks

## 1. Foundations (superzej-host/cmd)

- [x] 1.1 Exit-code constants (`EXIT_OK/ERROR/RETRYABLE/NOT_FOUND`) + doc
      contract in `cmd/mod.rs`; refactor `host.rs` retryable `exit(2)` onto them.
- [x] 1.2 `emit_json<T: Serialize>` helper (single compact JSON doc, no ANSI) +
      typed `NotFound` error downcast to exit 3 in `main()`.

## 2. Namespaces (superzej-host)

- [x] 2.1 `DiffArgs`/`DiskArgs`/`CleanArgs` flattened arg structs shared by
      legacy and namespaced variants.
- [x] 2.2 New `cmd/wt.rs` `Action { List, New, Rm, Diff, Disk, Clean }`;
      List/Diff/Disk/Clean delegate 1:1 to existing fns.
- [x] 2.3 `cmd/repos.rs` `Action { List, Recent }`; `main.rs` `Wt`/`Repo`
      variants; legacy `List/Diff/Disk/Clean/Repos/Recent` hidden
      (`hide = true`) but byte-identical in behavior — **smoke**: legacy and
      namespaced outputs match.

## 3. Core additions (superzej-core — coverage-gated)

- [x] 3.1 db v34: `intents` table (additive `IF NOT EXISTS`);
      `store/intent.rs` `IntentStore { put_intent, take_intents }`
      (claim-and-delete txn, FIFO) + shared `FocusIntent` model — **unit
      tests**: round-trip, FIFO order, take empties, kind isolation.
- [x] 3.2 `WorkspaceStore::delete_tab_groups_for_worktree(session, worktree)` —
      **unit test** alongside existing tab_groups tests.
- [x] 3.3 `profile.rs`: default profile takes the singleton flock (contention
      still `Acquired`, silent) + `instance_running()` probe — **unit test**
      via `try_lock_nb` on a scratch file.

## 4. Headless lifecycle (superzej-host)

- [x] 4.1 `wt new [name] [--repo] [--base] [--env] [--json]` — wizard pipeline
      minus UI/sandbox; prints the created path; rollback on failure —
      **smoke**: path exists, `git worktree list` registers, `--json` shape.
- [x] 4.2 `wt rm <path|branch> [--delete-branch] [--force]` — resolve via DB,
      refuse main worktree, confirm unless `--force`, synchronous teardown, git
      remove, DB cleanup incl. tab-group rows — **smoke**: checkout gone,
      branch kept by default / dropped with `--delete-branch`, tab_groups rows
      cleaned, unknown target exits 3.

## 5. Machine output (superzej-host)

- [x] 5.1 `--json` on `list`/`wt list`, `env list`, `host list`, `ci runs`,
      `share list`, `forward list`, `disk` (derived `Serialize` structs via
      `emit_json`) — **smoke**: JSON parses on list/env/host/disk.
- [ ] 5.2 ~~(stretch, cuttable) `diff --stat --json` via `--numstat`~~ — CUT
      (deferred; the other nine surfaces cover the scripting need).

## 6. Help + completions (superzej-host)

- [x] 6.1 `cli_help.rs`: `GROUPS` table + `attach()` top-level help template
      rendering grouped commands from the live clap tree — **unit test**:
      drift guard (every non-hidden command in exactly one group) + headings
      render; **smoke**: `--help` shows `Workspace`/`Forge`.
- [x] 6.2 `completions <shell>` via `clap_complete` (workspace dep), binary
      name from argv[0] — **smoke**: `completions bash` emits a script.

## 7. Remote open (superzej-host)

- [x] 7.1 `cmd/open.rs`: resolve repo (path or basename/slug; miss → exit 3
      with candidates); running instance → `put_intent`; else
      `set_active_workspace` + launch (`--no-launch` = DB writes only).
- [x] 7.2 Consume path: `build_model` `take_intents` (tolerates missing
      table), run-loop drain applies last intent via `switch_workspace` —
      **smoke**: `open --no-launch` sets the active-workspace pointer,
      basename resolution works, unknown repo exits 3.

## 8. Docs + validate

- [x] 8.1 CLI contract doc (exit codes, JSON conventions, namespace map) +
      `tasks.md` roadmap annotations (A 6, AK 454).
- [x] 8.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
