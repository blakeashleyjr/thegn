# Tasks — cross-profile session move

## 1. Catalog + core (thegn-core)

- [x] 1.1 `Verb::MigrateSession` + catalog row `sessions.migrate` (Admin
      scope via `required_scope`, surface Cli only); catalog coverage tests
      (Admin-never-on-MCP/plugin test picks it up automatically).
- [x] 1.2 Pure profile-root resolution for a _non-active_ profile (target
      store path from name, no reroot) beside `profile.rs`; unit tests
      including the existing socket-length edge cases.
- [x] 1.3 Pure move-plan model: group rows in → plan out (transfer set,
      live-session blockers, collision verdict, boundary-crossing notice);
      exhaustive unit tests (95% core gate).

## 2. Store transfer (thegn-core)

- [x] 2.1 Store methods: read a group's `tab_groups`/`group_tabs` (+worktree
      registration), insert-with-cleared-`pane_sessions` transactionally,
      delete-source transactionally, detect already-committed target
      (idempotent resume).
- [x] 2.2 Tests: collision abort, crash-window duplication + resume,
      no-env-shaped-columns pin, worktree registration created only when
      absent.

## 3. CLI (thegn-host)

- [x] 3.1 `thegn session move <worktree...> --to-profile <name> [--kill]
[--dry-run] [--json]`: preflight (source daemon session lookup via the
      existing discovery), refusal listing live ids, `--kill` through
      `sessions.kill`, transfer, report.
- [x] 3.2 Live-source-compositor guard: detect the source profile's flock and
      refuse moving a group open in a running UI (message: close it or kill
      its panes).
- [x] 3.3 Best-effort target-daemon notification via its `notify.push` door;
      an unreachable target is reported as a warning after confirmed success.
- [x] 3.4 Smoke coverage with two isolated `XDG_STATE_HOME` profile roots
      (cold move, --kill move, collision, dry-run).

## 4. Docs + gate

- [x] 4.1 Document the verb in `docs/help/cli.md` and the semantics (cold
      move, what crosses, what never does) in
      `docs/help/daemon-and-sessions.md` (help + prose ratchets).
- [ ] 4.2 Run `just ci` once (includes openspec-validate).
