# superzej sidebar plugin (Phase 2 — not yet implemented)

A small Rust → WASM `zellij-tile` plugin that renders the worktree dashboard as an
always-visible sidebar tile, replacing the `superzej dashboard --watch` pane.

Planned design:

- **Subscribes to** `TabUpdate`, `PaneUpdate`, and `Timer` (periodic git refresh).
- **Renders** per active tab: repo name, and one row per worktree pane — branch,
  agent, dirty/clean, ahead/behind. Highlights the focused pane.
- **Fed by scripts** via `zellij pipe --plugin file:~/.local/share/superzej/sidebar.wasm
--name superzej_status -- '<json>'`; the plugin implements `pipe()` to ingest the
  same JSON that `superzej list --json` already emits.
- **Click-through:** clicking a row focuses that tab / runs `superzej` actions.

Build (when implemented): `crane` + `fenix` targeting **`wasm32-wasip1`** (not
nixpkgs `pkgsCross.wasi32`'s `wasm32-unknown-wasi`). Add as a separate flake output
`packages.superzej-plugin` and ship to `~/.local/share/superzej/sidebar.wasm` via the
home-manager module; uncomment the `Alt-s` keybind in `layouts/superzej.kdl`.
