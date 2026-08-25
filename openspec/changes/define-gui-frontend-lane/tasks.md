# Tasks — define-gui-frontend-lane

## 1. Dependency gates

- [ ] 1.1 Add the GUI substrate list (`egui`, `eframe`, `iced`, `winit`,
      `wgpu`, `gpui`, `tauri`, `slint`, `druid`) to `deny.toml` bans, with a
      comment pointing at this change as the lane decision (same style as the
      `vt100`/`russh` bans).
- [ ] 1.2 Extend `crates/thegn-core/tests/crate_boundaries.rs` with the same
      list as substrates with an **empty owner set**, so the failure message
      names the frontend-lane rule rather than a generic ban.
- [ ] 1.3 Verify `just deps-audit` and the boundary test pass on the current
      tree (nothing in the workspace links any of these today).

## 2. Record the decision

- [ ] 2.1 Add a frontend-lane note to `docs/ARCHITECTURE.md` (§6 external
      doors or a short §11): terminal UI is the reference frontend; any GUI is
      a thin client of the daemon; the gate that enforces it.
- [ ] 2.2 Add the roadmap item to `tasks.md` group **AP**: native GUI frontend,
      gated on `add-runtime-session-split` + pinned attach wire +
      `add-ui-component-contract`, thin-client only (cite THE-40).

## 3. Validate

- [ ] 3.1 Run `just ci` once (includes deps-audit, boundary tests, and
      openspec-validate).
