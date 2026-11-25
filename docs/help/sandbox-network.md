---
id: sandbox-network
title: Sandbox network
parent: sandboxing
order: 4
---

# Sandbox network

Part of [[sandboxing]]. The network mode and the allow and block lists.

## Network

By default a `standard` sandbox keeps networking so fetch, clone, build,
and debuggers work; `sealed` removes it entirely. `[sandbox]` also
carries hostname allow/block lists for the middle ground. These lists are
advisory, not an egress boundary: no local backend enforces them, and only the
Sprites and Vercel providers lower them to their own policy API (Vercel takes
allow lists only). `network = "none"` is the
only enforced deny, and `*` in `network_block` means offline — see
[[config-reference]]. thegn warns when lists are set but not enforced: at
launch (standalone sandboxed terminals included), at `thegn env provision`, and
in `thegn doctor`.

### Vercel sandboxes: deny by default

Unlike the advisory `[sandbox]` lists, a Vercel sandbox's network is enforced
by Vercel and **denied by default**. `[env.<n>.provider] egress` picks `deny`
(default), `registries` (package registries and git hosts only) or `allow-all`,
and `egress_allow` adds hosts in the grammar below. The policy is applied on
create and on every resume. See [[vercel-sandbox]].

### Host patterns

Entries in `network_allow` and `network_block` share one grammar:

| Entry     | Matches                                                       |
| --------- | ------------------------------------------------------------- |
| `*`       | Any host. Whole entry only, no port suffix.                   |
| `x.com`   | `x.com` and every subdomain of it. `badx.com` does not match. |
| `*.x.com` | Subdomains of `x.com`, never `x.com` itself.                  |
| `=x.com`  | Exactly `x.com`.                                              |

Add a port suffix to any name form: `:443`, a range such as `:8000-8100`, or
`:*` for any port. A block entry without a port suffix blocks every port.
Matching ignores case and one trailing dot. IP literals, names with an empty
label, and percent-encoded or other non-hostname characters are refused. A
malformed entry never matches, so a bad allow entry cannot widen the list and a
bad block entry cannot block.

On the `apple` backend, `network = "host"` is refused with
`network-unexpressible` (Apple `container` has no host network; use `nat`).
`network = "none"` (and therefore `sealed`) is refused too: Apple can create
an internal network, but it still reaches services bound on your Mac, so thegn
will not call it offline. Use podman or docker (they run in a VM on macOS) for
offline or sealed worktrees.

`[sandbox.vpn]` can put the sandbox on an overlay network (tailscale,
headscale, wireguard, openvpn, netbird, zerotier). The `mode` decides how:
`sidecar` (default, best isolation — a companion container owns the
netns), `proxy` (userspace SOCKS/HTTP; **not** a containment boundary, but
the only honest option for bwrap/systemd), `in_container`, or `netns`.
`on_error` decides what happens when the tunnel will not come up: `fail`
refuses to launch, `warn` launches anyway, `offline` forces
`network=none` so nothing leaks onto the host network.
A VPN is a route, not egress control: thegn never reports a tunnel as
enforcing the allow/block lists. `dns` picks how resolution composes with the
overlay: `tunnel` (default) lets the overlay own resolution; `filter-only`
drops the overlay's pushed DNS (such as Tailscale MagicDNS) so the runtime's
default resolver is used; `filter-front` is reserved, warns and behaves as
`tunnel`, and `thegn config validate` rejects it. No mode filters
hostnames.

A running VPN sidecar is reused only when its tunnel configuration matches.
Otherwise the result follows `on_error`: `fail` refuses launch, `warn`
continues without the VPN, and `offline` forces `network=none`. The sidecar
stays running. Stop this worktree's sandbox (close the worktree, or remove both
the worktree container and sidecar) so both are recreated with the new
configuration. An unlabelled sidecar from before this check counts as changed.

Separately, `[network]` auto-detects when the machine is offline and
pauses remote refreshes rather than hammering unreachable endpoints;
local caches are served stale and it restores on the next success.
