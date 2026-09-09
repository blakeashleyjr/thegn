# Tasks

## 1. Shared profile-order store (thegn-core)

- [x] 1.1 `profile::order` module: read/write `~/.config/thegn/profiles-order.json`
      via `util::xdg_config_home()` (never the rerooted state root); `load_order()`,
      `save_order(&[String])`, and `apply_order(known: &[String], order: &[String])`
      that returns the display order (ordered first, unknown appended
      alphabetically). **Unit tests**: unknown profiles append stably; missing/empty
      sidecar ⇒ pure alphabetical; optional `config.toml` `profile_order` seed is
      used only when the sidecar is absent.

## 2. Switcher renders in stored order (host)

- [x] 2.1 `palette::build_profile_palette` collects the full profile set (config
      `profiles` + on-disk scan + `default`) then orders it via
      `profile::order::apply_order`; `✓` active marker unchanged. **Unit test** on
      the pure ordering given a fixed set + stored order.

## 3. Reorder in the switcher overlay (host)

- [x] 3.1 Extend the `MoveItemUp`/`MoveItemDown` handler (`run.rs`) so that when
      the profile switcher overlay is open it moves the highlighted entry, updates
      the in-memory list, and persists the **entire** new order via
      `profile::order::save_order` (best-effort). Overlay repaint ⇒ `Full` frame;
      no new wake source.

## 4. Docs + help

- [x] 4.1 Document `profile_order` / the `profiles-order.json` sidecar in
      `config/config.toml.example`.
- [x] 4.2 Update the switcher/keybindings help page prose to describe reordering
      the highlighted profile with `Ctrl+Alt+↑/↓` while the switcher is open
      (no new action id ⇒ help ratchet only needs the page text).

## 5. Validate

- [x] 5.1 `cargo test -p thegn-core profile::order` + `-p thegn-host` switcher
      tests green; `cargo clippy --workspace` clean.
- [x] 5.2 Reconciled commit `0b49a318` against the sidecar, seed fallback,
      complete profile collection, switcher-only reorder path, persistence,
      and focused tests. Full workspace CI is a landing gate, not an
      outstanding profile-order requirement.
- [x] 5.3 Strict OpenSpec validation passed during archive-readiness review.
