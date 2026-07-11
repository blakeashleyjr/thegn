# Tasks

## 1. Core harness registry + sanitizer (thegn-core)

- [ ] 1.1 `agent_session.rs`: `HarnessSpec` registry (claude/codex/gemini) with
      argv `detect` and `resume(session_id, safe_argv)` reconstruction —
      **unit tests**: detect each harness from argv; resume argv shape per harness.
- [ ] 1.2 `sanitize_args(argv)` deny-list (`--api-key`, `--token`, `--auth`,
      `-p/--prompt`, …) + allow-list (`--model`, `--sandbox`, `--cwd`,
      `--dangerously-bypass-approvals-and-sandbox`) — **unit tests**: deny value
      removed in `--k v` and `--k=v` forms, allow-listed survive, no secret
      substring in the round-tripped resume command.

## 2. Persistence (state-db, thegn-core)

- [ ] 2.1 `agent_sessions` table + `user_version` bump + forward migration; upsert
      / query-by-worktree helpers — **unit tests** on an isolated `XDG_STATE_HOME`:
      upsert, end-session, query latest per worktree, only sanitized argv stored.

## 3. Capture + installer (thegn-host)

- [ ] 3.1 `thegn agent hooks setup`: idempotent merge of a thegn hook into
      `~/.claude/settings.json`, `~/.codex/hooks.json`, `~/.gemini/settings.json`;
      report detected harnesses; re-run is a no-op.
- [ ] 3.2 `thegn agent record --harness … --session-id … [--worktree|--pane]`:
      resolve target from flags or `$THEGN_WORKTREE`/`$THEGN_PANE`, sanitize
      argv, upsert the record over the control path (`spawn_blocking` write).

## 4. Restore (thegn-host)

- [ ] 4.1 On resurrection, reconstruct the resume argv for restored agent panes
      via `HarnessSpec::resume` and launch through the existing restore consent
      flow; fall back to a plain relaunch + notice when the upstream session is
      gone. Respect panes the user opted out of restoring.

## 5. Docs + validate

- [ ] 5.1 Document `thegn agent hooks setup` / `agent record` and the resume
      behavior in the CLI/agent doc section + `config/config.toml.example` deny/allow
      overrides.
- [ ] 5.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
