---
id: theming
title: Themes
order: 25
actions: [cycle-theme, theme-builder-open]
---

# Themes

Press `Ctrl-Alt-t` to cycle the merged catalog of built-in and saved local
themes. Press `Ctrl-Alt-Shift-t` to open the centered **Theme Builder** popup.
The popup dims the workspace, keeps unsaved edits transient, and previews
palette changes live across the sidebar, tabs, status bar, diffs, panes, and
activity indicators.

## Theme Builder keys

- `↑` / `↓` selects a built-in or user theme in the catalog.
- `Tab` / `Shift-Tab` moves focus through the editable tokens and the Apply row.
- `Enter` edits the focused token, or applies the draft from the Apply row.
- `←` / `→` moves within an editing value; `Backspace` deletes; `#rgb` and
  `#rrggbb` are accepted color values.
- `Esc` cancels the active field, reverts a token edit, or closes the popup and
  restores the palette captured when it opened.
- `Ctrl-S` opens **Save as**; enter a name and press `Enter` to save it.
- `i` opens a local-path import field; press `Enter` to preview a Gogh YAML or
  JSON theme. Bracketed paste is inserted as field data, never run as a
  command.

Mouse clicks use the same popup rectangle as painting. A click outside follows
the modal dismissal setting and cannot discard dirty edits. Apply failures stay
visible in the popup; the theme closes only after a successful write.

## CLI

The headless commands use the same names and local theme directory:

```sh
thegn theme list
thegn theme set prism
thegn theme set my-theme
thegn theme import ~/Downloads/theme.yml --name my-theme
```

`theme set` accepts a built-in or valid local name and writes the existing
`[theme].preset` key. A local selection also writes its existing
`[theme.colors]` and `[theme.hues]` overrides, so it remains effective after a
reload. `theme import` reads only a bounded local file, validates the Gogh
scheme, and saves a versioned TOML user theme. It performs no network access.

User themes live under `$XDG_CONFIG_HOME/thegn/themes` (normally
`~/.config/thegn/themes`). Files with invalid content are ignored by the
catalog and reported by the popup's store status. A local file whose name
matches a built-in cannot shadow that built-in.

The shipped Gogh importer accepts `name`, optional `variant`,
`background`, `foreground`, `cursor`, and `color_01` through `color_16`.
Foreground maps to text, background to `bg0`, and cursor to focus; the ANSI
colors seed semantic hues and are resolved into the same palette roles used by
the rest of the UI.
