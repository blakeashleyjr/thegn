---
id: acknowledgments
title: Acknowledgments
order: 52
---

# Acknowledgments

thegn is one person's tool standing on a great deal of other people's work.
This page says thank you, in three parts: what thegn is built on, whose ideas
shaped it, and which services it talks to. The legal side (every crate, its
licence, and the notices some licences require) is in
`THIRD_PARTY_NOTICES.md`, which ships next to the binary and in every release.

thegn itself is dual-licensed under MIT or Apache-2.0, at your option.

## Built on

Each of these is a real dependency of the workspace, not a wish list.

- **termwiz and wezterm.** thegn draws the outside terminal with termwiz:
  surfaces, diffing and input decoding. Wez Furlong's code is vendored at
  `vendor/termwiz` with one small patch so a failed terminal write on exit
  cannot panic. Without it, the compositor would be a much bigger project.
- **alacritty_terminal.** The emulator grid behind every pane: parsing escape
  sequences and keeping scrollback correct is hard, and Alacritty's crate does
  it well.
- **portable-pty.** Opens the pseudo-terminals that panes run in, on Linux and
  macOS.
- **tokio.** The async runtime under the daemon, the control sockets and the
  network clients.
- **ratatui.** The widget buffer that the panel, sidebar and plugin UIs render
  into before termwiz puts it on screen.
- **gix (gitoxide) and libgit2.** Fast in-process git reads, so the sidebar can
  show dozens of worktrees without spawning a `git` process each time. libgit2
  (through the `git2` crate) backs the file-search engine.
- **fff-search and neo_frizbee.** The file and grep engine, and the SIMD fuzzy
  matcher behind the command palette and list filtering.
- **rusqlite and SQLite.** Where thegn keeps its state. SQLite's public-domain
  gift is that this is boring.
- **LMDB (heed).** The index store used by file search.
- **tree-sitter.** Parses source files so thegn can talk about functions and
  types instead of lines.
- **syntect.** Syntax highlighting in diffs.
- **tokei.** Language and line counts for the project overview.
- **iroh.** Peer-to-peer connectivity for call-home tunnels and direct pairing
  (see [[cli]]).
- **whisper.cpp, whisper-rs and cpal.** Local dictation ([[voice]]): the model
  runtime, its Rust binding, and microphone capture. The Whisper models
  themselves come from OpenAI via the `ggerganov/whisper.cpp` releases on
  Hugging Face.
- **axum and reqwest.** The HTTP server and client libraries behind the proxy
  daemon, provider calls and model downloads.
- **zbus and keyring.** Media control over D-Bus and the OS credential store.
- **Fluent.** The localisation system the interface strings go through.
- **bubblewrap, passt, podman and Docker.** The sandbox runs on whichever of
  these you have: bubblewrap for the lightweight Linux sandbox, passt for
  rootless networking, and podman, Docker or Apple's `container` for the OCI
  backends. See [[sandbox-backends]].
- **git, lazygit, yazi, fzf, gum, delta, gh and gdu.** Not linked in, but the
  Nix package puts them on `PATH` and the drawers and pickers lean on them.
  yazi's `git.yazi` plugin is bundled verbatim (MIT, yazi-rs). difftastic
  powers the structural diffs when you have it ([[git-and-diffs]]).
- **The theme authors.** The theme presets are named after colour palettes by
  the Catppuccin, Nord, Dracula, Gruvbox, Tokyo Night, Solarized, Rosé Pine,
  One Dark, Monokai Pro, Ayu, Everforest, Kanagawa and Night Owl communities.
  Only the colours are used.

And the several hundred smaller crates underneath them, every one listed in
`THIRD_PARTY_NOTICES.md`.

## Inspired by

- **zellij.** thegn began as a layer on top of zellij and still owes it the
  shape of a workspace of tabs and panes. The zellij architecture has since
  been replaced with a native compositor ([[comparison]]).
- **tmux.** Detach and reattach with scrollback intact, and a daemon that owns
  the processes ([[daemon-and-sessions]]).
- **wezterm.** For termwiz, and for showing how far a terminal library can go.
- **Ghostty and kitty.** Ghostty is a shipped profile and the main test
  terminal ([[terminal-compatibility]]); kitty's graphics protocol is what
  thegn relays for inline images, and its keyboard protocol is one of the key
  encodings thegn understands.
- **Jujutsu (jj).** thegn detects colocated jj repositories and stays out of
  their way instead of fighting them.
- **Zed.** The multibuffer idea behind aggregated search results, and the
  extension model's approach to acquiring and pinning external tools (the
  managed-tool resolver and its grants).
- **Vim.** Registers and yank semantics in copy mode ([[copy-and-select]]).
- **bat and delta.** The default diff colour theme follows the one they use.
- **lazygit and yazi.** thegn does not try to reimplement a full git UI or file
  manager; both run inside thegn as drawers and panes.

## Services

thegn works with these. Nothing is sent to any of them unless you turn the
feature on ([[privacy]]).

- **Sprites** (Fly.io's persistent microVMs) and **Vercel Sandbox**: remote
  environments for worktrees ([[environments]], [[vercel-sandbox]]).
- **Fly.io and machine0**: further provider backends on the dev channel.
- **GitHub**: pull requests, checks and the merge queue ([[pr-queue]]).
- **Linear**: issue tracking from inside the workspace.
- **Hugging Face**: hosts the Whisper models voice mode downloads.

## Saying more

If we have missed credit that is due, or got a licence wrong, please open an
issue; see `SUPPORT.md`. The way to fix a missing entry is to add it to
`scripts/third_party_extra.md` and regenerate `THIRD_PARTY_NOTICES.md`.
