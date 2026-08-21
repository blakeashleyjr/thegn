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
shared; activating it opens a detail popup.

## Auto port forwards (`[forward]`)

For sandboxed worktrees ([[sandboxing]]), dev-server ports detected
inside the container are forwarded to the host's loopback automatically
for local browser preview. The **system → forward** section lists the
active forwards: `↵` copies the preview URL, `o` opens it.

`[share]` is off until a provider is configured; **auto-forwarding is on
by default** for container-sandboxed worktrees (`[forward] auto = false`
disables it) — see the [[config-reference]].
