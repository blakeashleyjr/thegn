---
id: configuration
title: Configuration
order: 30
actions: [mode-normal, mode-vim-normal, mode-vim-insert, mode-emacs]
---

# Configuration

Behavior lives in `~/.config/thegn/config.toml`. Layers, low to high:
built-in defaults < the config file < `THEGN_*` environment variables <
CLI flags. A repo-root `.thegn.{toml,yaml,yml,json}` overlays per-repo
settings (sandbox, keybinds, env selection).

`[workspace.<slug>]` in your own config refines settings for one repo —
including `[workspace.<slug>.merge_queue]` and
`[workspace.<slug>.pr_queue]`, which is where a repo whose gate,
integration branch, or review rules differ from your defaults belongs. `thegn
config explain <key>`, run inside the repo, names the layer that won.

The file is watched: edits apply live, no restart.

## Highlights

- `[theme]` — `accent` recolors every surface; presets cycle with
  `Ctrl-Alt-t`; color/glyph fidelity degrade automatically per terminal.
- `[keybinds]` (+ `[keybinds.vim_normal]`, `[keybinds.emacs]`) — rebind
  anything; the [[keybindings]] page always shows the _effective_ result.
- **Keymap modes**: Normal (default), VimNormal (with a `Space` leader
  layer, plus a vim-insert passthrough mode), and Emacs. Switch live with
  `Ctrl-Alt-n` / `Ctrl-Alt-v` / `Ctrl-Alt-e`; `keymap_preset =
"vscode"|"jetbrains"` overlays familiar IDE chords.
- `[[actions]]` — custom shell or composite actions, surfaced in the
  [[command-palette]] and bindable.
- `[[agents]]` / `[[tools]]` — the `Alt-w` "what to run" picker entries.
- `[merge_queue]`, `[pr_queue]`, `[sandbox]`, `[share]`, `[forward]`,
  `[media]`, `[replay]`, `[lifecycle]` — optional feature groups.

## Inspecting

```sh
thegn config show        # the effective merged config
thegn config get ui.language          # any dotted key; --json for real types
thegn config set merge_queue.regenerate_paths '["Cargo.lock", "pnpm-lock.yaml"]'
thegn config explain merge_queue.gate_command   # value + which layer set it
thegn config validate    # --strict also rejects *reserved* provider kinds
thegn doctor             # resolved terminal capabilities + every provider's probe
thegn keys list          # every binding, grouped by zone (--json, --zone)
thegn keys validate      # chord conflicts; exits non-zero, so it fits a hook
thegn keys hints --zone sidebar   # what that zone's hint strip renders
```

Some provider `kind` values are **reserved**: the name is accepted so a
config stays forward-compatible (for example `[ci] provider = "drone"`,
`[[forges]] kind = "forgejo"`, `[media] backend = "jellyfin"`), but this build
has no implementation behind it. A reserved value loads with a warning and
falls back to the default; `thegn config validate --strict` rejects it by
name, and `thegn doctor` lists it as unavailable with the reason.

`keys list` covers the keymap registry **and** the zone-local tables — the
sidebar's row keys and each panel section's row-mode keys. Those are handled
by the focused zone rather than the registry, so they are not rebindable, but
they are listed so nothing is hidden. See [[keybindings]] for the rebindable
set.

The complete key-by-key documentation is the generated
[[config-reference]] — it can never drift from the shipped example.
