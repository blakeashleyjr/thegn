# Platform: native Windows

## ADDED Requirements

### Requirement: The compositor targets Windows Terminal and refuses conhost

On native Windows the compositor SHALL start only when the environment shows
evidence of a modern terminal (`WT_SESSION`, a known-modern
`$TERM`/`$TERM_PROGRAM`, an explicit truecolor advertisement, or a 256-color
`$TERM`). Legacy conhost.exe MUST be refused at startup with an error naming
Windows Terminal — degrading silently into broken rendering is not an option.
Under Windows Terminal, capability detection MUST resolve Full Unicode,
undercurl, and synchronized output without POSIX locale variables.

#### Scenario: Launch inside Windows Terminal

- **WHEN** `thegn` starts with `WT_SESSION` set and no `LANG`/`LC_*`
- **THEN** the compositor runs with truecolor + Full Unicode glyphs +
  undercurl + DECSET-2026 sync, not the ASCII/basic fallback

#### Scenario: Launch inside bare conhost

- **WHEN** `thegn` starts on Windows with no modern-terminal evidence
- **THEN** it exits with an error pointing at Windows Terminal instead of
  rendering degraded chrome

### Requirement: Pane shells resolve and invoke by platform dialect

New-pane shells SHALL resolve platform-natively: `$SHELL`/probe-chain on unix,
pwsh → powershell → `%COMSPEC%` on Windows — never a hardcoded `/bin/sh` on a
host that lacks it. Shell argv construction SHALL apply POSIX interactive/login
flags (`-i`/`-l`) only to POSIX-flavored shells; PowerShell and cmd.exe get a
bare argv.

#### Scenario: New tab on Windows

- **WHEN** a worktree tab opens its default pane on native Windows with pwsh
  installed
- **THEN** the pane spawns `pwsh.exe` with no arguments (no `-i`, no `-l`)
  under ConPTY

### Requirement: Display-path basenames are separator-agnostic

Anywhere a display name is derived from a filesystem-absolute path (tab
titles, sidebar/search labels, overlays, toasts, share labels, provider
inference) the derivation SHALL treat `/` and `\` as separators (via
`util::basename`), and provider inference SHALL strip a trailing `.exe`.
Git-relative paths (which git emits with `/` on every platform) keep plain
`'/'` handling.

#### Scenario: Windows worktree title

- **WHEN** a worktree at `C:\Users\u\worktrees\feature-x` is shown in the tab
  bar or search labels
- **THEN** the displayed leaf is `feature-x`, not the full backslashed path
