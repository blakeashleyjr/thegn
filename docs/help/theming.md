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

The command palette also lists it as **Open theme builder**.

- `↑` / `↓` selects a built-in or user theme in the catalog.
- `v` switches the editor column between the **token view** (the twenty
  palette tokens) and the **element view**, which groups the UI by what each
  token paints: sidebar, tabs and masthead, status bar, panes, diff, activity
  dots, git graph, and popups. Each element row shows a live swatch, the
  element, and the token behind it; a token can appear under several elements.
- `Tab` / `Shift-Tab` moves focus through the rows and the Apply row.
- In the element view, the selected group's part of the workspace (sidebar,
  masthead, status bar, center or panel) stays lit behind the builder while the
  rest dims; if that part is hidden, the builder says so.
- In the element view, `Enter` or `Space` on a group header collapses or
  expands it, and `←` / `→` collapse or expand the group under the cursor.
- `Enter` on a token or element opens the **palette picker** for that token,
  or applies the draft from the Apply row.
- `Esc` cancels the active field or picker, or closes the popup and restores
  the palette captured when it opened.
- `Ctrl-S` opens **Save as**; enter a name and press `Enter` to save it.
- `i` opens a local-path import field; press `Enter` to preview a Gogh YAML or
  JSON theme. Bracketed paste is inserted as field data, never run as a
  command.

## Palette picker

The picker lists three blocks of swatches: the theme's own token colors, the
ANSI 16, and a 12-hue by 7-lightness ramp.

- Arrow keys or `h` `j` `k` `l` move the cursor. Every move previews the color
  live across the workspace.
- `Enter` keeps the color. Like a typed edit, it stays transient until you
  apply or save the draft.
- `Esc` reverts the token to the value it had when the picker opened.
- `#` switches to typed hex input for the same token: `←` / `→` move within the
  value, `Backspace` deletes, and `#rgb` and `#rrggbb` are accepted. `Enter`
  keeps the value and `Esc` reverts it.

Under the preview, a contrast line reports the selected token's worst
contrast pair, for example `text contrast 2.1:1` and
`against bg0, below 4.5:1`. Amber marks a pair below its floor, red a pair
below 3:1.

While a token, element or picker row is selected, the preview underlines each
sample that uses that color and marks its line with a leading arrow. A
`highlighted: …` legend under the preview lists the interface elements the
color paints, or reads `not shown in preview` when the sample has no segment for
it. The highlight lives in the builder's preview, not on the workspace behind
the popup.

Each catalog row ends with a six-cell swatch strip (bg0, panel, accent, focus,
green, red) painted from that theme's own palette. The strip is dropped when the
catalog column is too narrow to fit a name beside it.

## Mouse

Mouse clicks use the same popup rectangle as painting. Click a catalog row to
select a theme, a token or element row to focus it, a group header to collapse
or expand it, and an element's swatch to open the picker. In the picker, the
first click on a swatch previews it and a second click keeps it. The scroll
wheel scrolls the list under the pointer: the catalog, the element view, or
the picker. A click outside follows the modal dismissal setting and cannot
discard dirty edits. Apply failures stay visible in the popup; the theme
closes only after a successful write.

## CLI

The headless commands use the same names and local theme directory:

```sh
thegn theme list
thegn theme set prism
thegn theme set my-theme
thegn theme import ~/Downloads/theme.yml --name my-theme
thegn theme rename my-theme my-new-theme
```

`theme set` accepts a built-in or valid local name and writes the existing
`[theme].preset` key; the runtime resolver loads the selected local file and
applies any existing `[theme.colors]` and `[theme.hues]` overrides on top.
`theme import` reads only a bounded local file, validates the Gogh scheme, and
saves a versioned TOML user theme. It reports any contrast warnings and
performs no network access.

`theme rename OLD NEW` moves a local theme to a new name: the theme's file is
renamed with it, because the file name is the theme's identity. The new file is
written first and the old one removed afterwards, so an interruption never
loses a theme. It refuses to rename a built-in, the theme you currently have
selected (select another with `theme set` first), a name another theme already
uses, or a name that maps to the same file as the old one.

User themes live under `$XDG_CONFIG_HOME/thegn/themes` (normally
`~/.config/thegn/themes`). Files with invalid content are ignored by the
catalog and reported by the popup's store status. Each file must be named after
the theme's slug; duplicate theme names are reported and ignored. A local file
whose name matches a built-in cannot shadow that built-in.
Saving or importing never replaces a theme file that already exists unless
the builder loaded that exact file: `thegn theme import` refuses a name that
is already taken, and **Save as** over an existing theme is refused when the
file changed on disk after the catalog loaded it (reopen the builder to reload it).
Saving or importing is also refused when another `.toml` theme file claims the
same exact name or slug, even if that file's filename does not match. Rename or
remove the claiming file before saving again.

The shipped Gogh importer accepts `name`, optional `variant`,
`background`, `foreground`, `cursor`, and `color_01` through `color_16`.
Foreground maps to text, background to `bg0`, and cursor to focus; the ANSI
colors seed semantic hues and are resolved into the same palette roles used by
the rest of the UI.
