# Design

## Harness abstraction (core)

A small pure registry describes each supported harness:

```
HarnessSpec {
  id: "claude" | "codex" | "gemini" | ...,
  detect: fn(argv) -> bool,               // is this launch this harness?
  hook_target: HookTarget,                // which config file + shape
  resume: fn(session_id, safe_args) -> Vec<String>,  // rebuild argv
}
```

- `resume` for Claude Code → `["claude", "--resume", <id>, ..safe_args]`; Codex →
  `["codex", "resume", <id>, ..safe_args]`; Gemini → its documented resume flag.
- All of this lives in `thegn-core::agent_session` and is **pure + unit-tested**
  (95% core gate): detection from argv, resume-command reconstruction per harness,
  and the sanitizer.

## Secret sanitizer (core, the safety-critical piece)

`sanitize_args(argv) -> Vec<String>` drops secret-bearing flags and their values
(`--api-key`, `--api-key=…`, `--token`, `--auth`, `-p/--prompt` inline text, and
a configurable deny-list) while **preserving** safe operational flags
(`--model`, `--sandbox`, `--cwd`, `--dangerously-bypass-approvals-and-sandbox`,
etc. via an allow-list). Unit tests assert: every deny-listed flag+value is
removed in both `--k v` and `--k=v` forms, allow-listed flags survive, and the
output round-trips through `resume` with no secret substring present.

## Capture path (host)

`thegn agent hooks setup` writes/merges a hook into each installed harness's own
config:

- Claude Code: a `SessionStart`/`SessionEnd` hook in `~/.claude/settings.json`
  that runs `thegn agent record --harness claude --session-id "$SESSION_ID" …`.
- Codex: `~/.codex/hooks.json` entries for `session-start` / `session-end`.
- Gemini CLI: `~/.gemini/settings.json` equivalent.

The installer is **idempotent** (detects an existing thegn hook and no-ops),
merges rather than overwrites, and reports which harnesses were found. `thegn
agent record` resolves the worktree/pane from `$THEGN_WORKTREE`/`$THEGN_PANE`
and upserts a session record.

## Persistence (state-db, `user_version` bump)

New table `agent_sessions`:

```
worktree_path TEXT, pane_id TEXT, harness TEXT, session_id TEXT,
exec TEXT, safe_argv TEXT (json, sanitized), cwd TEXT,
started_at INTEGER, ended_at INTEGER NULL,
PRIMARY KEY (worktree_path, pane_id, harness)
```

Only **sanitized** argv is stored. This requires a `user_version` bump and a
forward migration (git remains source of truth for worktrees; this is cache +
resurrection, consistent with the DB's role).

## Restore path (host)

On session resurrection, for each restored pane that had a live agent record with
no `ended_at` (or per the session-history UX), the host reconstructs the resume
argv via `HarnessSpec::resume(session_id, safe_argv)` and launches it as the
pane's process — following the _existing_ restore consent flow (it does not
resume agents the user opted out of restoring). If the harness/session id is gone
upstream, it falls back to a plain relaunch and surfaces a notice.

## Invariants

- **Event loop**: capture is out-of-band — the hook subprocess calls `thegn
agent record`, which writes over the control path (mpsc + `TerminalWaker`
  pulse). No blocking DB I/O on the loop; the upsert runs on `spawn_blocking`.
- **Render**: none directly; a restored agent pane repaints as any pane does.
- **State**: one `user_version` bump for `agent_sessions`; forward migration
  only.
- **Additivity**: no agent → no records → restore behaves exactly as today. Core
  logic has no tokio/proxy dependency.

## Alternatives considered

- **Screen-scraping the session id from the agent's TUI** — fragile and
  harness-version-dependent; the hook mechanism is the sanctioned, stable source.
- **Storing full transcripts** — unnecessary and a privacy/secret risk; the
  harness already owns its transcript and resumes from its own id.
