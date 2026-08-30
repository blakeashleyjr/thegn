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
  level, undercurl, mouse, OSC 52 clipboard, synchronized output, and
  keyboard reporting (the `keyboard` row — see **Keyboard** below).

The resolved row is the truth. If it says `ascii` and you expected
Unicode, the detection or your locale is the thing to fix.

The degradation ladder is a CI gate, not a promise: `just term-check` runs
`thegn doctor` under six environments (kitty, bare xterm, `NO_COLOR`,
256-color, and the glyph/color overrides) and fails the build if any resolves
differently from the table above.

## Nix batteries terminal

`nix run github:blakeashleyjr/thegn#batteries` is a composed Nix launch path:
it runs stable thegn inside the flake's pinned Alacritty with FiraCode Nerd
Font supplied through launcher-scoped fontconfig. It does not install the font
globally. The launcher creates
`$XDG_CONFIG_HOME/thegn/alacritty.toml` (defaulting to
`~/.config/thegn/alacritty.toml`) from the shipped profile only when the file is
absent, then preserves that user-owned copy. `THEGN_ALACRITTY_CONFIG` points the
font picker at the same file.

Run `thegn doctor` in that window to verify the terminal and resolved
capabilities. The derivation is build-tested on x86_64 Linux; clean-host
interactive evidence is still required before additional hosts are called
verified. Ghostty remains a shipped profile (`config/ghostty.config`), but it
is not the emulator pinned by the batteries package. macOS font-picker and
alternate-emulator parity remain deferred until they have a macOS host
rehearsal.

The same evidence rule keeps broader distribution work out of this path: no
`install.sh --batteries`, distro package-manager mutation, Homebrew cask,
downloadable unsigned macOS app, Windows Terminal profile, or
Flatpak/AppImage/nix-bundle is promised. Those require their relevant host,
signing, or driver rehearsal before they become install instructions.

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

## Keyboard

Terminals have no distinct byte for `Ctrl-1`. The classic encoding only
covers `Ctrl` + letter and a handful of punctuation, so `Ctrl-<digit>` and
`Ctrl-Alt-<digit>` are simply not expressible — unless the terminal speaks
**xterm `modifyOtherKeys` level 2**, which reports every modified key as an
unambiguous escape sequence (`CSI 49;5u` for `Ctrl-1`).

thegn asks for level 2 at startup (`CSI > 4 ; 2 m`) and that is its **only**
disambiguation. It deliberately does not push the kitty keyboard protocol:
the terminal library thegn uses cannot decode kitty sequences that carry a
sub-parameter, which is the form Ghostty emits — every modified chord
decoded to a spill of literal characters that leaked into the focused pane.
`modifyOtherKeys` gives the same disambiguation in a form that parses
correctly, so it is the one thegn relies on.

Two chord families depend on it:

- `Ctrl-1..9` — jump to a workspace ([[workspaces-and-worktrees]]).
- `Ctrl-Alt-1..9` — launch or focus a pinned program ([[drawer-and-corner]]).

`Alt-1..9` (jump to a worktree) does **not** — `Alt` has a legacy encoding
that every terminal sends.

### When the terminal ignores the request

Alacritty, tmux without `extended-keys on`, the Linux console, older VTE,
Terminal.app and kitty-protocol-only emulators all leave level 2 off. The
chords are then not merely inert — they arrive as something else:

| chord       | what thegn actually sees         |
| ----------- | -------------------------------- |
| `Ctrl-1`    | plain `1` — nothing happens      |
| `Ctrl-2`    | `Ctrl-Space` → opens the palette |
| `Ctrl-3`    | `Escape`                         |
| `Ctrl-4..7` | junk control bytes               |
| `Ctrl-8`    | `Backspace`                      |
| `Ctrl-9`    | plain `9`                        |

`Ctrl-Alt-<digit>` has no legacy encoding at all, so the pins are dead.

### What thegn does about it

At startup thegn asks the terminal what level it actually ended up in,
rather than guessing from `TERM`. `thegn doctor` reports the answer as the
`keyboard` row under **Resolved capabilities**, in one of three states:

```
  keyboard      modifyOtherKeys=2 (Ctrl+<digit> chords OK)
  keyboard      not reported (Ctrl+1..9 / Ctrl+Alt+1..9 cannot reach thegn)
  keyboard      unknown (no probe — assuming supported)
```

The broken state prints the remedy next to it. **Unknown always means
"assume it works"** — a terminal that stays quiet is not a terminal that
said no, so nothing is taken away.

When the answer is a definite no, the [[sidebar]] stops painting the
`Ctrl-<digit>` workspace hints. The digits do not renumber and the layout
does not shift; thegn just stops advertising a chord your terminal cannot
send. Everything else is unaffected — `Alt-<digit>`, the arrows, `Alt-o`,
and the [[command-palette]] all reach the same actions.

### Fixing it

**In tmux**, enable extended keys — this is by far the most common cause,
because tmux swallows the request unless told not to:

```tmux
set -g extended-keys on
# tmux 3.4+ additionally:
set -as terminal-features '*:extkeys'
```

**Otherwise**, use a terminal that supports `modifyOtherKeys` level 2, or
rebind the family to chords your terminal can send. The workspace and pin
actions are ordinary bindable ids:

```toml
[keybinds]
summon-workspace-1 = "Ctrl Alt q"
summon-pin-1 = "Alt Shift p"
```

`summon-workspace-1` … `-9` and `summon-pin-1` … `-9` are all bindable; see
[[keybindings]] for the full list and [[config-reference]] for the
`[keybinds]` section.

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

| Terminal     | Setting                                                     |
| ------------ | ----------------------------------------------------------- |
| Terminal.app | Settings → Profiles → Keyboard → **Use Option as Meta key** |
| Ghostty      | `macos-option-as-alt = true`                                |
| Alacritty    | `[window] option_as_alt = "Both"`                           |
| kitty        | `macos_option_as_alt yes`                                   |
| WezTerm      | `send_composed_key_when_left_alt_is_pressed = false`        |
| iTerm2       | Profiles → Keys → Left/Right Option: `Esc+`                 |

Terminal.app leads the table because it is the terminal every Mac already has,
and the one the generated `thegn.app` falls back to when no other emulator is
installed — so it is the most likely place to hit this, and was the one entry
this table used to omit.

`thegn doctor` names the setting for the terminal you are actually in.

The profiles thegn ships (`config/alacritty.toml`, `config/ghostty.config`)
already set this, so `tg --standalone` and the generated macOS `thegn.app`
are unaffected; the setting is for the terminal _you_ launch thegn in.

Ghostty additionally treats an Option sequence that produces no printable
character as Alt regardless of the setting, so `Ctrl-Alt-*` may work there
before you change anything — but plain `Alt-<letter>` will not.

See [[configuration]] for the config layers, and [[config-reference]] for
every key.
