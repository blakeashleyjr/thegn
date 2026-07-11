# Per-sandbox VPN / tunnel attachment

Status: **landed (OCI sidecar v1)**, with bwrap/systemd proxy and DNS
`filter-front` chaining explicitly deferred (see _Deferred_ below).

## Goal

Attach each worktree's sandbox to its **own** overlay network / tunnel, with its
own identity, while the host's networking — including any host `tailscaled` —
stays completely untouched. Generic across providers: Tailscale, Headscale,
WireGuard, OpenVPN, NetBird, ZeroTier, plus a custom-command escape hatch.

## Design in one paragraph

thegn does **not** embed a tunnel datapath (that would fight the 0%-idle
event loop and mean re-implementing a control plane). Instead each tunnel daemon
runs as a per-worktree **sidecar container**; the worktree OCI container joins
the sidecar's network namespace via `--network container:<sidecar>`, so its only
egress is the tunnel and its own capabilities stay untouched (NET_ADMIN/TUN live
in the sidecar). This mirrors how the sandbox already shells out to `podman`
rather than embedding a runtime, and how `thegn-svc` uses service seams with
subprocess fallback.

## Layering

- **`thegn-core` (data, pure, unit-tested, counts toward the 95% gate)**
  - `config.rs`: `[sandbox.vpn]` — `VpnConfig` + a per-provider sub-table
    (`VpnProviderKind`/`VpnMode`/`VpnOnError`/`VpnDnsMode` config-enums).
    Discriminant + sub-table shape (like `IssuesConfig`), not a flat struct.
    Secrets are **refs** (`env:VAR` / `file:PATH`) via `expand_env_ref`.
  - `SandboxProfile::SealedTunnel` (aliases `tunnel-only`/`vpn-only`): same
    lockdown as `sealed` (read-only root, drop ALL caps on the worktree,
    no-new-privs, tight pids) but the worktree has **no direct host egress** —
    it's attached to the tunnel; with no VPN it degrades to `network=none`.
    Plain `sealed` _refuses_ a VPN (`permits_vpn()` is false).
  - `sandbox.rs`: `SandboxSpec.vpn: Option<VpnSpec>`, `VpnParams`,
    `build_vpn_spec()` (reconciles provider × profile), deterministic
    `vpn_sidecar_name()` (`<base>-szvpn`), and the `oci_create_opts` wiring:
    emits `--network container:<sidecar>` for sidecar/proxy mode and
    **suppresses `--dns` and `-p`** (illegal on a container-netns join);
    `in_container` mode adds NET_ADMIN + `/dev/net/tun` to the worktree itself.
    `teardown`/`teardown_by_path` also `rm -f` the `-szvpn` sidecar.
    `oci_runtime_prefix()` exposes the container-CLI prefix to the host.
- **`thegn-svc::vpn` (behavior, subprocess seam, smoke-tested)**
  - `VpnProvider` trait + `BuiltinProvider` dispatching on `VpnParams`.
  - Pure, unit-tested per-provider plan builders → `SidecarPlan` (image, run
    flags, env with resolved secrets, mounts, materialized files, command,
    readiness probe, optional proxy). `requirements()`, `dns()`.
  - `up`/`ready`/`down` (sync, blocking subprocess — run off the event loop by
    the host, like `sandbox::ensure`). Secrets resolve only here; secret files
    are written 0600 under `$XDG_STATE_HOME/thegn/vpn/<sidecar>/`.
- **`thegn-host`**
  - `agent.rs::attach_vpn()` sequences it: `provider.up()` → `ready()` BEFORE
    `sandbox::ensure()` (the worktree joins the sidecar netns), injects userspace
    proxy `ALL_PROXY`/`HTTPS_PROXY` into `env_overrides`, and applies `on_error`
    (`fail` bails — a tunnel failure must never fall through to a less-isolated
    backend; `warn` continues; `offline` forces `network=none`).
  - `agent.rs::deregister_vpn()` + `run.rs` close path: de-register the ephemeral
    node (`tailscale logout`, …) before `teardown_by_path` removes the sidecar.
  - `sandbox_events.rs`: `-szvpn` container events map back to their worktree.

## Modes

- `sidecar` (default): TUN transparent routing; sidecar holds the caps.
- `proxy`: userspace tunnel (Tailscale/NetBird) exposing SOCKS5; worktree opts in
  via `ALL_PROXY`. No NET_ADMIN/TUN. Not a containment boundary.
- `in_container`: client runs in the worktree container (needs NET_ADMIN + TUN;
  weakens `hardened`).
- `netns`: join a host-prepared netns (host-toolchain backends; deferred).

## Provider/mode → capability burden

In `sidecar`/`proxy` mode the NET_ADMIN/TUN burden is on the **sidecar**, so the
worktree's caps are untouched (this is why sidecar is the default). Userspace
(Tailscale/NetBird `proxy`) needs neither. WireGuard/OpenVPN/ZeroTier are
TUN-only. `requirements()` encodes this; `resolve_scoped` refuses a VPN under
plain `sealed`.

## DNS

- `tunnel` (default): the overlay owns resolution (MagicDNS / pushed DNS); for a
  sidecar the sidecar's `/etc/resolv.conf` governs. `network_allow/block` bypassed.
- `filter-only`: suppress the overlay's pushed DNS (Tailscale `TS_ACCEPT_DNS=false`).
- `filter-front`: chain the allow/block filter in front of the tunnel's resolver.
  `dns_filter::DnsPolicy.upstream` is the seam (configurable, tested). **Deferred**:
  with a sidecar the loopback filter can't reach the sidecar's netns, so this
  currently behaves like `tunnel`.

## Verification

- Unit: `cargo test -p thegn-core --lib vpn`, `... dns_filter`,
  `cargo test -p thegn-svc vpn` (provider plan builders, reconciliation
  matrix, profile floors, sidecar-join/dns/ports suppression, upstream wiring).
- Manual (per provider, against a throwaway/ephemeral identity):
  `just start name=vpn-dev` with `[sandbox] enabled=true`, `[sandbox.vpn]
provider="tailscale"`, `auth_key="env:TS_AUTHKEY"` (ephemeral key). Open a
  worktree pane → inside the sandbox `tailscale status` shows a fresh ephemeral
  node on the configured tailnet, external IP differs from the host; on the host
  `tailscale status` is unchanged and no sidecar socket/state is shared.
  `sealed-tunnel`: `capsh --print` shows empty caps yet egress works only through
  the tunnel. Close the worktree → the `-szvpn` sidecar is removed and the node
  de-registers.

## Deferred follow-ups

1. **bwrap/systemd `proxy` mode** — needs a _host-process_ userspace daemon
   (not an OCI sidecar) on host loopback + `ALL_PROXY` in `wrap_script`; honestly
   non-isolating (shares the host netns). A separate bring-up path from the OCI
   sidecar runner.
2. **DNS `filter-front` for sidecar tunnels** — run/forward the allow/block
   filter inside or reachable-from the sidecar netns (the `upstream` seam exists).
3. **CLI/status surfacing** — show tunnel state + node identity in the SANDBOXES
   panel (events already map back to the worktree).
4. **`[env.<name>.vpn]`** — when the named-environment / `Placement` layer lands,
   a tunnel becomes a property of a named env for free (`VpnConfig` is whole-table
   in the overlay and identity-bearing).
