---
id: terminal-compatibility
title: Terminal compatibility
order: 31
parent: configuration
---

# Terminal compatibility

thegn always composes its frame in truecolor and Unicode, then **degrades
at the edges** to whatever your terminal actually supports. You should
never have to configure this — but when a glyph renders as a box or the
colors look wrong, these are the knobs.

## Start with doctor

```sh
thegn doctor
```

It prints three things worth reading together:

- **Terminal environment** — what was detected (`TERM`, `COLORTERM`,
  `NO_COLOR`, …).
- **Config modes** — what `[theme]` asked for.
- **Resolved capabilities** — what the two produced: color depth, glyph
  level, undercurl, mouse, OSC 52 clipboard, synchronized output.

The resolved row is the truth. If it says `ascii` and you expected
Unicode, the detection or your locale is the thing to fix.

The degradation ladder is a CI gate, not a promise: `just term-check` runs
`thegn doctor` under six environments (kitty, bare xterm, `NO_COLOR`,
256-color, and the glyph/color overrides) and fails the build if any resolves
differently from the table above.

## Color

`[theme] color` — `auto` (default) sniffs `COLORTERM` / `TERM` /
`WT_SESSION` / `NO_COLOR` and degrades **truecolor → 256 → 16 → mono**.
Pin it with `"truecolor"`, `"256"`, `"16"`, or `"none"`/`"mono"`.

The `NO_COLOR` environment variable forces `none` unless you set an
explicit value in config. Env override: `THEGN_THEME_COLOR`.

## Glyphs

`[theme] glyphs` — `auto` sniffs the locale and terminal; `"unicode"`
forces the rounded look; `"ascii"` forces 7-bit fallbacks (`+ - | * o ^
v`) for bare terminals or fonts without box-drawing glyphs. Env override:
`THEGN_THEME_GLYPHS`.

Set `"ascii"` if borders, arrows, or the logotype render as tofu.

`[theme] agent_glyphs` controls the sidebar's per-worktree agent marker
separately, because it is the one place a Nerd Font helps most:

- `"letter"` (default) — universal 1–2 letter marks that render in any
  font (`C` claude, `Y` yazi, `Lg` lazygit, `Ed` editor, `D` diff).
- `"symbol"` — compact Unicode marks, degrading to letters on ASCII-only
  terminals.
- `"auto"` — symbols only on a confirmed-modern emulator.

Either way the focused [[sidebar]] row spells the agent's name out beside
its mark.

## Undercurl

`[theme] undercurl` — `auto` detects support from `$TERM` /
`$TERM_PROGRAM` / `$VTE_VERSION`; `"on"` / `"off"` force it. Terminals
without it fall back to a single underline, so nothing is lost.

## Stats icons

The masthead's stats widgets default to single-width Nerd Font glyphs. If
your font lacks them, set plain text instead:

```toml
[stats]
cpu_icon = "CPU"
mem_icon = "MEM"
```

Use single-width PUA glyphs (`U+E000`–`U+F8FF`) only — the plane-15
Material Design set advances two cells and leaves a gap between the icon
and its value. See [[bars]] for what the widgets show.

## Mouse

Detected, not configured. When the terminal reports mouse support, the
chrome is clickable — [[sidebar]] rows, tabs, panel sections, the status
bar's `?` and badges — and a pane running a mouse-aware app (htop,
lazygit) gets the events forwarded. Hold `Shift` to bypass the app and
force host-side selection, the convention every terminal uses.

## macOS: Option must send Alt

Not detected — you have to set it, once, in your terminal. thegn's primary
chords are Alt-based (`Alt-w`, `Alt-o`, `Alt-s`, `Alt-.`, and every
`Ctrl-Alt` chrome toggle). macOS's default is for **Option to compose
characters** rather than act as Alt, so `Alt-w` types `∑`, nothing happens,
and the key looks dead rather than unbound.

| Terminal  | Setting                                              |
| --------- | ---------------------------------------------------- |
| Ghostty   | `macos-option-as-alt = true`                         |
| Alacritty | `[window] option_as_alt = "Both"`                    |
| kitty     | `macos_option_as_alt yes`                            |
| WezTerm   | `send_composed_key_when_left_alt_is_pressed = false` |
| iTerm2    | Profiles → Keys → Left/Right Option: `Esc+`          |

The profiles thegn ships (`config/alacritty.toml`, `config/ghostty.config`)
already set this, so `tg --standalone` and the generated macOS `thegn.app`
are unaffected; the setting is for the terminal _you_ launch thegn in.

Ghostty additionally treats an Option sequence that produces no printable
character as Alt regardless of the setting, so `Ctrl-Alt-*` may work there
before you change anything — but plain `Alt-<letter>` will not.

See [[configuration]] for the config layers, and [[config-reference]] for
every key.
