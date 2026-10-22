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
browser, `n` starts a share, and `x` stops the highlighted share (the
stop-share action stops them all). Stopping a share also removes its saved
entry, so it will not return on the next launch. A failed share shows its
reason and can simply be retried. If a tailscale share's teardown cannot be
confirmed, it stays in the list as failed with the reason and keeps its saved
entry; stopping it again retries the teardown. If thegn cannot stop a bore,
frp or iroh client, that share also stays in the list as failed with the
reason and keeps its saved entry (shown as `failed` in `thegn share list`;
`"state": "failed"` with `--json`). A
restart on that port is refused until stopping it again succeeds. At the
panel's full width the list gains a detail column with the untruncated URL (or
iroh consumer command), provider, and reach.
The bottom status bar shows a `⇅` chip while anything is
shared; activating it opens a detail popup listing the shares (the
`open-shares` action opens the panel section instead).

From the command line, `thegn share stop <port>` tears down a Tailscale share
and removes its saved record only after teardown is confirmed; on failure it
exits nonzero and keeps the record so you can retry. A share started by a
foreground `thegn share start` is stopped by interrupting that command. Use
`thegn share stop <port> --forget` only to remove the saved record; it does not
touch the tunnel.
A Tailscale share runs `tailscale serve` in the worktree's VPN sidecar through
the container runtime and container id recorded when that sidecar starts. If a
running sidecar has no runtime record, share start refuses and stop cannot
verify teardown; restarting the worktree sandbox lets thegn record the runtime.
Stopping a share in thegn while it is still starting cancels the start, so
the tunnel does not come up after the stop request.
A share client started by thegn runs in its own process group. Stopping it sends
TERM to the group, then KILL after a short grace period if processes remain.
A Tailscale share cannot restart on a port whose previous share is still
stopping; wait for teardown to finish first.

Share clients receive secrets outside their command line. bore receives its
secret through `BORE_SECRET`; frp reads its token from a generated `frpc.toml`
in the share's private state directory (directory mode 0700, file mode 0600).
Clients start with a minimal environment that omits HOME, D-Bus, `IROH_SECRET`
and provider credentials.

## Auto port forwards (`[forward]`)

For sandboxed worktrees ([[sandboxing]]), dev-server ports detected
inside the container are forwarded to the host's loopback automatically
for local browser preview. The **system → forward** section lists the
active forwards: `↵` copies the preview URL, `o` opens it, `n` starts
watching a port, and `x` removes a forward.

Reverse tunnels have a per-run connection cap (`forward.reverse_max_connections`,
default 256) and a process-wide host cap
(`forward.reverse_max_connections_total`, default 1024). Zero rejects all
connections; values above Tokio's internal permit maximum are clamped. The
process-wide value is read when the first reverse tunnel starts, so restart
thegn to change it. The `[forward]` deadlines `reverse_write_stall_secs`
(default 60), `reverse_idle_secs` (default 7200), and
`reverse_lifetime_secs` (default 86400) apply on both ends of the tunnel. A
write-stall value of 0 uses 60 seconds; idle and lifetime values of 0 disable
those deadlines. Values are read when a tunnel starts or reconnects, so a
running tunnel keeps its starting values. The idle, lifetime and write-stall
deadlines are configurable; the dial timeout and retry behavior stay fixed. A
local write stall closes that connection, while a stall of
`reverse_write_stall_secs` writing to the shared tunnel ends the tunnel run;
the host supervisor reconnects it with bounded backoff. On the sandbox side,
transient accept errors such as running out of file descriptors are retried
with a short backoff instead of stopping the tunnel. Rejection warnings are
limited to one per minute per reason across the process. With `THEGN_PERF=1`,
the perf rollup also reports per-interval rejection counts when any have
changed.

## Frontend preview discovery (`[preview]`)

When `[preview] enabled = true`, thegn discovers a target from explicit
`preview.ports`, literal `--port`/`-p`/`PORT=` values in `package.json`'s
`dev` and `start` scripts, and loopback URLs printed by a worktree pane. It
never runs package scripts or launches a server. Config and package files are
read once at startup and again after a worktree switch or config reload; pane
and sandbox lifecycle events update the target without an idle poll.

The selected target appears at the top of **system → forward** with its port,
source, URL, and one of five honest states: `up` means a live pane/provider
event proves reachability, `seen` means a port appeared in pane output without
a reachability probe, `configured` means an explicit config target has no live
runtime evidence, `down` means a watched process or forward ended, and
`unknown` means the runtime has no event source. Package-script targets show
`unknown` until runtime events establish more. `unknown` is not treated as
a failure: `↵` still copies the URL and `o` still opens it in the configured
external browser (`[forward] browser`: an argv template with `%s` for the URL,
no shell; an explicit value is final, otherwise `$BROWSER`, then the OS opener;
only http/https URLs are opened; also used by `open_on_detect`). The active
worktree also gets a compact preview token in the sidebar.

An in-terminal browser is an optional drawer runtime occupant named `preview`.
When the installed drawer tool registry provides that occupant, it receives the
selected URL as ordinary argv/environment context and runs as a contained PTY;
the drawer owns its pooling and persistence. Without that registry entry the
preview drawer is unavailable and external `o` remains the supported path.

`[share]` is off until a provider is configured; **auto-forwarding is on
by default** for container-sandboxed worktrees (`[forward] auto = false`
disables it) — see the [[config-reference]].
