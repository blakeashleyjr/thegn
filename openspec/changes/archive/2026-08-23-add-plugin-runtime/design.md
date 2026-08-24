## Context

The v0.2 wire contract is complete and snapshot-pinned; the in-process model (`PluginRuntime`: negotiation, surface cache, state, audit) lives in core; `spawn_ndjson` is the one-shot process primitive. What's missing is process lifecycle, the event-loop seam, and the render path.

## Goals / Non-Goals

- Goals: run config- and directory-declared plugins in both modes; keep the 0%-idle and render-decision invariants; make `host.call` real (scope-checked, dispatched) for the verbs the control client exposes; a copy-paste example a shell script author can run.
- Non-Goals: PaletteAction/SidebarTab surfaces (C2), provider-as-plugin extension points (C4), a generic call-any-catalog-verb dispatcher and CLI restart (client-API phase, C3), sandboxing plugin processes.

## Decisions

- **Processes stay in svc, policy in host, model in core.** `loader`/`session` know nothing about the event loop; `thegn-host/src/plugins.rs` owns threads and channels; verbs mutate the core `PluginRuntime` so the audit/state/negotiation semantics stay covered by core tests.
- **One channel into the loop.** Every producer (resident reader threads, the one-shot scheduler) sends `PluginEventMsg` on a single mpsc and pulses the `TerminalWaker` — the handler drains in the existing `handlers/` idiom; statusbar dirtiness rides the normal chrome path (a changed view ⇒ `Full` plan, never a new render mode).
- **`host.call` never blocks the loop.** Requests queue to one dispatcher thread owning a current-thread tokio runtime + control client; replies go straight to the session's stdin writer (a shared handle), bypassing the loop entirely. Scope check happens before queuing, host-side, using the same `required_scope` lattice as control tokens.
- **Auto-restart with capped exponential backoff** (3 attempts, then disabled-until-reload) instead of a restart verb: the failure mode that matters (a plugin crashing on an event) heals itself; deliberate restarts ride config hot-reload, which already rebuilds the runtime.
- **`[[plugins]]` stays out of `config.toml.example`'s required keys** (developer surface, documented in help + `docs/extending/plugin.md`) — the example-file gate's allowlist entry gets the pointer.

## Risks / Trade-offs

- A hostile plugin can still spin CPU or write junk; `MAX_LINES`/line caps bound memory, timeouts bound one-shot runs, and the audit log records verbs — full isolation is future sandbox work, stated in the help page.
- The dispatcher thread serializes host.calls across plugins; acceptable at v0.2 scale (a slow daemon reply delays other plugins' calls, never the UI).
