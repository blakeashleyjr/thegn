# Thegn (thegn) Release Readiness Audit Plan

> **For Hermes:** Use `subagent-driven-development` or execute sequentially to run this audit. Do NOT execute this during plan generation.

**Goal:** Conduct an unbelievably detailed audit of the native `thegn` terminal multiplexer/IDE to ensure Phase 1 "prime time" release readiness. Identify bugs, performance problems, architectural drift, and UX regressions.

**Architecture:** A single-process Rust host (`thegn-host`) orchestrating a tokio event loop, `portable-pty` panes, `termwiz` diff-flush rendering, and an embedded SQLite state store. The system is entirely event-driven, blocking on `poll_input(None)`, with an invariant of ~0% idle CPU. It replaces the old zellij+WASM architecture.

**Tech Stack:** Rust (Edition 2024), tokio, termwiz, portable-pty, rusqlite, gix, taffy.

---

## 1. Scope

- **Performance & Jank:** First paint latency, PTY starvation, rendering efficiency, and instantaneous tab switching.
- **TUI Robustness & Rendering:** Terminal multiplexer pane title bar interference, layout geometry regressions (flickering, stale text), focus routing, modal cancel paths, and keybind resolution.
- **State & Persistence:** SQLite DB schema migration (v5 -> v6+), WAL synching, session resurrection, and persistent UI state.
- **IDE Parity (Phase 1 / Tier 1):** Full native git management, test explorer readiness, problems panel, run/task configurations, Search Everywhere palette, and attention routing.
- **Testing & Verification:** 95% line coverage on `thegn-core`, clean CI pipelines, and hermetic end-to-end smoke testing.

---

## 2. Current Evidence to Collect

To perform the audit, we will run the following read-only / verification commands:

**Verification Commands (Execution Phase):**

1. **Formatting:** `nix fmt -- --ci`
2. **Linting:** `cargo clippy --workspace --all-targets -- -D warnings` and `just lint` (includes shellcheck, yamllint, taplo).
3. **Tests:** `cargo test --workspace`
4. **Coverage:** `just coverage` (Checks `thegn-core` for ≥95% lines, excluding I/O seams).
5. **Docs:** `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace`
6. **Smoke Test:** `just smoke`
7. **Performance Benchmarks:** `just bench` (Measure baseline, cold launch to first frame, warm launch).

---

## 3. Audit Matrix

| Category           | Pass Criteria                                                             | Evidence Command / Path                               | Expected Output                                                              | Remediation Owner |
| ------------------ | ------------------------------------------------------------------------- | ----------------------------------------------------- | ---------------------------------------------------------------------------- | ----------------- |
| **Performance**    | Cold launch → first frame < 300ms. No blank/flash on startup.             | `just bench` / inspect `run.rs`                       | `thegn::startup` traces show instant cheap model build before git hydration. | Core Team         |
| **Performance**    | PTY channel has bounded backpressure / drain budget.                      | `rg -A 5 "try_recv" crates/thegn-host/src/run.rs`     | Drain budget implemented to prevent PTY starvation.                          | Core Team         |
| **TUI Layout**     | No stale cells survive layout changes.                                    | Inspect `crates/thegn-host/src/run.rs` rendering      | `full_repaint = true` clears logical frame on geometry change.               | Core Team         |
| **TUI Render**     | Multiplexer title bars do not overwrite row 0.                            | Inspect `crates/thegn-host/src/chrome.rs`             | Row 0 offset or `""` prepend logic exists if running inside tmux/zellij.     | Core Team         |
| **UX & Focus**     | Keybinds correctly fall through to PTY only when focused.                 | Inspect `route()` in `crates/thegn-host/src/focus.rs` | `forwards_to_pane` returns true only for Center or when drawer open.         | Core Team         |
| **Architecture**   | WASM IPC removed, replaced by in-process `SidebarState` and SQLite cache. | Search for `WASM` or `zellij_tile` dependencies       | No WASM plugins running in critical render path.                             | Core Team         |
| **Data Integrity** | Database migrations (to v6) preserve layout and worktrees gracefully.     | Review `crates/thegn-core/src/db.rs`                  | v5 to v6 migration runs atomically, handles malformed JSON safely.           | Core Team         |

---

## 4. Detailed Audit Tasks

### Task 1: Audit Performance & PTY Flow

**Objective:** Verify that `thegn` does not suffer from PTY flood starvation and renders the first frame instantly.
**Verification Steps:**

- Inspect `crates/thegn-host/src/run.rs` around `poll_input` and `try_recv()`.
- Ensure `drain_stats_chunks` or bounded budgets are correctly stopping long drains so the loop can render.
- Ensure `build_initial_model` is synchronous and cheap, and `kick_git_docs_fetch` runs entirely on `spawn_blocking`.

### Task 2: Audit TUI Geometry & Rendering Robustness

**Objective:** Ensure no visual ghosting, proper layout clearing, and correct handling of pane boundaries.
**Verification Steps:**

- Inspect `crates/thegn-host/src/compositor.rs` and `run.rs` for `full_repaint`.
- Verify the logical frame clear strategy (`Change::ClearScreen`) is used when `geometry_changed` is true.
- Audit `crates/thegn-host/src/chrome.rs` to ensure tab labels render in `tabbar_content` bounds to prevent far-left flashes.
- Check if terminal multiplexers (zellij/tmux) steal the first line (TUI Debugging Pattern 6).

### Task 3: Audit Event Loop Invariants & Sandboxing

**Objective:** Verify 0% idle CPU and correct event polling.
**Verification Steps:**

- Ensure `poll_input(None)` is the only blocking call in the main loop.
- Ensure all `spawn_blocking` and filesystem watchers (`notify`) pulse the `TerminalWaker`.
- Verify `THEGN_LOGIN_SHELL` flag logic avoids expensive login-shell startups by default (`crates/thegn-host/src/panes.rs`).

### Task 4: Audit Keybinds & Modal Cancel Paths

**Objective:** Ensure commands, overlays, and keybind locks function flawlessly.
**Verification Steps:**

- Verify `Ctrl+g` (locked mode) blocks all input routing except unlock.
- Audit command palette (`crates/thegn-host/src/palette.rs`) to ensure `Esc` correctly tears down the modal.
- Verify modal actions explicitly fall through or are consumed without swallowing legitimate keys.

### Task 5: Documentation & Roadmap Alignment Audit

**Objective:** Confirm implementation matches `CLAUDE.md`, `README.md`, and roadmap tier specs.
**Verification Steps:**

- Check `docs/superpowers/specs/2026-06-10-ide-feature-tiers-design.md` against actual codebase features (Search Everywhere, testing surfaces, diagnostic problems).
- Read `README.md` to ensure zellij/WASM references reflect the current native `thegn` architecture correctly.
- Review `tasks.md` to update progress markers for Phase 1 shell completion.

---

## 5. Risks and Blockers

- **Waker Starvation:** If a background task panics or fails to pulse the `TerminalWaker`, the UI will freeze indefinitely since there is no tick timeout.
- **Nix Environment:** Benchmarks and native C/GPU dependencies require `nix develop`. Executing the audit outside Nix may yield false negatives on builds.
- **Edition 2024 Constraints:** Usage of async traits across crate boundaries (e.g., `thegn-svc`) must be monitored to ensure it doesn't break compilation or require unwanted boxing overhead.

---

## 6. Execution Handoff

Plan complete and saved. Ready to execute the comprehensive readiness audit using `subagent-driven-development` or sequential task execution. I'll dispatch subagents or run the verification commands step-by-step to compile a detailed report of bugs, performance issues, and required improvements.

Shall I proceed with executing Task 1?
