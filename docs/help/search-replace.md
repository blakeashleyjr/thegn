---
id: search-replace
title: Search & Replace
order: 10
actions: [search-replace-open]
---

# Search & Replace

Project-wide find **and change**. `Ctrl-Shift-H` opens the Search & Replace
surface — a focusable overlay, not a panel — for the active worktree. It streams
matches as you type and applies replacements through one guarded, atomic write
path with a per-match preview.

| Key            | Action                                             |
| -------------- | -------------------------------------------------- |
| `Ctrl-Shift-H` | open the Search & Replace surface                  |
| `Tab`          | move between the **search** and **replace** fields |
| `↑` / `↓`      | move the selection through the results             |
| `Ctrl-t`       | toggle the selected match (or whole file) on/off   |
| `↵`            | apply the selected replacements                    |
| `Ctrl-o`       | open the selected match in `$EDITOR` at its line   |
| `Esc`          | close                                              |

You can also reach it from the command palette (the **Search & Replace** row),
which seeds it with your current `/` Content-mode query.

## Options

Toggle with `Alt` chords while the surface is open:

| Chord   | Option                                                                       |
| ------- | ---------------------------------------------------------------------------- |
| `Alt-r` | **regex** mode — capture groups `$1`, `${2}`, `$0` expand in the replacement |
| `Alt-c` | **case**-sensitive match                                                     |
| `Alt-w` | **whole-word** match                                                         |
| `Alt-s` | **structural** (AST) mode — needs `ast-grep` on `PATH`                       |
| `Alt-h` | include **hidden** files                                                     |
| `Alt-i` | ignore `.gitignore` (**no-ignore**)                                          |

`.git/` is always excluded, symlinks out of the worktree are never followed, and
results are bounded by `[search] max_results` (a `truncated` marker shows when
the cap is hit).

## Applying safely

Each match snapshots the line it sat on. When you apply, every file is re-read
and any match whose line **changed since the scan** is skipped and reported
(never clobbered) — so a concurrent agent edit can't be lost. Writes are
atomic (temp-then-rename, permissions preserved); a read-only file is reported
per-file and the rest of the batch still applies. Deselected matches are left
byte-identical.

## Structural rewrites

`Alt-s` switches to AST-pattern search via the `ast-grep` seam. ast-grep only
**computes** matches and replacement text (argv-only, JSON) — every write still
goes through thegn's guarded path. When the binary is absent the structural mode
says so and literal/regex search is unaffected (`thegn doctor` shows the probe).

## Headless (`thegn search`)

The same engine drives a CLI verb for scripting:

```text
thegn search <pattern> [--regex] [--case] [--word] [--glob '*.rs'] [--json]
thegn search <pattern> --replace '<tpl>'            # prints a plan (dry run)
thegn search <pattern> --replace '<tpl>' --apply    # writes via the guarded path
thegn search '<ast-pattern>' --structural --lang rust --replace '<rw>' --apply
```

`search.query` needs the read scope; `search.replace` needs write.
