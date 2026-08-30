---
id: share-and-forward
title: Share & port forwarding
order: 11
contexts: [panel:share, panel:forward]
actions: [share-worktree-port, stop-worktree-share, open-shares]
---

# Share & port forwarding

Two ways a worktree's dev server reaches a browser.

## Ingress shares (`[share]`)

`Alt-Shift-S` (share-worktree-port) exposes a port from the focused
worktree at a public URL. The [[panel]]'s **system → share** section
lists every active share: `↵` copies the URL, `o` opens it in the
browser, and `x` stops the highlighted share (the stop-share action
stops them all). A failed share shows its reason and can simply be
retried. At the panel's full width the list gains a detail column with
the untruncated URL (or iroh consumer command), provider, and reach.
The bottom status bar shows a `⇅` chip while anything is
shared; activating it opens a detail popup listing the shares (the
`open-shares` action opens the panel section instead).

## Auto port forwards (`[forward]`)

For sandboxed worktrees ([[sandboxing]]), dev-server ports detected
inside the container are forwarded to the host's loopback automatically
for local browser preview. The **system → forward** section lists the
active forwards: `↵` copies the preview URL, `o` opens it.

## Frontend preview discovery (`[preview]`)

When `[preview] enabled = true`, thegn discovers a target from explicit
`preview.ports`, literal `--port`/`-p`/`PORT=` values in `package.json`'s
`dev` and `start` scripts, and loopback URLs printed by a worktree pane. It
never runs package scripts or launches a server. Config and package files are
read once at startup and again after a worktree switch or config reload; pane
and sandbox lifecycle events update the target without an idle poll.

The selected target appears at the top of **system → forward** with its port,
source, URL, and one of three honest states: `up` means a live pane/provider
event proves reachability, `down` means that watched process or forward ended,
and `unknown` means the runtime has no event source. `unknown` is not treated as
a failure: `↵` still copies the URL and `o` still opens it in the configured
external browser. The active worktree also gets a compact preview token in the
sidebar.

An in-terminal browser is an optional drawer runtime occupant named `preview`.
When the installed drawer tool registry provides that occupant, it receives the
selected URL as ordinary argv/environment context and runs as a contained PTY;
the drawer owns its pooling and persistence. Without that registry entry the
preview drawer is unavailable and external `o` remains the supported path.

`[share]` is off until a provider is configured; **auto-forwarding is on
by default** for container-sandboxed worktrees (`[forward] auto = false`
disables it) — see the [[config-reference]].
