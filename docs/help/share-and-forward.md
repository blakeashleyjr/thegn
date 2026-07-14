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
worktree at a public URL; stop it with the stop-share action or from the
[[panel]]'s **system → share** section, which lists every active share
and its URL. The masthead shows an indicator while anything is shared —
its item opens the shares section.

## Auto port forwards (`[forward]`)

For sandboxed worktrees ([[sandboxing]]), dev-server ports detected
inside the container are forwarded to the host's loopback automatically
for local browser preview. The **system → forward** section lists the
active forwards.

Both features are off until their config tables enable them — see the
[[config-reference]].
