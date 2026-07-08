# Add control plane and remote

## Summary

Split compute from UI. Introduce a long-lived **daemon** that owns the
`portable-pty` panes so the terminal UI can attach/detach and **warm-reattach**
to agents that are still running; a **control API** (HTTP/gRPC + SSE/WebSocket
event feed) plus `szhost` verbs that drive a _running_ instance instead of only
resurrecting from SQLite; a `szhost serve` host with thin desktop/web/mobile
clients paired over a URL; a **persistent relay** that keeps remote PTYs alive
across client disconnects via a grace-period lease; and a **mobile companion**
that monitors and lightly controls a paired instance.

This is pure substrate. The daemon, API, relay, and clients carry **no AI
dependency** — AI/agent layers remain strictly additive consumers of these
seams, never a hard requirement of the shell.

## Impact

- **A** — item **769** (headless daemon owning PTYs; realizes the long-unbuilt
  A **7** headless daemon and A **8** supervision).
- **AK** — items **770** (CLI drives the live IDE; extends AK **454** headless
  CLI) and **771** (remote-server thin-client model; builds on AK **445**
  HTTP/gRPC API, **451** SSE/WebSocket event feed, **452** auth scopes/tokens).
- **J** — item **772** (persistent remote-session relay; realizes J **133**).
- **K** — item **773** (mobile companion app; downstream of 769/770 + AK **451**
  and AI **422/423** push notifications).
- Relates: AK **445/451/454**, J **133**, AP **501** (federation alignment for
  the thin-client model), AI **422/423** (push).
- New capability `control-plane`. Touches `superzej-host` (daemon split,
  attach/detach), `superzej-svc` (API/relay transport seams), and the SQLite
  `state-db` (daemon registry + lease records; `user_version` bump).

## Rationale

TODAY superzej is a foreground compositor that **resurrects** session state from
SQLite (`$XDG_STATE_HOME/superzej/superzej.db`), not a live PTY-owning daemon.
When the UI exits, its panes die with it; a running agent cannot be left and
rejoined. A 769 daemon that owns the PTYs is the enabler for everything else:
the UI becomes one attachable client, the CLI (770) and thin remote/mobile
clients (771/773) become peers over the AK API, and a relay (772) can hold the
remote PTY open while no client is connected. git stays the source of truth for
worktrees; the DB remains a cache + resurrection layer, now also recording which
daemon owns which session.

## Non-goals

- Building the AI/agent layer or the LLM proxy — this change only exposes the
  substrate they will later consume.
- A polished web/desktop GUI. 771/773 specify the pairing + protocol contract
  and a minimal companion surface, not a full client implementation.
- Multi-tenant cloud hosting or org federation beyond aligning the pairing model
  with AP 501.
- Replacing the foreground compositor. The in-process UI remains a first-class
  (and default local) client of the daemon.
