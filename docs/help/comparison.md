---
id: comparison
title: thegn compared with other tools
order: 44
---

# thegn compared with other tools

thegn is a terminal-native git-worktree workspace that is also its own terminal
multiplexer. One session holds projects for repositories and tabs for their
worktrees, with the sidebar, right panel, tab bar and status bar drawn in the
process. A background daemon owns pane processes so they can survive detach.
Pane processes can optionally run in a sandbox, and the stable shell includes
merge and PR queues. Linux x86_64 is supported, macOS is best-effort, and
Windows is deferred. See [[getting-started]], [[daemon-and-sessions]], and
[[sandboxing]].

## tmux

- **What thegn adds:** tmux is a widely portable, scriptable multiplexer where
  you control its keys and plugins. thegn adds worktree-aware tabs and a
  sidebar, a diff/PR/CI panel, and optionally sandboxed panes in one application.
- **What it costs or lacks:** It is a larger alpha application with its own
  Alt-based keymap; on macOS, Option must send Alt. It does not use tmux
  configuration or provide a drop-in for tmux scripting.
- **When not to use thegn:** Choose tmux when you are on a remote server where
  only tmux is installed, or you want a minimal, long-established multiplexer.

thegn can also run inside tmux. Enable `extended-keys on` in tmux; see
[[terminal-compatibility]].

## Zellij

- **What thegn adds:** thegn is written in Rust and draws its own application
  chrome in-process around worktree tabs and panes.
- **What it costs or lacks:** It does not use Zellij, support Zellij layout
  files, or load WASM plugins. thegn plugins are out-of-process programs that
  speak JSON; see [[plugins]].
- **When not to use thegn:** Choose Zellij if you rely on Zellij layouts or
  WASM plugins.

## lazygit

- **What thegn adds:** thegn provides its own git tab and diffs, alongside a
  workspace and session layer. You can also run optional `lazygit` in a pane;
  see [[git-and-diffs]].
- **What it costs or lacks:** lazygit alone is smaller and offers a deeper
  git-focused interface.
- **When not to use thegn:** Choose lazygit if you only want a git client and
  do not need a workspace or session layer.

## VS Code worktrees

- **What thegn adds:** Against a GUI editor with git worktree support, thegn
  provides a terminal workspace organized around projects and worktree tabs,
  with panes and in-process chrome.
- **What it costs or lacks:** thegn is terminal-only and has no editor of its
  own beyond launching `$EDITOR` or a configured editor. It has no graphical
  IDE, extension marketplace, or integrated debugger beyond the narrow
  BugStalker integration on Linux x86-64.
- **When not to use thegn:** Choose VS Code if you want a graphical IDE,
  language-server UI everywhere, or remote development through VS Code.

## What thegn is not

thegn is not a replacement for your shell. It is not an AI product: the shell
works without an agent configured, and configured agent and tool entries can
launch ordinary command-line programs. thegn-proxy is a separate, opt-in
component.
The application is alpha software, and its pre-1.0 configuration and CLI
surfaces can change. This page makes no performance comparison.

## Using them together

Run thegn inside tmux with extended keys enabled, open lazygit in a thegn pane,
or open your editor from the files drawer. See [[terminal-compatibility]],
[[git-and-diffs]], and [[best-practices]].
