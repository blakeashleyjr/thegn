# Design

## Where the order lives

Profiles have separate per-profile DBs, so the order cannot live in `thegn.db`
(each profile would see a different list). It lives in **shared, never-rerooted
config** at the real `XDG_CONFIG_HOME`:

- `~/.config/thegn/profiles-order.json` — `{ "order": ["default","personal",…] }`,
  written programmatically on reorder (mirrors the existing advisory
  `~/.config/claude-profiles/roster.json`-style sidecar pattern: a small JSON the
  code owns, kept out of the hand-commented `config.toml`).
- An optional `profile_order = [...]` key in the shared `config.toml` may **seed**
  the initial order; the sidecar (when present) is authoritative for writes.

Read via `util::xdg_config_home()` **directly** (not the rerooted state root),
exactly as `Config::profile_overlay_path` and `build_profile_palette`'s on-disk
scan already do. No SQLite schema change ⇒ **no `user_version` bump**.

## Switcher ordering

`build_profile_palette` currently collects into a `BTreeSet` (alphabetical). New
behavior: collect the full set (config `profiles` + on-disk + always `default`),
then sort by the stored order; any profile **not** in the order list is appended
in stable alphabetical order (so a freshly-created profile appears predictably at
the end until moved). The active-profile `✓` marker is unchanged.

## Reorder affordance

The switcher is a palette overlay, so the sidebar `move-item-up/down`
(`Ctrl+Alt+↑/↓`) handler is extended to act on the **highlighted switcher entry**
when the profile switcher overlay is open: move it up/down in the in-memory list,
then persist the **entire** new order (not a two-element swap) — the same
"persist whole visible order" rule `handlers/sidebar_reorder.rs` documents, so a
rebuild from disk matches what the user saw. Persist is a best-effort write to the
shared sidecar (`let _ =`; the DB/config is a cache, git/config on disk is truth).

## Event loop / rendering

Reordering mutates only overlay state ⇒ the master `dirty`/`chrome` channel ⇒ a
`Full` frame (overlay change), which `render_plan::plan` already classifies. No
new wake source, no timer, no polling — the 0%-idle contract is untouched.

## Help

The profile switcher is reached via the command palette / `switch-profile`
action. The new reorder-in-switcher behavior is documented on the existing
switcher/keybindings help page (help context: the command-palette/keybindings
page); the `move-item-up`/`move-item-down` action ids already exist, so no new
action id is minted — the ratchet only needs the page prose updated.

## Alternatives considered

- **Order in each profile's DB** — rejected: profiles don't share a DB, so no
  single source of truth.
- **Order only in `config.toml`** — rejected for writes: rewriting a hand-edited,
  commented TOML on every reorder is lossy; the JSON sidecar is safe to rewrite,
  with the TOML key kept as an optional seed.
