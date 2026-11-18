# Design

## Identity

The thegn sandbox id is the Vercel sandbox name. The session id changes on every
resume, so it is resolved per call by name and cached in-process only.

## Exec

`exec_api()` is false for Vercel on purpose: it also switches on the resident
bridge, reverse tunnels and the native pane path, none of which exist here.
`control_command_template()` returns `[self_exe, "vercel-exec", "{id}", "--"]`.

- Capture mode (non-tty stdin): `POST cmd wait:true`, relays NDJSON stdout, exits
  75 with "vercel sandbox stopped" if the sandbox is stopped. Never resumes.
- TTY mode: resumes if needed, opens the `/interactive` WebSocket PTY, and falls
  back to stream mode (detached command, log follow, stdin over `POST …/stdin`).

## Keep-alive

Created with a 30 minute timeout. While a bridge is live, every 5 minutes, if
under 10 minutes remain, extend by 15 minutes, capped by the thegn lifetime
ceiling. A 4xx on extend means the plan ceiling: stop extending, warn once.

## Credential-free transfer

`EnvProviderKind::credential_free()` is true for Vercel. Provisioning forces
`bundle_seed`, forces `git_token = None`, strips the `AgentConfigs`, `Dotfiles`,
`AtuinSync` and `HomeClosurePush` steps, and the applier refuses them as defence
in depth. The create body never carries `source.password`, credentials or env
secrets. The token is resolved at call time through the audited secret
chokepoint and lives only in a redacted client field.

## Lifetime

`max_lifetime_secs = 0` means 24 h for vercel. At the ceiling the reaper stops
the session and marks the ledger record `expired`; it deletes at ceiling plus
`VERCEL_DELETE_GRACE_SECS` (7 days), or at once when the worktree is gone.

## Out of scope

Checkpoints, snapshot storage cost, the Hosts panel, and a model credential for
headless agents (owner decision queued). No model credential is sent, so headless model agents cannot log in.

## Unverified

Not yet checked on a live account: the interactive PTY WebSocket, the sandbox-name and tag charsets, upload and output size limits, 429 behaviour, whether deleting a sandbox removes its snapshots, whether Nix works in the image, and the token scopes required.
