---
id: onboarding
title: The welcome tour
parent: getting-started
---

# The welcome tour

The first time thegn starts, it opens a full-screen welcome tour. It takes
about two minutes. Each of its eight screens can be skipped, and nothing is
saved until the last one.

| Key                      | What it does                                          |
| ------------------------ | ----------------------------------------------------- |
| `Enter`                  | Accept this screen and continue                       |
| `Tab`                    | Skip this screen: its choice goes back to your config |
| `Shift-Tab`, `Backspace` | Go back one screen                                    |
| `↑` `↓` (or `j` `k`)     | Choose in a list                                      |
| `←` `→`                  | The second choice on a screen (keymap, backend)       |
| `Esc`, `Ctrl-C`          | Leave the tour now; nothing is saved                  |

## The screens

1. **Welcome.** `a` here opens the classic setup wizard instead (forge auth,
   issue trackers, ssh hosts, paths).
2. **The workspace model.** A small diagram of a project, its worktrees, the
   tab strip and the panes. `↑` `↓` walks through the parts.
3. **Pick your look.** Moving through the themes recolours the whole screen at
   once. `←` `→` picks a keymap preset (`default`, `vscode`, `jetbrains`).
4. **Sandboxes, honestly.** What `open`, `standard` and `sealed` allow, and
   what no tier protects. Each backend in your chain is listed with the same
   state `thegn doctor` reports: ready, not running (with the command that
   starts it), not installed, or not on this OS. Start a runtime and press `r`
   to check again. When nothing is ready, the screen says that sandboxed
   launches will be refused. See [[sandbox-profiles]].
5. **Your coding agent.** The agent CLIs found on `PATH` (Claude, Codex, pi,
   Aider, Antigravity). The one you pick becomes the first `[[agents]]` entry.
   An agent that is not installed cannot be picked.
6. **Remote offload.** Sprites, Vercel and ssh hosts, and how to set them up
   later. The tour never asks for a token. See [[environments]] and
   [[vercel-sandbox]].
7. **Keys you'll use daily.** Press the real chords for new worktree, split,
   palette and close. The diagram reacts; nothing is created. The chords come
   from your keymap, so a rebound chord is the one you practise. After three
   misses the tour suggests your terminal may not be sending the key; see
   [[terminal-compatibility]].
8. **Finish.** The exact changes, as old and new values, then three things to
   try next. `Enter` saves, `a` saves and opens the classic setup wizard, and
   `Esc` closes without saving.

## What gets written

| Choice          | Where                                                  |
| --------------- | ------------------------------------------------------ |
| Theme           | `theme.preset` in `config.toml`                        |
| Keymap preset   | thegn's state database, as the keymap picker stores it |
| Sandbox tier    | `sandbox.profile` in `config.toml`                     |
| Sandbox backend | `sandbox.backend` in `config.toml`                     |
| Default agent   | the order of `[[agents]]` in `config.toml`             |

A choice that matches your current config writes nothing. The writes keep your
comments and every other key. If `config.toml` has no `[[agents]]` yet, the
tour writes your pick followed by the built-in entries, so the plain shell
stays available. A toast reports when the save finishes or fails.

To undo a change, run `thegn config edit`, or open the tour again.

## Opening it again

The tour opens once, on the first launch. To see it again, open the
[[command-palette]] and run **Welcome tour…**. `thegn setup` opens the
classic setup wizard. Headless and test launches set
`THEGN_SKIP_ONBOARDING=1` to skip both.

The tour does not animate. It fits an 80x24 terminal, and every status has a
word or symbol, so it reads the same with `NO_COLOR` set or
`[theme] glyphs = ascii`.
