# THE-440 plan — command-scoped agent permissions, no repository mutation

Stacked on THE-418 (`blake/the-418-…`, tip c3465c53).

## Investigation

- `crates/thegn-host/src/agent_permissions.rs::seed` read/modify/writes
  `<worktree>/.claude/settings.local.json` (follows symlinks, non-atomic,
  replaces `permissions.allow`), called from `agent::launch_spec_full` before
  sandbox resolution; failure is a warning and the launch proceeds. Remote
  launches are silently skipped.
- Harness behaviour, attested from the installed CLI (`claude --help`,
  Claude Code 2.1.274) and the official settings docs
  (code.claude.com/docs/en/settings):
  - `--settings <file-or-json>` — "Path to a settings JSON file or a JSON
    string to load additional settings from". Docs: applied above user,
    project and local files, below managed; "`--settings` lasts one session
    and doesn't write to any file"; "Lists merge instead of overriding …
    `permissions.allow` … combines the lists". Deny rules in any scope keep
    applying (managed/`allowManagedPermissionRulesOnly` still win).
  - `--allowedTools <tools...>` is variadic and would swallow a following
    positional prompt, so it is NOT used.
- codex / pi / aider / antigravity: no command-scoped allow-list mechanism
  thegn can attest (and the patterns are Claude-vocabulary anyway).

## Design

1. Harness seam (`thegn_core::harness`): new optional op
   `session_permission_args(&self, allow) -> Option<Vec<String>>` with cap bit
   `PERMISSIONS`; only `Claude` implements it, returning the argv pair
   `["--settings", "{\"permissions\":{\"allow\":[…]}}"]` (serde_json built,
   never string-concatenated).
2. `thegn_core::agent_task`: `EffectiveAgent::permission_args()` renders the
   argv shell-quoted token by token (`util::sh_quote`); an empty list is a
   no-op; a harness without the op is an `Err` naming the harness and the fix
   (fail closed — never dropped, never written to disk, never replaced by
   skip-permissions). `interactive_command()` / `headless_template()` append
   it after the model flag (the template form brace-escapes it so the JSON
   survives `{prompt}` substitution). `validate_agent_models` reports it.
3. Host: delete `agent_permissions.rs` and its call in `launch_spec_full` —
   the launch no longer touches the worktree. The resume / continue / fork
   daemon forms append the same args, so every daemon launch shape carries
   the policy; remote/provider launches carry it in the same argv.
4. Admission: `session open --stage` (fresh dispatch and `--resume-work`)
   resolves the effective agent and its permission args BEFORE claiming a
   roster row — an unattestable grant is refused with an actionable
   "permission policy hold" error: no row, no process, no task failure.
5. Docs: `docs/help/configuration.md`, `daemon-and-sessions.md`,
   `config/config.toml.example`, `config.rs` doc comments,
   `openspec/specs/agent/spec.md` requirement rewritten (command-scoped,
   merge semantics, fail closed, per-harness support).

## Tests

- core: claude args JSON shape; hostile patterns (quotes, `$()`, spaces,
  braces) round-trip; headless template substitution keeps the JSON intact;
  non-claude harness with permissions is an error; caps⇔ops.
- host: real launch seam (`command_for` → `launch_spec_full`, host backend,
  real git worktree) with a final `settings.local.json` symlink to a missing
  outside target, a symlinked `.claude` directory, and a FIFO leaf: outside
  target never created, `git status --porcelain=v1 -z` byte-identical, FIFO
  never opened (a read would block), argv carries the policy and a shell
  re-parse yields exactly the JSON; concurrent launches with different stage
  policies each carry only their own grant; resume/continue carry it;
  non-claude refused.
- session: stage admission refuses before any row is claimed.

## Acceptance mapping (expected)

- Symlink/FIFO/hard-link/non-owner/component-swap: no filesystem access at
  all ⇒ nothing to redirect (tested for symlink/dir-symlink/missing target/
  FIFO; the others are vacuous by construction).
- git status unchanged: tested.
- Concurrent launches: per-process argv, tested.
- Fail closed before spawn with a policy hold: tested (pre-claim refusal).
- Crash/kill/restart: nothing persisted ⇒ no residual widening (by construction).
- Attestation: the exact policy is in the process argv (daemon session argv);
  a roster-recorded digest is NOT added here (needs THE-505/THE-268 admission
  token) — left open.
- User settings untouched: never read or written.
- Docs/schemas: updated.
