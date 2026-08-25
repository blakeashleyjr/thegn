# Tasks — session fork

## 1. Catalog + wire (thegn-core / thegn-svc)

- [ ] 1.1 `Verb::ForkSession` + catalog row `sessions.fork` (non-streaming
      surfaces, scope = `sessions.open`'s); catalog coverage tests.
- [ ] 1.2 Wire types: `ForkSpec { session, cwd, worktree, scrollback, tab }`,
      `SessionInfo.forked_from: Option<String>`; regenerate
      `docs/api/control-v1.json` snapshot.
- [ ] 1.3 `ControlApi::fork` with a default-unimplemented impl (no adapter
      churn beyond the daemon); HTTP/gRPC routes from the `ROUTES` table.

## 2. Daemon (thegn-host)

- [ ] 2.1 Retain the resolved `OpenSpec` (argv/env/cwd/worktree/agent marker)
      on `SessionEntry`, memory-only; assert no recipe reaches the DB,
      tombstones, or any API response (test).
- [ ] 2.2 `fork()`: derive `spec'` (env replay for raw argv, fresh `agent:`
      re-resolution, `already_capped = false`, geometry from source), spawn
      via the existing open path, set `THEGN_FORKED_FROM`.
- [ ] 2.3 Scrollback hand-off: render the retained tail to a 0600 file under
      `forks/`, set `THEGN_FORK_SCROLLBACK`, best-effort delete at fork exit
      (tombstone burial).
- [ ] 2.4 Dead-session error naming `sessions.open`; unit tests: fork
      liveness, new-pid honesty, env identity overwrite, recipe-never-leaks.

## 3. CLI + UI

- [ ] 3.1 `thegn session fork <id> [--scrollback] [--fork-worktree] [--tab]
[--cwd]` (+ `--json`); no-daemon degradation; smoke coverage.
- [ ] 3.2 `fork-session` action (palette + pane context menu) on the focused
      pane's daemon session; adopt-intent placement (split beside source /
      `--tab`).
- [ ] 3.3 Worktree-fork composition: existing worktree-creation path first,
      cwd remap second; failure-domain behavior per spec (no implicit
      worktree deletion).
- [ ] 3.4 Show `forked_from` in `session list` and the session picker.

## 4. Docs + gate

- [ ] 4.1 Claim `fork-session` with real prose in
      `docs/help/daemon-and-sessions.md` (help + prose ratchets).
- [ ] 4.2 Run `just ci` once (includes openspec-validate and the wire-schema
      snapshot test).
