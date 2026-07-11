# Add agent session capture + resume-on-restore

## Summary

When thegn restores a session after detach or reboot, it restores the pane
layout and cwd — but a coding agent that was running in a pane comes back as a
_fresh_ agent with no memory. This change captures each agent's **native session
id** and relaunch parameters when it starts, persists them alongside the session
layout, and on restore **rebuilds a resume command** (`claude --resume <id>`,
`codex resume <id>`, `gemini … <id>`) so the agent picks up its own conversation
where it left off. A `thegn agent hooks setup` installer wires the capture into
each supported harness's own hook mechanism, and a **secret sanitizer** ensures
no credentials are ever persisted or replayed.

## Impact

- **I 117** — restore agent state where possible: this is the concrete
  mechanism, coupling the restored **layout ↔ agent session** bidirectionally.
- **Q 658** — agent session history + hibernation: the captured session records
  are the substrate for listing/resuming past sessions per worktree and for
  hibernate/rehydrate.
- **R 236–239** — native adapters (Claude Code / Codex / OpenCode / aider): the
  `hooks setup` installer + per-harness capture/resume mapping is a first slice
  of native-adapter support (session lifecycle only).

Extends the `agent` capability and adds a session-record table to `state-db`
(**one `user_version` bump**).

## Rationale

cmux and limux both solve this by installing hooks into the agent CLIs'
config files (`~/.claude/settings.json`, `~/.codex/hooks.json`,
`~/.gemini/settings.json`) that write the native session id to a sidecar file on
session-start; on restore they reconstruct the resume invocation. limux's
`agent_hooks.rs` additionally **strips secrets** (`--api-key`, `--token`, inline
prompts) while preserving safe flags (`--model`, `--sandbox`,
`--dangerously-bypass-approvals-and-sandbox`) — a clean reference. thegn
already has session detach/attach and reboot resurrection (I 111–113) and
per-pane cwd restore (v14); this closes the "half a restore" gap where the
terminal comes back but the agent's context does not. It is harness-agnostic and
strictly additive — worktrees with no agent are unaffected.

## Non-goals

- **Full ACP session lifecycle (R 230)** — for ACP-native agents, resume is a
  protocol concern (`session/resume`); this change targets the _CLI-hook_ harnesses
  (Claude Code, Codex, Gemini) that are not driven over ACP.
- **Storing conversation transcripts** — only the native session id + safe
  relaunch parameters are persisted; the transcript stays owned by the harness.
- **Auto-resuming without consent** — restore reconstructs and offers/launches
  the resume command per the existing session-restore behavior; it does not
  silently resume agents the user chose not to restore.
- **Persisting any secret** — API keys, tokens, and inline prompts are stripped
  before storage; a captured record with a secret in it is a bug the sanitizer
  tests guard against.
