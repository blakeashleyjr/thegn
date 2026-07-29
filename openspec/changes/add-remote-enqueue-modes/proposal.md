# Remote-sprite merge enqueue modes (route-to-host / push)

## Summary

The cross-host merge queue (`add-cross-host-merge-queue`) made the queue
host-aware: rows carry a `location`, membership is DB-resolved, and a drain run
**on the target host** bundle-fetches off-host branch tips before folding. Its
explicit non-goal was getting a branch **from** a remote host **into** that
queue without a human running the drain there — the enqueue-side dispatch.

This change closes that gap for **remote sprites** (a worktree provisioned on
another machine by a host thegn) with a config-selected `[merge_queue]
remote_mode`:

1. **`route_to_host` (default).** `merge add` on a sprite whose target repo lives
   on another host sends the enqueue to that **host's daemon** over the existing
   serve-mode control plane (`POST /v1/merge/add`, bearer-token, `MergeAdd`
   scope), so the **host's DB owns the row**. The host's next drain bundle-fetches
   the sprite's tip (already built) and lands it. One authoritative queue, one
   gate, one ordering.
2. **`push`.** The sprite treats its own clone as authoritative: it drains
   locally (fold + gate + advance its local target), then `git push`es the
   advanced target to `origin`. No host reach, no token; each sprite lands
   independently and `origin` is the convergence point.

A prerequisite bug is fixed separately (already landed): `resolve_worktree`
trusted a stale `$THEGN_WORKTREE` host path that doesn't exist inside a sprite,
so every worktree-scoped command failed there.

## Impact

- Roadmap: tasks.md **J (Remote access)** — the enqueue-side counterpart to
  `add-cross-host-merge-queue`, unblocking J128/J129's "work from a sprite".
- Spec: `merge-queue` — ADDED `remote_mode` behavior (route-to-host enqueue,
  push land+push); MODIFIED the enqueue requirement to route by mode.
- Code:
  - `thegn-core::config` — `RemoteMode` enum + `merge_queue.remote_mode` (done).
  - **route_to_host**: inject `THEGN_CONTROL_URL` + a `MergeAdd`-scoped
    `THEGN_CONTROL_TOKEN` into the sprite at provision (mirrors the proxy/iroh
    env injection in the Fly/OCI providers); a `ControlClient::merge_add` helper
    building `ControlAddr::Tcp{addr,token}` from those vars; `merge_ops`/`cmd`
    enqueue routing that picks the control-plane path when the target is off-host
    and the mode is route-to-host; the host `/v1/merge/add` records the caller's
    `location` on the row so the drain fetches the sprite tip.
  - **push**: after a local advance, `git push origin <target>` when the mode is
    push; the `remote_target_guard` is bypassed in push mode (the sprite lands
    its own clone rather than being redirected to the target host).
- Security: a narrow, `MergeAdd`-only bearer token now lives in a sprite's env.
  Scoped to a single verb; revocable via the pairing store.

## Rationale

The host side of route-to-host is already complete (`POST /v1/merge/add` →
`enqueue_worktree` into the host DB; `ControlClient` speaks `Tcp{addr,token}`;
rows are host-aware; the drain bundle-fetches off-host tips). The only missing
pieces are (a) giving the sprite coordinates to call home and (b) wiring the
client call — the smallest addition that reuses the built machinery. `push` is
offered for setups without a reachable host daemon (or that prefer `origin` as
the single source of truth), where a self-contained land+push is simpler than
standing up the control plane.

## Non-goals

- Auto-dispatching the **drain** to a remote host (still J128/J129); route-to-host
  moves only the enqueue. The drain still runs on the target host.
- Changing the fold/gate/CAS engine, conflict/agent handoff, or read panels.
- A third "ssh straight into the host DB" mode — the control plane is the one
  sanctioned cross-host write path.
