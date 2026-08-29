# THE-89 revision chunk 3: bootstrap, lifetime, and wake correctness

This is a revision chunk raised by the architect review of chunks 1–2. The
classifier and daemon wiring are useful, but the host-side cache is not yet a
complete cross-process projection of daemon state.

## Required corrections

### 1. Bootstrap the cache from authoritative daemon state

`start_error_state_bridge` subscribes to `/v1/events`, but the event endpoint
sends only `Hello` and future broadcast frames. It does not send an initial
activity snapshot. Therefore a compositor that reconnects while a detached
session already has `error_active = true` never learns that state until the
session changes state again.

Add an explicit snapshot path, either as a subscription-initialization frame or
as a typed session-list field, and seed the cache before consuming deltas. The
snapshot must include at least session id, owning worktree, and
`error_active`. Keep the existing delta ordering: subscribe/snapshot first,
then apply future `Activity` and `SessionExit` frames.

### 2. Bound cache entries to daemon lifetime

If the daemon is killed or the WebSocket drops before it can emit
`SessionExit`, the bridge task exits but leaves active entries in the
process-global cache. A later hydration pass can therefore show a permanent
false-positive glyph. Track the bridge/daemon identity with entries and clear
that identity's entries when the stream ends or is replaced; do not clear
entries belonging to a newer daemon connection accidentally.

Add an integration test for an active entry followed by bridge disconnect and
another for daemon restart/reconnect with a fresh snapshot.

### 3. Wake the compositor when the cache changes

The bridge currently mutates `agent_error_cache` without sending a refresh
event or pulsing the existing `TerminalWaker`. This violates the repository's
off-loop producer contract and makes the glyph depend on an unrelated
hydration tick. Route a cache transition through the existing model/sidebar
refresh channel and waker, coalescing unchanged values. Verify that a raised
and cleared bit each schedules the expected model refresh without introducing
a timer or an idle poll.

### 4. Preserve the issue's noise boundary

The shipped defaults include the unconstrained substring `permission denied`.
That can classify a normal tool-call result such as `Error: permission denied`
as a harness failure, which is precisely the false positive THE-89 is meant to
remove. Narrow or context-gate authentication signatures (and any similarly
broad defaults), then add regression tests for tool-call permission/auth
failures alongside the existing `Error: Command failed` and `Fetch` cases.

## Done criteria

- Reconnecting while a banner is active immediately projects the glyph.
- Daemon loss cannot leave a stale active entry in the compositor.
- Raise/clear transitions wake and rehydrate the sidebar promptly.
- Tool-call noise, including permission/auth failures, remains below
  `AttentionTier::Failure` unless a deliberate configured harness signature
  matches.
- Add focused unit/integration tests and rerun the lead-mandated core/host
  nextest filters with `XDG_STATE_HOME` set to a temporary directory.
