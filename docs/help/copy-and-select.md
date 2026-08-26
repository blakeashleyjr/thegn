---
id: copy-and-select
title: Copy & select
order: 15
parent: terminal-and-panes
actions:
  [
    copy-pane,
    search-pane,
    search-global,
    scroll-up,
    scroll-down,
    paste-register,
    paste-image,
  ]
---

# Copy & select

Getting text out of a pane, and finding it in the first place.

## Copying

| Key                           | Copies                                                                  |
| ----------------------------- | ----------------------------------------------------------------------- |
| `Ctrl-Alt-c` / `Ctrl-Shift-c` | the current selection, or the whole visible pane if nothing is selected |

Drag with the mouse to select; the drag itself copies on release. If
there is no selection, the copy key takes the pane's **visible screen** —
including what you have scrolled back to, not just the live tail.

Text goes out two ways at once, so it works whether or not you are over
ssh: an **OSC 52** sequence to the outer terminal, and the local system
clipboard. Anything copied also lands in the default register (`"`).

Selections are anchored to their **content**, not the screen: scroll the
viewport and the highlight stays on the lines you picked.

> A pane running a mouse-aware app (htop, lazygit) gets your drags
> forwarded to it instead. Hold `Shift` to bypass the app and select
> host-side — the convention every terminal uses.

## Scrollback

`Alt-PgUp` / `Alt-PgDn` walk the focused pane's history. Copying while
scrolled takes what you can see.

## Finding text

| Key          | Scope                      |
| ------------ | -------------------------- |
| `Ctrl-Alt-/` | the focused pane's history |
| `Ctrl-/`     | every pane at once         |

Both are incremental: type, arrow through matches, `↵` to jump. The
[[command-palette]]'s `/` mode is a different thing — it searches **file
contents** in the worktree rather than terminal output. See [[search]].

## Replay

`Alt-r` opens time-travel replay of the focused pane — scrub back through
what it printed. Needs `[replay]` enabled; see [[configuration]].

## Registers

Yanks land in named registers, vim-style. `"` is the default and `+` is
the system clipboard; some registers persist across sessions, so a yank
survives a restart. **Paste from register** (palette, or bind
`paste-register`) prompts for the register character and pastes it into the
focused pane.

## Paste an image

**Paste image** (palette, or bind `paste-image`) takes a screenshot straight
off the clipboard into the focused pane — handy for agent CLIs that accept an
image by file path. thegn reads the clipboard image **once** (only when you run
the action — never on a watcher or timer), checks it against
`[clipboard] max_image_bytes` (10 MiB by default), and drops it as a
generated-name PNG:

- a **local** pane gets a file under thegn's runtime paste dir (`0700` dir,
  `0600` file), and its absolute path is pasted;
- a **remote** pane (ssh / provider worktree) has the bytes streamed over the
  worktree's existing ssh control channel into a confined drop dir in your own
  account (`[clipboard] remote_dir`), and the **remote** path is pasted.

The register paste `"` `+` does the same when the clipboard holds no text —
text always wins. Old drops are swept on the next paste (`[clipboard]
keep_hours`); nothing runs in the background. Needs a clipboard-image tool
(`wl-paste`, `xclip`, or `pngpaste`); without one you get an honest status
message and nothing is sent. See [[configuration]] for `[clipboard]`.

See [[terminal-and-panes]] for panes and splits, and
[[terminal-compatibility]] if OSC 52 copy is not reaching your clipboard
(`thegn doctor` reports whether your terminal supports it).
