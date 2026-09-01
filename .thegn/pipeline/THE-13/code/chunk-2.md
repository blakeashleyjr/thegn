# Chunk 2 — event-driven target lifecycle, status projection, and drawer context

Commit subject (exact): `feat(the-13): integrate event-driven preview targets`

## Files touched

- `crates/thegn-host/src/preview.rs` — new host supervisor for live target
  candidates, bounded pane diagnostics, watcher ownership, and channel-facing
  events.
- `crates/thegn-host/src/preview_watch.rs` — new off-loop watcher adapters for
  PTY/child/provider event sources; no interval timer or idle poll.
- `crates/thegn-host/src/main.rs` — register the new host modules only.
- `crates/thegn-host/src/pty_drain.rs` — feed bounded pane output/exit facts to
  the preview supervisor after normal emulator handling.
- `crates/thegn-host/src/run.rs` — install/drain preview channels, trigger
  one-shot worktree/config scans off-loop, consume existing forward events,
  update damage/model, and keep edits as thin wiring, not new policy or a
  god-file subsystem.
- `crates/thegn-host/src/chrome.rs` — add the renderer-neutral `PreviewView`
  projection to `FrameModel` and its hydration/equality plumbing only.
- `crates/thegn-host/src/panel/sections/misc.rs` — render preview source/status
  beside existing Forward rows, reusing the existing hit/URL affordances.
- `crates/thegn-host/src/sidebar_view.rs` — render the compact active-worktree
  preview token through existing theme/capability segment helpers.
- `crates/thegn-host/src/drawer_state.rs` — THE-11 integration seam only:
  register/resolve the `preview` runtime occupant context; do not duplicate
  registry, pooling, persistence, or file-manager logic.
- `crates/thegn-host/src/panel/sections/mod.rs` — update the system-panel model
  fixture to provide the preview projection used by the render tests.
- `docs/help/share-and-forward.md` — document pane/config/package discovery,
  `up/down/unknown`, external open, and THE-11 preview drawer setup.
- `test/help-ratchet.txt` — regenerate/verify; no new action id is planned.
- `test/help-prose-ratchet.txt` — regenerate/verify the existing Forward page
  prose claim remains complete.
- `test/help-panel-prose-ratchet.txt` — regenerate/verify if the Forward panel
  gains a new named section; no new panel context is planned.
- `test/muse/snapshots/panel_system__system/xterm__100x30__linux.txt` — update
  only if the existing system-panel fixture visibly changes.

## Approach

Use the existing `PaneEvent::Output`/`Exit` channel and waker path. The host
passes output chunks to the pure core parser, associates hints with pane and
worktree metadata, and stores only a bounded diagnostic tail. On active
worktree/config change, an off-loop worker reads `package.json` and the explicit
config hint once; it sends results through mpsc and pulses `TerminalWaker`.

For sandboxed hints, consume the existing `ForwardEvent` provider seam and
create/reuse the loopback proxy. Keep OCI/vendor command names in the service
implementation. Do not create a second detector or read `[forward].poll_secs`
for preview. Watch PTY EOF/exit, provider events, proxy teardown, and worktree
switches. If a runtime cannot provide a watch source, expose `unknown` and
leave explicit open/fetch usable; never add a periodic reconnect probe.

The state model is live and memory-only. DB forward rows remain the existing
cache and are not a new source of truth. Status/token changes set chrome or
sidebar damage; pane output sets pane damage only. The renderer uses the
existing `Seg`, theme, and `active_glyphs()` chokepoints.

For THE-11, consume the registry's public context seam. A `preview` occupant
must be an ordinary contained PTY with URL argv/env context, owned/persisted by
THE-11. If THE-11 has not landed, compile the integration behind its agreed
adapter or leave the drawer registration unavailable; do not invent a browser
pane enum or generic command table.

## Overlap/dependency

This chunk is file-disjoint from chunk 1. Chunk 3 has an intentional one-line
overlap in `crates/thegn-host/src/main.rs` for registering its fetch module, so
the chunks must run serially. This chunk depends on chunk 1's
core types and must run serially after it. It also depends on
`tg/the-11-drawer-tools` landing its registry/context seam; the only intentional
overlap is the narrow `drawer_state.rs` integration point, so this chunk must
run serially with THE-11. Chunk 3 consumes the `PreviewView`/diagnostic state
and therefore follows this chunk for the live-target response path.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host preview`
- `cargo nextest run -p thegn-host forward`
- `cargo nextest run -p thegn-host panel_system`
- `cargo nextest run -p thegn-host drawer`
- `cargo nextest run -p thegn-host help`

Use a temporary `XDG_STATE_HOME` for all host tests or invocations. Do not run
the built binary against the live state DB, and do not run E2E; snapshot edits
are reviewed as artifacts and exercised by the later scoped UI gate.

## Done criteria

- Pane output, one-shot config/package scans, and provider events discover and
  retire targets without an idle timer; every off-loop result uses channel plus
  waker.
- The Forward/system and sidebar/drawer projections show honest `up`, `down`,
  or `unknown` plus port/source; no unrelated PTY output becomes full chrome
  damage.
- External `o` behavior is preserved; THE-11 owns the optional terminal-browser
  drawer occupant and no browser pane kind/engine is added.
- Help/config prose and any affected panel snapshots/ratchets are updated in
  the same commit.
- The coder commits exactly with subject:
  `feat(the-13): integrate event-driven preview targets`
