# Multiplexer parity — pane geometry ops, spatial drag, session recording

Linear: THE-53

## Why

THE-53 asked for a deep audit of thegn's terminal/multiplexing functionality
against tmux, zellij, asciinema and the drag-and-drop pane managers (tuios,
Claude Code Desktop, cmux, vibe-kanban, …14 tools). The audit's finding: thegn
already **is** a strong multiplexer where it matters most, and the honest gaps
are narrow and specific.

**Already at or beyond tmux/zellij parity (no work needed):**

- Detach/attach + persistence: daemon-owned PTYs, warm-reattach with snapshot
  - deltas, never-reap leases by default, scrollback snapshots across reboot
    (`control-plane`, `make-daemon-default`).
- Layout persistence: per-tab `CenterTree` JSON in `group_tabs.tab_layout`,
  named switchable layouts (`SaveLayout`/`ApplyLayout` + `layouts` table),
  JSON export/import (`ExportLayout`/`ImportLayout`), and read-only import of
  tmuxinator/sesh/zellij layouts (`thegn_core::layout_import`). The remaining
  layout-ownership work (daemon-owned session model, multi-client attach) is
  scoped by **add-runtime-session-split** — not re-scoped here.
- Splits/stacks/zoom/broadcast: `SplitDown`/`SplitRight`/`NewPane` (smart
  split), `CenterTree::Stack`, `ToggleZoom`, `ToggleSyncPanes`, spatial
  `Focus*` moves, floating via pins/corner overlay.
- Time travel: per-pane bounded recording + `Alt+r` scrub/search
  (`time-travel`) — beyond anything tmux ships.

**The gaps this change closes:**

1. **No pane resize at all.** `CenterTree::Branch` carries per-child weights,
   but nothing mutates them: no resize action exists in the keymap
   (roadmap G-91 claims resize; the action inventory says otherwise), and no
   mouse border drag. tmux (`C-b C-arrow`), zellij (resize mode) and every
   drag tool have this.
2. **No swap/move pane** (roadmap G-98, open). A pane cannot be rearranged
   after creation except by closing and re-splitting.
3. **Mouse stops at click.** The sidebar has press-drag-release reorder
   (add-sidebar-actions-and-mouse); the center has nothing spatial — no
   border drag-resize, no drag-a-pane-onto-another rearrange. This is the
   headline THE-53 feature ("seen in 14" tools).
4. **Session recording is a stub** (roadmap AN-483, open). `recorder.rs`
   writes a whole-UI asciicast v2 (`Ctrl+Alt+r`), but it is client-side only:
   nothing records while detached (exactly when an unattended agent session is
   most worth recording), there is no per-session recording, no CLI/control
   verb, and the per-pane time-travel ring cannot be exported as a `.cast`.

## What Changes

- **Pane geometry ops (keyboard-first):** new actions `resize-left` /
  `resize-right` / `resize-up` / `resize-down` (adjust the focused leaf's
  split weights by a step) and `swap-pane-left/right/up/down` (exchange the
  focused leaf with its spatial neighbor). Pure `CenterTree` mutations,
  persisted through the existing tab-layout persist, routed through the
  session owner (today the in-loop `Session`; via `SessionHandle` once
  add-runtime-session-split lands, so they become `apply_layout` mutations on
  a remote session with no re-design).
- **Mouse: border drag-resize.** Pane frame borders (chrome-owned cells — pane
  content mouse forwarding to PTY apps is untouched) hit-test as drag handles;
  dragging adjusts the adjacent branches' weights live.
- **Mouse: drag-and-drop rearrange.** Press-drag on a pane's frame/title
  lifts the pane; hovering another pane shows a drop-target highlight
  (center = swap, edge zones = re-anchor as a new split on that side);
  release commits, Esc cancels. Same damage discipline as the sidebar drag
  (drag feedback is chrome damage → `Full`), same keyboard-parity rule
  (every mouse op has a keyboard equivalent).
- **Daemon-side session recording:** a `sessions.record` capability
  (start/stop/status) — the daemon tees a session's output events into an
  asciicast v2 file under the profile state dir, recording continues while
  detached, zero cost when off. CLI: `thegn session record <id> [--stop]`.
  The existing whole-UI `Ctrl+Alt+r` recorder is unchanged.
- **Per-pane cast export from the replay ring:** the `Alt+r` replay overlay
  and a palette action export the retained recording as a `.cast` file
  (bounded by the existing replay budget).
- New capability spec **`panes`** (pane-tree manipulation had no behavioural
  spec; the center tree is currently spec'd nowhere), plus deltas to
  `time-travel` and `control-plane`.

## Impact

- **Roadmap:** completes G-91 (resize — honestly, this time), G-98 (swap pane
  positions), AN-483 (session recording); advances the THE-53 audit to
  closed. tasks.md wiring happens in the audit phase.
- **Specs:** new `panes`; ADDED requirements in `time-travel` (cast export)
  and `control-plane` (`sessions.record`).
- **Capability catalog:** one new row — `sessions.record`
  (`Verb::RecordSession`, scope via `required_scope`, surfaces
  Http/Grpc/Cli; deliberately not Mcp/Plugin in v1). Control wire schema
  snapshot (`docs/api/control-v1.json`) regenerates.
- **In-flight changes reconciled:** **add-runtime-session-split** (resize/swap
  are structural mutations — they extend the `SessionHandle` mutation set;
  this change does not depend on it landing, but is designed to route through
  it), **make-daemon-default** (recording targets daemon sessions, which are
  now the default), **add-sidebar-actions-and-mouse** /
  **fix-sidebar-drop-position-semantics** (drag/drop interaction conventions
  — insertion visualization, Esc-cancel — are kept consistent),
  **add-osc-attention-signaling** (none; recording tees raw output before any
  interpretation).
- **Help ratchet:** new action ids must be claimed with real prose in
  `docs/help/terminal-and-panes.md` (geometry ops, drag) and
  `docs/help/daemon-and-sessions.md` (recording); config keys documented in
  `config/config.toml.example`.

## Non-goals

- **Daemon-owned layout / multi-client attach** — owned by
  add-runtime-session-split.
- **Recording the composed UI in the daemon.** The daemon has no chrome; it
  records per-session PTY output (what asciinema records for a program). The
  whole-UI recorder remains the client-side `Ctrl+Alt+r`.
- **New layout algorithms** (BSP/spiral/master-stack á la tuios). The
  weighted split tree + stacks stay the model.
- **tmux command-language compatibility** (send-keys scripting, status-line
  formats, hooks). thegn's control API + `thegn session type` already cover
  the automation use cases natively.
- **asciinema upload/streaming.** Files are local; sharing is the user's
  business.
