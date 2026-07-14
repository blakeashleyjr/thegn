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
- `[merge_queue]`, `[sandbox]`, `[share]`, `[forward]`, `[media]`,
  `[replay]`, `[lifecycle]` — optional feature groups.

## Inspecting

```sh
thegn config show        # the effective merged config
thegn config get ui.language
thegn config validate
thegn doctor             # resolved terminal capabilities
```

The complete key-by-key documentation is the generated
[[config-reference]] — it can never drift from the shipped example.
