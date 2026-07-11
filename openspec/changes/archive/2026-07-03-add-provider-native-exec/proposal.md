# Add provider native exec

## Summary

Give managed-sandbox **providers** a generic, provider-agnostic **native exec**
capability: an interactive pane attaches to a sandbox over the provider's own API
(a PTY-over-WebSocket session) instead of shelling out to the vendor CLI. Sprites
is the first implementation (its `WSS /v1/sprites/{id}/exec` API); the seam
(`Provider::open_exec`/`attach_exec` + an `ExecSession` channel handle + a
`Stream`-backed pane) is generic so other providers implement it as needed. A new
per-env `exec` mode (`auto`/`api`/`cli`, default `auto`) selects it, and the live
session id is persisted so a restart **reattaches** the remote session and replays
its scrollback.

## Impact

- **Provider/placement layer** — `thegn-svc::provider` gains the `exec_api`
  capability + `open_exec`/`attach_exec`; the host pane gains a stream transport.
- Closes the long-noted follow-up in `config/config.toml.example` ("an API-exec
  (WSS) bridge that removes that CLI dependency is a follow-up").
- Relates to the sandbox/env groups (named execution environments, sandbox
  placement) — additive, AI-free.

## Rationale

Sprites (and similar microVM providers) expose **no SSH**; the only documented
interactive door besides their REST/WS API is the vendor CLI. Routing every pane
through `sprite exec` makes the CLI a hard dependency of dogfooding thegn on a
remote backend. The provider already does its control plane (create/checkpoint/fs)
natively over the API; native exec extends that to the interactive shell, so a
remote-backed worktree needs only an API token — no vendor binary — and gains
free cross-restart session resume.

## Non-goals

- SSH/mosh/iroh transports to sprites (the platform exposes no SSH; a tunnel would
  still ride the CLI).
- Native exec for non-shell agent panes (the shell path is wired first; the seam
  is general).
- Daytona native exec (no native exec API wired yet; it stays CLI).
