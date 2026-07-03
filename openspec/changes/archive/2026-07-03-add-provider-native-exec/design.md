# Design: provider native exec

## Why a channel handle, not a `dyn` trait

Async-fn-in-trait is not object-safe, so there is no `dyn ExecStream`. Instead a
provider's `open_exec` returns a concrete `ExecSession` built from channels:
`frames: Receiver<ExecFrame>` (server→client), `control: Sender<ExecControl>`
(client→server), and `session_id: watch::Receiver<Option<String>>`. The provider
spawns a driver task that bridges its socket to these channels. This is also the
exact shape the host already uses for off-loop producers, so it drops into the
pane event model unchanged.

## Transport inside the pane, not a parallel pane type

A pane stays one type (`PtyPane`) with an internal `PaneIo { Pty | Stream }`.
`Stream` holds the control sender + provider/sandbox ids; a relay task on the host
runtime pumps `ExecSession.frames` into the shared `PaneEvent` channel (pulsing the
waker) exactly like the PTY reader thread, and forwards stdin/resize back. The
emulator, render-plan damage tracking, and the event loop are transport-blind — so
the enforced render invariants (pane output ⇒ `Panes`, never a chrome recompose)
hold with no change.

## Non-blocking open

Pane spawn is synchronous on the loop thread; a WSS connect is not. So
`spawn_native` returns immediately with the control channel and a relay task that
does the `open_exec`/`attach_exec` itself; the pane shows nothing until connected.
The host's `block_on_provider` (a throwaway per-call runtime) is deliberately NOT
used — it would drop the long-lived driver task. The relay runs on the main 8-worker
runtime via a stored `Handle`.

## Decision point

`agent::native_shell_exec(cfg, worktree)` re-resolves the env exactly as
`launch_spec` (DB repo-root + effective env) so the two never disagree, and returns
`Some` only when the env is a `Provider` placement whose provider has `exec_api`,
whose `exec != cli`, and whose API token is present. Otherwise the CLI/PTY path is
used. The off-thread `launch_spec` still runs (it provisions the sandbox); its CLI
argv is simply unused when the pane goes native.

## Reattach

`materialize_with_specs` (the restore/focus spawn) checks `tab.pane_sessions[old]`:
present ⇒ `attach_exec(session)` (replays scrollback); absent ⇒ a fresh
`open_exec`. The session id is persisted in a new `group_tabs.pane_sessions` JSON
column (DB v22), captured at persist time from each `Stream` pane's
`provider_session()`, and pruned/remapped alongside `pane_cwds`/`pane_cmds`.

## Risks

- New dep `tokio-tungstenite` (rustls + webpki-roots) — shares reqwest's ring
  crypto provider, so one rustls in tree.
- DB v22 is additive; reconcile the `user_version` with sibling branches on merge.
