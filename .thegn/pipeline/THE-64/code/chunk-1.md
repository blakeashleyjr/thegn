# THE-64 — chunk 1: the `[ui] sidebar_dividers` config key

Read `.thegn/pipeline/THE-64/architect/design.md` §6 first. Work in
`/home/blake/.superzej/worktrees/thegn/tg-the-64-sidebar-distinction` on branch
`tg/the-64-sidebar-distinction`.

## Ordering

**Run this chunk FIRST.** Chunk 2 consumes the `UiConfig` field this chunk adds
and will not compile without it. File sets are disjoint — the dependency is the
field, not a shared file — but chunk 2 must start after this one lands.

## Files touched (exact)

- `crates/thegn-core/src/config_ui.rs`
- `config/config.toml.example`

Touch nothing else. In particular do **not** edit `sidebar_view.rs` — chunk 2
owns the `SidebarDisplay` wiring.

## What to build

Add one documented boolean to `[ui]`:

1. **Field** on `UiConfig` in `crates/thegn-core/src/config_ui.rs`. Put it
   directly after `sidebar_nav_skips_collapsed` (the field declared at
   `config_ui.rs:71`), so the sidebar-behaviour keys stay together:

   ```rust
   /// Lay out a one-row separator gap above each workspace header in the full
   /// sidebar, so adjacent repos read as separate groups instead of one stack
   /// of bands. Off ⇒ the tree lays out exactly as it did before the key
   /// existed (for vertically-tight setups: many repos, a short terminal).
   /// Never applies in the rail or while the `/` filter is active.
   pub sidebar_dividers: bool,
   ```

2. **Default `true`** in `impl Default for UiConfig`
   (`config_ui.rs:116-144`), in the same position — beside
   `sidebar_nav_skips_collapsed: true` at `config_ui.rs:125`.

3. **Document it** in `config/config.toml.example`, in the `[ui]` section
   beside the other `sidebar_*` keys (around
   `config/config.toml.example:130-135`). Match the file's house style — the
   key at its default value with a trailing `#` comment. Something like:

   ```toml
   sidebar_dividers = true  # blank separator row above each workspace header (off = the old dense layout)
   ```

   This file is not optional: `crates/thegn-core/tests/config_example.rs` is a
   drift test that **fails** unless every `Config` key appears here, and the
   in-app config-reference help page is generated from it
   (`crates/thegn-core/src/help/config_ref.rs:284`) — so this one edit covers
   the help surface too, with no hand-written page.

## Notes / traps

- A `bool` adds **no** `config_enum` definition, so the pinned count of `90`
  at `crates/thegn-core/src/config_validate.rs:619-624` must **not** change. If
  you find yourself editing that pin, you have added an enum by mistake.
- Do not add a ratchet entry anywhere (`test/*-ratchet.txt` are shrink-only).
- `thegn-core` is gated at 95% line coverage; a plain `bool` field with a
  `Default` is covered by the round-trip test below.

## Tests

Add a unit test in `config_ui.rs`'s existing `mod tests` (`config_ui.rs:146`),
next to `sidebar_show_jj`'s assertion at `config_ui.rs:250`:

- an empty `[ui]` table parses with `sidebar_dividers == true` (the default);
- `sidebar_dividers = false` round-trips to `false`.

Run — **scoped only, never a full-workspace gate**:

```sh
just quick thegn-core
cargo nextest run -p thegn-core config_ui
cargo nextest run -p thegn-core --test config_example
```

Do **not** run `just test`, `just ci`, `just coverage`, or e2e.

## Done criteria

- `UiConfig::sidebar_dividers` exists, defaults to `true`, documented in
  `config/config.toml.example`.
- The three scoped commands above pass.
- `crates/thegn-core/src/config_validate.rs`'s `90` pin is untouched; no
  `test/*-ratchet.txt` file changed.
- Committed on `tg/the-64-sidebar-distinction` with **exactly** this subject:

  ```text
  feat(config): [ui] sidebar_dividers key (THE-64)
  ```
