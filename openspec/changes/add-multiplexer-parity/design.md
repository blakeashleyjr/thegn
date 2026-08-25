# Design — multiplexer parity

## 1. Pane geometry ops are pure tree mutations

All four resize actions and all four swaps are functions on `CenterTree`
(`crates/thegn-host/src/center.rs`), beside the existing `split`/`remove`
mutators:

- `resize(tree, pane, dir, step)` — walk to the leaf, find the nearest
  ancestor `Split` whose axis matches `dir`, and shift weight between the
  child containing `pane` and its neighbor on the `dir` side. Weights are
  clamped so no child drops below a minimum share (a pane can never be
  resized to zero). If no ancestor matches the axis, the action is a no-op
  (statusbar hint, not an error).
- `swap(tree, pane, dir)` — resolve the spatial neighbor with the existing
  focus-neighbor resolution (the same geometry walk `FocusLeft/...` uses so
  swap and focus always agree on "the pane to the left"), then exchange the
  two `Leaf` nodes. Weights stay with the tree position, not the pane —
  swapping a small pane into a big slot makes it big (tmux `swap-pane`
  semantics).

Both are deterministic and unit-tested at the tree level (they live in
thegn-host, outside the core coverage gate, but get exhaustive unit tests
like the rest of `center.rs`). The run-loop handlers are thin arms in a
sibling handler module (`handlers/pane_geometry.rs`), not run.rs growth.

**Interplay with add-runtime-session-split:** resize/swap are exactly the
"structural mutations" that change routes through `SessionHandle`. Until it
lands they mutate the in-loop `Session` directly and persist via the existing
debounced tab-layout persist; when it lands they join the handle's mutation
set and become `apply_layout` operations on a remote session. The spec is
written at behavior level so neither ordering blocks the other.

## 2. Mouse: chrome-owned drag, pane-owned content

The mouse rule that keeps PTY apps working: cells **inside** a pane's content
rect forward to the application per `mousefilter.rs`; cells **on the pane
frame** (borders, title row) are chrome and belong to the compositor. All new
mouse behavior binds only to frame cells, so a full-screen app that wants
mouse events never fights the compositor.

- **Border drag-resize:** press on a shared border segment → grab the two
  adjacent branches; motion converts the pointer delta along the split axis
  into a weight shift (same clamps as keyboard resize); release commits (one
  persist). During the drag each motion event is chrome damage → `Full`
  frame; motion events are already batched by the 8 ms input-batching window,
  so no new throttling mechanism is needed.
- **Drag-rearrange:** press-and-hold on a pane's frame/title lifts it
  (`FrameModel.pane_drag`, mirroring `sidebar_drag`); hover feedback renders
  a highlight on the prospective target — center region = swap with that
  pane, an edge band (top/bottom/left/right ~quarter) = re-anchor: remove the
  dragged leaf and re-split the target on that side (weights renormalized).
  Release commits through the same tree mutations as the keyboard; Esc or
  release over a non-target cancels. Drop-target resolution is a pure
  function of (pointer cell, pane rects) — unit-tested like
  `dragdrop::drop_payload`.
- **Render discipline:** drag state lives on the frame model; every drag
  visual is chrome damage (`Full`), never a pane recompose triggered by pane
  output. The render-plan invariant tests stay the gate; an idle drag (no
  motion) adds no wakes.

Degradation: drop highlights and drag affordances use theme roles + the
active glyph set (no literals at draw sites); on `ascii` glyphs the highlight
falls back to reverse-video frame cells.

## 3. Daemon-side recording

The `SessionActor` already owns the ordered output stream (`on_output`).
Recording is a tee at that point:

- `SessionMsg::Record { spec, reply }` / `RecordStop` / status folded into
  the existing `Probe`/listing path. When on, the actor holds an
  `asciicast::Writer` (header once: width/height from `LiveMeta`, v2 events
  `[t, "o", data]`; resize events emitted as `"r"` rows×cols on `on_resize`).
- Writes are buffered and flushed on a small byte/time threshold; file I/O
  happens on the actor's blocking-friendly context (`spawn_blocking` for
  open/rotate), never on the UI loop — the UI is not even involved.
- **Free when off:** one `Option` check in `on_output`, no allocation —
  the same discipline as the replay ring's `feed`.
- Files: `$XDG_STATE_HOME/thegn/recordings/<session-id>_<ts>.cast` (the
  profile reroot makes this per-profile automatically), directory 0700,
  files 0600. A `[recording] max_bytes` cap stops the writer (with a final
  marker event and a status flag) rather than filling the disk.
- Lifecycle: recording stops (file finalized) on session exit; the tombstone
  carries the recording path so `session list --json` can report it briefly
  after death. Recording state survives client detach by construction (it
  lives in the actor).

The whole-UI `Ctrl+Alt+r` recorder is untouched — it records the composed
frame stream (what the _user_ saw); `sessions.record` records one session's
PTY output (what the _program_ wrote). Both are asciicast v2.

### Catalog / surfaces

One new row: `sessions.record` → `Verb::RecordSession`, summary "Start/stop
an asciicast recording of a session", surfaces `Http | Grpc | Cli`. Scope
comes from `required_scope(verb)` (write-level: it mutates daemon state and
the filesystem); the catalog never restates policy. Not exposed on
Mcp/Plugin in v1 — recording other sessions is a surveillance-adjacent power;
widening is a deliberate later decision (documented, not a `SURFACE_GAPS`
entry, since unlisted surfaces need none). Wire types (`RecordSpec`,
`RecordStatus`, the `SessionInfo.recording` flag) regenerate
`docs/api/control-v1.json`.

## 4. Replay ring → `.cast` export

The time-travel ring already stores timestamped byte events. Export replays
the retained slice through an asciicast v2 writer: header geometry = the
pane's rows/cols at the earliest retained event, then events with times
rebased to zero. Bounded by the existing `[replay]` budget — export is
honest about being a tail, and the overlay says how far back it reaches.
Triggered from the replay overlay (`e`) and a palette action
(`export-cast`); writes to the same recordings dir.

## Alternatives considered

- **Streaming the composed UI from the daemon** — rejected: the daemon has no
  chrome and never composes frames (add-runtime-session-split deliberately
  keeps composition client-side); per-session PTY recording is also the more
  useful artifact (replayable at any geometry, pipeable to `asciinema play`).
- **A zellij-style "resize mode"** — rejected for v1: chords-per-direction
  match the existing keymap grammar (`Focus*`), and a modal layer can be
  added later purely in keymap config (mode presets exist).
- **Drop-zones as a full layout re-tile (BSP re-insert)** — rejected: swap +
  edge re-anchor covers the 14-tool feature as users experience it, without
  inventing a second layout algorithm.
- **Recording via an external `asciinema rec` wrapper pane** — rejected: it
  cannot attach to an already-running daemon session, which is the actual
  gap.

## Security

- **Recordings are terminal output — they contain whatever secrets were
  printed** (tokens echoed by tools, `env` output). Mitigations: files are
  0600 under the per-profile state dir (inside the profile firewall), never
  exported over the control API (the verb returns status + path, not
  content), never auto-uploaded anywhere, and `thegn doctor` lists active
  recordings so a user can audit them. Starting a recording requires a
  write-scoped token; a read/observer client cannot record.
- **The recording verb is not on MCP/plugin surfaces** in v1 (see catalog
  note) — an agent must not silently bug another session.
- **Statusbar indicator:** a recording session shows a recording chip in the
  attached UI (same honesty rule as the existing `Ctrl+Alt+r` toast), so
  recording is never invisible to the person at the keyboard.
- **Drag/resize:** no new privilege or write surface — mutations are the same
  layout ops the keyboard already performs; observer clients still cannot
  mutate layout (add-runtime-session-split rule, unchanged).
- **No credentials in any new config key**; `[recording]` holds paths/limits
  only.

## Open questions

- Should border drag-resize require no modifier (frame cells are unambiguous)
  or mirror the sidebar's press-drag threshold to avoid accidental grabs on
  sloppy clicks? (Lean: a 1-cell motion threshold, no modifier.)
- Swap across a `Stack`: swapping a leaf with a stack swaps the whole stack
  node (lean), or only the active member? Lean whole-node — stacks are a
  positional unit.
- Should `sessions.record` optionally capture input (`"i"` events) for full
  asciicast fidelity? Lean no for v1 — input is the more secret-dense stream
  (passwords typed at prompts).
