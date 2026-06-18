# Superzej Readiness Audit - TUI & Runtime Deep Dive

2026-06-17_222200

> **For Hermes:** Use `subagent-driven-development` and `systematic-debugging` to execute this plan task-by-task.

**Goal:** Execute a deep-dive readiness audit of the native `szhost` terminal compositor focusing on runtime behavior, edge-case TUI bugs, and performance optimization as a follow-up to the initial release gate audit.

**Scope:** Event loop invariants, PTY backpressure, visual drift, modal state, drawer lifecycle, terminal resize, text input overlays, tests.

**Architecture Context:**

- Monolithic event loop blocks on `termwiz::terminal::Terminal::poll_input`.
- Off-thread async/blocking work sends on `tokio::sync::mpsc::unbounded_channel` or bounded channels and pulses `termwiz::terminal::TerminalWaker`.
- `PaneEmulator` and `Surface` diff-flush handles rendering.

---

## 1. Event Loop Invariants & Performance Fast-Paths

**Objective:** Ensure the compositor loop never blocks, handles PTY floods safely, and avoids rendering blank rows on startup.

**Evidence Collection / Validation:**

- **[ ] PTY Channel Bounds:** Check `Panes::new` and `spawn` in `panes.rs` to ensure PTY reader channels are bounded. Unbounded PTY channels can OOM the compositor during floods.
  - _Verify:_ `grep -A 2 -n 'tokio_mpsc::channel' crates/superzej-host/src/panes.rs`
  - _Expected:_ Should use `tokio::sync::mpsc::channel(256)` and `blocking_send`, NOT `unbounded_channel`.
- **[ ] `compose_pane` Row-Text Fast-Path:** Check if the optimization from `superzej-performance-optimization` (Pattern 2) is implemented in `compositor.rs` / `emulator.rs`.
  - _Verify:_ `grep -A 5 -n 'fn compose_pane' crates/superzej-host/src/compositor.rs`
  - _Expected:_ Should check `emu.row_text(row)`.
- **[ ] Multiplexer Blank Row (Title bar) Offset:** Check if `View()` or `chrome.rs` adds a blank row when running inside tmux/zellij (Pattern 6 in `tui-debugging`).
  - _Verify:_ `grep -A 5 -n 'ZELLIJ\|TMUX' crates/superzej-host/src/chrome.rs`
- **[ ] Visual Drift (Animation/Frames):** Check if `FrameModel` has frame counters that cause the TUI to slide or jump (Pattern 8 in `tui-debugging`).
  - _Verify:_ `grep -i 'frame\|anim' crates/superzej-host/src/chrome.rs`

## 2. Modal Cancel Paths, Inputs, & Command Prompts

**Objective:** Verify that text inputs, prompts, and modal overlays process keyboard combinations (Esc, Alt, Ctrl) correctly and split actions by focus.

**Evidence Collection / Validation:**

- **[ ] Host Input Action Dispatch:** Ensure `begin_new_workspace_prompt` and similar prompt actions don't just mutate state but return visible overlays, and that the submit/cancel key dispatch correctly restores prior focus (Pattern 14).
  - _Verify:_ `grep -n -B 5 -A 20 'host_input.is_some()' crates/superzej-host/src/run.rs`
- **[ ] Modal Cancel Key Fallthrough:** Ensure `wizard.rs` and `active_menu` overlays catch `Esc` _before_ the general `printable-key` filter.
  - _Verify:_ `grep -A 10 'fn handle_key' crates/superzej-host/src/wizard.rs`
- **[ ] Shifted Terminal Chords (Alt-x vs Alt-X):** Check `keymap.rs` and `Action::from_key` for case-sensitivity on Alt chords (Pattern 11 in `tui-debugging`).
  - _Verify:_ `grep -A 1 -i 'alt' crates/superzej-host/src/keymap.rs | grep 'shift'`
- **[ ] Compose Box Viewport Height:** Check that the input overlays (`InputOverlay`, `menu`) allocate at least 3 rows for a bordered input (Pattern 13 in `tui-debugging`).
  - _Verify:_ `grep -i 'height\|rows' crates/superzej-host/src/menu.rs` (if file exists) or similar overlay rendering.

## 3. Pane Split, Focus Routing, & Persistent State

**Objective:** Ensure pane deletion, drawer toggles, and UI interactions maintain consistency across the DB (`Session::persist`) and the UI representation.

**Evidence Collection / Validation:**

- **[ ] Drawer Persistence on Close:** Check `run.rs` when `drawer` or `pin` is closed/hidden to see if it calls `session.persist` and restores focus to the active pane.
  - _Verify:_ `grep -B 5 -A 20 -n 'drawer_pool.take' crates/superzej-host/src/run.rs` and the close action.
- **[ ] Focus Restoring After Tab Delete:** Check that deleting a worktree or tab leaves focus on the adjacent item without "teleporting" (Pattern 11 in `tui-debugging`).
  - _Verify:_ `grep -A 15 'fn delete_groups' crates/superzej-host/src/run.rs` (or similar delete functions).
- **[ ] Reserved Navigation Chords (Layer Dispatch):** Check `keymap.rs` for `Shift+Up/Down` accordion jumps being swallowed by focused panes (Pattern 15 in `tui-debugging`).
  - _Verify:_ Run tests `cargo test -p superzej-host --lib keymap::tests::shift_arrows_fall_through_to_accordion` (if test exists) or check `keymap.rs` for `Shift` + `Arrow` mappings.

## 4. PTY / Terminal Test Harness

**Objective:** Ensure sandbox and core terminal interactions have sufficient test coverage.

**Evidence Collection / Validation:**

- **[ ] Pre-commit Sandbox Test Isolation:** Check if `cargo test` is skipping sandbox (podman) by default unless explicitly configured (Pattern from `rust-testing-workflow`).
  - _Verify:_ `grep -A 5 -i 'cfg.sandbox.enabled' crates/superzej-host/src/panes.rs` (in `#[test]` blocks) or `crates/superzej-host/src/run.rs`.
- **[ ] AppTile Integration Gates:** Check if `superzej-dashboard` AppTile handles render/input tests and is properly wired into `AppHost` action loops.
  - _Verify:_ `cargo test -p superzej-dashboard`

---

## Execution Handoff

Ready to begin execution of the audit using `subagent-driven-development` and `systematic-debugging`. First up: investigating the event loop invariants, PTY backpressure channels, and fast-paths. Shall I proceed?
